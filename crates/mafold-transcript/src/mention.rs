//! The `@`-mention grammar, and the one answer to "whom did this message
//! summon" — for the api's bot trigger (`fire_bots`) and the daemon's reply
//! gate (`should_respond`) alike. Both used to carry their own copy of the
//! grammar; a rule that lives twice drifts.
//!
//! A mention is an `@` that isn't glued to another handle, then a handle —
//! `[A-Za-z0-9_:-]+`, `:` being the namespace separator (`@ops:claude`). The
//! boundary is "the byte before isn't a handle byte", NOT "is whitespace": CJK
//! writes no space (`帮我看看@ops:claude`). Clients label exactly these, and
//! only in the reader's prose (`crate::prose`): a handle in a card body or in
//! code renders no label, so it summons nobody.
//!
//! WHO a message summons depends on who wrote it:
//!   * a person — every handle in the prose. A person's `@` anywhere is them
//!     calling;
//!   * an AI — only the handles that OPEN a line: `@a 你来看下`, or a run of
//!     them, `@a @b 你们俩…`. An agent's message is long and talks ABOUT other
//!     agents all the time. Measured 2026-10-06 over ~10k messages in six
//!     groups: of 579 `@agent`s in AI-written text, 485 sat mid-sentence and
//!     one of those was a call; 54 opened a line and 49 of those were calls;
//!     37 opened a list item or a quote and none were. Before this, every one
//!     of them woke the agent — a daemon's progress report naming the hosted
//!     claude mid-sentence ("@mafold、@deepseek、@claude、@chatgpt 和用户建的托管
//!     bot 都走这一层") woke it, and it ran 11 tools on the owner's machine
//!     (2026-10-06 03:46, #托管bot日期).
//!
//!   A line opens with the `@` itself, after nothing but whitespace: a list
//!   item (`- @a`), a quote (`> @a`), a table cell (`| @a |`), emphasis
//!   (`**@a**`) or an inline code span before it (`` `x` @a ``) means the
//!   handle is being talked about, not called. A card or a code block is a
//!   block in the bubble, so the text after one does open a line.
//!
//! What a reply to someone's message does is not decided here — this module
//! reads text only.

use crate::prose;

/// True if a byte can appear INSIDE an @handle: alphanum, `_`, `-`, `:` (the
/// namespace separator). Anything else ends a handle — and, before an `@`,
/// marks a mention boundary. A multi-byte char (CJK, emoji) is never one of
/// these, so its trailing byte is a boundary: `帮我看看@ops:claude` mentions.
pub fn is_handle_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b':'
}

/// Every mention in exactly the bytes given, in order: the byte offset of its
/// `@`, and the handle as written.
pub fn scan(text: &str) -> impl Iterator<Item = (usize, &str)> + '_ {
    let b = text.as_bytes();
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i < b.len() {
            if b[i] == b'@' && (i == 0 || !is_handle_byte(b[i - 1])) {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && is_handle_byte(b[j]) {
                    j += 1;
                }
                let at = i;
                i = j.max(start);
                // `is_handle_byte` is ASCII-only, so start/j are char boundaries.
                if j > start {
                    return Some((at, &text[start..j]));
                }
            } else {
                i += 1;
            }
        }
        None
    })
}

/// Is `user` mentioned in exactly the bytes given? Case-insensitive; an empty
/// `user` matches nothing.
pub fn has(text: &str, user: &str) -> bool {
    !user.is_empty() && scan(text).any(|(_, h)| h.eq_ignore_ascii_case(user))
}

/// Between two handles of a line's opening run: `@a @b`, `@a, @b`, `@a、@b`.
fn run_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | '，' | '、')
}

/// The handles that open `line`, in order: the first mention when nothing
/// but whitespace stands before it, then each next one while only
/// [`run_separator`]s stand between.
fn opening(line: &str) -> impl Iterator<Item = &str> + '_ {
    let mut cursor = 0usize;
    scan(line).map_while(move |(at, h)| {
        let gap = &line[cursor..at];
        let opens = if cursor == 0 {
            gap.chars().all(char::is_whitespace)
        } else {
            gap.chars().all(run_separator)
        };
        cursor = at + 1 + h.len();
        opens.then_some(h)
    })
}

/// Whom `text` summons, lowercased, in the order written (duplicates kept —
/// the order is the floor, `.docs/a2a-v2.md`). `ai`: the author is an AI, so
/// only the handles that open a line count (module docs).
pub fn summoned(text: &str, ai: bool) -> Vec<String> {
    if !text.contains('@') {
        return Vec::new();
    }
    if ai {
        prose::visible_lines(text).lines().flat_map(opening).map(str::to_lowercase).collect()
    } else {
        scan(&prose::visible_prose(text)).map(|(_, h)| h.to_lowercase()).collect()
    }
}

/// Does `text` summon `user`? The raw scan runs first: cutting cards and code
/// and keeping only a line's opening can only take a mention away (a cut ends
/// on `%}` or a backtick, never on a handle byte), so the projection — the one
/// costly step — runs only on text that names `user` at all.
pub fn summons(text: &str, user: &str, ai: bool) -> bool {
    has(text, user) && summoned(text, ai).iter().any(|h| h.eq_ignore_ascii_case(user))
}

/// The lines of `text` that summon `user`, trimmed — WHERE an agent was
/// called, for telling it so. Whole lines either way (`visible_lines`), so the
/// two rules quote a line identically; an inline code span shows as `…`.
pub fn summoning_lines(text: &str, user: &str, ai: bool) -> Vec<String> {
    if !has(text, user) {
        return Vec::new();
    }
    prose::visible_lines(text)
        .lines()
        .filter(|l| {
            if ai {
                opening(l).any(|h| h.eq_ignore_ascii_case(user))
            } else {
                has(l, user)
            }
        })
        .map(|l| l.trim().replace(prose::INLINE_CUT, "…"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ai(s: &str) -> Vec<String> {
        summoned(s, true)
    }
    fn person(s: &str) -> Vec<String> {
        summoned(s, false)
    }

    #[test]
    fn the_grammar() {
        let h = |s: &str| scan(s).map(|(_, h)| h.to_string()).collect::<Vec<_>>();
        assert_eq!(h("hey @ops and @ops:claude"), ["ops", "ops:claude"]);
        assert_eq!(h("帮我看看@ops:claude"), ["ops:claude"], "CJK writes no space before @");
        assert_eq!(h("(@ops) cc @x, 看下"), ["ops", "x"]);
        assert!(h("mail me@ops.com").is_empty(), "@ glued to a handle is an address");
        assert!(h("@ alone, @@, @").is_empty());
        assert_eq!(h("@a@b"), ["a"], "the second @ is glued to the first handle");
        assert!(has("yo @OPS", "ops"));
        assert!(!has("@opsdu", "ops") && !has("@ops:claude", "ops") && !has("@ops", ""));
        let at: Vec<usize> = scan("x @a y @b").map(|(i, _)| i).collect();
        assert_eq!(at, [2, 7]);
    }

    /// The message that woke the hosted claude on 2026-10-06 03:46: a daemon's
    /// progress report, naming the hosted bots in passing.
    #[test]
    fn an_ai_naming_agents_mid_sentence_summons_nobody() {
        let report = "还没修完，进度如下：\n\
- **代码写完了，还没编译过。** 时钟层按\"正在回答的那个人\"的时区写今天。\
@mafold、@deepseek、@claude、@chatgpt 和用户建的托管 bot 都走这一层，所以是一起修的。\n\
- **实测正好赶上窗口**：编译一好，我就用本地 API（这个分支的代码，@mafold 接真模型）问\"今天几号\"并截图。";
        assert!(ai(report).is_empty(), "{:?}", ai(report));
        assert!(!summons(report, "claude", true));
        // The same words from a person still call every one of them.
        assert_eq!(person(report), ["mafold", "deepseek", "claude", "chatgpt", "mafold"]);
        assert!(summons(report, "claude", false));
    }

    #[test]
    fn an_ai_calls_by_opening_a_line() {
        assert_eq!(ai("@opsdu:codex 请改这三处，改完回一句。"), ["opsdu:codex"]);
        assert_eq!(ai("审完了。\n@linsky:opus48 v3 我看过了，可以装。"), ["linsky:opus48"]);
        assert_eq!(ai("  @a 缩进也算"), ["a"]);
        assert_eq!(ai("@linsky:opus48"), ["linsky:opus48"], "a bare handle line");
        assert_eq!(ai("@Ops:Claude 大小写"), ["ops:claude"]);
        assert!(summons("先说结论。\n\n@ops:claude 你来接", "ops:claude", true));
    }

    #[test]
    fn a_run_of_handles_opening_a_line_calls_them_all_in_order() {
        assert_eq!(ai("@a @b 你们俩看下"), ["a", "b"]);
        assert_eq!(ai("@a, @b, @c please"), ["a", "b", "c"]);
        assert_eq!(ai("@a、@b 你们"), ["a", "b"]);
        // The run ends at the first word; a later handle is just a name.
        assert_eq!(ai("@a 先看，然后交给 @b"), ["a"]);
        assert_eq!(ai("@linsky:opus48 —— @opsdu 让你推分支"), ["linsky:opus48"]);
        // Each line is judged on its own.
        assert_eq!(ai("@a 做 X\n顺带 @c\n@b 做 Y"), ["a", "b"]);
    }

    #[test]
    fn an_ai_talking_about_an_agent_summons_nobody() {
        for s in [
            "这层 @claude 也走",
            "来，@linsky:opus48，你反驳。",           // vocative mid-line: the one real call in 485
            "做完了，交给 @b",                         // a call at the END of a line is not one
            "- @claude 按次扣触发人的钱包",            // list item
            "1. @chatgpt 全量 1115",                  // numbered list item
            "> @claude 说过的话",                      // quote
            "| @chatgpt | 1243 |",                    // table cell
            "**@mafold** 会私信你",                    // emphasis
            "改的是 `fire_bots_on_finish` @claude 那条路", // inline code before it
            "用 ` 分隔。\n下一行 ` @claude 也走",          // …even a span running over a line break
            "`@claude` 你来",                           // the handle itself in code
            "```\n@claude 你来\n```",                   // fenced code
            "{% mafold/ask %}\nq|x|0|?\no|交给 @claude|好\n{% /mafold/ask %}", // card body
            "{% mafold/trace summary=\"s\" %}\n@claude 你来\n{% /mafold/trace %}\n收工。", // folded narration
            "我是 @deepseek 的 owner",
            "",
        ] {
            assert!(ai(s).is_empty(), "{s:?} → {:?}", ai(s));
        }
    }

    #[test]
    fn a_block_before_the_handle_still_opens_a_line() {
        // A card or a code block is a block in the bubble: what follows starts
        // a fresh line, so it can call.
        assert_eq!(ai("{% mafold/trace summary=\"s\" %}\n…\n{% /mafold/trace %}\n@b 你来"), ["b"]);
        assert_eq!(ai("看图 {% mafold/kline symbol=\"BTC\" /%}@b 你来"), ["b"]);
        assert_eq!(ai("```\nls\n```\n@b 接着"), ["b"]);
    }

    #[test]
    fn a_person_calls_from_anywhere_in_the_prose() {
        assert_eq!(person("帮我看看@ops:claude"), ["ops:claude"]);
        assert_eq!(person("- @a 和 @b"), ["a", "b"]);
        assert!(person("`@a` and {% x %}@b{% /x %}").is_empty(), "never from code or a card body");
    }

    #[test]
    fn summons_agrees_with_summoned() {
        for (s, who) in [
            ("@a 你来\n顺带 @b", "a"),
            ("@a 你来\n顺带 @b", "b"),
            ("这层 @claude 也走", "claude"),
            ("`x` @a", "a"),
            ("{% x /%}@a", "a"),
            ("@A:b", "a:b"),
        ] {
            for is_ai in [true, false] {
                assert_eq!(
                    summons(s, who, is_ai),
                    summoned(s, is_ai).iter().any(|h| h == who),
                    "{s:?} {who} ai={is_ai}"
                );
            }
        }
    }

    #[test]
    fn summoning_lines_quote_where_the_call_is() {
        let s = "这层 @b 也走\n@b 你看下 `x` 那段\n- @b 列表";
        assert_eq!(summoning_lines(s, "b", true), ["@b 你看下 … 那段"]);
        assert_eq!(summoning_lines(s, "b", false), ["这层 @b 也走", "@b 你看下 … 那段", "- @b 列表"]);
        assert!(summoning_lines("no one here", "b", true).is_empty());
    }
}
