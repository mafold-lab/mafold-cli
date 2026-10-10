//! Row previews: publisher metadata or slug, never a per-card label table.

pub const PREVIEW_CHARS: usize = 240;

/// Only the requested version matters here; instance props such as
/// name/summary are not metadata.
fn version(attrs: &str) -> Option<&str> {
    attr(attrs, "version")
}

/// One attribute of a tag's raw attribute run. Mirrors the generic attribute
/// reader in cards/split.ts, so a value means here what it means to the card.
pub(crate) fn attr<'a>(attrs: &'a str, want: &str) -> Option<&'a str> {
    let mut rest = attrs;
    let mut found = None;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let Some(start) = rest.find(|c: char| c.is_ascii_alphabetic()) else {
            break;
        };
        rest = &rest[start..];
        let n = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(rest.len());
        let key = &rest[..n];
        rest = rest[n..].trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        rest = rest[1..].trim_start();
        let value;
        if rest.starts_with(['\"', '\'']) {
            let quote = rest.as_bytes()[0] as char;
            rest = &rest[1..];
            let Some(end) = rest.find(quote) else { break };
            value = &rest[..end];
            rest = &rest[end + 1..];
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            value = &rest[..end];
            rest = &rest[end..];
        }
        if key == want {
            found = Some(value);
        }
    }
    found
}

/// What a row preview says for one card, per its publisher's metadata.
pub enum CardLabel {
    /// The card's published name (already in the reader's language, if the
    /// publisher gave one). Blank reads as unknown ⇒ the slug.
    Name(String),
    /// The publisher opted the card out of previews (`"preview": false`) —
    /// chrome such as a reply's usage footer, which says nothing about what
    /// was said. Every bot reply ended its row with "… Result" before this.
    ///
    /// It still carries its published name (blank ⇒ the slug): hidden means
    /// "don't add yourself to what was said", not "make a message that is
    /// nothing but you say nothing". A turn that ran its tools and answered
    /// nothing is all trace and stamp; quoted, it read "Attachment" — it had
    /// none (2026-10-10). With nothing else to say, its first card names it.
    Hidden(String),
}

pub fn message_preview(
    text: &str,
    label: impl FnMut(&str, Option<&str>) -> Option<CardLabel>,
) -> String {
    message_text(text, label).chars().take(PREVIEW_CHARS).collect()
}

/// [`message_preview`] before the cut: the whole text, said the same way —
/// for a surface that shows ALL of a message's words (the garden's quotes),
/// where the row's 240 characters would lose the rest.
pub fn message_text(
    text: &str,
    mut label: impl FnMut(&str, Option<&str>) -> Option<CardLabel>,
) -> String {
    // The first hidden card's name: what the message is called when nothing
    // else in it says anything.
    let mut unsaid: Option<String> = None;
    let named = crate::prose::map_card_text(text, preview_prose, |tag, attrs| {
        // A bare tag (`{% result /%}`) is from before `owner/slug`, when every
        // card was Mafold's: its name and its opt-out are those of the official
        // card it was — `result` is the usage footer, hidden, not the word
        // "result" at the end of a quote (2026-10-07). Names only: rendering
        // still never resolves a bare tag.
        let tag = official(tag);
        let slug = || tag.rsplit('/').next().unwrap_or(&tag).to_owned();
        match label(&tag, version(attrs)) {
            Some(CardLabel::Hidden(name)) => {
                unsaid.get_or_insert_with(|| if name.trim().is_empty() { slug() } else { name });
                String::new()
            }
            Some(CardLabel::Name(name)) if !name.trim().is_empty() => name,
            _ => slug(),
        }
    });
    let said = named.split_whitespace().collect::<Vec<_>>().join(" ");
    if said.is_empty() { unsaid.map(|n| n.trim().to_owned()).unwrap_or_default() } else { said }
}

/// The tag a preview names a card by: `owner/slug` as written, a bare tag as
/// the official card it predates (`result` → `mafold/result`). Mirrors
/// `previewTag` in `cards/split.ts`.
pub fn official(tag: &str) -> std::borrow::Cow<'_, str> {
    if tag.contains('/') {
        std::borrow::Cow::Borrowed(tag)
    } else {
        std::borrow::Cow::Owned(format!("mafold/{tag}"))
    }
}

/// A text run as a preview says it: markdown's marks (`*`, `#`, `>`, the
/// backticks) dropped, but what code QUOTES kept — `开成 `*`` says `*`, it
/// used to say nothing ("设成 即可"). Code is [`crate::prose::code_ranges`],
/// the renderer's own rule. A table says its cells: the bubble draws it as a
/// table, so its pipes and its `|---|` row are marks too — a reply that
/// answered with one used to preview as "| | | |---|---| | 搬了什么 |"
/// (linsky 2026-10-07). Mirrors `previewProse` in `cards/split.ts`.
fn preview_prose(prose: &str) -> String {
    let code = crate::prose::code_ranges(prose);
    let rows = table_rows(prose);
    let mut row = rows.iter().peekable(); // in order: the one `i` is in or the next
    let mut out = String::with_capacity(prose.len());
    let mut chars = prose.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        while row.next_if(|&&(_, b, _)| b <= i).is_some() {}
        if let Some(&&(_, _, delimiter)) = row.peek().filter(|&&&(a, _, _)| a <= i) {
            if delimiter {
                continue;
            }
            // `\|` is a pipe in a cell's text (`\\` a backslash, escaping
            // nothing); any other pipe ends a cell — inside a code span too,
            // as GFM splits cells before inlines.
            if c == '\\' {
                match chars.peek() {
                    Some(&(_, '|')) => {
                        out.push('|');
                        chars.next();
                        continue;
                    }
                    Some(&(_, '\\')) => {
                        out.push_str("\\\\");
                        chars.next();
                        continue;
                    }
                    _ => {}
                }
            }
            if c == '|' {
                out.push(' ');
                continue;
            }
        }
        if c != '`' && (!matches!(c, '*' | '#' | '>') || crate::prose::in_code(i, &code)) {
            out.push(c);
        }
    }
    out
}

/// The lines of `s` that are GFM tables, as `(start, end, is the delimiter
/// row)` byte ranges (no newline): a header row, then a delimiter row with as
/// many cells (`|---|:--:|`), then every line up to a blank one or one that
/// starts another block — what remark-gfm draws as a table. A line in a fenced
/// block is never part of one; inline code isn't read until the rows are cut.
fn table_rows(s: &str) -> Vec<(usize, usize, bool)> {
    let fenced = crate::prose::fenced_ranges(s);
    let mut lines = Vec::new();
    let mut pos = 0;
    loop {
        let end = s[pos..].find('\n').map_or(s.len(), |n| pos + n);
        lines.push((pos, end));
        if end == s.len() {
            break;
        }
        pos = end + 1;
    }
    let text = |(a, b): (usize, usize)| s[a..b].strip_suffix('\r').unwrap_or(&s[a..b]);
    let free = |(at, _): (usize, usize)| !crate::prose::in_code(at, &fenced);
    let mut rows = Vec::new();
    let mut j = 0;
    while j + 1 < lines.len() {
        let (head, delimiter) = (lines[j], lines[j + 1]);
        let n = header_cells(text(head));
        if n.is_none() || n != delimiter_cells(text(delimiter)) || !free(head) || !free(delimiter) {
            j += 1;
            continue;
        }
        rows.push((head.0, head.1, false));
        rows.push((delimiter.0, delimiter.1, true));
        j += 2;
        while j < lines.len() && !blank(text(lines[j])).is_empty() && !starts_block(text(lines[j])) && free(lines[j]) {
            rows.push((lines[j].0, lines[j].1, false));
            j += 1;
        }
    }
    rows
}

/// `line` without the blanks GFM means at its ends (spaces and tabs) — not
/// Rust's or JavaScript's idea of whitespace, which differ from each other.
fn blank(line: &str) -> &str {
    line.trim_matches(|c| matches!(c, ' ' | '\t' | '\r'))
}

/// A line that begins another block, which ends a table (GFM): a quote, a
/// heading, a list item, a fence, a thematic break.
fn starts_block(line: &str) -> bool {
    let indent = line.bytes().take_while(|&c| c == b' ' || c == b'\t').count();
    if indent > 3 {
        return false;
    }
    let b = &line.as_bytes()[indent..];
    let spaced = |n: usize| b.get(n).is_none_or(|&c| c == b' ' || c == b'\t');
    let run = |f: fn(&u8) -> bool| b.iter().take_while(|c| f(c)).count();
    let thematic = || {
        let marks: Vec<u8> = b.iter().copied().filter(|&c| c != b' ' && c != b'\t').collect();
        marks.len() >= 3 && matches!(marks[0], b'-' | b'*' | b'_') && marks.iter().all(|&c| c == marks[0])
    };
    match b.first() {
        Some(b'>') => true,
        Some(b'#') => {
            let n = run(|&c| c == b'#');
            n <= 6 && spaced(n)
        }
        Some(b'-' | b'*' | b'+') if spaced(1) => true,
        Some(c) if c.is_ascii_digit() => {
            let n = run(u8::is_ascii_digit);
            n <= 9 && matches!(b.get(n), Some(b'.' | b')')) && spaced(n + 1)
        }
        _ => crate::prose::fence_mark(line).is_some() || thematic(),
    }
}

/// A row's cells, split at its unescaped pipes — the outer pipes are optional.
fn cells(line: &str) -> Vec<&str> {
    let t = blank(line);
    let t = t.strip_prefix('|').unwrap_or(t);
    // The closing pipe is a pipe only if an even run of backslashes is before it.
    let escapes = t.strip_suffix('|').map(|r| r.bytes().rev().take_while(|&c| c == b'\\').count());
    let t = match escapes {
        Some(n) if n % 2 == 0 => &t[..t.len() - 1],
        _ => t,
    };
    let mut out = Vec::new();
    let (mut from, mut escaped) = (0, false);
    for (i, c) in t.char_indices() {
        match c {
            '|' if !escaped => {
                out.push(&t[from..i]);
                from = i + 1;
            }
            _ => {}
        }
        escaped = c == '\\' && !escaped;
    }
    out.push(&t[from..]);
    out
}

/// How many cells a header row has; `None` when the line has no unescaped
/// pipe (it can't head a table).
fn header_cells(line: &str) -> Option<usize> {
    let n = cells(line).len();
    let piped = blank(line).starts_with('|') || n > 1;
    (piped && !blank(line).is_empty()).then_some(n)
}

/// How many cells a delimiter row (`|---|:--:|`) has; `None` when it isn't one.
fn delimiter_cells(line: &str) -> Option<usize> {
    if !line.contains('|') {
        return None;
    }
    let cells = cells(line);
    cells
        .iter()
        .all(|c| {
            let c = blank(c);
            let c = c.strip_prefix(':').unwrap_or(c);
            let c = c.strip_suffix(':').unwrap_or(c);
            !c.is_empty() && c.bytes().all(|b| b == b'-')
        })
        .then_some(cells.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_client_preview_fixtures() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/preview-fixtures.json")).unwrap();
        for case in fixtures.as_array().unwrap() {
            let hidden: Vec<&str> = case["hidden"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let out = message_preview(case["content"].as_str().unwrap(), |tag, version| {
                let key = version.map_or_else(|| tag.to_owned(), |v| format!("{tag}@{v}"));
                let name = case["names"][&key].as_str();
                if hidden.contains(&tag) {
                    return Some(CardLabel::Hidden(name.unwrap_or_default().to_owned()));
                }
                name.map(|n| CardLabel::Name(n.to_owned()))
            });
            assert_eq!(out, case["expected"].as_str().unwrap(), "{}", case["name"]);
            assert!(out.chars().count() <= PREVIEW_CHARS);
        }
    }

    /// The uncut text is the preview before its cut — one reading, two lengths.
    #[test]
    fn message_text_is_the_preview_uncut() {
        let body = format!("{} {{% a/run %}}inside{{% /a/run %}} **end**", "词".repeat(300));
        let name = |_: &str, _: Option<&str>| Some(CardLabel::Name("Run".into()));
        let whole = message_text(&body, name);
        assert_eq!(whole, format!("{} Run end", "词".repeat(300)));
        assert_eq!(message_preview(&body, name), whole.chars().take(PREVIEW_CHARS).collect::<String>());
    }
}
