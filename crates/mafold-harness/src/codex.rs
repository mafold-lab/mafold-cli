//! Runtime #4: the Codex Responses API on a ChatGPT login
//! (`chatgpt.com/backend-api/codex/responses`) — Mafold Harness on its
//! owner's computer, signed in with Codex (mafold-cli, flag harnessOnCodex;
//! ops 11-23 「codex as fuel也补充一下(connection 和 本地复用)」).
//!
//! Called directly, not through the real `codex`: its own shell and patch
//! tools can't be reliably switched off the way `claude --tools ""` can, and
//! the harness brings its own. Where the login comes from — a Codex
//! connection opened from the owner's vault, or the computer's own Codex
//! login — is the host's ([`CodexCredentials`]); this side only speaks the
//! wire.
//!
//! The wire itself is the half every Codex caller shares: the kernel's chat
//! transcript going up as a Responses request ([`build_request_from_chat`])
//! and the SSE coming down ([`SseParser`]). mafold-api's @chatgpt relay
//! (`brains::codex_connection`) uses the same two, so the protocol has one
//! place to change.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use mafold_types::connections::codex;
use serde_json::{json, Value};

use crate::kernel::TokenUsage;
use crate::runtime::{effort_for, quota_signal, retrying, send_once_more, transport_error, Attempt, ChatEvent, ModelRuntime, ToolCall, TurnMsg};

// ───────────────────────────── going up ─────────────────────────────

/// Chat-completions tools → the Responses flattened form (`type`/`name`/
/// `description`/`parameters` at top level). The kernel speaks the chat
/// shape; this endpoint does not.
pub fn tools_to_responses(tools: &Value) -> Value {
    let arr = tools.as_array().cloned().unwrap_or_default();
    Value::Array(
        arr.into_iter()
            .filter_map(|t| {
                let f = t.get("function")?;
                Some(json!({
                    "type": "function",
                    "name": f.get("name")?.clone(),
                    "description": f.get("description").cloned().unwrap_or(json!("")),
                    "parameters": f
                        .get("parameters")
                        .cloned()
                        .unwrap_or(json!({"type": "object"})),
                    "strict": false,
                }))
            })
            .collect(),
    )
}

/// A base64 image counts as this many characters toward the billing clamp,
/// whatever its byte length.
///
/// Calibrated, not guessed. Probed against the real endpoint: a 64×64 image
/// reports `input_tokens: 47` for the whole request, and a 1024×1024 one
/// (3 MB png / 4 MB data url — our per-image ceiling) reports `1266`. At
/// `plausible_input`'s ×3 rule this allowance is a ~12k-token headroom, so
/// it clears the worst case by roughly ten times while still refusing the
/// 4.1M-token ceiling a raw base64 length would have handed a supplier.
const IMAGE_CHARS: usize = 4_000;

/// The size of a request as the BILLING clamp should see it.
///
/// `plausible_input` bounds a device-reported token count by what we
/// actually sent, which is what stops a supplying machine inflating its
/// bill on the market path. Measuring that with `body.to_string().len()`
/// silently stopped holding the moment images could ride: a 1 MB photo
/// inlines to ~1.4M characters, which is a ~4.1M-token ceiling for input a
/// vendor prices in the low thousands — the clamp would still be there and
/// would never fire. Text counts as text; an image counts as a flat
/// allowance.
pub fn billable_chars(body: &Value) -> usize {
    let mut n = 0usize;
    // instructions + tools + the scaffolding, minus the input items.
    if let Some(s) = body.get("instructions").and_then(Value::as_str) {
        n += s.len();
    }
    if let Some(t) = body.get("tools") {
        n += t.to_string().len();
    }
    for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
        match item.get("content").or_else(|| item.get("output")).and_then(Value::as_array) {
            Some(parts) => {
                for p in parts {
                    match p.get("type").and_then(Value::as_str) {
                        Some("input_image") => n += IMAGE_CHARS,
                        _ => n += p.get("text").and_then(Value::as_str).map_or(0, str::len),
                    }
                }
            }
            // function_call / function_call_output and anything else: its
            // literal size is its cost, same as before.
            None => n += item.to_string().len(),
        }
    }
    n
}

/// Chat-format `content` → Responses content parts.
///
/// `content` is a plain string on a text turn and the OpenAI multimodal
/// ARRAY when the kernel attached a picture. THE TRAP THIS CLOSES: this
/// used to read it with `as_str().unwrap_or("")`, so the first multimodal
/// message yielded `""` — and the caller's `if !content.is_empty()` then
/// dropped the whole item, taking the user's QUESTION with it, not just
/// their photo.
fn content_parts(content: &Value, text_part: &str) -> Vec<Value> {
    match content {
        Value::String(s) if !s.is_empty() => {
            vec![json!({ "type": text_part, "text": s })]
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(|t| json!({ "type": text_part, "text": t })),
                // Images are INPUT only — this endpoint has no assistant
                // image part, so one on a non-user turn is dropped rather
                // than refused. Note the Responses shape takes `image_url`
                // as a BARE STRING, unlike chat-completions' object; this
                // was read off the official client's own request via
                // `codex debug prompt-input -i`.
                Some("image_url") if text_part == "input_text" => p
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .map(|u| json!({ "type": "input_image", "image_url": u, "detail": "high" })),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A string-only sink's view of `content` (`instructions`,
/// `function_call_output`): the text parts, joined. Flattening beats
/// `unwrap_or("")`, which would silently drop a system prompt that happened
/// to arrive in array form.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Build the request from the KERNEL's chat-format transcript (system /
/// user / assistant(+tool_calls) / tool), so tool rounds ride the same
/// endpoint contract as plain turns: system → `instructions` (the endpoint
/// refuses `role:"system"`), assistant `tool_calls` → `function_call`
/// items, `role:"tool"` replies → `function_call_output` items.
pub fn build_request_from_chat(model: &str, messages: &[Value], tools: Option<&Value>) -> Value {
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let raw = m.get("content").unwrap_or(&Value::Null);
        match role {
            "system" => {
                let content = text_of(raw);
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(&content);
            }
            "user" => {
                let parts = content_parts(raw, "input_text");
                if !parts.is_empty() {
                    input.push(json!({
                        "type": "message", "role": "user", "content": parts,
                    }));
                }
            }
            "assistant" => {
                let parts = content_parts(raw, "output_text");
                if !parts.is_empty() {
                    input.push(json!({
                        "type": "message", "role": "assistant", "content": parts,
                    }));
                }
                for tc in m.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let f = tc.get("function").cloned().unwrap_or(json!({}));
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.get("id").cloned().unwrap_or(json!("")),
                        "name": f.get("name").cloned().unwrap_or(json!("")),
                        "arguments": f.get("arguments").cloned().unwrap_or(json!("{}")),
                    }));
                }
            }
            "tool" => {
                // A tool that handed back a picture (`Read` on a photo) keeps
                // it: `output` may be content parts, the same `input_image`
                // the official client sends for its own `view_image`. Text
                // alone stays the plain string it always was.
                let pictured = raw
                    .as_array()
                    .is_some_and(|ps| ps.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("image_url")));
                let output = if pictured { json!(content_parts(raw, "input_text")) } else { json!(text_of(raw)) };
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": m.get("tool_call_id").cloned().unwrap_or(json!("")),
                    "output": output,
                }));
            }
            _ => {}
        }
    }
    if instructions.is_empty() {
        // The endpoint REQUIRES non-empty instructions; a profile always
        // has a persona section, so this is a belt for tests, not a path.
        instructions.push(' ');
    }
    json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "tools": tools.map(tools_to_responses).unwrap_or_else(|| json!([])),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "store": false,
        "stream": true,
    })
}

// ───────────────────────────── coming down ─────────────────────────────

pub enum Event {
    Text(String),
    /// What the model thought, in the summary the endpoint streams when the
    /// request asked for one (`reasoning.summary`).
    Thinking(String),
    Failed(String),
    /// A complete tool call, off a `response.output_item.done` frame.
    /// Function calls land WHOLE in item events on this endpoint (argument
    /// deltas exist only for custom tools), so there is no slot assembly.
    Call {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The turn's terminal frame, carrying the priced usage the backend
    /// reports (`None` when it reports none — old backends, and every
    /// stream that dies before this frame).
    ///
    /// This is the METER for the resale path: it is parsed on the server,
    /// out of bytes the supplying device only forwarded. The device could
    /// still lie about them, which is why the caller cross-checks the
    /// reported output against the text it actually received.
    Completed(Option<TokenUsage>),
}

/// Incremental SSE reader. The device guarantees complete LINES per chunk,
/// but an SSE *event* may still span chunks, so the carry works on lines.
#[derive(Default)]
pub struct SseParser {
    carry: String,
    pub stats: mafold_transcript::RunStats,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &str) -> Vec<Event> {
        let mut out = Vec::new();
        self.carry.push_str(chunk);
        while let Some(pos) = self.carry.find('\n') {
            let line: String = self.carry.drain(..=pos).collect();
            let line = line.trim();
            let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                continue; // `event:` names, comments, blank separators
            };
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str).unwrap_or("") {
                "response.output_text.delta" => {
                    if let Some(t) = v.get("delta").and_then(Value::as_str) {
                        if !t.is_empty() {
                            out.push(Event::Text(t.to_string()));
                        }
                    }
                }
                "response.reasoning_summary_text.delta" => {
                    if let Some(t) = v.get("delta").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                        out.push(Event::Thinking(t.to_string()));
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = v.get("item") {
                        if item.get("type").and_then(Value::as_str) == Some("function_call") {
                            out.push(Event::Call {
                                call_id: item
                                    .get("call_id")
                                    .or_else(|| item.get("id"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                name: item.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                                arguments: item.get("arguments").and_then(Value::as_str).unwrap_or("{}").to_string(),
                            });
                        }
                    }
                }
                "response.completed" => {
                    let u = &v["response"]["usage"];
                    self.stats = if u.get("input_tokens").is_some() {
                        mafold_transcript::RunStats::codex(u)
                    } else {
                        mafold_transcript::RunStats::openai(u)
                    };
                    self.stats.effort = v["response"]["reasoning"]["effort"].as_str().map(str::to_string);
                    self.stats.context_used_tokens = self.stats.input_tokens;
                    self.stats.context_basis = self.stats.input_tokens.map(|_| "last_request_input".into());
                    self.stats.model = v["response"]["model"].as_str().map(str::to_string);
                    out.push(Event::Completed(usage_of(&v)));
                }
                "response.failed" | "response.incomplete" => {
                    out.push(Event::Failed(failure_message(&v)));
                }
                "error" => {
                    let msg = v.get("message").and_then(Value::as_str).unwrap_or("the backend reported an error").to_string();
                    out.push(Event::Failed(msg));
                }
                _ => {}
            }
        }
        out
    }
}

/// The priced usage out of a `response.completed` frame.
///
/// Absent numbers are a `None`, never a zero: zero is a real (free) turn
/// and "the backend didn't say" is not. The distinction is what decides
/// between billing a fact and billing an estimate.
fn usage_of(v: &Value) -> Option<TokenUsage> {
    let u = v.get("response")?.get("usage")?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    // `input_tokens` on the Responses API; `prompt_tokens` is the
    // chat-completions spelling, accepted because the same account type has
    // been seen serving both.
    let input = n("input_tokens").or_else(|| n("prompt_tokens"))?;
    let output = n("output_tokens").or_else(|| n("completion_tokens"))?;
    Some(TokenUsage { input_tokens: input, output_tokens: output })
}

/// Dig the human sentence out of a failed/incomplete response event.
fn failure_message(v: &Value) -> String {
    let r = v.get("response").unwrap_or(v);
    r.get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            r.get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(|s| format!("incomplete: {s}"))
        })
        .unwrap_or_else(|| "the backend ended the response without output".into())
}

// ───────────────────────────── the runtime ─────────────────────────────

/// A Codex login, as one request sends it: the bearer and the ChatGPT
/// account it is for.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexAuth {
    pub access_token: String,
    pub account_id: String,
}

/// Written by hand so the token never reaches a log line.
impl std::fmt::Debug for CodexAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth").field("access_token", &"<redacted>").field("account_id", &self.account_id).finish()
    }
}

/// Where a turn's Codex login comes from, and how it is renewed — the host's
/// to say (mafold-cli: the owner's Codex connection, or this computer's own
/// Codex login). `Err` is the turn's failure, in the words the reply ends with.
#[async_trait]
pub trait CodexCredentials: Send + Sync {
    /// The login to send now.
    async fn current(&self) -> Result<CodexAuth, String>;
    /// The endpoint refused `refused` (HTTP 401): renew it — the renewed
    /// grant stored wherever the login lives — and hand back the fresh one.
    async fn renew(&self, refused: &CodexAuth) -> Result<CodexAuth, String>;
}

// ───────────────────────────── the catalog ─────────────────────────────

/// One model a Codex login runs, as its catalog says.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodexModel {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The efforts it takes, as the catalog lists them. Empty: no dial.
    #[serde(default)]
    pub efforts: Vec<String>,
}

/// How long a catalog read may take: it is a menu, never a reply's wait.
const CATALOG_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// The models `auth`'s login runs, from Codex's own catalog
/// (`codex::models_url`) — what the Codex CLI's picker lists for it, rather
/// than a roster written here that the next release leaves behind (ops 11-24:
/// an agent on Codex offered only GPT-5.6 while Codex itself listed GPT-6.x).
pub async fn list_models(auth: &CodexAuth) -> Result<Vec<CodexModel>, String> {
    list_models_at(&codex::models_url(), auth).await
}

/// [`list_models`] from `url` — a test's stand-in.
pub async fn list_models_at(url: &str, auth: &CodexAuth) -> Result<Vec<CodexModel>, String> {
    let req = codex::models_headers(&auth.access_token, &auth.account_id)
        .iter()
        .fold(reqwest::Client::new().get(url), |r, (k, v)| r.header(k.as_str(), v.as_str()))
        .timeout(CATALOG_WAIT);
    let resp = req.send().await.map_err(|e| format!("codex models: {}", transport_error(e)))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("codex models answered {}", status.as_u16()));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| "codex models: not a model list".to_string())?;
    let models = parse_catalog(&v);
    if models.is_empty() {
        return Err("codex models: the list is empty".into());
    }
    Ok(models)
}

/// The catalog's answer: the models its picker shows (`visibility: list`), in
/// its own order (`priority`, lowest first), each with its efforts
/// (`supported_reasoning_levels`). The same reading as mafold-router's
/// (`catalog::from_codex`), which leaves the efforts out only because its
/// Codex calls send none.
pub fn parse_catalog(body: &Value) -> Vec<CodexModel> {
    let mut rows: Vec<(i64, CodexModel)> = Vec::new();
    for m in body.get("models").and_then(Value::as_array).into_iter().flatten() {
        if m.get("visibility").and_then(Value::as_str) != Some("list") {
            continue;
        }
        let Some(slug) = m.get("slug").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if rows.iter().any(|(_, o)| o.id == slug) {
            continue;
        }
        let text = |k: &str| m.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let efforts = m
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| l.get("effort").and_then(Value::as_str).or_else(|| l.as_str()))
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect();
        rows.push((
            m.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX),
            CodexModel { id: slug.to_string(), label: text("display_name").unwrap_or_else(|| slug.to_string()), description: text("description"), efforts },
        ));
    }
    rows.sort_by_key(|(p, _)| *p);
    rows.into_iter().map(|(_, m)| m).collect()
}

/// Runtime #4 — see the module docs. `Clone` for [`retrying`], like the others.
#[derive(Clone)]
pub struct CodexResponsesRuntime {
    creds: Arc<dyn CodexCredentials>,
    http: reqwest::Client,
    url: String,
    model: String,
    /// The conversation's id: the session the endpoint pins, and the key its
    /// prompt cache is held under.
    session: String,
    reasoning_effort: Option<String>,
}

impl CodexResponsesRuntime {
    pub fn new(creds: Arc<dyn CodexCredentials>, model: String, session: String) -> Self {
        Self {
            creds,
            // No client timeout, for runtime #1's reason: a long generation
            // is not a hang.
            http: reqwest::Client::new(),
            url: codex::RESPONSES_URL.to_string(),
            model,
            session,
            reasoning_effort: None,
        }
    }

    /// Post somewhere else than Codex's own endpoint — a test's stand-in.
    pub fn with_url(mut self, url: String) -> Self {
        self.url = url;
        self
    }

    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl ModelRuntime for CodexResponsesRuntime {
    fn label(&self) -> &'static str {
        "codex"
    }

    fn configured_model(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn turn_model(&self, model_override: Option<&str>) -> Option<String> {
        Some(model_override.unwrap_or(&self.model).to_string())
    }

    fn step(&self, messages: Vec<Value>, tools: Option<Value>, _want_usage: bool, model: Option<String>) -> BoxStream<'static, ChatEvent> {
        // Usage always comes, on `response.completed`.
        let runtime = self.clone();
        retrying(move || runtime.attempt(&messages, tools.as_ref(), model.clone()))
    }
}

impl CodexResponsesRuntime {
    /// One request body: the shared translation, the prompt cache keyed by
    /// the conversation, and the thinking asked for — its summary streamed
    /// back, at the effort picked when one was.
    fn body(&self, messages: &[Value], tools: Option<&Value>, model: &str) -> Value {
        let mut body = build_request_from_chat(model, messages, tools);
        body["prompt_cache_key"] = json!(self.session);
        let mut reasoning = json!({ "summary": "auto" });
        if let Some(e) = effort_for(self.reasoning_effort.as_deref(), model) {
            reasoning["effort"] = json!(e);
        }
        body["reasoning"] = reasoning;
        body
    }

    /// One POST and the read of its stream. A 401 renews the login once and
    /// asks again; a 400 about the thinking asked for asks again without it;
    /// a stream that ended before `response.completed` BROKE, and the step is
    /// asked again from the top ([`retrying`]).
    fn attempt(&self, messages: &[Value], tools: Option<&Value>, model: Option<String>) -> BoxStream<'static, Attempt> {
        let model = model.unwrap_or_else(|| self.model.clone());
        let mut body = self.body(messages, tools, &model);
        let (http, url, session, creds) = (self.http.clone(), self.url.clone(), self.session.clone(), self.creds.clone());
        let label = self.label();

        Box::pin(async_stream::stream! {
            let mut auth = match creds.current().await {
                Ok(a) => a,
                Err(why) => { yield Attempt::Event(ChatEvent::Failed(why)); return; }
            };
            let send = |auth: &CodexAuth, body: &Value| {
                let (http, url, body) = (http.clone(), url.clone(), body.clone());
                let headers = codex::responses_headers(&auth.access_token, &auth.account_id, &session);
                async move {
                    send_once_more(|| {
                        headers.iter().fold(http.post(&url), |r, (k, v)| r.header(k.as_str(), v.as_str())).json(&body)
                    })
                    .await
                }
            };
            let (mut renewed, mut plain) = (false, false);
            let resp = loop {
                let resp = match send(&auth, &body).await {
                    Ok(r) => r,
                    Err(e) => { yield Attempt::Event(ChatEvent::Failed(format!("{label}: {}", transport_error(e)))); return; }
                };
                let st = resp.status();
                if st == reqwest::StatusCode::UNAUTHORIZED && !renewed {
                    renewed = true;
                    auth = match creds.renew(&auth).await {
                        Ok(a) => a,
                        Err(why) => { yield Attempt::Event(ChatEvent::Failed(why)); return; }
                    };
                    continue;
                }
                if st == reqwest::StatusCode::BAD_REQUEST && !plain {
                    let t = resp.text().await.unwrap_or_default();
                    let lower = t.to_ascii_lowercase();
                    // Thinking is a nicety, the reply is not: a model or plan
                    // that takes no reasoning settings is asked plainly.
                    if ["reasoning", "effort", "summary"].iter().any(|w| lower.contains(w)) {
                        tracing::warn!("harness: codex 400 on reasoning — retrying without it");
                        plain = true;
                        if let Some(o) = body.as_object_mut() { o.remove("reasoning"); }
                        continue;
                    }
                    yield Attempt::Event(ChatEvent::Failed(format!("{label} {st}: {}", refusal(&t, &auth.access_token))));
                    return;
                }
                break resp;
            };
            if !resp.status().is_success() {
                let st = resp.status();
                let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_string);
                let t = resp.text().await.unwrap_or_default();
                let said = refusal(&t, &auth.access_token);
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
                // The plan's usage window is spent (`usage_limit_reached`,
                // with `resets_at`): said as quota, so the reply says until when.
                if let Some((kind, resets_at)) = quota_signal(st.as_u16(), retry_after.as_deref(), &t, now) {
                    yield Attempt::Event(ChatEvent::QuotaExhausted { kind, resets_at, detail: format!("{label} {st}: {said}") });
                    return;
                }
                yield Attempt::Event(ChatEvent::Failed(format!("{label} {st}: {said}")));
                return;
            }

            let mut bytes = resp.bytes_stream().map_err(transport_error);
            let mut parser = SseParser::default();
            let mut carry: Vec<u8> = Vec::new();
            let (mut content, mut calls, mut usage) = (String::new(), Vec::<ToolCall>::new(), None::<TokenUsage>);
            let mut completed = false;
            loop {
                let chunk = match StreamExt::next(&mut bytes).await {
                    None => break,
                    // The reply was whole: at most a trailing frame is lost.
                    Some(Err(_)) if completed => break,
                    Some(Err(e)) => { yield Attempt::Broke(format!("{label} stream: {e}")); return; }
                    Some(Ok(b)) => b,
                };
                carry.extend_from_slice(&chunk);
                // Whole lines only: a UTF-8 character split across two reads
                // must never reach the parser halved.
                let Some(end) = carry.iter().rposition(|&b| b == b'\n') else { continue };
                let lines: Vec<u8> = carry.drain(..=end).collect();
                for ev in parser.feed(&String::from_utf8_lossy(&lines)) {
                    match ev {
                        Event::Text(t) => {
                            content.push_str(&t);
                            yield Attempt::Event(ChatEvent::Text(t));
                        }
                        Event::Thinking(t) => yield Attempt::Event(ChatEvent::Thinking(t)),
                        Event::Call { call_id, name, arguments } => calls.push(ToolCall { id: call_id, name, arguments, ..Default::default() }),
                        // The model's own answer that it couldn't finish —
                        // not a broken stream, so not asked again.
                        Event::Failed(why) => { yield Attempt::Event(ChatEvent::Failed(format!("{label}: {why}"))); return; }
                        Event::Completed(u) => { usage = u; completed = true; }
                    }
                }
            }
            if !completed {
                yield Attempt::Broke(format!("{label} stream ended before the response was complete"));
                return;
            }
            let mut stats = parser.stats;
            stats.model = stats.model.or(Some(model));
            yield Attempt::Event(ChatEvent::Done(TurnMsg {
                content,
                calls,
                tokens: usage.map(|u| u.output_tokens).unwrap_or(0),
                input_tokens: usage.map(|u| u.input_tokens).unwrap_or(0),
                stats,
                ..Default::default()
            }));
        })
    }
}

/// What a refusal says, for the person: the endpoint's own sentence
/// (`error.message`, or `detail`) when the body is its JSON, else the start
/// of the body — and never the token, whatever came back.
fn refusal(body: &str, token: &str) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let e = &v["error"];
            let message = e["message"].as_str().or_else(|| v["detail"].as_str())?.to_string();
            Some(match e["type"].as_str() {
                Some(t) => format!("{t}: {message}"),
                None => message,
            })
        })
        .unwrap_or_else(|| body.chars().take(400).collect());
    if token.len() >= 8 { said.replace(token, "[token]") } else { said }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── the catalog ──

    /// What the picker lists, in its order, each with its efforts; hidden
    /// models and repeats left out.
    #[test]
    fn the_catalog_lists_what_codex_s_picker_does() {
        let body = json!({ "models": [
            { "slug": "gpt-5.6-sol", "display_name": "GPT-5.6 Sol", "visibility": "list", "priority": 5,
              "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "high" }] },
            { "slug": "gpt-6.2-sol", "display_name": "GPT-6.2 Sol", "description": "Newest", "visibility": "list", "priority": 1,
              "supported_reasoning_levels": [{ "effort": "low", "description": "x" }, { "effort": "max" }, { "effort": "ultra" }] },
            { "slug": "codex-auto-review", "visibility": "hide", "priority": 0 },
            { "slug": "gpt-6.2-sol", "visibility": "list", "priority": 9 },
            { "slug": "gpt-5.6-pro", "visibility": "list" }
        ]});
        let got = parse_catalog(&body);
        assert_eq!(got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-6.2-sol", "gpt-5.6-sol", "gpt-5.6-pro"]);
        assert_eq!(got[0].efforts, ["low", "max", "ultra"]);
        assert_eq!((got[0].label.as_str(), got[0].description.as_deref()), ("GPT-6.2 Sol", Some("Newest")));
        assert_eq!(got[2].label, "gpt-5.6-pro", "no name: its id");
        assert!(got[2].efforts.is_empty());
    }

    /// The read wears Codex's identity and the login, and a refusal is said,
    /// never the token.
    #[tokio::test]
    async fn the_catalog_is_read_with_the_login() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/codex/models", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for status in ["200 OK", "401 Unauthorized"] {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_string());
                let body = json!({ "models": [{ "slug": "gpt-6.2-sol", "visibility": "list" }] }).to_string();
                let reply = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        let auth = CodexAuth { access_token: "tok-fake".into(), account_id: "acct-fake".into() };
        let got = list_models_at(&url, &auth).await.unwrap();
        assert_eq!(got.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-6.2-sol"]);
        let req = seen.lock().unwrap()[0].to_lowercase();
        assert!(req.contains("authorization: bearer tok-fake") && req.contains("chatgpt-account-id: acct-fake"), "{req}");
        assert!(req.contains(&format!("originator: {}", codex::ORIGINATOR)), "{req}");
        let err = list_models_at(&url, &auth).await.unwrap_err();
        assert!(err.contains("401") && !err.contains("tok-fake"), "{err}");
    }

    // ── the request ──

    /// THE TRAP. A multimodal `content` array used to be read with
    /// `as_str().unwrap_or("")`, which yielded `""` and then dropped the whole
    /// user item — so attaching a photo would have deleted the user's QUESTION
    /// from the request, not merely failed to send the picture.
    #[test]
    fn a_multimodal_message_keeps_its_text_and_carries_the_image() {
        let messages = vec![json!({ "role": "user", "content": [
            { "type": "text", "text": "who is this?" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
        ]})];
        let body = build_request_from_chat("gpt-5.6-sol", &messages, None);
        let input = body["input"].as_array().expect("input");

        assert_eq!(input.len(), 1, "the user's message must survive");
        let parts = input[0]["content"].as_array().expect("parts");
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[0]["text"], "who is this?");
        // Responses takes `image_url` as a BARE STRING (verified against the
        // official client via `codex debug prompt-input -i`), not
        // chat-completions' nested object.
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["image_url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[1]["detail"], "high");
    }

    /// No vendor accepts an assistant image part, and this endpoint has no
    /// such item at all — so an image on a non-user turn is dropped while its
    /// words survive.
    #[test]
    fn an_assistant_turn_keeps_words_and_drops_pixels() {
        let messages = vec![json!({ "role": "assistant", "content": [
            { "type": "text", "text": "here you go" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
        ]})];
        let body = build_request_from_chat("gpt-5.6-sol", &messages, None);
        let parts = body["input"][0]["content"].as_array().expect("parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "output_text");
        assert_eq!(parts[0]["text"], "here you go");
    }

    /// A system prompt that arrives in array form must still reach
    /// `instructions` — the endpoint REQUIRES it non-empty and refuses
    /// `role:"system"` inside `input`, so flattening beats `unwrap_or("")`.
    #[test]
    fn an_array_system_prompt_still_becomes_instructions() {
        let messages = vec![
            json!({ "role": "system", "content": [{ "type": "text", "text": "persona" }] }),
            json!({ "role": "user", "content": "hi" }),
        ];
        let body = build_request_from_chat("gpt-5.6-sol", &messages, None);
        assert_eq!(body["instructions"], "persona");
        for item in body["input"].as_array().unwrap() {
            assert_ne!(item["role"], "system", "the endpoint refuses role:system in input");
        }
    }

    /// The market clamp must keep holding once pictures can ride. Measuring the
    /// request with `body.to_string().len()` silently stopped bounding
    /// anything: a 1 MB photo is ~1.4M characters of base64, i.e. a ~4.1M-token
    /// ceiling for input the vendor prices in the low thousands.
    #[test]
    fn an_inlined_image_does_not_blow_the_billing_clamp() {
        let big = format!("data:image/png;base64,{}", "A".repeat(1_400_000));
        let messages = vec![json!({ "role": "user", "content": [
            { "type": "text", "text": "who is this?" },
            { "type": "image_url", "image_url": { "url": big } },
        ]})];
        let body = build_request_from_chat("gpt-5.6-sol", &messages, None);

        let raw = body.to_string().len();
        let billable = billable_chars(&body);
        assert!(raw > 1_000_000, "the request really is that big on the wire: {raw}");
        assert!(billable < 10_000, "an image must count as a flat allowance, not its base64 length: {billable}");
        // …and the text still counts as text.
        assert!(billable > "who is this?".len());
    }

    /// A tool that handed back a picture (`Read` on a photo) keeps it on the
    /// way out — before, `function_call_output` was the text parts alone, so
    /// the model was told "<image …>" and shown nothing — and the picture
    /// counts as the flat allowance, not its base64.
    #[test]
    fn a_picture_a_tool_handed_back_reaches_the_model() {
        let big = format!("data:image/png;base64,{}", "A".repeat(1_400_000));
        let messages = vec![
            json!({ "role": "user", "content": "look at it" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "Read", "arguments": "{}" } }] }),
            json!({ "role": "tool", "tool_call_id": "c1", "content": [
                { "type": "text", "text": "<image name=\"a.png\">" },
                { "type": "image_url", "image_url": { "url": big } },
                { "type": "text", "text": "</image>" },
            ]}),
            json!({ "role": "tool", "tool_call_id": "c2", "content": "plain words" }),
        ];
        let body = build_request_from_chat("gpt-5.6-sol", &messages, None);
        let items = body["input"].as_array().unwrap();
        let output = |id: &str| items.iter().find(|i| i["type"] == "function_call_output" && i["call_id"] == id).unwrap()["output"].clone();
        let out = output("c1");
        assert_eq!(out[1]["type"], "input_image");
        assert_eq!(out[1]["image_url"], json!(big));
        assert_eq!(out[0], json!({ "type": "input_text", "text": "<image name=\"a.png\">" }));
        assert_eq!(output("c2"), "plain words", "text stays a string");
        assert!(billable_chars(&body) < 10_000, "{}", billable_chars(&body));
    }

    /// The kernel's chat transcript → the endpoint's items: system rides as
    /// instructions, assistant tool_calls become `function_call` items, tool
    /// replies become `function_call_output`, and the chat tools array is
    /// flattened to the Responses form.
    #[test]
    fn chat_transcript_translates_to_responses_items() {
        let messages = vec![
            json!({ "role": "system", "content": "persona here" }),
            json!({ "role": "user", "content": "draw a cat" }),
            json!({ "role": "assistant", "content": "on it", "tool_calls": [
                { "id": "c1", "type": "function",
                  "function": { "name": "imagegen", "arguments": "{\"prompt\":\"cat\"}" } }
            ]}),
            json!({ "role": "tool", "tool_call_id": "c1", "content": "{\"ok\":true}" }),
        ];
        let tools = json!([{ "type": "function", "function": {
            "name": "imagegen", "description": "d", "parameters": { "type": "object" }
        }}]);
        let body = build_request_from_chat("gpt-5.6", &messages, Some(&tools));

        assert_eq!(body["instructions"], "persona here");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "c1");
        assert_eq!(input[2]["name"], "imagegen");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[3]["output"], "{\"ok\":true}");
        // Responses wants the FLATTENED tool form — name at top level, no
        // nested `function` object.
        assert_eq!(body["tools"][0]["name"], "imagegen");
        assert!(body["tools"][0].get("function").is_none());
    }

    // ── the stream ──

    /// Function calls land WHOLE in `response.output_item.done` frames on
    /// this endpoint — the parser hands them out as complete calls.
    #[test]
    fn function_call_items_come_out_of_the_sse() {
        let mut p = SseParser::default();
        let frame = "data: {\"type\":\"response.output_item.done\",\"item\":{\
                     \"type\":\"function_call\",\"call_id\":\"c9\",\"name\":\"imagegen\",\
                     \"arguments\":\"{\\\"prompt\\\":\\\"a fox\\\"}\"}}\n";
        let evs = p.feed(frame);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            Event::Call { call_id, name, arguments } => {
                assert_eq!(call_id, "c9");
                assert_eq!(name, "imagegen");
                let v: Value = serde_json::from_str(arguments).unwrap();
                assert_eq!(v["prompt"], "a fox");
            }
            _ => panic!("expected a Call event"),
        }
        // Non-function items (a message item closing) stay invisible.
        let evs = p.feed("data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n");
        assert!(evs.is_empty());
    }

    #[test]
    fn deltas_and_completion_parse_from_sse_lines() {
        let mut p = SseParser::default();
        let evs = p.feed(
            "event: message\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\
             data: {\"type\":\"response.completed\",\"response\":{}}\n",
        );
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        // Completed, but with nothing to bill on: a backend that reports no
        // usage must read as "didn't say", never as a free turn.
        assert!(matches!(evs.last(), Some(Event::Completed(None))));
    }

    #[test]
    fn the_thinking_summary_comes_out_as_thinking() {
        let mut p = SseParser::default();
        let evs = p.feed("data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"weighing it\"}\n");
        assert!(matches!(&evs[..], [Event::Thinking(t)] if t == "weighing it"));
    }

    #[test]
    fn completed_preserves_display_metadata_without_changing_the_meter() {
        let mut p = SseParser::default();
        p.feed("data: {\"type\":\"response.completed\",\"response\":{\"model\":\"fixture\",\"reasoning\":{\"effort\":\"high\"},\"usage\":{\"input_tokens\":1200,\"output_tokens\":340,\"input_tokens_details\":{\"cached_tokens\":1000}}}}\n");
        assert_eq!(p.stats.total_tokens, Some(1540));
        assert_eq!(p.stats.cache_read_tokens, Some(1000));
        assert_eq!(p.stats.model.as_deref(), Some("fixture"));
        assert_eq!(p.stats.effort.as_deref(), Some("high"));
        assert_eq!(p.stats.context_used_tokens, Some(1200));
    }

    /// The meter. `response.completed` carries the numbers a resale turn is
    /// billed on, and they are parsed on the server, out of bytes a supplying
    /// device merely forwarded.
    #[test]
    fn completed_carries_the_priced_usage() {
        let mut p = SseParser::default();
        let evs = p.feed(
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":\
             {\"input_tokens\":1200,\"output_tokens\":340,\"total_tokens\":1540}}}\n",
        );
        let Some(Event::Completed(Some(u))) = evs.last() else {
            panic!("expected a completed frame with usage, got {} events", evs.len());
        };
        assert_eq!((u.input_tokens, u.output_tokens), (1200, 340));

        // The chat-completions spelling, which the same account type has
        // been seen serving.
        let mut p = SseParser::default();
        let evs = p.feed(
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":\
             {\"prompt_tokens\":7,\"completion_tokens\":9}}}\n",
        );
        let Some(Event::Completed(Some(u))) = evs.last() else { panic!("no usage") };
        assert_eq!((u.input_tokens, u.output_tokens), (7, 9));

        // Half a usage object is not a bill.
        let mut p = SseParser::default();
        let evs = p.feed("data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5}}}\n");
        assert!(matches!(evs.last(), Some(Event::Completed(None))));
    }

    /// An SSE event split mid-line across two relay chunks must reassemble,
    /// not vanish — the exact bug the api's own SSE reader once had.
    #[test]
    fn a_line_split_across_chunks_reassembles() {
        let mut p = SseParser::default();
        assert!(p.feed("data: {\"type\":\"response.output_te").is_empty());
        let evs = p.feed("xt.delta\",\"delta\":\"ok\"}\n");
        assert!(matches!(&evs[..], [Event::Text(t)] if t == "ok"));
    }

    #[test]
    fn failures_surface_the_backends_own_words() {
        let mut p = SseParser::default();
        let evs = p.feed("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"usage limit reached\"}}}\n");
        assert!(matches!(&evs[..], [Event::Failed(m)] if m == "usage limit reached"));

        let evs = p.feed("data: {\"type\":\"error\",\"message\":\"server_is_overloaded\"}\n");
        assert!(matches!(&evs[..], [Event::Failed(m)] if m == "server_is_overloaded"));
    }

    #[test]
    fn done_markers_and_noise_are_ignored() {
        let mut p = SseParser::default();
        assert!(p.feed(": keepalive\n\ndata: [DONE]\n\ndata: not json\nevent: x\n").is_empty());
    }

    // ── the runtime, against a stand-in endpoint ──

    /// A login that hands out `tokens` in turn: the first from `current`, the
    /// next from each `renew`.
    struct Logins {
        tokens: Mutex<Vec<&'static str>>,
        renewed: Mutex<u32>,
    }

    impl Logins {
        fn new(tokens: &[&'static str]) -> Arc<Self> {
            Arc::new(Self { tokens: Mutex::new(tokens.iter().rev().copied().collect()), renewed: Mutex::new(0) })
        }
        fn next(&self) -> Result<CodexAuth, String> {
            let t = self.tokens.lock().unwrap().pop().ok_or("no login left")?;
            Ok(CodexAuth { access_token: t.into(), account_id: "acct-fake".into() })
        }
    }

    #[async_trait]
    impl CodexCredentials for Logins {
        async fn current(&self) -> Result<CodexAuth, String> {
            self.next()
        }
        async fn renew(&self, _refused: &CodexAuth) -> Result<CodexAuth, String> {
            *self.renewed.lock().unwrap() += 1;
            self.next()
        }
    }

    /// A stand-in endpoint: answers each connection with the next reply and
    /// keeps every request (head + body).
    async fn upstream(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/backend-api/codex/responses", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut req = Vec::new();
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                kept.lock().unwrap().push(String::from_utf8_lossy(&req).to_string());
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (url, seen)
    }

    fn sse(lines: &[&str]) -> String {
        let mut out = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".to_string();
        for l in lines {
            out.push_str("data: ");
            out.push_str(l);
            out.push_str("\n\n");
        }
        out
    }

    fn status(code: &str, body: &str) -> String {
        format!("HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
    }

    const STEP: &[&str] = &[
        r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
        r#"{"type":"response.output_text.delta","delta":"reading it"}"#,
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"Read","arguments":"{\"path\":\"a\"}"}}"#,
        r#"{"type":"response.completed","response":{"model":"gpt-5.6-sol","usage":{"input_tokens":50,"output_tokens":7}}}"#,
    ];

    async fn run(rt: CodexResponsesRuntime) -> Vec<ChatEvent> {
        let tools = json!([{ "type": "function", "function": { "name": "Read", "parameters": { "type": "object" } } }]);
        let messages = vec![json!({ "role": "system", "content": "persona" }), json!({ "role": "user", "content": "hi" })];
        rt.step(messages, Some(tools), true, None).collect().await
    }

    #[tokio::test]
    async fn a_step_end_to_end_wears_the_codex_identity_and_streams_the_reply() {
        let (url, seen) = upstream(vec![sse(STEP)]).await;
        let rt = CodexResponsesRuntime::new(Logins::new(&["at-fake-1"]), "gpt-5.6-sol".into(), "sess-1".into())
            .with_url(url)
            .with_reasoning_effort(Some("medium".into()));
        let events = run(rt).await;
        assert!(events.iter().any(|e| matches!(e, ChatEvent::Thinking(t) if t == "thinking")));
        let Some(ChatEvent::Done(m)) = events.last() else { panic!("no step") };
        assert_eq!(m.content, "reading it");
        assert_eq!((m.calls[0].id.as_str(), m.calls[0].name.as_str()), ("c1", "Read"));
        assert_eq!((m.input_tokens, m.tokens), (50, 7));
        let seen = seen.lock().unwrap();
        let req = seen[0].to_ascii_lowercase();
        assert!(req.starts_with("post /backend-api/codex/responses "), "{req}");
        assert!(req.contains("\r\nauthorization: bearer at-fake-1\r\n"), "{req}");
        assert!(req.contains("\r\nchatgpt-account-id: acct-fake\r\n"), "{req}");
        assert!(req.contains(&format!("\r\noriginator: {}\r\n", codex::ORIGINATOR)), "{req}");
        assert!(req.contains("\r\nsession_id: sess-1\r\n"), "{req}");
        let body: Value = serde_json::from_str(seen[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["instructions"], "persona");
        assert_eq!(body["store"], false);
        assert_eq!(body["prompt_cache_key"], "sess-1");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["tools"][0]["name"], "Read");
    }

    /// A refused login is renewed once, and the step asked again on the new
    /// one; the token never shows in what the person reads.
    #[tokio::test]
    async fn a_401_renews_the_login_once_and_asks_again() {
        let (url, seen) = upstream(vec![status("401 Unauthorized", r#"{"detail":"expired"}"#), sse(STEP)]).await;
        let logins = Logins::new(&["at-fake-old", "at-fake-new"]);
        let rt = CodexResponsesRuntime::new(logins.clone(), "gpt-5.6-sol".into(), "s".into()).with_url(url);
        let events = run(rt).await;
        assert!(matches!(events.last(), Some(ChatEvent::Done(_))));
        assert_eq!(*logins.renewed.lock().unwrap(), 1);
        assert!(seen.lock().unwrap()[1].to_ascii_lowercase().contains("bearer at-fake-new"));

        // Refused twice: the second refusal is the answer, not a third ask.
        let (url, _) = upstream(vec![status("401 Unauthorized", "{}"), status("401 Unauthorized", r#"{"detail":"no at-fake-b2 here"}"#)]).await;
        let rt = CodexResponsesRuntime::new(Logins::new(&["at-fake-b1", "at-fake-b2"]), "gpt-5.6-sol".into(), "s".into()).with_url(url);
        let events = run(rt).await;
        let Some(ChatEvent::Failed(why)) = events.last() else { panic!("expected a failure") };
        assert!(why.contains("401") && !why.contains("at-fake-b2"), "{why}");
    }

    /// The plan's window is spent: quota, with when it lifts — not a failure.
    #[tokio::test]
    async fn a_spent_plan_reads_as_quota_with_its_reset() {
        let body = r#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","resets_at":1790438821}}"#;
        let (url, _) = upstream(vec![status("429 Too Many Requests", body)]).await;
        let rt = CodexResponsesRuntime::new(Logins::new(&["at-fake"]), "gpt-5.6-sol".into(), "s".into()).with_url(url);
        let events = run(rt).await;
        assert!(matches!(events.last(), Some(ChatEvent::QuotaExhausted { resets_at: Some(1790438821), .. })));
    }

    /// Thinking settings the model won't take are dropped, not the reply.
    #[tokio::test]
    async fn a_400_on_reasoning_asks_again_without_it() {
        let (url, seen) = upstream(vec![status("400 Bad Request", r#"{"detail":"Unsupported parameter: reasoning.summary"}"#), sse(STEP)]).await;
        let rt = CodexResponsesRuntime::new(Logins::new(&["at-fake"]), "gpt-5.6-sol".into(), "s".into()).with_url(url);
        let events = run(rt).await;
        assert!(matches!(events.last(), Some(ChatEvent::Done(_))));
        let second: Value = serde_json::from_str(seen.lock().unwrap()[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(second.get("reasoning").is_none());
    }
}
