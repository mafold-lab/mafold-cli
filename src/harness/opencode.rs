//! OpenCode harness (Mafold Harness) — runs the pinned OpenCode build
//! (`super::opencode_pin`) on the owner's Mafold wallet and normalizes its
//! server's event stream into [`AgentEvent`]s, so its turns render as the same
//! cards every other harness produces.
//!
//! What shapes this impl:
//! - **One `opencode serve` per turn.** The wallet key in [`Turn::env`] was
//!   minted for this one reply (`mintReplyKey`) and dies when the reply ends,
//!   so the process holding it ends with the turn too. Nothing is pooled or
//!   prewarmed: a warm process would be holding a dead key.
//! - **A server, not a stream on stdout.** OpenCode's headless surface is an
//!   HTTP server: a session, `prompt_async`, and one SSE stream (`/event`) for
//!   everything that happens. It listens on loopback, where every local user
//!   can reach it, and its tools run bash — so each turn's server gets a random
//!   password of its own (`OPENCODE_SERVER_PASSWORD`, basic auth).
//! - **The wallet or nothing.** The one provider it can see is `mafold`, the
//!   router's OpenAI-compatible door with the reply's key; `enabled_providers`
//!   holds it to that, and other providers' credentials are scrubbed from its
//!   environment. A turn without a wallet key is refused, never run on
//!   something else.
//! - **Its own home.** `XDG_*` point at `~/.mafold/runtimes/opencode/home/<bot>`,
//!   so the user's own OpenCode config, plugins and logins never load, and
//!   project config is off (a repo's `opencode.json` could send the key
//!   elsewhere). Sessions live there across turns, which is how a conversation
//!   resumes on the next turn's fresh server.
//! - **Headless means nothing may ask.** Every permission OpenCode would stop
//!   to ask about is allowed, and its ask-the-user tool is off; an ask that
//!   still arrives is answered at once instead of holding the reply forever.
//! - **Its $ figure is not the bill.** The wallet bills by the router's ledger;
//!   OpenCode's own estimate is never reported as a cost.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use mafold_transcript::RunStats;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use super::opencode_pin;
use super::{AgentEvent, CommandOutcome, Harness, Turn, TurnOutcome};
use crate::client::Client;

pub struct OpenCode;

/// What a wallet turn without its key answers. The daemon puts the key in
/// [`Turn::env`] for a wallet reply and only then; without it there is
/// nothing this agent may run on.
pub const NO_WALLET_KEY: &str = "This agent runs on the wallet, but the reply has no wallet key, so nothing ran.";

/// The provider id OpenCode knows the wallet by.
const PROVIDER: &str = "mafold";

/// The context window OpenCode is told each wallet model has. Lower than
/// DeepSeek's own on purpose: it is where OpenCode compacts, and every model
/// call re-sends the whole context on the owner's wallet.
const CONTEXT_TOKENS: u64 = 128_000;

/// The output cap OpenCode asks for per call (`max_tokens`) — OpenCode's own
/// ceiling. The router holds input plus this much before each call, so a
/// bigger one would turn away replies a small wallet could pay for.
const OUTPUT_TOKENS: u64 = 32_000;

/// How long `opencode serve` may take to say where it listens.
const STARTUP: Duration = Duration::from_secs(90);

/// Ordinary requests to the turn's own server.
const REQUEST: Duration = Duration::from_secs(30);

/// After Stop: how long the aborted reply gets to settle before its server is
/// killed anyway.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// OpenCode retries a refused model call by itself, with backoff and without
/// end (a 429, a 5xx). Past this many tries the reply is ended with what the
/// router said instead of retrying for as long as someone waits.
const RETRY_GIVE_UP: u64 = 6;

/// SSE reconnects one turn may make before its stream counts as lost.
const RECONNECTS: usize = 5;

/// Environment prefixes another provider's credentials live under, plus any
/// OpenCode setting of the user's and any wallet value the daemon itself
/// inherited: the child gets the wallet from [`Turn::env`] and nowhere else.
/// Everything else (PATH, HOME, tool logins) stays, so the agent's tools work.
const SCRUB: &[&str] = &["OPENAI_", "ANTHROPIC_", "DEEPSEEK_", "GEMINI_", "OPENROUTER_", "OPENCODE_", "MAFOLD_WALLET_"];

#[async_trait]
impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    /// The pinned build, never an `opencode` on PATH.
    fn available(&self) -> bool {
        opencode_pin::installed()
    }

    /// Yes: the session exists (and is named) before the prompt is sent.
    fn names_session_first(&self) -> bool {
        true
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        let pin = match opencode_pin::Source::here() {
            Ok(pin) => pin,
            Err(e) => return Ok(TurnOutcome { error: Some(set_up_failed(&e)), ..Default::default() }),
        };
        run_on(turn, sink, Place::Pinned(pin)).await
    }

    fn discover(&self, _workdir: &str) -> Value {
        // OpenCode's own commands are a TUI feature; forwarded, a `/name` is
        // just prompt text. The daemon's control commands are added apart.
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

    /// The pin's, when it is installed (`opencode_pin::installed`).
    async fn cli_version(&self) -> String {
        if opencode_pin::installed() {
            opencode_pin::VERSION.to_string()
        } else {
            String::new()
        }
    }
}

/// Which OpenCode a turn runs, and where agents' homes are.
enum Place {
    /// The pinned build, installed first if it isn't here yet. This
    /// computer's ([`opencode_pin::Source::here`]), or a test's scratch one.
    Pinned(opencode_pin::Source),
    /// A given binary and homes root — the tests' door.
    #[cfg_attr(not(test), allow(dead_code))]
    At { bin: PathBuf, root: PathBuf },
}

/// How often a reply waiting on the pinned build's install shows it is alive
/// — well inside the generating card's 30 s keepalive.
const INSTALL_TICK: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(5) };

/// While the pinned build installs for a reply: no process serves the turn
/// yet, so nothing else beats its generating card, which read «No signal»
/// two minutes into a slow download and invited the Stop that threw it away.
/// So it beats from here (`Pulse`), and the reply says how far the download
/// is once per quarter of it. Never ends; dropped with the install.
async fn installing(sink: &UnboundedSender<AgentEvent>, root: &Path) {
    let mut told = 0;
    loop {
        tokio::time::sleep(INSTALL_TICK).await;
        let _ = sink.send(AgentEvent::Pulse { chars: 0, tokens: None });
        if let Some(p) = opencode_pin::progress_in(root).filter(|p| p.step() > told) {
            told = p.step();
            let _ = sink.send(AgentEvent::Notice(format!("Installing OpenCode {} — {}", opencode_pin::VERSION, p.mb())));
        }
    }
}

/// A turn's words for an install of the pinned build that didn't work.
fn set_up_failed(e: &anyhow::Error) -> String {
    format!("couldn't set up Mafold Harness (OpenCode {}) on this computer: {e:#}", opencode_pin::VERSION)
}

async fn run_on(turn: Turn, sink: UnboundedSender<AgentEvent>, place: Place) -> Result<TurnOutcome> {
    // Effort and thinking: the wallet's models are run as the router serves
    // them, with no dial wired yet. No ask hook and no steer hook either, so
    // a mid-turn message arrives as the follow-up turn (`can_steer` false).
    let Turn {
        prompt,
        conv,
        surface,
        draft,
        workdir,
        session,
        model,
        effort: _,
        thinking: _,
        cancel,
        system,
        ask_file: _,
        steer_file: _,
        env,
        mount,
        skill_plugins: _,
        proc,
    } = turn;
    if !Path::new(&workdir).is_dir() {
        bail!("working directory does not exist: {workdir} — check --workdir");
    }
    let Some(wallet) = Wallet::from_env(&env, dev_base(&env)) else {
        return Ok(TurnOutcome { error: Some(NO_WALLET_KEY.into()), ..Default::default() });
    };
    let model = match pick_model(model.as_deref(), &wallet.models) {
        Ok(m) => m,
        Err(why) => return Ok(TurnOutcome { error: Some(why), ..Default::default() }),
    };
    let (bin, root, pin) = match place {
        Place::At { bin, root } => (bin, root, None),
        Place::Pinned(pin) => {
            let bin = if super::launchable(&pin.binary()) {
                pin.binary()
            } else {
                let _ = sink.send(AgentEvent::Notice(format!(
                    "Setting up Mafold Harness (OpenCode {}) on this computer first — a one-time download of about 80 MB.",
                    opencode_pin::VERSION
                )));
                tokio::select! {
                    r = pin.ensure() => match r {
                        Ok(b) => b,
                        Err(e) => return Ok(TurnOutcome { error: Some(set_up_failed(&e)), ..Default::default() }),
                    },
                    _ = cancel.notified() => return Ok(TurnOutcome { stopped: true, ..Default::default() }),
                    _ = installing(&sink, &pin.root) => unreachable!("never ends"),
                }
            };
            (bin, pin.root.clone(), Some(pin))
        }
    };
    let spec = Spec {
        home: opencode_pin::agent_home_in(&root, &agent_name(&surface)),
        program: bin,
        pin,
        workdir,
        prompt,
        system,
        model,
        wallet,
        seat: env,
        conv,
        surface,
        reads: mount.reads,
        draft,
    };
    run_turn(&spec, session.as_deref(), &cancel, &sink, &proc).await
}

/// The wallet a turn runs on, as the daemon's fuel step handed it over.
#[derive(Clone, PartialEq)]
struct Wallet {
    key: String,
    /// The router's OpenAI-compatible base (ends in `/v1`).
    base: String,
    /// What the key may run, in the router's order.
    models: Vec<String>,
}

/// Written by hand so the key never reaches a log line: the first `{:?}` of
/// a wallet (or of something holding one) in a trace or an error would
/// otherwise print a live key to the owner's wallet — as `client::ReplyKey`
/// is for the same reason.
impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("key", &"<redacted>")
            .field("base", &self.base)
            .field("models", &self.models)
            .finish()
    }
}

impl Wallet {
    /// All three of key, base and models from `env` — or none. `dev_base`
    /// stands in for the base when given (debug builds, [`dev_base`]).
    fn from_env(env: &[(String, String)], dev_base: Option<String>) -> Option<Self> {
        let get = |k: &str| {
            env.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty())
        };
        let key = get("MAFOLD_WALLET_KEY")?;
        let base = get("MAFOLD_WALLET_BASE")?;
        let models: Vec<String> = get("MAFOLD_WALLET_MODELS")?
            .split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        if models.is_empty() {
            return None;
        }
        let base = dev_base.unwrap_or(base).trim_end_matches('/').to_string();
        Some(Self { key, base, models })
    }
}

/// Debug builds only: `MAFOLD_DEV_WALLET_BASE` (the turn's, else the
/// daemon's) points the wallet at a local router. A release build never
/// sends a reply key anywhere but the base the API minted it for.
fn dev_base(env: &[(String, String)]) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    env.iter()
        .rev()
        .find(|(k, _)| k == "MAFOLD_DEV_WALLET_BASE")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("MAFOLD_DEV_WALLET_BASE").ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The model this turn runs: the chat's pick when the wallet runs it, the
/// wallet's first when nothing was picked — and a refusal, not a substitute,
/// when the pick is a model this reply's key can't run.
fn pick_model(wanted: Option<&str>, models: &[String]) -> std::result::Result<String, String> {
    let wanted = wanted.map(str::trim).filter(|m| !m.is_empty());
    let Some(w) = wanted else {
        return Ok(models[0].clone());
    };
    let w = w.strip_prefix(&format!("{PROVIDER}/")).unwrap_or(w);
    match models.iter().find(|m| m.eq_ignore_ascii_case(w)) {
        Some(m) => Ok(m.clone()),
        None => Err(format!(
            "This agent is set to `{w}`, which the wallet doesn't run — it runs {}. \
             Pick one of those (Customize ▸ Model, or `/model` in this chat); nothing ran on another model.",
            models.join(", ")
        )),
    }
}

/// Whose OpenCode home a turn uses: the bot part of its surface tag
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

/// The config OpenCode gets in `OPENCODE_CONFIG_CONTENT`. The key is never in
/// it — `{env:MAFOLD_WALLET_KEY}` — so the config can be logged as it is.
fn config(base: &str, models: &[String], model: &str) -> Value {
    let mut list = Map::new();
    for m in models {
        list.insert(m.clone(), json!({ "name": m, "limit": { "context": CONTEXT_TOKENS, "output": OUTPUT_TOKENS } }));
    }
    let id = format!("{PROVIDER}/{model}");
    json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
        // Its per-step git snapshot of the workdir backs a revert Mafold has
        // no door to — on a large repo, a copy of it in the agent's home.
        "snapshot": false,
        "model": id,
        "small_model": id,
        "enabled_providers": [PROVIDER],
        "provider": {
            PROVIDER: {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Mafold Wallet",
                "options": { "baseURL": base, "apiKey": "{env:MAFOLD_WALLET_KEY}" },
                "models": list,
            }
        },
        // Each one OpenCode asks about by default (a path outside the
        // workdir, a `.env` read, the same call three times) would hold a
        // headless reply forever. `"*": "allow"` would also bring back the
        // ask-the-user tool, so they are named one by one (measured, 1.19.5).
        "permission": {
            "edit": "allow",
            "bash": "allow",
            "webfetch": "allow",
            "read": "allow",
            "external_directory": "allow",
            "doom_loop": "allow",
            "question": "deny",
        },
        "tools": { "question": false },
    })
}

/// Which inherited variables the child must not see ([`SCRUB`]).
fn scrubbed<I: IntoIterator<Item = String>>(names: I) -> Vec<String> {
    names
        .into_iter()
        .filter(|n| {
            let up = n.to_ascii_uppercase();
            SCRUB.iter().any(|p| up.starts_with(p))
        })
        .collect()
}

/// What OpenCode itself is told: where its home is, its config, its
/// password, and to fetch nothing on its own.
fn opencode_env(home: &Path, config: &Value, password: &str) -> Vec<(String, String)> {
    let dir = |d: &str| home.join(d).to_string_lossy().into_owned();
    let mut env = vec![
        ("XDG_CONFIG_HOME".to_string(), dir("config")),
        ("XDG_DATA_HOME".to_string(), dir("data")),
        ("XDG_CACHE_HOME".to_string(), dir("cache")),
        ("XDG_STATE_HOME".to_string(), dir("state")),
        // OpenCode's own idea of home, apart from HOME (which the agent's
        // tools keep). The XDG folders alone left `~/.opencode` in play: its
        // `opencode.json` reached the model's system prompt, and the server
        // wrote a `.gitignore` there (measured, 1.19.5).
        ("OPENCODE_TEST_HOME".to_string(), home.to_string_lossy().into_owned()),
        ("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string()),
        ("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string()),
    ];
    for flag in [
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "OPENCODE_DISABLE_LSP_DOWNLOAD",
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
        "OPENCODE_DISABLE_CLAUDE_CODE",
        "OPENCODE_DISABLE_PROJECT_CONFIG",
        // No plugin from anywhere but the binary itself.
        "OPENCODE_PURE",
    ] {
        env.push((flag.to_string(), "1".to_string()));
    }
    env
}

/// Everything one turn needs, resolved.
struct Spec {
    program: PathBuf,
    /// Where `program` came from, when it is the pinned build — what puts it
    /// right if it won't start (`opencode_pin::repair_in`).
    pin: Option<opencode_pin::Source>,
    home: PathBuf,
    workdir: String,
    prompt: String,
    system: Option<String>,
    model: String,
    wallet: Wallet,
    /// [`Turn::env`], applied verbatim (the wallet key rides in it).
    seat: Vec<(String, String)>,
    conv: String,
    surface: String,
    reads: crate::chat::ReadsFor,
    draft: String,
}

async fn run_turn(
    s: &Spec,
    session: Option<&str>,
    cancel: &Arc<Notify>,
    sink: &UnboundedSender<AgentEvent>,
    proc: &super::TurnProc,
) -> Result<TurnOutcome> {
    for d in ["config", "data", "cache", "state"] {
        std::fs::create_dir_all(s.home.join(d)).with_context(|| format!("couldn't create {}", s.home.display()))?;
    }
    seed_config(&s.home).with_context(|| format!("couldn't prepare {}", s.home.display()))?;
    // A fresh home's database is created by the first server to start in it,
    // and two creating it at once kill one of them (measured, 1.19.5: two
    // turns of one agent starting together). Held until this turn's server
    // has used it; after that any number share it.
    let lock = start_lock(&s.home).await;
    let stopped = || Ok(TurnOutcome { stopped: true, ..Default::default() });
    let started = tokio::select! {
        r = Server::start(s, proc) => r,
        _ = cancel.notified() => return stopped(),
    };
    let mut server = match (started, &s.pin) {
        (Ok(server), _) => server,
        // The pinned build that won't start may be a damaged one (cut short by
        // a power loss): nothing else would ever put it right, and every reply
        // would fail here. Checked against the digest its install recorded;
        // installed again once when it doesn't match, then tried once more.
        (Err(e), Some(pin)) => {
            let bin = pin.binary();
            if tokio::task::spawn_blocking(move || opencode_pin::intact(&bin)).await.unwrap_or(false) {
                // The pinned bytes: a new copy wouldn't start any better.
                return Err(e);
            }
            let _ = sink.send(AgentEvent::Notice(format!(
                "OpenCode {} on this computer wouldn't start, and it isn't the build Mafold installed — installing it again.",
                opencode_pin::VERSION
            )));
            let repaired = tokio::select! {
                r = pin.repair() => r,
                _ = cancel.notified() => return stopped(),
            };
            if let Err(again) = repaired {
                return Err(e.context(format!("it isn't the build Mafold installed, and installing it again failed: {again:#}")));
            }
            tokio::select! {
                r = Server::start(s, proc) => r?,
                _ = cancel.notified() => return stopped(),
            }
        }
        (Err(e), None) => return Err(e),
    };
    let out = drive(&server, s, session, cancel, sink, lock).await;
    server.stop().await;
    out
}

/// Everything between a listening server and the end of the turn.
async fn drive(
    server: &Server,
    s: &Spec,
    session: Option<&str>,
    cancel: &Arc<Notify>,
    sink: &UnboundedSender<AgentEvent>,
    lock: Option<std::fs::File>,
) -> Result<TurnOutcome> {
    let mut feed = server.subscribe().await?;
    let sid = server.session(session).await?;
    drop(lock);
    let _ = sink.send(AgentEvent::Session(sid.clone()));
    let mut map = Mapper::new(sink, &sid, &s.model);
    let _ = sink.send(AgentEvent::Stats(RunStats { model: Some(s.model.clone()), ..Default::default() }));

    let mut body = json!({
        "parts": [{ "type": "text", "text": s.prompt }],
        "model": { "providerID": PROVIDER, "modelID": s.model },
    });
    if let Some(sys) = s.system.as_deref().filter(|t| !t.trim().is_empty()) {
        body["system"] = json!(sys);
    }
    let started = Instant::now();
    let (status, answer) = server.post(&format!("/session/{sid}/prompt_async"), &body).await?;
    if !(200..300).contains(&status) {
        bail!("OpenCode refused the prompt ({status}): {}", answer);
    }

    let mut exit = super::ExitWatch::new(server.pid);
    let mut stop_at: Option<tokio::time::Instant> = None;
    let mut asked_stop = false;
    let mut error: Option<String> = None;
    let mut lost = 0usize;
    let ended = loop {
        let item = tokio::select! {
            item = feed.next() => item,
            _ = cancel.notified(), if !asked_stop => {
                asked_stop = true;
                stop_at = Some(tokio::time::Instant::now() + STOP_GRACE);
                server.abort(&sid).await;
                continue;
            }
            _ = async { tokio::time::sleep_until(stop_at.unwrap()).await }, if stop_at.is_some() => break End::Killed,
            _ = exit.gone() => {
                error.get_or_insert_with(|| super::EXITED_MID_TURN.to_string());
                break End::Exited;
            }
        };
        let ev = match item {
            Feed::Event(ev) => ev,
            Feed::Closed(why) => {
                // The stream went, the server didn't (`exit` would say): ask
                // it where the reply is and pick the stream up again.
                lost += 1;
                if lost > RECONNECTS {
                    error.get_or_insert_with(|| format!("lost OpenCode's event stream ({why})"));
                    break End::Lost;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                match server.subscribe().await {
                    Ok(f) => feed = f,
                    Err(_) => continue,
                }
                if !server.busy(&sid).await.unwrap_or(true) {
                    break End::Idle;
                }
                continue;
            }
        };
        match map.event(&ev) {
            Signal::None => {}
            Signal::Idle => break End::Idle,
            Signal::Error(e) => {
                error.get_or_insert(e);
            }
            Signal::Aborted => {
                if !asked_stop {
                    error.get_or_insert_with(|| "OpenCode aborted the reply".to_string());
                }
            }
            Signal::Retry { attempt, message } => {
                if attempt >= RETRY_GIVE_UP && error.is_none() && !asked_stop {
                    error = Some(format!("the wallet's router kept refusing this reply ({attempt} tries): {message}"));
                    stop_at = Some(tokio::time::Instant::now() + STOP_GRACE);
                    server.abort(&sid).await;
                }
            }
            Signal::Ask { session, id } => {
                println!("· opencode asked permission {id} mid-turn — answered once (Mafold allows every tool)");
                let _ = server
                    .post(&format!("/session/{session}/permissions/{id}"), &json!({ "response": "once" }))
                    .await;
            }
        }
    };
    let elapsed = started.elapsed();
    // Whatever the stream didn't deliver (a dropped event, the text part that
    // lands after `idle`) is in the session itself — when the server still
    // answers: one that had to be killed, or is gone, isn't asked.
    if matches!(ended, End::Idle | End::Lost) {
        if let Ok(Ok((200, msgs))) =
            tokio::time::timeout(REQUEST, server.get(&format!("/session/{sid}/message"))).await
        {
            // An error the stream lost (it ended while the stream was being
            // picked up again) is on the message it ended.
            if let Some(e) = map.reconcile(&msgs) {
                error.get_or_insert(e);
            }
        }
    }
    let produced = map.produced;
    let session = Some(sid);
    // The person's Stop wins over whatever went wrong on the way to it.
    if asked_stop {
        return Ok(TurnOutcome { produced, stopped: true, session, error: None, limit: None });
    }
    if let Some(e) = error {
        return Ok(TurnOutcome { produced, stopped: false, session, error: Some(e), limit: None });
    }
    let tokens = map.stats.total_tokens;
    let _ = sink.send(AgentEvent::Stats(map.stats.clone()));
    let _ = sink.send(AgentEvent::Done { duration_ms: Some(elapsed.as_secs_f64() * 1000.0), cost_usd: None, tokens });
    Ok(TurnOutcome { produced, stopped: false, session, error: None, limit: None })
}

enum End {
    Idle,
    Killed,
    Exited,
    Lost,
}

/// OpenCode installs its plugin SDK (`@opencode-ai/plugin`) into every config
/// folder it reads, in the background, whenever the folder has no
/// `node_modules`: a 27 MB npm metadata fetch into the user's own `~/.npm` —
/// restarted every turn, because the turn's end kills the server before it
/// finishes — for plugins this agent never loads (`OPENCODE_PURE`, none
/// configured). It skips the install when the folder's lockfile already lists
/// the package (measured, 1.19.5), so the agent's config folder starts with
/// one.
fn seed_config(home: &Path) -> std::io::Result<()> {
    let dir = home.join("config").join("opencode");
    std::fs::create_dir_all(dir.join("node_modules"))?;
    let lock = dir.join("package-lock.json");
    if !lock.exists() {
        let deps = json!({ "@opencode-ai/plugin": opencode_pin::VERSION });
        std::fs::write(&lock, json!({ "lockfileVersion": 3, "packages": { "": { "dependencies": deps } } }).to_string())?;
    }
    Ok(())
}

/// Exclusive use of `home` while a server starts in it; `None` (start anyway)
/// when the lock can't be had in time.
async fn start_lock(home: &Path) -> Option<std::fs::File> {
    let path = home.join(".start.lock");
    let take = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(windows)]
        crate::platform::lock_file_exclusive(&f)?;
        Ok(f)
    });
    match tokio::time::timeout(STARTUP, take).await {
        Ok(Ok(Ok(f))) => Some(f),
        _ => None,
    }
}

/// Linux: the turn's server dies with the daemon that started it — however
/// the daemon dies (kill -9, the OOM killer, a crash), where no shutdown
/// handler runs to end it. `opencode serve` is an HTTP server that never
/// reads its stdin, so a pipe the daemon holds is no lifeline (measured,
/// 1.19.5: still serving 15 s after its stdin pipe was closed). Left behind,
/// it finishes the reply on the wallet's key into a session nobody reads,
/// then idles at ~240 MB for good, and the next daemon can't resume the turn
/// while it lives (`agent::pick_up`).
///
/// SIGKILL, not SIGTERM: nobody is left to follow a SIGTERM up, and the key
/// it holds spends the owner's wallet until it stops. Its SQLite home is
/// crash-safe. The kernel sends this when the THREAD that forked the server
/// ends; that is a tokio worker, which lives as long as the daemon's runtime
/// (this crate never calls `block_in_place`, which could retire one).
#[cfg(target_os = "linux")]
fn die_with_daemon(cmd: &mut tokio::process::Command) {
    let daemon = std::process::id();
    // SAFETY: between fork and exec only async-signal-safe calls are made
    // (prctl, getppid), and the errors built allocate nothing.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The daemon died between the fork and the line above: the
            // signal was missed, and this server must not start.
            if libc::getppid() as u32 != daemon {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// Elsewhere there is no such signal: a server a daemon left behind is ended
/// by the next one instead ([`end_orphan`]).
#[cfg(not(target_os = "linux"))]
fn die_with_daemon(_cmd: &mut tokio::process::Command) {}

/// At a daemon's start, for each process the turns of the one before it were
/// running in (`drafts::Outbox::take_orphans`, `agent::pick_up`): when it
/// still runs and is one of our servers, end it and wait (≤ 5 s) for it to
/// go. Waiting for a server to exit by itself — what a restart did — waits
/// forever, and gave the reply up. True when one was ended.
pub fn end_orphan(pid: u32, start: Option<u64>) -> bool {
    end_orphan_under(&opencode_pin::root(), pid, start)
}

/// [`end_orphan`] for the pinned builds under `root`. Ours = the very process
/// the journal named (its start matches — a pid is soon someone else's,
/// above all on Windows) running a binary from under `root`. Anything else —
/// another runtime's process, the user's own OpenCode, a stranger with a
/// reused pid — is left alone.
fn end_orphan_under(root: &Path, pid: u32, start: Option<u64>) -> bool {
    let Some(start) = start else { return false };
    if !crate::platform::pid_running(pid) || crate::platform::start_token(pid) != Some(start) {
        return false;
    }
    let Some(exe) = crate::platform::exe_of(pid) else { return false };
    if !runs_from(&exe, root) {
        return false;
    }
    if crate::platform::stop_and_wait(pid, Duration::from_secs(5)) {
        return true;
    }
    // It didn't take the hint (measured: 1.19.5 does within 3 s), and nobody
    // reads what it does any more.
    #[cfg(unix)]
    if crate::platform::start_token(pid) == Some(start) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    crate::platform::wait_until(Duration::from_secs(2), || !crate::platform::pid_running(pid))
}

/// Is `exe` a file under `root`? Both as given and as resolved: a home
/// reached through a symlink (`/var` → `/private/var`) names it either way,
/// and Windows spells drive paths with `\\?\` and in any case.
fn runs_from(exe: &Path, root: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        if cfg!(windows) {
            s.replace('/', "\\").to_lowercase()
        } else {
            s.to_string()
        }
    };
    let sep = std::path::MAIN_SEPARATOR;
    let exe_forms = [Some(exe.to_path_buf()), std::fs::canonicalize(exe).ok()];
    let root_forms = [Some(root.to_path_buf()), std::fs::canonicalize(root).ok()];
    exe_forms.iter().flatten().any(|e| {
        root_forms.iter().flatten().any(|r| {
            let r = norm(r);
            let r = r.trim_end_matches(sep);
            norm(e).strip_prefix(r).is_some_and(|rest| rest.starts_with(sep))
        })
    })
}

/// One turn's `opencode serve`.
struct Server {
    child: tokio::process::Child,
    pid: Option<u32>,
    url: String,
    auth: String,
    http: reqwest::Client,
    /// Its stdout and stderr readers: a full pipe would stall it.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// It serves the turn (`TurnProc`, which the turn's journal records) and
    /// is a live child for the shutdown handler, both from its spawn on —
    /// see [`Server::start`].
    _serving: super::Serving,
    _live: super::ChildGuard,
}

/// The most of the server's stderr kept, for a start that fails.
const STDERR_TAIL: usize = 16 * 1024;

impl Server {
    /// Start a server for the turn `proc` belongs to, and wait until it
    /// listens.
    async fn start(s: &Spec, proc: &super::TurnProc) -> Result<Self> {
        let password = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let workdir = s.workdir.strip_prefix("\\\\?\\").unwrap_or(&s.workdir);
        let mut cmd = crate::platform::command(&s.program);
        // `--port 0` takes 4096 when it's free and any free port when it
        // isn't (measured: two at once got 4096 and 43719), so the port is
        // read off what the server says, never assumed.
        cmd.args(["serve", "--hostname", "127.0.0.1", "--port", "0", "--print-logs", "--log-level", "WARN"]);
        for k in scrubbed(std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned())) {
            cmd.env_remove(k);
        }
        for (k, v) in &s.seat {
            cmd.env(k, v);
        }
        for (k, v) in super::turn_env(&s.conv, &s.surface, &s.reads) {
            cmd.env(k, v);
        }
        // The reply being streamed right now — `mafold attach <file>` hangs
        // media on it.
        cmd.env("MAFOLD_DRAFT", &s.draft);
        cmd.env("MAFOLD_WALLET_KEY", &s.wallet.key);
        let cfg = config(&s.wallet.base, &s.wallet.models, &s.model);
        for (k, v) in opencode_env(&s.home, &cfg, &password) {
            cmd.env(k, v);
        }
        cmd.current_dir(workdir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        die_with_daemon(&mut cmd);
        let shown = s.program.to_string_lossy().into_owned();
        let mut child = cmd.spawn().map_err(|e| super::spawn_err(&shown, workdir, e))?;
        let pid = child.id();
        // The turn's process from here, not once it listens: a first start
        // in a fresh home can take a minute, and a daemon that dies in it
        // must still leave the server in the turn's journal (the render loop
        // records whatever `proc` names) for the restart to end — on macOS
        // and Windows nothing else ends it. Both guards go with the server,
        // or with this future if the start fails or is dropped.
        let serving = proc.serve(pid);
        let live = super::ChildGuard::new(pid);

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        if let Some(mut se) = child.stderr.take() {
            let acc = stderr.clone();
            tasks.push(tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = se.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    let mut a = acc.lock().unwrap();
                    a.extend_from_slice(&chunk[..n]);
                    if a.len() > STDERR_TAIL {
                        let cut = a.len() - STDERR_TAIL;
                        a.drain(..cut);
                    }
                }
            }));
        }
        let stdout = child.stdout.take().context("no stdout from opencode serve")?;
        let mut lines = BufReader::new(stdout).lines();
        let tail = |e: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&e.lock().unwrap()).trim().to_string();
        let found = tokio::time::timeout(STARTUP, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = listen_url(&line) {
                    return Some(url);
                }
            }
            None
        })
        .await;
        let url = match found {
            Ok(Some(url)) => url,
            Ok(None) => {
                let _ = child.wait().await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                // After `logfile::EXITED`, like the other runtimes: a
                // diagnostics file keeps its length, not OpenCode's words.
                bail!("opencode serve{}{}", crate::logfile::EXITED, tail(&stderr));
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                bail!("opencode serve didn't start listening within {}s: {}", STARTUP.as_secs(), tail(&stderr));
            }
        };
        // Keep its stdout read, or a full pipe stalls it.
        tasks.push(tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} }));
        let http = reqwest::Client::builder()
            // Loopback, never through the user's HTTP(S)_PROXY.
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("couldn't build the OpenCode client")?;
        use base64::Engine;
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}")));
        Ok(Self { child, pid, url, auth, http, tasks, _serving: serving, _live: live })
    }

    async fn get(&self, path: &str) -> Result<(u16, Value)> {
        let r = self.http.get(format!("{}{path}", self.url)).header("authorization", &self.auth).timeout(REQUEST).send().await?;
        Self::answer(r).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<(u16, Value)> {
        let r = self
            .http
            .post(format!("{}{path}", self.url))
            .header("authorization", &self.auth)
            .json(body)
            .timeout(REQUEST)
            .send()
            .await?;
        Self::answer(r).await
    }

    async fn answer(r: reqwest::Response) -> Result<(u16, Value)> {
        let status = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        let v = serde_json::from_str(&text).unwrap_or(if text.is_empty() { Value::Null } else { Value::String(text) });
        Ok((status, v))
    }

    /// This conversation's session when the server still has it, else a new
    /// one. A stored id can be gone (the agent's home was cleared) or not an
    /// OpenCode id at all (written by another runtime the agent ran on).
    async fn session(&self, stored: Option<&str>) -> Result<String> {
        if let Some(id) = stored.map(str::trim).filter(|s| !s.is_empty()) {
            let (status, v) = self.get(&format!("/session/{id}")).await?;
            match status {
                200 if v["id"].as_str() == Some(id) => return Ok(id.to_string()),
                // Not found, or not shaped like an OpenCode session id.
                200 | 400 | 404 => println!("↻ opencode: session {id} isn't here ({status}) — starting a new one"),
                _ => bail!("OpenCode couldn't look up session {id} ({status}): {v}"),
            }
        }
        let (status, v) = self.post("/session", &json!({ "title": "Mafold" })).await?;
        match v["id"].as_str() {
            Some(id) if status == 200 => Ok(id.to_string()),
            _ => bail!("OpenCode couldn't open a session ({status}): {v}"),
        }
    }

    /// Ask the reply to stop; the stream says when it has (`session.idle`).
    /// Bounded, so a wedged server can't hold up the kill that follows.
    async fn abort(&self, sid: &str) {
        let path = format!("/session/{sid}/abort");
        let body = json!({});
        let _ = tokio::time::timeout(STOP_GRACE, self.post(&path, &body)).await;
    }

    /// Is `sid` still working on the reply?
    async fn busy(&self, sid: &str) -> Result<bool> {
        let (_, v) = self.get("/session/status").await?;
        Ok(v.get(sid).is_some_and(|s| s["type"] != "idle"))
    }

    /// The event stream, already connected: the server's `server.connected`
    /// has been read, so nothing published after this returns is missed.
    async fn subscribe(&self) -> Result<Events> {
        let resp = self
            .http
            .get(format!("{}/event", self.url))
            .header("authorization", &self.auth)
            .header("accept", "text/event-stream")
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("OpenCode refused its event stream ({})", resp.status());
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut sse = Sse::new(resp);
            loop {
                match sse.next().await {
                    Ok(Some(v)) => {
                        if tx.send(Feed::Event(v)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(Feed::Closed("it ended".into()));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(Feed::Closed(format!("{e:#}")));
                        return;
                    }
                }
            }
        });
        let mut ev = Events { rx, task };
        match tokio::time::timeout(REQUEST, ev.next()).await {
            Ok(Feed::Event(v)) if v["type"] == "server.connected" => Ok(ev),
            Ok(Feed::Event(v)) => bail!("OpenCode's event stream began with {}", v["type"]),
            Ok(Feed::Closed(why)) => bail!("OpenCode's event stream closed at once ({why})"),
            Err(_) => bail!("OpenCode's event stream said nothing for {}s", REQUEST.as_secs()),
        }
    }

    /// End the server and reap it. Its tools' processes are its business: an
    /// aborted reply has had [`STOP_GRACE`] to end them, and a background job
    /// the agent started on purpose outlives the turn, as with every harness.
    async fn stop(&mut self) {
        if let Some(pid) = self.pid {
            crate::platform::terminate(pid);
        }
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// `opencode server listening on http://127.0.0.1:4096` → the URL.
fn listen_url(line: &str) -> Option<String> {
    let at = line.find("http://127.0.0.1:")?;
    let rest = &line[at..];
    let end = rest[17..].find(|c: char| !c.is_ascii_digit()).map(|i| i + 17).unwrap_or(rest.len());
    (end > 17).then(|| rest[..end].to_string())
}

enum Feed {
    Event(Value),
    Closed(String),
}

struct Events {
    rx: UnboundedReceiver<Feed>,
    task: tokio::task::JoinHandle<()>,
}

impl Events {
    async fn next(&mut self) -> Feed {
        self.rx.recv().await.unwrap_or_else(|| Feed::Closed("the reader stopped".into()))
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A minimal `text/event-stream` reader: `data:` lines up to a blank line are
/// one event, and OpenCode's are JSON.
struct Sse {
    resp: reqwest::Response,
    buf: Vec<u8>,
    data: String,
}

impl Sse {
    fn new(resp: reqwest::Response) -> Self {
        Self { resp, buf: Vec::new(), data: String::new() }
    }

    async fn next(&mut self) -> Result<Option<Value>> {
        loop {
            if let Some(v) = sse_take(&mut self.buf, &mut self.data) {
                return Ok(Some(v));
            }
            match self.resp.chunk().await? {
                Some(c) => self.buf.extend_from_slice(&c),
                None => return Ok(None),
            }
        }
    }
}

/// The next complete event in `buf`, consuming what it read. `data` carries a
/// half-read event between calls.
fn sse_take(buf: &mut Vec<u8>, data: &mut String) -> Option<Value> {
    while let Some(i) = buf.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buf.drain(..=i).collect();
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            if !data.is_empty() {
                let d = std::mem::take(data);
                if let Ok(v) = serde_json::from_str(&d) {
                    return Some(v);
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    None
}

/// What one event means for the turn, beyond the events it emitted.
#[derive(Debug, PartialEq)]
enum Signal {
    None,
    /// The session finished the reply.
    Idle,
    /// The reply failed, in these words.
    Error(String),
    /// The reply was aborted (Stop).
    Aborted,
    /// OpenCode is retrying a refused model call.
    Retry { attempt: u64, message: String },
    /// A permission ask that must be answered for the reply to go on.
    Ask { session: String, id: String },
}

/// OpenCode's events → [`AgentEvent`]s, for one session.
///
/// Every part arrives more than once (created, streamed, finished) and may
/// arrive again from [`Mapper::reconcile`], so the mapping is idempotent per
/// part: text is emitted as the suffix not yet said, a thinking block and a
/// tool's call and result once each, a step's tokens counted once.
struct Mapper<'a> {
    sink: &'a UnboundedSender<AgentEvent>,
    session: String,
    model: String,
    /// message id → role, from `message.updated`. The prompt comes back as a
    /// user message's text part, which is not the agent speaking.
    roles: HashMap<String, String>,
    /// part id → part type, so a delta knows what it is streaming.
    kinds: HashMap<String, String>,
    /// text part id → what of it has been emitted.
    said: HashMap<String, String>,
    thought: HashSet<String>,
    called: HashSet<String>,
    resulted: HashSet<String>,
    steps: HashSet<String>,
    /// This turn's prompt, as OpenCode stored it.
    user_msg: Option<String>,
    stats: RunStats,
    tool_calls: u64,
    tool_errors: u64,
    produced: bool,
}

impl<'a> Mapper<'a> {
    fn new(sink: &'a UnboundedSender<AgentEvent>, session: &str, model: &str) -> Self {
        Self {
            sink,
            session: session.to_string(),
            model: model.to_string(),
            roles: HashMap::new(),
            kinds: HashMap::new(),
            said: HashMap::new(),
            thought: HashSet::new(),
            called: HashSet::new(),
            resulted: HashSet::new(),
            steps: HashSet::new(),
            user_msg: None,
            stats: RunStats::default(),
            tool_calls: 0,
            tool_errors: 0,
            produced: false,
        }
    }

    fn send(&mut self, ev: AgentEvent) {
        if matches!(ev, AgentEvent::Text(_) | AgentEvent::Thinking(_) | AgentEvent::ToolCall { .. }) {
            self.produced = true;
        }
        let _ = self.sink.send(ev);
    }

    fn event(&mut self, ev: &Value) -> Signal {
        let p = &ev["properties"];
        let ty = ev["type"].as_str().unwrap_or("");
        if ty == "permission.asked" {
            if let (Some(s), Some(id)) = (p["sessionID"].as_str(), p["id"].as_str()) {
                return Signal::Ask { session: s.to_string(), id: id.to_string() };
            }
            return Signal::None;
        }
        let ours = p["sessionID"].as_str() == Some(self.session.as_str());
        if !ours {
            // A subagent's own session (the `task` tool): its card and result
            // come through this session; its streaming only keeps the reply
            // visibly alive.
            if ty == "message.part.delta" {
                let n = p["delta"].as_str().map_or(0, str::len) as u64;
                let _ = self.sink.send(AgentEvent::Pulse { chars: n, tokens: None });
            }
            return Signal::None;
        }
        match ty {
            "message.updated" => {
                let info = &p["info"];
                if let (Some(id), Some(role)) = (info["id"].as_str(), info["role"].as_str()) {
                    self.roles.insert(id.to_string(), role.to_string());
                    if role == "user" {
                        self.user_msg = Some(id.to_string());
                    }
                }
                Signal::None
            }
            "message.part.updated" => {
                self.part(&p["part"]);
                Signal::None
            }
            "message.part.delta" => {
                self.delta(p);
                Signal::None
            }
            "session.status" => {
                let st = &p["status"];
                if st["type"] == "retry" {
                    let _ = self.sink.send(AgentEvent::Pulse { chars: 0, tokens: None });
                    return Signal::Retry {
                        attempt: st["attempt"].as_u64().unwrap_or(0),
                        message: st["message"].as_str().unwrap_or("").to_string(),
                    };
                }
                Signal::None
            }
            "session.error" => match error_text(&p["error"]) {
                Some(e) => Signal::Error(e),
                None => Signal::Aborted,
            },
            "session.idle" => Signal::Idle,
            _ => Signal::None,
        }
    }

    fn delta(&mut self, p: &Value) {
        if p["field"] != "text" {
            return;
        }
        let (Some(part), Some(d)) = (p["partID"].as_str(), p["delta"].as_str()) else {
            return;
        };
        if p["messageID"].as_str().is_some_and(|m| self.is_user(m)) {
            return;
        }
        match self.kinds.get(part).map(String::as_str) {
            Some("text") => {
                self.said.entry(part.to_string()).or_default().push_str(d);
                self.send(AgentEvent::Text(d.to_string()));
            }
            // Thinking lands as one block when it ends; meanwhile it is
            // progress. So is a delta for a part not announced yet — its
            // text arrives whole with the part.
            _ => {
                let _ = self.sink.send(AgentEvent::Pulse { chars: d.len() as u64, tokens: None });
            }
        }
    }

    fn is_user(&self, msg: &str) -> bool {
        self.roles.get(msg).is_some_and(|r| r == "user")
    }

    fn part(&mut self, part: &Value) {
        let (Some(id), Some(ty)) = (part["id"].as_str(), part["type"].as_str()) else {
            return;
        };
        if part["messageID"].as_str().is_some_and(|m| self.is_user(m)) {
            return;
        }
        self.kinds.insert(id.to_string(), ty.to_string());
        match ty {
            "text" => {
                if part["ignored"] == true {
                    return;
                }
                let full = part["text"].as_str().unwrap_or("");
                let said = self.said.get(id).map(String::as_str).unwrap_or("");
                if full.len() > said.len() && full.starts_with(said) {
                    let rest = full[said.len()..].to_string();
                    self.said.insert(id.to_string(), full.to_string());
                    self.send(AgentEvent::Text(rest));
                }
            }
            "reasoning" => {
                let done = part["time"]["end"].is_number();
                let text = part["text"].as_str().unwrap_or("").trim();
                if done && !text.is_empty() && self.thought.insert(id.to_string()) {
                    self.send(AgentEvent::Thinking(text.to_string()));
                }
            }
            "tool" => self.tool(part),
            "step-finish" => {
                if self.steps.insert(id.to_string()) {
                    let step = step_stats(&part["tokens"], &self.model);
                    self.stats.add_request(&step);
                    self.stats.tool_calls = Some(self.tool_calls);
                    self.stats.tool_errors = Some(self.tool_errors);
                    let _ = self.sink.send(AgentEvent::Stats(self.stats.clone()));
                }
            }
            _ => {}
        }
    }

    fn tool(&mut self, part: &Value) {
        let Some(call) = part["callID"].as_str().filter(|c| !c.is_empty()) else {
            return;
        };
        let name = part["tool"].as_str().unwrap_or("tool");
        let st = &part["state"];
        let status = st["status"].as_str().unwrap_or("");
        // Pending carries no input yet; from `running` on it is complete.
        if status == "pending" {
            return;
        }
        if self.called.insert(call.to_string()) {
            let (canon, input) = map_tool(name, &st["input"]);
            self.tool_calls += 1;
            self.send(AgentEvent::ToolCall { id: call.to_string(), name: canon, input });
        }
        let (text, failed) = match status {
            "completed" => {
                let mut out = st["output"].as_str().unwrap_or("").to_string();
                let exit = st["metadata"]["exit"].as_i64().filter(|c| *c != 0);
                if let Some(code) = exit {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str(&format!("(exit {code})"));
                }
                (out, exit.is_some())
            }
            "error" => (st["error"].as_str().unwrap_or("the tool failed").to_string(), true),
            _ => return,
        };
        if self.resulted.insert(call.to_string()) {
            if failed {
                self.tool_errors += 1;
            }
            self.send(AgentEvent::ToolResult { id: call.to_string(), text });
            self.send(AgentEvent::ToolStatus { id: call.to_string(), failed });
        }
    }

    /// Feed the session's stored messages through the same mapping: whatever
    /// this turn's assistant messages hold that the stream didn't deliver is
    /// emitted now, and nothing twice. This turn's are the ones answering its
    /// prompt — the last user message when the stream never named it.
    /// Returns the error the last of them ended on, in words (`error_text`).
    fn reconcile(&mut self, msgs: &Value) -> Option<String> {
        let all = msgs.as_array()?;
        let prompt = self.user_msg.clone().or_else(|| {
            all.iter().rev().find(|m| m["info"]["role"] == "user").and_then(|m| m["info"]["id"].as_str()).map(str::to_string)
        })?;
        let mut ended_on = None;
        for m in all {
            let info = &m["info"];
            if info["role"] != "assistant" || info["parentID"].as_str() != Some(prompt.as_str()) {
                continue;
            }
            if let Some(id) = info["id"].as_str() {
                self.roles.insert(id.to_string(), "assistant".into());
            }
            for part in m["parts"].as_array().into_iter().flatten() {
                self.part(part);
            }
            ended_on = info.get("error").filter(|e| e.is_object()).and_then(error_text);
        }
        ended_on
    }
}

/// One model call's usage, as [`RunStats`] counts it: input INCLUDES cache
/// reads and writes (OpenCode's `input` excludes them), and output includes
/// reasoning (OpenCode reports it apart: 33 + 4 for a 37-token completion).
fn step_stats(t: &Value, model: &str) -> RunStats {
    let n = |v: &Value| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64);
    let cache_read = n(&t["cache"]["read"]);
    let cache_write = n(&t["cache"]["write"]);
    let input = n(&t["input"]).map(|i| i + cache_read.unwrap_or(0) + cache_write.unwrap_or(0));
    let output = n(&t["output"]).map(|o| o + n(&t["reasoning"]).unwrap_or(0));
    let total = n(&t["total"]).or_else(|| input.zip(output).map(|(a, b)| a + b));
    RunStats {
        model: Some(model.to_string()),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        total_tokens: total,
        context_used_tokens: input,
        context_limit_tokens: Some(CONTEXT_TOKENS),
        context_basis: Some("last_request_input".into()),
        ..Default::default()
    }
}

/// A session error in words — `None` for an abort, which is a Stop, not a
/// failure. A refused model call keeps the router's status and its own
/// sentence, which is what the daemon's failure card reads
/// (`mafold_transcript::failure::classify`).
fn error_text(err: &Value) -> Option<String> {
    let name = err["name"].as_str().unwrap_or("");
    let data = &err["data"];
    let message = data["message"].as_str().unwrap_or("").trim().to_string();
    match name {
        "MessageAbortedError" => None,
        "APIError" => {
            let body: Value = data["responseBody"].as_str().and_then(|b| serde_json::from_str(b).ok()).unwrap_or(Value::Null);
            let said = body["error"]["message"].as_str().map(str::to_string).unwrap_or(message);
            let kind = body["error"]["type"].as_str().map(|t| format!(" ({t})")).unwrap_or_default();
            Some(match data["statusCode"].as_u64() {
                Some(code) => format!("the wallet's router answered {code}{kind}: {said}"),
                None => format!("the wallet's router failed{kind}: {said}"),
            })
        }
        "MessageOutputLengthError" => Some("output_cap: the answer ran past the output limit".into()),
        _ if !message.is_empty() => Some(message),
        _ if !name.is_empty() => Some(name.to_string()),
        _ => Some(err.to_string()),
    }
}

/// An OpenCode tool call → the renderer's tool name and input shape, so its
/// tools draw the cards every other harness gets. Unknown tools pass
/// through as a generic tool card.
fn map_tool(name: &str, a: &Value) -> (String, Value) {
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    match name {
        "bash" => ("bash".into(), json!({ "command": s("command"), "description": s("description") })),
        "read" => ("read".into(), json!({ "file_path": s("filePath") })),
        "write" => ("write".into(), json!({ "file_path": s("filePath"), "content": s("content") })),
        "edit" => (
            "edit".into(),
            json!({ "file_path": s("filePath"), "old_string": s("oldString"), "new_string": s("newString") }),
        ),
        "glob" => ("glob".into(), json!({ "pattern": s("pattern") })),
        "grep" => ("grep".into(), json!({ "pattern": s("pattern") })),
        "webfetch" => ("webfetch".into(), json!({ "url": s("url") })),
        "todowrite" => ("todowrite".into(), json!({ "todos": a["todos"].clone() })),
        "task" => (
            "task".into(),
            json!({ "subagent_type": a["subagent_type"].as_str().unwrap_or("general"), "description": s("description") }),
        ),
        "skill" => ("skill".into(), json!({ "name": s("name") })),
        other => (other.to_string(), super::model_tool_input(other, a)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOL: &str = include_str!("testdata/opencode/tool.sse");
    const STOP: &str = include_str!("testdata/opencode/stop.sse");
    const ERR402: &str = include_str!("testdata/opencode/err402.sse");
    const ERR401: &str = include_str!("testdata/opencode/err401.sse");
    const ERR429: &str = include_str!("testdata/opencode/err429.sse");
    const MESSAGES: &str = include_str!("testdata/opencode/messages.json");

    /// The events of a recorded stream (`opencode serve` 1.19.5 against a fake
    /// provider, 10-30), through the real SSE reader.
    fn events(sse: &str) -> Vec<Value> {
        let mut buf = sse.as_bytes().to_vec();
        let mut data = String::new();
        let mut out = Vec::new();
        while let Some(v) = sse_take(&mut buf, &mut data) {
            out.push(v);
        }
        out
    }

    fn session_of(evs: &[Value]) -> String {
        evs.iter().find_map(|e| e["properties"]["sessionID"].as_str()).unwrap().to_string()
    }

    /// Run `sse` through a mapper: what it emitted and what it signalled.
    fn map(sse: &str) -> (Vec<AgentEvent>, Vec<Signal>) {
        let evs = events(sse);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut m = Mapper::new(&tx, &session_of(&evs), "deepseek-v4-flash");
        let signals: Vec<Signal> = evs.iter().map(|e| m.event(e)).filter(|s| *s != Signal::None).collect();
        drop(m);
        drop(tx);
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        (out, signals)
    }

    fn text(evs: &[AgentEvent]) -> String {
        evs.iter().filter_map(|e| if let AgentEvent::Text(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    /// The whole tool turn: thinking, the bash call and its output, the
    /// answer streamed as text, and each step's tokens — never the prompt.
    #[test]
    fn a_tool_turn_maps_to_thinking_tool_text_and_stats() {
        let (evs, signals) = map(TOOL);
        assert_eq!(signals, vec![Signal::Idle]);
        let content: Vec<&AgentEvent> = evs.iter().filter(|e| !matches!(e, AgentEvent::Pulse { .. } | AgentEvent::Stats(_))).collect();
        match &content[..] {
            [AgentEvent::Thinking(t), AgentEvent::ToolCall { id, name, input }, AgentEvent::ToolResult { id: rid, text: out }, AgentEvent::ToolStatus { id: sid, failed }, rest @ ..] =>
            {
                assert_eq!(t, "I will run echo.");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "echo tool-ran-$((6*7))");
                assert_eq!((rid, sid), (id, id));
                assert_eq!(out, "tool-ran-42\n");
                assert!(!failed);
                assert!(rest.iter().all(|e| matches!(e, AgentEvent::Text(_))), "{rest:?}");
            }
            other => panic!("unexpected shape: {other:?}"),
        }
        assert_eq!(text(&evs), "Tool said: tool-ran-42\n ");
        assert!(!text(&evs).contains("Run a shell command"), "the prompt is not the agent talking");
        let stats: Vec<&RunStats> = evs.iter().filter_map(|e| if let AgentEvent::Stats(s) = e { Some(s) } else { None }).collect();
        assert_eq!(stats.len(), 2, "one snapshot per model call");
        let last = stats.last().unwrap();
        assert_eq!(last.model_requests, Some(2));
        // 420 + 600 cached, then 440 + 600 cached.
        assert_eq!(last.input_tokens, Some(2060));
        // 33 + 4 reasoning, then 37.
        assert_eq!(last.output_tokens, Some(74));
        assert_eq!(last.cache_read_tokens, Some(1200));
        assert_eq!(last.total_tokens, Some(1057 + 1077));
        assert_eq!(last.context_used_tokens, Some(1040));
        assert_eq!(last.tool_calls, Some(1));
        assert_eq!(last.cost_usd, None, "OpenCode's $ estimate is not the bill");
    }

    /// Stop mid-answer: what streamed before it stays, and the abort reads as
    /// a stop, not an error.
    #[test]
    fn a_stopped_turn_keeps_what_it_said_and_signals_the_abort() {
        let (evs, signals) = map(STOP);
        assert_eq!(text(&evs), "w0 w1 w2 w3 w4 w5 ");
        assert_eq!(signals.first(), Some(&Signal::Aborted));
        assert!(signals.contains(&Signal::Idle));
        assert!(!text(&evs).contains("SLOW"), "the prompt is not the agent talking");
    }

    /// A refused model call keeps the router's status and its own sentence,
    /// and the shared failure reader names it.
    #[test]
    fn a_router_refusal_is_the_turns_error_in_the_routers_words() {
        let (_, signals) = map(ERR402);
        let Some(Signal::Error(e)) = signals.first() else { panic!("{signals:?}") };
        assert_eq!(
            e,
            "the wallet's router answered 402 (insufficient_quota): router says 402: wallet_cap_reached: @ops's monthly cap for @bot is used up"
        );
        let f = mafold_transcript::failure::classify(e);
        assert_eq!(f.kind, mafold_transcript::failure::FailureKind::Cap);
        assert_eq!(f.payer.as_deref(), Some("ops"));
        assert_eq!(signals.last(), Some(&Signal::Idle));

        let (_, signals) = map(ERR401);
        let Some(Signal::Error(e)) = signals.first() else { panic!("{signals:?}") };
        assert!(e.starts_with("the wallet's router answered 401 (invalid_api_key)"), "{e}");
        assert_eq!(mafold_transcript::failure::classify(e).status, Some(401));
    }

    /// A 429 is retried by OpenCode itself; each try is counted, so the
    /// reply can give up instead of waiting forever.
    #[test]
    fn a_retried_call_is_counted() {
        let (_, signals) = map(ERR429);
        let tries: Vec<u64> = signals.iter().filter_map(|s| if let Signal::Retry { attempt, .. } = s { Some(*attempt) } else { None }).collect();
        assert_eq!(tries, vec![1, 2, 3]);
        assert!(matches!(&signals[0], Signal::Retry { message, .. } if message == "router says 429"));
        assert!(!signals.contains(&Signal::Idle), "still retrying when the recording ended");
    }

    /// The session's own copy, fed after the stream, adds nothing the stream
    /// already said — and supplies what it missed.
    #[test]
    fn reconciling_with_the_session_is_idempotent_and_fills_gaps() {
        let msgs: Value = serde_json::from_str(MESSAGES).unwrap();
        let evs = events(TOOL);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut m = Mapper::new(&tx, &session_of(&evs), "deepseek-v4-flash");
        for e in &evs {
            m.event(e);
        }
        while rx.try_recv().is_ok() {}
        assert_eq!(m.reconcile(&msgs), None, "the tool turn ended well");
        assert!(rx.try_recv().is_err(), "nothing twice");

        // The 402 turn's message carries its error, for a stream that lost it.
        let refused = msgs.as_array().unwrap().iter().find(|x| x["info"]["role"] == "user" && x["parts"][0]["text"] == "FAIL402 now").unwrap();
        m.user_msg = refused["info"]["id"].as_str().map(str::to_string);
        let e = m.reconcile(&msgs).expect("the refusal is on the message");
        assert!(e.starts_with("the wallet's router answered 402") && e.contains("wallet_cap_reached"), "{e}");

        // A stream that lost every delta and every final part: the session
        // still has the answer.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut m = Mapper::new(&tx, &session_of(&evs), "deepseek-v4-flash");
        for e in evs.iter().filter(|e| e["type"] == "message.updated") {
            m.event(e);
        }
        let users: Vec<&str> = msgs.as_array().unwrap().iter().filter(|x| x["info"]["role"] == "user").filter_map(|x| x["info"]["id"].as_str()).collect();
        m.user_msg = Some(users[0].to_string());
        m.reconcile(&msgs);
        drop(m);
        drop(tx);
        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(e);
        }
        assert_eq!(text(&got), "Tool said: tool-ran-42\n ");
        assert!(got.iter().any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "bash")));
    }

    #[test]
    fn opencode_tools_draw_the_shared_cards() {
        let (n, i) = map_tool("edit", &json!({ "filePath": "a.rs", "oldString": "x", "newString": "y", "replaceAll": false }));
        assert_eq!(n, "edit");
        assert_eq!(i, json!({ "file_path": "a.rs", "old_string": "x", "new_string": "y" }));
        assert_eq!(map_tool("read", &json!({ "filePath": "b.rs", "offset": 3 })).1["file_path"], "b.rs");
        assert_eq!(map_tool("write", &json!({ "filePath": "c", "content": "hi" })).1["content"], "hi");
        let (n, i) = map_tool("todowrite", &json!({ "todos": [{ "content": "a", "status": "completed", "priority": "high", "id": "1" }] }));
        assert_eq!(n, "todowrite");
        assert_eq!(i["todos"][0]["status"], "completed");
        assert_eq!(map_tool("task", &json!({ "description": "look", "prompt": "p", "subagent_type": "explore" })).1["subagent_type"], "explore");
        // AskUserQuestion's daemon-only keys never pass through, whatever the tool.
        assert_eq!(map_tool("codesearch", &json!({ "query": "q" })), ("codesearch".into(), json!({ "query": "q" })));
    }

    /// The model, the router door, and nothing else: the only provider, the
    /// key by reference (never in the text), every ask allowed, the ask tool
    /// off.
    #[test]
    fn the_config_holds_opencode_to_the_wallet() {
        let models = vec!["deepseek-v4-flash".to_string(), "deepseek-v4-pro".to_string()];
        let c = config("https://api.mafold.ai/v1", &models, "deepseek-v4-pro");
        assert_eq!(c["model"], "mafold/deepseek-v4-pro");
        assert_eq!(c["small_model"], "mafold/deepseek-v4-pro");
        assert_eq!(c["enabled_providers"], json!(["mafold"]));
        let p = &c["provider"]["mafold"];
        assert_eq!(p["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(p["options"]["baseURL"], "https://api.mafold.ai/v1");
        assert_eq!(p["options"]["apiKey"], "{env:MAFOLD_WALLET_KEY}");
        assert_eq!(p["models"].as_object().unwrap().len(), 2);
        assert_eq!(p["models"]["deepseek-v4-flash"]["limit"]["output"], OUTPUT_TOKENS);
        for k in ["edit", "bash", "webfetch", "read", "external_directory", "doom_loop"] {
            assert_eq!(c["permission"][k], "allow", "{k}");
        }
        assert_eq!(c["permission"]["question"], "deny");
        assert_eq!(c["tools"]["question"], false);
        assert_eq!(c["share"], "disabled");
        assert_eq!(c["autoupdate"], false);
        assert_eq!(c["snapshot"], false);
        assert_eq!(c.as_object().unwrap().len(), 10, "a new key is a measured one: {c:#}");
    }

    /// Other providers' credentials and the user's OpenCode settings stay
    /// behind; the agent's tools keep everything else.
    #[test]
    fn the_child_env_drops_other_providers_and_keeps_the_rest() {
        let names = [
            "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "DEEPSEEK_API_KEY", "GEMINI_API_KEY",
            "OPENROUTER_API_KEY", "OPENCODE_CONFIG", "opencode_server_password", "MAFOLD_WALLET_KEY",
            "PATH", "HOME", "GH_TOKEN", "AWS_PROFILE", "MAFOLD_BOT_TOKEN", "MAFOLD_CONV", "XDG_CONFIG_HOME",
        ];
        let gone = scrubbed(names.iter().map(|s| s.to_string()));
        assert_eq!(
            gone,
            [
                "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "DEEPSEEK_API_KEY", "GEMINI_API_KEY",
                "OPENROUTER_API_KEY", "OPENCODE_CONFIG", "opencode_server_password", "MAFOLD_WALLET_KEY",
            ]
        );
        let home = Path::new("/h/.mafold/runtimes/opencode/home/bot");
        let env: HashMap<String, String> = opencode_env(home, &json!({}), "pw").into_iter().collect();
        assert_eq!(env["XDG_CONFIG_HOME"], home.join("config").to_string_lossy());
        assert_eq!(env["XDG_DATA_HOME"], home.join("data").to_string_lossy());
        assert_eq!(env["XDG_STATE_HOME"], home.join("state").to_string_lossy());
        assert_eq!(env["XDG_CACHE_HOME"], home.join("cache").to_string_lossy());
        assert_eq!(env["OPENCODE_SERVER_PASSWORD"], "pw");
        assert_eq!(env["OPENCODE_TEST_HOME"], home.to_string_lossy(), "not the user's ~/.opencode");
        for f in ["OPENCODE_DISABLE_AUTOUPDATE", "OPENCODE_DISABLE_MODELS_FETCH", "OPENCODE_DISABLE_PROJECT_CONFIG", "OPENCODE_PURE"] {
            assert_eq!(env[f], "1", "{f}");
        }
        assert!(!env.contains_key("MAFOLD_WALLET_KEY"), "the key comes from the turn, not from here");
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn the_wallet_is_all_three_or_nothing() {
        let full = env(&[
            ("MAFOLD_WALLET_KEY", "mrk_1"),
            ("MAFOLD_WALLET_BASE", "https://api.mafold.ai/v1/"),
            ("MAFOLD_WALLET_MODELS", " deepseek-v4-flash, ,deepseek-v4-pro "),
        ]);
        let w = Wallet::from_env(&full, None).unwrap();
        assert_eq!(w.key, "mrk_1");
        assert_eq!(w.base, "https://api.mafold.ai/v1");
        assert_eq!(w.models, ["deepseek-v4-flash", "deepseek-v4-pro"]);
        assert_eq!(Wallet::from_env(&full, Some("http://127.0.0.1:9/v1".into())).unwrap().base, "http://127.0.0.1:9/v1");
        for drop in ["MAFOLD_WALLET_KEY", "MAFOLD_WALLET_BASE", "MAFOLD_WALLET_MODELS"] {
            let some: Vec<_> = full.iter().filter(|(k, _)| k != drop).cloned().collect();
            assert!(Wallet::from_env(&some, None).is_none(), "without {drop}");
        }
        let empty_models = env(&[("MAFOLD_WALLET_KEY", "k"), ("MAFOLD_WALLET_BASE", "b"), ("MAFOLD_WALLET_MODELS", " , ")]);
        assert!(Wallet::from_env(&empty_models, None).is_none());
        assert_eq!(dev_base(&env(&[("MAFOLD_DEV_WALLET_BASE", " http://x/v1 ")])).as_deref(), Some("http://x/v1"));
    }

    /// The first `{wallet:?}` or `{spec:?}` someone adds to a trace must not
    /// put the reply's live key in the daemon's log (`client::ReplyKey` is
    /// redacted for the same reason).
    #[test]
    fn a_wallets_debug_never_shows_its_key() {
        let w = Wallet {
            key: "mrk_live_9f2c1e7a44b0".into(),
            base: "https://api.mafold.ai/v1".into(),
            models: vec!["deepseek-v4-flash".into()],
        };
        let shown = format!("{w:?} {w:#?}");
        assert!(!shown.contains("mrk_live_9f2c1e7a44b0"), "{shown}");
        assert!(shown.contains("<redacted>") && shown.contains("https://api.mafold.ai/v1") && shown.contains("deepseek-v4-flash"), "{shown}");
    }

    #[test]
    fn the_model_is_the_picked_one_or_a_refusal() {
        let models = vec!["deepseek-v4-flash".to_string(), "deepseek-v4-pro".to_string()];
        assert_eq!(pick_model(None, &models).unwrap(), "deepseek-v4-flash");
        assert_eq!(pick_model(Some(" "), &models).unwrap(), "deepseek-v4-flash");
        assert_eq!(pick_model(Some("mafold/deepseek-v4-pro"), &models).unwrap(), "deepseek-v4-pro");
        assert_eq!(pick_model(Some("DeepSeek-V4-Pro"), &models).unwrap(), "deepseek-v4-pro");
        let why = pick_model(Some("opus"), &models).unwrap_err();
        assert!(why.contains("`opus`") && why.contains("deepseek-v4-flash, deepseek-v4-pro") && why.contains("nothing ran"), "{why}");
    }

    /// The lockfile that tells OpenCode its plugin SDK is already in place —
    /// written once, never over one OpenCode keeps itself.
    #[test]
    fn the_config_folder_starts_with_the_plugin_sdk_accounted_for() {
        let home = std::env::temp_dir().join(format!("mafold-oc-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        seed_config(&home).unwrap();
        let dir = home.join("config/opencode");
        assert!(dir.join("node_modules").is_dir());
        let lock: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package-lock.json")).unwrap()).unwrap();
        assert_eq!(lock["packages"][""]["dependencies"]["@opencode-ai/plugin"], opencode_pin::VERSION);
        std::fs::write(dir.join("package-lock.json"), "{\"kept\":true}").unwrap();
        seed_config(&home).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("package-lock.json")).unwrap(), "{\"kept\":true}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn each_agent_has_its_own_home() {
        assert_eq!(agent_name("c1__ch__ops_wallet"), "ops_wallet");
        assert_eq!(agent_name("c1____ops_wallet--guest-bob"), "ops_wallet");
        assert_eq!(agent_name("c1____ops_wallet--guest"), "ops_wallet");
    }

    #[test]
    fn the_listening_line_gives_the_url() {
        assert_eq!(listen_url("opencode server listening on http://127.0.0.1:4096").as_deref(), Some("http://127.0.0.1:4096"));
        assert_eq!(listen_url("opencode server listening on http://127.0.0.1:43719\n").as_deref(), Some("http://127.0.0.1:43719"));
        assert_eq!(listen_url("Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured."), None);
        assert_eq!(listen_url("http://127.0.0.1:"), None);
    }

    #[test]
    fn errors_read_in_words() {
        assert_eq!(error_text(&json!({ "name": "MessageAbortedError", "data": { "message": "Aborted" } })), None);
        let e = error_text(&json!({ "name": "APIError", "data": { "message": "Forbidden", "statusCode": 403, "isRetryable": false,
            "responseBody": "{\"error\":{\"message\":\"wallet_grant_required: @ops no longer lets @bot spend from their wallet\",\"type\":\"wallet_grant_required\"}}" } }))
        .unwrap();
        assert_eq!(mafold_transcript::failure::classify(&e).kind, mafold_transcript::failure::FailureKind::Grant);
        assert_eq!(
            error_text(&json!({ "name": "APIError", "data": { "message": "boom", "statusCode": 503, "isRetryable": true } })).unwrap(),
            "the wallet's router answered 503: boom"
        );
        assert_eq!(error_text(&json!({ "name": "UnknownError", "data": { "message": "x broke" } })).unwrap(), "x broke");
    }

    /// A turn without the wallet is refused in the contract's words before
    /// anything is started — no install, no server.
    #[tokio::test]
    async fn a_turn_without_a_wallet_key_is_refused() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-nokey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut t = turn(&dir, None, "hi", Vec::new());
        t.env = env(&[("MAFOLD_WALLET_KEY", "k"), ("MAFOLD_WALLET_MODELS", "deepseek-v4-flash")]);
        let out = run_on(t, tx, Place::At { bin: PathBuf::from("/nonexistent/opencode"), root: dir.clone() }).await.unwrap();
        assert_eq!(out.error.as_deref(), Some(NO_WALLET_KEY));
        assert!(!out.produced && !out.stopped);
        assert!(rx.try_recv().is_err(), "nothing was said");
        assert!(!dir.join("home").exists(), "nothing was set up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pinned binary cut short (a power loss after an install, say) can't
    /// start, and nothing used to put it right: every wallet reply failed at
    /// "opencode serve exited" until someone deleted the file by hand. Now a
    /// start that fails re-checks the binary against the digest its install
    /// recorded, installs it again once when it doesn't match, and starts it
    /// again. (The stand-in package only answers `--version`, so the second
    /// start fails too — on the pinned bytes, put back.)
    #[cfg(unix)]
    #[tokio::test]
    async fn a_damaged_pinned_binary_is_installed_again_when_it_wont_start() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (bytes, integrity) = opencode_pin::tests::fake_package(&dir, opencode_pin::VERSION);
        let (url, registry) = opencode_pin::tests::serve(bytes).await;
        let pin = opencode_pin::Source {
            root: dir.join("root"),
            build: opencode_pin::Build {
                target: "test",
                package: "opencode-test",
                integrity: Box::leak(integrity.into_boxed_str()),
                path: "package/bin/opencode",
            },
            registries: vec![url],
        };
        let bin = pin.ensure().await.unwrap();
        let good = std::fs::read(&bin).unwrap();
        std::fs::write(&bin, b"\x7fELF cut short").unwrap();
        let wallet = env(&[
            ("MAFOLD_WALLET_KEY", "mrk_damaged_0123456789"),
            ("MAFOLD_WALLET_BASE", "http://127.0.0.1:9/v1"),
            ("MAFOLD_WALLET_MODELS", "deepseek-v4-flash"),
        ]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = run_on(turn(&work, None, "hi", wallet), tx, Place::Pinned(pin.clone())).await;
        registry.abort();
        assert!(out.is_err(), "the stand-in can't serve");
        assert_eq!(std::fs::read(&bin).unwrap(), good, "the pinned bytes are back");
        assert!(opencode_pin::intact(&bin));
        let mut notices = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let AgentEvent::Notice(n) = e {
                notices.push(n);
            }
        }
        assert!(notices.iter().any(|n| n.contains("installing it again")), "the reply says so: {notices:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A first reply that has to install the pinned build first keeps its
    /// generating card alive while it does (no process serves the turn yet,
    /// so nothing else beats and the card read «No signal» at two minutes,
    /// inviting a Stop that threw the download away) and says how far it has
    /// got. Here the download crawls and then turns out not to be the pinned
    /// bytes, so the reply ends saying that.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_reply_installing_the_pinned_build_keeps_beating_and_says_how_far_it_is() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-installing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (url, registry) = opencode_pin::tests::serve_slowly(vec![0u8; 3_000_000], Duration::ZERO, 12, Duration::from_millis(100)).await;
        let pin = opencode_pin::Source {
            root: dir.join("root"),
            build: *opencode_pin::build_for("linux-x64").unwrap(),
            registries: vec![url],
        };
        let wallet = env(&[
            ("MAFOLD_WALLET_KEY", "mrk_installing_0123456789"),
            ("MAFOLD_WALLET_BASE", "http://127.0.0.1:9/v1"),
            ("MAFOLD_WALLET_MODELS", "deepseek-v4-flash"),
        ]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = run_on(turn(&work, None, "hi", wallet), tx, Place::Pinned(pin)).await.unwrap();
        registry.abort();
        assert!(out.error.as_deref().is_some_and(|e| e.contains("doesn't match the pinned SHA-512")), "{:?}", out.error);
        let (mut pulses, mut notices) = (0, Vec::new());
        while let Ok(e) = rx.try_recv() {
            match e {
                AgentEvent::Pulse { .. } => pulses += 1,
                AgentEvent::Notice(n) => notices.push(n),
                _ => {}
            }
        }
        assert!(pulses >= 5, "the card kept beating through the install: {pulses} beats");
        assert!(
            notices.iter().any(|n| n.starts_with(&format!("Installing OpenCode {} — ", opencode_pin::VERSION)) && n.ends_with("/3.0 MB")),
            "{notices:?}"
        );
        assert!(notices.len() <= 6, "a line per quarter, not per chunk: {notices:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A turn's server is the turn's process from the moment it is spawned,
    /// not once it listens: the turn's `proc` — what the render loop
    /// journals as the turn's child — names it while a slow first start
    /// (up to 90 s) still waits for the listen line. A daemon that dies then
    /// (macOS and Windows have no parent-death signal) still leaves a
    /// journaled child for the restart to end. A Stop during the start ends
    /// the server and clears it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_starting_server_is_the_turns_process_before_it_listens() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-starting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // `sh serve …` runs the `serve` script in the turn's folder: a server
        // that never says it listens. (Not a script of our own exec'd: one
        // just written can read as busy while another test forks.)
        std::fs::write(work.join("serve"), "exec sleep 60\n").unwrap();
        let wallet = env(&[
            ("MAFOLD_WALLET_KEY", "mrk_starting_0123456789"),
            ("MAFOLD_WALLET_BASE", "http://127.0.0.1:9/v1"),
            ("MAFOLD_WALLET_MODELS", "deepseek-v4-flash"),
        ]);
        let t = turn(&work, None, "hi", wallet);
        let (proc, cancel) = (t.proc.clone(), t.cancel.clone());
        let (tx, _rx) = mpsc::unbounded_channel();
        let running = tokio::spawn(run_on(t, tx, Place::At { bin: PathBuf::from("/bin/sh"), root: dir.clone() }));
        let mut pid = None;
        for _ in 0..100 {
            pid = proc.pid();
            if pid.is_some() || running.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let still_starting = !running.is_finished();
        cancel.notify_one();
        let out = tokio::time::timeout(Duration::from_secs(10), running).await.expect("the Stop ends the start").unwrap().unwrap();
        let pid = pid.expect("the starting server serves the turn before it listens");
        assert!(still_starting, "named while the start was still waiting");
        assert!(out.stopped, "{:?}", out.error);
        assert_eq!(proc.pid(), None, "cleared once the start was given up");
        let gone = crate::platform::wait_until(Duration::from_secs(3), || !crate::platform::pid_running(pid));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the server went with the start");
    }

    /// Start `program args` from a thread of its own, the way a daemon's
    /// worker starts a turn's server, and let the thread end — which, for the
    /// kernel's parent-death signal, is the daemon dying. Its pid.
    #[cfg(target_os = "linux")]
    fn start_and_let_the_starter_end(program: &Path, args: &[&str], lifeline: bool) -> u32 {
        let (program, args) = (program.to_path_buf(), args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let mut cmd = crate::platform::command(&program);
                cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                if lifeline {
                    die_with_daemon(&mut cmd);
                }
                // A binary this test just wrote may still read as busy.
                for _ in 0..50 {
                    match cmd.spawn() {
                        Ok(c) => return c.id().unwrap(),
                        Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(Duration::from_millis(20)),
                        Err(e) => panic!("couldn't start {}: {e}", program.display()),
                    }
                }
                panic!("{} stayed busy", program.display());
            })
        })
        .join()
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn reap(pid: u32) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
            libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
        }
    }

    /// Linux: a turn's server dies with the daemon that started it, however
    /// the daemon dies — kill -9, the OOM killer, a crash — where no shutdown
    /// handler runs. `opencode serve` never reads its stdin, so a pipe the
    /// daemon holds is no lifeline (measured, 1.19.5: still serving 15 s
    /// after its stdin pipe closed); the kernel's parent-death signal is. With
    /// none, the child of a starter that is gone lives on (the control).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_turns_server_dies_with_the_daemon_that_started_it() {
        let sleep = Path::new("sleep");
        let with = start_and_let_the_starter_end(sleep, &["30"], true);
        let without = start_and_let_the_starter_end(sleep, &["30"], false);
        let gone = crate::platform::wait_until(Duration::from_secs(3), || !crate::platform::pid_running(with));
        let survived = crate::platform::pid_running(without);
        reap(with);
        reap(without);
        assert!(gone, "the server outlived the daemon that started it");
        assert!(survived, "without the lifeline the child lives on — so it is the lifeline that ended it");
    }

    /// The same through the harness's own start, with the real binary: a
    /// listening `opencode serve` whose daemon died hard — forgotten here, so
    /// neither `kill_on_drop` nor `Server::stop` runs, and its starter's thread
    /// ends — is gone. Run with MAFOLD_OPENCODE_BIN (and, to keep it off the
    /// network, inside `unshare --net`).
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs MAFOLD_OPENCODE_BIN (the pinned OpenCode binary)"]
    fn a_real_opencode_server_dies_with_its_daemon() {
        let bin = PathBuf::from(std::env::var("MAFOLD_OPENCODE_BIN").expect("set MAFOLD_OPENCODE_BIN"));
        let dir = std::env::temp_dir().join(format!("mafold-oc-lifeline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let spec = Spec {
            program: bin,
            pin: None,
            home: dir.join("home/ops_wallet"),
            workdir: work.to_string_lossy().into_owned(),
            prompt: String::new(),
            system: None,
            model: "deepseek-v4-flash".into(),
            wallet: Wallet { key: "mrk_lifeline_0123456789".into(), base: "http://127.0.0.1:9/v1".into(), models: vec!["deepseek-v4-flash".into()] },
            seat: Vec::new(),
            conv: "c1".into(),
            surface: "c1__ch-1__ops_wallet".into(),
            reads: Default::default(),
            draft: "draft-1".into(),
        };
        for d in ["config", "data", "cache", "state"] {
            std::fs::create_dir_all(spec.home.join(d)).unwrap();
        }
        seed_config(&spec.home).unwrap();
        let t0 = Instant::now();
        let (pid, url) = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let server = Server::start(&spec, &Default::default()).await.expect("the server listens");
                let got = (server.pid.unwrap(), server.url.clone());
                std::mem::forget(server);
                got
            })
        })
        .join()
        .unwrap();
        let listened = t0.elapsed();
        let t1 = Instant::now();
        let gone = crate::platform::wait_until(Duration::from_secs(5), || !crate::platform::pid_running(pid));
        // How it ended: SIGKILL is the lifeline's signal (not, say, a SIGPIPE
        // from the pipes its reader tasks dropped).
        // (A many-threaded process killed is reported to `waitpid` only once
        // its last thread is gone, a moment after its leader reads as a zombie.)
        let mut status = 0;
        let mut waited = 0;
        crate::platform::wait_until(Duration::from_secs(3), || {
            waited = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
            waited != 0
        });
        let why = std::io::Error::last_os_error();
        let reaped = waited == pid as i32;
        let signal = (reaped && libc::WIFSIGNALED(status)).then(|| libc::WTERMSIG(status));
        println!(
            "listened on {url} after {listened:?}; gone {:?} after its starter ended: {gone}, waitpid={waited} ({why}), status={status:#x}, signal {signal:?}",
            t1.elapsed()
        );
        reap(pid);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "opencode serve outlived the daemon that started it");
        assert_eq!(signal, Some(libc::SIGKILL), "ended by the lifeline");
    }

    /// A server a daemon that is gone left running is ended at the next start
    /// (`end_orphan`): every OS, where Linux's lifeline doesn't reach. Only
    /// when it is the very process the journal named (its start matches, not
    /// a later one given the same pid) and it runs our pinned OpenCode from
    /// under Mafold's runtimes folder — never anything else of the user's.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_daemons_server_is_ended_and_nothing_else_is() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("runtimes/opencode");
        let pinned = opencode_pin::binary_in(&root);
        std::fs::create_dir_all(pinned.parent().unwrap()).unwrap();
        let system_sleep = ["/usr/bin/sleep", "/bin/sleep"].into_iter().map(PathBuf::from).find(|p| p.is_file()).unwrap();
        std::fs::copy(&system_sleep, &pinned).unwrap();
        let ours = start_and_let_the_starter_end(&pinned, &["30"], false);
        let other = start_and_let_the_starter_end(&system_sleep, &["30"], false);
        let token = crate::platform::start_token;
        let checks = [
            ("someone else's program", end_orphan_under(&root, other, token(other))),
            ("a later process with the same pid", end_orphan_under(&root, ours, token(ours).map(|t| t + 1))),
            ("no start to tell it by", end_orphan_under(&root, ours, None)),
        ];
        let both_alive = crate::platform::pid_running(ours) && crate::platform::pid_running(other);
        let ended = end_orphan_under(&root, ours, token(ours));
        let ours_gone = !crate::platform::pid_running(ours);
        reap(ours);
        reap(other);
        let _ = std::fs::remove_dir_all(&dir);
        for (case, took) in checks {
            assert!(!took, "ended {case}");
        }
        assert!(both_alive, "nothing was ended before");
        assert!(ended && ours_gone, "the dead daemon's server was ended");
    }

    fn turn(workdir: &Path, session: Option<String>, prompt: &str, env: Vec<(String, String)>) -> Turn {
        Turn {
            prompt: prompt.to_string(),
            conv: "c1".into(),
            surface: "c1__ch-1__ops_wallet".into(),
            draft: "draft-1".into(),
            workdir: workdir.to_string_lossy().into_owned(),
            session,
            model: None,
            effort: None,
            thinking: None,
            cancel: Arc::new(Notify::new()),
            system: Some("SYSMARK: you are @ops:wallet on Mafold.".into()),
            ask_file: None,
            steer_file: None,
            env,
            mount: Default::default(),
            skill_plugins: None,
            proc: Default::default(),
        }
    }

    /// An OpenAI-compatible provider on loopback that answers like DeepSeek:
    /// a bash call first (when it may), the tool's output after, and a slow
    /// answer to "SLOW". It records each request's bearer and body.
    #[cfg(unix)]
    mod fake {
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[derive(Default)]
        pub struct Seen {
            pub requests: Vec<(Option<String>, Value)>,
        }

        pub async fn start() -> (String, Arc<Mutex<Seen>>, tokio::task::JoinHandle<()>) {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/v1", l.local_addr().unwrap());
            let seen = Arc::new(Mutex::new(Seen::default()));
            let s2 = seen.clone();
            let task = tokio::spawn(async move {
                while let Ok((sock, _)) = l.accept().await {
                    let seen = s2.clone();
                    tokio::spawn(async move {
                        let _ = serve(sock, seen).await;
                    });
                }
            });
            (base, seen, task)
        }

        async fn serve(mut s: tokio::net::TcpStream, seen: Arc<Mutex<Seen>>) -> std::io::Result<()> {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let head_end = loop {
                let n = s.read(&mut chunk).await?;
                if n == 0 {
                    return Ok(());
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let header = |name: &str| {
                head.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                })
            };
            let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            while buf.len() < head_end + len {
                let n = s.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
            seen.lock().unwrap().requests.push((header("authorization"), body.clone()));
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let last_user = msgs.iter().rev().find(|m| m["role"] == "user").map(|m| m["content"].to_string()).unwrap_or_default();
            let tools: Vec<String> =
                body["tools"].as_array().into_iter().flatten().filter_map(|t| t["function"]["name"].as_str().map(str::to_string)).collect();
            let tool_out = msgs.iter().rev().find(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_string());
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n").await?;
            let model = body["model"].as_str().unwrap_or("m").to_string();
            let frame = |delta: Value, finish: Value| {
                format!(
                    "data: {}\n\n",
                    json!({ "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": model,
                            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
                )
            };
            s.write_all(frame(json!({ "role": "assistant", "content": "" }), Value::Null).as_bytes()).await?;
            if last_user.contains("SLOW") {
                for i in 0..60 {
                    s.write_all(frame(json!({ "content": format!("w{i} ") }), Value::Null).as_bytes()).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                s.write_all(frame(json!({}), json!("stop")).as_bytes()).await?;
            } else if tools.iter().any(|t| t == "bash") && tool_out.is_none() {
                s.write_all(frame(json!({ "reasoning_content": "Let me run it." }), Value::Null).as_bytes()).await?;
                let call = json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
                    "function": { "name": "bash", "arguments": json!({ "command": "echo oc-integration-$((6*7))", "description": "Print a marker" }).to_string() } }] });
                s.write_all(frame(call, Value::Null).as_bytes()).await?;
                s.write_all(frame(json!({}), json!("tool_calls")).as_bytes()).await?;
            } else {
                let said = format!("The tool said {}", tool_out.unwrap_or_default().trim());
                for w in said.split(' ') {
                    s.write_all(frame(json!({ "content": format!("{w} ") }), Value::Null).as_bytes()).await?;
                }
                s.write_all(frame(json!({}), json!("stop")).as_bytes()).await?;
            }
            let usage = json!({ "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": model, "choices": [],
                "usage": { "prompt_tokens": 1000, "completion_tokens": 20, "total_tokens": 1020, "prompt_tokens_details": { "cached_tokens": 400 } } });
            s.write_all(format!("data: {usage}\n\ndata: [DONE]\n\n").as_bytes()).await?;
            s.shutdown().await
        }
    }

    /// A real turn through the harness: the OpenCode binary at
    /// `MAFOLD_OPENCODE_BIN` (the pinned 1.19.5), the wallet's key and base in
    /// `Turn::env`, a fake provider behind the base. A tool call runs bash and
    /// the answer quotes it; then, on the same session in a new server, a Stop
    /// mid-answer ends the turn as stopped and takes its server with it.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "needs MAFOLD_OPENCODE_BIN (the pinned OpenCode binary)"]
    async fn opencode_turn_against_fake_provider() {
        let Ok(bin) = std::env::var("MAFOLD_OPENCODE_BIN") else {
            panic!("set MAFOLD_OPENCODE_BIN to the pinned OpenCode binary");
        };
        let dir = std::env::temp_dir().join(format!("mafold-oc-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (base, seen, provider) = fake::start().await;
        let key = format!("mrk_it_{}", uuid::Uuid::new_v4().simple());
        // The user's own OpenCode, as one installed with its own installer
        // leaves it: a `~/.opencode` whose config must never load.
        let user_home = dir.join("user");
        std::fs::create_dir_all(user_home.join(".opencode")).unwrap();
        std::fs::write(dir.join("spy.md"), "SPYMARK: the user's own instructions").unwrap();
        std::fs::write(
            user_home.join(".opencode/opencode.json"),
            json!({ "instructions": [dir.join("spy.md")] }).to_string(),
        )
        .unwrap();
        let home = user_home.to_string_lossy().into_owned();
        let wallet = env(&[
            ("MAFOLD_WALLET_KEY", &key),
            ("MAFOLD_WALLET_BASE", &base),
            ("MAFOLD_WALLET_MODELS", "deepseek-v4-flash"),
            ("HOME", &home),
        ]);
        let place = || Place::At { bin: PathBuf::from(&bin), root: dir.join("root") };

        // 1. Tool call → bash → answer.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t0 = Instant::now();
        let t = turn(&work, None, "Run a shell command that prints a marker, then tell me what it said.", wallet.clone());
        let proc = t.proc.clone();
        let watch = tokio::spawn(async move {
            let mut pid = None;
            for _ in 0..600 {
                pid = pid.or(proc.pid());
                tokio::time::sleep(Duration::from_millis(50)).await;
                if pid.is_some() && proc.pid().is_none() {
                    break;
                }
            }
            pid
        });
        let out = run_on(t, tx, place()).await.expect("the turn ran");
        let took = t0.elapsed();
        let pid = watch.await.unwrap().expect("a server served the turn");
        let mut evs = Vec::new();
        while let Ok(e) = rx.try_recv() {
            evs.push(e);
        }
        println!("turn 1: {took:?}, {} events, outcome error={:?}", evs.len(), out.error);
        for e in evs.iter().filter(|e| !matches!(e, AgentEvent::Pulse { .. })) {
            println!("  {e:?}");
        }
        assert_eq!(out.error, None);
        assert!(out.produced && !out.stopped);
        let sid = out.session.clone().expect("a session");
        assert!(sid.starts_with("ses"), "{sid}");
        assert!(matches!(&evs[0], AgentEvent::Session(s) if *s == sid), "the session is named first: {:?}", evs[0]);
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::Thinking(t) if t == "Let me run it.")));
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::ToolCall { name, input, .. } if name == "bash" && input["command"] == "echo oc-integration-$((6*7))")));
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::ToolResult { text, .. } if text.contains("oc-integration-42"))));
        assert!(text(&evs).contains("The tool said oc-integration-42"), "{}", text(&evs));
        let Some(AgentEvent::Done { tokens, cost_usd, .. }) = evs.last() else { panic!("Done last: {:?}", evs.last()) };
        assert_eq!(*tokens, Some(2040));
        assert_eq!(*cost_usd, None);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.requests.len(), 2, "a tool round trip is two calls, and no title call");
            for (auth, body) in &seen.requests {
                assert_eq!(auth.as_deref(), Some(format!("Bearer {key}").as_str()), "the turn's key reached the provider");
                assert_eq!(body["model"], "deepseek-v4-flash");
                assert_eq!(body["max_tokens"], OUTPUT_TOKENS);
                let sys = body["messages"][0]["content"].to_string();
                assert!(sys.contains("SYSMARK"), "the turn's system text reached the model");
                assert!(!sys.contains("SPYMARK"), "the user's ~/.opencode config never loads");
                let tools: Vec<&str> = body["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
                assert!(tools.contains(&"bash") && !tools.contains(&"question"), "{tools:?}");
            }
            let second = &seen.requests[1].1["messages"];
            assert!(second.as_array().unwrap().iter().any(|m| m["reasoning_content"] == "Let me run it."), "reasoning goes back to DeepSeek");
        }
        assert!(!crate::platform::pid_alive(pid), "the turn's server is gone with the turn");
        let mut touched: Vec<String> = Vec::new();
        let mut stack = vec![user_home.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                touched.push(p.strip_prefix(&user_home).unwrap().to_string_lossy().into_owned());
                if p.is_dir() {
                    stack.push(p);
                }
            }
        }
        touched.sort();
        assert_eq!(touched, [".opencode", ".opencode/opencode.json"], "nothing of the user's OpenCode was written");
        assert!(dir.join("root/home/ops_wallet/data/opencode/opencode.db").is_file(), "the agent's own home holds its sessions");

        // 2. Same conversation, next turn: a new server resumes the session;
        //    Stop mid-answer.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t = turn(&work, Some(sid.clone()), "SLOW: count for me", wallet.clone());
        let cancel = t.cancel.clone();
        let proc = t.proc.clone();
        let run = tokio::spawn(run_on(t, tx, place()));
        let mut said = String::new();
        let t1 = loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Some(AgentEvent::Text(t))) => {
                    said.push_str(&t);
                    if said.contains("w2") {
                        break Instant::now();
                    }
                }
                Ok(Some(_)) => {}
                other => panic!("no answer streamed: {other:?}"),
            }
        };
        let pid = proc.pid().expect("serving");
        cancel.notify_one();
        let out = run.await.unwrap().expect("the stopped turn");
        let stop_took = t1.elapsed();
        println!("turn 2: stopped {stop_took:?} after Stop, said {said:?}");
        assert!(out.stopped && out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.session.as_deref(), Some(sid.as_str()), "the same session, resumed");
        assert!(stop_took < STOP_GRACE + Duration::from_secs(3), "{stop_took:?}");
        while let Ok(e) = rx.try_recv() {
            assert!(!matches!(e, AgentEvent::Done { .. }), "a stopped turn has no result card");
        }
        assert!(!crate::platform::pid_alive(pid), "the stopped turn's server is gone");
        let resumed = seen.lock().unwrap().requests.last().unwrap().1["messages"].as_array().unwrap().len();
        assert!(resumed > 3, "the resumed session carried the first turn ({resumed} messages)");

        // 3. Two turns of one agent starting together in a home that doesn't
        //    exist yet — two servers creating one database at once killed one
        //    of them, until the start lock.
        let fresh = || Place::At { bin: PathBuf::from(&bin), root: dir.join("root2") };
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        let t2 = Instant::now();
        let (a, b) = tokio::join!(
            run_on(turn(&work, None, "Run a shell command for A", wallet.clone()), tx_a, fresh()),
            run_on(turn(&work, None, "Run a shell command for B", wallet.clone()), tx_b, fresh()),
        );
        println!("turn 3: two at once on a fresh home in {:?}", t2.elapsed());
        for o in [a.expect("turn A"), b.expect("turn B")] {
            assert_eq!(o.error, None);
            assert!(o.produced && o.session.is_some());
        }

        provider.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shipped path on a computer that has never run Mafold Harness: the
    /// turn says it is setting up, downloads and checks the pinned build
    /// (network), installs it under `~/.mafold/runtimes`, then answers. Run
    /// with HOME set to a fresh folder under the temp dir — it refuses any
    /// other HOME, so it can never install into a real one.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "downloads OpenCode 1.19.5 (~80 MB); run with HOME=<a fresh temp folder>"]
    async fn a_turn_without_the_pinned_build_installs_it_first() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert!(home.starts_with(std::env::temp_dir()), "HOME must be a throwaway folder, not {}", home.display());
        assert!(!opencode_pin::installed(), "start from a computer without it");
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (base, seen, provider) = fake::start().await;
        let wallet = env(&[("MAFOLD_WALLET_KEY", "mrk_install"), ("MAFOLD_WALLET_BASE", &base), ("MAFOLD_WALLET_MODELS", "deepseek-v4-flash")]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t0 = Instant::now();
        let out = OpenCode.run(turn(&work, None, "Run a shell command that prints a marker", wallet), tx).await.unwrap();
        println!("installed and answered in {:?}", t0.elapsed());
        assert_eq!(out.error, None);
        let first = rx.try_recv().unwrap();
        assert!(matches!(&first, AgentEvent::Notice(n) if n.contains("Setting up Mafold Harness (OpenCode 1.19.5)")), "{first:?}");
        assert!(opencode_pin::installed());
        assert!(opencode_pin::binary().starts_with(home.join(".mafold/runtimes/opencode/1.19.5")));
        assert!(OpenCode.available() && OpenCode.cli_version().await == "1.19.5");
        assert_eq!(seen.lock().unwrap().requests.len(), 2);
        provider.abort();
    }
}
