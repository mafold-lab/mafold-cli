//! The card catalog an agent is handed — which published cards it may write
//! in a reply, and what each one is for.
//!
//! ONE builder for both runtimes: the self-hosted daemon's preamble
//! (`mafold-cli`, `mafold_preamble`) and a hosted bot's platform section
//! (`mafold-api`, `brains/platform.rs`). Before this, each listed the registry
//! by NAME only, and the two lists disagreed on what to leave out. A card's
//! manifest has carried an `agent` block since #985 — `{doc, example?, flag?,
//! offer?}`, "the prompt explains each card from its own manifest instead of
//! by hand-written name" — but nothing read it. So on 2026-10-29 @claude, asked
//! for pictures of something, saw `mafold/media` as a bare word next to a
//! paragraph telling it to build HTML for anything visual, and drew a logo grid
//! in `mafold/html`. The card made for the job never reached it as more than
//! its name.
//!
//! The rules live here, and only here:
//!   * transcript machinery ([`crate::render::is_transcript_card`]) is what the
//!     platform writes AROUND a reply — never on offer;
//!   * `agent.offer: false` — a card the host mounts itself (the activity
//!     center's tabs) — never on offer;
//!   * `agent.flag` — on offer only while that flag is on for whoever the turn
//!     answers to. The caller says who that is and asks the server; an agent
//!     told about a card whose service is shut for this reader writes one that
//!     renders empty;
//!   * everything else is on offer, with its `doc` and `example` when the
//!     manifest has them.

use serde_json::Value;

/// One card an agent may write this turn, with what its manifest tells agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedCard {
    /// Fully qualified `owner/slug` — a bare slug renders as "Unsupported card".
    pub id: String,
    /// `agent.doc`: what the card is and when to reach for it.
    pub doc: Option<String>,
    /// `agent.example`: the card written out, ready to copy.
    pub example: Option<String>,
}

/// Is `id` on offer to an agent, given its manifest's `agent` block and
/// whether a named flag is on for whoever the turn answers to? `None` = leave
/// it out of the catalog entirely.
pub fn offer(id: &str, agent: Option<&Value>, flag_on: impl Fn(&str) -> bool) -> Option<OfferedCard> {
    if crate::render::is_transcript_card(id) {
        return None;
    }
    let agent = agent.filter(|a| a.is_object());
    let field = |k: &str| agent.and_then(|a| a.get(k));
    if field("offer").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    if let Some(flag) = field("flag").and_then(Value::as_str).filter(|f| !f.is_empty()) {
        if !flag_on(flag) {
            return None;
        }
    }
    let text = |k: &str| {
        field(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    Some(OfferedCard { id: id.to_string(), doc: text("doc"), example: text("example") })
}

/// The catalog as prompt text: every card with notes, each followed by its
/// example on lines of its own (a card body must start at the line's first
/// column, so nothing is indented), then the rest by name. Empty for no cards.
pub fn render(cards: &[OfferedCard]) -> String {
    let (noted, named): (Vec<&OfferedCard>, Vec<&OfferedCard>) = cards.iter().partition(|c| c.doc.is_some());
    let mut s = String::new();
    if !noted.is_empty() {
        s.push_str(
            "Cards made for a job — when one fits what you are showing, write THAT card; don't \
             rebuild it in HTML or approximate it in markdown:\n",
        );
        for c in &noted {
            s.push_str(&format!("• {} — {}", c.id, c.doc.as_deref().unwrap_or_default()));
            match c.example.as_deref() {
                Some(ex) => s.push_str(&format!(" Example:\n{ex}\n")),
                None => s.push('\n'),
            }
        }
    }
    if !named.is_empty() {
        s.push_str(if noted.is_empty() { "Cards here" } else { "Also here" });
        s.push_str(" (each takes its own attributes): ");
        s.push_str(&named.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", "));
        s.push('.');
    }
    s.trim_end().to_string()
}

/// What a `mafold/html` page can reach — one sentence for every runtime. The
/// hosted prompt said "there is no network" for a week after #1048 found
/// that false and fixed only the daemon's copy; a model that believes it
/// can't link a picture embeds the picture instead (see [`PICTURES_BY_URL`]).
pub const HTML_NETWORK: &str = "Scripts run in an isolated origin — no access to the app, its \
cookies or storage — but the network is reachable, so a chart library from a CDN or a picture by \
URL loads.";

/// 2026-10-29: told html had no network, @claude fetched six pictures on a
/// computer, base64-encoded them and retyped all 35 KB into a `mafold/html`
/// card — 34,345 output tokens, eleven of the turn's twelve and a half
/// minutes, the card spinning until its closing tag arrived.
pub const PICTURES_BY_URL: &str = "Put a picture in by its URL. Never write its bytes into a reply \
(base64, a data: URI): every character is an output token, so a few small pictures take minutes \
to write while the card waits for its closing tag.";

/// For a runtime whose prompt tells the model to reach for `mafold/html`
/// constantly: the one exception, said once and only when it applies.
pub fn noted_cards_beat_html(cards: &[OfferedCard]) -> Option<&'static str> {
    cards.iter().any(|c| c.doc.is_some()).then_some(
        "The exception: a card listed with notes above that is made for exactly what you are \
         showing — write that card, as its notes say, rather than building the same thing in HTML.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn media() -> Value {
        json!({
            "flag": "inlineMedia",
            "doc": "Rows of pictures from web pages.",
            "example": "{% mafold/media %}\nrow|T|D\nsrc|https://a.b\n{% /mafold/media %}"
        })
    }

    #[test]
    fn a_flagged_card_is_offered_with_its_notes_only_while_the_flag_is_on() {
        let on = offer("mafold/media", Some(&media()), |f| f == "inlineMedia").unwrap();
        assert_eq!(on.doc.as_deref(), Some("Rows of pictures from web pages."));
        assert!(on.example.as_deref().unwrap().starts_with("{% mafold/media %}\nrow|"));
        assert_eq!(offer("mafold/media", Some(&media()), |_| false), None);
    }

    #[test]
    fn host_mounted_and_transcript_cards_are_never_offered() {
        let host = json!({"offer": false, "doc": "The activity center's tab."});
        assert_eq!(offer("mafold/receipts", Some(&host), |_| true), None);
        for t in ["mafold/run", "mafold/tool", "mafold/result", "mafold/generating"] {
            assert_eq!(offer(t, None, |_| true), None, "{t}");
        }
    }

    #[test]
    fn a_card_without_an_agent_block_is_offered_by_name() {
        let c = offer("mafold/kline", None, |_| false).unwrap();
        assert_eq!((c.doc, c.example), (None, None));
        // A blank doc is no doc; a malformed block is no block.
        let blank = offer("x/y", Some(&json!({"doc": "  "})), |_| false).unwrap();
        assert_eq!(blank.doc, None);
        assert!(offer("x/y", Some(&json!("not an object")), |_| false).is_some());
    }

    #[test]
    fn noted_cards_lead_with_their_examples_unindented_and_the_rest_follow_by_name() {
        let cards = vec![
            offer("mafold/ask", None, |_| true).unwrap(),
            offer("mafold/media", Some(&media()), |_| true).unwrap(),
            offer("mafold/kline", None, |_| true).unwrap(),
        ];
        let s = render(&cards);
        let media_at = s.find("• mafold/media — Rows of pictures").expect(&s);
        let also_at = s.find("Also here (each takes its own attributes): mafold/ask, mafold/kline.").expect(&s);
        assert!(media_at < also_at, "{s}");
        assert!(s.contains("Example:\n{% mafold/media %}\nrow|T|D\n"), "{s}");
        assert!(s.contains("don't rebuild it in HTML"), "{s}");
        assert!(noted_cards_beat_html(&cards).is_some());
    }

    #[test]
    fn without_notes_the_catalog_is_just_names() {
        let cards = vec![offer("mafold/ask", None, |_| true).unwrap()];
        assert_eq!(render(&cards), "Cards here (each takes its own attributes): mafold/ask.");
        assert_eq!(noted_cards_beat_html(&cards), None);
        assert_eq!(render(&[]), "");
    }
}
