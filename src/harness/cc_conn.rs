//! A CONNECTION to a `claude` process, and the pool that keeps warm ones.
//!
//! Until now every turn spawned its own `claude -p`, fed it the prompt as plain
//! text on stdin, and killed it 20s after the reply. That costs ~1.3s of cold
//! start per turn (measured: 1.19 / 1.33 / 1.36s with our real flags), gives us
//! no control channel at all, and — because `claude` SIGTERMs its own in-process
//! work when it exits — means a backgrounded subagent can never outlive the turn
//! that started it.
//!
//! A connection instead speaks `--input-format stream-json`: one JSON line per
//! user message, stdin stays OPEN, and the process can answer many turns. The
//! pool keeps a connection warm for [`IDLE_TTL`] after its last turn, so a
//! follow-up message lands on a process that is already up.
//!
//! Three things make this safe to leave running:
//!
//!   * **The reader is a task of the CONNECTION, not of the turn.** After our
//!     result, `claude` may keep writing (it runs a follow-up turn of its own
//!     when a background task finishes). Nobody reading stdout means the pipe
//!     fills at ~64KB and the process wedges — holding a session and whatever it
//!     spawned. The reader drains for the connection's whole life; between turns
//!     it keeps the task set and session id current and drops content frames.
//!   * **A connection with live in-process work is PINNED** (never evicted for
//!     idleness). Evicting it would kill the subagent/workflow it is running —
//!     that is correctness, not an optimization. Background *Bash* is not part
//!     of this: `bash_hook` already detaches those into their own session, which
//!     survives even a crash.
//!   * **Every failure falls back to the old behaviour.** No connection, a dead
//!     one, or a second concurrent turn in the same conversation → a fresh
//!     one-shot process, exactly as before.
//!
//! Sized from 7 days of this machine's real traffic: at a 30-minute TTL the pool
//! peaks at 7 processes (~1.8GB) against a same-minute peak of 6 today, and 71%
//! of turns land on a warm one.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// How long a connection stays warm with nothing to do. 30 minutes is the knee
/// of this machine's curve: it catches 71% of turns (median gap between two
/// turns of one conversation is 10.3 minutes) while an hour buys only 6 points
/// more and doubles the resident set.
const IDLE_TTL: Duration = Duration::from_secs(30 * 60);

/// Hard cap on warm connections. p99 of the measured curve is 5 and the 7-day
/// peak is 7; 16 is 2× headroom, not a target.
const MAX_WARM: usize = 16;

/// A connection PINNED by live in-process work can still be reclaimed once it
/// has been pinned this long — a task that never ends (a dev server, `tail -f`)
/// must not hold a process forever. The longest real background task measured on
/// this machine was 129 minutes, so this is above the real distribution, not in
/// the middle of it.
const MAX_PIN: Duration = Duration::from_secs(2 * 60 * 60);

/// At most this many connections may be pinned at once, so one fan-out cannot
/// fill the pool with processes nothing can evict. Past it a connection is
/// simply not pinned — i.e. it degrades to today's behaviour, it does not queue.
const MAX_PINNED: usize = 4;

/// Task kinds that live INSIDE the process, and therefore die with it. Bash is
/// deliberately absent: `bash_hook` detaches those before claude ever runs them.
const IN_PROCESS_TASKS: [&str; 5] = [
    "local_agent",
    "local_workflow",
    "mcp_task",
    "remote_agent",
    "in_process_teammate",
];

/// Is the warm pool on? Default yes; `MAFOLD_CC_POOL=0` forces every turn back
/// onto its own one-shot process (the kill switch this ships behind).
pub fn enabled() -> bool {
    !matches!(std::env::var("MAFOLD_CC_POOL").as_deref(), Ok("0") | Ok("off") | Ok("false"))
}

/// What must be IDENTICAL for two turns to share a process.
///
/// Model and effort could in principle be changed on a live connection
/// (`set_model`), but the system prompt cannot: `--append-system-prompt` is
/// fixed at spawn. Rather than keep a list of which knobs are re-settable —
/// a list that rots the moment the CLI adds one — everything that is passed at
/// spawn goes into the key, and a change simply gets a new process (1.5s).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct PoolKey(String);

impl PoolKey {
    pub fn new(
        conv: &str,
        surface: &str,
        workdir: &str,
        model: Option<&str>,
        effort: Option<&str>,
        thinking: Option<u32>,
        system: Option<&str>,
        env: &[(String, String)],
    ) -> Self {
        // A cheap stable digest of the long/structured inputs: we only ever
        // compare them.
        let digest = |s: &str| {
            let mut h: u64 = 1469598103934665603;
            for b in s.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(1099511628211);
            }
            format!("{h:x}")
        };
        let sys_sig = system.map(digest);
        // The SEAT this turn runs on — which Claude login the child uses
        // (`CLAUDE_SECURESTORAGE_CONFIG_DIR`). A connection is logged in as
        // exactly one account for its whole life, so a turn on another seat can
        // never reuse it; leaving this out of the key would answer one account's
        // message from another account's process, on that account's quota.
        let seat: Vec<String> = {
            let mut kv: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
            kv.sort();
            kv
        };
        let env_sig = if seat.is_empty() { "-".to_string() } else { digest(&seat.join("\u{1}")) };
        // The session to RESUME is deliberately not in here — it is matched at
        // [`take`] time against the session the connection actually holds. Keyed
        // on it, the first turn of a conversation (which resumes nothing) would
        // key on `None` while every turn after it keys on the id that first turn
        // produced, so the first warm connection could never be reused and sat
        // out its whole idle TTL for nothing.
        Self(format!(
            "{conv}\u{1}{surface}\u{1}{workdir}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{env_sig}",
            model.unwrap_or("-"),
            effort.unwrap_or("-"),
            thinking.map(|t| t.to_string()).unwrap_or_else(|| "-".into()),
            sys_sig.unwrap_or_else(|| "-".into()),
        ))
    }
}

/// State the reader task keeps current for the whole life of the connection —
/// read by the turn loop AND by the pool's eviction sweep, which is why it is
/// shared rather than owned by either.
struct Shared {
    /// The session id claude reports. Set once and then only if it changes
    /// (a compaction keeps it; a fork does not).
    session: Mutex<Option<String>>,
    /// Live in-process tasks, from the `background_tasks_changed` level signal.
    /// REPLACE semantics — the CLI documents that a consumer which only needs
    /// "is background work running" must swap its set rather than pair edges,
    /// so a missed bookend cannot wedge a stale indicator.
    tasks: Mutex<HashSet<String>>,
    /// Cleared when the reader sees EOF or the process is killed.
    alive: AtomicBool,
    /// Frames are forwarded to the turn only while this is set. Between turns
    /// claude's own follow-up output is dropped (matching today, where the
    /// process would already have been killed) rather than queued into the next
    /// turn, where it would read as that turn's work.
    in_turn: AtomicBool,
    /// The last few NON-JSON stdout lines — claude prints fatal reasons (a usage
    /// cap, an auth failure, a dead `--resume` id) as plain text, and they are
    /// the only explanation that exists when it then exits nonzero.
    plain: Mutex<VecDeque<String>>,
    /// Bounded stderr tail, drained concurrently so a chatty stderr can never
    /// fill its pipe and deadlock the turn.
    stderr: Mutex<String>,
    /// When this connection first became pinned, so [`MAX_PIN`] can bound it.
    pinned_since: Mutex<Option<Instant>>,
    /// THIS turn's draft / ask file / steer file. The hook callbacks read it,
    /// which is why the values live with the connection and not in the child's
    /// (unrewritable) environment.
    turn: Mutex<crate::turnenv::TurnEnv>,
    /// Control requests WE sent, awaiting their single reply.
    pending: Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<Value>>>,
}

impl Shared {
    fn note_plain(&self, line: &str) {
        const MAX_LINES: usize = 5;
        const MAX_CHARS: usize = 300;
        let mut s: String = line.chars().take(MAX_CHARS).collect();
        if line.chars().count() > MAX_CHARS {
            s.push('…');
        }
        let mut q = self.plain.lock().unwrap();
        q.push_back(s);
        while q.len() > MAX_LINES {
            q.pop_front();
        }
    }
}

/// One live `claude` process plus everything needed to hand it another turn.
pub struct Conn {
    pub key: PoolKey,
    /// Stable id — names this connection's turn file (see `crate::turnenv`).
    pub id: String,
    /// The draft id this process was SPAWNED with, i.e. the one its `MAFOLD_DRAFT`
    /// env still names. Later turns leave a forwarding address from it so an
    /// older `mafold attach` on the agent's `$PATH` still finds the live reply.
    pub spawn_draft: String,
    /// Lines to write to the child's stdin; dropping it closes stdin.
    out: Option<UnboundedSender<String>>,
    rx: UnboundedReceiver<Value>,
    shared: Arc<Shared>,
    pid: Option<u32>,
    /// Kept so dropping the Conn kills the process (`kill_on_drop`) and
    /// deregisters the pid from `live_children`.
    child: Option<tokio::process::Child>,
    _guard: super::ChildGuard,
    pub last_used: Instant,
    pub turns: u32,
    /// True once the hooks were registered over the control channel, so the
    /// caller knows it does NOT also need the `--settings` command hooks (which
    /// would fire a second, duplicate copy of every one of them).
    pub control_hooks: bool,
}

/// Callback ids we register. Prefixed because they share a namespace with any
/// other SDK host that might talk to this process.
///
/// The Bash hook is deliberately NOT here. Its whole mechanism is that the hook
/// PROCESS exits right after spawning the detached task, so init adopts it —
/// answered in-process instead, the task stays a child of the daemon, nobody
/// reaps it, and the completion monitor sees a zombie pid and reports the task
/// as still running forever. It stays a command hook, where that contract holds.
const CB_ASK: &str = "mf-ask";
const CB_STEER: &str = "mf-steer";

impl Conn {
    /// Spawn a process for `key`. `configure` receives the `Command` so the
    /// caller keeps ownership of every flag and env var — this module decides
    /// nothing about how claude is invoked, only how it is spoken to.
    pub async fn spawn(
        key: PoolKey,
        id: String,
        spawn_draft: String,
        mut cmd: Command,
        workdir: &str,
    ) -> Result<Self> {
        use std::process::Stdio;
        cmd.kill_on_drop(true)
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| super::spawn_err("claude", workdir, e))?;
        let pid = child.id();
        let guard = super::ChildGuard::new(pid);
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let shared = Arc::new(Shared {
            session: Mutex::new(None),
            tasks: Mutex::new(HashSet::new()),
            alive: AtomicBool::new(true),
            in_turn: AtomicBool::new(false),
            plain: Mutex::new(VecDeque::new()),
            stderr: Mutex::new(String::new()),
            pinned_since: Mutex::new(None),
            turn: Mutex::new(Default::default()),
            pending: Mutex::new(Default::default()),
        });
        // stdin is owned by a WRITER task, not by the turn: a hook callback is
        // answered from its own task (the ask hook blocks for up to ten minutes
        // waiting for the user), and two writers on one handle would interleave
        // half-lines. Everything that talks to claude sends a line here.
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        {
            let mut si = stdin;
            tokio::spawn(async move {
                while let Some(line) = out_rx.recv().await {
                    if si.write_all(line.as_bytes()).await.is_err() || si.flush().await.is_err() {
                        break;
                    }
                }
                // Channel closed → drop the handle, which is the EOF a one-shot
                // run exits on.
            });
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_reader(stdout, shared.clone(), tx, out_tx.clone());
        if let Some(se) = child.stderr.take() {
            let sh = shared.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(se).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut buf = sh.stderr.lock().unwrap();
                    if buf.len() < 8192 {
                        buf.push_str(&l);
                        buf.push('\n');
                    }
                }
            });
        }
        Ok(Self {
            key,
            id,
            spawn_draft,
            out: Some(out_tx),
            rx,
            shared,
            pid,
            child: Some(child),
            _guard: guard,
            last_used: Instant::now(),
            turns: 0,
            control_hooks: false,
        })
    }

    fn send(&self, v: &Value) -> bool {
        match &self.out {
            Some(tx) => tx.send(format!("{v}\n")).is_ok(),
            None => false,
        }
    }

    /// Register our hooks over the CONTROL channel instead of `--settings`.
    ///
    /// The settings form makes claude spawn `mafold <hook>` as a separate
    /// process — for the steer hook, once per tool call — and that process then
    /// has to find this turn's files through the environment it was born with.
    /// Registered here they are answered in-process by [`handle_hook`], which
    /// reads the CURRENT turn directly.
    ///
    /// Returns false when the CLI doesn't answer (too old to speak this), and
    /// the caller then falls back to the settings hooks. Ten seconds is a
    /// handshake, not a turn: a CLI that hasn't replied by then isn't going to.
    pub async fn register_hooks(&mut self, ask: bool, steer: bool) -> bool {
        let mut pre: Vec<Value> = Vec::new();
        if ask {
            pre.push(serde_json::json!({ "matcher": "AskUserQuestion", "hookCallbackIds": [CB_ASK] }));
        }
        let mut hooks = serde_json::Map::new();
        if !pre.is_empty() {
            hooks.insert("PreToolUse".into(), Value::Array(pre));
        }
        if steer {
            hooks.insert(
                "PostToolUse".into(),
                serde_json::json!([{ "matcher": "*", "hookCallbackIds": [CB_STEER] }]),
            );
        }
        if hooks.is_empty() {
            self.control_hooks = true; // nothing to register — trivially fine
            return true;
        }
        let rid = format!("mf-init-{}", self.id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.shared.pending.lock().unwrap().insert(rid.clone(), tx);
        let sent = self.send(&serde_json::json!({
            "type": "control_request",
            "request_id": rid,
            "request": { "subtype": "initialize", "hooks": Value::Object(hooks) },
        }));
        if !sent {
            return false;
        }
        let ok = matches!(
            tokio::time::timeout(Duration::from_secs(10), rx).await,
            Ok(Ok(v)) if v["subtype"] == "success"
        );
        self.shared.pending.lock().unwrap().remove(&rid);
        self.control_hooks = ok;
        ok
    }

    pub fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Tell a PREWARMED connection which transcript it stands for.
    ///
    /// It was spawned with `--resume <id>` but has not run a turn, so it has
    /// not reported a `session_id` yet and [`take`] — which matches on the
    /// session a connection actually holds — would never hand it out. Only ever
    /// called before the first turn; a connection that has spoken reports its
    /// own id and this must not overwrite that.
    pub fn adopt_session(&mut self, sid: &str) {
        let mut s = self.shared.session.lock().unwrap();
        if s.is_none() {
            *s = Some(sid.to_string());
        }
    }

    pub fn session_id(&self) -> Option<String> {
        self.shared.session.lock().unwrap().clone()
    }

    /// Live in-process work → this connection must not be evicted.
    ///
    /// ⚠️ This only knows about tasks the daemon itself detached. A command the
    /// HARNESS backgrounded on its own (claude moves a long-running Bash off the
    /// critical path after its own timeout) never reaches this registry, so the
    /// connection parks unpinned and whatever claude says when that command
    /// finishes lands in [`Conn::drain_parked`]. That gap is why the drain
    /// reports instead of staying quiet.
    pub fn busy_with_tasks(&self) -> bool {
        !self.shared.tasks.lock().unwrap().is_empty()
    }

    /// Take everything that arrived while parked: how many frames, and a short
    /// preview of any ASSISTANT TEXT among them.
    ///
    /// The preview is what makes the log line actionable — frame counts alone
    /// can't tell "claude emitted a stray keep-alive" from "claude wrote a
    /// thousand words nobody will ever see". Tool frames and thinking deltas are
    /// deliberately not previewed: they are noise for this purpose.
    fn drain_parked(&mut self) -> (usize, String) {
        let mut frames = Vec::new();
        while let Ok(v) = self.rx.try_recv() {
            frames.push(v);
        }
        (frames.len(), orphaned_text_preview(&frames))
    }

    pub fn plain_tail(&self) -> Vec<String> {
        self.shared.plain.lock().unwrap().iter().cloned().collect()
    }

    pub fn stderr_text(&self) -> String {
        self.shared.stderr.lock().unwrap().clone()
    }

    /// Open the floor for a turn: drop anything claude said while parked, then
    /// start forwarding, then send the prompt. The drain is what keeps a
    /// follow-up turn claude ran on its own from being read as this turn's work.
    ///
    /// **Dropping is right; dropping in silence is not.** Whatever arrived while
    /// this connection was parked belongs to a turn whose reply the daemon has
    /// already finalized — forwarding it would put one turn's words in the next
    /// one's bubble. But it is not noise: the usual producer is a background
    /// task finishing and claude writing up the result, i.e. exactly the output
    /// somebody is waiting for.
    ///
    /// 2026-09-17: an hour of work (a release plus a 2,099-object backfill) was
    /// reported into a parked connection and eaten here without a trace. The
    /// person watching the chat saw a bot that had simply gone quiet for two
    /// hours, and the daemon log said nothing at all. So: still dropped, now
    /// loud — a line in the log is the difference between "a known gap" and
    /// "the bot is haunted".
    pub async fn begin_turn(&mut self, prompt: &str, env: crate::turnenv::TurnEnv) -> Result<()> {
        let (dropped, orphan) = self.drain_parked();
        if !orphan.is_empty() {
            eprintln!(
                "[cc-pool] ⚠️ discarded {dropped} frame(s) claude produced while parked \
(pid {}) — that reply belonged to an already-finalized turn and reached nobody: {orphan}",
                self.pid().unwrap_or(0)
            );
        }
        self.shared.plain.lock().unwrap().clear();
        *self.shared.turn.lock().unwrap() = env;
        self.shared.in_turn.store(true, Ordering::SeqCst);
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": prompt }] },
        });
        if !self.send(&msg) {
            anyhow::bail!("connection is closed");
        }
        Ok(())
    }

    /// The next frame of the running turn, or None when the process ended.
    pub async fn recv(&mut self) -> Option<Value> {
        self.rx.recv().await
    }

    /// Stop forwarding; the connection stays up.
    pub fn end_turn(&mut self) {
        self.shared.in_turn.store(false, Ordering::SeqCst);
        self.last_used = Instant::now();
        self.turns += 1;
    }

    /// Close stdin — the EOF a one-shot run needs in order to exit on its own.
    pub fn close_stdin(&mut self) {
        self.out.take();
    }

    pub fn kill(&mut self) {
        self.shared.alive.store(false, Ordering::SeqCst);
        self.out.take();
        if let Some(c) = self.child.as_mut() {
            let _ = c.start_kill();
        }
    }

    /// Reap after [`kill`] / after stdin close, bounded. `claude` normally exits
    /// within a beat of its final result; one that does not is killed rather
    /// than waited on forever (an unbounded wait here used to park whole turns).
    pub async fn wait_exit(&mut self, grace: Duration) -> Option<std::process::ExitStatus> {
        let c = self.child.as_mut()?;
        match tokio::time::timeout(grace, c.wait()).await {
            Ok(Ok(s)) => Some(s),
            Ok(Err(_)) => None,
            Err(_) => {
                let _ = c.start_kill();
                let _ = c.wait().await;
                None
            }
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::SeqCst);
        // `kill_on_drop` handles the process; closing stdin first gives it the
        // EOF it would rather exit on.
        self.out.take();
    }
}

/// Drain stdout for the connection's whole life. Parsing happens here (once)
/// so the task set and session id stay current even between turns, when nobody
/// is consuming frames.
fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Shared>,
    tx: UnboundedSender<Value>,
    out: UnboundedSender<String>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(l)) => {
                    let line = l.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(line) {
                        Ok(v) => {
                            // Control traffic is the CONNECTION's business and
                            // never the turn's — a hook callback forwarded into
                            // the turn loop would be parsed as model output.
                            if v["type"] == "control_response" {
                                let rid = v["response"]["request_id"].as_str().unwrap_or("");
                                if let Some(w) = shared.pending.lock().unwrap().remove(rid) {
                                    let _ = w.send(v["response"].clone());
                                }
                                continue;
                            }
                            if v["type"] == "control_request" {
                                if v["request"]["subtype"] == "hook_callback" {
                                    // Answered in ITS OWN task: the ask hook
                                    // blocks until the user taps the card, and
                                    // the reader must keep draining stdout the
                                    // whole time or the pipe fills at ~64KB.
                                    let sh = shared.clone();
                                    let out = out.clone();
                                    tokio::spawn(async move { handle_hook(sh, out, v).await });
                                }
                                continue;
                            }
                            if let Some(sid) = v["session_id"].as_str() {
                                let mut s = shared.session.lock().unwrap();
                                if s.as_deref() != Some(sid) {
                                    *s = Some(sid.to_string());
                                }
                            }
                            note_tasks(&shared, &v);
                            if shared.in_turn.load(Ordering::SeqCst) {
                                let _ = tx.send(v);
                            }
                        }
                        // Not stream-json → a breadcrumb, not a frame.
                        Err(_) => shared.note_plain(line),
                    }
                }
                _ => break,
            }
        }
        shared.alive.store(false, Ordering::SeqCst);
    });
}

/// Answer one `hook_callback`. The bodies are the SAME functions the standalone
/// `mafold ask-hook` / `bash-hook` / `steer-hook` commands run, so a CLI old
/// enough to need the command form is told exactly the same thing.
///
/// A callback we don't recognise, or one with nothing to say, is answered with
/// a bare success and no payload — the control-protocol equivalent of a command
/// hook printing nothing, which claude reads as "no opinion, carry on". Never
/// left unanswered: the tool call is BLOCKED until this reply lands.
async fn handle_hook(shared: Arc<Shared>, out: UnboundedSender<String>, v: Value) {
    let rid = v["request_id"].as_str().unwrap_or_default().to_string();
    let req = &v["request"];
    let cb = req["callback_id"].as_str().unwrap_or_default();
    let payload: Option<Value> = match cb {
        CB_ASK => {
            let f = shared.turn.lock().unwrap().ask.clone();
            let f = Some(f).filter(|s| !s.is_empty());
            // BLOCKS (up to ten minutes) — on the blocking pool, never on a
            // runtime worker.
            tokio::task::spawn_blocking(move || crate::ask_hook::response(f.as_deref()))
                .await
                .ok()
        }
        CB_STEER => {
            let f = shared.turn.lock().unwrap().steer.clone();
            if f.is_empty() { None } else { crate::steer_hook::response(&f) }
        }
        _ => None,
    };
    let mut resp = serde_json::json!({ "subtype": "success", "request_id": rid });
    if let Some(p) = payload {
        resp["response"] = p;
    }
    let _ = out.send(format!("{}\n", serde_json::json!({ "type": "control_response", "response": resp })));
}

/// Track in-process work from the task frames. `background_tasks_changed` is a
/// LEVEL signal carrying every live task, so it replaces the set outright; the
/// `task_started` / `task_notification` edges only refine it between levels.
fn note_tasks(shared: &Arc<Shared>, v: &Value) {
    if v["type"] != "system" {
        return;
    }
    let mut set = shared.tasks.lock().unwrap();
    match v["subtype"].as_str() {
        Some("background_tasks_changed") => {
            set.clear();
            if let Some(list) = v["tasks"].as_array() {
                for t in list {
                    let kind = t["task_type"].as_str().unwrap_or("");
                    if IN_PROCESS_TASKS.contains(&kind) {
                        if let Some(id) = t["task_id"].as_str() {
                            set.insert(id.to_string());
                        }
                    }
                }
            }
        }
        Some("task_started") => {
            let kind = v["task_type"].as_str().unwrap_or("");
            if IN_PROCESS_TASKS.contains(&kind) {
                if let Some(id) = v["task_id"].as_str() {
                    set.insert(id.to_string());
                }
            }
        }
        Some("task_notification") => {
            if let Some(id) = v["task_id"].as_str() {
                set.remove(id);
            }
        }
        Some("task_updated") => {
            let done = matches!(
                v["patch"]["status"].as_str(),
                Some("completed") | Some("failed") | Some("killed") | Some("cancelled")
            );
            if done {
                if let Some(id) = v["task_id"].as_str() {
                    set.remove(id);
                }
            }
        }
        _ => {}
    }
    drop(set);
    // Stamp when the pin STARTED, so MAX_PIN measures the pin and not the
    // connection's whole life.
    let busy = !shared.tasks.lock().unwrap().is_empty();
    let mut since = shared.pinned_since.lock().unwrap();
    match (busy, since.is_some()) {
        (true, false) => *since = Some(Instant::now()),
        (false, true) => *since = None,
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// the pool

fn pool() -> &'static Mutex<Vec<Conn>> {
    static P: OnceLock<Mutex<Vec<Conn>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

fn next_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

/// Keys with a prewarm in flight. Without this a burst of messages for one
/// conversation would each start a process, and all but one would sit out the
/// whole idle TTL doing nothing.
fn prewarming() -> &'static Mutex<HashSet<String>> {
    static P: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Reserve the right to prewarm `key`. False when one is already warm or one is
/// already on its way.
pub fn claim_prewarm(key: &PoolKey) -> bool {
    {
        let p = pool().lock().unwrap();
        if p.iter().any(|c| &c.key == key && c.alive()) {
            return false;
        }
    }
    prewarming().lock().unwrap().insert(key.0.clone())
}

/// Releases the [`claim_prewarm`] reservation on drop, so a panic or an early
/// return can't wedge the key shut.
pub struct PrewarmGuard(pub PoolKey);
impl Drop for PrewarmGuard {
    fn drop(&mut self) {
        prewarming().lock().unwrap().remove(&self.0 .0);
    }
}

/// A fresh connection id for a one-shot process (never pooled).
pub fn oneshot_id() -> String {
    next_id()
}

/// Copy a built `Command` so the SAME invocation can be spawned twice.
///
/// Needed for exactly one thing: the hook fallback. We spawn without the
/// `--settings` command hooks and try to register them over the control
/// channel; a CLI too old to answer that has to be re-spawned WITH them, and
/// the second command must differ from the first in that one argument and
/// nothing else. Reading the args back beats keeping a parallel list that
/// quietly stops matching what we actually pass.
pub fn clone_cmd(src: &Command) -> Command {
    let std = src.as_std();
    let mut c = Command::new(std.get_program());
    for a in std.get_args() {
        c.arg(a);
    }
    for (k, v) in std.get_envs() {
        match v {
            Some(v) => { c.env(k, v); }
            None => { c.env_remove(k); }
        }
    }
    if let Some(d) = std.get_current_dir() {
        c.current_dir(d);
    }
    // Not readable back from the builder, so re-applied rather than lost (it is
    // what keeps a console window from flashing on Windows).
    crate::platform::no_window(&mut c);
    c
}

/// Take the warm connection for `key`, if there is a live one. Taking REMOVES
/// it: a second concurrent turn in the same conversation finds nothing and
/// opens its own process, which is exactly what happens today (turns in one
/// conversation already run concurrently and fork the session).
/// `want_session` is the transcript this turn asked to resume. A warm
/// connection is only the right one when it is ALREADY on that session —
/// otherwise the turn would silently continue a different conversation thread.
/// `None` means "no prior session" (a first turn, or a deliberate reset), which
/// never reuses: starting fresh is exactly what was asked for.
/// [`take`], but if a prewarm for this key is still on its way, WAIT for it
/// instead of racing it.
///
/// Racing is what happens without this, and it loses: the caller reaches here
/// milliseconds after asking for the prewarm when the round trips in between
/// are fast (a local api, a warm cache), while the process it asked for needs
/// ~1.3s to come up. The turn then starts its own — two processes for one key,
/// one of them useless. Waiting costs nothing over that: the alternative was
/// paying the same startup on a process nobody else can use.
pub async fn take_or_wait(key: &PoolKey, want_session: Option<&str>) -> Option<Conn> {
    if let Some(c) = take(key, want_session) {
        return Some(c);
    }
    // Bounded by what a spawn costs, not by hope: past this the prewarm is not
    // coming (it failed, or this CLI is slower than any we have measured) and
    // the caller is better off starting its own.
    const WAIT: Duration = Duration::from_secs(8);
    let began = Instant::now();
    let until = began + WAIT;
    while Instant::now() < until {
        if !prewarming().lock().unwrap().contains(&key.0) {
            break; // nothing in flight — don't wait on something that isn't coming
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        if let Some(c) = take(key, want_session) {
            // The half of the startup this turn still had to pay for. Subtract
            // it from the prewarm's own duration (logged when it lands) and the
            // rest is what the overlap actually hid.
            eprintln!("[cc-pool] waited {}ms for the prewarm", began.elapsed().as_millis());
            return Some(c);
        }
    }
    take(key, want_session)
}

pub fn take(key: &PoolKey, want_session: Option<&str>) -> Option<Conn> {
    let want = want_session?;
    let mut p = pool().lock().unwrap();
    sweep(&mut p);
    let i = p
        .iter()
        .position(|c| &c.key == key && c.alive() && c.session_id().as_deref() == Some(want))?;
    let c = p.remove(i);
    let (pid, turns, warm) = (c.pid(), c.turns, p.len());
    drop(p);
    // One line per reuse, in the daemon log. This is how "did it actually come
    // back warm" is answered without attaching a debugger — the pid is the
    // whole proof, since a cold turn would have a new one.
    eprintln!("[cc-pool] reusing pid {} (turn #{}, {warm} still warm)", pid.unwrap_or(0), turns + 1);
    Some(c)
}

/// Park a connection for reuse. A dead one is dropped (which kills it).
pub fn put(conn: Conn) {
    if !enabled() || !conn.alive() {
        return;
    }
    let (pid, turns, pinned) = (conn.pid(), conn.turns, conn.busy_with_tasks());
    let mut p = pool().lock().unwrap();
    p.push(conn);
    sweep(&mut p);
    let warm = p.len();
    drop(p);
    eprintln!(
        "[cc-pool] parked pid {} after turn #{turns}{} ({warm} warm)",
        pid.unwrap_or(0),
        if pinned { ", PINNED (live in-process task)" } else { "" },
    );
}

/// A short, single-line preview of the ASSISTANT TEXT inside frames that are
/// about to be thrown away.
///
/// The preview is what makes the drain's log line actionable — a frame count
/// alone cannot tell "claude emitted a stray keep-alive" from "claude wrote a
/// thousand words nobody will ever see". Tool calls and thinking deltas are
/// deliberately not previewed: for this purpose they are noise, and a log line
/// full of tool json is a log line people learn to skip.
///
/// Empty string = nothing worth reporting, which is the common case and must
/// stay quiet.
fn orphaned_text_preview(frames: &[Value]) -> String {
    const PREVIEW: usize = 240;
    let mut text = String::new();
    for v in frames {
        // The two shapes the turn reader consumes (`claude_code.rs`): streaming
        // text deltas, and the completed-message form.
        if v["type"] == "stream_event" && v["event"]["delta"]["type"] == "text_delta" {
            if let Some(t) = v["event"]["delta"]["text"].as_str() {
                text.push_str(t);
            }
        } else if v["type"] == "assistant" {
            if let Some(blocks) = v["message"]["content"].as_array() {
                for b in blocks {
                    if let Some(t) = b["text"].as_str() {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    if flat.is_empty() {
        return String::new();
    }
    let shown: String = flat.chars().take(PREVIEW).collect();
    format!("{shown}{}", if flat.chars().count() > PREVIEW { "…" } else { "" })
}

/// Drop everything — called before the daemon re-execs itself so an update
/// never leaves orphaned processes holding sessions.
pub fn shutdown_all() {
    let mut p = pool().lock().unwrap();
    for mut c in p.drain(..) {
        c.kill();
    }
}

/// Evict the dead, the idle-past-TTL, and the over-long pins; then enforce
/// [`MAX_WARM`] oldest-first. A pinned connection is skipped — killing it would
/// kill the subagent or workflow running inside it.
fn sweep(p: &mut Vec<Conn>) {
    let now = Instant::now();
    let mut pinned_left = MAX_PINNED;
    p.retain_mut(|c| {
        if !c.alive() {
            return false;
        }
        let idle = now.saturating_duration_since(c.last_used);
        if idle < IDLE_TTL {
            return true;
        }
        // Idle past the TTL — keep it only if live in-process work would die
        // with it, and only while that pin is within MAX_PIN and under the
        // pinned budget.
        if c.busy_with_tasks() && pinned_left > 0 {
            let pinned_for = c
                .shared
                .pinned_since
                .lock()
                .unwrap()
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or_default();
            if pinned_for < MAX_PIN {
                pinned_left -= 1;
                return true;
            }
        }
        c.kill();
        false
    });
    while p.len() > MAX_WARM {
        // Oldest idle first; never an unpinned-but-working one, which cannot
        // happen anyway (a connection in the pool is not running a turn).
        let Some(i) = p
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.busy_with_tasks())
            .min_by_key(|(_, c)| c.last_used)
            .map(|(i, _)| i)
        else {
            break;
        };
        let mut c = p.remove(i);
        c.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model: &str) -> PoolKey {
        PoolKey::new("c1", "s1", "/tmp", Some(model), None, None, None, &[])
    }

    fn seat(dir: &str) -> Vec<(String, String)> {
        vec![("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), dir.into())]
    }

    fn test_shared() -> Arc<Shared> {
        Arc::new(Shared {
            session: Mutex::new(None),
            tasks: Mutex::new(HashSet::new()),
            alive: AtomicBool::new(true),
            in_turn: AtomicBool::new(false),
            plain: Mutex::new(VecDeque::new()),
            stderr: Mutex::new(String::new()),
            pinned_since: Mutex::new(None),
            turn: Mutex::new(Default::default()),
            pending: Mutex::new(Default::default()),
        })
    }

    #[test]
    fn key_separates_every_spawn_time_knob() {
        assert_eq!(key("opus"), key("opus"));
        assert_ne!(key("opus"), key("haiku"));
        let a = PoolKey::new("c1", "s1", "/tmp", None, None, None, Some("you are x"), &[]);
        let b = PoolKey::new("c1", "s1", "/tmp", None, None, None, Some("you are y"), &[]);
        assert_ne!(a, b, "a changed system prompt cannot reuse a process");
        let c = PoolKey::new("c1", "s1", "/tmp", None, None, Some(8000), None, &[]);
        let d = PoolKey::new("c1", "s1", "/tmp", None, None, Some(9000), None, &[]);
        assert_ne!(c, d, "thinking budget is spawn-time env");
        // The SEAT is the one that would answer a message on the wrong account.
        let s1 = PoolKey::new("c1", "s1", "/tmp", None, None, None, None, &seat("/a"));
        let s2 = PoolKey::new("c1", "s1", "/tmp", None, None, None, None, &seat("/b"));
        assert_ne!(s1, s2, "two Claude logins are never one process");
        assert_eq!(s1, PoolKey::new("c1", "s1", "/tmp", None, None, None, None, &seat("/a")));
        // Two conversations are never the same process, whatever else matches.
        assert_ne!(
            PoolKey::new("c1", "s1", "/tmp", None, None, None, None, &[]),
            PoolKey::new("c2", "s1", "/tmp", None, None, None, None, &[]),
        );
    }

    /// Reuse is gated on the session the connection actually HOLDS, not on what
    /// the key says: a turn that asked to resume nothing gets a fresh process
    /// (that is what "no prior session" means), and a turn asking for a
    /// different transcript must never land on this one.
    #[test]
    fn a_turn_with_no_prior_session_never_takes_a_warm_one() {
        let k = key("opus");
        assert!(take(&k, None).is_none(), "no session asked for → no reuse");
    }

    #[test]
    fn pin_tracks_the_level_signal_and_ignores_bash() {
        let shared = test_shared();
        // A backgrounded subagent pins.
        note_tasks(
            &shared,
            &serde_json::json!({"type":"system","subtype":"task_started",
                "task_id":"t1","task_type":"local_agent"}),
        );
        assert_eq!(shared.tasks.lock().unwrap().len(), 1);
        assert!(shared.pinned_since.lock().unwrap().is_some());
        // A detached Bash task does NOT — bash_hook already moved it out of
        // claude's kill radius, so residency buys it nothing.
        note_tasks(
            &shared,
            &serde_json::json!({"type":"system","subtype":"task_started",
                "task_id":"t2","task_type":"local_bash"}),
        );
        assert_eq!(shared.tasks.lock().unwrap().len(), 1);
        // The level signal REPLACES the set.
        note_tasks(
            &shared,
            &serde_json::json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}),
        );
        assert!(shared.tasks.lock().unwrap().is_empty());
        assert!(shared.pinned_since.lock().unwrap().is_none(), "unpinned clears the clock");
    }

    #[test]
    fn task_completion_edges_unpin() {
        let shared = test_shared();
        note_tasks(
            &shared,
            &serde_json::json!({"type":"system","subtype":"task_started",
                "task_id":"t1","task_type":"local_workflow"}),
        );
        note_tasks(
            &shared,
            &serde_json::json!({"type":"system","subtype":"task_updated",
                "task_id":"t1","patch":{"status":"completed"}}),
        );
        assert!(shared.tasks.lock().unwrap().is_empty());
    }

    /// The plain-text tail is a breadcrumb trail for a failed run, not a
    /// transcript: bounded in both line count and line length so a chatty
    /// non-JSON stream can't grow it without limit.
    #[test]
    fn plain_tail_is_bounded_in_lines_and_line_length() {
        let shared = test_shared();
        for i in 0..20 {
            shared.note_plain(&format!("line {i}"));
        }
        let t = shared.plain.lock().unwrap().clone();
        assert_eq!(t.len(), 5, "keeps only the tail");
        assert_eq!(t.back().unwrap(), "line 19", "keeps the LAST lines, not the first");
        drop(t);
        shared.note_plain(&"x".repeat(1000));
        let t = shared.plain.lock().unwrap();
        assert!(t.back().unwrap().chars().count() <= 301, "long line truncated");
    }

    #[test]
    fn kill_switch_is_honoured() {
        std::env::set_var("MAFOLD_CC_POOL", "0");
        assert!(!enabled());
        std::env::remove_var("MAFOLD_CC_POOL");
        assert!(enabled());
    }

    fn delta(t: &str) -> Value {
        serde_json::json!({
            "type": "stream_event",
            "event": { "type": "content_block_delta", "delta": { "type": "text_delta", "text": t } }
        })
    }

    /// The whole point of the preview: a drop that carried real words must be
    /// distinguishable, in the log, from a drop that carried none.
    #[test]
    fn a_dropped_reply_is_quoted_so_the_log_shows_what_was_lost() {
        let frames = vec![
            delta("回填跑完了:"),
            serde_json::json!({ "type": "stream_event",
                "event": { "type": "content_block_delta",
                           "delta": { "type": "thinking_delta", "thinking": "不该出现" } } }),
            delta(" 2,099 成功"),
        ];
        let p = orphaned_text_preview(&frames);
        assert!(p.contains("回填跑完了"), "{p}");
        assert!(p.contains("2,099 成功"), "{p}");
        assert!(!p.contains("不该出现"), "thinking 不进预览:{p}");
    }

    /// A completed `assistant` message counts too — a turn that never streamed
    /// still said something.
    #[test]
    fn a_completed_message_counts_as_words_too() {
        let frames = vec![serde_json::json!({
            "type": "assistant",
            "message": { "content": [ { "type": "text", "text": "done" },
                                      { "type": "tool_use", "name": "Bash" } ] }
        })];
        assert_eq!(orphaned_text_preview(&frames), "done");
    }

    /// Silence must stay silent. Keep-alives, tool traffic and an empty park are
    /// the common case; a warning that fires on those is a warning nobody reads.
    #[test]
    fn frames_without_words_say_nothing() {
        assert_eq!(orphaned_text_preview(&[]), "");
        assert_eq!(
            orphaned_text_preview(&[
                serde_json::json!({ "type": "system", "subtype": "init" }),
                serde_json::json!({ "type": "stream_event",
                    "event": { "type": "content_block_delta",
                               "delta": { "type": "input_json_delta", "partial_json": "{\"a\":" } } }),
            ]),
            ""
        );
        // Whitespace-only is nothing, not something.
        assert_eq!(orphaned_text_preview(&[delta("  \n  ")]), "");
    }

    /// Long output is truncated (a log line, not a transcript) but marked, so
    /// nobody reads the tail as the whole of what was lost.
    #[test]
    fn a_long_lost_reply_is_cut_but_says_it_was_cut() {
        let long = "字".repeat(1000);
        let p = orphaned_text_preview(&[delta(&long)]);
        assert!(p.ends_with('…'), "{p}");
        assert_eq!(p.chars().count(), 241, "240 chars + the ellipsis");
    }
}
