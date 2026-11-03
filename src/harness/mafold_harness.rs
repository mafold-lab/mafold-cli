//! Mafold Harness on its owner's computer: the loop the official bots and
//! Cloud agents run on (`mafold-harness`), run in this process.
//!
//! Its runtime id is `opencode` — the one New Agent's Mafold Harness tile,
//! its sign-ins (the wallet, OpenCode Go) and every agent already stored use.
//! It named the pinned OpenCode build this replaces (ops, 11-02: Mafold
//! Harness is the house kernel, carried by mafold-cli).
//!
//! What shapes this impl:
//! - **Nothing to install.** A turn needs only its fuel — the reply's wallet
//!   key, or the owner's OpenCode Go key, handed over in [`Turn::env`] as they
//!   always were — and the OpenAI-compatible door it is spent at.
//! - **The daemon draws the reply.** Every event the loop writes reaches the
//!   daemon's sink through the transcript tap (`kernel::Turn::tap`), so a turn
//!   renders as the same cards every other runtime's do; the loop's frames
//!   only say how the turn ended.
//! - **A conversation of its own.** The model's messages for each chat live in
//!   a session file (`kernel::Turn::keep`) and are read back next turn — the
//!   daemon hands a turn only what is new.
//! - **Corrected mid-turn.** What someone says while it works is read from the
//!   turn's steer mailbox at the next tool boundary; the daemon has already
//!   drawn the seam.
//! - **Its folder, for the owner's circle** ([`super::computer`]); a guest's
//!   turn gets no tools at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use mafold_core::mafold_types::steer::{mailbox, Part};
use mafold_harness::kernel::{self, Keep, Live};
use mafold_harness::{HarnessPlugin, OpenAiCompatRuntime, PersonaPlugin, Tools, TurnCtx};
use mafold_transcript::Tap;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use super::computer::{self, Computer};
use super::opencode::{self, Fuel};
use super::{AgentEvent, CommandOutcome, Harness, HarnessProbe, Turn, TurnOutcome};
use crate::client::Client;

pub struct MafoldHarness;

/// OpenCode Go's OpenAI-compatible door (models.dev, provider `opencode-go`).
const GO_BASE: &str = "https://opencode.ai/inference/go/openai/v1";

/// How Mafold Harness names itself to OpenCode Go, which asks each client
/// for its own `User-Agent`.
const GO_USER_AGENT: &str = concat!("mafold-harness/", env!("CARGO_PKG_VERSION"));

/// The output cap asked for per call on the wallet — what the router holds
/// besides the input before each call, as the OpenCode build asked.
const WALLET_OUTPUT_TOKENS: u32 = 32_000;

/// How much of a conversation a turn sends back at most, in characters of its
/// JSON: past it the oldest exchanges go, whole.
const KEEP_CHARS: usize = 400_000;

/// A tool's output in an EARLIER exchange is kept to this much; the current
/// exchange keeps everything.
const OLD_RESULT_CHARS: usize = 4_000;

/// How often a working turn says it is alive while nothing streams.
const PULSE_EVERY: Duration = Duration::from_secs(1);

#[async_trait]
impl Harness for MafoldHarness {
    fn id(&self) -> &'static str {
        "opencode"
    }

    /// Always: it is part of this program.
    fn available(&self) -> bool {
        true
    }

    /// Yes: the session is named before the loop says anything.
    fn names_session_first(&self) -> bool {
        true
    }

    fn can_steer(&self) -> bool {
        true
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        run_turn(turn, sink, &home()).await
    }

    fn discover(&self, _workdir: &str) -> Value {
        Value::Array(vec![])
    }

    async fn command(
        &self,
        _client: &Client,
        _chat_id: &str,
        _name: &str,
        _arg: &str,
        _workdir: &str,
        _session: Option<&str>,
        _env: &[(String, String)],
    ) -> CommandOutcome {
        CommandOutcome::Forward
    }

    /// This program's: the loop is part of it.
    async fn cli_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// OpenCode Go's models this loop can talk to — the model menu of an agent
    /// on Go. On the wallet the models are the router's (`agent::report_caps`).
    async fn caps(&self, _env: &[(String, String)], _version: &str) -> Option<HarnessProbe> {
        opencode::go_probe(go_models().await)
    }
}

/// The ids of OpenCode Go's models this loop can run — `/model`'s examples and
/// its check.
pub async fn go_model_ids() -> Vec<String> {
    go_models().await.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect()
}

/// Where Mafold Harness keeps what it keeps on this computer: each agent's
/// sessions, and the list of Go's models.
fn home() -> PathBuf {
    crate::platform::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".mafold").join("runtimes").join("mafold-harness")
}

/// Whose sessions a turn uses: the bot part of its surface tag
/// (`{conv}__{channel}__{bot}`, a guest's mark dropped), else the daemon's
/// name.
fn agent_name(surface: &str) -> String {
    let parts: Vec<&str> = surface.split("__").collect();
    let bot = match parts[..] {
        [_, _, bot] => bot.split("--guest").next().unwrap_or(""),
        _ => "",
    };
    if !bot.is_empty() {
        return bot.to_string();
    }
    std::env::var("MAFOLD_DAEMON_NAME").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "default".into())
}

/// One turn, its sessions kept under `home` ([`home`]; a test's own).
async fn run_turn(turn: Turn, sink: UnboundedSender<AgentEvent>, home: &std::path::Path) -> Result<TurnOutcome> {
    let Turn {
        prompt,
        conv,
        surface,
        draft,
        workdir,
        session,
        model,
        effort,
        thinking: _,
        cancel,
        system,
        ask_file: _,
        steer_file,
        env,
        mount,
        skill_plugins: _,
        proc,
    } = turn;
    if !std::path::Path::new(&workdir).is_dir() {
        bail!("working directory does not exist: {workdir} — check --workdir");
    }
    let Some(fuel) = Fuel::from_env(&env, false) else {
        return Ok(TurnOutcome { error: Some(opencode::NO_WALLET_KEY.into()), ..Default::default() });
    };
    let runtime = match &fuel {
        Fuel::Wallet(w) => match opencode::pick_model(model.as_deref(), &w.models) {
            Ok(m) => OpenAiCompatRuntime::new(w.key.clone(), w.base.clone(), m, None, "the wallet's router")
                .with_max_tokens(Some(WALLET_OUTPUT_TOKENS)),
            Err(why) => return Ok(TurnOutcome { error: Some(why), ..Default::default() }),
        },
        Fuel::Go { key: Some(key), base } => OpenAiCompatRuntime::new(
            key.clone(),
            base.clone().unwrap_or_else(|| GO_BASE.to_string()).trim_end_matches('/').to_string(),
            opencode::go_model(model.as_deref()),
            None,
            "OpenCode Go",
        ),
        // Only your own OpenCode may run on a Go login of its own.
        Fuel::Go { key: None, .. } => {
            return Ok(TurnOutcome { error: Some(opencode::NO_WALLET_KEY.into()), ..Default::default() })
        }
    }
    .with_reasoning_effort(effort);
    let model = mafold_harness::ModelRuntime::configured_model(&runtime).map(str::to_string);

    let agent = agent_name(&surface);
    let dir = home.join(&agent).join("sessions");
    let (session, history) = match session.as_deref().filter(|s| uuid::Uuid::parse_str(s).is_ok()) {
        Some(id) => (id.to_string(), load(&dir, id)),
        None => (uuid::Uuid::new_v4().to_string(), Vec::new()),
    };
    let _ = sink.send(AgentEvent::Session(session.clone()));
    // OpenCode Go routes by conversation and refuses a request that doesn't
    // say which one (400 `MissingSessionID`): the kept session's id, and the
    // client's own name rather than an HTTP library's (Go's "Where can I use
    // it?", https://opencode.ai/v2/docs/console/go/).
    let runtime = match &fuel {
        Fuel::Go { .. } => runtime.with_headers(&[("x-opencode-session", session.as_str()), ("User-Agent", GO_USER_AGENT)]),
        Fuel::Wallet(_) => runtime,
    };

    let mut plugins: Vec<Arc<dyn HarnessPlugin>> = Vec::new();
    if let Some(s) = system.filter(|s| !s.trim().is_empty()) {
        plugins.push(Arc::new(PersonaPlugin::new(s)));
    }
    if !mount.guest {
        let mut extra: Vec<(String, String)> =
            super::turn_env(&conv, &surface, &mount.reads).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        extra.push(("MAFOLD_DRAFT".into(), draft.clone()));
        extra.push(("MAFOLD_SURFACE".into(), surface.clone()));
        plugins.push(Arc::new(Computer::new(PathBuf::from(&workdir), computer::shell_env(&env, extra), proc.clone())));
    }
    let ctx = TurnCtx {
        bot: agent.clone(),
        conv_id: uuid::Uuid::parse_str(&conv).ok(),
        draft_id: uuid::Uuid::parse_str(&draft).ok(),
        model: model.clone(),
        ..Default::default()
    };
    let tools = Tools::of(&plugins, &ctx, &agent).await;
    let mut system = kernel::system_prompt(&plugins, &ctx, []);
    let data = match kernel::prepare(&plugins, &ctx, &mut system).await {
        Ok(d) => d,
        Err(e) => return Ok(TurnOutcome { error: Some(e), session: Some(session), ..Default::default() }),
    };
    let mut messages = vec![json!({ "role": "system", "content": system })];
    messages.extend(data.into_iter().map(|d| json!({ "role": "user", "content": d })));
    let kept_from = messages.len();
    messages.extend(history);
    messages.push(json!({ "role": "user", "content": prompt }));

    // Stop: the daemon's `notify_one` leaves a permit, so one pressed before
    // this point still counts.
    let (stopped_tx, stopped_rx) = watch::channel(false);
    let watcher = tokio::spawn(async move {
        cancel.notified().await;
        let _ = stopped_tx.send(true);
    });
    let live = Arc::new(CliLive { stopped: stopped_rx, steer_file });

    let produced = Arc::new(AtomicBool::new(false));
    let tap: Tap = {
        let (sink, produced) = (sink.clone(), produced.clone());
        Arc::new(move |ev: &AgentEvent| {
            if matches!(ev, AgentEvent::Text(t) if !t.trim().is_empty()) || matches!(ev, AgentEvent::ToolCall { .. }) {
                produced.store(true, Ordering::Relaxed);
            }
            let _ = sink.send(ev.clone());
        })
    };
    let kept = Arc::new(Mutex::new(None::<Vec<Value>>));
    let keep: Keep = {
        let kept = kept.clone();
        Box::new(move |m: Vec<Value>| *kept.lock().unwrap() = Some(m))
    };
    let turn = kernel::Turn {
        ctx,
        runtime: Arc::new(runtime),
        plugins,
        tools,
        messages,
        model,
        fallback: None,
        live,
        // The daemon says a stopped reply was stopped, in its own words.
        stopped_line: String::new(),
        // The wallet is metered by the router's ledger (`fuel::settle`), and
        // OpenCode Go is the owner's own plan.
        bill: None,
        tap: Some(tap),
        keep: Some(keep),
    };

    let mut outcome = TurnOutcome { session: Some(session.clone()), ..Default::default() };
    let mut frames = kernel::run(turn);
    let mut pulsed = Instant::now();
    while let Some(frame) = frames.next().await {
        match frame {
            Err(e) => outcome.error = Some(e),
            Ok(f) if f.stopped => outcome.stopped = true,
            // An empty append is the loop saying it is still at work.
            Ok(f) if f.text.is_empty() && !f.rewrite && !f.steered && f.private.is_empty() => {
                if pulsed.elapsed() >= PULSE_EVERY {
                    pulsed = Instant::now();
                    let _ = sink.send(AgentEvent::Pulse { chars: 0, tokens: None });
                }
            }
            // Everything it says reached the sink through the tap.
            Ok(_) => {}
        }
    }
    watcher.abort();
    outcome.produced = produced.load(Ordering::Relaxed);
    if let Some(all) = kept.lock().unwrap().take() {
        let conversation: Vec<Value> = all.into_iter().skip(kept_from).collect();
        if let Err(e) = save(&dir, &session, &tidy(conversation)) {
            eprintln!("⚠ Mafold Harness couldn't keep session {session} ({e}) — the next reply starts without it");
        }
    }
    Ok(outcome)
}

/// Where the turn hears from the chat: Stop, and what was said meanwhile.
struct CliLive {
    stopped: watch::Receiver<bool>,
    steer_file: Option<String>,
}

#[async_trait]
impl Live for CliLive {
    fn is_cancelled(&self) -> bool {
        *self.stopped.borrow()
    }

    async fn cancelled(&self) {
        let mut rx = self.stopped.clone();
        if rx.wait_for(|s| *s).await.is_err() {
            // The watcher is gone without a Stop: none will come.
            std::future::pending::<()>().await;
        }
    }

    /// The mailbox, drained the way the Claude Code hook drains it — the same
    /// words reach this model — with no seam of its own: the daemon drew it
    /// when the message arrived.
    async fn boundary(&self) -> Vec<(Option<String>, Value)> {
        let Some(path) = &self.steer_file else { return Vec::new() };
        crate::steer_hook::take(path)
            .and_then(|raw| Part::for_model(&mailbox::parse(&raw)))
            .map(|text| vec![(None, json!(text))])
            .unwrap_or_default()
    }
}

// ─────────────────────────────── sessions ───────────────────────────────

/// A session's messages, or none — a file that can't be read starts the
/// conversation over rather than failing the turn.
fn load(dir: &std::path::Path, id: &str) -> Vec<Value> {
    std::fs::read(dir.join(format!("{id}.json")))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok())
        .unwrap_or_default()
}

fn save(dir: &std::path::Path, id: &str, messages: &[Value]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join(format!("{id}.json"));
    let tmp = dir.join(format!("{id}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(messages)?)?;
    std::fs::rename(tmp, file)
}

/// A conversation fit to be sent again:
/// - every tool call answered — one the turn ended before running (Stop, a
///   failure) gets a result saying so, or the next request is refused;
/// - tool output of earlier exchanges cut to [`OLD_RESULT_CHARS`];
/// - the oldest exchanges dropped, whole, past [`KEEP_CHARS`].
fn tidy(messages: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut owed: Vec<String> = Vec::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("").to_string();
        if role == "tool" {
            let id = m["tool_call_id"].as_str().unwrap_or("").to_string();
            owed.retain(|o| *o != id);
            out.push(m);
            continue;
        }
        settle(&mut out, &mut owed);
        if role == "assistant" {
            owed = m["tool_calls"]
                .as_array()
                .map(|c| c.iter().filter_map(|c| c["id"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
        }
        out.push(m);
    }
    settle(&mut out, &mut owed);

    // Exchanges begin where a person speaks.
    let starts: Vec<usize> = out.iter().enumerate().filter(|(_, m)| m["role"] == "user").map(|(i, _)| i).collect();
    if let Some(&last) = starts.last() {
        for m in &mut out[..last] {
            if m["role"] == "tool" {
                if let Some(s) = m["content"].as_str().filter(|s| s.len() > OLD_RESULT_CHARS) {
                    let cut = (0..=OLD_RESULT_CHARS).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
                    m["content"] = json!(format!("{}\n… (cut: {} characters more)", &s[..cut], s.len() - cut));
                }
            }
        }
    }
    let size = |ms: &[Value]| ms.iter().map(|m| m.to_string().len()).sum::<usize>();
    let mut from = 0;
    for &s in starts.iter().skip(1) {
        if size(&out[from..]) <= KEEP_CHARS {
            break;
        }
        from = s;
    }
    out.split_off(from)
}

/// The results still owed for the last assistant step's calls, said.
fn settle(out: &mut Vec<Value>, owed: &mut Vec<String>) {
    for id in owed.drain(..) {
        out.push(json!({ "role": "tool", "tool_call_id": id, "content": "(not run: the turn ended first)" }));
    }
}

// ───────────────────────────── Go's models ─────────────────────────────

/// models.dev's catalogue, where OpenCode itself reads OpenCode Go's models.
const CATALOGUE: &str = "https://models.dev/api.json";

/// How long a fetched list is used, and how long a failed fetch waits.
const MODELS_FOR: Duration = Duration::from_secs(6 * 3600);
const MODELS_RETRY: Duration = Duration::from_secs(600);

static GO_MODELS: Mutex<Option<(Instant, Option<Vec<Value>>)>> = Mutex::new(None);

/// OpenCode Go's models this loop can talk to (OpenAI-compatible ones), as
/// `{id, name}` — fetched now and then, kept on disk for the times the fetch
/// fails. Empty when nothing is known: a menu is never emptied by a failure.
pub(super) async fn go_models() -> Vec<Value> {
    let fresh = |at: Instant, got: bool| at.elapsed() < if got { MODELS_FOR } else { MODELS_RETRY };
    if let Some((at, list)) = GO_MODELS.lock().unwrap().clone() {
        if fresh(at, list.is_some()) {
            return list.unwrap_or_else(cached_go_models);
        }
    }
    let fetched = fetch_go_models().await;
    if let Some(list) = &fetched {
        let _ = std::fs::create_dir_all(models_file().parent().unwrap_or(std::path::Path::new(".")));
        let _ = std::fs::write(models_file(), serde_json::to_vec(list).unwrap_or_default());
    }
    *GO_MODELS.lock().unwrap() = Some((Instant::now(), fetched.clone()));
    fetched.unwrap_or_else(cached_go_models)
}

fn models_file() -> PathBuf {
    home().join("go-models.json")
}

fn cached_go_models() -> Vec<Value> {
    std::fs::read(models_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

async fn fetch_go_models() -> Option<Vec<Value>> {
    let resp = reqwest::Client::new().get(CATALOGUE).timeout(Duration::from_secs(30)).send().await.ok()?;
    let catalogue: Value = resp.error_for_status().ok()?.json().await.ok()?;
    let list = chat_models(&catalogue["opencode-go"]);
    (!list.is_empty()).then_some(list)
}

/// A provider's models from the catalogue that speak chat completions at the
/// provider's own door — those with no other door of their own, or one of the
/// same kind — as `{id, name}`, by id.
fn chat_models(provider: &Value) -> Vec<Value> {
    let Some(models) = provider["models"].as_object() else { return Vec::new() };
    models
        .iter()
        .filter(|(_, m)| {
            m["tool_call"].as_bool().unwrap_or(true)
                && m["status"].as_str() != Some("deprecated")
                && matches!(m["provider"]["npm"].as_str(), None | Some("@ai-sdk/openai-compatible"))
        })
        .map(|(id, m)| json!({ "id": id, "name": m["name"].as_str().unwrap_or(id) }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A stand-in OpenAI-compatible model: each request gets the next reply
    /// (an SSE body), and every request body is kept.
    async fn model(replies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let kept = bodies.clone();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut req = Vec::new();
                let mut buf = [0u8; 16 * 1024];
                let at = loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).to_lowercase();
                    let Some(head) = text.find("\r\n\r\n") else { continue };
                    let len = text[..head]
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if req.len() >= head + 4 + len {
                        break head + 4;
                    }
                };
                kept.lock().unwrap().push(serde_json::from_slice(&req[at..]).unwrap_or(Value::Null));
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                            reply.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        (base, bodies)
    }

    fn sse(frames: &[Value]) -> String {
        frames.iter().map(|f| format!("data: {f}\n\n")).collect::<String>() + "data: [DONE]\n\n"
    }

    fn says(text: &str) -> String {
        sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "content": text } }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        ])
    }

    fn runs(id: &str, name: &str, args: Value) -> String {
        sse(&[
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [
                { "index": 0, "id": id, "type": "function", "function": { "name": name, "arguments": args.to_string() } }
            ] } }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        ])
    }

    fn folder(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mafold-mh-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wallet(base: &str) -> Vec<(String, String)> {
        vec![
            ("MAFOLD_WALLET_KEY".into(), "reply-key".into()),
            ("MAFOLD_WALLET_BASE".into(), base.into()),
            ("MAFOLD_WALLET_MODELS".into(), "deepseek-v4,kimi-k3".into()),
        ]
    }

    fn turn(work: &std::path::Path, surface: &str, session: Option<String>, prompt: &str, env: Vec<(String, String)>) -> Turn {
        Turn {
            prompt: prompt.into(),
            conv: "4b62044f-3993-4376-8402-4d8b26df5924".into(),
            surface: surface.into(),
            draft: uuid::Uuid::new_v4().to_string(),
            workdir: work.to_string_lossy().into_owned(),
            session,
            model: None,
            effort: None,
            thinking: None,
            cancel: Arc::new(tokio::sync::Notify::new()),
            system: Some("You are Probe.".into()),
            ask_file: None,
            steer_file: None,
            env,
            mount: Default::default(),
            skill_plugins: None,
            proc: Default::default(),
        }
    }

    /// Runs `t` with its sessions kept in the test's own folder (the workdir's
    /// parent: a fresh one per test).
    async fn drive(t: Turn) -> (TurnOutcome, Vec<AgentEvent>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let home = PathBuf::from(&t.workdir).with_extension("home");
        let out = run_turn(t, tx, &home).await.expect("the turn ran");
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        (out, events)
    }

    fn surface(tag: &str) -> String {
        format!("4b62044f-3993-4376-8402-4d8b26df5924__{}__probe-{tag}", "")
    }

    #[tokio::test]
    async fn a_turn_on_the_wallet_runs_a_tool_in_its_folder_and_resumes_next_turn() {
        let work = folder("work");
        std::fs::write(work.join("hello.txt"), "hello from the folder").unwrap();
        let (base, bodies) = model(vec![
            runs("c1", "Bash", json!({ "command": "cat hello.txt" })),
            says("It says hello."),
            says("You asked about hello.txt."),
        ])
        .await;
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (out, events) = drive(turn(&work, &surface(&tag), None, "what's in hello.txt?", wallet(&base))).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        assert!(out.produced && !out.stopped);
        let session = out.session.clone().expect("a session");
        assert!(matches!(&events[0], AgentEvent::Session(s) if *s == session), "named first: {:?}", events.first());
        let result = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(result.as_deref(), Some("hello from the folder"));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Text(t) if t == "It says hello.")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done { .. })));

        // What the model was sent: the key, never in the shell; the model
        // the wallet runs first; the tools.
        let first = bodies.lock().unwrap()[0].clone();
        assert_eq!(first["model"], "deepseek-v4");
        let tools: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
        assert_eq!(tools, ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]);
        assert!(first["messages"][0]["content"].as_str().unwrap().starts_with("You are Probe."));

        // Next turn, same session: the conversation comes back.
        let (out2, _) = drive(turn(&work, &surface(&tag), Some(session.clone()), "and what did I ask?", wallet(&base))).await;
        assert!(out2.error.is_none(), "{:?}", out2.error);
        let third = bodies.lock().unwrap()[2].clone();
        let roles: Vec<&str> = third["messages"].as_array().unwrap().iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant", "user"]);
        assert_eq!(third["messages"][1]["content"], "what's in hello.txt?");
    }

    /// A stand-in OpenCode Go: like [`model`], but it refuses a request that
    /// doesn't name its conversation exactly as Go does (400
    /// `MissingSessionID`), and keeps each request's head.
    async fn go(replies: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let kept = heads.clone();
        tokio::spawn(async move {
            let mut replies = replies.into_iter();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut req = Vec::new();
                let mut buf = [0u8; 16 * 1024];
                let head = loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).to_lowercase();
                    let Some(at) = text.find("\r\n\r\n") else { continue };
                    let len = text[..at]
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if req.len() >= at + 4 + len {
                        break text[..at].to_string();
                    }
                };
                let named = head.contains("\r\nx-opencode-session: ");
                kept.lock().unwrap().push(head);
                let (status, kind, body) = if named {
                    let Some(reply) = replies.next() else { return };
                    ("200 OK", "text/event-stream", reply)
                } else {
                    let refusal = r#"{"type":"error","error":{"type":"MissingSessionID","message":"Request is missing x-opencode-session and cannot be routed efficiently."}}"#;
                    ("400 Bad Request", "application/json", refusal.to_string())
                };
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = sock.shutdown().await;
            }
        });
        (base, heads)
    }

    fn go_fuel(base: &str) -> Vec<(String, String)> {
        vec![(crate::fuel::GO_KEY_ENV.into(), "sk-owners-go".into()), ("MAFOLD_DEV_GO_BASE".into(), base.into())]
    }

    /// 11-02, the first real Mafold Harness run on OpenCode Go: every call
    /// refused, 400 `MissingSessionID`. Every call names the conversation by
    /// the session the turn keeps — the same id on the next turn — and the
    /// client by its own name.
    #[tokio::test]
    async fn on_go_every_call_names_its_conversation_and_the_client() {
        let work = folder("go");
        std::fs::write(work.join("hello.txt"), "hello from the folder").unwrap();
        let (base, heads) = go(vec![
            runs("c1", "Bash", json!({ "command": "cat hello.txt" })),
            says("It says hello."),
            says("Still here."),
        ])
        .await;
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (out, _) = drive(turn(&work, &surface(&tag), None, "what's in hello.txt?", go_fuel(&base))).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        let session = out.session.clone().expect("a session");
        let (out2, _) = drive(turn(&work, &surface(&tag), Some(session.clone()), "still there?", go_fuel(&base))).await;
        assert!(out2.error.is_none(), "{:?}", out2.error);
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 3, "both turns, nothing refused: {heads:?}");
        for h in heads.iter() {
            assert!(h.contains(&format!("\r\nx-opencode-session: {session}")), "{h}");
            assert!(h.contains(&format!("\r\nuser-agent: {GO_USER_AGENT}")), "{h}");
            assert!(h.contains("\r\nauthorization: bearer sk-owners-go"), "{h}");
        }
    }

    #[tokio::test]
    async fn the_shell_never_sees_the_key_it_runs_on() {
        let work = folder("key");
        let (base, _) = model(vec![
            runs("c1", "Bash", json!({ "command": "echo \"key=[$MAFOLD_WALLET_KEY]\"" })),
            says("Done."),
        ])
        .await;
        let (out, events) = drive(turn(&work, &surface("key"), None, "print it", wallet(&base))).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        let result = events.iter().find_map(|e| match e {
            AgentEvent::ToolResult { text, .. } => Some(text.clone()),
            _ => None,
        });
        assert_eq!(result.as_deref(), Some("key=[]"));
    }

    #[tokio::test]
    async fn a_guests_turn_gets_no_tools() {
        let work = folder("guest");
        let (base, bodies) = model(vec![says("Hi.")]).await;
        let mut t = turn(&work, &surface("guest"), None, "hi", wallet(&base));
        t.mount.guest = true;
        let (out, _) = drive(t).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        assert!(bodies.lock().unwrap()[0].get("tools").is_none(), "no tools offered");
    }

    #[tokio::test]
    async fn no_fuel_no_turn_and_a_model_the_wallet_doesnt_run_is_refused() {
        let work = folder("fuel");
        let (out, _) = drive(turn(&work, &surface("fuel"), None, "hi", vec![])).await;
        assert_eq!(out.error.as_deref(), Some(opencode::NO_WALLET_KEY));
        let mut t = turn(&work, &surface("fuel"), None, "hi", wallet("http://127.0.0.1:9/v1"));
        t.model = Some("gpt-9".into());
        let (out, _) = drive(t).await;
        assert!(out.error.unwrap().contains("which the wallet doesn't run"));
    }

    #[tokio::test]
    async fn stop_ends_the_turn_mid_tool() {
        let work = folder("stop");
        let (base, _) = model(vec![runs("c1", "Bash", json!({ "command": "sleep 30" }))]).await;
        let t = turn(&work, &surface("stop"), None, "wait", wallet(&base));
        let cancel = t.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel.notify_one();
        });
        let started = Instant::now();
        let (out, _) = drive(t).await;
        assert!(out.stopped, "stopped");
        assert!(started.elapsed() < Duration::from_secs(10), "it did not wait the sleep out");
    }

    #[tokio::test]
    async fn a_correction_reaches_the_model_at_the_next_boundary() {
        let work = folder("steer");
        // The person speaks while the tool runs: the command itself leaves
        // the message where the daemon would.
        let (base, bodies) = model(vec![
            runs("c1", "Bash", json!({ "command": "printf 'no, the other file\\n' > steer.txt" })),
            says("Switched to the other file."),
        ])
        .await;
        let steer = work.join("steer.txt");
        let mut t = turn(&work, &surface("steer"), None, "edit the file", wallet(&base));
        t.steer_file = Some(steer.to_string_lossy().into_owned());
        let (out, _) = drive(t).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        let second = bodies.lock().unwrap()[1].clone();
        let last = second["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["role"], "user");
        assert!(last["content"].as_str().unwrap().contains("no, the other file"), "{last}");
        assert!(!steer.exists(), "the mailbox was drained");
    }

    #[test]
    fn a_kept_conversation_answers_every_call_and_stays_in_budget() {
        let call = |id: &str| json!({ "role": "assistant", "content": "", "tool_calls": [{ "id": id, "type": "function", "function": { "name": "Bash", "arguments": "{}" } }] });
        let tidied = tidy(vec![json!({ "role": "user", "content": "go" }), call("c1")]);
        assert_eq!(tidied.last().unwrap()["tool_call_id"], "c1", "a call the turn never ran is answered");

        let big = "x".repeat(OLD_RESULT_CHARS * 3);
        let mut ms = Vec::new();
        for i in 0..400 {
            ms.push(json!({ "role": "user", "content": format!("q{i}") }));
            ms.push(call(&format!("c{i}")));
            ms.push(json!({ "role": "tool", "tool_call_id": format!("c{i}"), "content": big }));
            ms.push(json!({ "role": "assistant", "content": format!("a{i}") }));
        }
        let tidied = tidy(ms);
        let size: usize = tidied.iter().map(|m| m.to_string().len()).sum();
        assert!(size <= KEEP_CHARS, "{size}");
        assert_eq!(tidied[0]["role"], "user", "it starts where someone spoke");
        assert_eq!(tidied.last().unwrap()["content"], "a399", "the newest exchange stays");
        let last_tool = tidied.iter().rev().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(last_tool["content"].as_str().unwrap().len(), big.len(), "the newest exchange's output is whole");
    }

    #[test]
    fn each_agent_has_its_own_sessions() {
        assert_eq!(agent_name("c1__ch__ops_wallet"), "ops_wallet");
        assert_eq!(agent_name("c1____ops_wallet--guest-bob"), "ops_wallet");
        assert_eq!(agent_name("c1____ops_wallet--guest"), "ops_wallet");
    }

    #[test]
    fn only_go_models_that_speak_chat_completions_are_offered() {
        let go = json!({ "models": {
            "kimi-k2.7-code": { "name": "Kimi K2.7 Code", "tool_call": true },
            "minimax-m3": { "name": "MiniMax M3", "provider": { "npm": "@ai-sdk/anthropic" } },
            "glm-5.3": { "name": "GLM 5.3", "provider": { "npm": "@ai-sdk/openai-compatible" } },
            "no-tools": { "name": "No tools", "tool_call": false }
        }});
        let ids: Vec<String> = chat_models(&go).iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
        assert_eq!(ids, ["glm-5.3", "kimi-k2.7-code"]);
    }
}
