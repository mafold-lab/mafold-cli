//! Runtime #2: Anthropic's own Messages API (`POST /v1/messages`), for a Cloud
//! agent that runs on its owner's Anthropic-format key — Anthropic itself or
//! any endpoint that speaks its wire.
//!
//! The kernel only speaks OpenAI chat: its history, its tools and the step it
//! reads back. Through the wallet that is fine — mafold-router translates on
//! its side (`mafold-router/src/anthropic.rs`). On the owner's own key there
//! is no router in between, so the same translation happens here, ported from
//! the router's (it is a bin crate, so it can't be a dependency): the request
//! going up, the SSE coming down, and Claude's thinking blocks carried as the
//! router carries them (`reasoning_details`, `format: anthropic-claude-v1`) so
//! the kernel hands them back exactly the way it already does.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use mafold_transcript::RunStats;
use serde_json::{json, Value};

use crate::runtime::{
    claude_family, effort_for, quota_signal, retrying, send_once_more, transport_error, Attempt, ChatEvent,
    ModelRuntime, ToolCall, TurnMsg, PULSE_EVERY,
};

/// The API version every request names. Anthropic requires the header and
/// has kept `2023-06-01` as the one current version; newer features
/// (adaptive thinking, effort) ride on the body, not on this.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic requires `max_tokens` and has no default. The router's number:
/// every current Claude model takes 64K, and thinking counts against it — at
/// 8192 a thinking model could spend all of it thinking and answer nothing.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// What a 400 on the output cap falls back to: an older model, or a proxy in
/// front of one, whose ceiling is lower. Anthropic has no "provider default"
/// to fall back to the way an OpenAI-format endpoint does — the field is
/// required — so it is a smaller number, not none.
const SMALL_MAX_TOKENS: u32 = 8_192;

/// `reasoning_details` entries that are Claude's thinking, signed by it. The
/// router's name, OpenRouter's too — so a block reads the same whichever of
/// the two produced it, and the kernel never learns a second word.
const REASONING_FORMAT: &str = "anthropic-claude-v1";

/// Runtime #2 — see the module docs. `Clone` for [`retrying`], like runtime #1.
#[derive(Clone)]
pub struct AnthropicRuntime {
    api_key: String,
    http: reqwest::Client,
    model: String,
    /// `…/v1/messages`, worked out once from what the owner typed.
    url: String,
    max_tokens: Option<u32>,
    /// OpenAI's word (`low` … `max`, `none`, [`crate::EFFORT_BY_MODEL`]),
    /// turned into adaptive thinking + effort per request by
    /// [`apply_reasoning`] — for the Claude models that take it, nothing for
    /// any other model id behind an Anthropic-format proxy.
    reasoning_effort: Option<String>,
}

impl AnthropicRuntime {
    /// `base_url` as the owner typed it: `https://api.anthropic.com`, a
    /// proxy's root, or either with `/v1` on the end.
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            // No client timeout, for runtime #1's reason: a long generation
            // is not a hang.
            http: reqwest::Client::new(),
            model,
            url: messages_url(&base_url),
            max_tokens: None,
            reasoning_effort: None,
        }
    }

    /// Send through this client — mafold-api's, which won't connect inside
    /// its own network when the address is one an owner typed.
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        self
    }

    pub fn with_max_tokens(mut self, n: Option<u32>) -> Self {
        self.max_tokens = n.filter(|n| *n > 0);
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Where the Messages API is, from a base URL. A trailing `/v1` is a common
/// way to write it (that's how OpenAI-format base URLs are written), so it is
/// taken as already there rather than doubled.
fn messages_url(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

#[async_trait]
impl ModelRuntime for AnthropicRuntime {
    fn label(&self) -> &'static str {
        "anthropic"
    }

    fn configured_model(&self) -> Option<&str> {
        Some(&self.model)
    }

    fn turn_model(&self, model_override: Option<&str>) -> Option<String> {
        Some(model_override.unwrap_or(&self.model).to_string())
    }

    fn step(
        &self,
        messages: Vec<Value>,
        tools: Option<Value>,
        _want_usage: bool,
        model: Option<String>,
    ) -> BoxStream<'static, ChatEvent> {
        // Usage always comes: Anthropic puts it on `message_start` and
        // `message_delta` whether asked or not.
        let runtime = self.clone();
        retrying(move || runtime.attempt(&messages, tools.as_ref(), model.clone()))
    }
}

impl AnthropicRuntime {
    /// One POST and the read of its stream; when it BROKE is runtime #1's
    /// rule, with Anthropic's words for it: an `error` event mid-stream, a
    /// read that failed, or a stream that ended with no `message_stop` and
    /// no `stop_reason`.
    fn attempt(&self, messages: &[Value], tools: Option<&Value>, model: Option<String>) -> BoxStream<'static, Attempt> {
        let http = self.http.clone();
        let url = self.url.clone();
        let key = self.api_key.clone();
        let model = model.unwrap_or_else(|| self.model.clone());
        let effort = effort_for(self.reasoning_effort.as_deref(), &model);
        let mut body = request_body(&model, messages, tools, self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS));
        apply_reasoning(&mut body, &model, effort.as_deref());
        let label = self.label();

        Box::pin(async_stream::stream! {
            let send = |body: &Value| {
                let (http, url, key, body) = (http.clone(), url.clone(), key.clone(), body.clone());
                async move {
                    send_once_more(|| {
                        http.post(&url)
                            .header("x-api-key", &key)
                            .header("anthropic-version", ANTHROPIC_VERSION)
                            .json(&body)
                    })
                    .await
                }
            };
            let mut resp = match send(&body).await {
                Ok(r) => r,
                Err(e) => { yield Attempt::Event(ChatEvent::Failed(format!("{label}: {}", transport_error(e)))); return; }
            };
            // The optional asks, dropped one at a time on a 400 that names
            // them — thinking harder and a bigger cap are niceties, the reply
            // is not. A 400 about anything else is the answer.
            for _ in 0..2 {
                if resp.status() != reqwest::StatusCode::BAD_REQUEST {
                    break;
                }
                let st = resp.status();
                let t = resp.text().await.unwrap_or_default();
                let lower = t.to_ascii_lowercase();
                let dropped = if body.get("thinking").is_some() && ["thinking", "effort", "output_config"].iter().any(|w| lower.contains(w)) {
                    tracing::warn!("harness: anthropic 400 on thinking — retrying without it");
                    if let Some(o) = body.as_object_mut() { o.remove("thinking"); o.remove("output_config"); }
                    true
                } else if lower.contains("max_tokens") && body["max_tokens"].as_u64().is_some_and(|n| n > SMALL_MAX_TOKENS as u64) {
                    tracing::warn!("harness: anthropic 400 on max_tokens — retrying at {SMALL_MAX_TOKENS}");
                    body["max_tokens"] = json!(SMALL_MAX_TOKENS);
                    true
                } else {
                    false
                };
                if !dropped {
                    yield Attempt::Event(ChatEvent::Failed(format!("{label} {st}: {}", refusal(&t, &key))));
                    return;
                }
                resp = match send(&body).await {
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
                let said = refusal(&t, &key);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                if let Some((kind, resets_at)) = quota_signal(st.as_u16(), retry_after.as_deref(), &t, now) {
                    yield Attempt::Event(ChatEvent::QuotaExhausted { kind, resets_at, detail: format!("{label} {st}: {said}") });
                    return;
                }
                yield Attempt::Event(ChatEvent::Failed(format!("{label} {st}: {said}")));
                return;
            }

            let mut bytes = resp.bytes_stream().map_err(transport_error);
            let mut read = StreamRead::new(&model, label);
            let mut carry: Vec<u8> = Vec::new();
            let mut last_pulse = Instant::now();
            'read: loop {
                let chunk = match StreamExt::next(&mut bytes).await {
                    None => break,
                    // Already said why it stopped: at most the last usage
                    // frame is lost, and asking again would swap out the
                    // answer the person just read.
                    Some(Err(e)) if read.finished => {
                        tracing::warn!("harness: {label} stream: {e} — after the reply was whole, keeping it");
                        break;
                    }
                    Some(Err(e)) => { yield Attempt::Broke(format!("{label} stream: {e}")); return; }
                    Some(Ok(b)) => b,
                };
                carry.extend_from_slice(&chunk);
                while let Some(pos) = carry.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = carry.drain(..=pos).collect();
                    let shown = read.feed(String::from_utf8_lossy(&line).trim());
                    if read.broke.is_some() {
                        break 'read;
                    }
                    for ev in shown.events {
                        yield Attempt::Event(ev);
                    }
                    if shown.progressed && last_pulse.elapsed() >= PULSE_EVERY {
                        last_pulse = Instant::now();
                        yield Attempt::Event(ChatEvent::Pulse);
                    }
                }
            }
            if read.broke.is_none() && !carry.is_empty() {
                let shown = read.feed(String::from_utf8_lossy(&carry).trim());
                if read.broke.is_none() {
                    for ev in shown.events {
                        yield Attempt::Event(ev);
                    }
                }
            }
            match read.finish() {
                Ok(m) => yield Attempt::Event(ChatEvent::Done(m)),
                Err(why) => yield Attempt::Broke(why),
            }
        })
    }
}

/// What a refusal says, for the person: Anthropic's own sentence
/// (`error.message`) when the body is its JSON, else the start of the body —
/// and never the key, whatever a proxy chose to echo back.
fn refusal(body: &str, key: &str) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            let e = v.get("error")?;
            let message = e.get("message").and_then(Value::as_str)?;
            Some(match e.get("type").and_then(Value::as_str) {
                Some(t) => format!("{t}: {message}"),
                None => message.to_string(),
            })
        })
        .unwrap_or_else(|| body.chars().take(400).collect());
    if key.len() >= 8 { said.replace(key, "[key]") } else { said }
}

// ───────────────────────────── going up ─────────────────────────────

/// The kernel's OpenAI-shaped step → an Anthropic Messages request, streaming,
/// with the router's two cache breakpoints. Effort is [`apply_reasoning`]'s.
fn request_body(model: &str, messages: &[Value], tools: Option<&Value>, max_tokens: u32) -> Value {
    let mut body = to_messages(model, messages, tools, max_tokens);
    if let Some(text) = body.get("system").and_then(Value::as_str).map(str::to_string) {
        body["system"] = json!([{ "type": "text", "text": text }]);
    }
    mark_cache_breakpoints(&mut body);
    body
}

/// The router's translation (`anthropic::to_messages`), plus one thing it
/// leaves to its own upstream: consecutive messages of one role are merged.
/// A tool loop that ran two tools sends two `tool` messages, and Anthropic
/// wants every result for one assistant step in the single user message
/// right after it — the API merges them itself, a proxy in front of it may
/// not.
///
/// The asymmetries it covers: `system` is a message there and a top-level
/// field here; tools are flat here with `input_schema`; calls and results are
/// `tool_use` / `tool_result` blocks; images are a different block.
fn to_messages(model: &str, messages: &[Value], tools: Option<&Value>, max_tokens: u32) -> Value {
    let mut system = Vec::new();
    let mut msgs: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        let (role, content) = match role {
            "system" | "developer" => {
                if let Some(t) = content.as_str() {
                    system.push(t.to_string());
                } else if let Some(blocks) = content.as_array() {
                    system.extend(blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string)));
                }
                continue;
            }
            "assistant" => {
                // Its own thinking first, byte for byte — Anthropic wants
                // thinking ahead of text and tool_use, and the signature
                // covers every byte.
                let mut blocks = thinking_blocks(m);
                match to_content(content, false) {
                    Value::String(t) if !t.is_empty() => blocks.push(json!({ "type": "text", "text": t })),
                    Value::Array(parts) => blocks.extend(parts),
                    _ => {}
                }
                for call in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    let function = call.get("function").unwrap_or(call);
                    let input = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": function.get("name").and_then(Value::as_str).unwrap_or(""),
                        "input": input,
                    }));
                }
                ("assistant", Value::Array(blocks))
            }
            "tool" => {
                let content = match to_content(content, true) {
                    Value::Null => json!(""),
                    c => c,
                };
                let result = json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": content,
                });
                ("user", json!([result]))
            }
            // Anthropic knows user and assistant; anything else reads as user.
            _ => ("user", to_content(content, true)),
        };
        let blocks = as_blocks(content);
        match msgs.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(prev) = last["content"].as_array_mut() {
                    prev.extend(blocks);
                }
            }
            _ => msgs.push(json!({ "role": role, "content": blocks })),
        }
    }
    // An empty block list is a 400 ("all messages must have non-empty
    // content"); an empty string is what the router sends for nothing said.
    for m in &mut msgs {
        if m["content"].as_array().is_some_and(Vec::is_empty) {
            m["content"] = json!("");
        }
    }
    if msgs.is_empty() {
        msgs.push(json!({ "role": "user", "content": "" }));
    }

    let mut body = json!({ "model": model, "messages": msgs, "max_tokens": max_tokens, "stream": true });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    let mapped: Vec<Value> = tools
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = t.get("function").unwrap_or(t);
            Some(json!({
                "name": f.get("name").and_then(Value::as_str)?,
                "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                "input_schema": f.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            }))
        })
        .collect();
    if !mapped.is_empty() {
        body["tools"] = json!(mapped);
    }
    body
}

/// A message's content as a block list, so two of one role can be joined. An
/// empty text block is a 400 of its own ("text content blocks must be
/// non-empty"), so none is made.
fn as_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::String(t) if t.is_empty() => Vec::new(),
        Value::String(t) => vec![json!({ "type": "text", "text": t })],
        Value::Array(parts) => parts
            .into_iter()
            .filter(|p| !(p["type"] == "text" && p["text"].as_str().is_some_and(str::is_empty)))
            .collect(),
        _ => Vec::new(),
    }
}

/// `data:image/png;base64,…` → a base64 source; an http(s) link → a url
/// source. Anything else is `None`, said in words by the caller rather than
/// sent as a block Anthropic refuses.
fn to_image_block(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let media_type = meta.strip_suffix(";base64")?;
        if media_type.is_empty() || data.is_empty() {
            return None;
        }
        return Some(json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({ "type": "image", "source": { "type": "url", "url": url } }));
    }
    None
}

/// OpenAI content parts → Anthropic blocks. Passed through verbatim, an
/// `image_url` part is a deterministic 400 on every request that replays it —
/// the router's 09-02 lesson, where one picture locked a chat for good.
/// `images = false` for assistant messages: Anthropic takes images only from
/// the user side, so a picture the agent sent becomes a line of text.
fn to_content(content: Value, images: bool) -> Value {
    let Some(parts) = content.as_array() else {
        return content;
    };
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        match p.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    out.push(json!({ "type": "text", "text": t }));
                }
            }
            Some("image_url") => {
                // `{"url": …}` or a bare string — read the shape, not the sender.
                let url = p.get("image_url").and_then(|u| u.get("url").and_then(Value::as_str).or_else(|| u.as_str()));
                let Some(url) = url else { continue };
                if !images {
                    out.push(json!({ "type": "text", "text": "[picture left out: this message can't carry one]" }));
                    continue;
                }
                match to_image_block(url) {
                    Some(block) => out.push(block),
                    None => out.push(json!({ "type": "text", "text": "[picture left out: its format can't be sent]" })),
                }
            }
            // Already Anthropic's words (image, tool_result, document…).
            _ => out.push(p.clone()),
        }
    }
    Value::Array(out)
}

/// The `reasoning_details` this runtime handed out ([`StreamRead`]), back as
/// thinking blocks. Only Claude's own (`format`): a signature is valid only
/// for the model that made it, and anything else would be a 400.
fn thinking_blocks(m: &Value) -> Vec<Value> {
    let Some(items) = m.get("reasoning_details").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|d| d.get("format").and_then(Value::as_str) == Some(REASONING_FORMAT))
        .filter_map(|d| match d.get("type").and_then(Value::as_str)? {
            "reasoning.text" => Some(json!({
                "type": "thinking",
                "thinking": d.get("text").and_then(Value::as_str).unwrap_or(""),
                "signature": d.get("signature").and_then(Value::as_str)?,
            })),
            "reasoning.encrypted" => Some(json!({ "type": "redacted_thinking", "data": d.get("data").and_then(Value::as_str)? })),
            _ => None,
        })
        .collect()
}

/// The router's two cache breakpoints: the last system block (tools above it
/// ride along) and the last block of the last message. A tool loop only
/// appends, so from its second step on everything before is a cache read —
/// on the owner's own bill, where it matters as much as on the wallet. Never
/// on an empty text block (400) or a thinking block.
fn mark_cache_breakpoints(body: &mut Value) {
    let mark = json!({ "type": "ephemeral" });
    if let Some(block) = body
        .get_mut("system")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
        .filter(|b| b.get("text").and_then(Value::as_str).is_some_and(|t| !t.is_empty()))
    {
        block["cache_control"] = mark.clone();
    }
    let Some(last) = body.get_mut("messages").and_then(Value::as_array_mut).and_then(|msgs| msgs.last_mut()) else {
        return;
    };
    if let Some(text) = last.get("content").and_then(Value::as_str).map(str::to_string) {
        if !text.is_empty() {
            last["content"] = json!([{ "type": "text", "text": text, "cache_control": mark }]);
        }
        return;
    }
    if let Some(block) = last.get_mut("content").and_then(Value::as_array_mut).and_then(|blocks| blocks.last_mut()) {
        let cacheable = match block.get("type").and_then(Value::as_str) {
            Some("text") => block.get("text").and_then(Value::as_str).is_some_and(|t| !t.is_empty()),
            Some("tool_result" | "image" | "tool_use" | "document") => true,
            _ => false,
        };
        if cacheable {
            block["cache_control"] = mark;
        }
    }
}

/// OpenAI's `reasoning_effort` → Anthropic's adaptive thinking + effort, the
/// router's reading (`anthropic::apply_reasoning`): by family and version,
/// no list. Opus/Sonnet from 4.6 and Fable/Mythos take it; 4.6 has no xhigh;
/// Haiku, older models and any id that isn't Claude's get nothing — an
/// Anthropic-format proxy in front of another vendor's model must not be
/// asked for a field it has never heard of.
fn apply_reasoning(body: &mut Value, model: &str, effort: Option<&str>) {
    let Some(effort) = effort.map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty() && e != "none") else {
        return;
    };
    let Some((family, version)) = claude_family(model) else { return };
    let adaptive = match family {
        "fable" | "mythos" => true,
        "opus" | "sonnet" => version >= (4, 6),
        _ => false,
    };
    if !adaptive {
        return;
    }
    let effort = match effort.as_str() {
        "minimal" | "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" if version == (4, 6) && family != "fable" && family != "mythos" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => return,
    };
    body["thinking"] = json!({ "type": "adaptive" });
    body["output_config"] = json!({ "effort": effort });
}

// ───────────────────────────── coming down ─────────────────────────────

/// One SSE frame, in the terms the read cares about. `event:` lines are left
/// alone: the same name is the `type` inside each `data:` line.
#[derive(Debug, PartialEq)]
enum Frame {
    Text(String),
    /// (block index, id, name)
    ToolStart(usize, String, String),
    ToolArgs(usize, String),
    ThinkingStart(usize),
    Thinking(usize, String),
    Signature(usize, String),
    Redacted(usize, String),
    BlockStop(usize),
    /// From `message_start` (input) or a `message_delta` (output) — merged.
    Usage(Value),
    /// The stop reason, and the usage on the same `message_delta`.
    Stop(String, Option<Value>),
    Done,
    /// The stream said it broke (`overloaded_error` partway, say). Its own
    /// kind: ignored, it would read as a whole reply that happens to be short.
    Error(String),
}

fn parse_frame(line: &str) -> Option<Frame> {
    let raw = line.strip_prefix("data:")?.trim();
    if raw.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(raw).ok()?;
    let idx = || v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match v.get("type").and_then(Value::as_str)? {
        "content_block_start" => {
            let b = v.get("content_block")?;
            match b.get("type").and_then(Value::as_str)? {
                "tool_use" => Some(Frame::ToolStart(idx(), s(b, "id").unwrap_or_default(), s(b, "name").unwrap_or_default())),
                "thinking" => Some(Frame::ThinkingStart(idx())),
                "redacted_thinking" => Some(Frame::Redacted(idx(), s(b, "data")?)),
                _ => None,
            }
        }
        "content_block_delta" => {
            let d = v.get("delta")?;
            match d.get("type").and_then(Value::as_str)? {
                "text_delta" => Some(Frame::Text(s(d, "text")?)),
                "input_json_delta" => Some(Frame::ToolArgs(idx(), s(d, "partial_json")?)),
                "thinking_delta" => Some(Frame::Thinking(idx(), s(d, "thinking")?)),
                "signature_delta" => Some(Frame::Signature(idx(), s(d, "signature")?)),
                _ => None,
            }
        }
        "content_block_stop" => Some(Frame::BlockStop(idx())),
        "message_start" => v.get("message").and_then(|m| m.get("usage")).map(|u| Frame::Usage(u.clone())),
        "message_delta" => {
            let usage = v.get("usage").cloned();
            match v.get("delta").and_then(|d| s(d, "stop_reason")) {
                Some(reason) => Some(Frame::Stop(reason, usage)),
                None => usage.map(Frame::Usage),
            }
        }
        "message_stop" => Some(Frame::Done),
        "error" => Some(Frame::Error(match v.get("error") {
            Some(e) => format!(
                "{}: {}",
                e.get("type").and_then(Value::as_str).unwrap_or("error"),
                e.get("message").and_then(Value::as_str).unwrap_or("no message")
            ),
            None => "error: no message".into(),
        })),
        _ => None,
    }
}

/// What one fed line showed.
#[derive(Default)]
struct Shown {
    events: Vec<ChatEvent>,
    progressed: bool,
}

/// One step's stream, read into what the kernel gets: the events as they
/// come, then the [`TurnMsg`] — shaped like runtime #1's, so the kernel can't
/// tell which runtime it came from.
struct StreamRead {
    out: TurnMsg,
    label: &'static str,
    /// Merged across `message_start` and `message_delta` — each carries half.
    usage: serde_json::Map<String, Value>,
    calls: Vec<ToolCall>,
    /// Block index → its place in `calls`.
    call_at: HashMap<usize, usize>,
    /// Thinking blocks being written: (text, signature, redacted data). A
    /// block goes back only whole — the signature comes last.
    thinking: BTreeMap<usize, (String, String, Option<String>)>,
    /// A thinking block has shown text: the next block's first piece starts
    /// a new paragraph.
    thought_shown: bool,
    new_thought: bool,
    finished: bool,
    broke: Option<String>,
}

impl StreamRead {
    fn new(model: &str, label: &'static str) -> Self {
        let mut out = TurnMsg::default();
        out.stats.model = Some(model.to_string());
        out.stats.provider = Some(label.to_string());
        Self {
            out,
            label,
            usage: Default::default(),
            calls: Vec::new(),
            call_at: HashMap::new(),
            thinking: BTreeMap::new(),
            thought_shown: false,
            new_thought: false,
            finished: false,
            broke: None,
        }
    }

    fn merge_usage(&mut self, u: &Value) {
        for (k, v) in u.as_object().into_iter().flatten() {
            // Max, not overwrite: a later frame may say 0 for what an
            // earlier one counted.
            match (v.as_u64(), self.usage.get(k).and_then(Value::as_u64)) {
                (Some(n), Some(had)) if had >= n => {}
                (Some(_), _) => {
                    self.usage.insert(k.clone(), v.clone());
                }
                _ => {}
            }
        }
    }

    fn feed(&mut self, line: &str) -> Shown {
        let mut shown = Shown::default();
        let Some(frame) = parse_frame(line) else { return shown };
        match frame {
            Frame::Text(t) => {
                if !t.is_empty() {
                    self.out.content.push_str(&t);
                    shown.events.push(ChatEvent::Text(t));
                }
            }
            Frame::ToolStart(i, id, name) => {
                self.calls.push(ToolCall { id, name, ..Default::default() });
                self.call_at.insert(i, self.calls.len() - 1);
                shown.progressed = true;
            }
            Frame::ToolArgs(i, piece) => {
                if let Some(&at) = self.call_at.get(&i) {
                    self.calls[at].arguments.push_str(&piece);
                    shown.progressed = true;
                }
            }
            Frame::ThinkingStart(i) => {
                self.thinking.insert(i, Default::default());
                self.new_thought = self.thought_shown;
            }
            Frame::Thinking(i, t) => {
                if let Some(b) = self.thinking.get_mut(&i) {
                    b.0.push_str(&t);
                }
                if !t.is_empty() {
                    let t = if std::mem::take(&mut self.new_thought) { format!("\n\n{t}") } else { t };
                    self.thought_shown = true;
                    shown.events.push(ChatEvent::Thinking(t));
                    shown.progressed = true;
                }
            }
            Frame::Signature(i, sig) => {
                if let Some(b) = self.thinking.get_mut(&i) {
                    b.1.push_str(&sig);
                }
            }
            Frame::Redacted(i, data) => {
                self.thinking.insert(i, (String::new(), String::new(), Some(data)));
            }
            Frame::BlockStop(i) => {
                if let Some((text, signature, redacted)) = self.thinking.remove(&i) {
                    let detail = match redacted {
                        Some(data) => Some(json!({ "type": "reasoning.encrypted", "data": data, "format": REASONING_FORMAT, "index": i })),
                        None if !signature.is_empty() => Some(json!({
                            "type": "reasoning.text", "text": text, "signature": signature,
                            "format": REASONING_FORMAT, "index": i,
                        })),
                        // No signature: it can't go back, so it doesn't.
                        None => None,
                    };
                    self.out.reasoning.extend(detail);
                }
            }
            Frame::Usage(u) => self.merge_usage(&u),
            Frame::Stop(reason, usage) => {
                if let Some(u) = usage {
                    self.merge_usage(&u);
                }
                self.finished = true;
                self.out.truncated = reason == "max_tokens";
                self.out.stop_reason = Some(reason);
            }
            Frame::Done => self.finished = true,
            Frame::Error(why) => self.broke = Some(format!("{} stream: {why}", self.label)),
        }
        shown
    }

    /// The step, whole — or why its stream broke.
    fn finish(mut self) -> Result<TurnMsg, String> {
        if let Some(why) = self.broke {
            return Err(why);
        }
        if !self.finished {
            return Err(format!("{} stream ended before the reply did (no stop_reason, no message_stop)", self.label));
        }
        if !self.usage.is_empty() {
            let stats = RunStats::anthropic(&Value::Object(self.usage));
            self.out.tokens = stats.output_tokens.unwrap_or(0);
            self.out.input_tokens = stats.input_tokens.unwrap_or(0);
            self.out.stats = RunStats {
                model: self.out.stats.model.take(),
                provider: self.out.stats.provider.take(),
                context_used_tokens: stats.input_tokens,
                context_basis: stats.input_tokens.map(|_| "last_request_input".into()),
                ..stats
            };
        }
        self.out.calls = self.calls.into_iter().filter(|c| !c.name.is_empty()).collect();
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_messages_url_is_found_from_any_way_of_writing_the_base() {
        for (base, url) in [
            ("https://api.anthropic.com", "https://api.anthropic.com/v1/messages"),
            ("https://api.anthropic.com/", "https://api.anthropic.com/v1/messages"),
            ("https://api.anthropic.com/v1", "https://api.anthropic.com/v1/messages"),
            ("https://api.anthropic.com/v1/", "https://api.anthropic.com/v1/messages"),
            ("https://x.example/anthropic", "https://x.example/anthropic/v1/messages"),
            (" https://x.example/anthropic/v1 ", "https://x.example/anthropic/v1/messages"),
        ] {
            assert_eq!(messages_url(base), url, "{base}");
        }
    }

    /// A two-tool step and its results, as the kernel sends the next request.
    fn tool_loop() -> Vec<Value> {
        vec![
            json!({ "role": "system", "content": "You are Amelia." }),
            json!({ "role": "system", "content": "Today is Monday." }),
            json!({ "role": "user", "content": [
                { "type": "text", "text": "what's in these?" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                { "type": "image_url", "image_url": { "url": "https://x.example/a.jpg" } },
            ]}),
            json!({
                "role": "assistant", "content": "Looking.",
                "reasoning_details": [
                    { "type": "reasoning.text", "text": "Two files.", "signature": "S1", "format": REASONING_FORMAT, "index": 0 },
                    { "type": "reasoning.text", "text": "someone else's", "signature": "X", "format": "google", "index": 1 },
                ],
                "tool_calls": [
                    { "id": "t1", "type": "function", "function": { "name": "Read", "arguments": "{\"path\":\"a\"}" } },
                    { "id": "t2", "type": "function", "function": { "name": "Read", "arguments": "{\"path\":" } },
                ],
            }),
            json!({ "role": "tool", "tool_call_id": "t1", "content": "A" }),
            json!({ "role": "tool", "tool_call_id": "t2", "content": "B" }),
        ]
    }

    #[test]
    fn a_tool_loop_goes_up_in_anthropics_shape() {
        let tools = json!([{ "type": "function", "function": { "name": "Read", "description": "read a file", "parameters": { "type": "object" } } }]);
        let body = request_body("claude-opus-5-5", &tool_loop(), Some(&tools), 64_000);
        assert_eq!(body["system"], json!([{ "type": "text", "text": "You are Amelia.\n\nToday is Monday.", "cache_control": { "type": "ephemeral" } }]));
        assert_eq!(body["tools"], json!([{ "name": "Read", "description": "read a file", "input_schema": { "type": "object" } }]));
        assert_eq!((body["max_tokens"].as_u64(), body["stream"].as_bool()), (Some(64_000), Some(true)));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "user, assistant, ONE user with both results: {msgs:?}");
        assert_eq!(msgs[0]["content"][1]["source"], json!({ "type": "base64", "media_type": "image/png", "data": "AAAA" }));
        assert_eq!(msgs[0]["content"][2]["source"], json!({ "type": "url", "url": "https://x.example/a.jpg" }));
        let a = &msgs[1]["content"];
        assert_eq!(a[0], json!({ "type": "thinking", "thinking": "Two files.", "signature": "S1" }), "only Claude's own block, first");
        assert_eq!(a[1], json!({ "type": "text", "text": "Looking." }));
        assert_eq!(a[2], json!({ "type": "tool_use", "id": "t1", "name": "Read", "input": { "path": "a" } }));
        assert_eq!(a[3]["input"], json!({}), "a call cut mid-JSON goes back as {{}}");
        assert_eq!(a.as_array().unwrap().len(), 4);
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!((&results[0]["tool_use_id"], &results[0]["content"]), (&json!("t1"), &json!("A")));
        assert_eq!(results[1]["tool_use_id"], "t2");
        assert_eq!(results[1]["cache_control"], json!({ "type": "ephemeral" }), "the last block is the breakpoint");
        assert!(results[0].get("cache_control").is_none());
    }

    #[test]
    fn nothing_empty_is_sent_and_an_agents_own_picture_becomes_words() {
        let body = request_body(
            "m",
            &[
                json!({ "role": "user", "content": "hi" }),
                json!({ "role": "assistant", "content": [{ "type": "image_url", "image_url": { "url": "https://x.example/me.png" } }] }),
                json!({ "role": "assistant", "content": "" }),
                json!({ "role": "user", "content": "" }),
            ],
            None,
            1000,
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"], json!([{ "type": "text", "text": "[picture left out: this message can't carry one]" }]));
        assert_eq!(msgs[2]["content"], json!(""), "an empty user message stays a bare empty string, no breakpoint on it");
        assert!(body.get("system").is_none() && body.get("tools").is_none());
    }

    #[test]
    fn effort_is_asked_only_of_claude_models_that_take_it() {
        let ask = |model: &str, effort: Option<&str>| {
            let mut body = json!({});
            apply_reasoning(&mut body, model, effort);
            body
        };
        assert_eq!(ask("claude-opus-5-5", Some("high")), json!({ "thinking": { "type": "adaptive" }, "output_config": { "effort": "high" } }));
        assert_eq!(ask("claude-sonnet-4-6", Some("xhigh"))["output_config"]["effort"], "high");
        for (model, effort) in [("claude-haiku-5-5", Some("high")), ("glm-5", Some("high")), ("claude-opus-5-5", Some("none")), ("claude-opus-5-5", None)] {
            assert_eq!(ask(model, effort), json!({}), "{model} {effort:?}");
        }
    }

    /// A step as Anthropic streams it: thinking (two blocks, one redacted),
    /// text, a tool call in pieces, usage split across start and delta.
    const STEP: &[&str] = &[
        "event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":12,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":1}}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Read it, "}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"then answer."}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"BLOB"}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"On it."}}"#,
        r#"data: {"type":"content_block_stop","index":2}"#,
        r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}"#,
        r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":3}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
        r#"data: {"type":"message_stop"}"#,
    ];

    fn read(lines: &[&str]) -> (Vec<String>, Result<TurnMsg, String>) {
        let mut r = StreamRead::new("claude-opus-5-5", "anthropic");
        let mut shown = Vec::new();
        for l in lines {
            for e in r.feed(l).events {
                match e {
                    ChatEvent::Text(t) => shown.push(format!("text:{t}")),
                    ChatEvent::Thinking(t) => shown.push(format!("thinking:{t}")),
                    _ => {}
                }
            }
        }
        (shown, r.finish())
    }

    #[test]
    fn a_streamed_step_reads_into_the_same_turn_runtime_one_makes() {
        let (shown, m) = read(STEP);
        assert_eq!(shown, ["thinking:Read it, ", "thinking:then answer.", "text:On it."]);
        let m = m.expect("whole");
        assert_eq!(m.content, "On it.");
        assert_eq!(m.calls.len(), 1);
        assert_eq!((m.calls[0].id.as_str(), m.calls[0].name.as_str(), m.calls[0].arguments.as_str()), ("toolu_1", "Read", r#"{"path":"a"}"#));
        assert_eq!(m.stop_reason.as_deref(), Some("tool_use"));
        assert!(!m.truncated);
        assert_eq!((m.input_tokens, m.tokens), (117, 40), "input counts the cache, like the router's prompt_tokens");
        assert_eq!((m.stats.cache_read_tokens, m.stats.cache_write_tokens), (Some(100), Some(5)));
        assert_eq!(m.stats.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(m.reasoning.len(), 2);
        // What the kernel hands back is what goes up as thinking again.
        let back = crate::kernel::assistant_value(&m);
        let up = request_body("claude-opus-5-5", &[json!({ "role": "user", "content": "x" }), back], None, 1000);
        let blocks = &up["messages"][1]["content"];
        assert_eq!(blocks[0], json!({ "type": "thinking", "thinking": "Read it, then answer.", "signature": "SIG" }));
        assert_eq!(blocks[1], json!({ "type": "redacted_thinking", "data": "BLOB" }));
        assert_eq!(blocks[3]["type"], "tool_use");
    }

    #[test]
    fn a_second_thinking_block_is_a_new_paragraph() {
        let (shown, _) = read(&[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"One."}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"Two."}}"#,
        ]);
        assert_eq!(shown, ["thinking:One.", "thinking:\n\nTwo."]);
    }

    #[test]
    fn an_error_midway_or_a_stream_that_just_stops_is_broken_not_done() {
        let (_, m) = read(&[STEP[1], STEP[10], r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#]);
        assert_eq!(m.err().as_deref(), Some("anthropic stream: overloaded_error: Overloaded"));
        let (_, m) = read(&[STEP[1], STEP[10]]);
        assert!(m.err().unwrap().contains("ended before the reply did"));
        let (_, m) = read(&[STEP[1], r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":9}}"#]);
        assert!(m.unwrap().truncated);
    }

    #[test]
    fn a_refusal_says_anthropics_sentence_and_never_the_key() {
        let key = "sk-ant-api03-secretsecret";
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(refusal(body, key), "authentication_error: invalid x-api-key");
        assert_eq!(refusal(&format!("bad key {key} rejected"), key), "bad key [key] rejected");
    }

    /// A stand-in Messages API: answers each connection with the next reply
    /// and keeps every request (head + body).
    async fn upstream(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let kept = seen.clone();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut req = Vec::new();
                let mut buf = vec![0u8; 64 * 1024];
                // Read until the whole body (content-length) is in.
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
        (base, seen)
    }

    fn sse(lines: &[&str]) -> String {
        let mut out = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".to_string();
        for l in lines {
            out.push_str(l);
            out.push_str("\n\n");
        }
        out
    }

    fn status(code: &str, body: &str) -> String {
        format!("HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
    }

    async fn run(rt: AnthropicRuntime) -> Vec<ChatEvent> {
        let tools = json!([{ "type": "function", "function": { "name": "Read", "parameters": { "type": "object" } } }]);
        rt.step(vec![json!({ "role": "user", "content": "hi" })], Some(tools), true, None).collect().await
    }

    #[tokio::test]
    async fn a_step_end_to_end_signs_with_the_key_and_streams_the_reply() {
        let (base, seen) = upstream(vec![sse(STEP)]).await;
        let rt = AnthropicRuntime::new("sk-ant-k".into(), format!("{base}/v1/"), "claude-opus-5-5".into())
            .with_reasoning_effort(Some(crate::EFFORT_BY_MODEL.into()));
        let events = run(rt).await;
        let Some(ChatEvent::Done(m)) = events.last() else { panic!("no step") };
        assert_eq!(m.calls[0].name, "Read");
        let seen = seen.lock().unwrap();
        let req = seen[0].to_ascii_lowercase();
        assert!(req.starts_with("post /v1/messages "), "{req}");
        assert!(req.contains("\r\nx-api-key: sk-ant-k\r\n") && req.contains("\r\nanthropic-version: 2023-06-01\r\n"), "{req}");
        assert!(!req.contains("authorization:"), "x-api-key only: {req}");
        let body: Value = serde_json::from_str(seen[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["output_config"]["effort"], "high", "auto → the model's own default");
        assert_eq!(body["tools"][0]["name"], "Read");
    }

    #[tokio::test]
    async fn a_400_on_thinking_is_asked_again_without_it() {
        let (base, seen) = upstream(vec![
            status("400 Bad Request", r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking: unknown field"}}"#),
            sse(STEP),
        ])
        .await;
        let rt = AnthropicRuntime::new("k".into(), base, "claude-opus-5-5".into()).with_reasoning_effort(Some("high".into()));
        let events = run(rt).await;
        assert!(matches!(events.last(), Some(ChatEvent::Done(_))));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].contains("\"thinking\"") && !seen[1].contains("\"thinking\""));
    }

    #[tokio::test]
    async fn a_bad_key_fails_in_anthropics_words_and_a_429_is_quota() {
        let (base, _) = upstream(vec![status("401 Unauthorized", r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#)]).await;
        let events = run(AnthropicRuntime::new("sk-ant-wrongwrong".into(), base, "claude-opus-5-5".into())).await;
        let Some(ChatEvent::Failed(t)) = events.last() else { panic!("not a failure") };
        assert_eq!(t, "anthropic 401 Unauthorized: authentication_error: invalid x-api-key");

        let (base, _) = upstream(vec![format!(
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
        )])
        .await;
        let events = run(AnthropicRuntime::new("k".into(), base, "claude-opus-5-5".into())).await;
        assert!(matches!(events.last(), Some(ChatEvent::QuotaExhausted { resets_at: Some(_), .. })));
    }

    #[tokio::test]
    async fn a_stream_that_broke_is_asked_again_from_the_top() {
        let broken = sse(&[STEP[1], STEP[10], r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#]);
        let (base, seen) = upstream(vec![broken, sse(STEP)]).await;
        let events = run(AnthropicRuntime::new("k".into(), base, "claude-opus-5-5".into())).await;
        assert!(events.iter().any(|e| matches!(e, ChatEvent::Restart)), "what it showed is taken back");
        assert!(matches!(events.last(), Some(ChatEvent::Done(_))));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
