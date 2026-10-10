//! Codex harness — drives OpenAI's local `codex` CLI headlessly
//! (`codex exec --json`) and normalizes its thread/item JSONL event stream into
//! [`AgentEvent`]s. The renderer (`crate::render`) turns those into the same chat
//! text + cards it produces for every other harness, so card rendering is
//! identical — this file only has to speak Codex's dialect.
//!
//! Differences from Claude Code that shape this impl:
//! - **No `--append-system-prompt`.** Codex has no system-prompt flag, so the
//!   daemon's mafold preamble (identity / conversation / embeddable cards) is
//!   folded into the front of the prompt instead.
//! - **Block-level streaming, not token-level.** Codex `--json` emits complete
//!   `item.completed` events (a whole assistant message, a whole reasoning block)
//!   rather than per-token deltas. The transcript is still interleaved in arrival
//!   order (§8) — narration, tool cards, results — just at item granularity.
//! - **Reasoning effort IS its thinking.** Codex has no separate extended-thinking
//!   budget, so the chat's `/think` budget is ignored here; depth is controlled by
//!   the owner-set effort (mapped to `model_reasoning_effort`).
//! - **Auth lives on the host.** Codex uses its own login (`codex login`, or a
//!   `CODEX_API_KEY` / `OPENAI_API_KEY` in the environment); we don't strip it.
//! - **Generated images never appear on the stream.** See [`ImageSweep`] — the
//!   only harness dialect we have to read off the filesystem instead.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use super::codex_app_server::{CodexAppServer, CodexAppServerOptions};
use super::{AgentEvent, CapsSource, CommandOutcome, Harness, HarnessProbe, ModelCap, Turn, TurnOutcome};
use mafold_transcript::RunStats;
use crate::client::Client;

pub struct Codex;

#[async_trait]
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn available(&self) -> bool {
        super::on_path("codex")
    }

    /// Yes: `thread.started` is the first thing `codex exec --json` says.
    fn names_session_first(&self) -> bool {
        true
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        // `thinking` and `ask_file` don't apply to Codex (no extended-thinking
        // budget; no AskUserQuestion tool / PreToolUse hook) — accepted and
        // ignored. `surface` likewise: the bash-hook that detaches background
        // tasks is only wired for Claude Code, so nothing here registers any.
        // `steer_file` too: with no hook to drain it mid-turn, `can_steer()`
        // stays false and the daemon delivers a mid-turn message as the
        // FOLLOW-UP turn instead — never dropped, just later, and the user is
        // told which of the two they got.
        let Turn {
            prompt,
            workdir,
            session,
            model,
            effort,
            thinking: _,
            cancel,
            system,
            ask_file: _,
            steer_file: _,
            // Codex keys its login by `CODEX_HOME`, which the daemon's own
            // environment already carries; per-turn seats are a Claude Code
            // thing today (`crate::accounts`).
            env: _,
            conv,
            surface,
            draft,
            // The drive mount is Claude Code's today (`--plugin-dir`); how
            // Codex takes an outside skills folder is measured before it is
            // wired (`.docs/bot-drive-v1.md` §5.5).
            mount: _,
            // No PreToolUse hook to hold it with (`crate::drive::skill_gate`).
            skill_plugins: _,
            proc,
        } = turn;
        if !Path::new(&workdir).is_dir() {
            bail!("working directory does not exist: {workdir} — check --workdir");
        }

        // Codex has no system-prompt flag; fold the mafold preamble into the
        // prompt so a mafold-unaware agent still knows it's acting as this bot.
        let full_prompt = match &system {
            Some(sys) if !sys.trim().is_empty() => format!("{sys}\n\n---\n\n{prompt}"),
            _ => prompt,
        };

        let program = super::program("codex");
        let p = RunParams {
            program: &program,
            full_prompt: &full_prompt,
            workdir: &workdir,
            model: model.as_deref(),
            effort: effort.as_deref(),
            conv: &conv,
            surface: &surface,
            draft: &draft,
            cancel: &cancel,
            sink: &sink,
            proc: &proc,
        };
        run_turn(&p, session.as_deref()).await
    }

    fn discover(&self, _workdir: &str) -> Value {
        // Codex custom prompts (`~/.codex/prompts/*.md`) are a TUI feature — a
        // forwarded `/name` wouldn't resolve in headless `codex exec` (it lands as
        // literal prompt text), so publishing them would only add dead menu
        // entries. The daemon's own control commands (/clear /new /model /status …)
        // are added separately and DO work.
        Value::Array(vec![])
    }

    async fn command(&self, _client: &Client, _chat_id: &str, _name: &str, _arg: &str, _workdir: &str, _session: Option<&str>, _env: &[(String, String)]) -> CommandOutcome {
        // No emulated slash commands yet — anything that isn't a daemon control
        // command is forwarded to `codex exec` as a prompt.
        CommandOutcome::Forward
    }

    async fn status_line(&self, _env: &[(String, String)]) -> String {
        auth_status_line().await
    }

    async fn cli_version(&self) -> String {
        codex_version().await
    }

    /// Ask THIS `codex` what it takes: App Server's `model/list` names every
    /// model the login can pick and, per model, the reasoning tiers it accepts
    /// (`supportedReasoningEfforts`) — the same list codex's own model picker
    /// draws, `ultra` included where a model has it. `account/read` says which
    /// login answered. Neither opens a thread, so neither costs a turn.
    ///
    /// `env` doesn't apply: codex keys its login by `CODEX_HOME`, which the
    /// daemon's own environment already carries (see [`Harness::run`]).
    async fn caps(&self, _env: &[(String, String)], _version: &str) -> Option<HarnessProbe> {
        let server = CodexAppServer::spawn(CodexAppServerOptions::default()).await.ok()?;
        let models = list_models(&server).await;
        let account = server
            .request("account/read", json!({}))
            .await
            .ok()
            .and_then(|r| r["account"]["email"].as_str().map(str::to_string))
            .filter(|s| !s.is_empty());
        let _ = server.shutdown().await;
        let models = models?;
        Some(HarnessProbe {
            account,
            source: CapsSource::Handshake,
            models,
            ..Default::default()
        })
    }

    /// Codex keys its login by `CODEX_HOME` — the daemon's own (a supervisor
    /// can pin one per bot), or `~/.codex` when unset — so that is the seat a
    /// roster belongs to. Two codex bots on two logins must not read each
    /// other's answer off the same file.
    fn seat_key(&self, env: &[(String, String)]) -> String {
        env.iter()
            .find(|(k, _)| k == "CODEX_HOME")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("CODEX_HOME").ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|home| format!("home-{home}"))
            .unwrap_or_else(|| "default".into())
    }
}

/// Most `model/list` pages to follow. The roster is a dozen models on one
/// page today; the cap only keeps a server that never stops paging from
/// holding the probe forever.
const MODEL_PAGES: usize = 20;

/// Every visible model, across pages. `None` unless the list ENDED: a page
/// that fails, a cursor that doesn't move, or more pages than any roster has
/// would all leave a list missing its tail — which drops real models from the
/// sheet and judges tiers against the wrong roster, worse than keeping what
/// it already has.
async fn list_models(server: &CodexAppServer) -> Option<Vec<ModelCap>> {
    let mut models: Vec<ModelCap> = Vec::new();
    let mut cursor = Value::Null;
    for _ in 0..MODEL_PAGES {
        let mut params = json!({ "includeHidden": false });
        if !cursor.is_null() {
            params["cursor"] = cursor.clone();
        }
        let page = server.request("model/list", params).await.ok()?;
        for m in models_of(&page) {
            if !models.iter().any(|x| x.id == m.id) {
                models.push(m);
            }
        }
        let next = match &page["nextCursor"] {
            Value::String(s) if s.is_empty() => Value::Null,
            v => v.clone(),
        };
        if next.is_null() {
            return Some(models);
        }
        if next == cursor {
            return None;
        }
        cursor = next;
    }
    None
}

/// One `model/list` page as [`ModelCap`]s, in codex's order. `model` is what
/// `--model` takes; `id` (the preset) is the same string today and an alias
/// when it isn't. Every field but the slug is optional-tolerant — the shape has
/// grown between builds, and a missing key means "this build didn't say".
fn models_of(page: &Value) -> Vec<ModelCap> {
    let s = |v: &Value, k: &str| v[k].as_str().map(str::to_string).filter(|x| !x.is_empty());
    page["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["hidden"].as_bool() != Some(true))
        .filter_map(|m| {
            let id = s(m, "model").or_else(|| s(m, "id"))?;
            let aliases = s(m, "id").filter(|alias| *alias != id).into_iter().collect();
            Some(ModelCap {
                display: s(m, "displayName").unwrap_or_else(|| id.clone()),
                resolved: None,
                aliases,
                description: s(m, "description"),
                efforts: m["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e["reasoningEffort"].as_str().or_else(|| e.as_str()))
                    .map(str::to_string)
                    .collect(),
                id,
            })
        })
        .collect()
}

/// Borrowed per-turn invocation parameters shared by the resume attempt and its
/// fresh-thread retry.
#[derive(Clone, Copy)]
struct RunParams<'a> {
    /// The `codex` binary to spawn — resolved once by [`Harness::run`]. A
    /// parameter rather than a lookup inside the run so the tests below can
    /// drive the whole event loop against a scripted stream.
    program: &'a std::ffi::OsStr,
    full_prompt: &'a str,
    workdir: &'a str,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    conv: &'a str,
    /// The turn's surface tag — where its forum channel comes from (`turn_env`).
    surface: &'a str,
    draft: &'a str,
    cancel: &'a std::sync::Arc<tokio::sync::Notify>,
    sink: &'a UnboundedSender<AgentEvent>,
    proc: &'a super::TurnProc,
}

/// A resume failure that means "this thread id is unusable" (expired rollout, or
/// a session written by another harness) — retry fresh, don't surface.
fn is_stale_thread(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("thread/resume") || s.contains("no rollout found")
}

/// The `codex exec` argv for one turn — everything EXCEPT the prompt, which is
/// fed on stdin. That is what the trailing `-` means: both `codex exec [PROMPT]`
/// and `codex exec resume <ID> [PROMPT]` document it as "read the instructions
/// from stdin".
///
/// **No argument here may ever contain a newline**, which is why the prompt isn't
/// one. It is always multi-line (the mafold preamble is joined to the message
/// with `\n\n---\n\n`) and it grows without bound (it carries the conversation) —
/// the two things Windows refuses to spawn:
/// - an npm-installed codex is `%APPDATA%\npm\codex.cmd`, a BATCH FILE, and since
///   the BatBadBut fix (CVE-2024-24576) Rust's std refuses to spawn one with any
///   argument containing `\r` or `\n`: `InvalidInput: batch file arguments are
///   invalid`. On argv, every Codex turn on a stock Windows install therefore
///   died before the process even started — and the error named nothing an owner
///   could act on. (Reported from the field; see the test below.)
/// - a command line is hard-capped at 32,767 UTF-16 units, so a long enough chat
///   makes `CreateProcessW` refuse the spawn outright (os error 206).
///
/// Both disappear when the prompt is stdin; Claude Code's harness feeds its own
/// prompt that way for the same reasons.
fn exec_args(session: Option<&str>, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    // Resume the conversation's Codex thread for context. The subcommand form
    // is `codex exec resume <THREAD_ID> [options] [prompt]`; options and the
    // prompt still parse after it.
    if let Some(sid) = session {
        args.push("resume".into());
        args.push(sid.into());
    }
    args.push("--json".into());
    // The daemon already gates WHO can drive the bot (allow-list), exactly as it
    // does for Claude Code's `--dangerously-skip-permissions`. So run Codex with
    // full autonomy — no approval prompts (which would hang a headless run), no
    // sandbox.
    args.push("--dangerously-bypass-approvals-and-sandbox".into());
    // Bot workdirs aren't necessarily git repos; Codex otherwise refuses.
    args.push("--skip-git-repo-check".into());
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.into());
    }
    // Reasoning effort (owner-set via Customization) → Codex's config key, AS
    // GIVEN. Which tiers exist is codex's to say (`model/list`, see `caps`) and
    // the daemon's to check before the turn (`harness::effort_for_turn`); a
    // table here clamped xhigh/max to high and dropped ultra on the floor, so
    // the run took `~/.codex/config.toml`'s tier instead. None = codex's own.
    if let Some(eff) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        args.push("-c".into());
        args.push(format!("model_reasoning_effort={}", toml_string(eff)));
    }
    // `--` stops flag parsing so the `-` after it is read as the prompt argument.
    args.push("--".into());
    args.push("-".into());
    args
}

/// One turn: a `codex exec`, and — when it ends without an answer — one more
/// on the same thread asking for it.
///
/// Codex ends a turn the moment the model makes no tool call, whatever it
/// said: a model that ran its commands and stopped without a word completes
/// cleanly (`turn.completed`, an empty or absent final message — Codex's own
/// `last_agent_message: null`), and the bubble used to be stamped ✓ with tool
/// cards and no answer in it. Claude Code 2.1.294 asks once more in that spot,
/// and so does the api's hosted harness (#909); this is the same rule for the
/// codex driver: [`NO_VISIBLE_OUTPUT`], verbatim, on the thread that just
/// went quiet, streaming into the same reply. Still nothing after that, and
/// the turn fails as the shared `empty_reply` — the `{% mafold/error %}` card
/// says so, with no ✓ result card over nothing.
///
/// Not asked when the turn showed nothing at all: that is the daemon's
/// empty-turn path (`agent.rs`), which re-carries the whole message. Nor when
/// the turn has no reply to answer in (`draft` empty — `mafold agent
/// --inbox`, where what the model writes is never seen and a turn that only
/// ran `mafold send` is exactly how it should end).
///
/// [`NO_VISIBLE_OUTPUT`]: mafold_transcript::failure::NO_VISIBLE_OUTPUT
async fn run_turn(p: &RunParams<'_>, session: Option<&str>) -> Result<TurnOutcome> {
    let started_ms = mafold_transcript::stats::now_ms();
    let may_ask = !p.draft.is_empty();
    let mut items = super::codex_stats::ItemStats::default();
    let first = match run_once(p, session, Leg::first(started_ms, may_ask), &mut items).await {
        // The stored thread id can be stale or foreign — Codex expires
        // rollouts, and a conversation may carry a session written by a
        // DIFFERENT harness (a bot switched to codex mid-conversation).
        // Retry once WITHOUT resume: the failed attempt exits before
        // emitting any event, so the fresh run streams into a clean turn.
        Err(e) if session.is_some() && is_stale_thread(&e) => {
            run_once(p, None, Leg::first(started_ms, may_ask), &mut items).await
        }
        r => r,
    }?;
    let (Some(usage), Some(thread)) = (first.held, first.outcome.session.clone()) else {
        return Ok(first.outcome);
    };
    println!("↻ codex: the turn ended without an answer — asking once more on thread {thread}");
    let ask = RunParams { full_prompt: mafold_transcript::failure::NO_VISIBLE_OUTPUT, ..*p };
    let mut outcome = match run_once(&ask, Some(&thread), Leg::follow_up(started_ms, usage), &mut items).await {
        Ok(more) => {
            let mut o = more.outcome;
            if o.stopped || o.error.is_some() {
                // Ended like any stopped or failed turn: no result card.
            } else if !more.answered {
                o.error = Some(mafold_transcript::failure::empty_reply("codex"));
            } else if let Some(stats) = more.held {
                // The turn's one `Done`, after everything both runs said.
                close(p.sink, stats, &items, Some(&thread), started_ms).await;
            }
            o
        }
        Err(e) => TurnOutcome { error: Some(format!("{e:#}")), ..Default::default() },
    };
    outcome.produced |= first.outcome.produced;
    outcome.session = outcome.session.or(Some(thread));
    Ok(outcome)
}

/// Which `codex exec` of a turn this is ([`run_turn`]).
struct Leg {
    /// When the turn started: the rollout's measurements from then on are
    /// this turn's, across both runs.
    started_ms: u64,
    /// A first run whose silence may be asked about (the turn has a reply).
    may_ask: bool,
    /// The first run's usage, when this one IS the ask — what it reports is
    /// the two together, one turn. The ask never closes the turn itself:
    /// [`run_turn`] does, once, knowing how it ended.
    before: Option<RunStats>,
}

impl Leg {
    fn first(started_ms: u64, may_ask: bool) -> Self {
        Self { started_ms, may_ask, before: None }
    }

    fn follow_up(started_ms: u64, before: RunStats) -> Self {
        Self { started_ms, may_ask: false, before: Some(before) }
    }
}

/// How one `codex exec` ended.
struct LegEnd {
    outcome: TurnOutcome,
    /// Something was said (or drawn) since its last tool call.
    answered: bool,
    /// Its usage, when it completed without closing the turn: a first run
    /// that went quiet (held for the ask), or the ask itself.
    held: Option<RunStats>,
}

/// Whether `item` says something about the answer: a non-empty
/// `agent_message` is one (`Some(true)`); a tool call means anything said
/// before it was not the end (`Some(false)`); everything else says nothing.
fn answers(phase: &str, item: &Value) -> Option<bool> {
    if super::codex_stats::is_tool(item) {
        return Some(false);
    }
    (phase == "item.completed"
        && item["type"] == "agent_message"
        && item["text"].as_str().is_some_and(|t| !t.trim().is_empty()))
    .then_some(true)
}

/// Two runs' usage as one turn's: the token counts add up, and a count either
/// run didn't report makes the total unknown rather than half of it.
fn add_usage(before: &RunStats, now: &RunStats) -> RunStats {
    let sum = |a: Option<u64>, b: Option<u64>| a.zip(b).map(|(a, b)| a.saturating_add(b));
    RunStats {
        input_tokens: sum(before.input_tokens, now.input_tokens),
        output_tokens: sum(before.output_tokens, now.output_tokens),
        cache_read_tokens: sum(before.cache_read_tokens, now.cache_read_tokens),
        cache_write_tokens: sum(before.cache_write_tokens, now.cache_write_tokens),
        total_tokens: sum(before.total_tokens, now.total_tokens),
        ..Default::default()
    }
}

/// One `codex exec` invocation (optionally resuming `session`), streaming
/// normalized events into the sink.
async fn run_once(
    p: &RunParams<'_>,
    session: Option<&str>,
    leg: Leg,
    item_stats: &mut super::codex_stats::ItemStats,
) -> Result<LegEnd> {
    let RunParams { program, full_prompt, workdir, model, effort, conv, surface, draft, cancel, sink, proc } = *p;
    let stats_started_ms = leg.started_ms;
    let _ = sink.send(AgentEvent::Stats(RunStats {
        model: model.map(str::to_string),
        effort: effort.map(str::trim).filter(|e| !e.is_empty()).map(str::to_string),
        ..Default::default()
    }));

    let mut cmd = crate::platform::command(program);
        cmd.args(exec_args(session, model, effort));
        // Export the current conversation and forum channel, so `mafold room`,
        // `send` and `read` default to THIS room and channel (`turn_env`).
        for (k, v) in super::turn_env(conv, surface) {
            cmd.env(k, v);
        }
        // The reply being streamed right now — `mafold attach <file>` hangs
        // media on it. Codex's own generated images are swept up automatically
        // (see ImageSweep); this is the door for everything else it draws.
        cmd.env("MAFOLD_DRAFT", draft);
        cmd.kill_on_drop(true);

        let mut child = cmd
            .current_dir(workdir)
            // PIPED, never null or inherited: the prompt goes in HERE — `exec_args`
            // ends with the `-` that tells codex to read it from stdin, and nothing
            // else on the command line ever wants stdin.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| super::spawn_err("codex", workdir, e))?;
        // Register this run in the live-children set so a daemon shutdown kills
        // exactly THIS process (see harness::live_children) — same contract as
        // the Claude harness; RAII deregisters on every exit path.
        let _child_guard = crate::harness::ChildGuard::new(child.id());
        // …and as the process running this turn, for the heartbeat
        // (`TurnProc`): a silent tool call in here is work, not a dead turn.
        let _serving = proc.serve(child.id());

        // Feed the prompt in its OWN task (same shape as the Claude harness): a
        // prompt past the pipe buffer (~64KB — and this one holds the conversation)
        // would otherwise block us here while codex is blocked writing stdout that
        // nobody is reading yet. Dropping the handle closes stdin, which is the EOF
        // the `-` waits for.
        if let Some(mut si) = child.stdin.take() {
            let prompt = full_prompt.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = si.write_all(prompt.as_bytes()).await;
                let _ = si.shutdown().await;
            });
        }

        let stdout = child.stdout.take().context("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();

        // Drain stderr CONCURRENTLY (same deadlock guard as the Claude harness): a
        // blocked stderr pipe would stall the turn while it holds the conv lock.
        let stderr_task = child.stderr.take().map(|se| {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let mut se = se;
                let _ = se.read_to_string(&mut buf).await;
                buf
            })
        });

        let mut produced = false;
        // Something was said since the last tool call (`answers`), or drawn.
        let mut answered = false;
        let mut held: Option<RunStats> = None;
        let mut stopped = false;
        let mut session_id: Option<String> = None;
        let mut error: Option<String> = None;
        let mut images: Option<ImageSweep> = None;
        // Codex has no stall watchdog, so before this a codex killed while a
        // process it started held its stdout left the turn open until someone
        // typed /stop. Its exit is the one sign that is always there.
        let mut exit = super::ExitWatch::new(child.id());

        // Emit whatever `image_gen` has written since the last check. Called
        // after every completed item (so a picture reaches the bubble while the
        // turn is still running, not in a lump at the end) and once more before
        // `Done`, which the render loop treats as terminal.
        macro_rules! sweep_images {
            () => {
                if let Some(sw) = images.as_mut() {
                    for path in sw.take_new() {
                        let _ = sink.send(AgentEvent::Image { path });
                        produced = true;
                        // A picture in the reply is something to look at.
                        answered = true;
                    }
                }
            };
        }

        loop {
            let line = tokio::select! {
                line = lines.next_line() => match line? { Some(l) => l, None => break },
                _ = cancel.notified() => { stopped = true; let _ = child.start_kill(); break; }
                _ = exit.gone() => { error = Some(super::EXITED_MID_TURN.to_string()); break; }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if let Some(phase) = v["type"].as_str().filter(|p| p.starts_with("item.")) {
                item_stats.observe(phase, &v["item"]);
            }
            match v["type"].as_str().unwrap_or("") {
                // The thread id — our resumable session for this conversation.
                "thread.started" => {
                    if let Some(t) = v["thread_id"].as_str() {
                        if session_id.is_none() {
                            session_id = Some(t.to_string());
                        }
                        // Baseline BEFORE any item can run, so this turn's
                        // sweeps see only this turn's images.
                        images = Some(ImageSweep::baseline(t));
                        let _ = sink.send(AgentEvent::Session(t.to_string()));
                    }
                }
                // One `codex exec` run = one turn; its completion ends the stream.
                "turn.completed" => {
                    // A turn that COMPLETED is a success, whatever it had to
                    // survive on the way — drop any retry notice taken below.
                    error = None;
                    sweep_images!(); // must precede Done — the renderer stops there
                    let usage = match &leg.before {
                        Some(before) => add_usage(before, &RunStats::codex(&v["usage"])),
                        None => RunStats::codex(&v["usage"]),
                    };
                    // Not closed here when the turn may go on — no answer and
                    // it can still be asked for — or when this run IS the ask:
                    // `Done` stamps the result card, which goes last, once
                    // (`run_turn`).
                    let ask = leg.may_ask && produced && !answered && session_id.is_some();
                    if ask || leg.before.is_some() {
                        held = Some(usage);
                    } else {
                        close(sink, usage, item_stats, session_id.as_deref(), stats_started_ms).await;
                    }
                    break;
                }
                // Model gave up mid-turn (stream ended, etc.) — surface + stop.
                "turn.failed" => {
                    error = Some(err_text(&v["error"]).unwrap_or_else(|| "the turn failed".into()));
                    let _ = child.start_kill();
                    break;
                }
                // NOT fatal on its own, however much it reads like it. Codex
                // streams its RETRY NOTICES through `error` — "Reconnecting... 2/5
                // (stream disconnected before completion: …)" comes straight out
                // of its `core/src/responses_retry.rs`, as does "Falling back from
                // WebSockets to HTTPS transport." — and then it carries on: up to
                // five notices, after which the turn either completes normally or
                // ends with a bare `error` + a `turn.failed`.
                //
                // Killing the child on the first one (what this did) shot codex
                // MID-RECONNECT: every transient blip — one proxy hiccup is enough
                // — became "⚠️ Agent stopped: Reconnecting... 2/5" on a turn that
                // would have finished by itself. Worse, `agent.rs` reads a turn
                // error on a RESUMED session as "this thread is corrupt" and drops
                // the session, so the next message lost the conversation too.
                //
                // So: remember it and let the STREAM decide. `turn.completed`
                // clears it, `turn.failed` overwrites it with the real reason, and
                // if the stream just ends this is the last word we had.
                "error" => {
                    error = Some(
                        v["message"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string()),
                    );
                }
                phase @ ("item.started" | "item.updated" | "item.completed") => {
                    if let Some(a) = answers(phase, &v["item"]) {
                        answered = a;
                    }
                    handle_item(phase, &v["item"], sink, &mut produced);
                    if phase == "item.completed" {
                        sweep_images!();
                    }
                }
                _ => {}
            }
        }
        // (The owned sender lives in `run()` — the channel closes when it returns,
        // and the renderer flushes its tail then.)

        if stopped || error.is_some() {
            let _ = child.start_kill(); // idempotent
            let _ = child.wait().await; // reap
            if let Some(t) = stderr_task {
                t.abort();
            }
            return Ok(LegEnd {
                outcome: TurnOutcome { produced, stopped, session: session_id, error, limit: None },
                answered,
                held: None,
            });
        }
        let status = child.wait().await?;
        if !status.success() {
            // It completed, then exited badly: no ask follows an `Err`, so a
            // run that did answer closes the turn it held here — the way a
            // run that never held anything already has.
            if let Some(usage) = held.take().filter(|_| answered) {
                close(sink, usage, item_stats, session_id.as_deref(), stats_started_ms).await;
            }
            let err = match stderr_task {
                Some(t) => t.await.unwrap_or_default(),
                None => String::new(),
            };
            let err = err.trim();
            bail!(
                "codex exited unsuccessfully{}",
                if err.is_empty() {
                    String::new()
                } else {
                    format!(": {err}")
                }
            );
        }
        if let Some(t) = stderr_task {
            t.abort();
        }
        Ok(LegEnd {
            outcome: TurnOutcome { produced, stopped, session: session_id, error: None, limit: None },
            answered,
            held,
        })
}

/// Close the turn's transcript: its numbers (`usage`, the tool tally, and
/// what the thread's rollout adds since `started_ms`), then `Done` — which
/// stamps the result card, so nothing may follow it.
async fn close(
    sink: &UnboundedSender<AgentEvent>,
    usage: RunStats,
    items: &super::codex_stats::ItemStats,
    thread: Option<&str>,
    started_ms: u64,
) {
    let mut stats = usage;
    stats.merge(&items.snapshot());
    if let Some(thread) = thread {
        let home = codex_home();
        let thread = thread.to_string();
        let metadata = tokio::task::spawn_blocking(move || super::codex_stats::metadata(&home, &thread, started_ms))
            .await
            .unwrap_or_default();
        stats.merge(&metadata);
    }
    let tokens = stats.total_tokens;
    let _ = sink.send(AgentEvent::Stats(stats));
    let _ = sink.send(AgentEvent::Done { duration_ms: None, cost_usd: None, tokens });
}

/// Where Codex keeps its state (`CODEX_HOME`, else `~/.codex`) — the same
/// resolution `codex` itself does.
fn codex_home() -> PathBuf {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    PathBuf::from(home).join(".codex")
}

/// Watches for images Codex's `image_gen` tool produced this turn.
///
/// It has to be a directory watch, because the picture is **not on the event
/// stream at all**: `codex exec --json` speaks a fixed item vocabulary
/// (`agent_message` / `reasoning` / `command_execution` / `file_change` /
/// `mcp_tool_call` / `web_search` / `todo_list`) with no image member. The
/// generated bytes go straight into the model's own context and to
/// `$CODEX_HOME/generated_images/<thread-id>/<call-id>.png` — so the model
/// sees the image, says "已生成", and everything downstream of it sees nothing.
/// That is the whole bug this exists to close.
///
/// BASELINED at `thread.started`, before the turn can write anything: a resumed
/// thread's directory already holds every image from previous turns, and
/// without a baseline the first resumed turn would re-send all of them.
struct ImageSweep {
    dir: PathBuf,
    seen: HashSet<OsString>,
}

impl ImageSweep {
    /// Start watching `thread_id`'s image directory, treating whatever is
    /// already there as old news.
    fn baseline(thread_id: &str) -> Self {
        let dir = codex_home().join("generated_images").join(thread_id);
        let mut s = Self { dir, seen: HashSet::new() };
        s.take_new(); // prime `seen`; earlier turns' output is not ours to send
        s
    }

    /// Images that appeared since the last call, oldest first (so a turn that
    /// draws several sends them in the order they were made). Missing dir — the
    /// overwhelmingly common case, since most turns draw nothing — is not an
    /// error, just an empty sweep.
    fn take_new(&mut self) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut fresh: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name();
            if self.seen.contains(&name) {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            self.seen.insert(name);
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            fresh.push((mtime, e.path()));
        }
        fresh.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        fresh.into_iter().map(|(_, p)| p).collect()
    }
}

/// `s` as a TOML basic string — `-c key=value` parses its value as TOML.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Normalize a Codex `item` event into `AgentEvent`s. `phase` is the outer
/// `item.started` / `item.updated` / `item.completed`. `item.id` correlates a
/// command's start (the tool card) with its completion (the output).
fn handle_item(phase: &str, item: &Value, sink: &UnboundedSender<AgentEvent>, produced: &mut bool) {
    let id = item["id"].as_str().unwrap_or("").to_string();
    let itype = item["type"].as_str().unwrap_or("");
    let completed = phase == "item.completed";

    match itype {
        // The assistant's reply text (whole block, at completion).
        "agent_message" if completed => {
            if let Some(t) = item["text"].as_str() {
                if !t.is_empty() {
                    let _ = sink.send(AgentEvent::Text(t.to_string()));
                    *produced = true;
                }
            }
        }
        // A reasoning / chain-of-thought block (collapsed in the UI).
        "reasoning" if completed => {
            if let Some(t) = item["text"].as_str() {
                if !t.trim().is_empty() {
                    let _ = sink.send(AgentEvent::Thinking(t.to_string()));
                    *produced = true;
                }
            }
        }
        // A shell command: the card at start, its output at completion. Named
        // "bash" so the result renders as a `{% bash %}` card, like Claude's Bash.
        "command_execution" => {
            if phase == "item.started" {
                let _ = sink.send(AgentEvent::ToolCall {
                    id,
                    name: "bash".into(),
                    input: json!({ "command": command_str(&item["command"]) }),
                });
                *produced = true;
            } else if completed {
                let mut text = item["aggregated_output"].as_str().unwrap_or("").to_string();
                if let Some(code) = item["exit_code"].as_i64() {
                    if code != 0 {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(&format!("(exit {code})"));
                    }
                }
                let _ = sink.send(AgentEvent::ToolResult { id, text });
            }
        }
        // File edits (Codex's apply_patch). We only know the paths + kind, not the
        // hunks, so render one clean per-file tool card (counted as an edit) rather
        // than a fake 0/0 diff. The actual patch, when applied via a shell
        // command, still shows through the command_execution card above.
        "file_change" if completed => {
            if let Some(changes) = item["changes"].as_array() {
                for (i, ch) in changes.iter().enumerate() {
                    let path = ch["path"].as_str().unwrap_or("");
                    let kind = ch["kind"].as_str().unwrap_or("update");
                    let _ = sink.send(AgentEvent::ToolCall {
                        id: format!("{id}-{i}"),
                        name: "apply_patch".into(),
                        input: json!({ "file_path": format!("{path} ({kind})") }),
                    });
                    *produced = true;
                }
            }
        }
        // An MCP tool call: the invocation card + its result text.
        "mcp_tool_call" => {
            if phase == "item.started" {
                let name = format!(
                    "{}.{}",
                    item["server"].as_str().unwrap_or("mcp"),
                    item["tool"].as_str().unwrap_or("tool"),
                );
                let _ = sink.send(AgentEvent::ToolCall {
                    id,
                    name,
                    input: item["arguments"].clone(),
                });
                *produced = true;
            } else if completed {
                let text = mcp_result_text(item);
                if !text.is_empty() {
                    let _ = sink.send(AgentEvent::ToolResult { id, text });
                }
            }
        }
        // A web search → the `{% web query=… %}` card (name mirrors Claude's).
        "web_search" if completed => {
            let _ = sink.send(AgentEvent::ToolCall {
                id,
                name: "websearch".into(),
                input: json!({ "query": item["query"].as_str().unwrap_or("") }),
            });
            *produced = true;
        }
        // The plan → a `{% todo %}` card (name mirrors Claude's TodoWrite). Emitted
        // once, at completion, to avoid a card per intermediate update.
        "todo_list" if completed => {
            let todos: Vec<Value> = item["items"].as_array().map(|arr| {
                arr.iter().map(|t| json!({
                    "content": t["text"].as_str().unwrap_or(""),
                    "status": if t["completed"].as_bool().unwrap_or(false) { "completed" } else { "pending" },
                })).collect()
            }).unwrap_or_default();
            let _ = sink.send(AgentEvent::ToolCall {
                id,
                name: "todowrite".into(),
                input: json!({ "todos": todos }),
            });
            *produced = true;
        }
        // A non-fatal item warning (e.g. truncated output) — surface, don't hide.
        "error" if completed => {
            if let Some(m) = item["message"].as_str() {
                if !m.trim().is_empty() {
                    let _ = sink.send(AgentEvent::Text(format!("\n> ⚠️ {}\n", m.trim())));
                    *produced = true;
                }
            }
        }
        _ => {}
    }
}

/// Codex's `command` is usually a string (`"bash -lc ls"`) but can be an argv
/// array; normalize both to a single displayable string.
fn command_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// Join an mcp_tool_call result's text content blocks; fall back to its error.
fn mcp_result_text(item: &Value) -> String {
    if let Some(blocks) = item["result"]["content"].as_array() {
        let text = blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return text;
        }
    }
    if let Some(e) = item["error"]["message"]
        .as_str()
        .or_else(|| item["error"].as_str())
    {
        if !e.trim().is_empty() {
            return format!("error: {e}");
        }
    }
    String::new()
}

/// An error object's message — `{message: "..."}` or a bare string.
fn err_text(e: &Value) -> Option<String> {
    e["message"]
        .as_str()
        .or_else(|| e.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// One-line auth status for the `/status` Account row: how the host is
/// authenticated (ChatGPT login vs API key), read from `$CODEX_HOME/auth.json`
/// (default `~/.codex`). The CLI version is reported separately on the Harness
/// row via `cli_version()`, so it isn't repeated here (parallel to Claude Code,
/// whose Account row is auth-only). Empty if Codex isn't installed.
async fn auth_status_line() -> String {
    // Gate on Codex being installed at all — otherwise this row is just noise.
    if codex_version().await.is_empty() {
        return String::new();
    }
    auth_mode().unwrap_or_else(|| "not logged in".into())
}

/// `codex --version` → "0.5.0" (first numeric-ish token; "" if the CLI is missing).
async fn codex_version() -> String {
    use std::time::Duration;
    let mut cmd = crate::platform::command(super::program("codex"));
    cmd.arg("--version").stdin(Stdio::null());
    match tokio::time::timeout(Duration::from_secs(8), cmd.output()).await {
        Ok(Ok(o)) => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Read `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`) and report the
/// auth mode without exposing any secret: a ChatGPT login (`tokens`) vs an API
/// key (`OPENAI_API_KEY`). None = no auth file.
fn auth_mode() -> Option<String> {
    let home = std::env::var("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".codex")
        });
    let text = std::fs::read_to_string(home.join("auth.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    if v.get("tokens").is_some() {
        Some("ChatGPT login".into())
    } else if v
        .get("OPENAI_API_KEY")
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.is_empty())
    {
        Some("API key".into())
    } else {
        Some("configured".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `codex` stand-in that prints `stream` on stdout, one JSON event per
    /// line, and exits 0 — enough to drive the whole event loop.
    fn scripted_codex(dir: &std::path::Path, stream: &[&str]) -> std::path::PathBuf {
        #[cfg(windows)]
        {
            let path = dir.join("codex.cmd");
            let mut s = String::from("@echo off\r\n");
            for line in stream {
                s.push_str(&format!("echo {line}\r\n"));
            }
            std::fs::write(&path, s).unwrap();
            path
        }
        #[cfg(not(windows))]
        {
            scripted_legs(dir, &[stream])
        }
    }

    /// A `codex` stand-in that prints `legs[n]` on its n-th run (nothing past
    /// the last) and keeps what run n was given: `argv.n`, and the prompt it
    /// read on stdin as `stdin.n`. A line `exit N` exits there with N.
    #[cfg(unix)]
    fn scripted_legs(dir: &std::path::Path, legs: &[&[&str]]) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("codex.sh");
        let d = dir.display();
        let mut s = format!(
            "#!/bin/sh\nn=$(cat '{d}/runs' 2>/dev/null || echo 0)\necho $((n + 1)) > '{d}/runs'\n\
             printf '%s\\n' \"$*\" > \"{d}/argv.$n\"\ncat > \"{d}/stdin.$n\"\ncase $n in\n"
        );
        for (i, stream) in legs.iter().enumerate() {
            s.push_str(&format!("{i})\n"));
            for line in *stream {
                if line.starts_with("exit ") {
                    s.push_str(&format!("{line}\n"));
                } else {
                    s.push_str(&format!("cat <<'MAFOLD_JSON'\n{line}\nMAFOLD_JSON\n"));
                }
            }
            s.push_str(";;\n");
        }
        s.push_str("esac\n");
        std::fs::write(&path, s).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Drive one turn against a scripted stream.
    async fn run_scripted(tag: &str, stream: &[&str]) -> TurnOutcome {
        run_scripted_events(tag, stream).await.0
    }

    /// The agent process is told its conversation AND its forum channel, through
    /// the real spawn path — `mafold send` / `read` default to the channel.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_agent_process_is_told_its_conversation_and_channel() {
        let dir = std::env::temp_dir().join(format!("mafold-codex-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (program, record) = crate::harness::env_probe::script(&dir);
        let workdir = dir.to_string_lossy().to_string();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        let (sink, _rx) = tokio::sync::mpsc::unbounded_channel();
        for (surface, want) in [("c1__ch-0001__opsdu_codex", "c1|ch-0001"), ("c1____opsdu_codex", "c1|")] {
            let _ = std::fs::remove_file(&record);
            let p = RunParams {
                program: program.as_os_str(),
                full_prompt: "hi",
                workdir: &workdir,
                model: None,
                effort: None,
                conv: "c1",
                surface,
                draft: "draft",
                cancel: &cancel,
                sink: &sink,
                proc: &crate::harness::TurnProc::default(),
            };
            let _ = run_turn(&p, None).await; // the probe says nothing; only what it was given counts
            assert_eq!(std::fs::read_to_string(&record).unwrap(), want, "{surface}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn run_scripted_events(tag: &str, stream: &[&str]) -> (TurnOutcome, Vec<AgentEvent>) {
        let dir = std::env::temp_dir().join(format!("mafold-codex-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = scripted_codex(&dir, stream);
        let (out, events) = drive(&dir, &program, "draft").await;
        let _ = std::fs::remove_dir_all(&dir);
        (out.unwrap(), events)
    }

    /// One turn through the real entry ([`run_turn`]) against `program`, as
    /// a reply to `draft` (empty: a turn with no reply, like the inbox's).
    async fn drive(dir: &std::path::Path, program: &std::path::Path, draft: &str) -> (Result<TurnOutcome>, Vec<AgentEvent>) {
        let workdir = dir.to_string_lossy().to_string();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        let (sink, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = run_turn(
            &RunParams {
                program: program.as_os_str(),
                full_prompt: "hi",
                workdir: &workdir,
                model: None,
                effort: None,
                conv: "conv",
                surface: "",
                draft,
                cancel: &cancel,
                sink: &sink,
                proc: &crate::harness::TurnProc::default(),
            },
            None,
        )
        .await;
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() { events.push(ev); }
        (out, events)
    }

    /// The legs of one scripted turn: what each run was given, and what came
    /// out of the whole turn.
    #[cfg(unix)]
    struct Legs {
        result: Result<TurnOutcome>,
        events: Vec<AgentEvent>,
        /// `(argv, stdin)` of each run, in order.
        runs: Vec<(String, String)>,
    }

    #[cfg(unix)]
    impl Legs {
        fn out(&self) -> &TurnOutcome {
            self.result.as_ref().expect("an outcome, not an Err")
        }

        fn dones(&self) -> usize {
            self.events.iter().filter(|e| matches!(e, AgentEvent::Done { .. })).count()
        }
    }

    #[cfg(unix)]
    async fn run_legs(tag: &str, legs: &[&[&str]]) -> Legs {
        run_legs_as(tag, legs, "draft").await
    }

    #[cfg(unix)]
    async fn run_legs_as(tag: &str, legs: &[&[&str]], draft: &str) -> Legs {
        let dir = std::env::temp_dir().join(format!("mafold-codex-legs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = scripted_legs(&dir, legs);
        let (result, events) = drive(&dir, &program, draft).await;
        let runs = (0..)
            .map_while(|n| {
                let argv = std::fs::read_to_string(dir.join(format!("argv.{n}"))).ok()?;
                Some((argv, std::fs::read_to_string(dir.join(format!("stdin.{n}"))).unwrap_or_default()))
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        Legs { result, events, runs }
    }

    /// The reply the renderer would build from `events`.
    #[cfg(unix)]
    fn transcript_of(events: &[AgentEvent]) -> String {
        let mut tx = mafold_transcript::Transcript::new();
        for event in events {
            tx.push(event);
        }
        tx.finish()
    }

    #[cfg(unix)]
    const THREAD: &str = r#"{"type":"thread.started","thread_id":"01a1f0aa-0000-7000-8000-000000000001"}"#;
    #[cfg(unix)]
    const RENAME_STARTED: &str = r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"mafold channels rename 民调","status":"in_progress"}}"#;
    #[cfg(unix)]
    const RENAME_DONE: &str = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"mafold channels rename 民调","aggregated_output":"renamed","exit_code":0,"status":"completed"}}"#;

    /// conv df712566's shape on the codex driver: commands ran, then the model
    /// stopped without a word (an empty final message, what a local rollout of
    /// 2026-10-05 holds). The turn used to be stamped ✓ over tool cards alone;
    /// now the same thread is asked once — Claude Code's line, verbatim — and
    /// the answer lands in the SAME reply, before the one result card, with the
    /// two runs' tokens added up.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_that_ends_after_its_tools_without_a_word_is_asked_once_more() {
        let r = run_legs(
            "asked",
            &[
                &[
                    THREAD,
                    r#"{"type":"item.completed","item":{"id":"m0","type":"agent_message","text":"I'll look it up."}}"#,
                    RENAME_STARTED,
                    RENAME_DONE,
                    r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":""}}"#,
                    r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":20}}"#,
                ],
                &[
                    THREAD,
                    r#"{"type":"item.completed","item":{"id":"m2","type":"agent_message","text":"The latest poll has 42%."}}"#,
                    r#"{"type":"turn.completed","usage":{"input_tokens":130,"cached_input_tokens":0,"output_tokens":10}}"#,
                ],
            ],
        )
        .await;
        assert_eq!(r.runs.len(), 2, "asked exactly once more");
        let (argv, stdin) = &r.runs[1];
        assert!(argv.contains("resume 01a1f0aa-0000-7000-8000-000000000001"), "on the same thread: {argv}");
        assert_eq!(stdin, mafold_transcript::failure::NO_VISIBLE_OUTPUT, "Claude Code's line, nothing else");
        assert_eq!(r.out().error, None);
        assert!(r.out().produced);
        assert_eq!(r.out().session.as_deref(), Some("01a1f0aa-0000-7000-8000-000000000001"));
        assert_eq!(r.dones(), 1, "one turn, one result card");
        let md = transcript_of(&r.events);
        let answer = md.find("The latest poll has 42%.").expect("the answer is in the reply");
        let result = md.find("mafold/result").expect("the result card");
        assert!(answer < result, "the result card goes last:\n{md}");
        assert!(md.contains("\"total_tokens\":260"), "both runs' tokens, one turn:\n{md}");
        assert!(md.contains("\"tool_calls\":1"), "{md}");
    }

    /// Still nothing after the ask: the turn fails as the shared `empty_reply`
    /// — the `{% mafold/error %}` card's "empty", with no ✓ result card over
    /// nothing — and is not asked again. Same for an ask that never even
    /// completes.
    #[cfg(unix)]
    #[tokio::test]
    async fn still_silent_after_the_ask_fails_as_an_empty_reply() {
        let quiet: &[&str] = &[THREAD, RENAME_STARTED, RENAME_DONE, r#"{"type":"turn.completed","usage":{"output_tokens":3}}"#];
        let never_asked: &[&str] = &[THREAD, r#"{"type":"item.completed","item":{"id":"m","type":"agent_message","text":"never asked"}}"#];
        for (tag, ask) in [
            ("silent", &[THREAD, r#"{"type":"turn.completed","usage":{"output_tokens":2}}"#][..]),
            ("eof", &[THREAD][..]),
        ] {
            let r = run_legs(tag, &[quiet, ask, never_asked]).await;
            assert_eq!(r.runs.len(), 2, "{tag}: one ask, never a second");
            let err = r.out().error.clone().expect("a turn with no answer is not a success");
            assert!(
                matches!(mafold_transcript::failure::classify(&err).kind, mafold_transcript::failure::FailureKind::Empty),
                "{tag}: {err}"
            );
            assert!(r.out().produced, "{tag}: its tool cards are on screen — not the daemon's empty-turn path");
            assert_eq!(r.dones(), 0, "{tag}: no result card on a turn that failed");
        }
    }

    /// An ask that answers and then exits badly still closes the turn — once,
    /// after the answer — and the exit is the turn's error, as it would be
    /// for any run.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_ask_that_answers_then_exits_badly_closes_the_turn_once() {
        let r = run_legs(
            "exit",
            &[
                &[THREAD, RENAME_STARTED, RENAME_DONE, r#"{"type":"turn.completed","usage":{"output_tokens":3}}"#],
                &[
                    THREAD,
                    r#"{"type":"item.completed","item":{"id":"m","type":"agent_message","text":"Here it is."}}"#,
                    r#"{"type":"turn.completed","usage":{"output_tokens":2}}"#,
                    "exit 1",
                ],
            ],
        )
        .await;
        assert_eq!(r.runs.len(), 2);
        let err = r.out().error.clone().expect("the bad exit is reported");
        assert!(err.contains("exited unsuccessfully"), "{err}");
        assert_eq!(r.dones(), 1, "closed once, not once per run");
        let md = transcript_of(&r.events);
        assert!(md.find("Here it is.").unwrap() < md.find("mafold/result").unwrap(), "{md}");
    }

    /// A turn with no reply to answer in — the inbox's (`draft` empty), where
    /// what the model writes is never seen and a turn that only ran `mafold
    /// send` is done — is never asked about its silence.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_with_no_reply_is_not_asked() {
        let sending = r#"{"type":"item.started","item":{"id":"s","type":"command_execution","command":"mafold send c 好的","status":"in_progress"}}"#;
        let sent = r#"{"type":"item.completed","item":{"id":"s","type":"command_execution","command":"mafold send c 好的","aggregated_output":"sent","exit_code":0,"status":"completed"}}"#;
        let r = run_legs_as(
            "inbox",
            &[
                &[THREAD, sending, sent, r#"{"type":"turn.completed","usage":{"output_tokens":3}}"#],
                &[THREAD, r#"{"type":"item.completed","item":{"id":"m","type":"agent_message","text":"never asked"}}"#],
            ],
            "",
        )
        .await;
        assert_eq!(r.runs.len(), 1, "the inbox's silence is its answer");
        assert_eq!(r.out().error, None);
        assert_eq!(r.dones(), 1);
    }

    /// What is NOT asked again: a turn that answered after its last command; a
    /// plain answer; an answer followed only by the plan codex completes at the
    /// end of the turn; a turn that showed nothing at all (the daemon's own
    /// empty-turn retry re-carries the message); and a stopped or failed one.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_answered_or_unfinished_turn_is_not_asked_again() {
        let answer = r#"{"type":"item.completed","item":{"id":"m","type":"agent_message","text":"Done: renamed it."}}"#;
        let plan = r#"{"type":"item.completed","item":{"id":"p","type":"todo_list","items":[{"text":"rename","completed":true}]}}"#;
        let completed = r#"{"type":"turn.completed","usage":{"output_tokens":5}}"#;
        let failed = r#"{"type":"turn.failed","error":{"message":"stream disconnected before completion"}}"#;
        let cases: [(&str, Vec<&str>); 5] = [
            ("after-tools", vec![THREAD, RENAME_STARTED, RENAME_DONE, answer, completed]),
            ("plain", vec![THREAD, answer, completed]),
            ("plan-last", vec![THREAD, RENAME_STARTED, RENAME_DONE, answer, plan, completed]),
            ("nothing", vec![THREAD, completed]),
            ("failed", vec![THREAD, RENAME_STARTED, RENAME_DONE, failed]),
        ];
        for (tag, stream) in cases {
            let r = run_legs(tag, &[&stream, &[THREAD, answer, completed]]).await;
            assert_eq!(r.runs.len(), 1, "{tag}: asked again");
            assert_eq!(r.dones(), usize::from(tag != "failed"), "{tag}");
        }
    }

    /// Which items speak to the answer.
    #[test]
    fn what_counts_as_an_answer() {
        let msg = |t: &str| json!({ "type": "agent_message", "text": t });
        assert_eq!(answers("item.completed", &msg("hi")), Some(true));
        assert_eq!(answers("item.completed", &msg("  \n")), None, "whitespace says nothing");
        assert_eq!(answers("item.completed", &msg("")), None, "codex's empty final message");
        assert_eq!(answers("item.started", &json!({ "type": "command_execution" })), Some(false));
        assert_eq!(answers("item.completed", &json!({ "type": "web_search" })), Some(false));
        assert_eq!(answers("item.completed", &json!({ "type": "reasoning", "text": "hmm" })), None);
        assert_eq!(answers("item.completed", &json!({ "type": "todo_list" })), None, "the plan closes at turn end");
    }

    /// Codex killed mid-turn while a process it started still holds its stdout:
    /// no EOF ever comes, and before this the turn simply never ended — codex
    /// has no stall watchdog, so it sat there until someone typed /stop. The
    /// process being gone has to end it, with what was in the pipe read first.
    #[tokio::test]
    async fn harness_exit_ends_the_turn_even_if_a_child_holds_the_pipe() {
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("mafold-codex-orphan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = crate::harness::orphan_fixture::script(&dir, r#"{"type":"thread.started","thread_id":"t-orphan"}"#);
        let workdir = dir.to_string_lossy().to_string();
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        let (sink, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let proc = crate::harness::TurnProc::default();
        let p = RunParams {
            program: program.as_os_str(),
            full_prompt: "hi",
            workdir: &workdir,
            model: None,
            effort: None,
            conv: "conv",
            surface: "",
            draft: "draft",
            cancel: &cancel,
            sink: &sink,
            proc: &proc,
        };
        let (out, after) =
            crate::harness::orphan_fixture::kill_mid_turn(run_turn(&p, None), &proc, &dir, Duration::from_secs(20)).await;
        let _ = std::fs::remove_dir_all(&dir);

        let out = out
            .expect("the turn was still open 20s after its process died — a child of it holds the pipe")
            .expect("an outcome, not an Err");
        assert!(
            after < crate::harness::EXIT_DRAIN + Duration::from_secs(2),
            "ended {after:?} after the kill — one {:?} drain, not more",
            crate::harness::EXIT_DRAIN
        );
        assert!(out.error.is_some(), "a turn whose agent died must say so");
        let mut saw_session = false;
        while let Ok(ev) = rx.try_recv() {
            saw_session |= matches!(ev, AgentEvent::Session(ref s) if s == "t-orphan");
        }
        assert!(saw_session, "what the process wrote before it died was still read");
    }

    #[tokio::test]
    async fn codex_stdout_preserves_usage_into_the_result_body() {
        let (out, events) = run_scripted_events("result-stats", &[
            r#"{"type":"thread.started","thread_id":"fixture"}"#,
            r#"{"type":"item.completed","item":{"id":"reply","type":"agent_message","text":"OK"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":20}}"#,
        ]).await;
        assert!(out.produced);
        let mut tx = mafold_transcript::Transcript::new();
        for event in events { tx.push(&event); }
        let md = tx.finish();
        assert!(md.contains("\"total_tokens\":120"), "{md}");
        assert!(md.contains("\"cache_read_tokens\":80"), "{md}");
        assert!(md.contains("\"cost_usd\":null"), "{md}");
        assert!(md.contains("\"context_used_tokens\":null"), "{md}");
    }

    /// Codex streams RETRY NOTICES through `error` events ("Reconnecting... 2/5
    /// …", from its `core/src/responses_retry.rs`) and then carries on. This
    /// harness used to kill the child on the first one, so a single proxy hiccup
    /// became "⚠️ Agent stopped: Reconnecting... 2/5" on a turn codex was still
    /// finishing — and `agent.rs` dropped the resumed session on top of it.
    /// A turn that reconnects and completes is a SUCCESS.
    #[tokio::test]
    async fn a_reconnect_notice_does_not_end_the_turn() {
        let out = run_scripted(
            "reconnect",
            &[
                r#"{"type":"thread.started","thread_id":"01a06fa7-61a1-7871-adaf-7410e6e063e3"}"#,
                r#"{"type":"turn.started"}"#,
                r#"{"type":"error","message":"Reconnecting... 1/5 stream disconnected before completion"}"#,
                r#"{"type":"error","message":"Reconnecting... 2/5 stream disconnected before completion"}"#,
                r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"OK"}}"#,
                r#"{"type":"turn.completed","usage":{"output_tokens":5}}"#,
            ],
        )
        .await;
        assert_eq!(out.error, None, "a completed turn must not carry a retry notice");
        assert!(out.produced, "the message it recovered to send must survive");
        assert_eq!(out.session.as_deref(), Some("01a06fa7-61a1-7871-adaf-7410e6e063e3"));
    }

    /// …and when the retries really are exhausted, the LAST word wins: codex's
    /// own reason from `turn.failed`, not the "Reconnecting... 5/5" banner that
    /// happens to precede it.
    #[tokio::test]
    async fn an_exhausted_retry_fails_with_codex_s_reason() {
        let out = run_scripted(
            "exhausted",
            &[
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                r#"{"type":"error","message":"Reconnecting... 5/5 stream disconnected before completion"}"#,
                r#"{"type":"error","message":"stream disconnected before completion: Transport error"}"#,
                r#"{"type":"turn.failed","error":{"message":"stream disconnected before completion: Transport error"}}"#,
            ],
        )
        .await;
        let err = out.error.expect("an exhausted retry must still fail the turn");
        assert!(err.contains("stream disconnected"), "{err}");
        assert!(!err.contains("Reconnecting"), "the banner is not the reason: {err}");
    }

    /// A stream that dies mid-turn without saying why still has to say SOMETHING:
    /// the last notice is the only word we had.
    #[tokio::test]
    async fn a_stream_that_just_stops_surfaces_its_last_notice() {
        let out = run_scripted(
            "eof",
            &[
                r#"{"type":"thread.started","thread_id":"t-2"}"#,
                r#"{"type":"error","message":"Reconnecting... 3/5 stream disconnected before completion"}"#,
            ],
        )
        .await;
        assert_eq!(
            out.error.as_deref(),
            Some("Reconnecting... 3/5 stream disconnected before completion")
        );
    }

    /// The invariant that keeps this harness alive on Windows: the prompt rides
    /// stdin (argv ends with `--` `-`), and nothing on the command line carries a
    /// newline — see [`exec_args`]. Resume path included: it takes the same
    /// trailing prompt argument.
    #[test]
    fn the_prompt_never_rides_argv() {
        for session in [None, Some("01a06b89-3f5e-7e21-b3dc-f0a9c61ffb73")] {
            let args = exec_args(session, Some("gpt-5-codex"), Some("xhigh"));
            assert!(
                !args.iter().any(|a| a.contains('\n') || a.contains('\r')),
                "a newline in argv is unspawnable against a .cmd: {args:?}"
            );
            assert_eq!(args.last().unwrap(), "-", "prompt must come from stdin: {args:?}");
            assert_eq!(args[args.len() - 2], "--", "{args:?}");
        }
    }

    /// The field regression, hermetically: an npm-installed codex is a BATCH FILE
    /// (`%APPDATA%\npm\codex.cmd`), and Rust's std refuses to spawn one with an
    /// argument containing a newline. With the prompt on argv this exact spawn
    /// failed with `batch file arguments are invalid` — no process, no turn, on
    /// every stock Windows install. It must stay spawnable.
    #[cfg(windows)]
    #[test]
    fn a_batch_file_codex_is_still_spawnable() {
        let dir = std::env::temp_dir().join(format!("mafold-batspawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bat = dir.join("codex.cmd");
        // Echoes its own argv back, so this also covers the quoting cmd.exe does
        // to `-c model_reasoning_effort="high"` on the way through.
        std::fs::write(&bat, "@echo off\r\necho %*\r\n").unwrap();

        let out = crate::platform::std_command(&bat)
            .args(exec_args(None, None, Some("high")))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut ch| {
                use std::io::Write;
                // The shape of a real prompt: preamble + separator + message.
                ch.stdin.take().unwrap().write_all(b"You are a bot.\n\n---\n\nhi")?;
                ch.wait_with_output()
            })
            .expect("a .cmd codex must still spawn");
        let echoed = String::from_utf8_lossy(&out.stdout);
        assert!(echoed.contains("--json"), "argv mangled: {echoed}");
        assert!(echoed.contains("model_reasoning_effort"), "argv mangled: {echoed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tier rides to codex exactly as the turn carries it — the field bug
    /// was `ultra` vanishing from argv (and xhigh/max arriving as `high`), so
    /// codex ran `~/.codex/config.toml`'s medium. Which tiers are real is
    /// checked before the turn (`harness::effort_for_turn`), not here.
    #[test]
    fn effort_reaches_codex_verbatim() {
        let effort_arg = |e: Option<&str>| {
            let args = exec_args(None, Some("gpt-6.1-sol"), e);
            args.iter()
                .position(|a| a == "-c")
                .map(|i| args[i + 1].clone())
        };
        for tier in ["low", "medium", "high", "xhigh", "max", "ultra"] {
            assert_eq!(effort_arg(Some(tier)), Some(format!("model_reasoning_effort=\"{tier}\"")));
        }
        assert_eq!(effort_arg(None), None, "unset = codex's own default");
        assert_eq!(effort_arg(Some("  ")), None);
        // Still one TOML string whatever it holds.
        assert_eq!(effort_arg(Some("a\"b")), Some(r#"model_reasoning_effort="a\"b""#.to_string()));
    }

    /// `model/list` as codex 0.161.0 answers it (trimmed): per-model tiers,
    /// `ultra` only where the model has it, hidden models left out, and the
    /// paging cursor ignored by the parse.
    #[test]
    fn model_list_becomes_per_model_tiers() {
        let tiers = |ts: &[&str]| -> Value {
            ts.iter().map(|t| json!({ "reasoningEffort": t, "description": "…" })).collect()
        };
        let page = json!({
            "data": [
                { "id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT-6.1-Sol",
                  "description": "Latest workhorse model.", "hidden": false, "isDefault": true,
                  "defaultReasoningEffort": "low",
                  "supportedReasoningEfforts": tiers(&["low", "medium", "high", "xhigh", "max", "ultra"]) },
                { "id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna", "hidden": false,
                  "supportedReasoningEfforts": tiers(&["low", "medium", "high", "xhigh", "max"]) },
                { "id": "gpt-daybreak-blue-latest", "model": "gpt-daybreak-blue-latest", "hidden": true,
                  "supportedReasoningEfforts": tiers(&["low"]) },
                { "id": "preset-x", "model": "gpt-x", "supportedReasoningEfforts": [] }
            ],
            "nextCursor": null
        });
        let models = models_of(&page);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6.1-sol", "gpt-6-luna", "gpt-x"], "hidden models stay out of the sheet");
        assert_eq!(models[0].display, "GPT-6.1-Sol");
        assert_eq!(models[0].efforts, ["low", "medium", "high", "xhigh", "max", "ultra"]);
        assert_eq!(models[1].efforts, ["low", "medium", "high", "xhigh", "max"]);
        // The slug is what `--model` takes; a different preset id still names it.
        assert_eq!(models[2].display, "gpt-x");
        assert!(models[2].matches("preset-x"));
        assert!(models[2].efforts.is_empty());
        assert!(models_of(&json!({})).is_empty());
    }

    /// A roster belongs to the codex login it was read from — two daemons on
    /// two `CODEX_HOME`s keep two answers, never one shared `default`.
    #[test]
    fn codex_rosters_are_kept_per_codex_home() {
        let pro = Codex.seat_key(&[("CODEX_HOME".into(), "/u/.codex-pro".into())]);
        let plus = Codex.seat_key(&[("CODEX_HOME".into(), "/u/.codex-plus".into())]);
        assert_ne!(pro, plus);
        assert_ne!(pro, "default");
        assert_ne!(
            super::super::caps_cache_path("codex", &pro),
            super::super::caps_cache_path("codex", &plus)
        );
    }

    /// The real binary, when there is one: the probe must come back with the
    /// tiers per model, and spend nothing doing it (no thread is opened).
    #[tokio::test]
    #[ignore = "requires an installed Codex CLI with App Server support"]
    async fn live_codex_reports_its_tiers() {
        if !super::super::on_path("codex") {
            return;
        }
        let probe = Codex.caps(&[], "").await.expect("codex answered model/list");
        assert!(!probe.models.is_empty());
        assert!(probe.models.iter().all(|m| !m.efforts.is_empty()), "{:?}", probe.models);
        for m in &probe.models {
            println!("  {} ({}) efforts={:?}", m.id, m.display, m.efforts);
        }
    }

    /// A sweep must report only what THIS turn drew. A resumed thread's
    /// directory already holds every image from every earlier turn, so without
    /// the baseline the first resumed turn re-sends the whole history.
    #[test]
    fn a_sweep_reports_only_images_written_after_the_baseline() {
        let dir = std::env::temp_dir().join(format!("mafold-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.png"), b"x").unwrap();

        // Baseline over the pre-existing file (bypasses codex_home for the test).
        let mut sw = ImageSweep { dir: dir.clone(), seen: HashSet::new() };
        sw.take_new();
        assert!(sw.take_new().is_empty(), "a quiet turn sweeps up nothing");

        std::fs::write(dir.join("new.png"), b"y").unwrap();
        let got = sw.take_new();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].file_name().unwrap(), "new.png");
        // Already reported — a later sweep in the same turn must not repeat it.
        assert!(sw.take_new().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Most turns draw nothing, so the directory usually doesn't exist. That is
    /// the normal path, not an error.
    #[test]
    fn sweeping_a_directory_that_was_never_created_is_empty() {
        let mut sw = ImageSweep {
            dir: std::env::temp_dir().join("mafold-sweep-does-not-exist"),
            seen: HashSet::new(),
        };
        assert!(sw.take_new().is_empty());
    }

    #[test]
    fn command_str_string_or_array() {
        assert_eq!(command_str(&json!("bash -lc ls")), "bash -lc ls");
        assert_eq!(command_str(&json!(["bash", "-lc", "ls"])), "bash -lc ls");
        assert_eq!(command_str(&json!(null)), "");
    }

    #[test]
    fn mcp_text_prefers_content_then_error() {
        let ok = json!({ "result": { "content": [{"type":"text","text":"a"},{"type":"text","text":"b"}] } });
        assert_eq!(mcp_result_text(&ok), "a\nb");
        let err = json!({ "result": { "content": [] }, "error": { "message": "boom" } });
        assert_eq!(mcp_result_text(&err), "error: boom");
        assert_eq!(mcp_result_text(&json!({})), "");
    }

    // Collect every AgentEvent a handler run emits (drain a sync channel).
    fn drain(f: impl FnOnce(&UnboundedSender<AgentEvent>, &mut bool)) -> (Vec<AgentEvent>, bool) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut produced = false;
        f(&tx, &mut produced);
        drop(tx);
        let mut out = vec![];
        // The unbounded receiver is async; pull synchronously via try_recv.
        let mut rx = rx;
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        (out, produced)
    }

    #[test]
    fn agent_message_becomes_text() {
        let item = json!({ "id": "i1", "type": "agent_message", "text": "hi there" });
        let (evs, produced) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        assert!(produced);
        assert!(matches!(&evs[..], [AgentEvent::Text(t)] if t == "hi there"));
    }

    #[test]
    fn command_execution_emits_call_then_result() {
        let started = json!({ "id": "c1", "type": "command_execution", "command": "bash -lc ls", "status": "in_progress" });
        let (evs, _) = drain(|tx, p| handle_item("item.started", &started, tx, p));
        match &evs[..] {
            [AgentEvent::ToolCall { id, name, input }] => {
                assert_eq!(id, "c1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "bash -lc ls");
            }
            _ => panic!("expected one ToolCall, got {evs:?}"),
        }
        let done = json!({ "id": "c1", "type": "command_execution", "aggregated_output": "docs\nsrc\n", "exit_code": 0, "status": "completed" });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &done, tx, p));
        assert!(
            matches!(&evs[..], [AgentEvent::ToolResult { id, text }] if id == "c1" && text == "docs\nsrc\n")
        );

        let failed = json!({ "id": "c2", "type": "command_execution", "aggregated_output": "nope", "exit_code": 2, "status": "failed" });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &failed, tx, p));
        assert!(
            matches!(&evs[..], [AgentEvent::ToolResult { text, .. }] if text == "nope\n(exit 2)")
        );
    }

    #[test]
    fn file_change_becomes_apply_patch_calls() {
        let item = json!({
            "id": "f1", "type": "file_change", "status": "completed",
            "changes": [ {"path":"a.rs","kind":"update"}, {"path":"b.rs","kind":"add"} ],
        });
        let (evs, produced) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        assert!(produced);
        match &evs[..] {
            [AgentEvent::ToolCall {
                id: i0,
                name: n0,
                input: in0,
            }, AgentEvent::ToolCall {
                id: i1,
                name: n1,
                input: in1,
            }] => {
                assert_eq!((i0.as_str(), n0.as_str()), ("f1-0", "apply_patch"));
                assert_eq!(in0["file_path"], "a.rs (update)");
                assert_eq!((i1.as_str(), n1.as_str()), ("f1-1", "apply_patch"));
                assert_eq!(in1["file_path"], "b.rs (add)");
            }
            _ => panic!("expected two apply_patch ToolCalls, got {evs:?}"),
        }
    }

    #[test]
    fn todo_list_maps_to_todowrite_shape() {
        let item = json!({
            "id": "t1", "type": "todo_list", "status": "completed",
            "items": [ {"text":"do a","completed":true}, {"text":"do b","completed":false} ],
        });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        match &evs[..] {
            [AgentEvent::ToolCall { name, input, .. }] => {
                assert_eq!(name, "todowrite");
                let todos = input["todos"].as_array().unwrap();
                assert_eq!(todos[0]["content"], "do a");
                assert_eq!(todos[0]["status"], "completed");
                assert_eq!(todos[1]["status"], "pending");
            }
            _ => panic!("expected one todowrite ToolCall, got {evs:?}"),
        }
    }

    #[test]
    fn reasoning_becomes_thinking() {
        let item = json!({ "id": "r1", "type": "reasoning", "text": "thinking hard" });
        let (evs, _) = drain(|tx, p| handle_item("item.completed", &item, tx, p));
        assert!(matches!(&evs[..], [AgentEvent::Thinking(t)] if t == "thinking hard"));
    }
}
