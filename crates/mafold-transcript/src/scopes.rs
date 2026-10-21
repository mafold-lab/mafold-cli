//! Which card in a message a controlled-state key belongs to — the server's
//! side of `client-shared/cardState.ts` `cardScopes`, read the same way.
//!
//! A key is `<tag>/<anchor>.<field>` (`.docs/card-state-v1.md` §3.2): the tag
//! as written, and the anchor the card's `sid` attribute when it has one, else
//! its ordinal among the cards with the same tag in this message, from 0, in
//! document order — a container before the cards inside it, nested cards
//! counted. The clients compute the same anchor for the same card, so a key
//! the server writes is the key the card reads.
//!
//! This only READS the message: it never changes a byte of it.

use crate::prose::{close_at, code_ranges, in_code, orphan_close, parse_tag};
use std::collections::HashMap;

/// One card in a message, and its state-key prefix.
#[derive(Debug, Clone, PartialEq)]
pub struct CardAt<'a> {
    /// The tag as written (`mafold/connection-grant`, `wallet/redpacket`).
    pub tag: &'a str,
    /// Its attribute run, as written.
    pub attrs: &'a str,
    /// `<tag>/<anchor>` — the prefix of its keys.
    pub scope: String,
}

impl CardAt<'_> {
    /// One attribute's value.
    pub fn attr(&self, name: &str) -> Option<&str> {
        crate::preview::attr(self.attrs, name)
    }
}

/// Every card in `md`, in document order, with its scope.
pub fn card_scopes(md: &str) -> Vec<CardAt<'_>> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::new();
    walk(md, &mut counts, &mut out);
    out
}

fn walk<'a>(md: &'a str, counts: &mut HashMap<&'a str, usize>, out: &mut Vec<CardAt<'a>>) {
    let ranges_in = |from: usize, to: usize| -> Vec<(usize, usize)> {
        code_ranges(&md[from..to]).into_iter().map(|(a, b)| (a + from, b + from)).collect()
    };
    let mut ranges = code_ranges(md);
    let mut i = 0usize;
    while let Some(rel) = md[i..].find("{%") {
        let start = i + rel;
        let Some(tag) = parse_tag(md, start) else {
            i = start + 2;
            continue;
        };
        i = tag.end;
        if tag.is_close || in_code(start, &ranges) {
            continue;
        }
        let n = counts.entry(tag.name).or_insert(0);
        let ordinal = *n;
        *n += 1;
        let attrs = tag.attrs.trim_end().trim_end_matches('/');
        let anchor = crate::preview::attr(attrs, "sid").filter(|s| !s.is_empty()).map(str::to_string);
        out.push(CardAt {
            tag: tag.name,
            attrs,
            scope: format!("{}/{}", tag.name, anchor.unwrap_or_else(|| ordinal.to_string())),
        });
        if tag.self_close {
            continue;
        }
        let (body_end, end) = close_at(md, tag.end, tag.name)
            .or_else(|| orphan_close(md, tag.end, &ranges, tag.name))
            .unwrap_or((md.len(), md.len()));
        // The cards inside count on from here, before the next sibling.
        walk(&md[tag.end..body_end], counts, out);
        if md[start..end].contains(['`', '~']) {
            ranges = ranges_in(end, md.len());
        }
        i = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(md: &str) -> Vec<String> {
        card_scopes(md).into_iter().map(|c| c.scope).collect()
    }

    /// The same cases `client-shared/cardState.ts` is tested on
    /// (`mafold-web/src/app/app/lib/cardState.spec.ts`): the two must agree.
    #[test]
    fn same_tag_cards_are_numbered_from_zero_each_tag_apart() {
        assert_eq!(
            scopes("{% mafold/result a=\"1\" /%}\ntext\n{% quote /%}\n{% mafold/result a=\"2\" /%}"),
            ["mafold/result/0", "quote/0", "mafold/result/1"]
        );
    }

    #[test]
    fn a_sid_is_the_anchor_and_still_takes_an_ordinal() {
        assert_eq!(scopes("{% mafold/result sid=\"fee\" /%}\n{% mafold/result /%}"), ["mafold/result/fee", "mafold/result/1"]);
    }

    #[test]
    fn nested_cards_count_in_document_order_container_first() {
        assert_eq!(
            scopes("{% run %}\n{% tool /%}\n{% tool /%}\n{% /run %}\n{% tool /%}"),
            ["run/0", "tool/0", "tool/1", "tool/2"]
        );
    }

    #[test]
    fn code_is_not_cards_and_attributes_are_readable() {
        let md = "`{% wallet/redpacket id=\"x\" /%}` then {% wallet/redpacket id=\"y\" amount=\"5\" /%}";
        let cards = card_scopes(md);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].scope, "wallet/redpacket/0");
        assert_eq!(cards[0].attr("id"), Some("y"));
        assert_eq!(cards[0].attr("amount"), Some("5"));
    }
}
