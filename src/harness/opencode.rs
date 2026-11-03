//! Your own OpenCode install as a runtime (`opencode-own`, [`OwnOpenCode`]),
//! on the owner's OpenCode Go plan. It normalizes OpenCode's server event
//! stream into [`AgentEvent`]s, so its turns render as the same cards every
//! other harness produces. Also here: a turn's fuel as the daemon hands it
//! over ([`Fuel`]: the wallet, or OpenCode Go), which Mafold Harness
//! (`super::mafold_harness`) reads the same way.
//!
//! What shapes this impl:
//! - **One `opencode serve` per turn.** A key in [`Turn::env`] is the reply's
//!   (the wallet's is minted for this one reply, `mintReplyKey`, and dies when
//!   it ends), so the process holding it ends with the turn too. Nothing is
//!   pooled or prewarmed: a warm process would be holding a dead key.
//! - **A server, not a stream on stdout.** OpenCode's headless surface is an
//!   HTTP server: a session, `prompt_async`, and one SSE stream (`/event`) for
//!   everything that happens. It listens on loopback, where every local user
//!   can reach it, and its tools run bash — so each turn's server gets a random
//!   password of its own (`OPENCODE_SERVER_PASSWORD`, basic auth).
//! - **Your own OpenCode is yours.** It runs the `opencode` you installed,
//!   with your config, plugins, agents and the Go login it already has;
//!   Mafold only pins the reply to OpenCode Go (adding the key from your
//!   Connections when that OpenCode has no Go login of its own) and makes it
//!   headless. Its sessions live with your own, which is how a conversation
//!   resumes on the next turn's fresh server.
//! - **Headless means nothing may ask.** Every permission OpenCode would stop
//!   to ask about is allowed, and its ask-the-user tool is off; an ask that
//!   still arrives is answered at once instead of holding the reply forever.
//! - **Its $ figure is not the bill.** Go is the owner's plan; OpenCode's own
//!   estimate is never reported as a cost.

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

use super::{AgentEvent, CapsSource, CommandOutcome, Harness, HarnessProbe, ModelCap, Turn, TurnOutcome};
use crate::client::Client;

/// What a turn without its fuel answers. The daemon puts the wallet's key, or
/// the owner's OpenCode Go key, in [`Turn::env`] for a reply and only then;
/// without either there is nothing this agent may run on.
pub const NO_WALLET_KEY: &str =
    "This agent runs on the wallet or OpenCode Go, but the reply has no key for either, so nothing ran.";

/// The provider id OpenCode knows the wallet by.
const PROVIDER: &str = "mafold";

/// OpenCode's own provider id for OpenCode Go — the one its models list and
/// its session headers (`x-opencode-session`) come with (measured, 1.19.5).
const GO_PROVIDER: &str = "opencode-go";

/// The Go model an agent starts on when it was given none
/// (`client-shared/newAgent.ts` `GO_MODEL_ORDER`'s first: a coding model).
const GO_DEFAULT_MODEL: &str = "kimi-k2.7-code";

/// Your own OpenCode's runtime id.
pub const OWN: &str = "opencode-own";

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

/// Your own OpenCode install, as a runtime of its own (OpenCode Go, ops
/// 10-31): the `opencode` on PATH, or where OpenCode's installer puts it.
/// One `opencode serve` per turn, in your own home, with your own config, and
/// on OpenCode Go alone.
pub struct OwnOpenCode;

#[async_trait]
impl Harness for OwnOpenCode {
    fn id(&self) -> &'static str {
        OWN
    }

    fn available(&self) -> bool {
        own_binary().is_some()
    }

    /// The OpenCode 1 line only: its server API is the one this driver was
    /// measured against. OpenCode 2 (its own GitHub line, not what the
    /// installer or npm hand out) is reported as not runnable here.
    fn runs_version(&self, version: Option<&str>) -> bool {
        version.is_none_or(|v| major(v) == Some(1))
    }

    fn names_session_first(&self) -> bool {
        true
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        let Some(bin) = own_binary() else {
            return Ok(TurnOutcome {
                error: Some("OpenCode isn't installed on this computer, so nothing ran. Install it there, or use Mafold Harness.".into()),
                ..Default::default()
            });
        };
        run_on(turn, sink, bin, None).await
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

    async fn cli_version(&self) -> String {
        match own_binary() {
            Some(bin) => tokio::task::spawn_blocking(move || super::version_of(&bin)).await.ok().flatten().unwrap_or_default(),
            None => String::new(),
        }
    }

    /// OpenCode Go's models, as your OpenCode lists them ([`go_probe`]).
    async fn caps(&self, _env: &[(String, String)], _version: &str) -> Option<HarnessProbe> {
        let bin = own_binary()?;
        go_probe(tokio::task::spawn_blocking(move || go_models(&bin)).await.ok()?)
    }
}

/// Your own `opencode`: on PATH, else where OpenCode's installer
/// (`opencode.ai/install`) puts it — `~/.opencode/bin`, which it adds to the
/// shell's rc files, not to the PATH a daemon started by the desktop app sees.
pub fn own_binary() -> Option<PathBuf> {
    super::resolve("opencode").or_else(|| {
        let exe = if cfg!(windows) { "opencode.exe" } else { "opencode" };
        let p = crate::platform::home_dir()?.join(".opencode").join("bin").join(exe);
        super::is_launchable(&p).then_some(p)
    })
}

/// A version's major number: `1.19.5` → 1, `opencode v2.2.4` → 2.
fn major(v: &str) -> Option<u64> {
    let t = v.split_whitespace().find(|t| t.chars().any(|c| c.is_ascii_digit()))?;
    t.trim_start_matches(['v', 'V']).split('.').next()?.parse().ok()
}

/// Is your own OpenCode signed in to OpenCode Go on this computer? Its
/// `/connect` login (`auth.json` under its data folder), or the variable it
/// reads Go's key from. Presence only: the key itself is never read here.
pub fn own_go_login() -> bool {
    own_go_login_in(std::env::var_os("XDG_DATA_HOME").map(PathBuf::from), std::env::var("OPENCODE_API_KEY").ok())
}

fn own_go_login_in(xdg_data: Option<PathBuf>, env_key: Option<String>) -> bool {
    if env_key.is_some_and(|k| !k.trim().is_empty()) {
        return true;
    }
    let Some(data) = xdg_data.or_else(|| crate::platform::home_dir().map(|h| h.join(".local").join("share"))) else {
        return false;
    };
    std::fs::read_to_string(data.join("opencode").join("auth.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| v.get(GO_PROVIDER).is_some_and(|e| e.is_object()))
}

// ── OpenCode Go's models, as an OpenCode lists them ────────────────────────

/// How long your own OpenCode's list is kept (it refreshes its models from
/// models.dev by itself).
const OWN_MODELS_FOR: Duration = Duration::from_secs(6 * 3600);
/// A listing that failed is tried again after this.
const MODELS_RETRY: Duration = Duration::from_secs(600);
/// The longest one listing may take.
const MODELS_LIST: Duration = Duration::from_secs(30);

/// `(path, size, mtime)` → when it was listed and what came back.
#[allow(clippy::type_complexity)]
static GO_MODELS: Mutex<Option<HashMap<PathBuf, (u64, Option<std::time::SystemTime>, Instant, Option<Vec<Value>>)>>> = Mutex::new(None);

/// The models OpenCode Go can run with your own OpenCode at `bin`, as it
/// lists them in your home (`opencode models opencode-go --verbose`), each
/// `{id, name}` — for the computer's report, which New Agent's model picker
/// reads. Empty when it can't tell. Blocking.
pub fn go_models(bin: &Path) -> Vec<Value> {
    let meta = std::fs::metadata(bin).ok();
    let (len, mtime) = (meta.as_ref().map_or(0, |m| m.len()), meta.as_ref().and_then(|m| m.modified().ok()));
    {
        let cache = GO_MODELS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((l, t, at, got)) = cache.as_ref().and_then(|c| c.get(bin)) {
            let keep = match got {
                Some(_) => at.elapsed() < OWN_MODELS_FOR,
                None => at.elapsed() < MODELS_RETRY,
            };
            if *l == len && *t == mtime && keep {
                return got.clone().unwrap_or_default();
            }
        }
    }
    let got = list_go_models(bin);
    GO_MODELS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(bin.to_path_buf(), (len, mtime, Instant::now(), got.clone()));
    got.unwrap_or_default()
}

fn list_go_models(bin: &Path) -> Option<Vec<Value>> {
    let mut cmd = crate::platform::std_command(bin);
    cmd.args(["models", GO_PROVIDER, "--verbose"]);
    // Go lists only once it has a key; a stand-in one, which nothing calls.
    let cfg = json!({ "autoupdate": false, "provider": { GO_PROVIDER: { "options": { "apiKey": "listing-only" } } } });
    for (k, v) in opencode_env(None, &cfg, "listing-only") {
        cmd.env(k, v);
    }
    let cwd = crate::platform::home_dir().unwrap_or_else(std::env::temp_dir);
    cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut out, &mut text);
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if started.elapsed() > MODELS_LIST => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let text = reader.join().ok()?;
    let models = parse_go_models(&text);
    (!models.is_empty()).then_some(models)
}

/// `opencode models <provider> --verbose`: each model's `provider/id` line,
/// then its JSON, pretty-printed (measured, 1.19.5). Go's models that can
/// call tools, as `{id, name}`, in OpenCode's order.
fn parse_go_models(text: &str) -> Vec<Value> {
    let mut models = Vec::new();
    let mut block = String::new();
    for line in text.lines() {
        if block.is_empty() && !line.starts_with('{') {
            continue;
        }
        block.push_str(line);
        block.push('\n');
        if line == "}" {
            if let Ok(m) = serde_json::from_str::<Value>(&block) {
                let ours = m["providerID"].as_str().is_none_or(|p| p == GO_PROVIDER);
                let tools = m["capabilities"]["toolcall"].as_bool() != Some(false);
                let live = m["status"].as_str() != Some("deprecated");
                if let (Some(id), true) = (m["id"].as_str(), ours && tools && live) {
                    let name = m["name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or(id);
                    models.push(json!({ "id": id, "name": name }));
                }
            }
            block.clear();
        }
    }
    models
}

/// Go's models ([`go_models`]) as a caps report: the Customize sheet's model
/// menu, drawn by the server from what this computer's OpenCode listed. No
/// tiers: OpenCode passes none, so the sheet offers none. `None` when the
/// listing came back empty — a listing that failed has learned nothing, and
/// must not empty a menu.
pub(super) fn go_probe(models: Vec<Value>) -> Option<HarnessProbe> {
    let models: Vec<ModelCap> = models
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            Some(ModelCap {
                // What `go_model` also takes for it.
                aliases: vec![format!("{GO_PROVIDER}/{id}")],
                display: m["name"].as_str().unwrap_or(&id).to_string(),
                id,
                resolved: None,
                description: None,
                efforts: Vec::new(),
            })
        })
        .collect();
    (!models.is_empty()).then(|| HarnessProbe { source: CapsSource::Handshake, models, ..Default::default() })
}

/// The ids of OpenCode Go's models, as your own OpenCode lists them on this
/// computer — `/model`'s examples and its check. Empty when it can't tell:
/// not installed, or the listing failed.
pub async fn go_model_ids() -> Vec<String> {
    let Some(bin) = own_binary() else { return Vec::new() };
    tokio::task::spawn_blocking(move || go_models(&bin))
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect()
}

/// One turn on the OpenCode at `bin`, in your own home. `xdg` stands in for
/// that home (the tests' door); `None` = yours, as your shell has it.
async fn run_on(turn: Turn, sink: UnboundedSender<AgentEvent>, bin: PathBuf, xdg: Option<PathBuf>) -> Result<TurnOutcome> {
    // Effort and thinking: the models are run as OpenCode serves them, with
    // no dial wired yet. No ask hook and no steer hook either, so a mid-turn
    // message arrives as the follow-up turn (`can_steer` false).
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
    let Some(fuel) = Fuel::from_env(&env, true) else {
        return Ok(TurnOutcome { error: Some(NO_WALLET_KEY.into()), ..Default::default() });
    };
    let model = match &fuel {
        Fuel::Wallet(wallet) => match pick_model(model.as_deref(), &wallet.models) {
            Ok(m) => m,
            Err(why) => return Ok(TurnOutcome { error: Some(why), ..Default::default() }),
        },
        Fuel::Go { .. } => go_model(model.as_deref()),
    };
    let spec = Spec {
        home: xdg.clone().unwrap_or_else(own_home),
        xdg,
        program: bin,
        workdir,
        prompt,
        system,
        model,
        fuel,
        seat: env,
        conv,
        surface,
        reads: mount.reads,
        draft,
    };
    run_turn(&spec, session.as_deref(), &cancel, &sink, &proc).await
}

/// What a turn runs on, as the daemon's fuel step handed it over.
#[derive(Clone, PartialEq)]
pub(super) enum Fuel {
    /// The owner's wallet, through the router.
    Wallet(Wallet),
    /// The owner's OpenCode Go plan: the key from their Connections, or none —
    /// your own OpenCode's own Go login. `base` stands in for Go's address
    /// (debug builds, [`dev_base`]).
    Go { key: Option<String>, base: Option<String> },
}

/// Written by hand for the same reason as [`Wallet`]'s.
impl std::fmt::Debug for Fuel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fuel::Wallet(w) => w.fmt(f),
            Fuel::Go { key, base } => f
                .debug_struct("Go")
                .field("key", &key.as_ref().map(|_| "<redacted>"))
                .field("base", base)
                .finish(),
        }
    }
}

impl Fuel {
    /// The wallet when its three variables are there; else OpenCode Go when
    /// its key is, or — for your own OpenCode, which has a Go login of its own
    /// or none — without one. `None` = nothing to run on.
    pub(super) fn from_env(env: &[(String, String)], own: bool) -> Option<Self> {
        if let Some(w) = Wallet::from_env(env, dev_base(env, "MAFOLD_DEV_WALLET_BASE")) {
            return (!own).then_some(Fuel::Wallet(w));
        }
        let key = env
            .iter()
            .rev()
            .find(|(n, _)| n == crate::fuel::GO_KEY_ENV)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty());
        (key.is_some() || own).then_some(Fuel::Go { key, base: dev_base(env, "MAFOLD_DEV_GO_BASE") })
    }

    /// The provider id OpenCode serves the turn through.
    fn provider(&self) -> &'static str {
        match self {
            Fuel::Wallet(_) => PROVIDER,
            Fuel::Go { .. } => GO_PROVIDER,
        }
    }

    /// Who answers a refused model call, for the reply's error.
    pub(super) fn who(&self) -> &'static str {
        match self {
            Fuel::Wallet(_) => "the wallet's router",
            Fuel::Go { .. } => "OpenCode Go",
        }
    }

    /// The context window the result card measures against: the one OpenCode
    /// is told on the wallet; Go's models bring their own.
    fn context_limit(&self) -> Option<u64> {
        match self {
            Fuel::Wallet(_) => Some(CONTEXT_TOKENS),
            Fuel::Go { .. } => None,
        }
    }

    /// The variable the key rides in, and the key — what the server is started
    /// with on top of the turn's env.
    fn key_env(&self) -> Option<(&'static str, &str)> {
        match self {
            Fuel::Wallet(w) => Some(("MAFOLD_WALLET_KEY", w.key.as_str())),
            Fuel::Go { key: Some(k), .. } => Some((crate::fuel::GO_KEY_ENV, k.as_str())),
            Fuel::Go { key: None, .. } => None,
        }
    }
}

/// A Go model as OpenCode names it within its provider: what the agent was
/// given (with or without the `opencode-go/` prefix), else the default.
pub(super) fn go_model(wanted: Option<&str>) -> String {
    wanted
        .map(|m| m.trim().trim_start_matches("opencode-go/").to_string())
        .filter(|m| !m.is_empty() && !m.contains('/'))
        .unwrap_or_else(|| GO_DEFAULT_MODEL.to_string())
}

/// Where your own OpenCode's agents keep what Mafold needs of them: the start
/// lock, one for every agent, since they share your one OpenCode home.
fn own_home() -> PathBuf {
    crate::platform::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".mafold").join("runtimes").join(OWN)
}

/// The wallet a turn runs on, as the daemon's fuel step handed it over.
#[derive(Clone, PartialEq)]
pub(super) struct Wallet {
    pub(super) key: String,
    /// The router's OpenAI-compatible base (ends in `/v1`).
    pub(super) base: String,
    /// What the key may run, in the router's order.
    pub(super) models: Vec<String>,
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

/// Debug builds only: `MAFOLD_DEV_WALLET_BASE` / `MAFOLD_DEV_GO_BASE` (the
/// turn's, else the daemon's) point the wallet at a local router, Go at a
/// local stand-in. A release build never sends a key anywhere but where it
/// belongs: the base the API minted a reply key for, and OpenCode's own Go.
fn dev_base(env: &[(String, String)], var: &str) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    env.iter()
        .rev()
        .find(|(k, _)| k == var)
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var(var).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The model this turn runs: the chat's pick when the wallet runs it, the
/// wallet's first when nothing was picked — and a refusal, not a substitute,
/// when the pick is a model this reply's key can't run.
pub(super) fn pick_model(wanted: Option<&str>, models: &[String]) -> std::result::Result<String, String> {
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

/// The config OpenCode gets in `OPENCODE_CONFIG_CONTENT`. A key is never in
/// it — `{env:MAFOLD_WALLET_KEY}`, `{env:MAFOLD_GO_KEY}` — so the config can be
/// logged as it is. Your own OpenCode gets only what a headless reply needs
/// on top of its own config: the model, the key when it has no Go login of
/// its own, and every permission answered.
fn config(fuel: &Fuel, model: &str) -> Value {
    let id = format!("{}/{model}", fuel.provider());
    let mut cfg = json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "share": "disabled",
        "model": id,
        "small_model": id,
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
    });
    let provider = match fuel {
        Fuel::Wallet(w) => {
            let mut list = Map::new();
            for m in &w.models {
                list.insert(m.clone(), json!({ "name": m, "limit": { "context": CONTEXT_TOKENS, "output": OUTPUT_TOKENS } }));
            }
            Some(json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "Mafold Wallet",
                "options": { "baseURL": w.base, "apiKey": "{env:MAFOLD_WALLET_KEY}" },
                "models": list,
            }))
        }
        // OpenCode's own Go provider — its models, its address, its session
        // headers — with the owner's key by reference.
        Fuel::Go { key, base } => {
            let mut options = Map::new();
            if key.is_some() {
                options.insert("apiKey".into(), json!(format!("{{env:{}}}", crate::fuel::GO_KEY_ENV)));
            }
            if let Some(b) = base {
                options.insert("baseURL".into(), json!(b));
            }
            (!options.is_empty()).then(|| json!({ "options": options }))
        }
    };
    if let Some(p) = provider {
        cfg["provider"] = json!({ fuel.provider(): p });
    }
    cfg
}

/// Which inherited variables the child must not see: only Mafold's own fuel
/// — your OpenCode's settings and logins (`OPENCODE_API_KEY` is Go's) are
/// its.
fn scrubbed<I: IntoIterator<Item = String>>(names: I) -> Vec<String> {
    names
        .into_iter()
        .filter(|n| {
            let up = n.to_ascii_uppercase();
            ["MAFOLD_WALLET_", "MAFOLD_GO_"].iter().any(|p| up.starts_with(p))
        })
        .collect()
}

/// What OpenCode itself is told: the reply's config and password, and not to
/// update itself mid-reply — its home, plugins and models are yours (`xdg`
/// stands in for your home in tests).
fn opencode_env(xdg: Option<&Path>, config: &Value, password: &str) -> Vec<(String, String)> {
    let mut env = vec![
        ("OPENCODE_CONFIG_CONTENT".to_string(), config.to_string()),
        ("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string()),
        ("OPENCODE_DISABLE_AUTOUPDATE".to_string(), "1".to_string()),
    ];
    if let Some(x) = xdg {
        for (k, d) in [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")] {
            env.push((k.to_string(), x.join(d).to_string_lossy().into_owned()));
        }
        env.push(("OPENCODE_TEST_HOME".to_string(), x.to_string_lossy().into_owned()));
    }
    env
}

/// Everything one turn needs, resolved.
struct Spec {
    program: PathBuf,
    /// Where the agents' start lock lives ([`own_home`]; a test's `xdg`).
    home: PathBuf,
    /// Stands in for your home (the tests' door); `None` = yours.
    xdg: Option<PathBuf>,
    workdir: String,
    prompt: String,
    system: Option<String>,
    model: String,
    fuel: Fuel,
    /// [`Turn::env`], applied verbatim (the fuel's key rides in it).
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
    std::fs::create_dir_all(&s.home).with_context(|| format!("couldn't create {}", s.home.display()))?;
    // A fresh home's database is created by the first server to start in it,
    // and two creating it at once kill one of them (measured, 1.19.5: two
    // turns of one agent starting together). Held until this turn's server
    // has used it; after that any number share it.
    let lock = start_lock(&s.home).await;
    let stopped = || Ok(TurnOutcome { stopped: true, ..Default::default() });
    let mut server = tokio::select! {
        r = Server::start(s, proc) => r?,
        _ = cancel.notified() => return stopped(),
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
    let mut map = Mapper::new(sink, &sid, &s.model).on(s.fuel.who(), s.fuel.context_limit());
    let _ = sink.send(AgentEvent::Stats(RunStats { model: Some(s.model.clone()), ..Default::default() }));

    let mut body = json!({
        "parts": [{ "type": "text", "text": s.prompt }],
        "model": { "providerID": s.fuel.provider(), "modelID": s.model },
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
                    error = Some(format!("{} kept refusing this reply ({attempt} tries): {message}", s.fuel.who()));
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
/// it finishes the reply on the owner's key into a session nobody reads,
/// then idles at ~240 MB for good, and the next daemon can't resume the turn
/// while it lives (`agent::pick_up`). Mafold Harness's shell commands get the
/// same lifeline (`super::computer`).
///
/// SIGKILL, not SIGTERM: nobody is left to follow a SIGTERM up, and the key
/// it holds spends the owner's money until it stops. Its SQLite home is
/// crash-safe. The kernel sends this when the THREAD that forked the server
/// ends; that is a tokio worker, which lives as long as the daemon's runtime
/// (this crate never calls `block_in_place`, which could retire one).
#[cfg(target_os = "linux")]
pub(super) fn die_with_daemon(cmd: &mut tokio::process::Command) {
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
pub(super) fn die_with_daemon(_cmd: &mut tokio::process::Command) {}

/// At a daemon's start, for each process the turns of the one before it were
/// running in (`drafts::Outbox::take_orphans`, `agent::pick_up`): when it
/// still runs and is a server of the OpenCode build Mafold Harness ran on
/// until it became part of mafold — one a daemon of an older mafold left
/// mid-reply — end it and wait (≤ 5 s) for it to go. Waiting for a server to
/// exit by itself — what a restart did — waits forever, and gave the reply
/// up. True when one was ended.
pub fn end_orphan(pid: u32, start: Option<u64>) -> bool {
    end_orphan_under(&retired_pin_root(), pid, start)
}

/// Where that OpenCode build was installed: `~/.mafold/runtimes/opencode`.
fn retired_pin_root() -> PathBuf {
    crate::platform::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".mafold").join("runtimes").join("opencode")
}

/// [`end_orphan`] for the builds under `root`. Ours = the very process
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
        if let Some((k, v)) = s.fuel.key_env() {
            cmd.env(k, v);
        }
        let cfg = config(&s.fuel, &s.model);
        for (k, v) in opencode_env(s.xdg.as_deref(), &cfg, &password) {
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
        // records whatever `proc` names), so the restart doesn't run the turn
        // again while it lives — on macOS and Windows nothing ends it. Both
        // guards go with the server, or with this future if the start fails
        // or is dropped.
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
    /// Who answers a refused model call ([`Fuel::who`]).
    who: &'static str,
    /// The context window the card measures against ([`Fuel::context_limit`]).
    limit: Option<u64>,
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
            who: "the wallet's router",
            limit: Some(CONTEXT_TOKENS),
        }
    }

    /// For a turn on `who`'s models, measured against `limit`.
    fn on(mut self, who: &'static str, limit: Option<u64>) -> Self {
        self.who = who;
        self.limit = limit;
        self
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
            "session.error" => match error_text(&p["error"], self.who) {
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
                    let step = step_stats(&part["tokens"], &self.model, self.limit);
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
            ended_on = info.get("error").filter(|e| e.is_object()).and_then(|e| error_text(e, self.who));
        }
        ended_on
    }
}

/// One model call's usage, as [`RunStats`] counts it: input INCLUDES cache
/// reads and writes (OpenCode's `input` excludes them), and output includes
/// reasoning (OpenCode reports it apart: 33 + 4 for a 37-token completion).
fn step_stats(t: &Value, model: &str, limit: Option<u64>) -> RunStats {
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
        context_limit_tokens: limit,
        context_basis: Some("last_request_input".into()),
        ..Default::default()
    }
}

/// A session error in words — `None` for an abort, which is a Stop, not a
/// failure. A refused model call keeps the router's status and its own
/// sentence, which is what the daemon's failure card reads
/// (`mafold_transcript::failure::classify`).
fn error_text(err: &Value, who: &str) -> Option<String> {
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
                Some(code) => format!("{who} answered {code}{kind}: {said}"),
                None => format!("{who} failed{kind}: {said}"),
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

    /// OpenCode wired to the wallet: the router door as its provider, the key
    /// by reference (never in the text), every ask allowed, the ask tool off.
    #[test]
    fn the_config_wires_opencode_to_the_wallet() {
        let models = vec!["deepseek-v4-flash".to_string(), "deepseek-v4-pro".to_string()];
        let w = Fuel::Wallet(Wallet { key: "mrk_x".into(), base: "https://api.mafold.ai/v1".into(), models });
        let c = config(&w, "deepseek-v4-pro");
        assert_eq!(c["model"], "mafold/deepseek-v4-pro");
        assert_eq!(c["small_model"], "mafold/deepseek-v4-pro");
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
        assert_eq!(c.as_object().unwrap().len(), 8, "a new key is a measured one: {c:#}");
        assert!(!c.to_string().contains("mrk_x"), "the key is never in the text");
    }

    /// Your own OpenCode keeps its own config: no providers switched off, no
    /// snapshot setting of ours — only the Go model, every permission
    /// answered, and the owner's key (by reference) when that OpenCode has no
    /// Go login.
    #[test]
    fn your_own_opencode_gets_only_what_a_headless_go_reply_needs() {
        let signed_in = config(&Fuel::Go { key: None, base: None }, "glm-5.3");
        assert_eq!(signed_in["model"], "opencode-go/glm-5.3");
        assert_eq!(signed_in["small_model"], "opencode-go/glm-5.3");
        for k in ["enabled_providers", "snapshot", "provider"] {
            assert!(signed_in.get(k).is_none(), "{k}: yours, not ours — {signed_in:#}");
        }
        assert_eq!(signed_in["permission"]["bash"], "allow");
        assert_eq!(signed_in["permission"]["question"], "deny");
        assert_eq!(signed_in["tools"]["question"], false);
        let keyed = config(&Fuel::Go { key: Some("sk-go-secret".into()), base: None }, "glm-5.3");
        assert_eq!(keyed["provider"], json!({ "opencode-go": { "options": { "apiKey": "{env:MAFOLD_GO_KEY}" } } }));
        assert!(keyed.get("enabled_providers").is_none());
        assert!(!keyed.to_string().contains("sk-go-secret"), "the key is never in the text");
        let dev = config(&Fuel::Go { key: Some("k".into()), base: Some("http://127.0.0.1:9/v1".into()) }, "glm-5.3");
        assert_eq!(dev["provider"]["opencode-go"]["options"]["baseURL"], "http://127.0.0.1:9/v1");
    }

    /// Fuel from the turn's env: the wallet's three variables, else the Go
    /// key, else — your own OpenCode only — its own Go login. Mafold Harness
    /// with neither, and your own OpenCode on the wallet, run on nothing.
    #[test]
    fn a_turn_runs_on_the_wallet_or_go_and_nothing_else() {
        let wallet = env(&[("MAFOLD_WALLET_KEY", "mrk_1"), ("MAFOLD_WALLET_BASE", "https://x/v1"), ("MAFOLD_WALLET_MODELS", "m")]);
        assert!(matches!(Fuel::from_env(&wallet, false), Some(Fuel::Wallet(_))));
        assert!(Fuel::from_env(&wallet, true).is_none(), "your own OpenCode doesn't run on the wallet");
        let go = env(&[(crate::fuel::GO_KEY_ENV, "sk-go")]);
        assert_eq!(Fuel::from_env(&go, false), Some(Fuel::Go { key: Some("sk-go".into()), base: None }));
        assert_eq!(Fuel::from_env(&go, true), Some(Fuel::Go { key: Some("sk-go".into()), base: None }));
        assert_eq!(Fuel::from_env(&[], false), None, "Mafold Harness with no key runs nothing");
        assert_eq!(Fuel::from_env(&[], true), Some(Fuel::Go { key: None, base: None }), "its own Go login");
        assert_eq!(format!("{:?}", Fuel::Go { key: Some("sk-go".into()), base: None }).contains("sk-go"), false);
        assert_eq!(go_model(Some("opencode-go/glm-5.3")), "glm-5.3");
        assert_eq!(go_model(Some(" glm-5.3 ")), "glm-5.3");
        assert_eq!(go_model(None), GO_DEFAULT_MODEL);
        assert_eq!(go_model(Some("deepseek/deepseek-v4-flash")), GO_DEFAULT_MODEL, "another provider's model isn't Go's");
    }

    /// Signed in to Go here: OpenCode's `/connect` login in its data folder,
    /// or the variable Go's key is read from. Another provider's login isn't.
    #[test]
    fn your_own_opencodes_go_login_is_its_auth_file_or_its_variable() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("opencode")).unwrap();
        assert!(!own_go_login_in(Some(dir.clone()), None), "no auth file");
        std::fs::write(dir.join("opencode/auth.json"), r#"{"anthropic":{"type":"api","key":"x"}}"#).unwrap();
        assert!(!own_go_login_in(Some(dir.clone()), None), "another provider's login");
        std::fs::write(dir.join("opencode/auth.json"), r#"{"opencode-go":{"type":"api","key":"x"}}"#).unwrap();
        assert!(own_go_login_in(Some(dir.clone()), None));
        assert!(own_go_login_in(Some(dir.join("nowhere")), Some("sk".into())), "the variable");
        assert!(!own_go_login_in(Some(dir.join("nowhere")), Some(" ".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What `opencode models opencode-go --verbose` prints (1.19.5): a
    /// `provider/id` line, then the model's JSON. Tool-less and deprecated
    /// models are left out; the order is OpenCode's.
    #[test]
    fn go_models_are_read_off_opencodes_listing() {
        let listing = r#"opencode-go/beam-free
{
  "id": "beam-free",
  "providerID": "opencode-go",
  "name": "Beam Free",
  "capabilities": {
    "toolcall": true
  },
  "status": "active"
}
opencode-go/old-one
{
  "id": "old-one",
  "providerID": "opencode-go",
  "name": "Old One",
  "capabilities": { "toolcall": true },
  "status": "deprecated"
}
opencode-go/kimi-k2.7-code
{
  "id": "kimi-k2.7-code",
  "providerID": "opencode-go",
  "name": "Kimi K2.7 Code",
  "capabilities": {
    "toolcall": true
  }
}
opencode-go/chatty
{
  "id": "chatty",
  "providerID": "opencode-go",
  "name": "Chatty",
  "capabilities": { "toolcall": false }
}
"#;
        assert_eq!(
            parse_go_models(listing),
            vec![json!({ "id": "beam-free", "name": "Beam Free" }), json!({ "id": "kimi-k2.7-code", "name": "Kimi K2.7 Code" })]
        );
        assert!(parse_go_models("Error: Provider not found: opencode-go").is_empty());
    }

    /// The model menu of an agent on Go is Go's models as OpenCode listed
    /// them, by OpenCode's own names, with no tiers — and a listing that came
    /// back empty reports nothing rather than an empty menu.
    #[test]
    fn go_models_become_the_agents_model_menu() {
        let p = go_probe(vec![
            json!({ "id": "kimi-k2.7-code", "name": "Kimi K2.7 Code" }),
            json!({ "id": "glm-5.3", "name": "GLM-5.3" }),
            json!({ "name": "no id" }),
        ])
        .expect("two models");
        assert_eq!(p.source, CapsSource::Handshake);
        assert_eq!(p.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["kimi-k2.7-code", "glm-5.3"]);
        assert_eq!(p.models[0].display, "Kimi K2.7 Code");
        assert_eq!(p.models[0].aliases, ["opencode-go/kimi-k2.7-code"]);
        assert!(p.models.iter().all(|m| m.efforts.is_empty()) && p.efforts.is_empty() && p.modes.is_empty());
        // What the server reads off it: a model menu, and nothing about tiers.
        let caps = mafold_core::mafold_types::HarnessCaps {
            harness: "opencode".into(),
            version: "1.19.5".into(),
            machine: "m".into(),
            machine_name: None,
            account: None,
            probed_at: 0,
            source: p.source,
            models: p.models,
            efforts: p.efforts,
            modes: p.modes,
        };
        assert!(caps.tells_models() && !caps.tells_efforts());
        assert!(go_probe(Vec::new()).is_none());
    }

    #[test]
    fn only_opencode_1_is_driven_as_your_own() {
        assert_eq!(major("1.19.5"), Some(1));
        assert_eq!(major("opencode v2.2.4"), Some(2));
        assert_eq!(major("garbage"), None);
        assert!(OwnOpenCode.runs_version(Some("1.19.5")));
        assert!(!OwnOpenCode.runs_version(Some("opencode v2.2.4")));
        assert!(OwnOpenCode.runs_version(None), "not installed: Install is offered");
    }

    /// Your own OpenCode keeps its settings and logins (Go's key may be in
    /// OPENCODE_API_KEY) and its own home; only Mafold's fuel is dropped. A
    /// test's `xdg` stands in for that home.
    #[test]
    fn the_child_env_drops_only_mafolds_fuel() {
        let names = ["OPENCODE_API_KEY", "OPENAI_API_KEY", "MAFOLD_WALLET_KEY", "mafold_wallet_base", "MAFOLD_GO_KEY", "PATH"];
        assert_eq!(scrubbed(names.map(String::from)), ["MAFOLD_WALLET_KEY", "mafold_wallet_base", "MAFOLD_GO_KEY"]);
        let env: HashMap<String, String> = opencode_env(None, &json!({}), "pw").into_iter().collect();
        assert_eq!(env["OPENCODE_SERVER_PASSWORD"], "pw");
        assert_eq!(env["OPENCODE_DISABLE_AUTOUPDATE"], "1");
        assert_eq!(env["OPENCODE_CONFIG_CONTENT"], "{}");
        for k in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "OPENCODE_TEST_HOME", "OPENCODE_PURE", "OPENCODE_DISABLE_PROJECT_CONFIG"] {
            assert!(!env.contains_key(k), "{k}: your own OpenCode's");
        }
        assert!(!env.contains_key("MAFOLD_GO_KEY"), "the key comes from the turn, not from here");
        let you = Path::new("/h/you");
        let env: HashMap<String, String> = opencode_env(Some(you), &json!({}), "pw").into_iter().collect();
        assert_eq!(env["XDG_CONFIG_HOME"], you.join("config").to_string_lossy());
        assert_eq!(env["XDG_DATA_HOME"], you.join("data").to_string_lossy());
        assert_eq!(env["XDG_STATE_HOME"], you.join("state").to_string_lossy());
        assert_eq!(env["XDG_CACHE_HOME"], you.join("cache").to_string_lossy());
        assert_eq!(env["OPENCODE_TEST_HOME"], you.to_string_lossy());
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
        assert_eq!(dev_base(&env(&[("MAFOLD_DEV_WALLET_BASE", " http://x/v1 ")]), "MAFOLD_DEV_WALLET_BASE").as_deref(), Some("http://x/v1"));
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

    #[test]
    fn the_listening_line_gives_the_url() {
        assert_eq!(listen_url("opencode server listening on http://127.0.0.1:4096").as_deref(), Some("http://127.0.0.1:4096"));
        assert_eq!(listen_url("opencode server listening on http://127.0.0.1:43719\n").as_deref(), Some("http://127.0.0.1:43719"));
        assert_eq!(listen_url("Warning: OPENCODE_SERVER_PASSWORD is not set; server is unsecured."), None);
        assert_eq!(listen_url("http://127.0.0.1:"), None);
    }

    /// Who answers on the wallet, as the Mapper says it by default.
    const ROUTER: &str = "the wallet's router";

    #[test]
    fn errors_read_in_words() {
        assert_eq!(error_text(&json!({ "name": "MessageAbortedError", "data": { "message": "Aborted" } }), ROUTER), None);
        let e = error_text(&json!({ "name": "APIError", "data": { "message": "Forbidden", "statusCode": 403, "isRetryable": false,
            "responseBody": "{\"error\":{\"message\":\"wallet_grant_required: @ops no longer lets @bot spend from their wallet\",\"type\":\"wallet_grant_required\"}}" } }), ROUTER)
        .unwrap();
        assert_eq!(mafold_transcript::failure::classify(&e).kind, mafold_transcript::failure::FailureKind::Grant);
        assert_eq!(
            error_text(&json!({ "name": "APIError", "data": { "message": "boom", "statusCode": 503, "isRetryable": true } }), ROUTER).unwrap(),
            "the wallet's router answered 503: boom"
        );
        assert_eq!(error_text(&json!({ "name": "UnknownError", "data": { "message": "x broke" } }), ROUTER).unwrap(), "x broke");
        // On OpenCode Go, the refusal is Go's: a key it turns away reads as a
        // credential, its limits as a limit — the same cards as any runtime.
        let go = error_text(&json!({ "name": "APIError", "data": { "message": "Unauthorized", "statusCode": 401,
            "responseBody": "{\"error\":{\"message\":\"Invalid API key\",\"type\":\"AuthError\"}}" } }), "OpenCode Go")
        .unwrap();
        assert_eq!(go, "OpenCode Go answered 401 (AuthError): Invalid API key");
        assert_eq!(mafold_transcript::failure::classify(&go).kind, mafold_transcript::failure::FailureKind::Auth);
    }

    /// Your own OpenCode handed the wallet (which it doesn't run on) is
    /// refused in the contract's words before anything is started — no
    /// server, no home.
    #[tokio::test]
    async fn your_own_opencode_on_the_wallet_is_refused() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-nokey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut t = turn(&dir, None, "hi", Vec::new());
        t.env = env(&[("MAFOLD_WALLET_KEY", "k"), ("MAFOLD_WALLET_BASE", "https://x/v1"), ("MAFOLD_WALLET_MODELS", "deepseek-v4-flash")]);
        let out = run_on(t, tx, PathBuf::from("/nonexistent/opencode"), Some(dir.join("you"))).await.unwrap();
        assert_eq!(out.error.as_deref(), Some(NO_WALLET_KEY));
        assert!(!out.produced && !out.stopped);
        assert!(rx.try_recv().is_err(), "nothing was said");
        assert!(!dir.join("you").exists(), "nothing was set up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A turn's server is the turn's process from the moment it is spawned,
    /// not once it listens: the turn's `proc` — what the render loop
    /// journals as the turn's child — names it while a slow first start
    /// (up to 90 s) still waits for the listen line. A daemon that dies then
    /// (macOS and Windows have no parent-death signal) still leaves a
    /// journaled child, so the restart doesn't run the turn twice. A Stop
    /// during the start ends the server and clears it.
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
        let t = turn(&work, None, "hi", env(&[(crate::fuel::GO_KEY_ENV, "sk-go-starting")]));
        let (proc, cancel) = (t.proc.clone(), t.cancel.clone());
        let (tx, _rx) = mpsc::unbounded_channel();
        let running = tokio::spawn(run_on(t, tx, PathBuf::from("/bin/sh"), Some(dir.join("you"))));
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
    #[ignore = "needs MAFOLD_OPENCODE_BIN (an OpenCode 1 binary)"]
    fn a_real_opencode_server_dies_with_its_daemon() {
        let bin = PathBuf::from(std::env::var("MAFOLD_OPENCODE_BIN").expect("set MAFOLD_OPENCODE_BIN"));
        let dir = std::env::temp_dir().join(format!("mafold-oc-lifeline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        let you = dir.join("you");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&you).unwrap();
        let spec = Spec {
            program: bin,
            home: you.clone(),
            xdg: Some(you),
            workdir: work.to_string_lossy().into_owned(),
            prompt: String::new(),
            system: None,
            model: GO_DEFAULT_MODEL.into(),
            fuel: Fuel::Go { key: Some("sk-go-lifeline-0123456789".into()), base: Some("http://127.0.0.1:9/v1".into()) },
            seat: Vec::new(),
            conv: "c1".into(),
            surface: "c1__ch-1__ops_wallet".into(),
            reads: Default::default(),
            draft: "draft-1".into(),
        };
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
    /// a later one given the same pid) and it runs the OpenCode build Mafold
    /// Harness used to install, from under Mafold's runtimes folder — never
    /// anything else of the user's.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_daemons_server_is_ended_and_nothing_else_is() {
        let dir = std::env::temp_dir().join(format!("mafold-oc-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("runtimes/opencode");
        let pinned = root.join("1.19.5").join("opencode");
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
    /// answer to "SLOW". Like OpenCode Go, it turns the key `sk-go-bad` away
    /// with a 401, and "FAIL429" gets a 429 (retry after 50 ms) every time.
    /// It records each request's bearer and body.
    #[cfg(unix)]
    mod fake {
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[derive(Default)]
        pub struct Seen {
            pub requests: Vec<(Option<String>, Value)>,
            /// Each request's `x-opencode-session` — the session id OpenCode Go
            /// routes and caches by.
            pub sessions: Vec<Option<String>>,
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
            {
                let mut seen = seen.lock().unwrap();
                seen.requests.push((header("authorization"), body.clone()));
                seen.sessions.push(header("x-opencode-session"));
            }
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let last_user = msgs.iter().rev().find(|m| m["role"] == "user").map(|m| m["content"].to_string()).unwrap_or_default();
            let tools: Vec<String> =
                body["tools"].as_array().into_iter().flatten().filter_map(|t| t["function"]["name"].as_str().map(str::to_string)).collect();
            let tool_out = msgs.iter().rev().find(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_string());
            let refusal = if header("authorization").as_deref() == Some("Bearer sk-go-bad") {
                Some(("401 Unauthorized", "", json!({ "error": { "message": "Invalid API key", "type": "AuthError" } })))
            } else if last_user.contains("FAIL429") {
                Some(("429 Too Many Requests", "retry-after-ms: 50\r\n", json!({ "error": { "message": "Rate limit exceeded", "type": "RateLimitError" } })))
            } else {
                None
            };
            if let Some((status, extra, body)) = refusal {
                let body = body.to_string();
                s.write_all(
                    format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
                        .as_bytes(),
                )
                .await?;
                return s.shutdown().await;
            }
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

    /// A real turn through your own OpenCode: the OpenCode binary at
    /// `MAFOLD_OPENCODE_BIN`, your home standing in as `xdg`, the owner's Go
    /// key in `Turn::env` and a stand-in for Go behind `MAFOLD_DEV_GO_BASE`.
    /// A tool call runs bash and the answer quotes it; then, on the same
    /// session in a new server, a Stop mid-answer ends the turn as stopped and
    /// takes its server with it; then two turns start together in a fresh
    /// home.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "needs MAFOLD_OPENCODE_BIN (an OpenCode 1 binary)"]
    async fn your_own_opencode_turn_against_a_stand_in_for_go() {
        let bin = PathBuf::from(std::env::var("MAFOLD_OPENCODE_BIN").expect("set MAFOLD_OPENCODE_BIN"));
        let dir = std::env::temp_dir().join(format!("mafold-oc-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (base, seen, provider) = fake::start().await;
        let key = format!("sk-go-{}", uuid::Uuid::new_v4().simple());
        let go = env(&[(crate::fuel::GO_KEY_ENV, &key), ("MAFOLD_DEV_GO_BASE", &base)]);
        let you = dir.join("you");

        // 1. Tool call → bash → answer.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t0 = Instant::now();
        let mut t = turn(&work, None, "Run a shell command that prints a marker, then tell me what it said.", go.clone());
        t.model = Some("opencode-go/glm-5.3".into());
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
        let out = run_on(t, tx, bin.clone(), Some(you.clone())).await.expect("the turn ran");
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
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::ToolCall { name, input, .. } if name == "bash" && input["command"] == "echo oc-integration-$((6*7))")));
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::ToolResult { text, .. } if text.contains("oc-integration-42"))));
        assert!(text(&evs).contains("The tool said oc-integration-42"), "{}", text(&evs));
        assert!(evs.iter().any(|e| matches!(e, AgentEvent::Stats(s) if s.model.as_deref() == Some("glm-5.3"))));
        let Some(AgentEvent::Done { cost_usd, .. }) = evs.last() else { panic!("Done last: {:?}", evs.last()) };
        assert_eq!(*cost_usd, None, "OpenCode's estimate is never the bill");
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.requests.len(), 2, "a tool round trip is two calls, and no title call");
            for ((auth, body), session) in seen.requests.iter().zip(&seen.sessions) {
                assert_eq!(auth.as_deref(), Some(format!("Bearer {key}").as_str()), "the owner's Go key reached Go");
                assert_eq!(body["model"], "glm-5.3", "the agent's model, Go's prefix dropped");
                assert_eq!(session.as_deref(), Some(sid.as_str()), "Go is told the session");
                let sys = body["messages"][0]["content"].to_string();
                assert!(sys.contains("SYSMARK"), "the turn's system text reached the model");
                let tools: Vec<&str> = body["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
                assert!(tools.contains(&"bash") && !tools.contains(&"question"), "{tools:?}");
            }
        }
        assert!(!crate::platform::pid_alive(pid), "the turn's server is gone with the turn");
        assert!(you.join("data/opencode/opencode.db").is_file(), "its sessions live with yours");

        // 2. Same conversation, next turn: a new server resumes the session;
        //    Stop mid-answer.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t = turn(&work, Some(sid.clone()), "SLOW: count for me", go.clone());
        let cancel = t.cancel.clone();
        let proc = t.proc.clone();
        let run = tokio::spawn(run_on(t, tx, bin.clone(), Some(you.clone())));
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

        // 3. Two turns starting together in a home that doesn't exist yet —
        //    two servers creating one database at once killed one of them,
        //    until the start lock.
        let fresh = Some(dir.join("you2"));
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        let t2 = Instant::now();
        let (a, b) = tokio::join!(
            run_on(turn(&work, None, "Run a shell command for A", go.clone()), tx_a, bin.clone(), fresh.clone()),
            run_on(turn(&work, None, "Run a shell command for B", go.clone()), tx_b, bin.clone(), fresh.clone()),
        );
        println!("turn 3: two at once on a fresh home in {:?}", t2.elapsed());
        for o in [a.expect("turn A"), b.expect("turn B")] {
            assert_eq!(o.error, None);
            assert!(o.produced && o.session.is_some());
        }

        provider.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Your own OpenCode on OpenCode Go: the binary at `MAFOLD_OPENCODE_BIN`
    /// standing in for the one you installed, your home standing in as `xdg`.
    /// Signed in to Go there already, it runs on that login and your own
    /// config loads (your instructions reach the model); with no Go login of
    /// its own, the owner's key from Connections is what it sends. Its
    /// sessions are yours, in your OpenCode's data.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "needs MAFOLD_OPENCODE_BIN (an OpenCode 1 binary)"]
    async fn your_own_opencode_runs_a_turn_on_its_go_login_or_the_owners_key() {
        let bin = PathBuf::from(std::env::var("MAFOLD_OPENCODE_BIN").expect("set MAFOLD_OPENCODE_BIN"));
        let dir = std::env::temp_dir().join(format!("mafold-oc-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (base, seen, provider) = fake::start().await;
        let xdg = dir.join("you");
        std::fs::create_dir_all(xdg.join("config/opencode")).unwrap();
        std::fs::create_dir_all(xdg.join("data/opencode")).unwrap();
        std::fs::write(dir.join("mine.md"), "OWNMARK: your own instructions").unwrap();
        std::fs::write(xdg.join("config/opencode/opencode.json"), json!({ "instructions": [dir.join("mine.md")] }).to_string()).unwrap();
        std::fs::write(xdg.join("data/opencode/auth.json"), r#"{"opencode-go":{"type":"api","key":"sk-your-own-go"}}"#).unwrap();
        let dev = env(&[("MAFOLD_DEV_GO_BASE", &base)]);

        // 1. Signed in to Go: its own login, no key from Mafold.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = run_on(turn(&work, None, "Run a shell command that prints a marker, then tell me what it said.", dev.clone()), tx, bin.clone(), Some(xdg.clone()))
            .await
            .expect("the turn ran");
        let mut evs = Vec::new();
        while let Ok(e) = rx.try_recv() {
            evs.push(e);
        }
        assert_eq!(out.error, None, "{evs:?}");
        assert!(text(&evs).contains("The tool said oc-integration-42"));
        {
            let seen = seen.lock().unwrap();
            for (auth, body) in &seen.requests {
                assert_eq!(auth.as_deref(), Some("Bearer sk-your-own-go"), "its own Go login");
                assert_eq!(body["model"], GO_DEFAULT_MODEL);
                assert!(body["messages"][0]["content"].to_string().contains("OWNMARK"), "your own config loads");
            }
        }
        assert!(xdg.join("data/opencode/opencode.db").is_file(), "its sessions live with yours");

        // 2. No Go login of its own: the owner's key from Connections.
        std::fs::write(xdg.join("data/opencode/auth.json"), r#"{"anthropic":{"type":"api","key":"x"}}"#).unwrap();
        let keyed = env(&[("MAFOLD_DEV_GO_BASE", &base), (crate::fuel::GO_KEY_ENV, "sk-owners-go")]);
        let (tx, _rx) = mpsc::unbounded_channel();
        seen.lock().unwrap().requests.clear();
        let out = run_on(turn(&work, None, "Run a shell command that prints a marker, then tell me what it said.", keyed), tx, bin.clone(), Some(xdg.clone()))
            .await
            .expect("the turn ran");
        assert_eq!(out.error, None);
        assert!(seen.lock().unwrap().requests.iter().all(|(a, _)| a.as_deref() == Some("Bearer sk-owners-go")));
        provider.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Your own OpenCode on OpenCode Go when Go says no, and when you stop it.
    /// A key Go turns away ends the reply with Go's own 401 (a credential
    /// card). Go's 429s are retried by OpenCode itself, and the reply ends
    /// with the limit in words (a rate-limit card): Go's own 429 once
    /// OpenCode's tries run out (1.19.5, Go asking for 50 ms between them),
    /// Mafold's past [`RETRY_GIVE_UP`] if they never did. Stop mid-answer
    /// ends it as stopped, nothing failed, its server gone.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "needs MAFOLD_OPENCODE_BIN (an OpenCode 1 binary)"]
    async fn opencode_go_refusals_and_stop_end_the_reply_in_words() {
        use mafold_transcript::failure::{classify, FailureKind};
        let bin = PathBuf::from(std::env::var("MAFOLD_OPENCODE_BIN").expect("set MAFOLD_OPENCODE_BIN"));
        let dir = std::env::temp_dir().join(format!("mafold-oc-go-no-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let (base, seen, provider) = fake::start().await;
        let go = |key: &str| env(&[(crate::fuel::GO_KEY_ENV, key), ("MAFOLD_DEV_GO_BASE", &base)]);
        let you = || Some(dir.join("you"));

        // 1. A key Go turns away.
        let (tx, _rx) = mpsc::unbounded_channel();
        let t0 = Instant::now();
        let out = run_on(turn(&work, None, "hello", go("sk-go-bad")), tx, bin.clone(), you()).await.expect("the turn ran");
        let e = out.error.expect("a key Go turns away is an error");
        println!("401: {:?} → {e}", t0.elapsed());
        assert_eq!(e, "OpenCode Go answered 401 (AuthError): Invalid API key");
        assert_eq!(classify(&e).kind, FailureKind::Auth);

        // 2. Go's rate limit, every try.
        seen.lock().unwrap().requests.clear();
        let (tx, _rx) = mpsc::unbounded_channel();
        let t1 = Instant::now();
        let out = run_on(turn(&work, None, "FAIL429 now", go("sk-go-ok")), tx, bin.clone(), you()).await.expect("the turn ran");
        let e = out.error.expect("a reply Go keeps refusing ends");
        let calls = seen.lock().unwrap().requests.len() as u64;
        println!("429: {:?}, {calls} calls → {e}", t1.elapsed());
        assert!(
            e == "OpenCode Go answered 429 (RateLimitError): Rate limit exceeded"
                || e.starts_with(&format!("OpenCode Go kept refusing this reply ({RETRY_GIVE_UP} tries)")),
            "{e}"
        );
        assert_eq!(classify(&e).kind, FailureKind::RateLimit, "{e}");
        assert!(calls > 1, "Go was asked again: {calls}");

        // 3. Stop mid-answer.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let t = turn(&work, None, "SLOW: count for me", go("sk-go-ok"));
        let cancel = t.cancel.clone();
        let proc = t.proc.clone();
        let run = tokio::spawn(run_on(t, tx, bin.clone(), you()));
        let mut said = String::new();
        let t2 = loop {
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
        let stop_took = t2.elapsed();
        println!("stop: {stop_took:?} after Stop, said {said:?}");
        assert!(out.stopped && out.error.is_none(), "{:?}", out.error);
        assert!(stop_took < STOP_GRACE + Duration::from_secs(3), "{stop_took:?}");
        while let Ok(e) = rx.try_recv() {
            assert!(!matches!(e, AgentEvent::Done { .. }), "a stopped turn has no result card");
        }
        assert!(!crate::platform::pid_alive(pid), "the stopped turn's server is gone");

        provider.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
