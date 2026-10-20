//! A message as an AGENT reads it in its history: the way a person sees a chat
//! they scroll back through. Every card is still there — its tag and its
//! attributes, which is the line a person reads on the collapsed card
//! (`summary="Ran 11 shell commands"`, `duration="12.6s" cost="$5.59"`) — and
//! its body is folded into one note saying how much is behind it and how to
//! open it. Prose outside the cards is not touched.
//!
//! One rule for every card, not a list of which cards are "machinery": a run
//! group, a result stamp, an html page the agent delivered and a forwarded
//! record all fold the same way, because a person sees all of them folded and
//! opens the one they need. Opening is a read of the message itself
//! (`mafold msg <id>` for the daemon's agents, `get_message` for the hosted
//! brains) — what it returns is the message exactly as stored.
//!
//! Before this the history CUT the transcript cards with their bodies and left
//! nothing behind: an agent's turn read as «—», and the agent could not know
//! there was anything to look up. The cards that weren't cut kept their whole
//! bodies, so one delivered page cost a row 8–24k characters.

use crate::prose::{close_at, code_ranges, in_code, orphan_close, parse_tag};

/// How long one attribute value may be before it is cut (chars). Long enough
/// for every summary line a card shows, short enough that a card carrying a
/// JSON blob in an attribute doesn't carry it into every history row.
pub const ATTR_MAX: usize = 200;

/// `md` with every top-level card folded. `open` is how to read the whole
/// message (e.g. ``"`mafold msg 3f2c…`"``), quoted in every fold note; `None`
/// where there is no way to (the note then only says what was folded).
///
/// The card grammar is the renderer's ([`crate::prose`], the twin of
/// `cards/split.ts`): a tag in a fence or a backtick span is code and stays as
/// written; a container runs to its close, else to the close its author
/// misspelled, else to the end of the message.
pub fn fold(md: &str, open: Option<&str>) -> String {
    let ranges_in = |from: usize, to: usize| -> Vec<(usize, usize)> {
        code_ranges(&md[from..to]).into_iter().map(|(a, b)| (a + from, b + from)).collect()
    };
    let mut ranges = code_ranges(md);
    let mut out = String::with_capacity(md.len().min(8192));
    let mut copied = 0usize;
    let mut i = 0usize;
    while let Some(rel) = md[i..].find("{%") {
        let start = i + rel;
        let Some(tag) = parse_tag(md, start) else {
            i = start + 2;
            continue;
        };
        i = tag.end;
        // A stray close at the top level is text someone wrote; code is code.
        if tag.is_close || in_code(start, &ranges) {
            continue;
        }
        let (body_end, end) = if tag.self_close {
            (tag.end, tag.end)
        } else {
            close_at(md, tag.end, tag.name)
                .or_else(|| orphan_close(md, tag.end, &ranges, tag.name))
                .unwrap_or((md.len(), md.len()))
        };
        out.push_str(&md[copied..start]);
        out.push_str(&folded_card(tag.name, tag.attrs, tag.self_close, &md[tag.end..body_end], open));
        // A backtick inside the card's span says nothing about code after it.
        if md[start..end].contains(['`', '~']) {
            ranges = ranges_in(end, md.len());
        }
        copied = end;
        i = end;
    }
    out.push_str(&md[copied..]);
    out
}

fn folded_card(name: &str, attrs: &str, self_close: bool, body: &str, open: Option<&str>) -> String {
    let mut a = attrs.trim_end();
    if self_close {
        a = a.strip_suffix('/').unwrap_or(a).trim_end();
    }
    let mut a = shorten_attrs(a);
    if !a.is_empty() && !a.starts_with(char::is_whitespace) {
        a.insert(0, ' ');
    }
    if self_close {
        return format!("{{% {name}{a} /%}}");
    }
    let body = body.trim();
    if body.is_empty() {
        return format!("{{% {name}{a} %}}{{% /{name} %}}");
    }
    format!("{{% {name}{a} %}}\n{}\n{{% /{name} %}}", note(body, open))
}

/// The line that stands in for a folded body.
fn note(body: &str, open: Option<&str>) -> String {
    let chars = body.chars().count();
    let cards = count_cards(body);
    let mut s = format!("[folded: {chars} chars");
    if cards > 0 {
        s.push_str(&format!(", {cards} card{} inside", if cards == 1 { "" } else { "s" }));
    }
    match open {
        Some(o) => s.push_str(&format!(" — full text: {o}]")),
        None => s.push(']'),
    }
    s
}

/// Cards opened inside a body, at any depth — what a person would find on
/// expanding it (`Ran 11 shell commands` holds eleven).
fn count_cards(body: &str) -> usize {
    let ranges = code_ranges(body);
    let mut n = 0;
    let mut i = 0;
    while let Some(rel) = body[i..].find("{%") {
        let start = i + rel;
        let Some(tag) = parse_tag(body, start) else {
            i = start + 2;
            continue;
        };
        i = tag.end;
        if !tag.is_close && !in_code(start, &ranges) {
            n += 1;
        }
    }
    n
}

/// Every attribute value longer than [`ATTR_MAX`] cut, with how much went.
/// Quoted values keep their quotes (the tag stays a tag); a bracketed value
/// (`data={…}`, `rows=[…]`) is cut the same way — it is read, not parsed.
fn shorten_attrs(attrs: &str) -> String {
    let chars: Vec<char> = attrs.chars().collect();
    let mut out = String::with_capacity(attrs.len().min(4 * ATTR_MAX));
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    if chars[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                let close = j.min(chars.len());
                out.push('"');
                push_cut(&mut out, &chars[i + 1..close], true);
                if j < chars.len() {
                    out.push('"');
                }
                i = j + 1;
            }
            '{' | '[' => {
                let (mut depth, mut j, mut in_str) = (0i32, i, false);
                while j < chars.len() {
                    let c = chars[j];
                    if in_str {
                        if c == '\\' {
                            j += 1;
                        } else if c == '"' {
                            in_str = false;
                        }
                    } else {
                        match c {
                            '"' => in_str = true,
                            '{' | '[' => depth += 1,
                            '}' | ']' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    j += 1;
                }
                let end = (j + 1).min(chars.len());
                push_cut(&mut out, &chars[i..end], false);
                i = end;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn push_cut(out: &mut String, val: &[char], quoted: bool) {
    if val.len() <= ATTR_MAX {
        out.extend(val);
        return;
    }
    let mut keep = &val[..ATTR_MAX];
    // Never end inside a quoted value on a lone backslash: it would escape the
    // closing quote and swallow the rest of the tag.
    if quoted {
        let trailing = keep.iter().rev().take_while(|c| **c == '\\').count();
        if trailing % 2 == 1 {
            keep = &keep[..keep.len() - 1];
        }
    }
    out.extend(keep);
    out.push_str(&format!("…[+{} chars]", val.len() - keep.len()));
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Option<&str> = Some("`mafold msg m1`");

    #[test]
    fn a_container_keeps_its_tag_and_attributes_and_folds_its_body() {
        let md = "改好了。\n{% mafold/run summary=\"Ran 2 shell commands\" %}\n{% mafold/bash cmd=\"ls\" /%}\n{% mafold/bash cmd=\"cargo test\" /%}\n{% /mafold/run %}\n两道门都加上了。";
        let out = fold(md, OPEN);
        assert_eq!(
            out,
            "改好了。\n{% mafold/run summary=\"Ran 2 shell commands\" %}\n[folded: 63 chars, 2 cards inside — full text: `mafold msg m1`]\n{% /mafold/run %}\n两道门都加上了。"
        );
    }

    #[test]
    fn every_card_folds_the_same_way_content_or_machinery() {
        let md = "Here is the page:\n{% mafold/html %}<html>…a long page…</html>{% /mafold/html %}\n{% mafold/result duration=\"12.6s\" cost=\"$5.59\" %}\n{\"input_tokens\":848642}\n{% /mafold/result %}";
        let out = fold(md, OPEN);
        assert!(out.starts_with("Here is the page:\n{% mafold/html %}\n[folded: "), "{out}");
        assert!(out.contains("{% mafold/result duration=\"12.6s\" cost=\"$5.59\" %}\n[folded: "), "{out}");
        for gone in ["<html>", "input_tokens"] {
            assert!(!out.contains(gone), "{gone} survived: {out}");
        }
    }

    #[test]
    fn self_closing_and_empty_cards_stay_as_they_are() {
        assert_eq!(fold("a {% quote symbol=\"AAPL\" /%} b", OPEN), "a {% quote symbol=\"AAPL\" /%} b");
        assert_eq!(fold("{% mafold/todo %}\n\n{% /mafold/todo %}", OPEN), "{% mafold/todo %}{% /mafold/todo %}");
    }

    #[test]
    fn a_long_attribute_is_cut_and_says_how_much() {
        let long = "x".repeat(ATTR_MAX + 50);
        let out = fold(&format!("{{% mafold/stats data=\"{long}\" kind=\"day\" /%}}"), OPEN);
        assert_eq!(out, format!("{{% mafold/stats data=\"{}…[+50 chars]\" kind=\"day\" /%}}", "x".repeat(ATTR_MAX)));
        let json = format!("{{\"rows\":[{}]}}", "1,".repeat(ATTR_MAX));
        let out = fold(&format!("{{% mafold/chart data={json} title=\"t\" /%}}"), OPEN);
        assert!(out.contains("…[+") && out.ends_with(" title=\"t\" /%}"), "{out}");
    }

    #[test]
    fn a_cut_never_leaves_a_lone_backslash_before_the_closing_quote() {
        let v = format!("{}\\\"tail{}", "a".repeat(ATTR_MAX - 1), "b".repeat(20));
        let out = fold(&format!("{{% x note=\"{v}\" /%}}"), None);
        assert!(out.contains(&format!("{}…[+", "a".repeat(ATTR_MAX - 1))), "{out}");
        assert!(out.ends_with("\" /%}"), "{out}");
    }

    #[test]
    fn tags_in_code_are_code() {
        let md = "use `{% mafold/run %}` like this:\n```\n{% mafold/html %}x{% /mafold/html %}\n```\nok";
        assert_eq!(fold(md, OPEN), md);
    }

    #[test]
    fn an_unclosed_container_folds_to_the_end_and_is_closed() {
        let out = fold("still going\n{% mafold/trace %}\nstep 1\nstep 2", OPEN);
        assert_eq!(out, "still going\n{% mafold/trace %}\n[folded: 13 chars — full text: `mafold msg m1`]\n{% /mafold/trace %}");
    }

    #[test]
    fn without_a_way_to_open_it_the_note_only_says_what_was_folded() {
        assert_eq!(fold("{% mafold/html %}<b>x</b>{% /mafold/html %}", None), "{% mafold/html %}\n[folded: 8 chars]\n{% /mafold/html %}");
    }

    #[test]
    fn plain_text_and_stray_closes_are_untouched() {
        for md in ["just words", "a {% /mafold/run %} b", "{% not closed at all"] {
            assert_eq!(fold(md, OPEN), md);
        }
    }
}
