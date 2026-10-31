//! `mafold dev-wallet-turn` (debug builds only, hidden): one wallet reply,
//! driven through the functions the daemon's turn calls for it and nothing
//! else. The PR 4c billing table (`mafold-api`'s `harness_table_tests`) runs
//! it against the api's real handlers and the real router: the full daemon
//! can't run against that rig (no WebSocket, no draft streaming there), and
//! devapi can't make wallet calls at all.
//!
//! What runs, in `agent::handle`'s order: [`fuel::step`] (the draft's key,
//! into the turn's env — the env starts empty, as a wallet turn has no seat),
//! the OpenCode harness as the daemon selects it (`harness::select`, the
//! pinned build under `~/.mafold/runtimes`), and the renderer's wallet steps
//! ([`fuel::before_done`] / [`fuel::without_done`]) on a transcript built the
//! way `agent::render_loop` builds it — so the result card printed is the one
//! the reply would end on. What doesn't run: the owner-only gate (it reads the
//! trigger's sender off the WebSocket; the CLI's unit tests cover it, and the
//! api refuses the mint for a guest anyway), draft streaming, and the
//! finalize — the caller's to do, as it would be the daemon's.
//!
//! The last stdout line is one JSON object. The key is never in it: it lives
//! in the turn's env and the conceal list only.
//!
//! [`fuel::step`]: crate::fuel::step
//! [`fuel::before_done`]: crate::fuel::before_done
//! [`fuel::without_done`]: crate::fuel::without_done

use std::sync::Arc;
use std::time::{Duration, Instant};

use mafold_transcript::{Advance, RunStats, Transcript};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::client::Client;
use crate::harness::{AgentEvent, Turn, TurnProc};

/// The command's arguments.
pub struct Ask {
    pub draft: String,
    pub workdir: String,
    pub prompt: String,
    pub model: Option<String>,
    /// Stop the turn this long after its first words (text or thinking)
    /// arrive — a person who watched it start answering, then pressed Stop.
    /// Counted from the first words, not the start: how long OpenCode takes
    /// to come up says nothing about the case.
    pub stop_after_ms: Option<u64>,
    /// The agent's handle and the conversation, for the turn's surface
    /// (`{conv}__{channel}__{bot}`): which agent home OpenCode runs in.
    pub bot: Option<String>,
    pub chat: Option<String>,
}

pub async fn run(client: Client, ask: Ask) -> anyhow::Result<()> {
    let out = turn(&client, ask).await;
    println!("{out}");
    Ok(())
}

/// An event's kind, for the order the table shows — never its payload.
fn kind(ev: &AgentEvent) -> String {
    let shown = format!("{ev:?}");
    shown.chars().take_while(|c| c.is_ascii_alphanumeric()).collect()
}

/// `kinds`, consecutive repeats folded (`Text×12`).
fn folded(kinds: &[String]) -> Vec<String> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for k in kinds {
        match out.last_mut() {
            Some((last, n)) if last == k => *n += 1,
            _ => out.push((k.clone(), 1)),
        }
    }
    out.into_iter().map(|(k, n)| if n == 1 { k } else { format!("{k}×{n}") }).collect()
}

/// The `{% mafold/result … %}…{% /mafold/result %}` card in `md`, and the
/// stats its body carries.
fn result_card(md: &str) -> (Option<String>, Value) {
    const OPEN: &str = "{% mafold/result";
    const CLOSE: &str = "{% /mafold/result %}";
    let Some(at) = md.rfind(OPEN) else { return (None, Value::Null) };
    let rest = &md[at..];
    let card = match rest.find(CLOSE) {
        Some(end) => &rest[..end + CLOSE.len()],
        None => rest,
    };
    let body = card
        .split_once("%}")
        .map(|(_, b)| b.trim_end_matches(CLOSE).trim())
        .and_then(|b| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    (Some(card.to_string()), body)
}

async fn turn(client: &Client, ask: Ask) -> Value {
    let started = Instant::now();
    let bot = ask.bot.clone().unwrap_or_default();
    let chat = ask.chat.clone().unwrap_or_default();
    let ms = |t: Instant| t.elapsed().as_millis() as u64;
    // The fuel step, as `agent::handle` takes it once the draft exists. No
    // seat: a wallet turn's env is what the step puts in it.
    let mut env: Vec<(String, String)> = Vec::new();
    let fuel = match crate::fuel::step(client, true, &ask.draft, &mut env).await {
        Ok(f) => f,
        Err(why) => {
            // `handle()` ends such a turn on the error card, nothing started.
            let (card, _) = crate::agent::failure_card(&bot, &why, None);
            return json!({
                "outcome": "refused", "error": why, "error_card": card, "text": "", "stats": Value::Null,
                "result_card": Value::Null, "events": [], "opencode_pid": Value::Null, "ms": ms(started),
            });
        }
    };
    let wallet = fuel.as_ref().map(|f| f.draft.clone());

    let cancel = Arc::new(Notify::new());
    let proc = TurnProc::default();
    let surface = if bot.is_empty() { String::new() } else { crate::agent::surface_tag(&bot, &chat, None) };
    let turn = Turn {
        prompt: ask.prompt.clone(),
        conv: chat.clone(),
        surface,
        draft: ask.draft.clone(),
        workdir: ask.workdir.clone(),
        session: None,
        model: ask.model.clone(),
        effort: None,
        thinking: None,
        cancel: cancel.clone(),
        system: None,
        ask_file: None,
        steer_file: None,
        env,
        mount: Default::default(),
        skill_plugins: None,
        proc: proc.clone(),
    };

    // The process serving the turn, seen at least once — whether one started.
    let watching = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let watcher = {
        let (proc, watching) = (proc.clone(), watching.clone());
        tokio::spawn(async move {
            let mut seen = None;
            while watching.load(std::sync::atomic::Ordering::SeqCst) {
                seen = seen.or(proc.pid());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            seen.or(proc.pid())
        })
    };

    // The renderer: `render_loop`'s transcript and its wallet steps, minus
    // the draft streaming.
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let renderer = {
        let client = client.clone();
        let cancel = cancel.clone();
        let draft = ask.draft.clone();
        let stop_after = ask.stop_after_ms.map(Duration::from_millis);
        tokio::spawn(async move {
            let mut tx = Transcript::with_text_policy(crate::cardtags::commit_boundary, crate::cardtags::qualify);
            tx.push(&AgentEvent::Stats(RunStats {
                run_id: Some(draft),
                started_at_ms: Some(mafold_transcript::stats::now_ms()),
                ..Default::default()
            }));
            let mut kinds: Vec<String> = Vec::new();
            let mut text = String::new();
            let mut stop_at: Option<u64> = None;
            let mut done = false;
            while let Some(ev) = ev_rx.recv().await {
                if let (Some(after), None) = (stop_after, stop_at) {
                    if matches!(&ev, AgentEvent::Text(t) | AgentEvent::Thinking(t) if !t.is_empty()) {
                        stop_at = Some(ms(started) + after.as_millis() as u64);
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(after).await;
                            cancel.notify_one();
                        });
                    }
                }
                if let AgentEvent::Text(t) = &ev {
                    text.push_str(t);
                }
                if let (AgentEvent::Done { .. }, Some(draft)) = (&ev, &wallet) {
                    let bill = crate::fuel::before_done(&client, draft).await;
                    kinds.push("Stats(wallet)".into());
                    tx.push(&bill);
                }
                kinds.push(kind(&ev));
                if tx.push(&ev) == Advance::Done {
                    done = true;
                    break;
                }
            }
            if !done {
                if let Some(draft) = &wallet {
                    for ev in crate::fuel::without_done(&client, draft).await {
                        kinds.push(if matches!(ev, AgentEvent::Stats(_)) { "Stats(wallet)".into() } else { kind(&ev) });
                        tx.push(&ev);
                    }
                }
            }
            (tx.finish_folded(), kinds, text, stop_at)
        })
    };

    let result = crate::harness::select("opencode").run(turn, ev_tx).await;
    let harness_ms = ms(started);
    watching.store(false, std::sync::atomic::Ordering::SeqCst);
    let pid = watcher.await.ok().flatten();
    let (md, kinds, text, stop_at) = renderer.await.unwrap_or_default();
    let (card, stats) = result_card(&md);
    let (outcome, error, session, produced) = match &result {
        Ok(o) if o.stopped => ("stopped", None, o.session.clone(), o.produced),
        Ok(o) if o.error.is_some() => ("error", o.error.clone(), o.session.clone(), o.produced),
        Ok(o) if !o.produced => ("empty", None, o.session.clone(), false),
        Ok(o) => ("done", None, o.session.clone(), true),
        Err(e) => ("error", Some(format!("{e:#}")), None, false),
    };
    let limit = result.as_ref().ok().and_then(|o| o.limit.clone());
    let error_card = error.as_deref().map(|e| crate::agent::failure_card(&bot, e, limit.as_ref()).0);
    // Held to here: the key stays masked for as long as the turn had it.
    drop(fuel);
    json!({
        "outcome": outcome,
        "error": error,
        "error_card": error_card,
        "text": text,
        "produced": produced,
        "session": session,
        "stats": stats,
        "result_card": card,
        "events": folded(&kinds),
        "opencode_pid": pid,
        "stop_sent_at_ms": stop_at,
        "harness_ms": harness_ms,
        "ms": ms(started),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_and_its_stats_come_out_of_the_reply() {
        let md = "said\n\n{% mafold/result duration=\"1s\" %}{\"wallet\":[{\"currency\":\"deepseek-v4-flash\",\"charged\":12}]}{% /mafold/result %}";
        let (card, stats) = result_card(md);
        assert!(card.unwrap().starts_with("{% mafold/result duration"));
        assert_eq!(stats["wallet"][0]["charged"], 12);
        assert_eq!(result_card("no card"), (None, Value::Null));
    }

    #[test]
    fn kinds_fold_and_say_no_payload() {
        let evs = [AgentEvent::Text("a".into()), AgentEvent::Text("secret".into()), AgentEvent::Session("s".into())];
        let kinds: Vec<String> = evs.iter().map(kind).collect();
        assert_eq!(folded(&kinds), ["Text×2", "Session"]);
    }
}
