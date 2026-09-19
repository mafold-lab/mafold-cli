//! Row previews: publisher metadata or slug, never a per-card label table.

pub const PREVIEW_CHARS: usize = 240;

/// Mirrors the generic attribute reader in cards/split.ts. Only the requested
/// version matters here; instance props such as name/summary are not metadata.
fn version(attrs: &str) -> Option<&str> {
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
        if key == "version" {
            found = Some(value);
        }
    }
    found
}

pub fn message_preview(
    text: &str,
    mut display_name: impl FnMut(&str, Option<&str>) -> Option<String>,
) -> String {
    let named = crate::prose::map_card_text(
        text,
        |prose| {
            prose
                .chars()
                .filter(|c| !matches!(c, '*' | '`' | '#' | '>'))
                .collect()
        },
        |tag, attrs| {
            display_name(tag, version(attrs))
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| tag.rsplit('/').next().unwrap_or(tag).to_owned())
        },
    );
    named
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(PREVIEW_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_client_preview_fixtures() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../tests/preview-fixtures.json")).unwrap();
        for case in fixtures.as_array().unwrap() {
            let out = message_preview(case["content"].as_str().unwrap(), |tag, version| {
                let key = version.map_or_else(|| tag.to_owned(), |v| format!("{tag}@{v}"));
                case["names"][&key].as_str().map(str::to_owned)
            });
            assert_eq!(out, case["expected"].as_str().unwrap(), "{}", case["name"]);
            assert!(out.chars().count() <= PREVIEW_CHARS);
        }
    }
}
