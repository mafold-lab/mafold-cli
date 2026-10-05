//! `{% mafold/only for="…" %}…{% /mafold/only %}` — one message, read
//! differently by each reader.
//!
//! A message is one stored body, and until now everyone in the room was handed
//! the same bytes. That is wrong for a handful of things a bot has to say IN a
//! room but only TO one person: a compaction summary is a digest of the whole
//! conversation, written for the bot's owner (2026-10-05, owner: 「压缩摘要只给
//! bot 主人」); a secure-input card tells the person named on it what to do and
//! everyone else only that something was asked. Hiding those in the client is a
//! courtesy, not privacy — every reader already HAS the text. So the server cuts
//! the block out of the copy each reader is sent (.docs/secure-input-v1.md §9),
//! and this file is the one definition of that cut: the api applies it at every
//! egress, the daemon writes the markup, `mafold read` prints what is left.
//!
//! The rules:
//!   * `for` is a comma list — `owner` (whoever answers for the author: the
//!     author itself when it has no owner, i.e. a person; the bot's owner when
//!     a bot wrote it), `sender` (the author), `@name`. Absent ⇒ `owner`.
//!   * The author always reads their own words. Nothing can be hidden from the
//!     one who wrote it — and a daemon reads its own reply back and writes it
//!     again when it stamps a card (`own_message` → `editMessage`); were it cut
//!     for the author, the first stamp would overwrite the owner's block with
//!     the marker, for good.
//!   * A reader outside the audience gets the WHOLE block, open tag to close
//!     tag, replaced by `{% mafold/only for="…" hidden="true" /%}` — same
//!     audience, no content, no other attribute (an attribute is content too).
//!   * The cut errs wide. A block runs to its matching close (nested `only`
//!     counted), or to the END of the message when there is none — mid-stream,
//!     or a close the author misspelled. Tags are cut inside code spans too.
//!     The client's splitter is narrower on every one of those (first close,
//!     code is literal), so whatever it would show as the block, the cut has
//!     already removed; where they disagree a reader loses a few words of prose
//!     after a malformed block, never sees the block.
//!   * Blocks nest: a reader inside the outer audience still gets the inner
//!     blocks cut by their own audiences. Blocks inside OTHER cards' bodies
//!     (the compaction card) are cut just the same — the scan is over the whole
//!     text, not over top-level islands.
//!   * Storage keeps the full text. This is visibility, not encryption: real
//!     secrets go to the vault, never into a message.

use std::borrow::Cow;

use crate::preview::attr;
use crate::prose::parse_tag;

/// The card this markup is (`cards/only`).
pub const TAG: &str = "mafold/only";

/// Who a block is relative to: the message's author and whoever answers for it.
#[derive(Clone, Copy, Debug)]
pub struct Author<'a> {
    /// The account that wrote the message — `sender`.
    pub sender: &'a str,
    /// `owner`: the author's owner, or the author itself when it has none.
    pub owner: &'a str,
}

impl<'a> Author<'a> {
    /// `owner` is the account's owner if it has one (`Account::owner()`: a
    /// bot's), else the account itself — a person answers for their own words.
    /// No kind check: having an owner is the whole difference.
    pub fn new(sender: &'a str, owner: Option<&'a str>) -> Self {
        Self { sender, owner: owner.filter(|o| !o.is_empty()).unwrap_or(sender) }
    }
}

/// Is `name` this card? Case-insensitive on purpose: the cut must cover every
/// spelling a host might still resolve to it.
pub fn is_only(name: &str) -> bool {
    name.eq_ignore_ascii_case(TAG)
}

/// Could `text` hold a block at all? A byte scan, so the egress walk over every
/// message of every response costs nothing for the ones that don't.
pub fn mentions(text: &str) -> bool {
    // Only the name right after each `{%` is read: the hub asks this of every
    // draft snapshot for every recipient, and a long transcript is mostly text.
    let b = text.as_bytes();
    text.match_indices("{%").any(|(at, _)| {
        let mut p = at + 2;
        while p < b.len() && (b[p].is_ascii_whitespace() || b[p] == b'/') {
            p += 1;
        }
        let start = p;
        while p < b.len() && p - start < 64 && (b[p].is_ascii_alphanumeric() || matches!(b[p], b'_' | b':' | b'-' | b'/')) {
            p += 1;
        }
        let name = &b[start..p];
        name.len() >= TAG.len() && name[name.len() - TAG.len()..].eq_ignore_ascii_case(TAG.as_bytes())
    })
}

/// The audience written on a tag (`for`), `owner` when absent or blank — in
/// the closed alphabet a marker is written in. A tag inside a JSON string (a
/// merge-forward's frozen record) has its quotes escaped; read through that.
pub fn audience(attrs: &str) -> String {
    let unescaped;
    let attrs = if attrs.contains("\\\"") {
        unescaped = attrs.replace("\\\"", "\"");
        unescaped.as_str()
    } else {
        attrs
    };
    spelled(attr(attrs, "for").unwrap_or(""))
}

/// Does `reader` belong to `audience`, for a message by `author`? `None` is
/// nobody — an anonymous page, or a forward (see [`hide_all`]).
pub fn admits(audience: &str, author: Author<'_>, reader: Option<&str>) -> bool {
    let Some(reader) = reader.map(|r| r.trim_start_matches('@')).filter(|r| !r.is_empty()) else {
        return false;
    };
    let is = |name: &str| reader.eq_ignore_ascii_case(name.trim_start_matches('@'));
    // The author always reads their own words (module rules).
    if is(author.sender) {
        return true;
    }
    audience.split(',').map(str::trim).filter(|w| !w.is_empty()).any(|who| {
        if who.starts_with('@') {
            is(who)
        } else if who.eq_ignore_ascii_case("owner") {
            is(author.owner)
        } else if who.eq_ignore_ascii_case("sender") {
            is(author.sender)
        } else {
            is(who) // a bare name: lenient, it can only ever name one account
        }
    })
}

/// What a reader outside `audience` gets in place of the block. The audience is
/// re-spelled from a closed alphabet — it came from the author, and it goes
/// back inside quotes.
pub fn marker(audience: &str) -> String {
    format!("{{% {TAG} for=\"{}\" hidden=\"true\" /%}}", spelled(audience))
}

/// `audience` in a closed alphabet (names, `@`, commas, spaces).
fn spelled(audience: &str) -> String {
    let clean: String = audience
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '@' | ',' | ' ' | ':' | '_' | '-' | '.'))
        .collect();
    match clean.trim() {
        "" => "owner".to_string(),
        c => c.to_string(),
    }
}

/// One block found in a text: where its open tag starts, where its body runs,
/// and where the whole span ends.
struct Block {
    start: usize,
    audience: String,
    /// `None` for a leaf (`{% mafold/only … /%}`).
    body: Option<(usize, usize)>,
    end: usize,
    /// Written with `\"` — it sits inside a JSON string.
    escaped: bool,
}

impl Block {
    /// The marker for this block, in the quoting its context needs.
    fn marker(&self) -> String {
        let m = marker(&self.audience);
        if self.escaped {
            m.replace('"', "\\\"")
        } else {
            m
        }
    }
}

/// The next `only` tag at or after `from` that opens a block (leaf or
/// container). Stray close tags are skipped — they are text.
fn next_block(text: &str, from: usize) -> Option<Block> {
    let mut i = from;
    while let Some(rel) = text[i..].find("{%") {
        let start = i + rel;
        let Some(tag) = parse_tag(text, start) else {
            i = start + 2;
            continue;
        };
        i = tag.end;
        if tag.is_close || !is_only(tag.name) {
            continue;
        }
        let audience = audience(tag.attrs);
        let escaped = tag.attrs.contains("\\\"");
        if tag.self_close {
            return Some(Block { start, audience, body: None, end: tag.end, escaped });
        }
        let (body_end, end) = matching_close(text, tag.end).unwrap_or((text.len(), text.len()));
        return Some(Block { start, audience, body: Some((tag.end, body_end)), end, escaped });
    }
    None
}

/// `(start, end)` of the close that matches an `only` opened just before
/// `from`, nested `only` containers counted. Code spans are NOT skipped —
/// see the module rules.
fn matching_close(text: &str, from: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut i = from;
    while let Some(rel) = text[i..].find("{%") {
        let start = i + rel;
        let Some(tag) = parse_tag(text, start) else {
            i = start + 2;
            continue;
        };
        i = tag.end;
        if !is_only(tag.name) {
            continue;
        }
        if tag.is_close {
            if depth == 0 {
                return Some((start, tag.end));
            }
            depth -= 1;
        } else if !tag.self_close {
            depth += 1;
        }
    }
    None
}

/// `text` as `reader` may read it (see the module rules). Borrows when nothing
/// is cut — the common case by far.
pub fn view_for<'a>(text: &'a str, author: Author<'_>, reader: Option<&str>) -> Cow<'a, str> {
    if !mentions(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::new();
    let mut copied = 0usize;
    let mut i = 0usize;
    while let Some(b) = next_block(text, i) {
        i = b.end;
        if admits(&b.audience, author, reader) {
            // In the audience: the block stays, but what it holds may be
            // another block for someone else.
            if let Some((from, to)) = b.body {
                if let Cow::Owned(inner) = view_for(&text[from..to], author, reader) {
                    out.push_str(&text[copied..from]);
                    out.push_str(&inner);
                    copied = to;
                }
            }
            continue;
        }
        let m = b.marker();
        if text[b.start..b.end] == m {
            continue; // already the marker — a forward, or a second pass
        }
        out.push_str(&text[copied..b.start]);
        out.push_str(&m);
        copied = b.end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// Every block hidden, whoever reads it. For a body that is about to be copied
/// under a NEW author — a forward, a merge-forward's frozen record: `owner` and
/// `sender` were relative to the original author, and re-reading them against
/// the forwarder would hand the block to whoever forwarded it and whoever owns
/// them. Forwarding never widens an audience; it drops the block.
pub fn hide_all(text: &str) -> Cow<'_, str> {
    view_for(text, Author::new("", None), None)
}

/// The block in words, for plain-text readers (`mafold read`): a hidden marker
/// becomes `[🔒 only owner]`, a block the reader may see keeps its text between
/// `[🔒 only owner]` and `[/🔒]` — so an agent reading it knows where the
/// restricted part ends and doesn't repeat it to the room.
pub fn readable(text: &str) -> Cow<'_, str> {
    if !mentions(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::new();
    let mut copied = 0usize;
    let mut i = 0usize;
    while let Some(b) = next_block(text, i) {
        out.push_str(&text[copied..b.start]);
        let who = &b.audience;
        match b.body {
            None => out.push_str(&format!("[🔒 only {who}]")),
            Some((from, to)) => {
                let inner = readable(&text[from..to]);
                out.push_str(&format!("[🔒 only {who}] {} [/🔒]", inner.trim()));
            }
        }
        copied = b.end;
        i = b.end;
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: Author<'static> = Author { sender: "ops:claude", owner: "ops" };

    fn cut(text: &str, reader: &str) -> String {
        view_for(text, BOT, Some(reader)).into_owned()
    }

    #[test]
    fn owner_reads_the_block_others_get_the_marker() {
        let msg = "hi\n{% mafold/only for=\"owner\" %}the summary{% /mafold/only %}\nbye";
        assert_eq!(cut(msg, "ops"), msg, "the owner gets it verbatim");
        let other = cut(msg, "linsky");
        assert_eq!(other, "hi\n{% mafold/only for=\"owner\" hidden=\"true\" /%}\nbye");
        assert!(!other.contains("summary"));
    }

    #[test]
    fn the_author_always_reads_its_own_words() {
        // A daemon stamping its own card reads its reply back and writes it
        // again — cut for the author, that write would erase the block.
        let msg = "{% mafold/only for=\"owner\" %}s{% /mafold/only %}";
        assert_eq!(cut(msg, "ops:claude"), msg);
        assert_eq!(cut(msg, "OPS:Claude"), msg, "usernames compare case-insensitively");
    }

    #[test]
    fn audiences_owner_sender_and_names() {
        let p = |f: &str| format!("{{% mafold/only for=\"{f}\" %}}x{{% /mafold/only %}}");
        // `sender` is the bot itself: its owner is NOT in it.
        assert!(cut(&p("sender"), "ops").contains("hidden"));
        // Names, with or without `@`, any case, spaces around commas.
        assert_eq!(cut(&p("@alice, @Bob"), "bob"), p("@alice, @Bob"));
        assert_eq!(cut(&p("alice"), "alice"), p("alice"));
        assert!(cut(&p("@alice"), "mallory").contains("hidden"));
        // `owner` of a person's message is the person.
        let human = Author::new("alice", None);
        assert_eq!(view_for(&p("owner"), human, Some("alice")), p("owner"));
        assert!(view_for(&p("owner"), human, Some("bob")).contains("hidden"));
        // Absent `for` means owner.
        let bare = "{% mafold/only %}x{% /mafold/only %}";
        assert_eq!(cut(bare, "ops"), bare);
        assert_eq!(cut(bare, "eve"), "{% mafold/only for=\"owner\" hidden=\"true\" /%}");
        // Nobody: an anonymous reader gets nothing.
        assert!(view_for(&p("owner"), BOT, None).contains("hidden"));
    }

    #[test]
    fn unclosed_block_runs_to_the_end() {
        // Mid-stream, or a misspelled close: the client gives the card the
        // body it has, so the cut must take all of it.
        let msg = "a {% mafold/only for=\"owner\" %}secret, still streaming";
        assert_eq!(cut(msg, "eve"), "a {% mafold/only for=\"owner\" hidden=\"true\" /%}");
        let typo = "a {% mafold/only %}secret{% /mafold/olny %} tail";
        assert!(!cut(typo, "eve").contains("secret"));
    }

    #[test]
    fn nested_blocks_and_blocks_inside_other_cards() {
        let msg = "{% mafold/compact before=\"9\" after=\"1\" %}\n{% mafold/only for=\"owner\" %}\nflag|x\nsum\n{% /mafold/only %}\n{% /mafold/compact %}";
        let other = cut(msg, "eve");
        assert_eq!(
            other,
            "{% mafold/compact before=\"9\" after=\"1\" %}\n{% mafold/only for=\"owner\" hidden=\"true\" /%}\n{% /mafold/compact %}"
        );
        assert_eq!(cut(msg, "ops"), msg);

        // Outer for alice and bob, inner for alice only: bob gets the outer
        // with the inner cut; the matching close is the OUTER one.
        let n = "{% mafold/only for=\"@alice,@bob\" %}A {% mafold/only for=\"@alice\" %}B{% /mafold/only %} C{% /mafold/only %} D";
        assert_eq!(cut(n, "alice"), n);
        assert_eq!(
            cut(n, "bob"),
            "{% mafold/only for=\"@alice,@bob\" %}A {% mafold/only for=\"@alice\" hidden=\"true\" /%} C{% /mafold/only %} D"
        );
        let eve = cut(n, "eve");
        assert_eq!(eve, "{% mafold/only for=\"@alice,@bob\" hidden=\"true\" /%} D");
    }

    #[test]
    fn spellings_code_spans_and_smuggled_attributes() {
        // Odd spacing and case still count as the card.
        let odd = "{%Mafold/Only   for=\"owner\"%}s{%  /MAFOLD/ONLY %}!";
        assert_eq!(cut(odd, "eve"), "{% mafold/only for=\"owner\" hidden=\"true\" /%}!");
        // Inside backticks it is cut too: erring wide is the safe direction.
        let code = "`{% mafold/only %}secret{% /mafold/only %}`";
        assert!(!cut(code, "eve").contains("secret"));
        // A leaf is normalized for outsiders: an attribute is content as well.
        let leaf = "{% mafold/only for=\"owner\" body=\"secret\" /%}";
        assert_eq!(cut(leaf, "eve"), "{% mafold/only for=\"owner\" hidden=\"true\" /%}");
        assert_eq!(cut(leaf, "ops"), leaf);
        // A `for` that tries to break out of its quotes is sanitized.
        let evil = "{% mafold/only for='a\" hidden=\"false' %}x{% /mafold/only %}";
        assert_eq!(cut(evil, "eve"), "{% mafold/only for=\"a hiddenfalse\" hidden=\"true\" /%}");
    }

    #[test]
    fn markers_are_stable_and_untouched_text_is_borrowed() {
        let m = marker("owner");
        assert!(matches!(view_for(&m, BOT, Some("eve")), Cow::Borrowed(_)), "cutting a marker again changes nothing");
        assert!(matches!(view_for("plain text, {% mafold/quote x=1 /%}", BOT, Some("eve")), Cow::Borrowed(_)));
        assert!(mentions("{% mafold/only %}") && mentions("{%/MAFOLD/ONLY%}"));
        assert!(!mentions("only words") && !mentions("{% mafold/quote %}"));
    }

    #[test]
    fn hide_all_drops_every_block_whoever_reads() {
        let msg = "fwd {% mafold/only for=\"sender\" %}mine{% /mafold/only %} {% mafold/only for=\"@x\" /%}";
        assert_eq!(
            hide_all(msg),
            "fwd {% mafold/only for=\"sender\" hidden=\"true\" /%} {% mafold/only for=\"@x\" hidden=\"true\" /%}"
        );
    }

    /// A merge-forward freezes each message as a JSON string inside the
    /// chatrecord card's body, so a block there is spelled `for=\"owner\"`.
    /// The cut must answer in the same spelling — a raw `"` would end the JSON
    /// string and the whole record would stop parsing.
    #[test]
    fn a_block_inside_a_json_string_stays_json() {
        let entry = serde_json::json!([{ "content": "a {% mafold/only for=\"@alice\" %}pw{% /mafold/only %} b" }]);
        let body = format!("{{% mafold/chatrecord %}}{entry}{{% /mafold/chatrecord %}}");
        let out = view_for(&body, BOT, Some("eve")).into_owned();
        let json = &out["{% mafold/chatrecord %}".len()..out.len() - "{% /mafold/chatrecord %}".len()];
        let back: serde_json::Value = serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {json}"));
        assert_eq!(back[0]["content"], "a {% mafold/only for=\"@alice\" hidden=\"true\" /%} b");
        // Its reader is still admitted through the escaping, and a second
        // pass over the escaped marker changes nothing.
        assert_eq!(view_for(&body, BOT, Some("alice")), body);
        assert!(matches!(view_for(&out, BOT, Some("eve")), Cow::Borrowed(_)));
        assert_eq!(hide_all(&body), out);
    }

    #[test]
    fn readable_tokens() {
        assert_eq!(readable("a {% mafold/only for=\"owner\" hidden=\"true\" /%} b"), "a [🔒 only owner] b");
        assert_eq!(
            readable("{% mafold/only for=\"@alice,@bob\" %}pw{% /mafold/only %}"),
            "[🔒 only @alice,@bob] pw [/🔒]"
        );
        assert!(matches!(readable("nothing here"), Cow::Borrowed(_)));
    }
}
