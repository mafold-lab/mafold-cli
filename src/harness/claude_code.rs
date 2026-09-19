//! Claude Code harness — drives the local `claude` CLI headlessly
//! (`claude -p --output-format stream-json`) and normalizes its stream-json into
//! [`AgentEvent`]s. Skill/command discovery and the emulated slash commands live
//! in `crate::discover` / `crate::commands` (both Claude-Code-specific).

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use super::cc_conn;
use super::{
    AgentEvent, CapsSource, CommandOutcome, Harness, HarnessProbe, ModeCap, ModelCap, SeatHealth,
    SeatLimit, Turn, TurnOutcome,
};
use mafold_transcript::RunStats;
use crate::client::Client;

pub struct ClaudeCode;

#[async_trait]
impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn available(&self) -> bool {
        super::on_path("claude")
    }

    /// Yes — via the PostToolUse hook registered in [`Self::run`].
    fn can_steer(&self) -> bool {
        true
    }

    /// Open the connection this turn will want, while the caller is still
    /// assembling its prompt. Everything it needs is in the SHAPE; the per-turn
    /// half reaches the child through `turnenv` at `begin_turn`.
    ///
    /// Silent about failure on purpose — a prewarm that doesn't land just means
    /// the turn starts cold, which is what happened before this existed.
    fn prewarm(&self, shape: super::TurnShape) {
        if !cc_conn::enabled() {
            return;
        }
        let key = cc_conn::PoolKey::new(
            &shape.conv, &shape.surface, &shape.workdir, shape.model.as_deref(),
            shape.effort.as_deref(), shape.thinking, shape.system.as_deref(), &shape.env,
        );
        // Nothing to gain when one is already warm (or already on its way), and
        // a second process for the same key would just sit out its TTL.
        let Some(sid) = shape.session.clone() else { return };
        if !cc_conn::claim_prewarm(&key) {
            return;
        }
        tokio::spawn(async move {
            let _release = cc_conn::PrewarmGuard(key.clone());
            let began = std::time::Instant::now();
            let exe = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_else(|| "mafold".into());
            let Built { mut cmd, hook_settings, bash_only } = build_cmd(&shape, &exe);
            let id = cc_conn::oneshot_id();
            cmd.env("MAFOLD_TURN", crate::turnenv::path_for(&id));
            let fallback = hook_settings.as_ref().map(|s| {
                let mut f = cc_conn::clone_cmd(&cmd);
                f.arg("--settings").arg(s);
                f
            });
            cmd.arg("--settings").arg(&bash_only);
            let Ok(mut c) = cc_conn::Conn::spawn(key, id.clone(), String::new(), cmd, &shape.workdir).await
            else { return };
            if !c.register_hooks(true, true).await {
                let Some(f) = fallback else { return };
                c.kill();
                c.wait_exit(std::time::Duration::from_secs(5)).await;
                let Ok(c2) = cc_conn::Conn::spawn(c.key.clone(), id, String::new(), f, &shape.workdir).await
                else { return };
                c = c2;
            }
            // A connection only answers for the session it actually holds, and
            // a prewarmed one has not spoken yet — so claim the session the turn
            // will ask to resume. It was passed `--resume` with exactly that id.
            c.adopt_session(&sid);
            eprintln!(
                "[cc-pool] prewarmed pid {} in {}ms",
                c.pid().unwrap_or(0),
                began.elapsed().as_millis()
            );
            cc_conn::put(c);
        });
    }

    async fn run(&self, turn: Turn, sink: UnboundedSender<AgentEvent>) -> Result<TurnOutcome> {
        let Turn { prompt, workdir, session, model, effort, thinking, cancel, system, ask_file, steer_file, conv, surface, draft, env } = turn;
        let _ = sink.send(AgentEvent::Stats(RunStats {
            effort: effort.clone(), ..Default::default()
        }));
        if !Path::new(&workdir).is_dir() {
            bail!("working directory does not exist: {workdir} — check --workdir");
        }
        // Everything that is fixed at spawn time goes into the key: two turns
        // share a process only when every one of these agrees (a changed system
        // prompt cannot be re-set on a live connection, and a different SEAT is
        // a different Claude login, so each gets its own).
        let key = cc_conn::PoolKey::new(
            &conv,
            &surface,
            &workdir,
            model.as_deref(),
            effort.as_deref(),
            thinking,
            system.as_deref(),
            &env,
        );
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_else(|| "mafold".into());
        let shape = super::TurnShape {
            conv: conv.clone(),
            surface: surface.clone(),
            workdir: workdir.clone(),
            session: session.clone(),
            model: model.clone(),
            effort: effort.clone(),
            thinking,
            system: system.clone(),
            env: env.clone(),
        };
        let Built { mut cmd, hook_settings, bash_only } = build_cmd(&shape, &exe);
        // The reply being streamed right now — `mafold attach <file>` hangs
        // media on it. Kept in the env for an OLDER `mafold` on the agent's
        // $PATH; ours reads the turn file, which is current on every turn.
        cmd.env("MAFOLD_DRAFT", &draft);
        // Watches the permission mailbox for as long as this turn runs (see
        // `permission_watcher`). Bound, never read: it is held for its Drop, so
        // that all of `run`'s exit paths stop the watcher without any of them
        // having to remember to.
        let _perm_watch: Option<PermWatch> = ask_file
            .as_ref()
            .map(|af| permission_watcher(format!("{af}.perm"), sink.clone()));
        // Don't let the console child flash a window (the agent runs detached).
        crate::platform::no_window(&mut cmd);
        cmd.env_remove("CLAUDECODE").env_remove("ANTHROPIC_API_KEY");

        // A WARM connection for this exact configuration, or a new process.
        // `take` removes it from the pool, so a second concurrent turn in the
        // same conversation finds nothing and opens its own — which is what
        // happens today (turns already run concurrently and fork the session).
        let mut conn = match cc_conn::take_or_wait(&key, session.as_deref()).await {
            Some(c) => c,
            None => {
                let id = cc_conn::oneshot_id();
                // The child reads its PER-TURN values (draft / ask / steer) back
                // out of this file, because its env cannot be rewritten and a
                // connection outlives the turn that spawned it. See `turnenv`.
                cmd.env("MAFOLD_TURN", crate::turnenv::path_for(&id));
                // Cloned BEFORE any `--settings`, so the fallback carries ONE
                // (the full set) rather than two conflicting ones.
                let fallback = hook_settings.as_ref().map(|s| {
                    let mut f = cc_conn::clone_cmd(&cmd);
                    f.arg("--settings").arg(s);
                    f
                });
                // The Bash hook goes in either way; ask and steer are registered
                // over the control channel just below.
                cmd.arg("--settings").arg(&bash_only);
                let mut c =
                    cc_conn::Conn::spawn(key.clone(), id.clone(), draft.clone(), cmd, &workdir).await?;
                if !c.register_hooks(ask_file.is_some(), steer_file.is_some()).await {
                    if let Some(f) = fallback {
                        // Too old for the control channel: start over with the
                        // hooks claude spawns as processes. One extra spawn
                        // (~1.3s), once per connection, on old CLIs only.
                        eprintln!(
                            "[cc-pool] this claude didn't answer the hook handshake — \
                             falling back to command hooks"
                        );
                        c.kill();
                        c.wait_exit(std::time::Duration::from_secs(5)).await;
                        c = cc_conn::Conn::spawn(key.clone(), id, draft.clone(), f, &workdir).await?;
                    }
                }
                c
            }
        };
        let reused = conn.turns > 0;
        // Current values for THIS turn, before the prompt goes in. Written to
        // the file (a hook claude spawns as a process reads it) AND handed to
        // the connection (its in-process hook callbacks read that copy).
        let tenv = crate::turnenv::TurnEnv {
            draft: draft.clone(),
            ask: ask_file.clone().unwrap_or_default(),
            steer: steer_file.clone().unwrap_or_default(),
            perm: ask_file.as_ref().map(|af| format!("{af}.perm")).unwrap_or_default(),
            surface: surface.clone(),
        };
        crate::turnenv::write(&crate::turnenv::path_for(&conn.id), &tenv);
        // An OLDER `mafold` on the agent's $PATH still reads `MAFOLD_DRAFT` from
        // its env — which names the draft this connection was SPAWNED for. It
        // already knows to follow the forwarding address a steer leaves; point
        // that at this turn's draft so a picture lands on the right reply even
        // from a binary that predates `MAFOLD_TURN`.
        if reused && conn.spawn_draft != draft {
            let _ = std::fs::write(crate::agent::draft_ptr_path(&conn.spawn_draft), &draft);
        }
        conn.begin_turn(&prompt, tenv).await?;

        let mut produced = false;
        let mut stopped = false;
        // Did the CURRENT assistant message arrive as text deltas? Reset on every
        // `message_start`. Reply text has three possible carriers (partial deltas,
        // the completed `assistant` message, the final `result`) and claude does
        // not guarantee the first one — when the partial stream is missing, the
        // completed message is the only copy we get. Streaming stays the primary
        // path; this flag is what keeps the fallbacks below from double-posting it.
        let mut streamed_text = false;
        // The text of the assistant message CURRENTLY streaming, cleared the
        // moment that message lands as a completed `assistant` event. A
        // `message_start` arriving while this is non-empty means the message it
        // belongs to never landed: the API connection dropped mid-response and
        // claude is retrying it by re-streaming from the first token, so the
        // partial attempt has to be un-said before the retry re-says it.
        let mut msg_text = String::new();
        let mut session_id: Option<String> = None;
        // Set when an API / execution error ends the turn — surfaced to the user.
        let mut error: Option<String> = None;
        // Set when the seat REFUSED a request (a `rejected` rate-limit event):
        // if the turn then ends on an error, that is why, and the caller can
        // move it to another login (see `TurnOutcome::limit`).
        let mut limit: Option<super::LimitHit> = None;
        // The last few NON-JSON stdout lines. `claude` prints its fatal reasons
        // as plain text on stdout — a usage cap, an auth failure, a `--resume`
        // id whose transcript is gone — NOT as stream-json, and the parser below
        // drops every line it can't parse. When the run then exits nonzero with
        // an empty stderr, this tail is the ONLY explanation that exists; without
        // it the daemon reported a bare "claude exited unsuccessfully" and the
        // reason was destroyed at the exact moment it was needed.
        // Collected by the connection's reader (see `cc_conn`) and read back below.
        // Real output-token progress for the generating heartbeat: the API's
        // `message_delta` usage is cumulative PER assistant message, so completed
        // messages accumulate into `tokens_done` when the next one starts.
        let mut tokens_done: u64 = 0;
        let mut tokens_cur: u64 = 0;
        // Receipts for OTHER queued messages stepped over so far (see the
        // `result` arm). Bounded so a pathological queue can't hold a turn open
        // forever — past the bound we take the next receipt as ours and end the
        // turn, which is the old behaviour, not a new way to hang.
        const MAX_SKIPPED_RECEIPTS: u32 = 8;
        let mut skipped_receipts: u32 = 0;

        // Stall watchdog: a healthy turn always keeps stdout moving (text deltas,
        // tool events, thinking) — even a long tool call is bracketed by its
        // tool_use/tool_result events within the tool's own timeout. A child that
        // goes fully silent longer than this is hung (the "typing forever, never
        // sends" failure): kill it and surface the reason through the error path,
        // which keeps the session so a resend resumes with context. Generous on
        // purpose — the longest legitimate silence is a slow tool run.
        const STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);
        loop {
            // Frames come from the CONNECTION's reader, which keeps draining
            // stdout between turns too — an unread pipe fills at ~64KB and
            // wedges the process. Plain (non-JSON) stdout is kept there as well;
            // we collect it at the end, for the same "why did it die" reason.
            let v = tokio::select! {
                frame = conn.recv() => match frame { Some(v) => v, None => break },
                _ = cancel.notified() => { stopped = true; conn.kill(); break; }
                _ = tokio::time::sleep(STALL_AFTER) => {
                    error = Some(format!(
                        "no output from the agent for {} minutes — the run looks stalled and was stopped. Your context is kept; just resend to retry.",
                        STALL_AFTER.as_secs() / 60
                    ));
                    conn.kill();
                    break;
                }
            };
            if session_id.is_none() {
                if let Some(sid) = v["session_id"].as_str() { session_id = Some(sid.to_string()); }
            }
            // Claude Code compacting its OWN context, mid-turn. It runs for
            // minutes (135s / 162s / 308s in this machine's transcripts) and
            // streams nothing at all while it works, so an unrelayed compaction
            // reads as a hung reply — exactly the moment a user gives up and
            // resends. Compaction is in place: the session id doesn't change,
            // so `--resume` is unaffected and nothing needs re-persisting.
            //
            // Deliberately does NOT set `produced`: this is our narration, not
            // model output. A turn that only compacted and then said nothing is
            // still an empty turn, and must stay eligible for the caller's
            // empty-turn retry.
            if v["type"] == "system" && v["subtype"] == "init" {
                let _ = sink.send(AgentEvent::Stats(RunStats {
                    model: v["model"].as_str().map(str::to_string),
                    ..Default::default()
                }));
            }
            if v["type"] == "system" && v["subtype"] == "compact_boundary" {
                let _ = sink.send(AgentEvent::Compacted { pre_tokens: compaction_pre_tokens(&v) });
                continue;
            }
            // Another local claude session tried to say something to this one
            // and the receive-side policy PARKED it. Silent until now: the
            // sender saw nothing, the user saw nothing, and the message either
            // arrives much later or never. It changes what the user would do
            // (go look at the other session), so it belongs in the reply.
            if v["type"] == "system" && v["subtype"] == "peer_message_hold" {
                if let Some(t) = peer_hold_notice(&v) {
                    let _ = sink.send(AgentEvent::Notice(t));
                }
                continue;
            }
            // Usage-limit state. Relayed ONLY when it is not "allowed": claude
            // emits one of these on ordinary healthy turns too, and echoing
            // "your quota is fine" into every reply is noise, not news.
            if v["type"] == "rate_limit_event" {
                if let Some((kind, resets_at, status)) = rate_limit_notice(&v["rate_limit_info"]) {
                    // A refusal is the seat itself saying no. A threshold
                    // warning (`allowed_warning`) is not — the request went
                    // through — so it must never move the turn to another
                    // login, or a 76%-used account would hand every turn away
                    // for the rest of the week.
                    if status == "rejected" {
                        limit = Some(super::LimitHit { kind: kind.clone(), resets_at });
                    }
                    let _ = sink.send(AgentEvent::RateLimited { kind, resets_at, status });
                }
                continue;
            }
            // A fatal error event (NOT a transient API blip the SDK retries away
            // silently) — stop now and report the reason, instead of relaying an
            // endless error/retry stream while the turn never finalizes.
            if v["type"] == "error" {
                error = Some(
                    v["error"].as_str()
                        .or_else(|| v["message"].as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string()),
                );
                conn.kill();
                break;
            }
            // Streaming assistant text — plus silent progress pulses for the
            // deltas that are NOT rendered (thinking / tool-arg json): they keep
            // the generating card's heartbeat honest while the model works
            // without visible output.
            if v["type"] == "stream_event" {
                let ev_type = &v["event"]["type"];
                if ev_type == "content_block_delta" {
                    let d = &v["event"]["delta"];
                    if d["type"] == "text_delta" {
                        if let Some(t) = d["text"].as_str() {
                            let _ = sink.send(AgentEvent::Text(t.to_string()));
                            msg_text.push_str(t);
                            produced = true;
                            streamed_text = true;
                        }
                    } else if let Some(t) = d["thinking"].as_str().or_else(|| d["partial_json"].as_str()) {
                        let _ = sink.send(AgentEvent::Pulse { chars: t.len() as u64, tokens: None });
                    }
                } else if ev_type == "message_start" {
                    // Text still pending from the LAST message_start means that
                    // message never completed — this is a retry of it, not the
                    // next message, and it will re-stream from the first token.
                    // Take back the abandoned attempt (the transcript removes it
                    // only if it is still the exact tail, so a producer that
                    // resumed instead of restarting is left alone).
                    if !msg_text.is_empty() {
                        let _ = sink.send(AgentEvent::TextRewind(std::mem::take(&mut msg_text)));
                    }
                    // A new assistant message: bank the previous one's usage.
                    tokens_done += std::mem::take(&mut tokens_cur);
                    streamed_text = false;
                } else if ev_type == "message_delta" {
                    // Cumulative REAL output tokens for the current message.
                    if let Some(t) = v["event"]["usage"]["output_tokens"].as_u64() {
                        tokens_cur = t;
                        let _ = sink.send(AgentEvent::Pulse { chars: 0, tokens: Some(tokens_done + tokens_cur) });
                    }
                }
            }
            // Completed assistant message → tool calls + thinking, plus the text
            // itself when it never streamed (see `streamed_text`).
            if v["type"] == "assistant" {
                // A SUBAGENT's message. It arrives on this same stream, marked
                // only by `parent_tool_use_id` (the partial-delta stream never
                // carries one — verified 118/118 — so only whole messages can
                // be a subagent's). Flattened into the main timeline it reads
                // as work the main agent did, and its final text would land in
                // the reply as if the main agent had said it. Attribute it to
                // the call that started it instead.
                if let Some(parent) = v["parent_tool_use_id"].as_str() {
                    if let Some(blocks) = v["message"]["content"].as_array() {
                        for b in blocks {
                            let line = match b["type"].as_str() {
                                Some("tool_use") => Some(mafold_transcript::step_line(
                                    b["name"].as_str().unwrap_or("tool"),
                                    &b["input"],
                                )),
                                // What it reported back, trimmed to a line: the
                                // full text arrives again as the tool RESULT of
                                // the call that started it.
                                Some("text") => b["text"]
                                    .as_str()
                                    .map(str::trim)
                                    .filter(|t| !t.is_empty())
                                    .map(|t| format!("· {}", first_line(t))),
                                // Its thinking is not the user's business — the
                                // main agent's already collapses into a trace.
                                _ => None,
                            };
                            if let Some(l) = line {
                                let _ = sink.send(AgentEvent::SubagentStep {
                                    parent: parent.to_string(),
                                    text: l,
                                });
                                produced = true;
                            }
                        }
                    }
                    continue;
                }
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        match b["type"].as_str() {
                            // Normally a no-op: the deltas already streamed this
                            // text and re-sending it would double the reply. But a
                            // turn whose partial stream never arrived carries its
                            // ONLY copy here — without this the entire reply is
                            // dropped and the user gets "(the agent produced no
                            // output)" while the transcript holds a full answer.
                            Some("text") if !streamed_text => {
                                if let Some(t) = b["text"].as_str() {
                                    if !t.trim().is_empty() {
                                        let _ = sink.send(AgentEvent::Text(t.to_string()));
                                        produced = true;
                                    }
                                }
                            }
                            Some("tool_use") => {
                                let _ = sink.send(AgentEvent::ToolCall {
                                    id: b["id"].as_str().unwrap_or("").to_string(),
                                    name: b["name"].as_str().unwrap_or("tool").to_string(),
                                    input: b["input"].clone(),
                                });
                                produced = true;
                            }
                            Some("thinking") => {
                                if let Some(t) = b["thinking"].as_str() {
                                    if !t.trim().is_empty() {
                                        let _ = sink.send(AgentEvent::Thinking(t.to_string()));
                                        produced = true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // This message LANDED — whatever it streamed is final, and the
                // next `message_start` is a genuinely new message, not a retry.
                msg_text.clear();
            }
            if v["type"] == "assistant" {
                let usage = RunStats::anthropic(&v["message"]["usage"]);
                let _ = sink.send(AgentEvent::Stats(RunStats {
                    model: v["message"]["model"].as_str().map(str::to_string),
                    context_used_tokens: usage.input_tokens,
                    context_basis: usage.input_tokens.map(|_| "last_request_input".into()),
                    ..Default::default()
                }));
            }
            // Tool results (e.g. bash output).
            // A subagent's tool results are already implied by the step line we
            // drew for the call, so they are dropped rather than doubling every
            // line in the card.
            if v["type"] == "user" && v["parent_tool_use_id"].as_str().is_some() {
                continue;
            }
            if v["type"] == "user" {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        if b["type"] == "tool_result" {
                            if let Some(id) = b["tool_use_id"].as_str() {
                                let _ = sink.send(AgentEvent::ToolStatus {
                                    id: id.to_string(), failed: b["is_error"].as_bool().unwrap_or(false),
                                });
                                let _ = sink.send(AgentEvent::ToolResult { id: id.to_string(), text: tool_result_text(b) });
                                produced = true;
                            }
                        }
                    }
                }
            }
            if v["type"] == "result" {
                // Whose result is this? Claude answers a background task's
                // completion with a TURN OF ITS OWN and emits a result for it —
                // on a connection that serves many turns those arrive while we
                // are reading. `origin` says so structurally, so the guess below
                // (`is_queued_receipt`) is now only the fallback for a CLI too
                // old to stamp it.
                if is_other_turns_result(&v) {
                    continue;
                }
                // A non-success result means the agent GAVE UP (API error, max
                // turns, exec error). Surface the specific reason and stop
                // cleanly, instead of ending as if it had succeeded (previously
                // any `result` was reported as a normal Done — errors vanished).
                let subtype = v["subtype"].as_str().unwrap_or("");
                if v["is_error"].as_bool().unwrap_or(false) || (!subtype.is_empty() && subtype != "success") {
                    error = Some(
                        v["result"].as_str().map(str::trim).filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("agent ended with `{}`", if subtype.is_empty() { "error" } else { subtype })),
                    );
                    break;
                }
                // Not OUR receipt. `claude -p --resume` drains the session's
                // input QUEUE before it looks at the prompt we came here with,
                // and a turn the user stopped leaves an item in that queue (a
                // `<task-notification>` saying a background shell has no
                // completion record). Claude closes that queued item as a turn
                // of its own — no model call, so zero usage, empty `result`,
                // ~100ms — emits a `result` for it, and only THEN dequeues our
                // prompt and starts working on the real answer.
                //
                // Breaking on that receipt is the "0.1s empty reply" the field
                // kept hitting: the renderer flushed a bare
                // `{% mafold/result duration="0.1s" %}`, the real answer was
                // streamed into a pipe with no reader left, and `child.wait()`
                // below then blocked forever on a process that was still
                // working — so the draft was never finalized and NEITHER retry
                // in `agent::handle` ever got to run. Step over the receipt and
                // keep reading; our own result is still coming.
                if is_queued_receipt(&v, produced) && skipped_receipts < MAX_SKIPPED_RECEIPTS {
                    skipped_receipts += 1;
                    continue;
                }
                // Last-resort carrier: a turn that succeeded but reached us with
                // NOTHING (no deltas, no assistant message, no tool events) still
                // has its final answer here. Delivering it beats finalizing an
                // empty bubble — the failure this guards against is silent, and
                // the daemon's empty-turn retry would otherwise burn a second
                // full turn just to lose the reply again.
                if !produced {
                    if let Some(t) = v["result"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        let _ = sink.send(AgentEvent::Text(t.to_string()));
                        produced = true;
                    }
                }
                let mut stats = RunStats::anthropic(&v["usage"]);
                stats.model_requests = v["num_turns"].as_u64();
                if let Some(models) = v["modelUsage"].as_object().filter(|m| m.len() == 1) {
                    if let Some((model, usage)) = models.iter().next() {
                        stats.model = Some(model.clone());
                        stats.context_limit_tokens = usage["contextWindow"].as_u64();
                    }
                }
                stats.cost_usd = v["total_cost_usd"].as_f64();
                stats.cost_kind = stats.cost_usd.map(|_| "reported".into());
                let _ = sink.send(AgentEvent::Stats(stats));
                let toks = usage_tokens(&v);
                let _ = sink.send(AgentEvent::Done {
                    duration_ms: v["duration_ms"].as_f64(),
                    cost_usd: v["total_cost_usd"].as_f64(),
                    tokens: if toks > 0 { Some(toks) } else { None },
                });
                break;
            }
        }
        drop(sink); // close → the renderer flushes its tail

        // The session id the connection saw is the authority: on a reused one
        // our local `session_id` only sees the frames of THIS turn, and a
        // connection that forked or compacted knows better.
        let session_id = conn.session_id().or(session_id);

        if stopped || error.is_some() {
            conn.kill(); // a turn that ended badly never goes back in the pool
            conn.wait_exit(std::time::Duration::from_secs(5)).await;
            crate::turnenv::remove(&crate::turnenv::path_for(&conn.id));
            return Ok(TurnOutcome { limit: limit_hit(limit.clone(), error.as_deref()), produced, stopped, session: session_id, error });
        }
        // `claude` normally exits within a beat of its final `result`. When it
        // does NOT — more queued input behind us, a background task it is still
        // winding down — an unbounded wait PARKS THE WHOLE TURN here: the reply
        // is never finalized (a draft that stays open forever), the caller's
        // retries never run, and the child lives on writing into a pipe with no
        // reader until it deadlocks on a full pipe buffer, still holding the
        // session and whatever tools it spawned. Two such orphans were alive on
        // the field machine when this was diagnosed — one of them still starting
        // shells 13 minutes after its "reply" had been rendered.
        //
        // We already have this turn's result, so an overstaying child is not a
        // failure of the reply: give it a grace period, then kill it and return
        // what we have.
        // The turn is over and the process is healthy. Park it for the next one:
        // 30 minutes of idle, or for as long as it still has in-process work
        // (a subagent or workflow would be KILLED with it — see `cc_conn`).
        if cc_conn::enabled() && conn.alive() {
            conn.end_turn();
            cc_conn::put(conn);
            return Ok(TurnOutcome { produced, stopped, session: session_id, error: None, limit: None });
        }

        const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(20);
        conn.close_stdin();
        let status = match conn.wait_exit(EXIT_GRACE).await {
            Some(s) => s,
            None => {
                crate::turnenv::remove(&crate::turnenv::path_for(&conn.id));
                return Ok(TurnOutcome { limit: limit_hit(limit.clone(), error.as_deref()), produced, stopped, session: session_id, error });
            }
        };
        crate::turnenv::remove(&crate::turnenv::path_for(&conn.id));
        if !status.success() {
            // The connection's reader already drained stderr (no post-wait read
            // that could have deadlocked) — just collect what it captured.
            let err = conn.stderr_text();
            let plain_tail = conn.plain_tail();
            // Reaching here at all means the process died WITHOUT a terminal
            // `result` line — EVERY `result`, success or `is_error`, breaks the
            // loop above and returns before this point. So this is the silent
            // death: claude quit without saying why on the stream, and (in the
            // cases seen in the field) without saying why on stderr either.
            //
            // NOT `bail!`. A nonzero exit means "the turn ended on an error",
            // which is exactly what `TurnOutcome::error` carries — and only that
            // form reaches the caller's stale-resume recovery, which drops the
            // resumed session and retries ONCE on a fresh one so the user's
            // message still gets answered. An `Err` here bypassed BOTH retry
            // paths in `agent::handle` (they only match `Ok`), so this silent
            // death burned the whole turn on a reply card that lived a couple of
            // seconds and the user had to notice and resend by hand. `produced`
            // rides along, so a run that already streamed work is never redone.
            let reason = exit_reason(status.code(), &err, &plain_tail);
            return Ok(TurnOutcome {
                produced,
                stopped,
                session: session_id,
                limit: limit_hit(limit, Some(&reason)),
                error: Some(reason),
            });
        }
        Ok(TurnOutcome { produced, stopped, session: session_id, error: None, limit: None })
    }

    fn discover(&self, workdir: &str) -> Value {
        crate::discover::all(workdir)
    }

    async fn command(&self, _client: &Client, _chat_id: &str, name: &str, arg: &str, workdir: &str, session: Option<&str>, env: &[(String, String)]) -> CommandOutcome {
        match crate::commands::handle(name, arg, workdir, session, env).await {
            crate::commands::Outcome::Reply(text) => CommandOutcome::Reply(text),
            crate::commands::Outcome::Forward => CommandOutcome::Forward,
        }
    }

    async fn status_line(&self, env: &[(String, String)]) -> String {
        crate::commands::auth_status_line(env).await
    }

    /// The seat `env` selects, asked the same quota-free question Claude Code
    /// asks itself (`/api/oauth/usage`), with the failure taxonomy kept apart
    /// — 401 wants a login, 403 does not, unreachable wants patience.
    async fn seat_health(&self, env: &[(String, String)]) -> SeatHealth {
        use crate::commands::UtilizationProbe as P;
        match crate::commands::probe_utilization(env).await {
            P::Ok(v) => {
                let limits = v["limits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| {
                        let percent = l["percent"].as_f64()?;
                        let kind = l["kind"].as_str().unwrap_or("usage").to_string();
                        Some(SeatLimit {
                            label: seat_limit_label(&kind, l),
                            kind,
                            percent,
                            resets_at: l["resets_at"].as_str().and_then(crate::commands::iso_epoch_secs),
                        })
                    })
                    .collect();
                // The plan chip comes from `~/.claude.json`, which the LAST
                // login to sign in owns — only the default seat can claim it.
                let tier = if crate::accounts::Account::from_env(env).is_default() {
                    crate::commands::plan_tier()
                } else {
                    None
                };
                SeatHealth::from_limits(tier, limits)
            }
            P::NoCredential => SeatHealth::unauthenticated(),
            P::Http(s) => SeatHealth::from_status(s),
            P::Unreachable => SeatHealth::unreachable(),
        }
    }

    async fn cli_version(&self) -> String {
        crate::commands::claude_version().await
    }

    /// Ask THIS `claude` what it accepts. Three questions, none of which costs
    /// a turn:
    ///
    /// 1. the control-protocol handshake — `initialize` over stream-json with
    ///    no user message. It answers with the model roster, each model's own
    ///    effort tiers, and which login was asked (~4s, zero tokens).
    /// 2. `--effort <nonsense> --version` — the binary names its valid tiers in
    ///    the warning it prints. This is both the fallback when 1 can't be had
    ///    and the PROOF that this build complains about a tier it doesn't know.
    /// 3. `--effort <candidate> --version` for the values Claude Code takes and
    ///    prints nowhere ([`HIDDEN_EFFORTS`]). Silence = accepted — but only on
    ///    a build we just watched complain in 2, because on a build that never
    ///    complains silence means nothing and nothing may be claimed from it.
    ///
    /// Every answer here is about the binary in front of us: nothing in this
    /// function names a model, and the one place a tier is named is a question.
    async fn caps(&self, env: &[(String, String)], _version: &str) -> Option<HarnessProbe> {
        // (2) first — it is ~300ms and it decides how (3) may be read.
        let complaint = effort_probe(env, EFFORT_NONSENSE).await;
        let valid = complaint.as_deref().and_then(valid_efforts);
        let answer = handshake(env).await;
        let mut probe = HarnessProbe {
            models: answer.as_ref().map(models_of).unwrap_or_default(),
            account: answer.as_ref().and_then(account_of),
            ..Default::default()
        };
        if !probe.models.is_empty() {
            probe.source = CapsSource::Handshake;
        } else if let Some(tiers) = valid.clone() {
            // No roster, but the tiers are real and the effort menu is the one
            // that was wrong. Reporting them says nothing about the models —
            // `HarnessCaps::tells_models` keeps that menu as it was.
            probe.source = CapsSource::HelpText;
            probe.efforts = tiers;
        }
        // (3) — only where a "no complaint" answer is worth something.
        if valid.is_some() {
            for candidate in HIDDEN_EFFORTS {
                let taken = effort_probe(env, candidate)
                    .await
                    .is_some_and(|err| !complains_about(&err, candidate));
                if taken {
                    probe.modes.push(ModeCap { id: (*candidate).to_string(), pins_effort: None });
                }
            }
        }
        (!probe.is_empty()).then_some(probe)
    }
}

/// A value `--effort` cannot possibly mean, used to make the binary state its
/// own valid list.
const EFFORT_NONSENSE: &str = "mafold-probe";

/// Effort values Claude Code accepts but prints nowhere — not in `--help`, not
/// among the handshake's per-model tiers. A name here is a QUESTION put to the
/// binary, never a claim: a build that doesn't take it never offers it, and a
/// build that stops taking it drops it at the next probe.
const HIDDEN_EFFORTS: &[&str] = &["ultracode"];

/// Run `claude --effort <value> --version` and hand back its stderr. The pair
/// is deliberate: `--version` makes the binary parse the flags and exit at once
/// (no session, no quota), which is all the question needs.
///
/// `None` when the CLI couldn't be run at all — distinct from an empty stderr,
/// which is the binary saying it has no objection.
async fn effort_probe(env: &[(String, String)], value: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new(super::program("claude"));
    cmd.arg("--effort")
        .arg(value)
        .arg("--version")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null());
    crate::platform::no_window(&mut cmd);
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Did the binary refuse `value` as a tier it doesn't know?
fn complains_about(stderr: &str, value: &str) -> bool {
    let s = stderr.to_lowercase();
    s.contains("--effort") && s.contains(&value.to_lowercase()) && s.contains("unknown")
}

/// The tiers named in that complaint: `… Valid values: low, medium, high,
/// xhigh, max.` → the five. None when the binary said nothing of the sort, and
/// that None is load-bearing — it means this build's silence proves nothing.
fn valid_efforts(stderr: &str) -> Option<Vec<String>> {
    let at = stderr.to_lowercase().find("valid values:")?;
    let list = &stderr[at + "valid values:".len()..];
    let list = list.split(['\n', '.']).next().unwrap_or(list);
    let tiers: Vec<String> = list
        .split(',')
        .map(|s| s.trim().trim_matches('`').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty() && s.len() <= 32 && !s.contains(' '))
        .collect();
    (!tiers.is_empty()).then_some(tiers)
}

/// The models the handshake reported, in its own order. Every field is
/// optional-tolerant: the shape has grown between builds (2.1.272 added
/// `agents`), so a missing key means "this build didn't say", never a parse
/// failure that costs the whole roster.
fn models_of(resp: &Value) -> Vec<ModelCap> {
    let s = |v: &Value, k: &str| v[k].as_str().map(str::to_string).filter(|x| !x.is_empty());
    resp["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = s(m, "value")?;
            Some(ModelCap {
                display: s(m, "displayName").unwrap_or_else(|| id.clone()),
                resolved: s(m, "resolvedModel"),
                // Claude Code takes `--model fable` as well as the id it
                // reports, but it accepts an unknown `--model` in silence (it
                // only complains once a turn spends a token), so there is no
                // quota-free way to ASK which other spellings work. An alias
                // here would be this daemon guessing about someone else's
                // build, which is the whole habit this probe exists to end.
                aliases: Vec::new(),
                description: s(m, "description"),
                efforts: m["supportedEffortLevels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect(),
                id,
            })
        })
        .collect()
}

/// The login the handshake answered for (`account.email`), when it names one.
fn account_of(resp: &Value) -> Option<String> {
    resp["account"]["email"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// The `initialize` control request, answered by `claude` itself.
///
/// No user message is ever written, so the process starts up, reports what it
/// is, and exits without running a turn — the one way to ask "what do you
/// support" that doesn't spend the thing being asked about.
async fn handshake(env: &[(String, String)]) -> Option<Value> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new(super::program("claude"));
    cmd.arg("-p")
        .arg("--input-format").arg("stream-json")
        .arg("--output-format").arg("stream-json")
        .arg("--verbose")
        .arg("--dangerously-skip-permissions")
        // The user's global MCP servers have nothing to say about this and
        // would each be spawned to not say it (the same reason `run` sets it).
        .arg("--strict-mcp-config")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        // Ask the BINARY, not a project: a workdir brings its own settings and
        // hooks, and the answer must not depend on which bot asked.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::platform::no_window(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let _guard = super::ChildGuard::new(child.id());
    let req = serde_json::json!({
        "type": "control_request",
        "request_id": HANDSHAKE_ID,
        "request": { "subtype": "initialize" },
    });
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{req}\n").as_bytes()).await;
        // EOF: there is no prompt coming, and claude exits once it has answered.
        drop(stdin);
    }
    let stdout = child.stdout.take()?;
    let mut lines = BufReader::new(stdout).lines();
    let found = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["type"] == "control_response" && v["response"]["request_id"] == HANDSHAKE_ID {
                return Some(v["response"]["response"].clone());
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    // Nothing more is wanted from it, and a build that waits for more input
    // must not outlive the question.
    let _ = child.start_kill();
    found
}

const HANDSHAKE_ID: &str = "mafold-caps";

/// The `--mcp-config` that mounts our permission server and nothing else.
///
/// Its own function so a test can prove the server KEY here and the
/// `mcp__server__tool` reference passed to `--permission-prompt-tool` still name
/// the same thing. If they ever disagree claude finds no such tool, and the
/// failure mode is not an error — it is every gated call hanging until it times
/// out ten minutes later.
fn permission_mcp_config(exe: &str) -> String {
    serde_json::json!({
        "mcpServers": {
            crate::permission_mcp::SERVER: { "command": exe, "args": ["permission-mcp"] }
        }
    })
    .to_string()
}

/// Stops the permission watcher and clears its mailbox when the turn ends,
/// whichever way it ended. A guard rather than five `abort()` calls: the watcher
/// polls forever by construction, and `run` returns from five different places
/// (spawn error, cancel, clean exit, non-zero exit, stream error) — one of them
/// forgetting would leak a task per turn, forever.
struct PermWatch {
    task: tokio::task::JoinHandle<()>,
    file: String,
}

impl Drop for PermWatch {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Turn each permission question `permission_mcp` publishes into the same
/// `AskUserQuestion` event the model's own interactive asks produce.
///
/// The reason this is a FILE watcher and not a stream reader: claude does not
/// put the permission-prompt tool call on its output stream at all (verified —
/// the stream shows only the `Bash` call it is asking about, and the MCP tool
/// isn't even in the session's tool list). So the question has to arrive out of
/// band. Emitting it as `AskUserQuestion` is what makes the rest free: the
/// renderer already draws that name as `{% mafold/ask %}`, and the daemon
/// already arms the turn's answer mailbox on it. Nothing downstream needed a new
/// concept for "a permission question" — it IS a question.
fn permission_watcher(file: String, sink: UnboundedSender<AgentEvent>) -> PermWatch {
    let path = file.clone();
    let task = tokio::spawn(async move {
        // Requests are strictly sequential (the turn is blocked on each one), so
        // "how many lines have I already drawn" is enough to never draw twice.
        let mut drawn = 0usize;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            // `std::fs` on purpose: a few hundred bytes out of the temp dir, and
            // tokio's `fs` feature isn't enabled in this crate.
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            for line in body.lines().skip(drawn) {
                drawn += 1;
                let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
                let id = record["tool_use_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("perm-{drawn}"));
                if sink
                    .send(AgentEvent::ToolCall {
                        id,
                        name: "AskUserQuestion".into(),
                        input: crate::permission_mcp::ask_card_input(&record),
                    })
                    .is_err()
                {
                    return; // renderer is gone — the turn is over
                }
            }
        }
    });
    PermWatch { task, file }
}

/// The context size a `compact_boundary` event says it compacted. Its own
/// function so the JSON path is pinned by a test against a real captured event —
/// a silently-wrong path here reads exactly like no compaction at all.
fn compaction_pre_tokens(v: &Value) -> Option<u64> {
    v["compactMetadata"]["preTokens"].as_u64()
}

/// The usage-limit state worth relaying from a `rate_limit_event`'s
/// `rate_limit_info` — `(kind, resets_at, status)` — or None when the limit
/// is healthy. Claude emits one of these on ordinary turns too, so the
/// "allowed" gate is what keeps this from stamping a quota notice onto every
/// single reply — it is load-bearing, not defensive. The status rides along
/// because `rejected` (refused) and `allowed_warning` (full, but extra usage
/// is paying) are different news, to the reader and to the seat logic.
fn rate_limit_notice(info: &Value) -> Option<(String, Option<i64>, String)> {
    let status = info["status"].as_str().unwrap_or("allowed");
    if status == "allowed" {
        return None;
    }
    Some((
        info["rateLimitType"].as_str().unwrap_or("usage").to_string(),
        info["resetsAt"].as_i64(),
        status.to_string(),
    ))
}

/// Did this run end BECAUSE the seat's usage window is full? Only a turn that
/// ended on an error can have — a refused request the SDK then recovered from
/// is not a wall. The evidence is a `rejected` rate-limit event seen on the
/// stream, or a fatal reason that says so in words (`Claude usage limit
/// reached|<epoch>` on stdout, `You've hit your session limit · resets …` as
/// the result).
fn limit_hit(seen: Option<super::LimitHit>, error: Option<&str>) -> Option<super::LimitHit> {
    let text = error?;
    if seen.is_some() {
        return seen;
    }
    crate::accounts::usage_limit_reset(text).map(|resets_at| super::LimitHit { kind: "usage".into(), resets_at })
}

/// The human label for a usage window, the way the `/stats` card names them.
fn seat_limit_label(kind: &str, l: &Value) -> String {
    match kind {
        "session" => "Session".to_string(),
        "weekly_all" => "Week (all models)".to_string(),
        "weekly_scoped" => format!("Week ({})", l["scope"]["model"]["display_name"].as_str().unwrap_or("scoped")),
        other => {
            let s = other.replace('_', " ");
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Why a `claude` run that exited nonzero failed, in the most useful words we
/// have: stderr when claude wrote there, else the plain-text stdout tail, else
/// the exit code itself. Never a bare "exited unsuccessfully" — a failure with
/// no reason attached is unactionable for the user AND undiagnosable from the
/// daemon log, which is how this class of dead reply went unexplained for weeks.
fn exit_reason(code: Option<i32>, stderr: &str, plain_tail: &[String]) -> String {
    const HEAD: &str = "claude exited unsuccessfully";
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return format!("{HEAD}: {stderr}");
    }
    let tail: Vec<&str> = plain_tail.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    if !tail.is_empty() {
        return format!("{HEAD}: {}", tail.join(" / "));
    }
    match code {
        Some(c) => format!("{HEAD} (exit code {c}, and it printed nothing to stdout or stderr)"),
        None => format!("{HEAD} (killed by a signal, and it printed nothing to stdout or stderr)"),
    }
}

/// A tool_result's `content` can be a string or an array of `{type:text,text}`.
fn tool_result_text(b: &Value) -> String {
    match &b["content"] {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().filter_map(|i| i["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Every `usage` field that counts toward a turn's token total.
const USAGE_KEYS: [&str; 4] =
    ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"];

fn usage_tokens(v: &Value) -> u64 {
    let u = &v["usage"];
    USAGE_KEYS.iter().filter_map(|k| u[*k].as_u64()).sum()
}

/// Everything a `claude` process needs at SPAWN time, and the two hook blobs
/// whose fate the handshake decides.
struct Built {
    cmd: tokio::process::Command,
    /// The full COMMAND-hook settings, attached only when the control-channel
    /// handshake fails.
    hook_settings: Option<String>,
    /// The Bash hook alone — always attached (see `cc_conn`'s callback ids).
    bash_only: String,
}

/// Build the command for a turn's SHAPE. Nothing per-turn goes in here: the
/// draft, the ask/steer/permission mailboxes and the watcher all belong to one
/// turn, while this process may serve many, and they reach the child through
/// `turnenv` instead. That is what lets [`ClaudeCode::prewarm`] build the same
/// process before there is a message to answer.
fn build_cmd(shape: &super::TurnShape, exe: &str) -> Built {
        let mut cmd = tokio::process::Command::new(super::program("claude"));
        // `-p` with NO prompt argument: the prompt goes in on stdin instead (see
        // the write below). It is the one input here that grows without bound —
        // it carries the conversation — and Windows hard-caps a command line at
        // 32,767 UTF-16 units, so on argv a long enough chat makes `CreateProcessW`
        // refuse the spawn outright (os error 206, ERROR_FILENAME_EXCED_RANGE).
        // Unconditionally, not past some Windows-only threshold: a size cliff that
        // only one platform falls off, and only on long conversations, is exactly
        // the kind of special case that gets shipped untested. `run_claude_stdin`
        // feeds /usage the same way.
        cmd.arg("-p")
            // One JSON line per user message instead of raw text. This is what
            // lets stdin stay OPEN after the prompt: the process can take
            // another turn (see `cc_conn`), and the same pipe carries the
            // control channel. A one-shot run closes stdin at the end and exits
            // exactly as it always did.
            .arg("--input-format").arg("stream-json")
            .arg("--output-format").arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .arg("--dangerously-skip-permissions")
            // Ignore the user's GLOBAL MCP servers (e.g. browser-use): claude
            // would otherwise spawn every configured MCP server on EVERY turn,
            // which pops a Python dock icon and adds seconds of startup latency
            // per reply. The daemon passes no --mcp-config, so this loads none.
            .arg("--strict-mcp-config");
        if let Some(m) = &shape.model {
            cmd.arg("--model").arg(m);
        }
        // Reasoning effort (owner-set via Customization). No flag = Claude Code's
        // own default (currently xHigh).
        if let Some(e) = &shape.effort {
            cmd.arg("--effort").arg(e);
        }
        // Export the current conversation so `mafold room …` (run by the agent
        // via the room skill) defaults to THIS room. Per-turn (not a global env)
        // because concurrent turns run different conversations.
        cmd.env("MAFOLD_CONV", &shape.conv);
        // The surface (conv + forum channel) the reply lands on — the bash-hook
        // registers detached background tasks under it, so their wrap-up turn
        // comes back to THIS channel instead of leaking into another one.
        cmd.env("MAFOLD_SURFACE", &shape.surface);
        // Name this process in the machine's session registry
        // (`~/.claude/sessions/<pid>.json`), which is the address book every
        // other local claude session sees. Without it the name is auto-derived
        // from the pid — `mafold-3e`, a different one every turn — so nobody
        // could address us twice. Keyed by the SURFACE, so a warm connection
        // keeps one name for as long as it serves that conversation.
        cmd.env("CLAUDE_CODE_SESSION_NAME", session_peer_name(&shape.surface));
        // The seat: which Claude login this turn runs on
        // (`CLAUDE_SECURESTORAGE_CONFIG_DIR`, see `crate::accounts`). Empty for
        // the default login — the daemon's own environment already is it.
        // Only the credential moves with it; `~/.claude` (memory, skills,
        // sessions) stays shared, which is what lets a `--resume` below carry
        // on under a different account.
        cmd.envs(shape.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        // Extended thinking: a non-zero budget makes the model think before each
        // reply (streamed as `thinking` blocks). No env = Claude Code's default.
        if let Some(budget) = shape.thinking {
            cmd.env("MAX_THINKING_TOKENS", budget.to_string());
        }
        // mafold awareness: who the bot is, the conversation, embeddable cards.
        if let Some(sys) = &shape.system {
            cmd.arg("--append-system-prompt").arg(sys);
        }
        // Interactive AskUserQuestion: a PreToolUse hook intercepts the native
        // tool (which would otherwise auto-decline headless), blocks until the
        // user answers the chat card, then returns the answer as a deny-reason —
        // which claude feeds back as the tool result, same turn. The hook waits
        // on MAFOLD_ASK_FILE (the daemon writes the answer there). See ask_hook.
        let mut pre: Vec<serde_json::Value> = Vec::new();
        let mut post: Vec<serde_json::Value> = Vec::new();
        let mut hook_settings: Option<String> = None;
        // Detach run_in_background Bash tasks into their own session (registered
        // under ~/.mafold/bgtasks by MAFOLD_SURFACE) — claude kills its own
        // background shells the moment it exits, so without this they can never
        // outlive the turn. See bash_hook.
        //
        // Unconditional, NOT nested under the ask-file arm it used to share:
        // background detaching has nothing to do with interactive questions, and
        // tying them together meant a turn with no ask-file silently lost its
        // background tasks to claude's exit-time killpg.
        // ALWAYS a command hook, never a control callback. Its mechanism is
        // that the hook PROCESS exits right after spawning the detached task,
        // which is what makes init adopt it; answered inside the daemon the task
        // would stay our child, nobody would reap it, and the completion monitor
        // would read the zombie pid as "still running" and never report the
        // result. Kept in its own settings blob so it survives either side of
        // the hook handshake below.
        let bash_hook = serde_json::json!({
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": format!("\"{exe}\" bash-hook") }]
        });
        let bash_only = serde_json::json!({ "hooks": { "PreToolUse": [bash_hook.clone()] } }).to_string();
        pre.push(bash_hook);
        {
            pre.push(serde_json::json!({
                "matcher": "AskUserQuestion",
                "hooks": [{ "type": "command", "command": format!("\"{exe}\" ask-hook") }]
            }));
            // The user's OWN `ask` rules (`ask: ["Bash(rm *)"]`) mean "a person
            // must say yes". They outrank `--dangerously-skip-permissions`, an
            // `allow` rule, and a PreToolUse hook's `allow` — all three verified
            // against claude 2.1.260 — and headless there is nobody to ask, so
            // claude denied them outright. Point it at a person instead: this
            // server puts the question in the reply as the ask card and blocks on
            // the tap. `--strict-mcp-config` stays, so the user's global MCP
            // servers still don't load — this config names ours and nothing else.
            cmd.arg("--mcp-config").arg(permission_mcp_config(exe));
            cmd.arg("--permission-prompt-tool").arg(crate::permission_mcp::TOOL_REF);
        }
        // Mid-turn steering: what the user says while this turn runs reaches the
        // model at the next tool-result boundary. PostToolUse, matching every
        // tool, so the tool that was running when they spoke finishes normally
        // and nothing already on screen is un-said. See `steer_hook`.
        {
            post.push(serde_json::json!({
                "matcher": "*",
                "hooks": [{ "type": "command", "command": format!("\"{exe}\" steer-hook") }]
            }));
        }
        if !pre.is_empty() || !post.is_empty() {
            let mut hooks = serde_json::Map::new();
            if !pre.is_empty() {
                hooks.insert("PreToolUse".into(), serde_json::Value::Array(pre));
            }
            if !post.is_empty() {
                hooks.insert("PostToolUse".into(), serde_json::Value::Array(post));
            }
            // NOT attached yet. This is the COMMAND form of every hook — claude
            // spawns `mafold <hook>` as its own process for each one, and for
            // the steer hook that is a process per tool call. Ask and steer are
            // registered over the control channel instead (in-process, and able
            // to read the CURRENT turn rather than the environment this process
            // was born with), so this is kept as the fallback for a CLI that
            // can't do that. Attaching both would fire every hook twice.
            hook_settings = Some(serde_json::json!({ "hooks": hooks }).to_string());
        }
        cmd.kill_on_drop(true);
        if let Some(sid) = &shape.session {
            cmd.arg("--resume").arg(sid);
            // Somebody else is holding this exact transcript right now (a VS
            // Code tab, a terminal). Print-mode `--resume` does NOT fork — it
            // hands back the same session id and appends to the same file — so
            // without this both writers braid into one session tree and the
            // resume pointer ends up wherever the last write landed. Forking
            // inherits everything they've typed up to this instant and leaves
            // their thread alone, which is what `/resume` has been promising in
            // words all along. The new id arrives on the stream (`session_id`)
            // and is what the caller stores, so this costs one fork, not one
            // per turn.
            if crate::commands::session_held_elsewhere(sid) {
                cmd.arg("--fork-session");
            }
        }
        Built { cmd, hook_settings, bash_only }
}

/// The first non-empty line of `t`, bounded — a subagent's report can be pages
/// long and it is going into one row of a fixed-width card.
fn first_line(t: &str) -> String {
    let line = t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    const MAX: usize = 120;
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    line.chars().take(MAX).collect::<String>() + "…"
}

/// The name this turn's process announces in the local session registry.
///
/// Short and stable, because it is an ADDRESS: another session types it into
/// `SendMessage`. Derived from the surface (conversation, plus the forum
/// channel when there is one) so the same conversation always answers to the
/// same name, and two conversations never collide.
fn session_peer_name(surface: &str) -> String {
    let short: Vec<String> = surface
        .split("__")
        .take(2)
        .map(|p| p.chars().filter(|c| c.is_ascii_alphanumeric()).take(6).collect::<String>())
        .filter(|s: &String| !s.is_empty())
        .collect();
    if short.is_empty() {
        return "mafold".into();
    }
    format!("mafold-{}", short.join("-"))
}

/// What to say about a `peer_message_hold`, or None when it isn't news.
///
/// Only the `held` state is: it is the one that means a message is NOT going to
/// arrive on its own. `released` says it went through after all (the user will
/// simply see it), and `dropped` is reported by the sending side.
fn peer_hold_notice(v: &Value) -> Option<String> {
    if v["state"].as_str()? != "held" {
        return None;
    }
    let who = v["from_name"]
        .as_str()
        .or_else(|| v["from"].as_str())
        .unwrap_or("another session on this machine");
    let why = match v["cause"].as_str() {
        Some("mode-mismatch") => " (it runs in a different permission mode)",
        Some("no-mode-asserted") => " (it didn't say which permission mode it runs in)",
        Some(_) | None => "",
    };
    Some(format!("`{who}` tried to message this session and it was held{why} — it has not reached me."))
}

/// Does this `result` belong to a turn claude started BY ITSELF?
///
/// Claude answers a background task's completion with a whole turn of its own
/// and emits a `result` for it. On a connection that serves many turns those
/// land while we are reading, and taking one as ours would end the reply early
/// (the "0.1s empty reply"). `origin` is absent only on a result that answers a
/// message the CLIENT sent, which is why this is a field test and not a guess.
///
/// Unknown `origin.kind` values count as NOT ours: the set grows over time, and
/// every member of it is by definition a turn we did not ask for.
fn is_other_turns_result(v: &Value) -> bool {
    v["origin"]["kind"].as_str().is_some()
}

/// Is this successful `result` the receipt for a message that ISN'T ours — a
/// queued `<task-notification>` claude closed out before it even looked at our
/// prompt? Three things are true of one and of nothing else: this run has
/// produced nothing at all, the result text is empty, and it burned zero tokens
/// (no model call happened, so there is no usage). A real turn always spends
/// input tokens, so a real turn can never look like this.
///
/// `is_error` / non-success subtypes are handled before this is consulted.
fn is_queued_receipt(v: &Value, produced: bool) -> bool {
    !produced
        && usage_tokens(v) == 0
        && v["result"].as_str().map(str::trim).is_none_or(str::is_empty)
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use mafold_transcript::{Advance, Transcript};

    /// The flag and the config have to agree on the server's name. They are
    /// written in two different files, and disagreeing costs a ten-minute hang
    /// per gated call rather than an error anyone would notice.
    #[test]
    fn the_mounted_server_is_the_one_the_flag_names() {
        let cfg: Value = serde_json::from_str(&permission_mcp_config("/usr/local/bin/mafold")).unwrap();
        let servers = cfg["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 1, "only ours may be mounted: {cfg}");
        let (name, spec) = servers.iter().next().unwrap();
        assert_eq!(
            crate::permission_mcp::TOOL_REF,
            format!("mcp__{name}__{}", crate::permission_mcp::TOOL),
        );
        assert_eq!(spec["command"], "/usr/local/bin/mafold");
        assert_eq!(spec["args"][0], "permission-mcp");
    }

    /// The watcher's whole job: a line `permission_mcp` appended becomes an
    /// interactive ask on the sink, carrying the command it is asking about.
    #[tokio::test]
    async fn a_published_question_becomes_an_ask_event() {
        let file = std::env::temp_dir()
            .join(format!("mafold-permwatch-{}.jsonl", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&file);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = permission_watcher(file.clone(), tx);

        std::fs::write(
            &file,
            "{\"tool_name\":\"Bash\",\"input\":{\"command\":\"rm -rf build\"},\"tool_use_id\":\"toolu_9\"}\n",
        )
        .unwrap();

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the watcher never drew the question")
            .expect("sink closed");
        let AgentEvent::ToolCall { id, name, input } = ev else {
            panic!("expected a tool call, got {ev:?}");
        };
        // Named for the tool the renderer already draws as an ask card, and
        // that the daemon already arms the answer mailbox on.
        assert_eq!(name, "AskUserQuestion");
        // Carries claude's own id, so the question is traceable to the call.
        assert_eq!(id, "toolu_9");
        assert!(
            input["questions"][0]["question"].as_str().unwrap().contains("rm -rf build"),
            "{input}"
        );
        drop(watch);
        let _ = std::fs::remove_file(&file);
    }

    /// The event has to survive the real renderer as a TAPPABLE card — if it
    /// came out as a plain tool card, the turn would block on a question with
    /// no buttons on it.
    #[test]
    fn the_ask_event_renders_as_a_tappable_card() {
        let mut tx = Transcript::new();
        let advance = tx.push(&AgentEvent::ToolCall {
            id: "toolu_9".into(),
            name: "AskUserQuestion".into(),
            input: crate::permission_mcp::ask_card_input(&serde_json::json!({
                "tool_name": "Bash",
                "input": { "command": "rm .obsidian/app.json.bak" },
            })),
        });
        // Immediate: nobody can answer a question that is still sitting in a
        // 300ms render batch.
        assert!(matches!(advance, Advance::Immediate), "{advance:?}");
        let md = tx.finish();
        // Carries its OWN action: the tap is relayed to this daemon rather than
        // posted as a chat message (which is what the bare card's default
        // `ask:answer` does). A bare opener here would mean a stray "Allow"
        // bubble in the room on every guarded command.
        assert!(
            md.contains(&format!("{{% mafold/ask action=\"{}\" %}}", crate::permission_mcp::ACTION)),
            "{md}"
        );
        assert!(md.contains("rm .obsidian/app.json.bak"), "{md}");
        assert!(md.contains(&format!("o|{}|", crate::permission_mcp::ALLOW)), "{md}");
        assert!(md.contains(&format!("o|{}|", crate::permission_mcp::DENY)), "{md}");
    }

    /// The watcher stops with the turn. It polls forever by construction, so a
    /// leak here is one live task per reply for the life of the daemon.
    #[tokio::test]
    async fn dropping_the_guard_stops_the_watcher_and_clears_the_mailbox() {
        let file = std::env::temp_dir()
            .join(format!("mafold-permdrop-{}.jsonl", std::process::id()))
            .to_string_lossy()
            .into_owned();
        std::fs::write(&file, "").unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = permission_watcher(file.clone(), tx);
        let task = watch.task.abort_handle();
        drop(watch);
        assert!(task.is_finished() || { tokio::task::yield_now().await; task.is_finished() });
        assert!(!std::path::Path::new(&file).exists(), "mailbox left behind");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape claude emitted at 13:45:08 on the field machine, right
    /// after a stopped turn left a `<task-notification>` in the session queue.
    /// Breaking the read loop here is what rendered the "0.1s empty reply" and
    /// then parked the turn forever in `child.wait()`.
    #[test]
    fn queued_notification_receipt_is_not_our_turn() {
        let v: Value = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "duration_ms": 103, "result": "", "session_id": "e6b17a8d",
            "usage": {"input_tokens": 0, "output_tokens": 0}
        });
        assert!(is_queued_receipt(&v, false), "must be stepped over");
    }

    /// A real answer's receipt must END the turn — never be mistaken for a
    /// queued one, or the reply would hang until the stall watchdog fires.
    #[test]
    fn a_real_turns_receipt_is_ours() {
        let real: Value = serde_json::json!({
            "type": "result", "subtype": "success", "duration_ms": 21000,
            "result": "done", "usage": {"input_tokens": 12000, "output_tokens": 300}
        });
        assert!(!is_queued_receipt(&real, true), "streamed output already proves it is ours");
        assert!(!is_queued_receipt(&real, false), "text + usage prove it is ours");

        // Streaming carried the whole reply, so the final result text is empty —
        // usage still says a model call happened.
        let streamed: Value = serde_json::json!({
            "type": "result", "subtype": "success", "result": "",
            "usage": {"input_tokens": 9000, "cache_read_input_tokens": 400}
        });
        assert!(!is_queued_receipt(&streamed, false), "zero-text but real usage is ours");
    }

    #[test]
    fn usage_tokens_sums_every_counter() {
        let v: Value = serde_json::json!({"usage": {
            "input_tokens": 1, "output_tokens": 2,
            "cache_read_input_tokens": 4, "cache_creation_input_tokens": 8
        }});
        assert_eq!(usage_tokens(&v), 15);
        assert_eq!(usage_tokens(&serde_json::json!({})), 0);
    }

    /// The three 0.0s cards in the field carried this on stderr — that path
    /// already worked and must keep working.
    #[test]
    fn stderr_is_the_reason_when_claude_writes_there() {
        let r = exit_reason(Some(1), "No conversation found with session ID: 29cbfee1\n", &[]);
        assert!(r.contains("No conversation found"), "{r}");
    }

    /// The regression this fixes: claude printed the reason as PLAIN TEXT on
    /// stdout, the stream-json parser dropped it as unparseable, stderr was
    /// empty — and the user got a bare "claude exited unsuccessfully" with
    /// nothing to act on and nothing in the log to diagnose.
    #[test]
    fn plain_stdout_tail_is_the_reason_when_stderr_is_empty() {
        let tail = vec!["Claude usage limit reached|1785900000".to_string()];
        let r = exit_reason(Some(1), "   \n ", &tail);
        assert!(r.contains("usage limit reached"), "{r}");
    }

    /// Even with nothing on either stream, the exit code is real information —

    /// The exact three results one turn produced on 2026-09-15 when it started a
    /// subagent and a background shell: ours, then two turns claude ran on its
    /// own to close those out. On a pooled connection all three arrive while we
    /// are still reading, so only the first may end the turn.
    #[test]
    fn a_background_tasks_follow_up_turn_is_not_our_result() {
        let ours = serde_json::json!({
            "type": "result", "subtype": "success", "result_index": 0, "origin": null,
        });
        let after_subagent = serde_json::json!({
            "type": "result", "subtype": "success", "result_index": 1,
            "origin": {"kind": "task-notification"},
        });
        assert!(!is_other_turns_result(&ours), "the answer to our prompt ends the turn");
        assert!(is_other_turns_result(&after_subagent), "a task-notification turn does not");
        let future = serde_json::json!({ "type": "result", "origin": {"kind": "added-in-2027"} });
        assert!(is_other_turns_result(&future), "the set grows; unknown is still not ours");
        let old = serde_json::json!({ "type": "result", "subtype": "success" });
        assert!(!is_other_turns_result(&old), "a CLI too old to stamp it falls through");
    }

    /// Only a HELD peer message is news. `released` means it got through (the
    /// user will see it), and `dropped` is the sender's side of the story.
    #[test]
    fn only_a_held_peer_message_is_relayed() {
        let held = serde_json::json!({
            "type": "system", "subtype": "peer_message_hold", "state": "held",
            "lane": "socket", "from": "mafold-3e", "from_name": "review session",
            "cause": "mode-mismatch",
        });
        let t = peer_hold_notice(&held).expect("held is news");
        assert!(t.contains("review session"), "{t}");
        assert!(t.contains("permission mode"), "the cause is the actionable half: {t}");
        for state in ["released", "dropped"] {
            assert!(peer_hold_notice(&serde_json::json!({"state": state, "from": "x"})).is_none());
        }
        assert!(peer_hold_notice(&serde_json::json!({"state": "held"})).is_some());
    }

    /// The name is an ADDRESS another session types, so it has to be stable for
    /// a conversation and distinct between two.
    #[test]
    fn the_peer_name_is_stable_per_surface_and_distinct_between_them() {
        let a = session_peer_name("85c0609e-5cb9-4f05-a2af-cb99f0cfa1f9");
        assert_eq!(a, session_peer_name("85c0609e-5cb9-4f05-a2af-cb99f0cfa1f9"));
        assert_ne!(a, session_peer_name("7abf9f90-077b-4bbf-9aa4-a69f9557833e"));
        let chan = session_peer_name("85c0609e-5cb9__c71bbd28-7781");
        assert_ne!(a, chan, "a forum channel is part of the surface, and of the name");
        assert!(chan.starts_with("mafold-") && chan.len() <= 24, "an address must be typeable: {chan}");
        assert_eq!(session_peer_name(""), "mafold");
    }

    /// A subagent's report goes into ONE row of a fixed-width card.
    #[test]
    fn a_subagents_line_is_the_first_line_and_bounded() {
        assert_eq!(first_line("\n\n  hello \nworld"), "hello");
        assert_eq!(first_line(""), "");
        let long = "x".repeat(500);
        let cut = first_line(&long);
        assert!(cut.chars().count() <= 121 && cut.ends_with('…'), "{}", cut.len());
    }

    /// Blank-only breadcrumbs must not masquerade as an explanation.
    #[test]
    fn whitespace_only_tail_falls_through_to_the_exit_code() {
        let r = exit_reason(Some(2), "", &["".into(), "   ".into()]);
        assert!(r.contains("exit code 2"), "{r}");
    }

    /// A real auto-compaction event, verbatim from a transcript on disk. Pins
    /// the JSON path — read the wrong one and compaction goes back to being
    /// invisible, with nothing failing to say so.
    #[test]
    fn pre_tokens_come_from_a_real_compact_boundary_event() {
        let v: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted",
                "isMeta":false,"level":"info",
                "compactMetadata":{"trigger":"auto","preTokens":302336,"durationMs":135931,
                                   "preCompactDiscoveredTools":["WebFetch","WebSearch"]}}"#,
        )
        .unwrap();
        assert_eq!(compaction_pre_tokens(&v), Some(302336));
    }

    /// The event carries no post-compaction count, so an absent `preTokens`
    /// must degrade to "compacted, size unknown" rather than to a zero that a
    /// caller could mistake for a real measurement.
    #[test]
    fn a_compaction_event_without_pre_tokens_is_none_not_zero() {
        let v: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto"}}"#,
        )
        .unwrap();
        assert_eq!(compaction_pre_tokens(&v), None);
    }

    /// A real healthy rate-limit event, verbatim off the stream. Claude sends
    /// one of these on ordinary turns — relaying it would stamp a quota notice
    /// onto every reply.
    #[test]
    fn a_healthy_rate_limit_is_not_relayed() {
        let v: Value = serde_json::from_str(
            r#"{"status":"allowed","resetsAt":1785901800,"rateLimitType":"five_hour",
                "overageStatus":"rejected","isUsingOverage":false}"#,
        )
        .unwrap();
        assert_eq!(rate_limit_notice(&v), None);
    }

    #[test]
    fn an_exhausted_rate_limit_is_relayed_with_its_kind_and_reset() {
        let v: Value = serde_json::from_str(
            r#"{"status":"rejected","resetsAt":1785901800,"rateLimitType":"five_hour"}"#,
        )
        .unwrap();
        assert_eq!(rate_limit_notice(&v), Some(("five_hour".into(), Some(1785901800), "rejected".into())));
    }

    /// A THRESHOLD warning, verbatim off this machine's stream on
    /// 2026-09-06: utilization 0.91 past a 0.75 threshold, `isUsingOverage`
    /// false — the request was allowed. It is carried (with its status, which
    /// is what keeps the renderer and the seat logic from mistaking it for a
    /// refusal), never dropped: only `allowed` proper is a non-event here.
    #[test]
    fn a_threshold_warning_keeps_its_status() {
        let v: Value = serde_json::from_str(
            r#"{"status":"allowed_warning","resetsAt":1786712400,"rateLimitType":"seven_day",
                "utilization":0.91,"isUsingOverage":false,"surpassedThreshold":0.75}"#,
        )
        .unwrap();
        assert_eq!(
            rate_limit_notice(&v),
            Some(("seven_day".into(), Some(1786712400), "allowed_warning".into()))
        );
        // …and it must NOT be treated as the seat refusing the turn.
        assert_eq!(limit_hit(None, Some("some unrelated error")), None);
    }

    /// An unfamiliar shape must not be silently swallowed: anything that isn't
    /// explicitly "allowed" is worth telling the user about.
    #[test]
    fn an_unrecognized_rate_limit_status_is_still_relayed() {
        let v: Value = serde_json::from_str(r#"{"status":"something_new"}"#).unwrap();
        assert_eq!(rate_limit_notice(&v), Some(("usage".into(), None, "something_new".into())));
    }

    /// The wall is only a wall when the turn ENDED on it: a refusal the SDK
    /// recovered from (the turn finished clean) must not send the caller off
    /// to re-run a finished turn on another account.
    #[test]
    fn a_limit_only_counts_when_the_turn_ended_on_an_error() {
        let seen = Some(super::super::LimitHit { kind: "five_hour".into(), resets_at: Some(1) });
        assert_eq!(limit_hit(seen.clone(), None), None, "clean end: no wall");
        assert_eq!(limit_hit(seen.clone(), Some("anything")), seen, "the event is the evidence");
        // No event, but the reason says it in words (both field shapes).
        let by_text = limit_hit(None, Some("claude exited unsuccessfully: Claude usage limit reached|1785900000")).unwrap();
        assert_eq!(by_text.resets_at, Some(1785900000));
        assert!(limit_hit(None, Some("You've hit your session limit · resets 6:30pm (Asia/Shanghai)")).is_some());
        assert_eq!(limit_hit(None, Some("No conversation found with session ID: 29cbfee1")), None);
    }

    /// The `/api/oauth/usage` windows become the seat's limits, most-occupied
    /// first, labelled the way `/stats` labels them.
    #[test]
    fn seat_limit_labels_follow_the_stats_card() {
        let scoped: Value = serde_json::json!({ "scope": { "model": { "display_name": "Fable" } } });
        assert_eq!(seat_limit_label("session", &Value::Null), "Session");
        assert_eq!(seat_limit_label("weekly_all", &Value::Null), "Week (all models)");
        assert_eq!(seat_limit_label("weekly_scoped", &scoped), "Week (Fable)");
        assert_eq!(seat_limit_label("some_new_window", &Value::Null), "Some new window");
    }

    /// A real `initialize` answer (2.1.272, trimmed) becomes the roster, tiers
    /// and all — including the model that has NO tiers, which is an answer and
    /// not a gap: Haiku takes no `--effort`, so the sheet must offer it none.
    #[test]
    fn the_handshake_answer_becomes_the_roster() {
        let resp: Value = serde_json::json!({
            "account": { "email": "ops@example.com", "subscriptionType": "Claude Max" },
            "models": [
                { "value": "opus[1m]", "resolvedModel": "claude-opus-5[1m]",
                  "displayName": "Opus (1M context)", "description": "Best for everyday",
                  "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"] },
                { "value": "haiku", "resolvedModel": "claude-haiku-4-5-20251001",
                  "displayName": "Haiku", "description": "Fastest for quick answers" },
                { "displayName": "a model with no value at all" }
            ]
        });
        let models = models_of(&resp);
        assert_eq!(models.len(), 2, "a row with no flag value names nothing and is dropped");
        assert_eq!(models[0].id, "opus[1m]");
        assert_eq!(models[0].resolved.as_deref(), Some("claude-opus-5[1m]"));
        assert_eq!(models[0].efforts, ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(models[1].display, "Haiku");
        assert!(models[1].efforts.is_empty());
        assert!(models[1].aliases.is_empty(), "an alias here would be a guess — see models_of");
        assert_eq!(account_of(&resp).as_deref(), Some("ops@example.com"));

        // A build that says less is still worth reading: what it did say stands.
        let sparse: Value = serde_json::json!({ "models": [{ "value": "sonnet" }] });
        let m = models_of(&sparse);
        assert_eq!(m[0].display, "sonnet", "no display name → the value is the label");
        assert!(account_of(&sparse).is_none());
    }

    /// The binary's own complaint is the tier list, and the complaint itself is
    /// what makes silence readable.
    #[test]
    fn the_complaint_names_the_valid_tiers() {
        let warn = "Warning: Unknown --effort value 'mafold-probe' — ignoring it and using the \
                    default effort. Valid values: low, medium, high, xhigh, max.\n";
        assert_eq!(valid_efforts(warn).unwrap(), ["low", "medium", "high", "xhigh", "max"]);
        assert!(complains_about(warn, EFFORT_NONSENSE));
        // `ultracode` went in and drew no complaint: this build takes it.
        assert!(!complains_about("", "ultracode"));
        // A build that says nothing about anything proves nothing: with no
        // valid-values line there is no list, and `caps` then claims no mode.
        assert!(valid_efforts("2.1.272 (Claude Code)").is_none());
        // Wording drift must not turn one sentence into five bogus tiers.
        assert!(valid_efforts("Valid values: none at all here").is_none());
    }

    /// The whole probe against the `claude` on THIS machine — the only test
    /// that can prove the questions are still the right questions, because the
    /// answers live in someone else's binary. Ignored by default (it needs an
    /// installed, logged-in CLI and takes ~5s); run it whenever Claude Code
    /// ships a version that might have moved the handshake:
    ///
    ///     cargo test --bin mafold -- --ignored live_claude_caps
    #[tokio::test]
    #[ignore = "requires an installed, logged-in Claude Code CLI"]
    async fn live_claude_caps_probe() {
        let probe = ClaudeCode.caps(&[], "").await.expect("the local claude answered nothing");
        println!("source={:?} account={:?}", probe.source, probe.account);
        for m in &probe.models {
            println!("  {} ({}) efforts={:?}", m.id, m.display, m.efforts);
        }
        println!("  modes={:?}", probe.modes);
        assert_eq!(probe.source, CapsSource::Handshake, "the handshake is the main road");
        assert!(!probe.models.is_empty());
        assert!(
            probe.models.iter().any(|m| !m.efforts.is_empty()),
            "at least one model must report its tiers, or the effort menu has nothing to be built from"
        );
        assert!(probe.efforts.is_empty(), "a handshake attributes every tier to a model");
    }
}
