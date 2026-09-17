//! The normalized event vocabulary an agent turn speaks.
//!
//! Anything that drives an agent — the self-hosted daemon's harnesses (Claude
//! Code, Codex, Kimi Code) or a brain running inside the api — parses its own
//! native output down to this one shape. [`crate::render`] turns these into
//! chat cards, so what a turn LOOKS like in the transcript is decided in
//! exactly one place, no matter who produced it.

use serde_json::Value;

/// One normalized event from an agent turn. Producer-specific output formats
/// (Claude Code stream-json, DeepSeek tool calls, …) are parsed down to this
/// common shape.
/// (`Session` is unused by Claude Code — it returns its session in the daemon's
/// `TurnOutcome` — but other producers may stream it; kept as common API
/// surface.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Sparse, cumulative statistics for THIS run. None means unreported;
    /// successive snapshots replace known fields, never add them twice.
    Stats(crate::RunStats),
    /// Explicit tool outcome, separate from display text. Unknown outcomes
    /// must not be guessed from a string containing the word "error".
    ToolStatus { id: String, failed: bool },
    /// The producer's resumable session id for this conversation (first seen).
    Session(String),
    /// A chunk of streamed assistant text.
    Text(String),
    /// The producer ABANDONED the assistant message it was streaming and is
    /// starting that message over — an API connection dropped mid-response and
    /// the SDK retries by re-streaming from the first token, not by resuming.
    /// Carries the text of the abandoned attempt so the transcript can un-say
    /// exactly that and nothing else.
    ///
    /// Without it the reply grows one duplicate copy of the opening line per
    /// retry. On 2026-08-15 three attempts spliced into
    /// `Now theNow the RN twin — first `TNow the RN twin — first `TextArea`…`,
    /// which left an ODD number of backticks and swallowed the 19.5k of tool
    /// cards behind it into the bubble as raw text.
    TextRewind(String),
    /// A tool / function call the agent made.
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of a tool call (correlated by `id`).
    ToolResult { id: String, text: String },
    /// One step a SUBAGENT took, attributed to the tool call that started it
    /// (`parent` is that call's id).
    ///
    /// A subagent's work comes back on the same stream as the main agent's —
    /// Claude Code marks it only with `parent_tool_use_id` — so without this it
    /// is flattened into the main timeline and reads as work the main agent
    /// did, and its final text can land in the reply as if the main agent had
    /// said it. Already summarized to one line by the producer, because the
    /// card it lands in is a fixed-width bubble, not a transcript.
    SubagentStep { parent: String, text: String },
    /// Something happened around the turn that the user has to be TOLD, but
    /// which is not model output: another local session tried to message this
    /// one and the policy parked it, the model was quietly downgraded, a tool
    /// was denied. Rendered in time order, like a steer, because when it
    /// happened is part of what it means — and registered as a notice line, so
    /// one arriving last is never mistaken for the answer.
    ///
    /// Producer-agnostic on purpose: any harness that learns one of these emits
    /// it. A notice must change what the user would do — anything merely
    /// explanatory belongs in the trace.
    Notice(String),
    /// A thinking / chain-of-thought block (collapsed in the UI).
    Thinking(String),
    /// An image the agent PRODUCED this turn, as a path on the producer's
    /// machine. The render loop uploads it and attaches it to the reply, so it
    /// arrives in the same bubble as the text — identical to a person sending a
    /// photo.
    ///
    /// Producer-agnostic on purpose: each one maps its own native image output
    /// onto this event (Codex's `image_gen` writes to
    /// `$CODEX_HOME/generated_images/…`), exactly as each maps its own
    /// file-edit shape onto `ToolCall`. Nothing downstream knows which model
    /// drew the picture.
    Image { path: std::path::PathBuf },
    /// Streaming activity that is NOT rendered as content (thinking / tool-arg
    /// deltas, usage updates): `chars` of raw stream progress plus, when the
    /// producer knows it, the REAL cumulative output-token count for the turn.
    /// Drives the `{% generating %}` card's live heartbeat (beat / elapsed /
    /// tokens) so the indicator reflects actual model progress — never the
    /// transcript.
    Pulse { chars: u64, tokens: Option<u64> },
    /// The producer compacted its OWN context part-way through the turn (Claude
    /// Code's auto-compact). It takes minutes and produces no stream output
    /// while it runs, so relaying it is what keeps a long reply from reading as
    /// a hang. `pre_tokens` is the context size that was compacted, when the
    /// producer reports it.
    Compacted { pre_tokens: Option<u64> },
    /// The producer reported a usage limit that is NOT in the healthy state.
    /// Producers emit this ONLY for the non-healthy states; a limit that's
    /// fine is not news and must not be relayed into every reply.
    ///
    /// `status` is the producer's own word for how bad it is, and the two
    /// cases are genuinely different events:
    /// - `rejected` — the request was REFUSED. The seat is out; the turn ends
    ///   unless something else can run it.
    /// - `allowed_warning` — utilization crossed a threshold (0.75) and the
    ///   request went through anyway. Claude Code repeats this every turn
    ///   until the window resets, i.e. for days, so it is a signal for the
    ///   seat logic and NOT something to render (see `render`).
    RateLimited { kind: String, resets_at: Option<i64>, status: String },
    /// End-of-turn summary.
    Done {
        duration_ms: Option<f64>,
        cost_usd: Option<f64>,
        tokens: Option<u64>,
    },
    /// (driver-internal) The user answered the pending interactive ask — the
    /// daemon's agent loop emits this when it routes a reply into `ask_file`,
    /// and the renderer stamps the answer into the open `{% ask %}` card so the
    /// message content itself records "answered" (survives reload, reaches
    /// every client). Harnesses never emit this.
    AskAnswered(String),
    /// (driver-internal) The user spoke again WHILE this turn was running and
    /// the driver steered the turn with it instead of starting a second one.
    /// Carries what they said.
    ///
    /// It is transcript content, not bookkeeping: the reply that comes back has
    /// to show WHERE the correction landed, or the turn reads as if the model
    /// changed its mind on its own. Rendered in time order — after the tool
    /// activity that had already happened, before whatever the correction
    /// caused. Harnesses never emit this.
    Steered(String),
}
