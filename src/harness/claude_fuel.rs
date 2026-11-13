//! Mafold Harness on its owner's Claude Code plan (ops 11-13: "add a mafold
//! harness fuel option: claude code"; magpie's Claude bridge for the shape).
//!
//! The house kernel keeps its loop — its prompt, its tools, its sessions, what
//! someone says while it works — and each model step is made by the real
//! `claude` on this computer, signed in as its owner. Nothing here pretends to
//! be Claude Code: it is Claude Code, started like every other one the daemon
//! starts ([`super::claude`], which names itself `claude-mafold` to
//! Anthropic), in its scripted mode, with the harness's prompt as its system
//! prompt.
//!
//! One turn:
//! - The first step starts `claude -p` (stream-json both ways) with the
//!   kernel's system prompt, none of Claude Code's own tools (`--tools ""`),
//!   and the kernel's tools offered as an MCP server of ours: `claude` starts
//!   `mafold harness-bridge` ([`bridge_main`]), a pipe back to this process
//!   over a loopback socket ([`Bridge`]).
//! - Claude calling one of those tools ends the step with that call. The
//!   kernel runs it with its own plugins (the folder-bound computer), and the
//!   next step hands the result to the `claude` still waiting on it, which
//!   carries on in the same process.
//! - What the person said meanwhile rides in with that result; a turn that
//!   ended silent is asked again as a new message, the way the kernel asks.
//! - The conversation is Claude Code's own session, resumed next turn
//!   ([`ClaudeEngine::session`]). The kernel keeps its copy too: a session
//!   `claude` can't resume starts again from that.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::BoxStream;
use mafold_harness::{ChatEvent, ModelRuntime, ToolCall, TurnMsg};
use mafold_transcript::RunStats;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::LimitHit;

/// The MCP server's name in `--mcp-config`: Claude sees each kernel tool as
/// `mcp__mafold__<name>`, and only those may run.
const SERVER: &str = "mafold";

/// The hidden subcommand `claude` starts as that server ([`bridge_main`]),
/// and the two variables that tell it where this turn listens.
pub const BRIDGE_COMMAND: &str = "harness-bridge";
const ADDR_ENV: &str = "MAFOLD_BRIDGE_ADDR";
const TOKEN_ENV: &str = "MAFOLD_BRIDGE_TOKEN";

/// How long Claude Code waits on one of the kernel's tools, in ms. Its own
/// default would end a long build; a person's Stop ends the turn instead.
const TOOL_TIMEOUT_MS: &str = "86400000";

/// How much of a tool's output Claude Code passes on, in tokens. The kernel's
/// tools cut their own output; this only keeps Claude Code from cutting it a
/// second time, at its default 25,000.
const TOOL_OUTPUT_TOKENS: &str = "100000";

/// Claude Code's effort tiers (`claude --help`): anything else the agent
/// carries is another fuel's and stays off the command line.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The model names `claude --model` takes besides a full `claude-…` id.
const ALIASES: [&str; 6] = ["default", "opus", "sonnet", "haiku", "fable", "opusplan"];

/// How a turn's `claude` is started.
#[derive(Clone)]
pub struct Spawn {
    pub workdir: PathBuf,
    /// The turn's env: which Claude login (`CLAUDE_SECURESTORAGE_CONFIG_DIR`,
    /// `crate::accounts`), and whatever else the daemon set.
    pub env: Vec<(String, String)>,
    /// A model `claude` knows ([`claude_model`]); `None` = its own default.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The Claude Code session this conversation goes on in, if it has one.
    pub resume: Option<String>,
    /// Which of the kernel's messages are the kept conversation (after the
    /// system prompt and the turn's data, before this turn's words): told to
    /// a `claude` that starts without the session.
    pub history: Range<usize>,
    /// This program, which `claude` starts as the bridge.
    pub exe: PathBuf,
}

/// The turn's model, as `claude --model` takes it: a Claude id or one of its
/// aliases (`opus`, `sonnet[1m]`). Anything else — a model another fuel ran
/// the agent on — is left to Claude Code's default rather than refused.
pub fn claude_model(wanted: Option<&str>) -> Option<String> {
    let m = wanted.map(str::trim).filter(|m| !m.is_empty())?;
    let base = m.split('[').next().unwrap_or(m).to_ascii_lowercase();
    (base.starts_with("claude") || ALIASES.contains(&base.as_str())).then(|| m.to_string())
}

/// The program `claude` starts as the bridge: this one. Debug builds only:
/// `MAFOLD_DEV_BRIDGE_EXE` (the turn's, else the process's) names another —
/// a test binary has no `harness-bridge`, the debug `mafold` beside it has.
pub fn bridge_exe(env: &[(String, String)]) -> PathBuf {
    let dev = cfg!(debug_assertions)
        .then(|| {
            env.iter()
                .rev()
                .find(|(k, _)| k == "MAFOLD_DEV_BRIDGE_EXE")
                .map(|(_, v)| v.clone())
                .or_else(|| std::env::var("MAFOLD_DEV_BRIDGE_EXE").ok())
        })
        .flatten()
        .filter(|v| !v.trim().is_empty());
    dev.map(PathBuf::from).unwrap_or_else(|| crate::platform::current_exe().unwrap_or_else(|_| PathBuf::from("mafold")))
}

/// What a Claude Code above this process marks its children with. A daemon
/// started from one would hand them down, and a `claude` that believes it is
/// another's child keeps no session of its own to resume.
const PARENT_MARKS: [&str; 9] = [
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

/// The model runtime the kernel steps: one `claude` per turn, started by the
/// first step and kept for the rest.
pub struct ClaudeEngine {
    spawn: Spawn,
    state: Arc<tokio::sync::Mutex<Option<Proc>>>,
    session: Arc<Mutex<Option<String>>>,
    limit: Arc<Mutex<Option<LimitHit>>>,
}

impl ClaudeEngine {
    pub fn new(spawn: Spawn) -> Self {
        let session = Arc::new(Mutex::new(spawn.resume.clone()));
        Self { spawn, state: Default::default(), session, limit: Default::default() }
    }

    /// The Claude Code session the turn ran in — the one to resume next turn.
    pub fn session(&self) -> Option<String> {
        self.session.lock().unwrap().clone()
    }

    /// The usage window that refused the turn, when one did: what moves the
    /// turn to the owner's next Claude login (`accounts::failover`).
    pub fn limit(&self) -> Option<LimitHit> {
        self.limit.lock().unwrap().clone()
    }

    /// The turn is over: `claude` is told nothing more is coming and given a
    /// moment to end on its own, its session put away, then stopped — at once
    /// for a turn someone stopped, which may have left it waiting on a tool.
    pub async fn finish(&self, stopped: bool) {
        let Some(mut p) = self.state.lock().await.take() else { return };
        drop(p.stdin.take());
        let grace = if stopped { Duration::ZERO } else { Duration::from_secs(3) };
        if tokio::time::timeout(grace, p.child.wait()).await.is_err() {
            let _ = p.child.start_kill();
        }
    }
}

#[async_trait::async_trait]
impl ModelRuntime for ClaudeEngine {
    fn label(&self) -> &'static str {
        "Claude Code"
    }

    fn configured_model(&self) -> Option<&str> {
        self.spawn.model.as_deref()
    }

    /// Claude reads pictures: a tool's picture goes to it as one.
    fn turn_model(&self, _model_override: Option<&str>) -> Option<String> {
        self.spawn.model.clone()
    }

    fn step(&self, messages: Vec<Value>, tools: Option<Value>, _want_usage: bool, _model: Option<String>) -> BoxStream<'static, ChatEvent> {
        let (spawn, state, session, limit) = (self.spawn.clone(), self.state.clone(), self.session.clone(), self.limit.clone());
        Box::pin(async_stream::stream! {
            let mut guard = state.lock().await;
            match guard.as_mut() {
                None => match start(&spawn, &messages, tools.as_ref(), &session).await {
                    Ok(p) => *guard = Some(p),
                    Err(e) => {
                        yield ChatEvent::Failed(e);
                        return;
                    }
                },
                Some(p) => {
                    if let Err(e) = p.deliver(&messages).await {
                        yield ChatEvent::Failed(e);
                        return;
                    }
                }
            }
            let Some(p) = guard.as_mut() else { return };
            p.seen = messages.len();
            let mut step = Step::default();
            loop {
                // A call is taken only once the message that asked for it has
                // been read: the call comes over the bridge, the message over
                // stdout, and the call can win that race — taken first, it
                // ended the step before the words in front of it.
                if p.held.iter().any(|c| p.asked(c)) {
                    let mut calls = Vec::new();
                    while let Ok(more) = p.calls.try_recv() {
                        p.held.push_back(more);
                    }
                    for call in std::mem::take(&mut p.held) {
                        if p.asked(&call) {
                            calls.push(p.wait_on(call));
                        } else {
                            p.held.push_back(call);
                        }
                    }
                    yield ChatEvent::Done(step.done(calls));
                    return;
                }
                let next = match p.early.pop_front() {
                    Some(v) => Next::Line(Some(v)),
                    None => tokio::select! {
                        biased;
                        line = p.out.recv() => Next::Line(line),
                        Some(call) = p.calls.recv() => Next::Call(call),
                    },
                };
                match next {
                    Next::Line(Some(v)) => {
                        for out in step.read(&v, &mut p.uses, &session) {
                            match out {
                                Out::Event(ev) => yield ev,
                                Out::End(ev) => {
                                    if let ChatEvent::QuotaExhausted { kind, resets_at, .. } = &ev {
                                        *limit.lock().unwrap() = Some(LimitHit { kind: kind.clone(), resets_at: *resets_at });
                                    }
                                    yield ev;
                                    return;
                                }
                            }
                        }
                    }
                    Next::Line(None) => {
                        yield ChatEvent::Failed(p.exit_reason().await);
                        return;
                    }
                    Next::Call(call) => p.held.push_back(call),
                }
            }
        })
    }
}

enum Next {
    Line(Option<Value>),
    Call(McpCall),
}

/// One running `claude` and what it waits on.
struct Proc {
    child: tokio::process::Child,
    /// Taken (and so closed) when the turn is over: `claude -p` ends once its
    /// input does.
    stdin: Option<tokio::process::ChildStdin>,
    /// What `claude` writes, a JSON value a line; closed once it's gone.
    out: mpsc::UnboundedReceiver<Value>,
    /// Lines read before the first step's reading began (the start's check).
    early: VecDeque<Value>,
    /// The kernel tools `claude` calls, as it calls them.
    calls: mpsc::UnboundedReceiver<McpCall>,
    /// Calls that came before the message asking for them was read.
    held: VecDeque<McpCall>,
    /// The tool uses Claude asked for and hasn't called yet.
    uses: Vec<Use>,
    /// Calls `claude` waits on: the tool use each answers, and where.
    waiting: Vec<(String, oneshot::Sender<Value>)>,
    /// How many of the kernel's messages it has been told.
    seen: usize,
    stderr: Arc<Mutex<String>>,
    _bridge: Option<Bridge>,
    files: Vec<PathBuf>,
}

impl Drop for Proc {
    fn drop(&mut self) {
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// A tool use in Claude's message: its id, the kernel tool's name, the input.
struct Use {
    id: String,
    name: String,
    input: Value,
}

/// One `tools/call` from `claude`, held open until the kernel answers it.
struct McpCall {
    name: String,
    arguments: Value,
    reply: oneshot::Sender<Value>,
}

impl Proc {
    /// Has Claude's message asking for `call` been read?
    fn asked(&self, call: &McpCall) -> bool {
        self.uses.iter().any(|u| u.name == call.name)
    }

    /// `call` waits on the kernel now: the tool use it is (matched by name and
    /// input, `claude` calling them in order), as the kernel's call.
    fn wait_on(&mut self, call: McpCall) -> ToolCall {
        let at = self
            .uses
            .iter()
            .position(|u| u.name == call.name && u.input == call.arguments)
            .or_else(|| self.uses.iter().position(|u| u.name == call.name));
        let id = match at {
            Some(i) => self.uses.remove(i).id,
            None => format!("toolu_mafold_{}", uuid::Uuid::new_v4().simple()),
        };
        self.waiting.push((id.clone(), call.reply));
        ToolCall { id, name: call.name, arguments: call.arguments.to_string(), extra: Default::default() }
    }

    /// What the kernel added since the last step: results for the calls
    /// `claude` waits on — what the person said meanwhile riding along with
    /// the last of them — or, with nothing waiting, a new message.
    async fn deliver(&mut self, messages: &[Value]) -> Result<(), String> {
        let mut results: HashMap<&str, &Value> = HashMap::new();
        let mut said: Vec<&Value> = Vec::new();
        for m in messages.get(self.seen..).unwrap_or(&[]) {
            match m["role"].as_str() {
                Some("tool") => {
                    results.insert(m["tool_call_id"].as_str().unwrap_or(""), &m["content"]);
                }
                Some("user") => said.push(&m["content"]),
                // The kernel's copy of what the last step said.
                _ => {}
            }
        }
        if self.waiting.is_empty() {
            if said.is_empty() {
                return Ok(());
            }
            let content: Vec<Value> = said.iter().flat_map(|c| message_blocks(c)).collect();
            return self.send(content).await;
        }
        let last = self.waiting.len() - 1;
        for (i, (id, reply)) in self.waiting.drain(..).enumerate() {
            let mut content = match results.get(id.as_str()) {
                Some(c) => tool_blocks(c),
                None => vec![json!({ "type": "text", "text": "(not run: the turn moved on without it)" })],
            };
            if i == last {
                for c in &said {
                    content.extend(tool_blocks(c));
                }
            }
            let _ = reply.send(json!({ "content": content }));
        }
        Ok(())
    }

    /// One user message to `claude`, as stream-json.
    async fn send(&mut self, content: Vec<Value>) -> Result<(), String> {
        let line = json!({ "type": "user", "message": { "role": "user", "content": content } });
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        let Some(stdin) = self.stdin.as_mut() else { return Err("Claude Code: the turn is over".into()) };
        match stdin.write_all(&bytes).await {
            Ok(()) => stdin.flush().await.map_err(|e| format!("Claude Code: couldn't write to claude ({e})")),
            Err(_) => Err(self.exit_reason().await),
        }
    }

    /// Why `claude` is gone, in its own words when it left any.
    async fn exit_reason(&mut self) -> String {
        let code = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.ok().and_then(|s| s.ok()).and_then(|s| s.code());
        let said = self.stderr.lock().unwrap().trim().to_string();
        let code = code.map(|c| format!(" (exit {c})")).unwrap_or_default();
        if said.is_empty() {
            format!("Claude Code: claude stopped before it answered{code}")
        } else {
            format!("Claude Code: claude stopped before it answered{code}: {said}")
        }
    }
}

/// What one step has read so far.
#[derive(Default)]
struct Step {
    text: String,
    stats: RunStats,
    /// A request of its own was read (`message_start`). A step without one
    /// goes on with the last: Claude's message asked for several tools, and
    /// `claude` runs them one by one, each a step of the kernel's.
    requested: bool,
    stop_reason: Option<String>,
    /// The usage window `claude` said refused it (`rate_limit_event`).
    rejected: Option<(String, Option<i64>)>,
    /// An API error `claude` put in a message of its own.
    error: Option<String>,
}

enum Out {
    Event(ChatEvent),
    /// The step's last event.
    End(ChatEvent),
}

impl Step {
    /// Read one line of `claude`'s output.
    fn read(&mut self, v: &Value, uses: &mut Vec<Use>, session: &Mutex<Option<String>>) -> Vec<Out> {
        // A subagent's lines carry the tool use they belong to; this `claude`
        // has no tools that start one, so only the main thread is read.
        if !v["parent_tool_use_id"].is_null() {
            return Vec::new();
        }
        match v["type"].as_str().unwrap_or("") {
            "system" => {
                if let Some(id) = v["session_id"].as_str().filter(|_| v["subtype"] == "init") {
                    *session.lock().unwrap() = Some(id.to_string());
                }
                Vec::new()
            }
            "stream_event" => self.stream(&v["event"]),
            "assistant" => {
                let m = &v["message"];
                let synthetic = m["model"].as_str() == Some("<synthetic>");
                for block in m["content"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                    match block["type"].as_str() {
                        Some("tool_use") => {
                            let name = block["name"].as_str().unwrap_or("");
                            uses.push(Use {
                                id: block["id"].as_str().unwrap_or("").to_string(),
                                name: name.strip_prefix(&format!("mcp__{SERVER}__")).unwrap_or(name).to_string(),
                                input: block["input"].clone(),
                            });
                        }
                        Some("text") if synthetic => self.error = block["text"].as_str().map(str::to_string),
                        _ => {}
                    }
                }
                Vec::new()
            }
            "rate_limit_event" => {
                let info = &v["rate_limit_info"];
                if info["status"] == "rejected" {
                    self.rejected = Some((info["rateLimitType"].as_str().unwrap_or("usage").to_string(), info["resetsAt"].as_i64()));
                }
                Vec::new()
            }
            "result" => {
                if let Some(id) = v["session_id"].as_str() {
                    *session.lock().unwrap() = Some(id.to_string());
                }
                vec![Out::End(self.result(v))]
            }
            _ => Vec::new(),
        }
    }

    /// One of Anthropic's stream events, as `claude` passes it on.
    fn stream(&mut self, e: &Value) -> Vec<Out> {
        match e["type"].as_str().unwrap_or("") {
            "message_start" => {
                let m = &e["message"];
                self.stats = usage_stats(&m["usage"]);
                self.stats.model = m["model"].as_str().map(str::to_string);
                self.requested = true;
                self.stop_reason = None;
                Vec::new()
            }
            "content_block_delta" => {
                let d = &e["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = d["text"].as_str().unwrap_or("");
                        self.text.push_str(t);
                        vec![Out::Event(ChatEvent::Text(t.to_string()))]
                    }
                    "thinking_delta" => vec![Out::Event(ChatEvent::Thinking(d["thinking"].as_str().unwrap_or("").to_string()))],
                    "input_json_delta" => vec![Out::Event(ChatEvent::Pulse)],
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                if let Some(r) = e["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(r.to_string());
                }
                if let Some(n) = e["usage"]["output_tokens"].as_u64() {
                    self.stats.output_tokens = Some(n);
                    self.stats.total_tokens = self.stats.input_tokens.map(|i| i.saturating_add(n));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// `claude` ended its turn: the step's answer, or why there is none.
    fn result(&mut self, v: &Value) -> ChatEvent {
        let failed = v["is_error"].as_bool().unwrap_or(false) || v["subtype"].as_str().is_some_and(|s| s != "success");
        if !failed {
            return ChatEvent::Done(self.done(Vec::new()));
        }
        let said = v["result"]
            .as_str()
            .map(str::to_string)
            .or_else(|| v["errors"].as_array().map(|e| e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.error.clone())
            .unwrap_or_else(|| format!("claude ended the turn with {}", v["subtype"].as_str().unwrap_or("an error")));
        let limit = self
            .rejected
            .clone()
            .or_else(|| crate::accounts::usage_limit_reset(&said).map(|at| ("usage".to_string(), at)));
        match limit {
            Some((kind, resets_at)) => ChatEvent::QuotaExhausted { kind, resets_at, detail: said },
            None => ChatEvent::Failed(format!("Claude Code: {said}")),
        }
    }

    /// The step as the kernel takes it, with the calls `claude` waits on.
    fn done(&mut self, calls: Vec<ToolCall>) -> TurnMsg {
        let stop_reason = if calls.is_empty() { self.stop_reason.take() } else { Some("tool_use".into()) };
        // No request of its own: the card counts nothing for it, rather than
        // an unknown that blanks the reply's total (`RunStats::add_request`).
        if !self.requested {
            self.stats.model_requests = Some(0);
        }
        TurnMsg {
            tokens: self.stats.output_tokens.unwrap_or(0),
            input_tokens: self.stats.input_tokens.unwrap_or(0),
            stats: std::mem::take(&mut self.stats),
            content: std::mem::take(&mut self.text),
            calls,
            stop_reason,
            ..Default::default()
        }
    }
}

/// A request's usage, as the result card counts it.
fn usage_stats(u: &Value) -> RunStats {
    let mut s = RunStats::anthropic(u);
    s.context_used_tokens = s.input_tokens;
    s.context_basis = s.input_tokens.map(|_| "last_request_input".into());
    s
}

// ───────────────────────────── starting ─────────────────────────────

/// Start `claude` for the turn and give it its opening message: on the kept
/// session when there is one, else — or when `claude` can't find it — afresh,
/// told the conversation so far.
async fn start(spawn: &Spawn, messages: &[Value], tools: Option<&Value>, session: &Arc<Mutex<Option<String>>>) -> Result<Proc, String> {
    if let Some(id) = spawn.resume.as_deref() {
        match launch(spawn, messages, tools, Some(id)).await {
            Ok(p) => return Ok(p),
            Err(Launch::NoSession(why)) => {
                println!("🆕 Claude Code couldn't go on in session {id} ({why}) — starting one, told the conversation so far");
                *session.lock().unwrap() = None;
            }
            Err(Launch::Failed(e)) => return Err(e),
        }
    }
    launch(spawn, messages, tools, None).await.map_err(|e| match e {
        Launch::NoSession(e) | Launch::Failed(e) => e,
    })
}

enum Launch {
    /// `claude` doesn't have the session it was told to go on in.
    NoSession(String),
    Failed(String),
}

async fn launch(spawn: &Spawn, messages: &[Value], tools: Option<&Value>, resume: Option<&str>) -> Result<Proc, Launch> {
    let scratch = std::env::temp_dir();
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let mut files = Vec::new();
    let system_file = scratch.join(format!("mafold-claude-system-{tag}.txt"));
    let system = messages.first().filter(|m| m["role"] == "system").and_then(|m| m["content"].as_str()).unwrap_or("");
    write_private(&system_file, system).map_err(|e| Launch::Failed(format!("Claude Code: couldn't write the prompt file ({e})")))?;
    files.push(system_file.clone());

    let (calls_tx, calls_rx) = mpsc::unbounded_channel();
    let mcp_tools: Vec<Value> = tools.and_then(Value::as_array).map(|t| t.iter().filter_map(mcp_tool).collect()).unwrap_or_default();
    let (bridge, mcp_file) = if mcp_tools.is_empty() {
        (None, None)
    } else {
        let bridge = Bridge::open(mcp_tools, calls_tx).await.map_err(|e| Launch::Failed(format!("Claude Code: couldn't open the tool bridge ({e})")))?;
        let file = scratch.join(format!("mafold-claude-mcp-{tag}.json"));
        write_private(&file, &bridge.config(&spawn.exe).to_string())
            .map_err(|e| Launch::Failed(format!("Claude Code: couldn't write the tool config ({e})")))?;
        files.push(file.clone());
        (Some(bridge), Some(file))
    };

    let mut cmd = command(spawn, &system_file, mcp_file.as_deref(), resume);
    let mut child = cmd.spawn().map_err(|e| Launch::Failed(format!("Claude Code: couldn't start claude ({e}) — is Claude Code installed here?")))?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        return Err(Launch::Failed("Claude Code: claude started without its pipes".into()));
    };
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if out_tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let said = Arc::new(Mutex::new(String::new()));
    {
        let said = said.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut s = said.lock().unwrap();
                s.push_str(&line);
                s.push('\n');
                if s.len() > 4000 {
                    let cut = s.len() - 4000;
                    let cut = (cut..s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
                    s.drain(..cut);
                }
            }
        });
    }
    let mut p = Proc {
        child,
        stdin: Some(stdin),
        out: out_rx,
        early: VecDeque::new(),
        calls: calls_rx,
        held: VecDeque::new(),
        uses: Vec::new(),
        waiting: Vec::new(),
        seen: 0,
        stderr: said,
        _bridge: bridge,
        files,
    };
    p.send(opening(messages, &spawn.history, resume.is_some())).await.map_err(Launch::Failed)?;
    // A resumed session is checked before the turn reads on: a `claude` that
    // can't find it ends at once, and the turn starts afresh instead.
    if resume.is_some() {
        match p.out.recv().await {
            Some(v) => p.early.push_back(v),
            None => {
                let why = p.exit_reason().await;
                let lower = why.to_ascii_lowercase();
                return Err(if lower.contains("no conversation found") || lower.contains("session") {
                    Launch::NoSession(why)
                } else {
                    Launch::Failed(why)
                });
            }
        }
    }
    Ok(p)
}

/// Debug builds only: `MAFOLD_DEV_CLAUDE_BIN` in the turn's env stands in for
/// `claude` — the engine's own tests, with a `claude` that does what they need.
fn dev_claude(env: &[(String, String)]) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    env.iter().rev().find(|(k, _)| k == "MAFOLD_DEV_CLAUDE_BIN").map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The command for one turn's `claude`.
fn command(spawn: &Spawn, system_file: &Path, mcp_file: Option<&Path>, resume: Option<&str>) -> tokio::process::Command {
    let mut cmd = match dev_claude(&spawn.env) {
        Some(bin) => {
            let mut c = crate::platform::command(bin);
            c.env("CLAUDE_CODE_ENTRYPOINT", super::CLAUDE_ENTRYPOINT);
            c
        }
        None => super::claude(),
    };
    cmd.arg("-p")
        .args(["--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"])
        // The harness's prompt is the whole system prompt. A session is
        // resumed only under the prompt it began with (`mafold_harness`
        // keeps its fingerprint), so an edited persona or a new model line
        // starts a session that carries it.
        .arg("--system-prompt-file")
        .arg(system_file)
        // None of Claude Code's own tools, skills or settings: what it can do
        // is what the kernel offers, as on every other fuel.
        .args(["--tools", ""])
        .arg("--disable-slash-commands")
        .args(["--setting-sources", ""])
        .arg("--strict-mcp-config");
    if let Some(f) = mcp_file {
        cmd.arg("--mcp-config").arg(f).arg("--allowedTools").arg(format!("mcp__{SERVER}"));
    }
    if let Some(m) = &spawn.model {
        cmd.arg("--model").arg(m);
    }
    if let Some(e) = spawn.effort.as_deref().filter(|e| EFFORTS.contains(e)) {
        cmd.arg("--effort").arg(e);
    }
    if let Some(id) = resume {
        cmd.arg("--resume").arg(id);
    }
    for mark in PARENT_MARKS {
        cmd.env_remove(mark);
    }
    cmd.current_dir(&spawn.workdir)
        .envs(spawn.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        // The owner's Claude Code login is the fuel, never a key in the env.
        .env_remove("ANTHROPIC_API_KEY")
        .env("MCP_TOOL_TIMEOUT", TOOL_TIMEOUT_MS)
        .env("MAX_MCP_OUTPUT_TOKENS", TOOL_OUTPUT_TOKENS)
        // Every kernel tool stays in front of the model, never deferred.
        .env("ENABLE_TOOL_SEARCH", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// A file only its owner can read: the prompt and the bridge's token.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(text.as_bytes())
}

/// The turn's first message to `claude`: the turn's data, the conversation so
/// far when `claude` starts without it, and the turn's own words.
fn opening(messages: &[Value], history: &Range<usize>, resumed: bool) -> Vec<Value> {
    let users = |ms: &[Value]| -> Vec<Value> { ms.iter().filter(|m| m["role"] == "user").flat_map(|m| message_blocks(&m["content"])).collect() };
    let data_end = history.start.clamp(1, messages.len().max(1));
    let mut blocks = users(messages.get(1..data_end).unwrap_or(&[]));
    if !resumed {
        if let Some(t) = transcript(messages.get(history.clone()).unwrap_or(&[])) {
            blocks.push(json!({ "type": "text", "text": t }));
        }
    }
    blocks.extend(users(messages.get(history.end.min(messages.len())..).unwrap_or(&[])));
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "(no message)" }));
    }
    blocks
}

/// The kept conversation as text, for a `claude` that starts without it.
fn transcript(history: &[Value]) -> Option<String> {
    let mut out = String::new();
    for m in history {
        let text = |c: &Value| match c {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        };
        match m["role"].as_str() {
            Some("user") => out.push_str(&format!("<user>\n{}\n</user>\n", text(&m["content"]))),
            Some("assistant") => {
                let said = text(&m["content"]);
                if !said.trim().is_empty() {
                    out.push_str(&format!("<assistant>\n{said}\n</assistant>\n"));
                }
                for c in m["tool_calls"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                    out.push_str(&format!(
                        "<tool_call name=\"{}\">{}</tool_call>\n",
                        c["function"]["name"].as_str().unwrap_or(""),
                        c["function"]["arguments"].as_str().unwrap_or("")
                    ));
                }
            }
            Some("tool") => out.push_str(&format!("<tool_result>\n{}\n</tool_result>\n", text(&m["content"]))),
            _ => {}
        }
    }
    (!out.trim().is_empty()).then(|| {
        format!("This conversation so far, as Mafold kept it (you are the assistant in it):\n<conversation>\n{out}</conversation>")
    })
}

/// A kernel message's content as Anthropic content blocks: text as text, a
/// picture (`image_url` with a data URL) as an image.
fn message_blocks(content: &Value) -> Vec<Value> {
    parts(content)
        .into_iter()
        .map(|p| match p {
            Part::Text(t) => json!({ "type": "text", "text": t }),
            Part::Image { mime, data } => json!({ "type": "image", "source": { "type": "base64", "media_type": mime, "data": data } }),
        })
        .collect()
}

/// The same, as an MCP tool result's content.
fn tool_blocks(content: &Value) -> Vec<Value> {
    parts(content)
        .into_iter()
        .map(|p| match p {
            Part::Text(t) => json!({ "type": "text", "text": t }),
            Part::Image { mime, data } => json!({ "type": "image", "data": data, "mimeType": mime }),
        })
        .collect()
}

enum Part {
    Text(String),
    Image { mime: String, data: String },
}

fn parts(content: &Value) -> Vec<Part> {
    match content {
        Value::String(s) => vec![Part::Text(s.clone())],
        Value::Array(items) => items
            .iter()
            .filter_map(|p| match p["type"].as_str() {
                Some("text") => p["text"].as_str().map(|t| Part::Text(t.to_string())),
                Some("image_url") => {
                    let url = p["image_url"]["url"].as_str().or_else(|| p["image_url"].as_str())?;
                    let (head, data) = url.strip_prefix("data:")?.split_once(',')?;
                    let mime = head.strip_suffix(";base64")?;
                    Some(Part::Image { mime: mime.to_string(), data: data.to_string() })
                }
                _ => Some(Part::Text(p.to_string())),
            })
            .collect(),
        Value::Null => Vec::new(),
        other => vec![Part::Text(other.to_string())],
    }
}

/// A kernel tool (`{"type":"function","function":{…}}`) as an MCP tool.
fn mcp_tool(t: &Value) -> Option<Value> {
    let f = &t["function"];
    let name = f["name"].as_str()?;
    let schema = match &f["parameters"] {
        Value::Object(o) if !o.is_empty() => Value::Object(o.clone()),
        _ => json!({ "type": "object", "properties": {} }),
    };
    Some(json!({ "name": name, "description": f["description"].as_str().unwrap_or(""), "inputSchema": schema }))
}

// ───────────────────────────── the bridge ─────────────────────────────

/// The turn's end of the MCP server `claude` talks to: a loopback socket the
/// bridge process ([`bridge_main`]) connects to, behind a token, serving the
/// kernel's tools and handing each call over as an [`McpCall`].
struct Bridge {
    addr: std::net::SocketAddr,
    token: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Bridge {
    async fn open(tools: Vec<Value>, calls: mpsc::UnboundedSender<McpCall>) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (tools, key) = (Arc::new(tools), token.clone());
        let task = tokio::spawn(async move {
            let mut served = tokio::task::JoinSet::new();
            while let Ok((sock, _)) = listener.accept().await {
                served.spawn(serve(sock, key.clone(), tools.clone(), calls.clone()));
            }
        });
        Ok(Self { addr, token, task })
    }

    /// `--mcp-config` for it: this program as a stdio server, told where.
    fn config(&self, exe: &Path) -> Value {
        json!({ "mcpServers": { SERVER: {
            "type": "stdio",
            "command": exe.to_string_lossy(),
            "args": [BRIDGE_COMMAND],
            "env": { ADDR_ENV: self.addr.to_string(), TOKEN_ENV: self.token },
        } } })
    }
}

/// One bridge connection: the token first, then MCP's JSON-RPC, a message a
/// line each way.
async fn serve(sock: tokio::net::TcpStream, token: String, tools: Arc<Vec<Value>>, calls: mpsc::UnboundedSender<McpCall>) {
    let _ = sock.set_nodelay(true);
    let (read, mut write) = sock.into_split();
    let mut lines = BufReader::new(read).lines();
    if lines.next_line().await.ok().flatten().as_deref().map(str::trim) != Some(token.as_str()) {
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            let mut bytes = v.to_string().into_bytes();
            bytes.push(b'\n');
            if write.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let mut pending = tokio::task::JoinSet::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(req) = serde_json::from_str::<Value>(line.trim()) else { continue };
        // A notification (`notifications/initialized`, a cancellation) wants
        // no answer.
        let Some(id) = req.get("id").filter(|v| !v.is_null()).cloned() else { continue };
        let ok = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
        match req["method"].as_str().unwrap_or("") {
            "initialize" => {
                let version = req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18");
                let _ = tx.send(ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mafold-harness", "version": env!("CARGO_PKG_VERSION") },
                })));
            }
            "tools/list" => {
                let _ = tx.send(ok(json!({ "tools": *tools })));
            }
            "ping" => {
                let _ = tx.send(ok(json!({})));
            }
            "tools/call" => {
                let (reply, answer) = oneshot::channel();
                let call = McpCall {
                    name: req["params"]["name"].as_str().unwrap_or("").to_string(),
                    arguments: match &req["params"]["arguments"] {
                        Value::Null => json!({}),
                        a => a.clone(),
                    },
                    reply,
                };
                if calls.send(call).is_err() {
                    let _ = tx.send(ok(json!({ "content": [{ "type": "text", "text": "(not run: the turn is over)" }], "isError": true })));
                    continue;
                }
                let tx = tx.clone();
                pending.spawn(async move {
                    let result = answer.await.unwrap_or_else(|_| json!({ "content": [{ "type": "text", "text": "(not run: the turn ended first)" }], "isError": true }));
                    let _ = tx.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                });
            }
            other => {
                let _ = tx.send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("method not found: {other}") } }));
            }
        }
    }
    pending.abort_all();
    drop(tx);
    let _ = writer.await;
}

/// `mafold harness-bridge`: the stdio MCP server `claude` starts for a Mafold
/// Harness turn — a pipe to that turn's socket ([`Bridge`]), which does all
/// the talking. Ends when either side does.
pub fn bridge_main() -> anyhow::Result<()> {
    let addr = std::env::var(ADDR_ENV)?;
    let token = std::env::var(TOKEN_ENV)?;
    let sock = std::net::TcpStream::connect(&addr)?;
    let _ = sock.set_nodelay(true);
    let mut to_turn = sock.try_clone()?;
    writeln!(to_turn, "{token}")?;
    let from_turn = sock.try_clone()?;
    std::thread::spawn(move || {
        let mut out = std::io::stdout().lock();
        for line in std::io::BufReader::new(from_turn).lines() {
            let Ok(line) = line else { break };
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
        // The turn is gone: so is this server.
        std::process::exit(0);
    });
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        writeln!(to_turn, "{line}")?;
    }
    let _ = sock.shutdown(std::net::Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_model_claude_knows_goes_on_the_command_line() {
        assert_eq!(claude_model(Some("claude-opus-5-5")).as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude_model(Some("opus")).as_deref(), Some("opus"));
        assert_eq!(claude_model(Some("sonnet[1m]")).as_deref(), Some("sonnet[1m]"));
        assert_eq!(claude_model(Some("claude-sonnet-5[1m]")).as_deref(), Some("claude-sonnet-5[1m]"));
        for other in [None, Some(""), Some("kimi-k2.7-code"), Some("deepseek-v4-pro"), Some("beam-free")] {
            assert_eq!(claude_model(other), None, "{other:?}");
        }
    }

    /// A kernel tool becomes an MCP tool with its schema; one with no
    /// parameters still gets an object schema, which MCP requires.
    #[test]
    fn kernel_tools_become_mcp_tools() {
        let t = json!({ "type": "function", "function": { "name": "Bash", "description": "Run it", "parameters": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] } } });
        assert_eq!(
            mcp_tool(&t).unwrap(),
            json!({ "name": "Bash", "description": "Run it", "inputSchema": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] } })
        );
        let bare = json!({ "type": "function", "function": { "name": "Ping", "parameters": {} } });
        assert_eq!(mcp_tool(&bare).unwrap()["inputSchema"], json!({ "type": "object", "properties": {} }));
    }

    /// The opening message: the turn's data, the conversation so far only for
    /// a `claude` that starts without it, then the turn's words.
    #[test]
    fn the_opening_message_tells_a_fresh_claude_the_conversation_so_far() {
        let messages = vec![
            json!({ "role": "system", "content": "persona" }),
            json!({ "role": "user", "content": "data" }),
            json!({ "role": "user", "content": "earlier question" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "Bash", "arguments": "{\"command\":\"ls\"}" } }] }),
            json!({ "role": "tool", "tool_call_id": "c1", "content": "a.txt" }),
            json!({ "role": "assistant", "content": "There is a.txt." }),
            json!({ "role": "user", "content": "now this" }),
        ];
        let texts = |b: Vec<Value>| b.iter().map(|x| x["text"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let resumed = texts(opening(&messages, &(2..6), true));
        assert_eq!(resumed, ["data", "now this"]);
        let fresh = texts(opening(&messages, &(2..6), false));
        assert_eq!(fresh.len(), 3);
        assert_eq!(fresh[0], "data");
        assert!(fresh[1].contains("<user>\nearlier question\n</user>"), "{}", fresh[1]);
        assert!(fresh[1].contains("<tool_call name=\"Bash\">{\"command\":\"ls\"}</tool_call>"), "{}", fresh[1]);
        assert!(fresh[1].contains("<tool_result>\na.txt\n</tool_result>"), "{}", fresh[1]);
        assert!(fresh[1].contains("<assistant>\nThere is a.txt.\n</assistant>"), "{}", fresh[1]);
        assert_eq!(fresh[2], "now this");
    }

    /// A picture a tool hands back reaches Claude as a picture.
    #[test]
    fn a_tools_picture_reaches_claude_as_a_picture() {
        let content = json!([{ "type": "text", "text": "shot" }, { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBOR" } }]);
        assert_eq!(
            tool_blocks(&content),
            vec![json!({ "type": "text", "text": "shot" }), json!({ "type": "image", "data": "iVBOR", "mimeType": "image/png" })]
        );
        assert_eq!(message_blocks(&content)[1], json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBOR" } }));
    }

    fn line(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// The step reads Claude's words as they stream, learns the session from
    /// `init`, and ends on the turn's result with the words and the usage.
    #[test]
    fn a_step_streams_the_words_and_ends_on_the_result() {
        let session = Mutex::new(None);
        let mut uses = Vec::new();
        let mut step = Step::default();
        let mut events = Vec::new();
        for l in [
            r#"{"type":"system","subtype":"init","session_id":"s-1","parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":1}}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s-1","result":"Hello"}"#,
        ] {
            events.extend(step.read(&line(l), &mut uses, &session));
        }
        assert_eq!(session.lock().unwrap().as_deref(), Some("s-1"));
        let Some(Out::End(ChatEvent::Done(m))) = events.pop() else { panic!("ends on Done") };
        assert_eq!(m.content, "Hello");
        assert!(m.calls.is_empty());
        assert_eq!(m.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!((m.input_tokens, m.tokens), (100, 7));
        assert_eq!(m.stats.model.as_deref(), Some("claude-opus-5-5"));
        let shown: Vec<String> = events
            .iter()
            .map(|o| match o {
                Out::Event(ChatEvent::Text(t)) => format!("text:{t}"),
                Out::Event(ChatEvent::Thinking(t)) => format!("thought:{t}"),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(shown, ["thought:hm", "text:Hel", "text:lo"]);
    }

    /// Claude's tool use is remembered under the kernel's name for it, and the
    /// call `claude` then makes is matched to it — same id the kernel answers.
    #[test]
    fn a_call_is_matched_to_the_tool_use_it_answers() {
        let session = Mutex::new(None);
        let mut step = Step::default();
        let mut uses = Vec::new();
        step.read(
            &line(r#"{"type":"assistant","parent_tool_use_id":null,"message":{"model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"mcp__mafold__Bash","input":{"command":"ls"}}]}}"#),
            &mut uses,
            &session,
        );
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "Bash");
        assert_eq!(uses[0].id, "toolu_1");
    }

    /// A refused turn says which window refused it, so the seat can move on.
    #[test]
    fn a_usage_limit_ends_the_step_as_a_quota_refusal() {
        let session = Mutex::new(None);
        let mut step = Step::default();
        let mut uses = Vec::new();
        step.read(&line(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1795000000}}"#), &mut uses, &session);
        let mut out = step.read(&line(r#"{"type":"result","subtype":"success","is_error":true,"result":"Claude usage limit reached|1795000000"}"#), &mut uses, &session);
        let Some(Out::End(ChatEvent::QuotaExhausted { kind, resets_at, .. })) = out.pop() else { panic!("a quota refusal") };
        assert_eq!((kind.as_str(), resets_at), ("five_hour", Some(1795000000)));

        let mut step = Step::default();
        let mut out = step.read(&line(r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["boom"]}"#), &mut uses, &session);
        let Some(Out::End(ChatEvent::Failed(e))) = out.pop() else { panic!("a failure") };
        assert_eq!(e, "Claude Code: boom");
    }
}
