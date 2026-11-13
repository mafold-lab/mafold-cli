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
//!   always were — and the OpenAI-compatible door it is spent at. On Claude
//!   Code ([`super::claude_fuel`]) the fuel is the owner's login here, and the
//!   `claude` installed beside it makes each step.
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

use super::claude_fuel::{self, ClaudeEngine};
use super::computer::{self, Computer};
use super::opencode::{self, Fuel};
use super::{AgentEvent, CommandOutcome, Harness, HarnessProbe, Turn, TurnOutcome};
use crate::client::Client;

pub struct MafoldHarness;

/// What a turn's model steps run on: an OpenAI-compatible door (the wallet's
/// router, OpenCode Go) with its fuel, or the owner's Claude Code here, with
/// the model `claude` is told (none: its own default).
enum Engine {
    OpenAi(OpenAiCompatRuntime, Fuel),
    Claude(Option<String>),
}

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
    // What the turn runs on: the owner's Claude Code login here, when the
    // daemon says so; else the wallet's key or the owner's Go key.
    let engine = if crate::fuel::claude_in(&env) {
        Engine::Claude(claude_fuel::claude_model(model.as_deref()))
    } else {
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
        .with_reasoning_effort(effort.clone());
        Engine::OpenAi(runtime, fuel)
    };
    let model = match &engine {
        Engine::OpenAi(runtime, _) => mafold_harness::ModelRuntime::configured_model(runtime).map(str::to_string),
        Engine::Claude(m) => m.clone(),
    };

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
    let engine = match engine {
        Engine::OpenAi(runtime, fuel @ Fuel::Go { .. }) => Engine::OpenAi(
            runtime.with_headers(&[("x-opencode-session", session.as_str()), ("User-Agent", GO_USER_AGENT)]),
            fuel,
        ),
        other => other,
    };

    let mut plugins: Vec<Arc<dyn HarnessPlugin>> = Vec::new();
    if let Some(s) = system.filter(|s| !s.trim().is_empty()) {
        plugins.push(Arc::new(PersonaPlugin::new(s)));
    }
    if let Some(id) = model.as_deref() {
        let (catalogue, on) = match &engine {
            Engine::OpenAi(_, Fuel::Go { .. }) => (cached_go_models(), "OpenCode Go"),
            Engine::OpenAi(_, Fuel::Wallet(_)) => (Vec::new(), "the Mafold wallet"),
            Engine::Claude(_) => (Vec::new(), "your owner's Claude Code plan"),
        };
        plugins.push(Arc::new(PersonaPlugin::new(model_line(id, &catalogue, on))));
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
    let print = prompt_print(&system);
    let mut messages = vec![json!({ "role": "system", "content": system })];
    messages.extend(data.into_iter().map(|d| json!({ "role": "user", "content": d })));
    let kept_from = messages.len();
    let kept = kept_from..kept_from + history.len();
    messages.extend(history);
    messages.push(json!({ "role": "user", "content": prompt }));

    // On Claude Code the turn's `claude` goes on in the conversation's own
    // Claude Code session; kept here to say which one after the turn.
    let mut on_claude: Option<Arc<ClaudeEngine>> = None;
    let runtime: Arc<dyn mafold_harness::ModelRuntime> = match engine {
        Engine::OpenAi(runtime, _) => Arc::new(runtime),
        Engine::Claude(model) => {
            let engine = Arc::new(ClaudeEngine::new(claude_fuel::Spawn {
                workdir: PathBuf::from(&workdir),
                env: env.clone(),
                model,
                effort: effort.clone(),
                resume: claude_session(&dir, &session, &print),
                history: kept,
                exe: claude_fuel::bridge_exe(&env),
            }));
            on_claude = Some(engine.clone());
            engine
        }
    };

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
        runtime,
        plugins,
        tools,
        messages,
        model,
        fallback: None,
        live,
        // The daemon says a stopped reply was stopped, in its own words.
        stopped_line: String::new(),
        // The wallet is metered by the router's ledger (`fuel::settle`), and
        // OpenCode Go and Claude Code are the owner's own plans.
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
    drop(frames);
    outcome.produced = produced.load(Ordering::Relaxed);
    if let Some(all) = kept.lock().unwrap().take() {
        let conversation: Vec<Value> = all.into_iter().skip(kept_from).collect();
        if let Err(e) = save(&dir, &session, &tidy(conversation)) {
            eprintln!("⚠ Mafold Harness couldn't keep session {session} ({e}) — the next reply starts without it");
        }
    }
    if let Some(engine) = on_claude {
        // A full usage window moves the turn to the owner's next Claude
        // login, as a Claude Code agent's does (`accounts::failover`).
        outcome.limit = engine.limit();
        engine.finish(outcome.stopped).await;
        if let Some(id) = engine.session() {
            keep_claude_session(&dir, &session, &id, &print);
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

/// The Claude Code session a session's conversation goes on in, on Claude
/// Code (`claude_fuel`): kept beside it, with the fingerprint of the system
/// prompt it began under ([`prompt_print`]). Claude Code keeps a session's
/// first system prompt for the rest of it, so under another prompt (an edited
/// persona, another model's line) the conversation starts a new session,
/// told what was said.
fn claude_session(dir: &std::path::Path, id: &str, print: &str) -> Option<String> {
    let kept = std::fs::read_to_string(dir.join(format!("{id}.claude"))).ok()?;
    let mut lines = kept.lines().map(str::trim);
    let claude = lines.next().filter(|s| uuid::Uuid::parse_str(s).is_ok())?;
    (lines.next() == Some(print)).then(|| claude.to_string())
}

fn keep_claude_session(dir: &std::path::Path, id: &str, claude: &str, print: &str) {
    let written =
        std::fs::create_dir_all(dir).and_then(|_| std::fs::write(dir.join(format!("{id}.claude")), format!("{claude}\n{print}\n")));
    if let Err(e) = written {
        eprintln!("⚠ Mafold Harness couldn't keep Claude Code session {claude} for {id} ({e}) — the next reply tells claude the conversation instead");
    }
}

/// A system prompt's fingerprint: the first 16 bytes of its SHA-256, in hex.
fn prompt_print(system: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(system.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect()
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

/// The prompt line naming the model this turn runs on and where (`on`), by the
/// name `catalogue` gives it — the kept list, so a turn never waits on a
/// fetch. Told nothing, Beam on Go said it was Claude (11-13).
fn model_line(id: &str, catalogue: &[Value], on: &str) -> String {
    let name = catalogue.iter().find(|m| m["id"] == id).and_then(|m| m["name"].as_str()).unwrap_or(id);
    mafold_core::mafold_types::which_model::line(id, name, Some(on), true)
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
        let system = first["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with("You are Probe."));
        assert!(system.contains("serving replies with the model `deepseek-v4` on the Mafold wallet. "), "the model is named: {system}");

        // Next turn, same session: the conversation comes back.
        let (out2, _) = drive(turn(&work, &surface(&tag), Some(session.clone()), "and what did I ask?", wallet(&base))).await;
        assert!(out2.error.is_none(), "{:?}", out2.error);
        let third = bodies.lock().unwrap()[2].clone();
        let roles: Vec<&str> = third["messages"].as_array().unwrap().iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "assistant", "user"]);
        assert_eq!(third["messages"][1]["content"], "what's in hello.txt?");
    }

    /// A stand-in `claude` (a script) for the engine's plumbing, no login
    /// needed: it speaks stream-json, talks to the bridge as Claude Code's MCP
    /// client does, and sends its tool call over the bridge BEFORE it writes
    /// the message asking for it — the race the step has to win.
    #[cfg(unix)]
    const STAND_IN_CLAUDE: &str = r#"#!/usr/bin/env python3
import json, os, socket, sys, time
args = sys.argv[1:]
def arg(name):
    return args[args.index(name) + 1] if name in args else None
log = open(os.environ["STAND_IN_LOG"], "a")
def note(v):
    log.write(json.dumps(v) + "\n"); log.flush()
def out(v):
    sys.stdout.write(json.dumps(v) + "\n"); sys.stdout.flush()
def ev(e):
    out({"type": "stream_event", "parent_tool_use_id": None, "event": e})
note({"argv": args, "entry": os.environ.get("CLAUDE_CODE_ENTRYPOINT"), "api_key": "ANTHROPIC_API_KEY" in os.environ,
      "system": open(arg("--system-prompt-file")).read()})
session = arg("--resume") or "11111111-2222-4333-8444-555555555555"
note({"opening": json.loads(sys.stdin.readline())})
out({"type": "system", "subtype": "init", "session_id": session, "parent_tool_use_id": None})
env = json.load(open(arg("--mcp-config")))["mcpServers"]["mafold"]["env"]
host, port = env["MAFOLD_BRIDGE_ADDR"].rsplit(":", 1)
mcp = socket.create_connection((host, int(port))).makefile("rw")
mcp.write(env["MAFOLD_BRIDGE_TOKEN"] + "\n")
def send(v):
    mcp.write(json.dumps(v) + "\n"); mcp.flush()
send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})
json.loads(mcp.readline())
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
note({"tools": [t["name"] for t in json.loads(mcp.readline())["result"]["tools"]]})
ev({"type": "message_start", "message": {"model": "claude-stand-in", "usage": {"input_tokens": 10, "output_tokens": 1}}})
if arg("--resume"):
    ev({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Still here."}})
    ev({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}})
    out({"type": "result", "subtype": "success", "is_error": False, "session_id": session, "result": "Still here."})
    sys.stdin.read()
    sys.exit(0)
if os.environ.get("STAND_IN_TWO_CALLS"):
    # One message asking for two tools, which it then runs one by one.
    out({"type": "assistant", "parent_tool_use_id": None, "message": {"model": "claude-stand-in", "content": [
        {"type": "tool_use", "id": "toolu_stand_in_a", "name": "mcp__mafold__Bash", "input": {"command": "echo a"}},
        {"type": "tool_use", "id": "toolu_stand_in_b", "name": "mcp__mafold__Bash", "input": {"command": "echo b"}}]}})
    ev({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}})
    ev({"type": "message_stop"})
    for i, cmd in ((3, "echo a"), (4, "echo b")):
        send({"jsonrpc": "2.0", "id": i, "method": "tools/call", "params": {"name": "Bash", "arguments": {"command": cmd}}})
        json.loads(mcp.readline())
    ev({"type": "message_start", "message": {"model": "claude-stand-in", "usage": {"input_tokens": 30, "output_tokens": 1}}})
    ev({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Both ran."}})
    ev({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}})
    out({"type": "result", "subtype": "success", "is_error": False, "session_id": session, "result": "Both ran."})
    sys.stdin.read()
    sys.exit(0)
ev({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Looking."}})
send({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "Bash", "arguments": {"command": "echo hi"}}})
time.sleep(0.3)
out({"type": "assistant", "parent_tool_use_id": None, "message": {"model": "claude-stand-in", "content": [
    {"type": "tool_use", "id": "toolu_stand_in_1", "name": "mcp__mafold__Bash", "input": {"command": "echo hi"}}]}})
ev({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}})
ev({"type": "message_stop"})
result = json.loads(mcp.readline())["result"]
said = "".join(c.get("text", "") for c in result["content"]).strip()
note({"result": result})
ev({"type": "message_start", "message": {"model": "claude-stand-in", "usage": {"input_tokens": 30, "output_tokens": 1}}})
ev({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "It said: " + said}})
ev({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}})
out({"type": "result", "subtype": "success", "is_error": False, "session_id": session, "result": "It said: " + said})
sys.stdin.read()
"#;

    /// The engine's plumbing against the stand-in: the call is taken only
    /// after the words in front of it, matched to Claude's own tool use, run
    /// by the kernel's Bash in the folder and handed back over the bridge;
    /// `claude` is started with the harness's prompt, only the kernel's
    /// tools, none of its own, our name and no API key; and the next turn
    /// goes on in the session it named.
    #[cfg(unix)]
    #[tokio::test]
    async fn on_claude_code_a_turn_steps_through_the_bridge_and_goes_on_next_turn() {
        use std::os::unix::fs::PermissionsExt;
        let work = folder("cc-stand-in");
        let bin = work.with_extension("claude");
        std::fs::write(&bin, STAND_IN_CLAUDE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = work.with_extension("log");
        let env = || {
            vec![
                (crate::fuel::FUEL_ENV.to_string(), crate::fuel::CLAUDE.to_string()),
                ("MAFOLD_DEV_CLAUDE_BIN".to_string(), bin.to_string_lossy().into_owned()),
                ("STAND_IN_LOG".to_string(), log.to_string_lossy().into_owned()),
                ("ANTHROPIC_API_KEY".to_string(), "sk-ant-never-passed-on".to_string()),
            ]
        };
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let mut first = turn(&work, &surface(&tag), None, "what does echo hi print?", env());
        first.model = Some("opus".into());
        let (out, events) = drive(first).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        let at = |want: &dyn Fn(&AgentEvent) -> bool| events.iter().position(|e| want(e));
        let looking = at(&|e| matches!(e, AgentEvent::Text(t) if t == "Looking.")).expect("the words before the call");
        let called = at(&|e| matches!(e, AgentEvent::ToolCall { id, name, .. } if id == "toolu_stand_in_1" && name == "Bash"))
            .expect("Claude's own tool use, run by the kernel");
        assert!(looking < called, "the words in front of the call come first: {events:?}");
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { text, .. } if text.trim() == "hi")), "{events:?}");
        let said: String = events.iter().filter_map(|e| match e { AgentEvent::Text(t) => Some(t.as_str()), _ => None }).collect();
        assert_eq!(said, "Looking.It said: hi");

        let lines: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let argv: Vec<&str> = lines[0]["argv"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        let flag = |name: &str| argv.iter().position(|a| *a == name).and_then(|i| argv.get(i + 1)).copied();
        assert_eq!(flag("--tools"), Some(""), "none of Claude Code's own tools: {argv:?}");
        assert_eq!(flag("--allowedTools"), Some("mcp__mafold"));
        assert_eq!(flag("--setting-sources"), Some(""));
        assert_eq!(flag("--model"), Some("opus"));
        assert!(argv.contains(&"--strict-mcp-config") && argv.contains(&"--disable-slash-commands") && !argv.contains(&"--resume"), "{argv:?}");
        assert_eq!(lines[0]["entry"], "claude-mafold");
        assert_eq!(lines[0]["api_key"], false, "the login is the fuel, never a key");
        let system = lines[0]["system"].as_str().unwrap();
        assert!(system.starts_with("You are Probe."), "{system}");
        assert!(system.contains("serving replies with the model `opus` on your owner's Claude Code plan."), "{system}");
        assert_eq!(lines[1]["opening"]["message"]["content"][0]["text"], "what does echo hi print?");
        assert_eq!(lines[2]["tools"], json!(["Bash", "Read", "Write", "Edit", "Glob", "Grep"]));
        let session = out.session.clone().expect("a session");
        let sessions = work.with_extension("home").join(format!("probe-{tag}")).join("sessions");
        let kept = std::fs::read_to_string(sessions.join(format!("{session}.claude"))).unwrap();
        assert!(kept.starts_with("11111111-2222-4333-8444-555555555555\n"), "{kept}");

        let mut second = turn(&work, &surface(&tag), Some(session.clone()), "still there?", env());
        second.model = Some("opus".into());
        let (out2, events2) = drive(second).await;
        assert!(out2.error.is_none(), "{:?}", out2.error);
        assert!(events2.iter().any(|e| matches!(e, AgentEvent::Text(t) if t == "Still here.")), "{events2:?}");
        let lines: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let second = lines.iter().rev().find(|l| l.get("argv").is_some()).unwrap();
        let argv: Vec<&str> = second["argv"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        let resumed = argv.iter().position(|a| *a == "--resume").and_then(|i| argv.get(i + 1)).copied();
        assert_eq!(resumed, Some("11111111-2222-4333-8444-555555555555"), "{argv:?}");
        let opening = lines.iter().rev().find_map(|l| l.get("opening")).unwrap();
        assert_eq!(opening["message"]["content"], json!([{ "type": "text", "text": "still there?" }]), "a resumed session isn't told it again");

        // Another prompt (the model changed, and with it the model line): not
        // the old session, which keeps its first prompt — a new one, told the
        // conversation so far.
        let (out3, _) = drive(turn(&work, &surface(&tag), Some(session), "and now?", env())).await;
        assert!(out3.error.is_none(), "{:?}", out3.error);
        let lines: Vec<Value> = std::fs::read_to_string(&log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let third = lines.iter().rev().find(|l| l.get("argv").is_some()).unwrap();
        assert!(!third["argv"].as_array().unwrap().iter().any(|a| a == "--resume"), "{third}");
        let opening = lines.iter().rev().find_map(|l| l.get("opening")).unwrap();
        let told = opening["message"]["content"][0]["text"].as_str().unwrap();
        assert!(told.contains("<user>\nwhat does echo hi print?\n</user>") && told.contains("It said: hi"), "{told}");
        assert_eq!(opening["message"]["content"][1]["text"], "and now?");
    }

    /// One message asking for two tools, which `claude` runs one by one: two
    /// steps of the kernel's on one request. The reply's card still adds up —
    /// two requests and both requests' tokens — where it used to have no
    /// token count at all and one request too many (11-13, on dev: Read and
    /// Bash asked for in one message).
    #[cfg(unix)]
    #[tokio::test]
    async fn on_claude_code_two_tools_in_one_message_are_one_request_on_the_card() {
        use std::os::unix::fs::PermissionsExt;
        let work = folder("cc-two-calls");
        let bin = work.with_extension("claude");
        std::fs::write(&bin, STAND_IN_CLAUDE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = vec![
            (crate::fuel::FUEL_ENV.to_string(), crate::fuel::CLAUDE.to_string()),
            ("MAFOLD_DEV_CLAUDE_BIN".to_string(), bin.to_string_lossy().into_owned()),
            ("STAND_IN_LOG".to_string(), work.with_extension("log").to_string_lossy().into_owned()),
            ("STAND_IN_TWO_CALLS".to_string(), "1".to_string()),
        ];
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (out, events) = drive(turn(&work, &surface(&tag), None, "run both", env)).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        let ran: Vec<&str> = events.iter().filter_map(|e| match e { AgentEvent::ToolResult { text, .. } => Some(text.trim()), _ => None }).collect();
        assert_eq!(ran, ["a", "b"], "{events:?}");
        let card = events.iter().rev().find_map(|e| match e { AgentEvent::Stats(s) => Some(s.clone()), _ => None }).expect("the card's stats");
        assert_eq!(card.model_requests, Some(2), "{card:?}");
        assert_eq!((card.input_tokens, card.output_tokens, card.total_tokens), (Some(40), Some(14), Some(54)), "{card:?}");
        assert_eq!(card.tool_calls, Some(2));
        assert_eq!(card.model.as_deref(), Some("claude-stand-in"));
    }

    /// The real thing, on this computer's own Claude Code login: a Mafold
    /// Harness turn on Claude Code runs the kernel's Bash in its folder
    /// through the bridge, says what it printed, and the next turn goes on in
    /// the same Claude Code session. Spends the plan, so by hand:
    /// `cargo build && cargo test a_real_claude -- --ignored --nocapture`
    /// (the debug `mafold` beside the test binary is the bridge).
    #[tokio::test]
    #[ignore = "runs the real claude on this computer's login"]
    async fn a_real_claude_turn_runs_a_harness_tool_and_goes_on_next_turn() {
        let exe = crate::platform::current_exe().unwrap().parent().unwrap().parent().unwrap().join("mafold");
        assert!(exe.exists(), "build the debug mafold first: {}", exe.display());
        let work = folder("cc");
        std::fs::write(work.join("note.txt"), "the parcel is in locker 42").unwrap();
        let env = || {
            vec![
                (crate::fuel::FUEL_ENV.to_string(), crate::fuel::CLAUDE.to_string()),
                ("MAFOLD_DEV_BRIDGE_EXE".to_string(), exe.to_string_lossy().into_owned()),
            ]
        };
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let mut first = turn(&work, &surface(&tag), None, "Read note.txt in your folder with your Read tool and tell me the locker number. Then run `echo bridge-$((6*7))` with Bash and quote what it printed.", env());
        first.model = Some("sonnet".into());
        let (out, events) = drive(first).await;
        for e in &events {
            println!("{e:?}");
        }
        assert!(out.error.is_none(), "{:?}", out.error);
        let session = out.session.clone().expect("a session");
        let calls: Vec<String> = events.iter().filter_map(|e| match e { AgentEvent::ToolCall { name, .. } => Some(name.clone()), _ => None }).collect();
        assert!(calls.iter().any(|c| c == "Read") && calls.iter().any(|c| c == "Bash"), "the kernel's tools ran: {calls:?}");
        let results: String = events.iter().filter_map(|e| match e { AgentEvent::ToolResult { text, .. } => Some(text.as_str()), _ => None }).collect();
        assert!(results.contains("locker 42") && results.contains("bridge-42"), "{results}");
        let said: String = events.iter().filter_map(|e| match e { AgentEvent::Text(t) => Some(t.as_str()), _ => None }).collect();
        assert!(said.contains("42") && said.contains("bridge-42"), "{said}");
        let home = work.with_extension("home");
        let sessions = home.join(format!("probe-{tag}")).join("sessions");
        let kept = std::fs::read_to_string(sessions.join(format!("{session}.claude"))).expect("the Claude Code session is kept");

        let mut second = turn(&work, &surface(&tag), Some(session.clone()), "What did the echo print, exactly? Answer from memory, no tools.", env());
        second.model = Some("sonnet".into());
        let (out2, events2) = drive(second).await;
        assert!(out2.error.is_none(), "{:?}", out2.error);
        let said2: String = events2.iter().filter_map(|e| match e { AgentEvent::Text(t) => Some(t.as_str()), _ => None }).collect();
        println!("second turn: {said2}");
        assert!(said2.contains("bridge-42"), "{said2}");
        assert_eq!(std::fs::read_to_string(sessions.join(format!("{session}.claude"))).unwrap(), kept, "same Claude Code session, same prompt");
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
    /// On Go the model is named as Go's catalogue names it; one the kept list
    /// doesn't know is named by its id.
    #[test]
    fn the_model_is_named_by_the_catalogue() {
        let catalogue = [json!({ "id": "beam-free", "name": "Beam Free" })];
        let said = model_line("beam-free", &catalogue, "OpenCode Go");
        assert!(said.starts_with("You are currently serving replies with the model `beam-free` (Beam Free) on OpenCode Go."), "{said}");
        assert!(said.contains("Your owner changes it"), "{said}");
        let said = model_line("glm-5.3", &catalogue, "OpenCode Go");
        assert!(said.starts_with("You are currently serving replies with the model `glm-5.3` on OpenCode Go. "), "{said}");
    }

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
