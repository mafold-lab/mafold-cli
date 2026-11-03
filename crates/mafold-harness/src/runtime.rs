//! Where completions come from: the [`ModelRuntime`] seam, the events a step
//! streams, and runtime #1 — any OpenAI-compatible `/chat/completions`
//! endpoint ([`OpenAiCompatRuntime`]): mafold-router for the wallet, OpenCode
//! Go, a vendor's own API.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use mafold_transcript::RunStats;
use serde_json::{json, Value};

/// Heartbeat cadence while the model streams tool-argument bytes (no prose).
pub(crate) const PULSE_EVERY: Duration = Duration::from_millis(250);

// ───────────────────────────── wire types ─────────────────────────────
// The OpenAI-compatible streaming shapes, verbatim from appbuilder.rs — tool
// calls stream as index-keyed fragments and are reassembled here.

#[derive(serde::Deserialize, Default, Clone)]
struct StreamFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}
#[derive(serde::Deserialize, Clone)]
struct StreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: StreamFn,
    /// Everything else the provider put on this call — Gemini's
    /// `extra_content` (its thought signature) is why this exists. Opaque
    /// here: kept on the call and handed back, never read (see
    /// [`ToolCall::extra`]).
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}
#[derive(serde::Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCall>>,
    /// Whole reasoning blocks (the router's carrier for Claude's thinking +
    /// signature). Kept and handed back verbatim; the one thing read out of
    /// them is the text a person may see ([`readable_thought`]).
    #[serde(default)]
    reasoning_details: Option<Vec<Value>>,
    /// Thinking as plain text, in pieces (`reasoning_content`: DeepSeek's
    /// dialect, which the router passes through untranslated). Kept and handed
    /// back — see [`TurnMsg::reasoning_content`] — and shown as it arrives
    /// ([`ChatEvent::Thinking`]).
    #[serde(default)]
    reasoning_content: Option<String>,
    /// A provider's opaque per-message extras (`extra_content`), the
    /// message-level twin of [`StreamToolCall::extra`]. Kept and handed back.
    #[serde(default)]
    extra_content: Option<Value>,
}
#[derive(serde::Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    /// `"length"` = the reply hit the output cap and was CUT — including in
    /// the middle of a tool call's arguments. See `TurnMsg::truncated`.
    #[serde(default)]
    finish_reason: Option<String>,
    /// The upstream's own word for why it stopped (`end_turn`, `refusal`…),
    /// which mafold-router puts beside `finish_reason` on the last frame. See
    /// `TurnMsg::stop_reason`.
    #[serde(default)]
    stop_reason: Option<String>,
}
#[derive(serde::Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    model: Option<String>,
    /// The gateway's word, mid-stream, that the reply broke: mafold-router
    /// sends `{"error":{"message","type":"server_error"}}` (then `[DONE]`) when
    /// its upstream dies or says `error` partway. Without this field the frame
    /// parsed as an empty chunk and the half reply passed for a whole one.
    #[serde(default)]
    error: Option<Value>,
}

/// One tool call, reassembled from its argument fragments.
#[derive(Clone, Default, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Unparsed JSON string, exactly as the model wrote it.
    pub arguments: String,
    /// The fields a provider put on this call beyond id / type / function,
    /// verbatim. Gemini 3 hangs its thought signature here
    /// (`extra_content.google.thought_signature`) and answers the next round
    /// of the same turn with a 400 when it is missing — so they ride along to
    /// [`assistant_value`] untouched. The kernel never reads them and names no
    /// vendor: which provider may see which namespace is the router's call
    /// (it strips foreign ones on the way up). Never stored across turns,
    /// like [`TurnMsg::reasoning`].
    ///
    /// [`assistant_value`]: crate::kernel::assistant_value
    pub extra: serde_json::Map<String, Value>,
}

/// A stream fragment's keys that build the call itself; everything else on it
/// is [`ToolCall::extra`].
const TOOL_CALL_OWN_KEYS: [&str; 4] = ["index", "id", "type", "function"];

/// The part of one `reasoning_details` block a person may read: a thinking
/// block's text (`reasoning.text` — Claude's summarized thinking, as
/// mafold-router hands it over) or a summary (`reasoning.summary`). An
/// encrypted block, or one that came back empty (Claude's `display:
/// "omitted"`), has nothing to show.
fn readable_thought(detail: &Value) -> Option<&str> {
    let text = match detail["type"].as_str()? {
        "reasoning.text" => detail["text"].as_str(),
        "reasoning.summary" => detail["summary"].as_str(),
        _ => None,
    }?;
    (!text.trim().is_empty()).then_some(text)
}

/// What one completion produced.
#[derive(Default)]
pub struct TurnMsg {
    /// Optional display measurements, kept separate from billing fallbacks.
    pub stats: RunStats,
    pub content: String,
    pub calls: Vec<ToolCall>,
    /// Output tokens retained for the existing billing contract.
    pub tokens: u64,
    /// Prompt tokens. Only a metered profile reads it; a runtime that cannot
    /// report it leaves 0 and billing falls back to its estimate.
    pub input_tokens: u64,
    /// The provider stopped this completion at its output cap
    /// (`finish_reason: "length"`). Whatever tool call was being written at
    /// that moment has half its arguments — see [`unrunnable_call`].
    pub truncated: bool,
    /// What the model thought on the way to this completion, as the provider
    /// handed it over (`reasoning_details`). Replayed verbatim on the next
    /// round of the SAME turn so a tool loop keeps its train of thought; it
    /// was dropped at the parser until 2026-10-07 and every step started
    /// thinking from scratch. Never stored across turns.
    pub reasoning: Vec<Value>,
    /// What the model thought, as text (`reasoning_content`), when the
    /// provider speaks that dialect: `Some("")` once it sent the key at all,
    /// so a step that thought nothing still hands back an empty one. DeepSeek
    /// in thinking mode refuses a request with tools (400: "The
    /// `reasoning_content` in the thinking mode must be passed back to the
    /// API.") when any assistant step in it that called tools lacks it — an
    /// earlier turn's too, so a host that keeps the conversation (mafold-cli)
    /// keeps it there. `None`: the provider never sent it, and nothing is added.
    pub reasoning_content: Option<String>,
    /// The provider's opaque extras on the message itself (`extra_content`),
    /// replayed the same way; the per-call ones live on [`ToolCall::extra`].
    pub extra_content: Option<Value>,
    /// The upstream's stop reason as it said it, when the runtime knows it
    /// (mafold-router passes Anthropic's through). `finish_reason` folds
    /// `end_turn` and `refusal` into one "stop"; whether a silent completion
    /// is worth asking again turns on which it was — see [`NO_VISIBLE_OUTPUT`].
    pub stop_reason: Option<String>,
}

/// What the model is told when a completion came back with nothing for the
/// person to read — no text, no call. Sent as the next user message in place
/// of the empty one, once per stretch between tool rounds; a second silence
/// ends the turn as the shared `empty_reply` failure. The line itself is
/// shared with the daemon's codex driver — see its definition.
pub use mafold_transcript::failure::NO_VISIBLE_OUTPUT;

/// Whether a silent completion that stopped for `stop_reason` is one the model
/// can be asked to finish: it ended its turn (`end_turn`, `stop_sequence`), or
/// the runtime doesn't say why (`None` — most providers). Not a `refusal`:
/// asking a safety classifier the same thing again gets the same answer.
pub(crate) fn silence_is_askable(stop_reason: Option<&str>) -> bool {
    matches!(stop_reason, None | Some("end_turn" | "stop_sequence" | "stop"))
}

/// Why a tool call must NOT be executed, as the tool message the model reads
/// instead — or None when it is fine to run.
///
/// 2026-09-26 (a person ↔ @mafold, seven turns in a row): every rewrite of a
/// 412-line app was cut at the 8192-token output cap in the middle of
/// `write_app`'s `source`. The half-written JSON failed to parse, was quietly
/// replaced with `{}`, and the tool answered "missing source" — so the model
/// concluded the PLATFORM was broken, retried the identical oversized call,
/// and promised the user it would "keep retrying". Running a call built from
/// a guess is how a truncation became a fake outage. Arguments that are not a
/// JSON object are never executed; the model is told what actually happened.
/// Empty arguments are a valid "no parameters" call.
pub fn unrunnable_call(call: &ToolCall, truncated: bool) -> Option<String> {
    let raw = call.arguments.trim();
    if raw.is_empty() {
        return None;
    }
    if serde_json::from_str::<Value>(raw).is_ok_and(|v| v.is_object()) {
        return None;
    }
    Some(if truncated {
        format!(
            "{{\"ok\":false,\"error\":\"NOT EXECUTED: your reply hit the output length limit while you were \
             still writing the arguments of `{name}` ({chars} characters in, JSON unfinished). Nothing ran \
             and nothing changed. This is not a platform fault and retrying the same call will be cut at \
             the same place — make it shorter (a smaller change, less code, split the work).\"}}",
            name = call.name,
            chars = raw.chars().count(),
        )
    } else {
        format!(
            "{{\"ok\":false,\"error\":\"NOT EXECUTED: the arguments of `{}` are not a valid JSON object, so \
             nothing ran. Call it again with well-formed JSON.\"}}",
            call.name
        )
    })
}

/// What a turn fails with when its final completion spent the whole output
/// cap — on thinking, for a model that thinks — and wrote no answer at all.
///
/// 2026-10-08 (#802 re-test): Sonnet 4.6 at `high` thought for 64,000 tokens
/// and 857 s on a 24-point puzzle and stopped at the cap with zero visible
/// characters. The turn then ended like any other, the reply was an empty
/// string, and the dispatcher discards an empty draft — so the person's
/// bubble simply vanished. A turn that ends this way is a failure; the shared
/// failure reader (`mafold_transcript::failure::classify`) keys on this marker
/// to say so — it lives there so the daemon's turns read it the same way.
pub const OUTPUT_CAP_NO_ANSWER: &str = mafold_transcript::failure::OUTPUT_CAP_NO_ANSWER;

/// The `reasoning_effort` value that means "pick it by the model this step
/// actually runs" — see [`default_reasoning_effort`]. A profile whose owner
/// chose nothing carries it; a real tier (`low` … `max`, or `none`) wins.
pub const EFFORT_BY_MODEL: &str = "auto";

/// How hard a model thinks when nobody chose — by its id, mechanically, the
/// same family/version reading the router uses to decide who takes adaptive
/// thinking at all (`mafold-router` `anthropic::apply_reasoning`). No list.
///
/// Claude Opus/Sonnet **4.6** default to `low`: at `high`, Sonnet 4.6 — the
/// hosted and house default model — went from 64 s to 324 s on a mortgage
/// card and spent a 24-point puzzle's entire 64K cap thinking (2026-10-08).
/// Every other model keeps `high`, which is what the 5.x line was measured at.
pub fn default_reasoning_effort(model: &str) -> &'static str {
    match claude_family(model) {
        Some(("opus" | "sonnet", (4, 6))) => "low",
        _ => "high",
    }
}

/// The effort a request on `model` asks for, given what the profile carries.
fn effort_for(configured: Option<&str>, model: &str) -> Option<String> {
    configured.map(|e| {
        if e.eq_ignore_ascii_case(EFFORT_BY_MODEL) {
            default_reasoning_effort(model).to_string()
        } else {
            e.to_string()
        }
    })
}

/// `claude-opus-5-5` → ("opus", (5, 5)); `claude-sonnet-4-6` → ("sonnet",
/// (4, 6)); a dated id keeps only its first two numbers. Same reading as the
/// router's, so the two never disagree about what a model is.
fn claude_family(model: &str) -> Option<(&str, (u32, u32))> {
    let rest = model.strip_prefix("claude-")?;
    let mut parts = rest.split('-');
    let family = parts.next()?;
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts
        .next()
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|n| *n < 100)
        .unwrap_or(0);
    Some((family, (major, minor)))
}

/// Keep the transport cause (DNS/TLS/connect/reset), like the CLI error
/// chain, without reflecting a credential-bearing endpoint URL into chat.
fn transport_error(error: reqwest::Error) -> String {
    let host = error.url().and_then(|u| u.host_str()).unwrap_or("provider").to_owned();
    format!("{host}: {:#}", anyhow::Error::new(error.without_url()))
}

/// One POST of a step — sent once more if the connection died before ANY reply
/// came back.
///
/// 2026-10-09 22:34: a hosted claude turn died on `client error (SendRequest):
/// connection closed before message completed` — the api↔router link dropped
/// mid-request (the catalog read had failed the same way two minutes earlier;
/// the router logged no upstream error). Nothing had reached the person yet,
/// so the whole turn was lost to a blip a second attempt walks past.
///
/// Only a failure with no response at all qualifies: once there is a status,
/// whatever it says is the provider's answer and goes the usual way. A replay
/// can't double a reply — none arrived — and the router drops the upstream
/// call when its client goes away. Once, not a loop: a link that is really
/// down still fails in a second, with the cause in the message.
async fn post_step(
    http: &reqwest::Client,
    url: &str,
    key: &str,
    body: &Value,
) -> reqwest::Result<reqwest::Response> {
    match http.post(url).bearer_auth(key).json(body).send().await {
        Err(e) if e.is_request() && !e.is_timeout() => {
            let host = e.url().and_then(|u| u.host_str()).unwrap_or("provider").to_owned();
            tracing::warn!("harness: {host} dropped the connection before replying ({}) — sending once more", causes(&e));
            tokio::time::sleep(Duration::from_millis(500)).await;
            http.post(url).bearer_auth(key).json(body).send().await
        }
        sent => sent,
    }
}

/// What a transport error is made of, without its own (URL-bearing) message.
fn causes(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = Vec::new();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push(c.to_string());
        cur = c.source();
    }
    if out.is_empty() { "no cause given".into() } else { out.join(": ") }
}

/// How long to wait before asking a step again after its stream BROKE — one
/// entry per retry, so a broken step is asked at most `len() + 1` times.
///
/// 2026-10-29 05:51Z: @claude was 13 model calls and 12 tool calls into a
/// turn when the api↔router link stalled under the 13th, whose reply the
/// router had already read whole from Anthropic. Six minutes later the socket
/// was reset (`Connection reset by peer`) and the step failed on the spot —
/// the person got a failure card for a turn that one more request finishes.
/// [`post_step`] only covers a link that drops before ANY status; a stream
/// that broke after its status was never asked again.
///
/// Safe to replay because a step has no side effects: tools run only after a
/// step is whole, and a broken step never becomes [`ChatEvent::Done`]. What
/// it had shown is taken back first ([`ChatEvent::Restart`]). Nor is the
/// person billed twice — the wallet is charged from a finished step's usage
/// only; a broken attempt costs the seat. Two things stay as they were: a
/// status is an answer, and a link that drops before ANY status gets
/// [`post_step`]'s one quick resend and then fails fast.
const STREAM_RETRY_AFTER: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];

/// One attempt at a step: the step's own events, or the word that its stream
/// broke before the reply was whole — which [`OpenAiCompatRuntime::step`]
/// answers by asking again, and only fails once it has run out of retries.
enum Attempt {
    Event(ChatEvent),
    Broke(String),
}

/// Does this 400 body refuse the MODEL rather than the request? mafold-router
/// says so by the error's `type`: `model_not_chat_capable` (the upstream
/// refused a tool-carrying chat turn outright — a model that only takes its
/// own vendor tool) or `model_disabled` (an operator took it off). Read by
/// type, never by the message: the message is the upstream's prose.
fn refuses_the_model(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["type"].as_str().map(|t| matches!(t, "model_not_chat_capable" | "model_disabled")))
        .unwrap_or(false)
}

/// A refused step, read as "this account is out of quota" — or not.
///
/// `Some((kind, resets_at))` for an HTTP 429 and nothing else: a 5xx, a 400,
/// a dropped socket are other failures and must not move a turn to another
/// model. `resets_at` comes from mafold-router's 429 body
/// (`error.resets_at`, unix seconds, the window it just observed) and falls
/// back to a `retry-after` header in seconds; neither ⇒ `None`, never a
/// guessed time. `kind` is the window's name when the body carries one.
fn quota_signal(status: u16, retry_after: Option<&str>, body: &str, now: i64) -> Option<(String, Option<i64>)> {
    if status != 429 {
        return None;
    }
    let err = serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("error").cloned());
    let kind = err
        .as_ref()
        .and_then(|e| e.get("kind"))
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .unwrap_or("quota")
        .to_string();
    let resets_at = err
        .as_ref()
        .and_then(|e| e.get("resets_at"))
        .and_then(Value::as_i64)
        .or_else(|| retry_after?.trim().parse::<i64>().ok().filter(|s| *s > 0).map(|s| now + s));
    Some((kind, resets_at))
}

/// Events a streaming completion emits while it runs.
pub enum ChatEvent {
    /// Assistant prose, as it arrives.
    Text(String),
    /// What the model thought, as it arrives: `reasoning_content` in pieces
    /// (DeepSeek, Kimi, GLM…), or the readable part of a whole
    /// `reasoning_details` block (Claude's summarized thinking). The kernel
    /// shows it folded, in front of what the step then says or does — the
    /// way Claude Code's thinking reads. The hand-back to the model is
    /// [`TurnMsg`]'s, not this.
    Thinking(String),
    /// Progress that is NOT prose — tool-argument fragments. Rendered as an
    /// empty delta so the generating card keeps its heartbeat.
    Pulse,
    Done(TurnMsg),
    Failed(String),
    /// The provider REFUSED this step because the account behind it is out of
    /// quota (HTTP 429) — as opposed to any other failure. Its own variant so
    /// the kernel decides what happens next from the STATUS, not by
    /// substring-matching an error string that crossed two process
    /// boundaries (`turn_failure_note` used to guess "429"/"rate" out of text).
    ///
    /// `resets_at` (unix seconds) when the gateway said when it lifts —
    /// mafold-router puts it in the 429 body; a bare `retry-after` works too.
    QuotaExhausted { kind: String, resets_at: Option<i64>, detail: String },
    /// The step's stream broke after it had shown some text or thinking, and
    /// the runtime is asking again from the top ([`STREAM_RETRY_AFTER`]):
    /// everything this step yielded so far is void. The kernel takes it back
    /// off the reply — the answer that follows is a new one, not the rest of
    /// the old one.
    Restart,
}

// ───────────────────────────── model runtime ─────────────────────────────

/// Where completions come from. `tools = None` means no tools are offered (a
/// profile whose plugins expose none); a runtime may still stream text, usage
/// and failures. Runtime #1 is OpenAI-compatible SSE; runtime #3
/// (connection-routed, the model on the sender's own device) implements the
/// same seam.
#[async_trait]
pub trait ModelRuntime: Send + Sync {
    /// Human-readable provider label for error messages.
    fn label(&self) -> &'static str;

    /// The model this runtime is CONFIGURED with — the agent's own pick, as
    /// opposed to a per-turn override.
    ///
    /// The billing gate needs it: a currency is a model id, so a gate told
    /// nothing falls back to "whichever currency the payer holds the most of"
    /// and silently swaps the agent's model out from under it. `None` for a
    /// runtime that resolves its model per turn (runtime #3 routes on the
    /// sender's credential) — there is no single configured answer there.
    fn configured_model(&self) -> Option<&str> {
        None
    }

    /// One model step over `messages`, streaming [`ChatEvent`]s.
    ///
    /// `want_usage`: ask the provider for token counts. Both plain replies and
    /// tool loops request it for the shared result details; absent usage stays
    /// unknown in the display even when billing separately uses an estimate.
    ///
    /// `model`: the turn's model override (a plugin's `model_override`, e.g.
    /// the sender's Customize pick), already validated by whoever produced
    /// it. `None` = the runtime's built-in default. A runtime that resolves
    /// its own model per turn (runtime #3 routes on the sender's credential)
    /// may ignore it.
    fn step(
        &self,
        messages: Vec<Value>,
        tools: Option<Value>,
        want_usage: bool,
        model: Option<String>,
    ) -> BoxStream<'static, ChatEvent>;

    /// The model id this runtime will ACTUALLY send, given the turn's override.
    ///
    /// The kernel needs it to ask the catalog whether pixels may be sent, and
    /// only the runtime knows: runtime #3 resolves its model from the sender's
    /// own Customize row and IGNORES the override, so a kernel guessing from
    /// `model_override` would guess `None` for every @chatgpt turn — the one
    /// bot this exists for.
    ///
    /// `None` ⇒ nothing to look up ⇒ no pixels. Defaulting closed is
    /// deliberate: sending bytes a model rejects turns a working turn into a
    /// 400, while not sending them costs one line of text.
    fn turn_model(&self, _model_override: Option<&str>) -> Option<String> {
        None
    }
}

/// Runtime #1: any `POST /v1/chat/completions` provider (DeepSeek, OpenAI,
/// proxies). Extracted from appbuilder's `chat_stream`; usage is requested
/// independently of whether the current request has tools.
///
/// `Clone` so a step's stream can own a copy and ask again after a broken
/// stream ([`STREAM_RETRY_AFTER`]); every field is a handle or a small value.
#[derive(Clone)]
pub struct OpenAiCompatRuntime {
    api_key: String,
    http: reqwest::Client,
    model: String,
    base_url: String,
    temperature: Option<f32>,
    provider_label: &'static str,
    /// Which family THIS TURN's model belongs to, answered by the catalog.
    ///
    /// `provider_label` alone is a per-profile constant, so a profile that can
    /// spend in more than one family labelled every failure with the same
    /// name — a hosted bot running a GPT model reported `anthropic 502`, and
    /// two sessions spent a day chasing Anthropic for a fault that was
    /// OpenAI's. A label that cannot be wrong about who failed is worth the
    /// indirection. `None` (unknown model, no catalog row) falls back to the
    /// profile's own label rather than guessing.
    provider_of_model: Option<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    /// The per-completion output cap to ask for. `None` leaves it to the
    /// provider/router default (64K for Claude through the router since
    /// 2026-10-07; it was 8192, which a thinking model could spend entirely on
    /// thinking and answer with nothing). A value the model rejects (400) is
    /// dropped and the request retried without it.
    max_tokens: Option<u32>,
    /// How hard to think — OpenAI's `reasoning_effort`, which the router turns
    /// into Claude's adaptive thinking + effort. `None` = never asked, which
    /// for Sonnet 4.6 (the hosted default) means **no thinking at all**.
    /// [`EFFORT_BY_MODEL`] = resolved per request from the model that step
    /// runs (a sender's pick can change it mid-profile).
    /// Rejected (400) ⇒ dropped and retried, like the other optional fields.
    reasoning_effort: Option<String>,
}

impl OpenAiCompatRuntime {
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        temperature: Option<f32>,
        provider_label: &'static str,
    ) -> Self {
        Self {
            api_key,
            // No client timeout on purpose (appbuilder's lesson): a long
            // generation is not a hang — liveness is the Pulse's job.
            http: reqwest::Client::new(),
            model,
            base_url,
            temperature,
            provider_label,
            provider_of_model: None,
            max_tokens: None,
            reasoning_effort: None,
        }
    }

    /// Ask for a larger output cap per completion. See
    /// [`OpenAiCompatRuntime::max_tokens`].
    pub fn with_max_tokens(mut self, n: Option<u32>) -> Self {
        self.max_tokens = n.filter(|n| *n > 0);
        self
    }

    /// Ask the model to think, at this effort. See
    /// [`OpenAiCompatRuntime::reasoning_effort`].
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        self
    }

    /// Teach the runtime to name the family of whatever model a turn actually
    /// ran on. See [`OpenAiCompatRuntime::provider_of_model`].
    pub fn with_provider_lookup(
        mut self,
        f: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    ) -> Self {
        self.provider_of_model = Some(f);
        self
    }

    /// Headers every request of this runtime carries besides the key, for a
    /// provider that routes by them. OpenCode Go asks a client for its own
    /// `User-Agent` and a stable `x-opencode-session` per conversation, and
    /// refuses a request without the session (400 `MissingSessionID`) — the
    /// first real Mafold Harness run on Go, 11-02, failed every call on it.
    /// A name or value that can't be a header is left out.
    pub fn with_headers(mut self, headers: &[(&str, &str)]) -> Self {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            if let (Ok(n), Ok(v)) =
                (reqwest::header::HeaderName::from_bytes(name.as_bytes()), reqwest::header::HeaderValue::from_str(value))
            {
                map.insert(n, v);
            }
        }
        // The same client `new` makes (no timeout), with the headers on it.
        if let Ok(http) = reqwest::Client::builder().default_headers(map).build() {
            self.http = http;
        }
        self
    }

    /// Who to blame for a failure on `model`. The catalog answers; the
    /// profile's own label is the fallback for a model nothing knows.
    fn label_for(&self, model: &str) -> String {
        self.provider_of_model
            .as_ref()
            .and_then(|f| f(model))
            .unwrap_or_else(|| self.provider_label.to_string())
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl ModelRuntime for OpenAiCompatRuntime {
    fn label(&self) -> &'static str {
        self.provider_label
    }

    fn configured_model(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn turn_model(&self, model_override: Option<&str>) -> Option<String> {
        Some(model_override.unwrap_or(&self.model).to_string())
    }

    /// One model step — asked again from the top when its stream breaks
    /// ([`OpenAiCompatRuntime::attempt`] says what that is), after the waits
    /// in [`STREAM_RETRY_AFTER`]. Fails only once the last attempt broke too.
    fn step(
        &self,
        messages: Vec<Value>,
        tools: Option<Value>,
        want_usage: bool,
        model: Option<String>,
    ) -> BoxStream<'static, ChatEvent> {
        let runtime = self.clone();
        Box::pin(async_stream::stream! {
            let mut waits = STREAM_RETRY_AFTER.iter();
            loop {
                let mut attempt = runtime.attempt(messages.clone(), tools.clone(), want_usage, model.clone());
                let mut shown = false;
                let mut broke = None;
                while let Some(item) = attempt.next().await {
                    match item {
                        Attempt::Event(ev) => {
                            shown |= matches!(ev, ChatEvent::Text(_) | ChatEvent::Thinking(_));
                            yield ev;
                        }
                        Attempt::Broke(why) => {
                            broke = Some(why);
                            break;
                        }
                    }
                }
                let Some(why) = broke else { return };
                let Some(wait) = waits.next() else {
                    yield ChatEvent::Failed(format!("{why} (asked {} times)", STREAM_RETRY_AFTER.len() + 1));
                    return;
                };
                tracing::warn!("harness: {why} — asking the step again in {wait:?}");
                // What the broken attempt showed goes before the next one
                // starts — never two answers run together in one bubble.
                if shown {
                    yield ChatEvent::Restart;
                }
                tokio::time::sleep(*wait).await;
            }
        })
    }
}

impl OpenAiCompatRuntime {
    /// One POST of a step and the read of its stream. The stream BROKE
    /// ([`Attempt::Broke`]) when it couldn't be read to its end (the
    /// connection reset under it), when the gateway said mid-stream that it
    /// broke (an `error` frame — mafold-router's word for an upstream that
    /// died partway), or when it just stopped without saying it was done (no
    /// `finish_reason`, no `[DONE]`). Anything else — every status, every
    /// refusal, a link that never answered — ends the step as it always did.
    fn attempt(
        &self,
        messages: Vec<Value>,
        tools: Option<Value>,
        want_usage: bool,
        model: Option<String>,
    ) -> BoxStream<'static, Attempt> {
        let http = self.http.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let key = self.api_key.clone();
        let model = model.unwrap_or_else(|| self.model.clone());
        let temperature = self.temperature;
        let max_tokens = self.max_tokens;
        let reasoning_effort = effort_for(self.reasoning_effort.as_deref(), &model);
        // Named after the model that actually ran, not after the profile.
        let label = self.label_for(&model);

        Box::pin(async_stream::stream! {
            let mut body = json!({
                "model": model,
                "messages": messages,
                "stream": true,
            });
            if let Some(t) = temperature {
                body["temperature"] = json!(t);
            }
            if let Some(n) = max_tokens {
                body["max_tokens"] = json!(n);
            }
            if let Some(e) = &reasoning_effort {
                body["reasoning_effort"] = json!(e);
            }
            if want_usage {
                // Real usage for both the compact result and its detail card.
                body["stream_options"] = json!({ "include_usage": true });
            }
            if let Some(tools) = tools {
                body["tools"] = tools;
                body["tool_choice"] = json!("auto");
            }
            // No status at all even after `post_step`'s quick resend: the link
            // is really down (or the endpoint is wrong) and the step fails fast,
            // as #880 settled. Only a stream that broke AFTER its status is
            // asked again.
            let mut resp = match post_step(&http, &url, &key, &body).await {
                Ok(r) => r,
                Err(e) => { yield Attempt::Event(ChatEvent::Failed(format!("{label}: {}", transport_error(e)))); return; }
            };
            // A 400 that refuses the MODEL, not anything in this request — the
            // router says which by type ([`refuses_the_model`]). The fallbacks
            // below drop one optional field at a time and ask again, which is
            // right when a provider rejects a field and pointless here: the
            // same model refuses the same way every time (10-10: Computer Use
            // was asked three times, and each of those twice upstream).
            if resp.status() == reqwest::StatusCode::BAD_REQUEST {
                let st = resp.status();
                let t = resp.text().await.unwrap_or_default();
                if refuses_the_model(&t) {
                    yield Attempt::Event(ChatEvent::Failed(format!("{label} {st}: {t}")));
                    return;
                }
                // Not the model: hand the fallbacks the same answer, as read.
                resp = reqwest::Response::from(
                    http::Response::builder().status(st).body(t).expect("status + string body"),
                );
            }
            // `stream_options` is OpenAI-compatible, but it is the only
            // optional field here and the whole turn dies if a provider (or a
            // proxy) rejects it. Token counts are a nicety; answering is not.
            if resp.status() == reqwest::StatusCode::BAD_REQUEST
                && body.get("stream_options").is_some()
            {
                tracing::warn!("harness: 400 with stream_options — retrying without it");
                body.as_object_mut().map(|o| o.remove("stream_options"));
                resp = match post_step(&http, &url, &key, &body).await {
                    Ok(r) => r,
                    Err(e) => { yield Attempt::Event(ChatEvent::Failed(format!("{label}: {}", transport_error(e)))); return; }
                };
            }
            // A provider (or a model) that does not take an effort setting must
            // still answer — thinking harder is a nicety, the reply is not.
            if resp.status() == reqwest::StatusCode::BAD_REQUEST && body.get("reasoning_effort").is_some() {
                tracing::warn!("harness: 400 with reasoning_effort — retrying without it");
                body.as_object_mut().map(|o| o.remove("reasoning_effort"));
                resp = match post_step(&http, &url, &key, &body).await {
                    Ok(r) => r,
                    Err(e) => { yield Attempt::Event(ChatEvent::Failed(format!("{label}: {}", transport_error(e)))); return; }
                };
            }
            // Same deal for a raised output cap: it only exists so long
            // tool arguments are not cut, and a model whose ceiling is lower
            // must still answer rather than lose the whole turn to a 400.
            if resp.status() == reqwest::StatusCode::BAD_REQUEST && body.get("max_tokens").is_some() {
                tracing::warn!("harness: 400 with max_tokens — retrying on the provider default");
                body.as_object_mut().map(|o| o.remove("max_tokens"));
                resp = match post_step(&http, &url, &key, &body).await {
                    Ok(r) => r,
                    Err(e) => { yield Attempt::Event(ChatEvent::Failed(format!("{label}: {}", transport_error(e)))); return; }
                };
            }
            if !resp.status().is_success() {
                let st = resp.status();
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let t = resp.text().await.unwrap_or_default();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if let Some((kind, resets_at)) = quota_signal(st.as_u16(), retry_after.as_deref(), &t, now) {
                    yield Attempt::Event(ChatEvent::QuotaExhausted { kind, resets_at, detail: format!("{label} {st}: {t}") });
                    return;
                }
                yield Attempt::Event(ChatEvent::Failed(format!("{label} {st}: {t}")));
                return;
            }

            let mut bytes = resp.bytes_stream().map_err(transport_error);
            // Whether the stream said it was done — a `finish_reason` or
            // `[DONE]` — and the gateway's mid-stream word that it broke. A
            // stream that ends with neither said is a cut, not an answer.
            let mut finished = false;
            let mut broke: Option<String> = None;
            let mut carry: Vec<u8> = Vec::new();
            let mut out = TurnMsg::default();
            out.stats.model = Some(model.clone());
            out.stats.provider = Some(label.clone());
            // Tool calls stream as index-keyed fragments: the first one
            // carries the id and the function name, every later one only
            // appends to `arguments`. Keyed by index, never by arrival order.
            let mut slots: HashMap<usize, ToolCall> = HashMap::new();
            let mut order: Vec<usize> = Vec::new();
            let mut last_pulse = Instant::now();

            macro_rules! fold_line {
                ($line:expr) => {{
                    let mut text: Option<String> = None;
                    let mut thought = String::new();
                    let mut progressed = false;
                    let line = String::from_utf8_lossy($line);
                    let line = line.trim();
                    if let Some(payload) = line.strip_prefix("data: ") {
                        finished |= payload == "[DONE]";
                        if payload != "[DONE]" {
                            if let Ok(chunk) = serde_json::from_str::<StreamChunk>(payload) {
                                if let Some(e) = chunk.error.filter(|e| !e.is_null()) {
                                    let kind = e["type"].as_str().unwrap_or("error");
                                    let message = e["message"].as_str().map(str::to_string).unwrap_or_else(|| e.to_string());
                                    broke = Some(format!("{label} stream: {kind}: {message}"));
                                }
                                if let Some(model) = chunk.model { out.stats.model = Some(model); }
                                if let Some(u) = chunk.usage {
                                    let stats = RunStats::openai(&u);
                                    out.tokens = stats.output_tokens.unwrap_or(0);
                                    out.input_tokens = stats.input_tokens.unwrap_or(0);
                                    out.stats = RunStats {
                                        model: out.stats.model.take(),
                                        provider: out.stats.provider.take(),
                                        ..stats
                                    };
                                }
                                for ch in chunk.choices {
                                    finished |= ch.finish_reason.is_some();
                                    if ch.finish_reason.as_deref() == Some("length") {
                                        out.truncated = true;
                                    }
                                    if let Some(reason) = ch.stop_reason {
                                        out.stop_reason = Some(reason);
                                    }
                                    if let Some(details) = ch.delta.reasoning_details {
                                        if !details.is_empty() {
                                            for t in details.iter().filter_map(readable_thought) {
                                                // Whole blocks, unlike `reasoning_content`'s
                                                // pieces: a second one is a new paragraph,
                                                // not the rest of the first.
                                                if !thought.is_empty() || out.reasoning.iter().any(|d| readable_thought(d).is_some()) {
                                                    thought.push_str("\n\n");
                                                }
                                                thought.push_str(t);
                                            }
                                            out.reasoning.extend(details);
                                            progressed = true;
                                        }
                                    }
                                    if let Some(r) = ch.delta.reasoning_content {
                                        let kept = out.reasoning_content.get_or_insert_with(String::new);
                                        if !r.is_empty() {
                                            kept.push_str(&r);
                                            thought.push_str(&r);
                                            progressed = true;
                                        }
                                    }
                                    if let Some(e) = ch.delta.extra_content.filter(|e| !e.is_null()) {
                                        out.extra_content = Some(e);
                                    }
                                    if let Some(c) = ch.delta.content {
                                        if !c.is_empty() {
                                            text = Some(match text.take() {
                                                Some(mut t) => { t.push_str(&c); t }
                                                None => c,
                                            });
                                        }
                                    }
                                    for tc in ch.delta.tool_calls.unwrap_or_default() {
                                        let slot = slots.entry(tc.index).or_insert_with(|| {
                                            order.push(tc.index);
                                            ToolCall::default()
                                        });
                                        if let Some(id) = tc.id {
                                            if !id.is_empty() { slot.id = id; }
                                        }
                                        if let Some(n) = tc.function.name {
                                            if !n.is_empty() { slot.name = n; }
                                        }
                                        if let Some(a) = tc.function.arguments {
                                            if !a.is_empty() {
                                                slot.arguments.push_str(&a);
                                                progressed = true;
                                            }
                                        }
                                        for (k, v) in tc.extra {
                                            if !TOOL_CALL_OWN_KEYS.contains(&k.as_str()) && !v.is_null() {
                                                slot.extra.insert(k, v);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    (text, thought, progressed)
                }};
            }

            'read: loop {
                let chunk = match StreamExt::next(&mut bytes).await {
                    None => break,
                    // The reply already said it was done (`finish_reason` comes
                    // after every word and tool argument): what is lost is at
                    // most the usage frame. Asking again would pay for another
                    // answer and swap out the one the person just read.
                    Some(Err(e)) if finished => {
                        tracing::warn!("harness: {label} stream: {e} — after the reply was whole, keeping it");
                        break;
                    }
                    Some(Err(e)) => { yield Attempt::Broke(format!("{label} stream: {e}")); return; }
                    Some(Ok(b)) => b,
                };
                carry.extend_from_slice(&chunk);
                while let Some(pos) = carry.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = carry.drain(..=pos).collect();
                    let (text, thought, progressed) = fold_line!(&line);
                    if broke.is_some() {
                        break 'read;
                    }
                    // A frame that ends the thinking and starts the answer
                    // says them in that order.
                    if !thought.is_empty() {
                        yield Attempt::Event(ChatEvent::Thinking(thought));
                    }
                    if let Some(t) = text {
                        out.content.push_str(&t);
                        yield Attempt::Event(ChatEvent::Text(t));
                    }
                    if progressed && last_pulse.elapsed() >= PULSE_EVERY {
                        last_pulse = Instant::now();
                        yield Attempt::Event(ChatEvent::Pulse);
                    }
                }
            }
            // A final unterminated line (stream ended without a trailing \n).
            if broke.is_none() && !carry.is_empty() {
                let (text, thought, _) = fold_line!(&carry);
                if broke.is_none() && !thought.is_empty() {
                    yield Attempt::Event(ChatEvent::Thinking(thought));
                }
                if let Some(t) = text.filter(|_| broke.is_none()) {
                    out.content.push_str(&t);
                    yield Attempt::Event(ChatEvent::Text(t));
                }
            }
            if broke.is_none() && !finished {
                broke = Some(format!("{label} stream ended before the reply did (no finish_reason, no [DONE])"));
            }
            if let Some(why) = broke {
                yield Attempt::Broke(why);
                return;
            }

            out.calls = order
                .into_iter()
                .filter_map(|i| slots.remove(&i))
                .filter(|c| !c.name.is_empty())
                .collect();
            yield Attempt::Event(ChatEvent::Done(out));
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没人选档位时按型号给:4.6 系(Sonnet/Opus)low,其余 high。按 id
    /// 机械地读,带日期后缀的老 id 也认得;一个真档位永远压过 `auto`。
    #[test]
    fn an_unchosen_effort_is_picked_by_the_model_and_a_chosen_one_wins() {
        assert_eq!(default_reasoning_effort("claude-sonnet-4-6"), "low");
        assert_eq!(default_reasoning_effort("claude-opus-4-6"), "low");
        assert_eq!(default_reasoning_effort("claude-sonnet-4-6-20260217"), "low");
        for m in ["claude-opus-5", "claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1", "deepseek-v4-pro"] {
            assert_eq!(default_reasoning_effort(m), "high", "{m}");
        }
        assert_eq!(effort_for(Some("auto"), "claude-sonnet-4-6").as_deref(), Some("low"));
        assert_eq!(effort_for(Some("auto"), "claude-opus-5-5").as_deref(), Some("high"));
        assert_eq!(effort_for(Some("high"), "claude-sonnet-4-6").as_deref(), Some("high"), "主人选的照用");
        assert_eq!(effort_for(Some("none"), "claude-opus-5-5").as_deref(), Some("none"));
        assert_eq!(effort_for(None, "claude-sonnet-4-6"), None, "没开这个开关的 runtime 不发");
    }

    /// THE REGRESSION THIS EXISTS FOR. `mafold-hosted` bills in two families,
    /// and the runtime's error label was `billing.providers[0]` — a constant.
    /// So a turn that failed on a GPT model announced itself as
    /// `anthropic 502`, and 2026-09-03 two sessions spent a day chasing
    /// Anthropic for a fault that belonged to the OpenAI seat. A label that
    /// names the wrong vendor is worse than no label: it is evidence pointing
    /// away from the bug.
    #[test]
    fn the_error_label_names_the_model_that_actually_ran() {
        let catalog: Arc<dyn Fn(&str) -> Option<String> + Send + Sync> =
            Arc::new(|m: &str| match m {
                "claude-sonnet-4-6" => Some("anthropic".to_string()),
                "gpt-5.6-luna" => Some("openai".to_string()),
                _ => None,
            });
        let rt = OpenAiCompatRuntime::new(
            "k".into(),
            "https://x.test/v1".into(),
            "claude-sonnet-4-6".into(),
            None,
            // What the manifest's `providers[0]` used to decide for everyone.
            "anthropic",
        )
        .with_provider_lookup(catalog);

        assert_eq!(rt.label_for("claude-sonnet-4-6"), "anthropic");
        assert_eq!(
            rt.label_for("gpt-5.6-luna"),
            "openai",
            "a GPT failure must not be reported as anthropic"
        );
        // A model the catalog has never heard of falls back to the profile's
        // own label rather than guessing a vendor from its name.
        assert_eq!(rt.label_for("something-unreleased"), "anthropic");
    }

    /// Without a lookup wired in, nothing changes — the plain single-family
    /// runtimes keep the label they were built with.
    #[test]
    fn a_runtime_with_no_lookup_keeps_its_own_label() {
        let rt = OpenAiCompatRuntime::new(
            "k".into(),
            "https://x.test/v1".into(),
            "m-pro".into(),
            None,
            "deepseek",
        );
        assert_eq!(rt.label_for("m-pro"), "deepseek");
        assert_eq!(rt.label_for("anything"), "deepseek");
    }

    #[tokio::test]
    async fn transport_failures_keep_the_cause_but_not_url_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            drop(socket);
        });
        let url = format!("http://name:secret-password@{address}/private-path?api_key=secret-query");
        let error = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build().unwrap()
            .get(url).send().await.unwrap_err();
        server.await.unwrap();
        let error = transport_error(error);
        assert!(error.contains("127.0.0.1"), "safe endpoint hostname is useful: {error}");
        assert!(error.matches(':').count() >= 2, "retain the underlying cause: {error}");
        for secret in ["secret-password", "secret-query", "private-path"] { assert!(!error.contains(secret), "{error}"); }
    }

    /// A stand-in upstream that answers each connection with the next script
    /// entry: `None` = read the request, then hang up without a word.
    async fn scripted_upstream(script: Vec<Option<&'static str>>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            for answer in script {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = vec![0u8; 64 * 1024];
                let _ = sock.read(&mut buf).await;
                match answer {
                    None => drop(sock),
                    Some(reply) => { let _ = sock.write_all(reply.as_bytes()).await; }
                }
            }
        });
        (url, hits)
    }

    const OK_REPLY: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";

    /// 2026-10-09: the api↔router link dropped mid-request and a whole turn
    /// was lost. Nothing had come back, so it is sent once more.
    #[tokio::test]
    async fn a_connection_dropped_before_any_reply_is_sent_once_more() {
        let (url, hits) = scripted_upstream(vec![None, Some(OK_REPLY)]).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = post_step(&http, &url, "k", &json!({"model": "m"})).await.expect("second attempt answers");
        assert_eq!(resp.status(), 200);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// Once, not a loop: a link that is really down fails fast, cause intact.
    #[tokio::test]
    async fn a_second_drop_is_reported_not_retried_again() {
        let (url, hits) = scripted_upstream(vec![None, None, Some(OK_REPLY)]).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let err = post_step(&http, &url, "k", &json!({})).await.expect_err("gives up after two");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(transport_error(err).matches(':').count() >= 2, "keeps the cause");
    }

    /// A status is an answer — even a bad one goes the usual way, never again.
    #[tokio::test]
    async fn an_answered_request_is_never_resent() {
        let (url, hits) = scripted_upstream(vec![
            Some("HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"),
            Some(OK_REPLY),
        ])
        .await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = post_step(&http, &url, "k", &json!({})).await.unwrap();
        assert_eq!(resp.status(), 502);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    async fn plain_step(base: &str) -> Vec<ChatEvent> {
        let rt = OpenAiCompatRuntime::new("k".into(), base.into(), "claude-opus-5-5".into(), None, "anthropic");
        rt.step(vec![json!({ "role": "user", "content": "hi" })], None, true, None).collect().await
    }

    fn failure_in(events: &[ChatEvent]) -> Option<&str> {
        events.iter().find_map(|e| match e { ChatEvent::Failed(t) => Some(t.as_str()), _ => None })
    }

    /// Counter-test: a link that drops before ANY status keeps #880's one
    /// quick resend and then fails fast — a dead or mistyped endpoint must not
    /// cost the person three rounds of waiting.
    #[tokio::test]
    async fn a_link_down_before_any_status_still_fails_fast() {
        let (url, hits) = scripted_upstream(vec![None, None, Some(OK_REPLY)]).await;
        let base = url.trim_end_matches("/chat/completions").to_string();
        let events = plain_step(&base).await;
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "post_step's resend, nothing more");
        assert!(failure_in(&events).is_some());
    }

    /// A streamed answer: these `data:` frames, then `[DONE]`.
    macro_rules! sse {
        ($($frame:literal),* $(,)?) => {
            concat!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                $("data: ", $frame, "\n\n",)*
                "data: [DONE]\n\n"
            )
        };
    }

    /// A DeepSeek step in thinking mode, as the router passes it through: the
    /// thinking in pieces, then a tool call.
    const DS_TOOL_STEP: &str = sse!(
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":""},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"Run it, "},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"then say what it printed."},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    );

    /// The same, from a step that thought nothing: the key, empty.
    const DS_BLANK_THOUGHT: &str = sse!(
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":""},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    );

    /// A provider without the dialect: no `reasoning_content` anywhere.
    const PLAIN_TOOL_STEP: &str = sse!(
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    );

    /// One step with a tool offered, against an upstream that answers `reply`.
    async fn tool_step(reply: &'static str) -> TurnMsg {
        let (url, _) = scripted_upstream(vec![Some(reply)]).await;
        let base = url.trim_end_matches("/chat/completions").to_string();
        let rt = OpenAiCompatRuntime::new("k".into(), base, "deepseek-v4-flash".into(), None, "the wallet's router");
        let tools = json!([{ "type": "function", "function": { "name": "Bash", "parameters": { "type": "object" } } }]);
        let events: Vec<ChatEvent> = rt.step(vec![json!({ "role": "user", "content": "run it" })], Some(tools), true, None).collect().await;
        let failed = failure_in(&events).map(str::to_string);
        events.into_iter().find_map(|e| match e { ChatEvent::Done(m) => Some(m), _ => None }).unwrap_or_else(|| panic!("no step: {failed:?}"))
    }

    /// DeepSeek refuses the next request of a tool loop (400, "The
    /// `reasoning_content` in the thinking mode must be passed back to the
    /// API.") when the step that called the tool comes back without its
    /// thinking. It was dropped at the parser until 11-02: Mafold Harness on
    /// the wallet failed every DeepSeek tool loop at its second call.
    #[tokio::test]
    async fn deepseeks_thinking_goes_back_with_the_step_that_called_a_tool() {
        let m = tool_step(DS_TOOL_STEP).await;
        assert_eq!(m.calls.len(), 1);
        assert_eq!(m.reasoning_content.as_deref(), Some("Run it, then say what it printed."));
        let sent = crate::kernel::assistant_value(&m);
        assert_eq!(sent["reasoning_content"], "Run it, then say what it printed.");
        assert_eq!(sent["tool_calls"][0]["function"]["name"], "Bash");
    }

    /// What DeepSeek checks is the key: a step that thought nothing hands back
    /// an empty one.
    #[tokio::test]
    async fn a_step_that_thought_nothing_still_hands_back_the_key() {
        let m = tool_step(DS_BLANK_THOUGHT).await;
        assert_eq!(m.reasoning_content.as_deref(), Some(""));
        assert_eq!(crate::kernel::assistant_value(&m)["reasoning_content"], "");
    }

    /// Counter-test: a provider that never sends it gets nothing added — its
    /// requests stay what they were.
    #[tokio::test]
    async fn no_reasoning_content_is_made_up() {
        let m = tool_step(PLAIN_TOOL_STEP).await;
        assert_eq!(m.reasoning_content, None);
        let sent = crate::kernel::assistant_value(&m);
        assert!(sent.get("reasoning_content").is_none(), "{sent}");
        assert_eq!(sent["tool_calls"][0]["function"]["name"], "Bash");
    }

    async fn events_of(reply: &'static str) -> Vec<ChatEvent> {
        let (url, _) = scripted_upstream(vec![Some(reply)]).await;
        let base = url.trim_end_matches("/chat/completions").to_string();
        let rt = OpenAiCompatRuntime::new("k".into(), base, "m".into(), None, "the router");
        rt.step(vec![json!({ "role": "user", "content": "hi" })], None, true, None).collect().await
    }

    /// What a step showed, in the order it showed it.
    fn shown(events: &[ChatEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Thinking(t) => Some(format!("thinking:{t}")),
                ChatEvent::Text(t) => Some(format!("text:{t}")),
                _ => None,
            })
            .collect()
    }

    /// DeepSeek's dialect: the thinking in pieces, then the answer.
    const DS_TEXT_STEP: &str = sse!(
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":""},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"Short "},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"answer."},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"42"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    );

    /// The thinking reaches the kernel as it streams, ahead of the answer —
    /// to be shown (11-03 "Show it"), on top of going back to the model.
    #[tokio::test]
    async fn the_thinking_streams_ahead_of_the_answer() {
        let events = events_of(DS_TEXT_STEP).await;
        assert_eq!(shown(&events), ["thinking:Short ", "thinking:answer.", "text:42"]);
        let Some(ChatEvent::Done(m)) = events.last() else { panic!("no step") };
        assert_eq!(m.reasoning_content.as_deref(), Some("Short answer."));
    }

    /// Claude through mafold-router: whole blocks in `reasoning_details`.
    /// The text of one is shown; an encrypted block, and one that came back
    /// empty (Claude's `display: "omitted"`), show nothing. All three still
    /// go back to the model.
    const CLAUDE_STEP: &str = sse!(
        r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"先想一下","signature":"S1","format":"anthropic-claude-v1","index":0}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.encrypted","data":"BLOB","format":"anthropic-claude-v1","index":1}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"","signature":"S2","format":"anthropic-claude-v1","index":2}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"是 42"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    );

    #[tokio::test]
    async fn a_thinking_blocks_text_is_shown_and_an_encrypted_or_empty_one_is_not() {
        let events = events_of(CLAUDE_STEP).await;
        assert_eq!(shown(&events), ["thinking:先想一下", "text:是 42"]);
        let Some(ChatEvent::Done(m)) = events.last() else { panic!("no step") };
        assert_eq!(m.reasoning.len(), 3, "every block still goes back");
    }

    /// Two whole blocks are two paragraphs, not one run-on sentence.
    const TWO_BLOCKS: &str = sse!(
        r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.text","text":"One.","signature":"S1","index":0}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.summary","summary":"Two.","index":1}]},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    );

    #[tokio::test]
    async fn a_second_block_is_a_new_paragraph() {
        assert_eq!(shown(&events_of(TWO_BLOCKS).await), ["thinking:One.", "thinking:\n\nTwo.", "text:ok"]);
    }

    /// A stand-in OpenCode Go: refuses a request without `x-opencode-session`
    /// exactly as Go does, answers one with it, and keeps every request head.
    async fn go_upstream(answers: usize) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = heads.clone();
        tokio::spawn(async move {
            for _ in 0..answers {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 64 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).split("\r\n\r\n").next().unwrap_or_default().to_lowercase();
                let reply = if head.contains("\r\nx-opencode-session: ") {
                    sse!(r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#)
                } else {
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n\
                     {\"type\":\"error\",\"error\":{\"type\":\"MissingSessionID\",\"message\":\"Request is missing x-opencode-session and cannot be routed efficiently.\"}}"
                };
                seen.lock().unwrap().push(head);
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (base, heads)
    }

    async fn go_step(rt: OpenAiCompatRuntime) -> Vec<ChatEvent> {
        rt.step(vec![json!({ "role": "user", "content": "hi" })], None, true, None).collect().await
    }

    /// 11-02, the first real run on OpenCode Go: every call 400
    /// `MissingSessionID`. With the headers on, the session and the client's
    /// own name ride the request and Go answers.
    #[tokio::test]
    async fn the_headers_ride_every_request() {
        let (base, heads) = go_upstream(1).await;
        let rt = OpenAiCompatRuntime::new("k".into(), base, "kimi-k2.7-code".into(), None, "OpenCode Go")
            .with_headers(&[("x-opencode-session", "6f1c0a52-session"), ("User-Agent", "mafold-harness/0.9.157")]);
        let events = go_step(rt).await;
        assert!(failure_in(&events).is_none(), "{:?}", failure_in(&events));
        assert!(events.iter().any(|e| matches!(e, ChatEvent::Done(_))));
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 1);
        assert!(heads[0].contains("\r\nx-opencode-session: 6f1c0a52-session"), "{}", heads[0]);
        assert!(heads[0].contains("\r\nuser-agent: mafold-harness/0.9.157"), "{}", heads[0]);
        assert!(heads[0].contains("\r\nauthorization: bearer k"), "the key still goes: {}", heads[0]);
    }

    /// Counter-test: the stand-in really refuses what the kernel sent before.
    #[tokio::test]
    async fn without_them_go_refuses_with_its_own_words() {
        let (base, heads) = go_upstream(3).await;
        let rt = OpenAiCompatRuntime::new("k".into(), base, "kimi-k2.7-code".into(), None, "OpenCode Go");
        let events = go_step(rt).await;
        let failed = failure_in(&events).expect("refused");
        assert!(failed.contains("MissingSessionID") || failed.contains("x-opencode-session"), "{failed}");
        assert!(heads.lock().unwrap().iter().all(|h| !h.contains("x-opencode-session")));
    }

    #[test]
    fn a_429_is_quota_and_nothing_else_is() {
        let now = 1_790_530_000;
        let router = r#"{"error":{"message":"上游 429","type":"rate_limit_exceeded","kind":"five_hour","resets_at":1790537400}}"#;
        assert_eq!(quota_signal(429, None, router, now), Some(("five_hour".into(), Some(1790537400))));
        assert_eq!(quota_signal(429, Some("60"), "{}", now), Some(("quota".into(), Some(now + 60))));
        assert_eq!(quota_signal(429, None, "not json", now), Some(("quota".into(), None)), "no time is not a made-up time");
        for other in [400, 402, 500, 502, 503] {
            assert_eq!(quota_signal(other, Some("60"), router, now), None, "{other} is not a quota");
        }
    }
}
