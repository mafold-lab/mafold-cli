//! Why a turn died — read ONE way, for every producer.
//!
//! A turn that fails has to say so, and what it says used to depend on who ran
//! it: the api's hosted brains glued a sentence onto a backticked provider dump
//! (`provider error: anthropic 402 Payment Required: {"error":{…}}`), the
//! self-hosted daemon wrote `⚠️ Agent stopped: <raw>`. Both now hand the raw
//! error to [`classify`] and the result to [`crate::render::failure_card`], so
//! the reader gets the same `{% mafold/error %}` card — a sentence in their own
//! language, what to do about it, and the raw error folded away under it —
//! whoever's turn it was.
//!
//! The error crosses process boundaries as TEXT (router → api brain → here;
//! Claude Code → daemon → here), so classification is on the text. That is
//! fragile by nature, which is why the rules are narrow phrases and statuses
//! rather than loose words («rate» also matches «generate») — and why an
//! unrecognised error is never dropped: it becomes [`FailureKind::Unknown`] and
//! still carries its raw text.

/// What a turn fails with when its final completion spent the whole output
/// cap — on thinking, for a model that thinks — and wrote no answer at all.
///
/// 2026-10-08 (#802 re-test): Sonnet 4.6 at `high` thought for 64,000 tokens
/// and 857 s on a 24-point puzzle and stopped at the cap with zero visible
/// characters. The turn then ended like any other, the reply was an empty
/// string, and the dispatcher discards an empty draft — so the person's
/// bubble simply vanished. A turn that ends this way is a failure; [`classify`]
/// keys on this marker to say so.
pub const OUTPUT_CAP_NO_ANSWER: &str = "output_cap_no_answer";

/// The wallet's refusal, worded once: the api writes it when a hold can't be
/// placed (`Store::router_hold_place`), the router passes it through inside its
/// 402, and [`classify`] reads the amounts back out of it. One function on both
/// ends so the two can't drift by a byte.
///
/// `need` is what the turn asked to hold (a ceiling: the request's input plus
/// its whole output cap), `have` what is free in `currency` itself, and
/// `convertible` what auto-convert could still bring in from other pockets.
/// Every amount is in units of `currency` — a model id; a wallet unit is one
/// output token of that model. `payer` is the account whose wallet it is: the
/// sentence goes back to whoever holds that account's key, and it is what lets
/// the card hand the amounts to that person alone.
pub fn wallet_short(payer: &str, need: i64, currency: &str, have: i64, convertible: i64) -> String {
    format!("insufficient balance: need {need} {currency}, have {have} plus {convertible} convertible, wallet @{payer}")
}

// ── refusals, worded once ───────────────────────────────────────────────────
//
// A turn can also be refused before any model runs — the agent's model is no
// longer offered, the payer's wallet holds nothing it can spend, their cap
// for this agent is used up, the machine that holds the credential is
// offline. Each used to be its own sentence in its own place (a langpack line,
// a hard-coded English one, a separate card), so one failure looked like three
// different things. Now each is written by one function here and read back by
// [`classify`] like any provider's error: one reader, one card. The words are
// for the details fold and the logs; the reader's sentence is the card's.

/// The agent's model isn't on offer from whoever runs it any more (`source`:
/// the runner that said so — `mafold-router 0.1.14 · …`).
pub fn model_not_offered(model: &str, source: &str) -> String {
    format!("model_not_offered: {model} isn't offered by {source}")
}

/// The payer's wallet can't start a turn in `currency` (a model id): `have` is
/// what it holds there, auto-convert included, and it isn't enough to begin.
pub fn wallet_empty(payer: &str, currency: &str, have: i64) -> String {
    format!("insufficient balance: wallet @{payer} holds {have} {currency}, and auto-convert can't fund it")
}

/// The payer holds nothing this agent can run on. `runs`: what it can run, as
/// the details' clue to what to top up with.
pub fn wallet_no_tokens(payer: &str, runs: &str) -> String {
    format!("insufficient balance: wallet @{payer} holds nothing this agent runs on — it runs {runs}")
}

/// The payer's monthly cap for this agent (`spender`) is used up.
pub fn cap_reached(payer: &str, spender: &str) -> String {
    format!("wallet_cap_reached: @{payer}'s monthly cap for @{spender} is used up")
}

/// The wallet's price table hasn't loaded, so no turn can be metered yet.
pub fn price_table_unavailable() -> String {
    "billing_unavailable: the official price table hasn't loaded yet".into()
}

/// Nobody could be identified to pay for this turn.
pub fn no_payer() -> String {
    "no_payer: couldn't tell who pays for this turn".into()
}

/// A usage window refused the turn: `detail` is the refusal as received, and
/// the window and reset time ride along in a marker [`classify`] reads back —
/// a bare `retry-after` header otherwise leaves no trace in the text.
pub fn quota_refused(detail: &str, window: &str, resets_at: Option<i64>) -> String {
    let at = resets_at.map(|t| format!(" resets_at={t}")).unwrap_or_default();
    format!("{} [quota window={window}{at}]", detail.trim())
}

/// None of the computers that hold `connection`'s credential are online, so
/// nothing can open it.
pub fn no_device(connection: &str) -> String {
    format!("no_device_online: none of the computers that hold `{connection}` are online")
}

/// The connection the agent is set to run on can't be used: `why` says how
/// (gone, the wrong kind, its authorization lapsed).
pub fn connection_unusable(connection: &str, why: &str) -> String {
    format!("connection_unusable: `{connection}` {why}")
}

/// Shared capacity for `model` couldn't take the turn (`why`: offline, empty,
/// every supplier failed). Never carries a supplier's own words — those are
/// someone else's account talking.
pub fn no_supplier(model: &str, why: &str) -> String {
    format!("no_supplier: no shared supplier of {model} could take this turn ({why})")
}

/// A paid bot whose paid use hasn't been switched on.
pub fn paid_use_not_enabled(bot: &str) -> String {
    format!("paid_use_not_enabled: @{bot} can't take paid turns yet")
}

/// What the model is told when its turn ended with nothing for the person to
/// read — word for word what Claude Code says in the same spot (2.1.294's
/// `thinking_only_retry`). Asked once; a second silence ends the turn as
/// [`empty_reply`]. One line for every loop that asks it: the api's hosted
/// harness (`brains::harness`) and the daemon's codex driver.
///
/// The loop rule it completes is everyone's: no tool call = the turn is over.
/// Alone, that rule let a model that thought, called a tool, and stopped
/// without a word be stamped ✓ with no answer in it (conv df712566,
/// 2026-10-10 02:54Z: 721 tokens of thinking + `name_channel`, then 2 tokens
/// and `end_turn`). Codex and Claude Code before 2.1.29x end the same way;
/// Claude Code now asks once more, and so do we. A protocol line, like the
/// output-cap notice — not a persona's words.
pub const NO_VISIBLE_OUTPUT: &str =
    "[Your previous response had no visible output. Please continue and produce a user-visible response.]";

/// The model answered with nothing — asked twice, nothing twice.
pub fn empty_reply(label: &str) -> String {
    format!("empty_reply: {label} answered with nothing")
}

/// A server restart cut a reply off before it finished.
pub fn turn_interrupted() -> String {
    "turn_interrupted: a server restart cut this reply off before it finished".into()
}

/// The model's maker refused the content itself — its safety policy blocked
/// the request or what came out of it (`why`: the vendor's own words, e.g.
/// Gemini's `IMAGE_SAFETY`). The marker is the OpenAI error code mafold-router
/// answers with, so a router refusal and one worded here read the same.
pub fn content_blocked(why: &str) -> String {
    format!("content_policy_violation: {}", why.trim())
}

/// What kind of failure it was — decides the card's sentence and what it offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The payer's Mafold wallet can't cover the turn. Nothing ran, nothing was charged.
    Balance,
    /// The account BEHIND the model (an API key, a Claude login) is out of
    /// credit — not the Mafold wallet, so the wallet is not where to go.
    Credit,
    /// The provider is throttling. Waiting fixes it.
    RateLimit,
    /// The credential was refused: a revoked key, an expired sign-in.
    Auth,
    /// The model chosen can't run (unknown, unpriced, retired).
    Model,
    /// The model runs, just not as a chat agent: its vendor refused a turn
    /// that carries tools (one that only takes its own vendor tool — Gemini's
    /// Computer Use). Not "right now" — no wait fixes it; another model does.
    NotChat,
    /// The conversation no longer fits the model's context window.
    Context,
    /// The model thought until its output cap and never wrote an answer.
    OutputCap,
    /// The gateway or provider is down or overloaded — ours or theirs.
    Unavailable,
    /// The connection to the model broke.
    Network,
    /// The turn was cut off (stalled, its process died) — context is kept.
    Interrupted,
    /// The agent's monthly spending cap (a wallet grant's) is used up — the
    /// wallet may be full; the payer raises or clears the cap.
    Cap,
    /// The computer that has to run this turn isn't there: none of the
    /// machines holding the credential are online, or the one running it
    /// dropped off.
    Offline,
    /// The connection the agent is set to run on is gone, the wrong kind, or
    /// its authorization lapsed — fixed in the agent's settings.
    Connection,
    /// The model answered, and said nothing at all.
    Empty,
    /// A paid bot whose paid use isn't switched on: no wait fixes it — its
    /// owner has to.
    NotOpen,
    /// The model's maker refused the content on policy (a safety filter on
    /// the prompt or on what it produced). Not an outage and not the wallet:
    /// rewording is what helps.
    Blocked,
    /// Nothing above. Still shown, raw text and all.
    Unknown,
}

impl FailureKind {
    /// The card's `kind` attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Balance => "balance",
            Self::Credit => "credit",
            Self::RateLimit => "rate_limit",
            Self::Auth => "auth",
            Self::Model => "model",
            Self::NotChat => "not_chat",
            Self::Context => "context",
            Self::OutputCap => "output_cap",
            Self::Unavailable => "unavailable",
            Self::Network => "network",
            Self::Interrupted => "interrupted",
            Self::Cap => "cap",
            Self::Offline => "offline",
            Self::Connection => "connection",
            Self::Empty => "empty",
            Self::NotOpen => "not_open",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }
}

/// One failed turn, as the card shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub kind: FailureKind,
    /// The HTTP status the provider answered, when the text carries one.
    pub status: Option<u16>,
    /// [`FailureKind::Balance`]: what the turn needed, in `currency` units.
    pub need: Option<i64>,
    /// [`FailureKind::Balance`]: what the wallet could put toward it, in
    /// `currency` units — free balance plus what auto-convert could bring in.
    pub have: Option<i64>,
    /// [`FailureKind::Balance`]: the wallet currency — a model id.
    pub currency: Option<String>,
    /// USD per 1M units of `currency` (its output price), when the caller
    /// knows the catalog — lets the card say «≈$0.42» instead of a token count.
    pub usd_out: Option<f64>,
    /// [`FailureKind::RateLimit`]: when the limit lifts (unix seconds), if
    /// whoever refused said so. Public: the card turns it into the reader's
    /// own clock time.
    pub resets_at: Option<i64>,
    /// [`FailureKind::RateLimit`]: which window ran out (`five_hour`,
    /// `seven_day`, a provider's own name).
    pub window: Option<String>,
    /// Who pays (lowercase handle, no `@`): the one reader the wallet button is for.
    pub payer: Option<String>,
    /// Who else may read the payer's part when no wallet is named — for a bot
    /// with no owner (a house bot), the person who asked. `None` ⇒ the bot's
    /// owner, whom `{% mafold/only for="owner" %}` already means.
    pub reader: Option<String>,
    /// The raw error, verbatim. Empty when it must not be shown (another
    /// person's account talking, e.g. a market supplier's).
    pub detail: String,
    /// `None` = the TURN failed (what this card has always meant). `Some(id)`
    /// = one step inside an otherwise answered turn failed — `imagegen`: the
    /// picture wasn't made, the reply around it still stands. Same card, same
    /// kinds, same payer-only details; only the headline and the text readers'
    /// one-liner say «the picture» instead of «this turn». An id, not a
    /// sentence: the card words it in the reader's language.
    pub step: Option<String>,
}

impl Failure {
    /// A failure of `kind` with nothing else known — for a producer that knows
    /// what happened without having to read it out of an error string.
    pub fn of(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            need: None,
            have: None,
            currency: None,
            usd_out: None,
            resets_at: None,
            window: None,
            payer: None,
            reader: None,
            detail: detail.into(),
            step: None,
        }
    }

    /// The same failure, scoped to one step of a turn (see [`Self::step`]).
    pub fn in_step(mut self, step: &str) -> Self {
        self.step = Some(step.to_string());
        self
    }
}

/// Read a raw error string into a [`Failure`]. Never fails: what it can't
/// place is [`FailureKind::Unknown`], with the text intact.
pub fn classify(err: &str) -> Failure {
    let detail = err.trim().to_string();
    let low = detail.to_lowercase();
    let status = status_of(&detail);
    let has = |words: &[&str]| words.iter().any(|w| low.contains(w));
    let mut out = Failure::of(FailureKind::Unknown, detail.clone());
    out.status = status;

    // The model's own dead end first: it is the one case whose fix is in the
    // reader's hands and none of the generic advice fits.
    if low.contains(OUTPUT_CAP_NO_ANSWER) {
        out.kind = FailureKind::OutputCap;
        return out;
    }
    // The refusals written above, by their own words — ahead of every status
    // and loose phrase below, which a refusal's details may happen to contain.
    let refused = if low.contains("model_not_chat_capable") {
        // mafold-router's type: the vendor refused a tool-carrying chat turn
        // for this model outright. Ahead of the 400 it arrives in.
        Some(FailureKind::NotChat)
    } else if low.contains("content_policy_violation") {
        // The router's code for a safety refusal ([`content_blocked`]) — also
        // inside a 400 that would otherwise read as «the model can't run».
        Some(FailureKind::Blocked)
    } else if has(&["model_not_offered", "model_disabled"]) {
        Some(FailureKind::Model)
    } else if low.contains("wallet_cap_reached") {
        out.payer = handle_after(&detail, "wallet_cap_reached: @");
        Some(FailureKind::Cap)
    } else if has(&["no_device_online", "no device picked the call up", "no device finished the call", "the device stopped relaying"]) {
        Some(FailureKind::Offline)
    } else if low.contains("connection_unusable") {
        Some(FailureKind::Connection)
    } else if has(&["empty_reply", "produced no output"]) {
        Some(FailureKind::Empty)
    } else if low.contains("no_supplier") {
        Some(FailureKind::Unavailable)
    } else if low.contains("paid_use_not_enabled") {
        Some(FailureKind::NotOpen)
    } else if has(&["turn_interrupted", "hit its 15-minute ceiling"]) {
        Some(FailureKind::Interrupted)
    } else {
        None
    };
    if let Some(kind) = refused {
        out.kind = kind;
        return out;
    }
    // The wallet refusing to START a turn ([`wallet_empty`], [`wallet_no_tokens`]).
    if let Some(payer) = handle_after(&detail, "insufficient balance: wallet @") {
        out.kind = FailureKind::Balance;
        out.payer = Some(payer);
        if let Some((have, currency)) = wallet_holds(&detail) {
            out.have = Some(have);
            out.currency = Some(currency);
        }
        return out;
    }
    // The Mafold wallet, by its own sentence ([`wallet_short`]) — it arrives
    // wrapped in a 402 whose bare status would read as any vendor's.
    if let Some((need, currency, have, payer)) = wallet_amounts(&detail) {
        out.kind = FailureKind::Balance;
        out.need = Some(need);
        out.currency = Some(currency);
        out.have = Some(have);
        out.payer = payer;
        return out;
    }
    out.kind = if has(&["looks stalled", "exited mid-turn", "no output from the agent"]) {
        FailureKind::Interrupted
    } else if has(&[
        "prompt is too long",
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "input is too long",
    ]) {
        FailureKind::Context
    // `上游` is mafold-router speaking about ITS seat: a vendor that refused
    // the gateway's own key or credit is our outage, not the reader's account.
    } else if matches!(upstream_status(&detail), Some(401..=403))
        || (low.contains("上游") && has(&["insufficient balance", "credit", "payment required"]))
    {
        FailureKind::Unavailable
    } else if status == Some(402)
        || has(&[
            "credit balance is too low",
            "insufficient_quota",
            "exceeded your current quota",
            "insufficient balance",
            "payment required",
            "billing_hard_limit",
        ])
    {
        FailureKind::Credit
    } else if status == Some(429)
        || has(&["rate limit", "rate_limit", "ratelimit", "too many requests", "usage limit", "usage_limit", "[quota window=", "限流"])
        || (low.contains("hit your") && low.contains("limit"))
    {
        FailureKind::RateLimit
    } else if matches!(status, Some(401 | 403))
        || has(&[
            "invalid_api_key",
            "invalid api key",
            "invalid x-api-key",
            "authentication_error",
            "unauthenticated",
            "please run /login",
            "not logged in",
            "token has expired",
            "permission_error",
            "wallet_grant_required",
            "insufficient_scope",
        ])
    {
        FailureKind::Auth
    } else if has(&["model_not_walletable", "不认识模型", "model_not_found", "unknown model", "invalid model"])
        || (low.contains("model")
            && (status == Some(404) || has(&["not_found_error", "does not exist", "may not exist", "not found", "not supported"])))
    {
        FailureKind::Model
    } else if matches!(status, Some(500..=599))
        || has(&[
            "billing_unavailable",
            "overloaded",
            "没有可用的供给席位",
            "service unavailable",
            "bad gateway",
            "internal server error",
        ])
    {
        FailureKind::Unavailable
    } else if has(&[
        "error sending request",
        "connection error",
        "connection reset",
        "connection refused",
        "connection closed",
        "timed out",
        "timeout",
        "dns error",
        "broken pipe",
        "econnreset",
        "unexpected eof",
        "stream ended",
    ]) {
        FailureKind::Network
    } else {
        FailureKind::Unknown
    };
    if out.kind == FailureKind::RateLimit {
        let (window, resets_at) = limit_lifts(&detail);
        out.window = window;
        out.resets_at = resets_at;
    }
    out
}

/// The handle right after `prefix` (`@ops` → `ops`), lowercase.
fn handle_after(s: &str, prefix: &str) -> Option<String> {
    let at = s.find(prefix)? + prefix.len();
    let h: String = s[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        .collect();
    (!h.is_empty()).then(|| h.to_lowercase())
}

/// `holds N <currency>` out of a [`wallet_empty`] sentence.
fn wallet_holds(s: &str) -> Option<(i64, String)> {
    let at = s.find(" holds ")? + " holds ".len();
    let mut words = s[at..].split_whitespace();
    let have: i64 = words.next()?.parse().ok()?;
    let currency = words.next()?.trim_end_matches(',').to_string();
    (!currency.is_empty()).then_some((have.max(0), currency))
}

/// Which window refused and when it lifts, in the three shapes it arrives:
/// [`quota_refused`]'s marker, mafold-router's 429 body (`"kind"`,
/// `"resets_at"`), Claude Code's `usage limit reached|<epoch>`.
fn limit_lifts(s: &str) -> (Option<String>, Option<i64>) {
    if let Some(at) = s.rfind("[quota window=") {
        let rest = &s[at + "[quota window=".len()..];
        let rest = rest.split(']').next().unwrap_or(rest);
        let mut parts = rest.split_whitespace();
        let window = parts.next().map(str::to_string).filter(|w| !w.is_empty() && w != "quota");
        let resets_at = parts.find_map(|p| p.strip_prefix("resets_at=")?.parse().ok());
        return (window, resets_at);
    }
    let field = |name: &str| -> Option<&str> {
        let at = s.find(&format!("\"{name}\""))? + name.len() + 2;
        let rest = s[at..].trim_start().strip_prefix(':')?.trim_start();
        Some(rest)
    };
    let window = field("kind").and_then(|r| r.strip_prefix('"')?.split('"').next()).map(str::to_string).filter(|w| !w.is_empty());
    let resets_at = field("resets_at")
        .and_then(|r| r.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
        .or_else(|| {
            // Found in the text itself: an offset taken from a lowercased copy
            // is not an offset into this one (`İ`, `K` change length).
            let at = ["usage limit reached|", "Usage limit reached|"].iter().find_map(|p| Some(s.find(p)? + p.len()))?;
            s[at..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
        });
    (window, resets_at)
}

/// The amounts out of a [`wallet_short`] sentence: `(need, currency, have,
/// payer)`, `have` counting what auto-convert could add. Also reads the older
/// wording (`have N plus convertible` — no figure, no payer), as an api one
/// release behind still writes it.
fn wallet_amounts(s: &str) -> Option<(i64, String, i64, Option<String>)> {
    let at = s.find("insufficient balance: need ")?;
    let rest = &s[at + "insufficient balance: need ".len()..];
    let mut words = rest.split_whitespace();
    let need: i64 = words.next()?.parse().ok()?;
    let currency = words.next()?.trim_end_matches(',').to_string();
    if words.next()? != "have" {
        return None;
    }
    let free: i64 = words.next()?.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok()?;
    let more = match (words.next(), words.next()) {
        (Some("plus"), Some(n)) => n.parse::<i64>().unwrap_or(0),
        _ => 0,
    };
    let payer = rest.find(", wallet @").map(|at| {
        rest[at + ", wallet @".len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
            .collect::<String>()
            .to_lowercase()
    });
    (!currency.is_empty()).then(|| (need, currency, free.max(0) + more.max(0), payer.filter(|p| !p.is_empty())))
}

/// The status mafold-router reports its OWN upstream answered with
/// (`上游 401 Unauthorized: …`) — nested inside the router's reply, which has
/// a status of its own (a refused seat comes back as a 502).
fn upstream_status(s: &str) -> Option<u16> {
    let at = s.find("上游 ")?;
    let n = s[at + "上游 ".len()..].get(..3)?;
    n.parse().ok()
}

/// The provider's HTTP status, when the text names one: the first standalone
/// 4xx/5xx before any JSON body (`anthropic 402 Payment Required: {…}`,
/// `API Error: 529 {…}`, `上游 429 Too Many Requests`). Standalone means
/// whitespace-delimited, so a port (`:443`) or a token count (`need 4020`)
/// is not one; an amount (`have 402`) is skipped by the word before it.
fn status_of(s: &str) -> Option<u16> {
    let head = s.split('{').next().unwrap_or(s);
    let mut prev = "";
    for word in head.split_whitespace() {
        let w = word.trim_start_matches('(').trim_end_matches([':', ',', ')']);
        if w.len() == 3 && !matches!(prev, "need" | "have" | "plus") {
            if let Ok(n) = w.parse::<u16>() {
                if (400..=599).contains(&n) {
                    return Some(n);
                }
            }
        }
        prev = word;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-09 本人截图那一条:托管 agent 的钱包不够,router 的 402 里裹着 api
    /// 自己那句话。要读成「钱包」,并且把需要多少、还剩多少原样读出来。
    #[test]
    fn the_wallet_402_is_the_wallet_with_its_amounts() {
        let raw = format!(
            "provider error: anthropic 402 Payment Required: {{\"error\":{{\"message\":\"resource exhausted: {}\",\"type\":\"insufficient_quota\"}}}}",
            wallet_short("ops", 123_456, "claude-sonnet-4-6", 700, 89)
        );
        let f = classify(&raw);
        assert_eq!(f.kind, FailureKind::Balance, "{f:?}");
        assert_eq!(f.status, Some(402));
        assert_eq!(f.need, Some(123_456));
        assert_eq!(f.have, Some(789), "free + convertible is what the wallet could give");
        assert_eq!(f.currency.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(f.payer.as_deref(), Some("ops"), "whose wallet it is, from whoever checked it");
        assert_eq!(f.detail, raw, "the raw error rides along verbatim");
    }

    /// 线上现在跑的 api 还写旧句子(`plus convertible` 不带数),照样要读出来。
    #[test]
    fn the_older_wallet_wording_still_reads() {
        let f = classify("anthropic 402 Payment Required: {\"error\":{\"message\":\"resource exhausted: insufficient balance: need 5000 deepseek-v4-pro, have -3 plus convertible\",\"type\":\"insufficient_quota\"}}");
        assert_eq!(f.kind, FailureKind::Balance);
        assert_eq!((f.need, f.have), (Some(5000), Some(0)), "a negative free balance shows as nothing left");
        assert_eq!(f.currency.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(f.payer, None, "the old sentence never named one");
    }

    /// 402 不全是钱包:router 自己的席位没钱(`上游 402`)是我们的故障;
    /// 自带 key 的账户没钱、Claude Code 的「Credit balance is too low」是
    /// 模型账号的钱 —— 这两种都不能叫人去 Mafold 钱包。
    #[test]
    fn a_402_that_is_not_the_wallet_does_not_send_you_to_the_wallet() {
        let seat = classify("provider error: deepseek 402 Payment Required: {\"error\":{\"message\":\"上游 402 Payment Required: {\\\"error\\\":{\\\"message\\\":\\\"Insufficient Balance\\\"}}\",\"type\":\"server_error\"}}");
        assert_eq!(seat.kind, FailureKind::Unavailable, "{seat:?}");
        let byo = classify("provider error: openai 429 Too Many Requests: {\"error\":{\"message\":\"You exceeded your current quota, please check your plan and billing details.\",\"type\":\"insufficient_quota\"}}");
        assert_eq!(byo.kind, FailureKind::Credit, "{byo:?}");
        let cc = classify("Credit balance is too low");
        assert_eq!(cc.kind, FailureKind::Credit);
        assert_eq!(classify("anthropic (alice) 402 Payment Required: {}").kind, FailureKind::Credit);
    }

    #[test]
    fn each_kind_is_read_from_what_providers_actually_say() {
        let cases: &[(&str, FailureKind)] = &[
            ("上游 429 Too Many Requests", FailureKind::RateLimit),
            ("API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}", FailureKind::RateLimit),
            ("You've hit your session limit · resets 3am", FailureKind::RateLimit),
            ("API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"Invalid bearer token\"}} · Please run /login", FailureKind::Auth),
            ("provider error: mafold 401 Unauthorized: {\"error\":{\"message\":\"Mafold PAT 无效或已吊销\",\"type\":\"invalid_api_key\"}}", FailureKind::Auth),
            ("provider error: anthropic 400 Bad Request: {\"error\":{\"message\":\"model_not_walletable: gpt-9 has no official price\",\"type\":\"model_not_walletable\"}}", FailureKind::Model),
            ("provider error: anthropic 400 Bad Request: {\"error\":{\"message\":\"上游 400 Bad Request: prompt is too long: 212000 tokens > 200000 maximum\"}}", FailureKind::Context),
            ("API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}", FailureKind::Unavailable),
            ("provider error: anthropic 503 Service Unavailable: {\"error\":{\"message\":\"billing unreachable\",\"type\":\"billing_unavailable\"}}", FailureKind::Unavailable),
            ("provider error: anthropic 502 Bad Gateway: {\"error\":{\"message\":\"上游 401 Unauthorized: invalid x-api-key\",\"type\":\"server_error\"}}", FailureKind::Unavailable),
            ("provider error: anthropic: router.mafold.com:443: error sending request for url: operation timed out", FailureKind::Network),
            ("no output from the agent for 10 minutes — the run looks stalled and was stopped. Your context is kept; just resend to retry.", FailureKind::Interrupted),
            ("anthropic: output_cap_no_answer — the model spent its whole output cap (64000 tokens) without writing an answer", FailureKind::OutputCap),
            ("API Error: 404 {\"type\":\"error\",\"error\":{\"type\":\"not_found_error\",\"message\":\"model: claude-opus-9\"}}", FailureKind::Model),
            ("There's an issue with the selected model (claude-opus-9). It may not exist or you may not have access to it.", FailureKind::Model),
            ("API Error: Connection error.", FailureKind::Network),
            ("HTTP status client error (401 Unauthorized) for url (https://router.mafold.com/v1/messages)", FailureKind::Auth),
            ("something nobody mapped", FailureKind::Unknown),
        ];
        for (raw, want) in cases {
            let f = classify(raw);
            assert_eq!(f.kind, *want, "{raw}");
            assert_eq!(f.detail, raw.trim(), "never drop the raw text: {raw}");
        }
    }

    /// 2026-10-10 本人:「一轮没成就是一种东西」—— 发起前就拦下的、@chatgpt 的、
    /// 限流的,都由这里一处写、classify 一处读,同一张卡。
    #[test]
    fn every_refusal_is_read_back_as_its_kind() {
        let cases: Vec<(String, FailureKind)> = vec![
            (model_not_offered("claude-sonnet-4-6", "mafold-router 0.1.14 · 127.0.0.1:4200"), FailureKind::Model),
            (cap_reached("ops", "ops:helper"), FailureKind::Cap),
            (price_table_unavailable(), FailureKind::Unavailable),
            (no_device("我的 Codex"), FailureKind::Offline),
            (connection_unusable("我的 Codex", "no longer exists"), FailureKind::Connection),
            (no_supplier("gpt-5.5", "every supplier is offline"), FailureKind::Unavailable),
            (paid_use_not_enabled("ops:cc"), FailureKind::NotOpen),
            (empty_reply("anthropic"), FailureKind::Empty),
            (turn_interrupted(), FailureKind::Interrupted),
            (no_payer(), FailureKind::Unknown),
            ("no device picked the call up".into(), FailureKind::Offline),
            ("the device stopped relaying mid-turn".into(), FailureKind::Offline),
            ("the turn hit its 15-minute ceiling".into(), FailureKind::Interrupted),
            ("(the agent produced no output)".into(), FailureKind::Empty),
            // mafold-router's own types, as the harness relays them.
            (
                "google 400 Bad Request: {\"error\":{\"message\":\"gemini-2.5-computer-use-preview-10-2025 不能当聊天 agent 用,换一个型号。上游原话:Gemini 上游 400 Bad Request: This model requires the use of the Computer Use tool.\",\"type\":\"model_not_chat_capable\"}}".into(),
                FailureKind::NotChat,
            ),
            (
                "anthropic 400 Bad Request: {\"error\":{\"message\":\"claude-haiku-4-5 已下架,换一个型号\",\"type\":\"model_disabled\"}}".into(),
                FailureKind::Model,
            ),
            // A safety refusal, worded here and as the router answers it — the
            // 400 around it must not read as «this model can't run».
            (content_blocked("图被拦下:IMAGE_SAFETY"), FailureKind::Blocked),
            (
                "router 400: {\"error\":{\"message\":\"提示词被拦下:PROHIBITED_CONTENT\",\"type\":\"invalid_request_error\",\"code\":\"content_policy_violation\"}}".into(),
                FailureKind::Blocked,
            ),
        ];
        for (raw, want) in cases {
            let f = classify(&raw);
            assert_eq!(f.kind, want, "{raw}");
            assert_eq!(f.detail, raw, "the words stay for the details: {raw}");
        }
        // Their own words win over anything their details happen to contain.
        assert_eq!(classify(&model_not_offered("x-429-model", "router 503")).kind, FailureKind::Model);
    }

    /// 发起前钱包就不够:谁的钱包、哪个币、有多少 —— 跟 402 那句一样读出来。
    #[test]
    fn a_wallet_that_cannot_start_a_turn_is_the_wallet() {
        let f = classify(&wallet_empty("Alice", "claude-sonnet-4-6", 120));
        assert_eq!(f.kind, FailureKind::Balance);
        assert_eq!(f.payer.as_deref(), Some("alice"));
        assert_eq!((f.have, f.currency.as_deref()), (Some(120), Some("claude-sonnet-4-6")));
        assert_eq!(f.need, None, "nothing was asked of it yet");
        let none = classify(&wallet_no_tokens("alice", "claude-sonnet-4-6, gpt-5.5 (+12)"));
        assert_eq!((none.kind, none.payer.as_deref(), none.have), (FailureKind::Balance, Some("alice"), None));
        let cap = classify(&cap_reached("Alice", "ops:helper"));
        assert_eq!(cap.payer.as_deref(), Some("alice"), "the cap is the payer's to raise");
    }

    /// 限流:哪个窗口、什么时候恢复,三种来路都读出来 —— 卡片按读者时区说时间。
    #[test]
    fn a_limit_says_which_window_and_when_it_lifts() {
        let router = classify("provider error: anthropic 429 Too Many Requests: {\"error\":{\"message\":\"上游 429\",\"type\":\"rate_limit_exceeded\",\"kind\":\"five_hour\",\"resets_at\":1790537400}}");
        assert_eq!((router.kind, router.window.as_deref(), router.resets_at), (FailureKind::RateLimit, Some("five_hour"), Some(1790537400)));
        let marked = classify(&quota_refused("anthropic 429 Too Many Requests: ", "seven_day", Some(1790600000)));
        assert_eq!((marked.window.as_deref(), marked.resets_at), (Some("seven_day"), Some(1790600000)));
        let bare = classify(&quota_refused("codex usage_limit_reached", "quota", None));
        assert_eq!((bare.kind, bare.window, bare.resets_at), (FailureKind::RateLimit, None, None), "never a made-up time");
        let claude = classify("Claude usage limit reached|1790537400");
        assert_eq!((claude.kind, claude.resets_at), (FailureKind::RateLimit, Some(1790537400)));
        // Characters that change length when lowercased come before it: the
        // offset is the text's own, never a lowercased copy's (review, 10-10).
        let odd = classify("K İ 中 Claude usage limit reached|1790537400");
        assert_eq!(odd.resets_at, Some(1790537400));
    }

    /// 「rate」也在 generate 里,「402」也可能是一个金额 —— 松散的词不算数。
    #[test]
    fn loose_words_and_stray_numbers_do_not_classify() {
        assert_eq!(classify("failed to generate a separate accurate answer").kind, FailureKind::Unknown);
        assert_eq!(classify("api.example.com:443: something odd").status, None);
        assert_eq!(classify("insufficient balance: need 4020 m, have 402 plus 0 convertible").status, None);
        assert_eq!(classify("API Error: 529 {\"status\":402}").status, Some(529), "the status before the body, not one inside it");
    }
}
