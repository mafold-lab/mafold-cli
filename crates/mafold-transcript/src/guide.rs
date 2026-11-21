//! The Mafold guide: how an AI writes for Mafold — card syntax, the two
//! cards worth using constantly, an html card's runtime (theme, shared state,
//! propose, full screen), and why cards are the native medium.
//!
//! ONE copy for every reader that gets the long form: the daemon's preamble on
//! a computer (`mafold-cli` `mafold_preamble`) and an AI that drives its agent
//! through a token (the api's connect page and `getGuide`, ops 11-21 「知识库
//! 可以共享」). They were one text in the CLI that a token-driven AI never saw,
//! which is why Muse wrote plain text and said it didn't know `mafold/html`.
//! Mafold Cloud keeps its compact form under the Mafold Free prompt budget
//! (`mafold-api` `brains/platform.rs`), from the same catalog constants.
//!
//! What differs by reader is only the tool a sentence names: a computer agent
//! runs `mafold …`, a token-driven AI calls the api.

use crate::catalog::{self, OfferedCard};

/// Who reads the guide — only the tools a sentence names differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reader {
    /// An agent on a computer, run by the daemon: it has the `mafold` CLI.
    Computer,
    /// An AI that drives its agent over the HTTP api with its own token.
    Token,
}

/// How to write cards, with `cards` (the agent's own menu,
/// [`catalog::offer`]) listed in the middle.
pub fn cards(cards: &[OfferedCard], reader: Reader) -> String {
    let mut s = String::from(
        "You can embed CARDS in your reply: write a Markdoc tag inline and Mafold renders it as a \
native card. Write the tag directly — do NOT wrap it in a code fence or escape it. \
A card reference is ALWAYS `owner/slug` — a bare name renders as \"Unsupported card\":\n  \
{% owner/cardname attribute=\"value\" /%}\n",
    );
    if cards.is_empty() {
        s.push_str("(No custom cards are published for you yet — plain text/Markdown is fine.)");
    } else {
        s.push_str(&catalog::render(cards));
        s.push_str("\nUse a card when it communicates better than prose; otherwise reply normally.");
    }
    // The two interactive cards worth reaching for constantly — write the tag
    // yourself, inline, rather than relying on a tool. They beat plain-text
    // fallbacks, so spell out the exact syntax and encourage liberal use.
    s.push_str(
        "\n\nTWO CARDS WORTH USING OFTEN — write the tag yourself, inline:\n\
\n• {% mafold/ask %} — a tap-to-answer question card; the most reliable way to offer the user choices \
here. Line-encoded body: one `q|<header>|<multi 0/1>|<question>` line per question, each followed by \
its `o|<label>|<description>` option lines. End your turn with it — the user's tap comes back as \
their next message and you continue. Example:\n\
{% mafold/ask %}\nq|Deploy|0|Ship to prod now?\no|Yes|blue-green, ~2 min\no|Hold|I'll review first\n{% /mafold/ask %}\n\
\n• {% mafold/html %} — a sandboxed mini-UI (charts, demos, small games, rich layouts, dashboards). Put \
your HTML between the tags and CLOSE with {% /mafold/html %} (NOT a second {% mafold/html %} — a common mistake \
that breaks the card). ",
    );
    s.push_str(catalog::HTML_NETWORK);
    s.push(' ');
    s.push_str(catalog::PICTURES_BY_URL);
    s.push_str(
        " Reach for it whenever something visual communicates better than text.\n\
  COLOURS: the frame hands you the READER'S OWN theme as CSS variables. Use them and your card is \
correct in light and dark without a single media query:\n\
    --mf-text  --mf-muted  --mf-subtle      (foreground, in three weights)\n\
    --mf-bg  --mf-bubble  --mf-float  --mf-card   (surfaces, back to front)\n\
    --mf-border  --mf-accent  --mf-on-accent  --mf-error  --mf-success\n\
  `html[data-theme]` is `light` or `dark` if you need to branch, and the frame's background is \
TRANSPARENT — the bubble shows through, so don't paint your own page background unless you mean to. \
DON'T hardcode a palette and DON'T write `@media (prefers-color-scheme: …)`: that follows the \
reader's OPERATING SYSTEM, while the app has its own appearance setting, and the two disagree often \
enough that it is the single most common way one of these cards comes out unreadable.\n\
  SHARED STATE — a poll, a shared checklist, a two-player game: the page gets `window.mafold`, data \
that lives with THIS message, read and written by everyone in the chat as themselves (the server \
checks every write). `await mafold.ready` → `{available}`; `mafold.viewer` → `{username,name}` or \
null; `mafold.writable` (signed in, message finished, online). Each person writes their OWN \
fields — `mafold.state.setMine(\"vote\", 2)`; `mafold.state.mine(\"vote\")`; \
`mafold.state.all(\"vote\")` lists everyone's as `{v,by,at,rev,user}` in write order (`rev` orders \
writes: first to take a seat wins). Nobody can write as someone else. `setMine(f, v, \
{readers:\"sender\"})` lets only you, the sender, see it — an anonymous vote you tally. A field \
everyone edits together is declared on the tag, `{% mafold/html shared=\"items\" %}`, and changed \
with `mafold.state.update(\"items\", list => [...(list||[]), x])` — concurrent edits all land. \
Repaint on `mafold.state.on(fn)` and `mafold.on(\"writable\", fn)`. You read and write the same \
state ",
    );
    s.push_str(match reader {
        Reader::Computer => "with `mafold state get|set <message id> …` (`mafold msg` lists it).",
        Reader::Token => "with the `getCardState` / `setCardState` calls (`message_id`, `key`, `value`).",
    });
    s.push_str(
        " Data that only lives as \
long as the card — this poll, this game — belongs here, not in an app.\n\
  SEND — when a tap on your card should reach the chat as the reader's own message: a page can't \
send, it offers words. Call `mafold.propose(\"I want to make something\")` from a tap on your page: \
the app asks the reader in a modal of its own, showing those words, and Send there posts them as \
their own reply to this card, and you hear it like any reply. It resolves `true` if they went out \
and `false` if the reader said no. Called without a tap, on load or on a timer, it is refused \
(`no_tap`). `mafold.sent` is what this reader last sent from this card, so repaint on \
`mafold.on(\"sent\", fn)` to show it answered. Words are at most 2000 characters and must be text \
the reader can see. Plain options need no page: `{% mafold/ask %}` does this already. Use propose \
when the choice has to look a certain way.\n\
  FULL SCREEN — don't count on the app for a full-screen button: a page that wants the whole \
screen (a game, a map, a long table) draws its own button and calls `mafold.fullscreen.enter()` \
from a tap on it. It resolves `true` once the page fills the screen and `false` if it didn't; like \
propose it needs a tap, so on load or on a timer it resolves `false`. `mafold.fullscreen.available` \
is false in apps too old to have it, so hide your button then, and keep it out of the top-right \
corner, where the app may still draw its own. The way out is always the app's (a strip above the \
page with its close button, and Escape), so your page doesn't have to be the exit; \
`mafold.fullscreen.exit()` and `.active` are there if you want them, and \
`mafold.on(\"fullscreen\", fn)` fires on the way in and out. On a phone, full screen opens a second \
copy of your page that starts over with `active` already true: check it on load and draw the \
full-screen view, rather than counting on what the inline copy had in its variables.\n\
\nLean on these — favor them over plain-text \"reply 1/2/3\" prompts or describing what a chart \
would look like.\n\
\nCARDS ARE THE NATIVE MEDIUM HERE, not a garnish. This is a chat client that renders live \
components inside the bubble, so anything with STRUCTURE — a comparison, a schedule, a set of \
numbers, a diff, a layout, a state machine, a board, a piece of music — reads better as a card \
than as prose or an ASCII table.\n\
\nAND WRITE YOUR OWN HTML — CONSTANTLY. {% mafold/html %} is NOT the fallback for when no \
published card fits; it is the card you should reach for most, because it can be anything you \
are willing to build. Real HTML/CSS/JS runs sandboxed in the bubble, scripts and all: an SVG \
chart, a sortable table, a side-by-side diff, a timeline, a stepper, a seating plan, a canvas \
animation, a small playable thing, a whole dashboard. Building the view is usually FASTER than \
writing the paragraph that describes it, and the reader gets something they can actually poke at. \
So: describing what a chart would look like, or drawing one in ASCII, when you could have drawn \
the chart — that is the habit to break. Default to showing; drop to prose only when the answer \
genuinely is a sentence.",
    );
    // The paragraph above is the loudest thing this prompt says about cards,
    // and a card made for the job (pictures from a page) lost to it: one
    // sentence, only when there is such a card on the menu.
    if let Some(rule) = catalog::noted_cards_beat_html(cards) {
        s.push(' ');
        s.push_str(rule);
    }
    s
}

/// What is wrong with the cards in a message `text`, one sentence each —
/// empty when nothing is. For an AI that can't see how its message rendered
/// (one driving its agent through a token): `sendMessage` still posts the
/// message and answers with these, so the next one comes out right. The
/// mistakes are the ones the guide warns about: a bare name renders as
/// "Unsupported card", a block card opened a second time instead of closed
/// eats the rest of the message, and a tag inside a code fence is shown as code.
pub fn card_problems(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut open: Vec<String> = Vec::new();
    let mut fenced = false;
    let mut fence_warned = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let mut rest = line;
        while let Some(i) = rest.find("{%") {
            rest = &rest[i + 2..];
            let Some(end) = rest.find("%}") else {
                out.push("A tag starting `{%` never ends with `%}`, so it shows as text.".to_string());
                break;
            };
            let inner = rest[..end].trim();
            rest = &rest[end + 2..];
            if fenced {
                if !fence_warned {
                    out.push("A card tag is inside a ``` code fence, so it shows as code: write the tag directly.".to_string());
                    fence_warned = true;
                }
                continue;
            }
            let closer = inner.starts_with('/');
            let selfclosing = inner.ends_with('/');
            let name = tag_name(inner.trim_start_matches('/'));
            if name.is_empty() {
                continue;
            }
            if !name.contains('/') {
                out.push(format!(
                    "`{{% {name} %}}` is a bare name and renders as \"Unsupported card\": write `owner/slug`, e.g. `mafold/{name}`."
                ));
                continue;
            }
            if closer {
                match open.iter().rposition(|o| *o == name) {
                    Some(at) => {
                        for left in open.drain(at..).skip(1) {
                            out.push(format!("`{{% {left} %}}` is never closed: end it with `{{% /{left} %}}`."));
                        }
                    }
                    None => out.push(format!("`{{% /{name} %}}` closes a card that was never opened.")),
                }
            } else if !selfclosing {
                if open.last() == Some(&name) {
                    out.push(format!(
                        "`{{% {name} %}}` is opened a second time where it should close: end it with `{{% /{name} %}}`."
                    ));
                    open.pop();
                    continue;
                }
                open.push(name);
            }
        }
    }
    for left in open {
        out.push(format!("`{{% {left} %}}` is never closed: end it with `{{% /{left} %}}`."));
    }
    out.dedup();
    out
}

/// A tag's name: what comes before its first space (`mafold/html shared="x"`),
/// without a self-closing tag's trailing `/`.
fn tag_name(inner: &str) -> String {
    inner.split_whitespace().next().unwrap_or_default().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_readers_get_the_same_card_teaching_and_their_own_state_tool() {
        let menu = vec![OfferedCard { id: "mafold/media".into(), doc: Some("Pictures.".into()), example: None }];
        let computer = cards(&menu, Reader::Computer);
        let token = cards(&menu, Reader::Token);
        for s in [&computer, &token] {
            assert!(s.starts_with("You can embed CARDS"), "{s}");
            assert!(s.contains("{% owner/cardname attribute=\"value\" /%}"), "{s}");
            assert!(s.contains("CLOSE with {% /mafold/html %}"));
            assert!(s.contains("--mf-accent") && s.contains("mafold.propose(") && s.contains("mafold.fullscreen.enter()"));
            assert!(s.contains("mafold/media — Pictures."));
            assert!(s.contains("The exception: a card listed with notes"));
            assert!(!s.contains("{{%"), "a format! escape leaked: {s}");
        }
        assert!(computer.contains("`mafold state get|set") && !token.contains("mafold state"));
        assert!(token.contains("`setCardState`") && !computer.contains("setCardState"));
    }

    #[test]
    fn a_broken_card_is_named_and_a_good_one_passes() {
        let good = "Here:\n{% mafold/ask %}\nq|A|0|B?\no|x|y\n{% /mafold/ask %}\n{% mafold/cite src=\"https://a.b\" /%}\n{% mafold/html shared=\"items\" %}<b>hi</b>{% /mafold/html %}";
        assert_eq!(card_problems(good), Vec::<String>::new());
        assert_eq!(card_problems("no cards at all, 50% off {x}"), Vec::<String>::new());
        let bare = card_problems("{% html %}<b>x</b>{% /html %}");
        assert!(bare[0].contains("bare name") && bare[0].contains("mafold/html"), "{bare:?}");
        let twice = card_problems("{% mafold/html %}<b>x</b>{% mafold/html %}");
        assert!(twice[0].contains("opened a second time"), "{twice:?}");
        let open = card_problems("{% mafold/html %}<b>x</b>");
        assert!(open[0].contains("never closed") && open[0].contains("{% /mafold/html %}"), "{open:?}");
        let fenced = card_problems("```\n{% mafold/ask %}\n```");
        assert!(fenced[0].contains("code fence"), "{fenced:?}");
        let stray = card_problems("{% /mafold/ask %}");
        assert!(stray[0].contains("never opened"), "{stray:?}");
    }
}
