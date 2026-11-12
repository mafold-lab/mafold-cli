//! The loop: one turn, from the messages a host built to the reply's last
//! frame — both paths, the plain one (no tools: one completion, prose
//! streamed as it arrives) and the tool loop.
//!
//! What a host hands it is the whole of what differs between hosts: the model
//! messages it built from its history, the plugins and tools of the profile,
//! the turn's settled model, where Stop and mid-turn messages come from
//! ([`Live`]) and who settles the bill ([`Settle`]). The reply comes back as
//! [`Frame`]s — appends and rewrites of one transcript, the shape mafold-api's
//! dispatcher pushes as drafts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use mafold_transcript::render::PrivateItem;
use mafold_transcript::{Advance, AgentEvent, RunStats, Tap, Transcript};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::plugin::{HarnessPlugin, ToolSpec, TurnCtx};
use crate::runtime::{
    silence_is_askable, unrunnable_call, ChatEvent, ModelRuntime, TurnMsg, NO_VISIBLE_OUTPUT,
    OUTPUT_CAP_NO_ANSWER, PULSE_EVERY,
};

/// One frame of the reply, as the loop hands it to its host.
///
/// Almost always an APPEND: `text` goes after everything the loop said so
/// far. `rewrite = true` is the one thing an append-only stream cannot say —
/// "the reply now reads like THIS" — said when a step's void half is taken
/// back, when the open tool group's card took another step, and at the end of
/// a turn, to fold the working trail into a `{% mafold/trace %}` and heal the
/// markdown.
#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub text: String,
    pub rewrite: bool,
    /// The turn honoured a Stop: it ended there, and says so (no text).
    pub stopped: bool,
    /// What someone said while the turn ran was just folded into it (`text`
    /// is the seam, `↩︎ …`). Their message is now the newest thing on the
    /// surface and the reply still being written sits ABOVE it, so the host
    /// moves the reply below it — the daemon does the same on
    /// `AgentEvent::Steered`.
    pub steered: bool,
    /// Parts of the reply's cards only some readers may read (a failure's
    /// raw error and amounts), handed over once the reply is final.
    pub private: Vec<PrivateItem>,
    /// Paint this NOW: a tool's call opened, or its result landed. A host
    /// that batches narration behind a timer must not batch this with it —
    /// the daemon pushes every tool event the moment it happens
    /// (`Advance::Immediate`), and cards that wait for the next tick, or for
    /// the model's next step, are what makes a working bot look idle.
    pub immediate: bool,
}

impl Frame {
    pub fn append(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    /// Replace the whole reply so far with `full`.
    pub fn rewrite(full: impl Into<String>) -> Self {
        Self { text: full.into(), rewrite: true, ..Self::default() }
    }

    /// The turn ended on a Stop (no text).
    pub fn stopped() -> Self {
        Self { stopped: true, ..Self::default() }
    }

    /// Append `seam` — the steer just folded in — and move the reply below the
    /// message(s) it came from.
    pub fn steered(seam: impl Into<String>) -> Self {
        Self { text: seam.into(), steered: true, ..Self::default() }
    }

    /// Hand over the private parts of cards in the reply (no text).
    pub fn private(items: Vec<PrivateItem>) -> Self {
        Self { private: items, ..Self::default() }
    }
}

/// A model turn's priced token usage — summed across the turn's rounds, so a
/// metered turn is ONE ledger line for the whole reply (native-harness-v1
/// §7.1).
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Where a running turn hears from the people it runs for: a Stop, and what
/// they said while it worked.
#[async_trait]
pub trait Live: Send + Sync {
    /// Has someone pressed Stop?
    fn is_cancelled(&self) -> bool;

    /// Resolves when someone presses Stop — selected against every pending
    /// provider and tool future, so a Stop does not wait for one to say
    /// something.
    async fn cancelled(&self);

    /// Everything that reached the turn since its last boundary, as the `user`
    /// messages the model reads next — and, for each, the seam drawn into the
    /// visible reply (`None`: nothing to draw — background nobody said to the
    /// agent, or a host that draws the seam itself).
    async fn boundary(&self) -> Vec<(Option<String>, Value)>;
}

/// A turn nobody is attached to — a test, a probe: nothing to steer, nothing
/// to stop.
pub struct Unattended;

#[async_trait]
impl Live for Unattended {
    fn is_cancelled(&self) -> bool {
        false
    }
    async fn cancelled(&self) {
        std::future::pending::<()>().await
    }
    async fn boundary(&self) -> Vec<(Option<String>, Value)> {
        Vec::new()
    }
}

/// Settles a metered turn: called once, last, with the whole turn's usage.
#[async_trait]
pub trait Settle: Send + Sync {
    async fn settle(&self, ctx: &TurnCtx, currency: &str, usage: TokenUsage);
}

/// Who pays for this turn, in what — the currency the host's billing gate
/// allowed — and the prompt's rough size, for the estimate a turn falls back
/// to when the provider reported no usage at all (stream died early).
pub struct Bill {
    pub currency: String,
    pub settle: Arc<dyn Settle>,
    pub prompt_chars: usize,
}

/// Which tools exist THIS turn, and which plugin answers each name.
pub struct Tools {
    pub specs: Vec<ToolSpec>,
    pub route: HashMap<String, usize>,
}

impl Tools {
    /// Ask every plugin. First registration wins; a collision is a wiring bug
    /// worth a log line, not a crash.
    pub async fn of(plugins: &[Arc<dyn HarnessPlugin>], ctx: &TurnCtx, bot: &str) -> Self {
        let mut specs: Vec<ToolSpec> = Vec::new();
        let mut route: HashMap<String, usize> = HashMap::new();
        for (idx, p) in plugins.iter().enumerate() {
            for spec in p.tools(ctx).await {
                if route.contains_key(&spec.name) {
                    tracing::warn!(
                        "harness({}): duplicate tool '{}' from plugin '{}' ignored",
                        bot, spec.name, p.id()
                    );
                    continue;
                }
                route.insert(spec.name.clone(), idx);
                specs.push(spec);
            }
        }
        Self { specs, route }
    }
}

/// The turn's system message: every plugin's sections in profile order, then
/// the host's own notes (who pays is the billing strategy's fact, not a
/// plugin's), blank lines between.
pub fn system_prompt(
    plugins: &[Arc<dyn HarnessPlugin>],
    ctx: &TurnCtx,
    notes: impl IntoIterator<Item = String>,
) -> String {
    let sections: Vec<String> = plugins
        .iter()
        .flat_map(|p| p.prompt_sections(ctx))
        .chain(notes)
        .filter(|s| !s.trim().is_empty())
        .collect();
    sections.join("\n\n")
}

/// Every plugin's per-turn snapshot: instructions go onto `system`, retrieved
/// data comes back to ride ahead of the history as its own messages.
pub async fn prepare(
    plugins: &[Arc<dyn HarnessPlugin>],
    ctx: &TurnCtx,
    system: &mut String,
) -> Result<Vec<String>, String> {
    let mut prepared_data = Vec::new();
    for plugin in plugins {
        let prepared = plugin.prepare(ctx).await?;
        for section in prepared.instructions { system.push_str("\n\n"); system.push_str(&section); }
        prepared_data.extend(prepared.data);
    }
    Ok(prepared_data)
}

/// One turn, everything settled.
pub struct Turn {
    pub ctx: TurnCtx,
    pub runtime: Arc<dyn ModelRuntime>,
    pub plugins: Vec<Arc<dyn HarnessPlugin>>,
    pub tools: Tools,
    /// System, prepared data and history, as the model reads them.
    pub messages: Vec<Value>,
    /// The turn's model — already through the host's billing gate. `None` =
    /// the runtime's own default.
    pub model: Option<String>,
    /// Who may answer if this turn's model is refused for quota.
    pub fallback: Option<String>,
    pub live: Arc<dyn Live>,
    /// The line a Stop leaves at the end of the reply, in the reader's
    /// language.
    pub stopped_line: String,
    /// `None` = nobody is charged for this turn.
    pub bill: Option<Bill>,
    /// A host that draws the reply from the events themselves (mafold-cli's
    /// daemon) hears every one the loop writes, and a broken step's text
    /// taken back as [`AgentEvent::TextRewind`]. `None` = the host reads the
    /// frames (mafold-api).
    pub tap: Option<Tap>,
    /// A host that keeps the conversation itself (mafold-cli's session file)
    /// gets it back once, as the turn leaves it: the messages it was handed,
    /// the steps and tool results that followed, and the answer. `None` = the
    /// host rebuilds history from the chat every turn (mafold-api).
    pub keep: Option<Keep>,
}

/// See [`Turn::keep`].
pub type Keep = Box<dyn FnOnce(Vec<Value>) + Send>;

/// The frame that brings the host's copy of the reply — `sent` — up to
/// `now`, the transcript's latest snapshot; `sent` becomes `now`. The new tail
/// when the reply only grew (narration, a group's first step); the whole reply
/// when something already on screen changed — the open tool group's card took
/// another step, or the finished reply folded. Nothing when they are equal.
fn catch_up(sent: &mut String, now: String) -> Option<Frame> {
    if now == *sent {
        return None;
    }
    let frame = match now.strip_prefix(sent.as_str()) {
        Some(tail) => Frame::append(tail),
        None => Frame::rewrite(now.as_str()),
    };
    *sent = now;
    Some(frame)
}

/// Draw what the model thought in the step under way ([`ChatEvent::Thinking`],
/// held until now) — ONE folded `{% mafold/thinking %}` in front of whatever
/// the step says or does next, the way Claude Code's thinking reads. Nothing
/// readable → nothing drawn: even an empty push closes the narration before
/// it. Says whether it drew.
fn draw_thought(tx: &mut Transcript, thought: &mut String) -> bool {
    let t = std::mem::take(thought);
    if t.trim().is_empty() {
        return false;
    }
    tx.push(&AgentEvent::Thinking(t));
    true
}

pub(crate) fn assistant_value(m: &TurnMsg) -> Value {
    let mut o = json!({ "role": "assistant", "content": m.content });
    if !m.calls.is_empty() {
        o["tool_calls"] = json!(m
            .calls
            .iter()
            .map(|c| {
                let mut call = json!({
                    "id": c.id, "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments }
                });
                if let Some(obj) = call.as_object_mut() {
                    for (k, v) in &c.extra {
                        obj.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                call
            })
            .collect::<Vec<_>>());
    }
    if !m.reasoning.is_empty() {
        o["reasoning_details"] = json!(m.reasoning);
    }
    // Even empty: the key is what DeepSeek checks (`TurnMsg::reasoning_content`).
    if let Some(r) = &m.reasoning_content {
        o["reasoning_content"] = json!(r);
    }
    if let Some(e) = &m.extra_content {
        o["extra_content"] = e.clone();
    }
    o
}

/// Run the turn. A turn with no tools is one completion, its prose streamed
/// as it arrives; otherwise the tool loop. Either way the stream ends on a
/// failure (`Err`, after whatever the turn had already shown) or the finished
/// reply.
pub fn run(turn: Turn) -> BoxStream<'static, Result<Frame, String>> {
    if turn.tools.specs.is_empty() {
        plain(turn)
    } else {
        tool_loop(turn)
    }
}

/// No tools: one completion, prose streamed as it arrives, then the same
/// result details the tool loop stamps.
fn plain(turn: Turn) -> BoxStream<'static, Result<Frame, String>> {
    let Turn { ctx, runtime, messages, model: model_override, fallback, live, stopped_line, bill, tap, keep, .. } = turn;
    let mut messages = messages;
    let mut keep = keep;
    let s = async_stream::stream! {
        // The completion's whole text, for a host that keeps the
        // conversation ([`Turn::keep`]).
        let mut answer = String::new();
        // Chars streamed, so a metered turn whose provider never sent
        // usage still produces a bill instead of a free ride.
        let mut out_chars = 0usize;
        let mut usage = TokenUsage::default();
        let mut tx = Transcript::new();
        if let Some(t) = &tap { tx.set_tap(t.clone()); }
        let mut stats = RunStats {
            run_id: Some(ctx.draft_id.unwrap_or_else(Uuid::new_v4).to_string()),
            model: ctx.model.clone(),
            started_at_ms: Some(mafold_transcript::stats::now_ms()),
            ..Default::default()
        };
        tx.push(&AgentEvent::Stats(stats.clone()));
        // A toolless turn has ONE boundary — before its single model
        // call — so anything the person said between the trigger and
        // now rides along here. What arrives later stays in the
        // mailbox and becomes the follow-up turn (the dispatcher runs
        // the leftovers when this one ends). Never a second parallel
        // reply, never dropped.
        // One move for the whole batch: every message in it is already
        // stored, so a single re-open lands below all of them.
        let mut seamed = false;
        for (seam, content) in live.boundary().await {
            if let Some(seam) = seam {
                tx.push(&AgentEvent::Steered(seam));
                seamed = true;
            }
            messages.push(json!({ "role": "user", "content": content }));
        }
        if seamed { yield Ok(Frame::steered(tx.take_committed())); }
        let mut stopped = false;
        let mut failed: Option<String> = None;
        // The model this turn is on NOW — the settled one, until a
        // quota refusal hands it to the profile's declared stand-in.
        let mut step_model = model_override.clone();
        let mut switched = false;
        // Up to two attempts: a completion that says NOTHING (no text,
        // no failure) is asked again once — with [`NO_VISIBLE_OUTPUT`]
        // in its place, the tool loop's question, not the same request
        // resent: a model that ended its turn silent on a prompt ends it
        // silent on that prompt again. A refusal isn't asked. A second
        // silence fails the turn (below), never a blank bubble. A switch
        // to the stand-in is not an attempt: it got no answer at all.
        let mut attempt = 0;
        let mut stop_reason: Option<String> = None;
        while attempt < 2 {
            if live.is_cancelled() { stopped = true; break; }
            let mut produced = false;
            let mut refused: Option<(String, Option<i64>, String)> = None;
            let mut chat = runtime.step(messages.clone(), None, true, step_model.clone());
            // The reply as it stood before this step said anything —
            // what a [`ChatEvent::Restart`] puts back — and what the step
            // has said since, for a tap to take back.
            let mut before_step: Option<(Transcript, usize)> = None;
            let mut step_text = String::new();
            // What it thought, held until it says something (or the step
            // ends) — it is drawn in front of that, never after.
            let mut thought = String::new();
            loop {
                let ev = tokio::select! {
                    biased;
                    _ = live.cancelled() => { stopped = true; break; }
                    ev = chat.next() => match ev { Some(ev) => ev, None => break },
                };
                match ev {
                    ChatEvent::Thinking(t) => thought.push_str(&t),
                    ChatEvent::Text(t) => {
                        if before_step.is_none() {
                            before_step = Some((tx.clone(), out_chars));
                        }
                        draw_thought(&mut tx, &mut thought);
                        produced |= !t.trim().is_empty();
                        out_chars += t.len();
                        if tap.is_some() { step_text.push_str(&t); }
                        tx.push(&AgentEvent::Text(t));
                        tx.seal();
                        yield Ok(Frame::append(tx.take_committed()));
                    }
                    ChatEvent::Restart => {
                        thought.clear();
                        if let Some((kept, chars)) = before_step.take() {
                            if let Some(t) = &tap { t(&AgentEvent::TextRewind(std::mem::take(&mut step_text))); }
                            tx = kept;
                            out_chars = chars;
                            produced = false;
                            // Every delta so far was committed text, so
                            // what the reader holds without this step's
                            // is exactly the content before it.
                            yield Ok(Frame::rewrite(tx.content()));
                        }
                    }
                    ChatEvent::Pulse => yield Ok(Frame::append(String::new())),
                    ChatEvent::Failed(e) => {
                        failed = Some(e);
                        break;
                    }
                    ChatEvent::QuotaExhausted { kind, resets_at, detail } => {
                        tracing::warn!("harness({}): quota refused on {:?}: {detail}", ctx.bot, step_model);
                        refused = Some((kind, resets_at, detail));
                        break;
                    }
                    ChatEvent::Done(m) => {
                        // Thought, then said nothing: still what it did.
                        draw_thought(&mut tx, &mut thought);
                        produced |= !m.content.trim().is_empty();
                        if keep.is_some() { answer = m.content.clone(); }
                        stop_reason = m.stop_reason.clone();
                        usage.output_tokens += m.tokens;
                        usage.input_tokens += m.input_tokens;
                        stats.add_request(&m.stats);
                    }
                }
            }
            if let Some((kind, resets_at, detail)) = refused {
                let stand_in = if switched { None } else { fallback.clone() };
                match stand_in {
                    Some(to) => {
                        // Someone answers after all: the notice says
                        // who, and when the refused window lifts.
                        tx.push(&AgentEvent::RateLimited {
                            kind,
                            resets_at,
                            status: "rejected".into(),
                            fallback: Some(to.clone()),
                        });
                        tx.seal();
                        let out = tx.take_committed();
                        if !out.is_empty() { yield Ok(Frame::append(out)); }
                        // The result stamp names the model that
                        // actually answered, not the one refused.
                        stats.model = Some(to.clone());
                        step_model = Some(to);
                        switched = true;
                        continue;
                    }
                    // Nothing may stand in: the turn didn't run, and
                    // says so the one way such a turn does — the
                    // dispatcher's failure card, with the window and
                    // when it lifts. Not billed, like any failure.
                    None => {
                        failed = Some(mafold_transcript::failure::quota_refused(&detail, &kind, resets_at));
                        break;
                    }
                }
            }
            if stopped || failed.is_some() || produced {
                break;
            }
            if !silence_is_askable(stop_reason.as_deref()) {
                attempt = 2;
                break;
            }
            tracing::info!(
                "harness({}): empty completion (attempt {attempt}, stop_reason {stop_reason:?}), asking once more",
                ctx.bot
            );
            attempt += 1;
            if attempt < 2 {
                messages.push(json!({ "role": "user", "content": NO_VISIBLE_OUTPUT }));
            }
        }
        // Asked twice, nothing twice: a turn that didn't answer. The
        // dispatcher used to discard the empty draft, so the person who
        // asked got no bubble at all.
        if !stopped && failed.is_none() && attempt >= 2 {
            failed = Some(format!(
                "{} (stop_reason: {})",
                mafold_transcript::failure::empty_reply(step_model.as_deref().unwrap_or("the model")),
                stop_reason.as_deref().unwrap_or("not reported"),
            ));
        }
        if let Some(e) = failed {
            if let Some(k) = keep.take() { k(messages); }
            // A failed stream is not billed — same rule the @claude
            // path has always had.
            yield Err(e);
            return;
        }
        if let Some(k) = keep.take() {
            if !answer.trim().is_empty() {
                messages.push(json!({ "role": "assistant", "content": answer }));
            }
            k(std::mem::take(&mut messages));
        }
        if stopped {
            tx.seal();
            tx.push_raw(&stopped_line);
        } else if out_chars > 0 {
            // A statistics stamp must not turn an empty answer into a
            // non-empty one and suppress the driver's empty-turn
            // handling.
            tx.push(&AgentEvent::Stats(stats));
            tx.push(&AgentEvent::Done { duration_ms: None, cost_usd: None, tokens: None });
        }
        let out = tx.take_committed();
        if !out.is_empty() { yield Ok(Frame::append(out)); }
        // The finished shape: healed markdown (one odd backtick must
        // not un-render everything after it). Nothing to fold on this
        // path, so the rewrite goes out only when a byte changed.
        let finished = tx.finish_folded();
        if finished != tx.content() { yield Ok(Frame::rewrite(finished)); }
        if stopped { yield Ok(Frame::stopped()); }
        // A stopped turn is settled too: the tokens it burned were
        // real. Only a FAILED stream (above) goes unbilled.
        if let Some(bill) = bill {
            if usage.output_tokens == 0 && usage.input_tokens == 0 {
                usage.output_tokens = (out_chars / 4) as u64;
                usage.input_tokens = (bill.prompt_chars / 4) as u64;
            }
            bill.settle.settle(&ctx, &bill.currency, usage).await;
        }
    };
    Box::pin(s)
}

/// The tool loop: the model's step, the tools it called, again — until it
/// answers without calling one, or someone presses Stop.
fn tool_loop(turn: Turn) -> BoxStream<'static, Result<Frame, String>> {
    let Turn { ctx, runtime, plugins, tools, messages, model: model_override, fallback, live, stopped_line, bill, tap, keep } = turn;
    let mut messages = messages;
    let mut keep = keep;
    let route = tools.route;
    let label = runtime.label();
    let tools_wire: Value = Value::Array(tools.specs.iter().map(|s| s.wire()).collect());

    let s = async_stream::stream! {
        let started = Instant::now();
        // Read the way the daemon reads it: as SNAPSHOTS, the open tool
        // group included (`Transcript::snapshot`). Consecutive calls with
        // no words between them are one group however many rounds they
        // span, and that one card grows step by step on screen — the
        // daemon's "Ran 10 shell commands", not ten "Ran 1 tool" pills.
        // `sent` is what the dispatcher holds; [`catch_up`] says the
        // difference.
        let mut tx = Transcript::new();
        // What a tool writes while it runs, heard as it writes it. `tx` is
        // the tool's until it returns, so without this its call card would
        // reach the person together with its result — a minute late, for a
        // tool that runs for a minute. The host's own tap hears everything,
        // as before.
        let (heard, mut hearing) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let listening = Arc::new(AtomicBool::new(false));
        {
            let (host, listening) = (tap.clone(), listening.clone());
            tx.set_tap(Arc::new(move |ev: &AgentEvent| {
                if let Some(t) = &host { t(ev); }
                if listening.load(Ordering::Relaxed) { let _ = heard.send(ev.clone()); }
            }));
        }
        let mut sent = String::new();
        let mut stats = RunStats {
            run_id: Some(ctx.draft_id.unwrap_or_else(Uuid::new_v4).to_string()),
            model: ctx.model.clone(),
            started_at_ms: Some(mafold_transcript::stats::now_ms()),
            ..Default::default()
        };
        tx.push(&AgentEvent::Stats(stats.clone()));
        let mut states: Vec<Box<dyn std::any::Any + Send>> =
            plugins.iter().map(|p| p.new_turn()).collect();
        let mut tokens_total = 0u64;
        // Summed across rounds so a metered turn is ONE ledger line for the
        // whole reply, not one per model call (native-harness-v1 §7.1).
        let mut input_total = 0u64;
        let mut executed_calls = 0u64;
        let mut last_prose = String::new();
        let mut stopped = false;
        let mut failed: Option<String> = None;
        let mut step_model = model_override.clone();
        let mut switched = false;
        // A silent stop was asked about since the last tool round (once per
        // stretch, like Claude Code).
        let mut asked = false;

        // No step limit, like Claude Code: the turn ends when the model
        // stops calling tools, or when someone presses Stop. There used to
        // be ten rounds, the last one sent with the tools silently taken
        // away "so the turn ends on a sentence" — and on 2026-10-27
        // (conv df712566) @claude, nine rounds into polling a build on the
        // owner's machine, found its tools gone with no word why, thought,
        // and stopped without a word; asked again, still toolless, the
        // same. The person got "It answered with nothing" for a turn that
        // had done the work.
        'rounds: loop {
            if live.is_cancelled() { stopped = true; break; }
            // THE boundary. What the person said while the tools ran
            // lands here — after the work that already happened, before
            // the model's next step — which is exactly where the daemon's
            // PostToolUse hook delivers a correction. Shown in the
            // transcript as a seam (`↩︎ …`), so the reply reads as
            // corrected rather than as a model changing its mind on its
            // own; and appended to the request as the person's words, so
            // the model actually reads it. The seam's delta also tells the
            // dispatcher to move the reply below what they said.
            let mut seamed = false;
            for (seam, content) in live.boundary().await {
                if let Some(seam) = seam {
                    tx.push(&AgentEvent::Steered(seam));
                    seamed = true;
                }
                messages.push(json!({ "role": "user", "content": content }));
            }
            if seamed {
                // Always one frame that says so, even with nothing new to
                // draw: it is what moves the reply below their message.
                let mut d = catch_up(&mut sent, tx.snapshot()).unwrap_or_else(|| Frame::append(String::new()));
                d.steered = true;
                yield Ok(d);
            }
            // Stop is also selected against pending model/tool futures,
            // so it does not depend on another provider chunk arriving.
            if live.is_cancelled() {
                stopped = true;
                break;
            }
            // One step — asked again, in the SAME round, only when a quota
            // refusal hands the turn to the declared stand-in. Every later
            // round then starts on the stand-in (`step_model`): asking the
            // refused model again each round would stamp a notice per round
            // and pay for a refused request every time.
            let turn = loop {
                let mut chat = runtime.step(messages.clone(), Some(tools_wire.clone()), true, step_model.clone());
                let mut turn: Option<TurnMsg> = None;
                let mut refused: Option<(String, Option<i64>, String)> = None;
                // The transcript before this step said anything — what a
                // [`ChatEvent::Restart`] puts back. Only text (and the
                // thinking drawn in front of it) moves it mid-step; tool
                // calls land after the step is whole.
                let mut before_step: Option<Transcript> = None;
                // What the step has said since, for a tap to take back.
                let mut step_text = String::new();
                // What it thought, held until it says something or the step
                // is whole — drawn in front of that, never after.
                let mut thought = String::new();
                loop {
                    let ev = tokio::select! {
                        biased;
                        _ = live.cancelled() => { stopped = true; break 'rounds; }
                        ev = chat.next() => match ev { Some(ev) => ev, None => break },
                    };
                    match ev {
                        ChatEvent::Thinking(t) => thought.push_str(&t),
                        ChatEvent::Text(t) => {
                            if before_step.is_none() {
                                before_step = Some(tx.clone());
                            }
                            // A snapshot only when something was committed:
                            // narration lands in batches, and recomputing
                            // the whole reply per token would be the cost
                            // of an O(n²) stream for nothing.
                            let before = tx.content().len();
                            draw_thought(&mut tx, &mut thought);
                            if tap.is_some() { step_text.push_str(&t); }
                            tx.push(&AgentEvent::Text(t));
                            if tx.content().len() != before {
                                if let Some(d) = catch_up(&mut sent, tx.snapshot()) { yield Ok(d); }
                            }
                        }
                        ChatEvent::Restart => {
                            thought.clear();
                            if let Some(kept) = before_step.take() {
                                if let Some(t) = &tap { t(&AgentEvent::TextRewind(std::mem::take(&mut step_text))); }
                                tx = kept;
                                // Shorter than what was sent ⇒ a rewrite.
                                if let Some(d) = catch_up(&mut sent, tx.snapshot()) { yield Ok(d); }
                            }
                        }
                        ChatEvent::Pulse => yield Ok(Frame::append(String::new())),
                        ChatEvent::Failed(e) => {
                            failed = Some(e);
                            break 'rounds;
                        }
                        ChatEvent::QuotaExhausted { kind, resets_at, detail } => {
                            tracing::warn!("harness({}): quota refused on {:?}: {detail}", ctx.bot, step_model);
                            refused = Some((kind, resets_at, detail));
                            break;
                        }
                        ChatEvent::Done(m) => {
                            // On screen now, while the calls it led to run —
                            // they land after it.
                            if draw_thought(&mut tx, &mut thought) {
                                if let Some(d) = catch_up(&mut sent, tx.snapshot()) { yield Ok(d); }
                            }
                            turn = Some(m);
                        }
                    }
                }
                let Some((kind, resets_at, detail)) = refused else { break turn };
                let stand_in = if switched { None } else { fallback.clone() };
                match stand_in {
                    Some(to) => {
                        tx.seal();
                        tx.push(&AgentEvent::RateLimited {
                            kind,
                            resets_at,
                            status: "rejected".into(),
                            fallback: Some(to.clone()),
                        });
                        if let Some(d) = catch_up(&mut sent, tx.snapshot()) { yield Ok(d); }
                        stats.model = Some(to.clone());
                        step_model = Some(to);
                        switched = true;
                    }
                    // Nobody may stand in: the turn fails like any other —
                    // see the simple loop.
                    None => {
                        failed = Some(mafold_transcript::failure::quota_refused(&detail, &kind, resets_at));
                        break 'rounds;
                    }
                }
            };
            let Some(turn) = turn else {
                failed = Some(format!("{label}: stream ended with no message"));
                break;
            };
            stats.add_request(&turn.stats);
            tokens_total += turn.tokens;
            input_total += turn.input_tokens;
            if !turn.content.trim().is_empty() {
                last_prose = turn.content.clone();
            }

            // No tool calls = the model is done talking to the platform
            // and has answered the user.
            if turn.calls.is_empty() {
                let silent = turn.content.trim().is_empty();
                // …unless the cap stopped it before it said anything: that
                // is no answer, and an empty reply is discarded without a
                // word. Fail the turn so the shared note says what happened.
                if turn.truncated && silent {
                    failed = Some(format!(
                        "{label}: {OUTPUT_CAP_NO_ANSWER} — the model spent its whole output cap ({} tokens) without writing an answer",
                        turn.tokens
                    ));
                } else if silent {
                    // …or it ended its turn with nothing for the person to
                    // read. Asked once more, the way Claude Code does: the
                    // silent completion is dropped, not replayed, and the
                    // question takes its place. Silent again — or stopped
                    // by a refusal, which asking won't change — and the
                    // turn fails as the shared empty reply, never a ✓.
                    let why = turn.stop_reason.as_deref();
                    if !asked && silence_is_askable(why) {
                        tracing::info!(
                            "harness({}): no visible output after {executed_calls} tool call(s) (stop_reason {why:?}) — asking once more",
                            ctx.bot
                        );
                        asked = true;
                        messages.push(json!({ "role": "user", "content": NO_VISIBLE_OUTPUT }));
                        continue;
                    }
                    failed = Some(format!(
                        "{} — {} (stop_reason: {})",
                        mafold_transcript::failure::empty_reply(step_model.as_deref().unwrap_or("the model")),
                        if asked { "asked again after a silent stop, silent again" } else { "stopped with nothing to read" },
                        why.unwrap_or("not reported"),
                    ));
                }
                // The answer, for a host that keeps the conversation.
                if keep.is_some() && !silent {
                    messages.push(assistant_value(&turn));
                }
                break;
            }
            messages.push(assistant_value(&turn));

            for call in &turn.calls {
                if live.is_cancelled() { stopped = true; break 'rounds; }
                executed_calls += 1;
                let refusal = unrunnable_call(call, turn.truncated);
                if let Some(why) = &refusal {
                    // On the card too: the user sees the call that did not
                    // run and why, not a phantom "edited 1 file".
                    tx.push(&AgentEvent::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: json!({ "detail": if turn.truncated { "cut off by the output limit — not run" } else { "malformed arguments — not run" } }),
                    });
                    tx.push(&AgentEvent::ToolResult { id: call.id.clone(), text: why.clone() });
                    tx.push(&AgentEvent::ToolStatus { id: call.id.clone(), failed: true });
                }
                let reply = match (refusal, route.get(&call.name)) {
                    (Some(why), _) => why,
                    (None, Some(&idx)) => {
                        // Painted as the tool writes it — the call card the
                        // moment it opens, not when the tool returns — the
                        // way the daemon paints every tool event. A copy
                        // follows along, fed the same events in the same
                        // order, so it reads exactly as `tx` will.
                        let mut following = tx.untapped();
                        listening.store(true, Ordering::Relaxed);
                        let execution = plugins[idx].execute(&ctx, states[idx].as_mut(), &mut tx, call);
                        tokio::pin!(execution);
                        let mut pulse = tokio::time::interval(PULSE_EVERY);
                        pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        let reply = loop {
                            tokio::select! {
                                biased;
                                _ = live.cancelled() => { stopped = true; break 'rounds; }
                                reply = &mut execution => break reply,
                                Some(ev) = hearing.recv() => {
                                    // At once when the transcript says so (a
                                    // call or its result), like the daemon's
                                    // push; anything else may wait for the
                                    // host's next tick.
                                    let now = following.push(&ev) == Advance::Immediate;
                                    if let Some(d) = catch_up(&mut sent, following.snapshot()) {
                                        yield Ok(Frame { immediate: now, ..d });
                                    }
                                }
                                _ = pulse.tick() => yield Ok(Frame::append(String::new())),
                            }
                        };
                        // `tx` has all of it now, whatever the copy missed.
                        listening.store(false, Ordering::Relaxed);
                        while hearing.try_recv().is_ok() {}
                        reply
                    }
                    (None, None) => format!("unknown tool: {}", call.name),
                };
                // A tool result is text — except when it isn't. A plugin
                // that has to hand back something a model SEES (a picture)
                // wraps OpenAI content parts in `{"mafold_content": [...]}`
                // and they become the tool message's content verbatim.
                // Deliberately a shape, not a tool-name list: any plugin
                // may use it and the kernel still knows no tool names
                // (native-harness-v1 ruling 3). Anything else — including
                // a plugin that legitimately returns a JSON array, as the
                // memory search does — stays the string it already was.
                let content = serde_json::from_str::<Value>(&reply)
                    .ok()
                    .and_then(|v| v.get("mafold_content").cloned())
                    .filter(Value::is_array)
                    .unwrap_or(Value::String(reply));
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": content,
                }));
                // The step reaches the person as soon as it is done — into
                // the group still open on screen, which stays open across
                // rounds until the model says something. Sealing it here,
                // every round, is what put "Ran 1 tool" × 9 in a row where
                // the daemon shows one card.
                if let Some(d) = catch_up(&mut sent, tx.snapshot()) { yield Ok(Frame { immediate: true, ..d }); }
            }

            // New tool results are new ground: a silence after them may be
            // asked about again.
            asked = false;
        }
        // A Stop can leave a tool mid-run; the closing acts below are not
        // the tool's to report.
        listening.store(false, Ordering::Relaxed);

        // Nothing said and nothing done: a turn that didn't answer. (The
        // plain path asks twice first; here the first round IS the answer.)
        // Handed back empty, the dispatcher discarded the draft and the
        // person who asked got no bubble at all.
        if !stopped && failed.is_none() && executed_calls == 0 && last_prose.trim().is_empty() && tx.content().trim().is_empty() {
            failed = Some(mafold_transcript::failure::empty_reply(step_model.as_deref().unwrap_or("the model")));
        }
        // Whatever happens next, the conversation as the model left it is
        // final here: the closing acts below write to the reply, not to it.
        if let Some(k) = keep.take() { k(std::mem::take(&mut messages)); }
        if let Some(e) = failed {
            // A provider failure mid-turn must not erase the transcript
            // of what already happened: seal it, fold it, and leave the
            // ONE failure note to the dispatcher — it names the cause and
            // what to do about it, and a second raw copy here was noise.
            if let Some(d) = catch_up(&mut sent, tx.finish_folded()) { yield Ok(d); }
            yield Err(e);
            return;
        }

        // Plugin closing acts (auto-deploy nets, platform-authored
        // confirmation lines) — before the Done stamp so their cards land
        // inside the run, and their LINES in the transcript ahead of the
        // stamp: the stamp is the last thing in a reply, and the fold
        // below must know about everything that precedes it.
        for (idx, p) in plugins.iter().enumerate() {
            // A stopped turn must not enter auto-deploy or other closing
            // side effects. Cancellation cannot undo a write already sent.
            if stopped || live.is_cancelled() { stopped = true; break; }
            let closing = p.finish(&ctx, states[idx].as_mut(), &mut tx, &last_prose);
            let line = tokio::select! {
                biased;
                _ = live.cancelled() => { stopped = true; break; }
                line = closing => line,
            };
            if let Some(line) = line {
                if !line.trim().is_empty() {
                    tx.seal();
                    tx.push_raw(&format!("\n\n{line}"));
                }
            }
        }

        stopped |= live.is_cancelled();
        if stopped {
            tx.seal();
            tx.push_raw(&stopped_line);
        } else if executed_calls > 0 || !last_prose.trim().is_empty() {
            stats.tool_calls = Some(executed_calls);
            tx.push(&AgentEvent::Stats(stats));
            tx.push(&AgentEvent::Done {
                duration_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
                cost_usd: None,
                tokens: None, // the shared stats carry the full input + output total
            });
        } else { tx.seal(); }

        // The finished shape — the daemon's `finish_folded`: everything
        // up to the last tool group under one `{% mafold/trace %}` lid,
        // the answer and its stamp in the open, the markdown healed. A
        // REWRITE when it folds, because a fold is the one thing an append
        // cannot say; the plain tail when the transcript declines to fold
        // (one group, no narration) and the reply only grew.
        if let Some(d) = catch_up(&mut sent, tx.finish_folded()) { yield Ok(d); }
        let private = tx.take_private();
        if !private.is_empty() { yield Ok(Frame::private(private)); }
        if stopped { yield Ok(Frame::stopped()); }

        // Settle LAST: the reply is already in the user's hands, and a
        // ledger write must never be able to swallow it. Reaching here
        // means the turn produced a reply — a failed stream returned
        // above, unbilled. A STOPPED one is billed for the rounds it
        // completed: those tokens were spent.
        if let Some(bill) = bill {
            let usage = TokenUsage {
                input_tokens: input_total,
                output_tokens: tokens_total,
            };
            bill.settle.settle(&ctx, &bill.currency, usage).await;
        }
    };
    Box::pin(s)
}

#[cfg(test)]
mod tap_tests {
    //! A host that draws the reply from the events (mafold-cli's daemon) hears
    //! every one the loop writes — a plugin's own cards included — and the
    //! text of a broken step taken back, exactly.

    use super::*;
    use crate::runtime::ToolCall;
    use std::sync::Mutex;

    /// Each step answers with the next script.
    struct Scripted(Mutex<Vec<Vec<ChatEvent>>>);

    #[async_trait]
    impl ModelRuntime for Scripted {
        fn label(&self) -> &'static str {
            "scripted"
        }
        fn step(&self, _: Vec<Value>, _: Option<Value>, _: bool, _: Option<String>) -> BoxStream<'static, ChatEvent> {
            Box::pin(futures::stream::iter(self.0.lock().unwrap().remove(0)))
        }
    }

    /// One tool, `lookup`, that draws its own card.
    struct Lookup;

    #[async_trait]
    impl HarnessPlugin for Lookup {
        fn id(&self) -> &'static str {
            "lookup"
        }
        async fn tools(&self, _: &TurnCtx) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "lookup".into(),
                description: "Look a word up.".into(),
                parameters: json!({ "type": "object", "properties": {} }),
                read_only: true,
            }]
        }
        async fn execute(
            &self,
            _: &TurnCtx,
            _: &mut (dyn std::any::Any + Send),
            tx: &mut Transcript,
            call: &ToolCall,
        ) -> String {
            tx.push(&AgentEvent::ToolCall { id: call.id.clone(), name: call.name.clone(), input: json!({}) });
            tx.push(&AgentEvent::ToolResult { id: call.id.clone(), text: "alpha means something".into() });
            "alpha means something".into()
        }
    }

    /// `lookup` again, but it opens its card and then works until it is told
    /// to finish.
    struct Slow(Arc<tokio::sync::Notify>);

    #[async_trait]
    impl HarnessPlugin for Slow {
        fn id(&self) -> &'static str {
            "slow"
        }
        async fn tools(&self, ctx: &TurnCtx) -> Vec<ToolSpec> {
            Lookup.tools(ctx).await
        }
        async fn execute(
            &self,
            _: &TurnCtx,
            _: &mut (dyn std::any::Any + Send),
            tx: &mut Transcript,
            call: &ToolCall,
        ) -> String {
            tx.push(&AgentEvent::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                input: json!({ "detail": "looking alpha up" }),
            });
            self.0.notified().await;
            tx.push(&AgentEvent::ToolResult { id: call.id.clone(), text: "alpha means something".into() });
            "alpha means something".into()
        }
    }

    fn done(content: &str, calls: Vec<ToolCall>) -> ChatEvent {
        ChatEvent::Done(TurnMsg { content: content.into(), calls, tokens: 3, ..Default::default() })
    }

    /// Runs `script` and hands back what the tap heard, in order, and the
    /// conversation the turn kept.
    async fn heard_and_kept(
        script: Vec<Vec<ChatEvent>>,
        plugins: Vec<Arc<dyn HarnessPlugin>>,
    ) -> (Vec<AgentEvent>, Vec<Value>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let kept = Arc::new(Mutex::new(None::<Vec<Value>>));
        let kept_into = kept.clone();
        let ctx = TurnCtx { bot: "probe".into(), ..Default::default() };
        let tools = Tools::of(&plugins, &ctx, "probe").await;
        let turn = Turn {
            ctx,
            runtime: Arc::new(Scripted(Mutex::new(script))),
            plugins,
            tools,
            messages: vec![json!({ "role": "user", "content": "hi" })],
            model: None,
            fallback: None,
            live: Arc::new(Unattended),
            stopped_line: String::new(),
            bill: None,
            tap: Some(Arc::new(move |ev: &AgentEvent| sink.lock().unwrap().push(ev.clone()))),
            keep: Some(Box::new(move |m: Vec<Value>| *kept_into.lock().unwrap() = Some(m))),
        };
        let frames: Vec<_> = run(turn).collect().await;
        assert!(frames.iter().all(Result::is_ok), "the turn failed: {frames:?}");
        let heard = events.lock().unwrap().clone();
        let kept = kept.lock().unwrap().take().expect("the turn hands its conversation back");
        (heard, kept)
    }

    async fn heard(script: Vec<Vec<ChatEvent>>, plugins: Vec<Arc<dyn HarnessPlugin>>) -> Vec<AgentEvent> {
        heard_and_kept(script, plugins).await.0
    }

    /// Who said what, in the kept conversation.
    fn roles(kept: &[Value]) -> Vec<String> {
        kept.iter()
            .map(|m| {
                let calls = m["tool_calls"].as_array().map_or(0, Vec::len);
                match m["role"].as_str().unwrap_or("?") {
                    "assistant" if calls > 0 => format!("assistant+{calls} call"),
                    "assistant" => format!("assistant:{}", m["content"].as_str().unwrap_or("")),
                    r => r.to_string(),
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn a_plain_turn_keeps_the_answer_it_gave_not_the_broken_step() {
        let script = vec![vec![
            ChatEvent::Text("Hel".into()),
            ChatEvent::Restart,
            ChatEvent::Text("Hello there.".into()),
            done("Hello there.", vec![]),
        ]];
        let (_, kept) = heard_and_kept(script, vec![]).await;
        assert_eq!(roles(&kept), ["user", "assistant:Hello there."]);
    }

    #[tokio::test]
    async fn the_tool_loop_keeps_its_steps_their_results_and_the_answer() {
        let call = ToolCall { id: "c1".into(), name: "lookup".into(), arguments: "{}".into(), ..Default::default() };
        let script = vec![
            vec![ChatEvent::Text("Let me check.".into()), done("Let me check.", vec![call])],
            vec![ChatEvent::Text("Alpha means something.".into()), done("Alpha means something.", vec![])],
        ];
        let (_, kept) = heard_and_kept(script, vec![Arc::new(Lookup)]).await;
        assert_eq!(roles(&kept), ["user", "assistant+1 call", "tool", "assistant:Alpha means something."]);
        assert_eq!(kept[2]["tool_call_id"], "c1");
        assert_eq!(kept[2]["content"], "alpha means something");
    }

    fn texts(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Text(t) => Some(format!("text:{t}")),
                AgentEvent::TextRewind(t) => Some(format!("rewind:{t}")),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_broken_step_is_taken_back_exactly_on_the_plain_path() {
        let script = vec![vec![
            ChatEvent::Text("Hel".into()),
            ChatEvent::Text("lo th".into()),
            ChatEvent::Restart,
            ChatEvent::Text("Hello there.".into()),
            done("Hello there.", vec![]),
        ]];
        let events = heard(script, vec![]).await;
        assert_eq!(texts(&events), ["text:Hel", "text:lo th", "rewind:Hello th", "text:Hello there."]);
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })), "ends on Done: {events:?}");
    }

    #[tokio::test]
    async fn the_tool_loop_tells_the_tap_its_plugins_cards_and_a_restart() {
        let call = ToolCall { id: "c1".into(), name: "lookup".into(), arguments: "{}".into(), ..Default::default() };
        let script = vec![
            vec![ChatEvent::Text("Let me ".into()), ChatEvent::Restart, ChatEvent::Text("Let me check.".into()), done("Let me check.", vec![call])],
            vec![ChatEvent::Text("Alpha means something.".into()), done("Alpha means something.", vec![])],
        ];
        let events = heard(script, vec![Arc::new(Lookup)]).await;
        assert_eq!(
            texts(&events),
            ["text:Let me ", "rewind:Let me ", "text:Let me check.", "text:Alpha means something."]
        );
        let call_at = events.iter().position(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "c1"));
        let result_at = events.iter().position(|e| matches!(e, AgentEvent::ToolResult { id, .. } if id == "c1"));
        let answer_at = events.iter().position(|e| matches!(e, AgentEvent::Text(t) if t.starts_with("Alpha")));
        assert!(call_at < result_at && result_at < answer_at, "call, result, answer in order: {events:?}");
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })), "ends on Done: {events:?}");
    }

    /// The daemon draws the thinking block from the event, so it hears one
    /// per step — in front of the call that step made, and of the answer.
    #[tokio::test]
    async fn the_tap_hears_each_steps_thinking_in_front_of_what_it_did() {
        let call = ToolCall { id: "c1".into(), name: "lookup".into(), arguments: "{}".into(), ..Default::default() };
        let script = vec![
            vec![ChatEvent::Thinking("Look ".into()), ChatEvent::Thinking("it up.".into()), done("", vec![call])],
            vec![ChatEvent::Thinking("Found it.".into()), ChatEvent::Text("Alpha means something.".into()), done("Alpha means something.", vec![])],
        ];
        let events = heard(script, vec![Arc::new(Lookup)]).await;
        let order: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Thinking(t) => Some(format!("thinking:{t}")),
                AgentEvent::ToolCall { id, .. } => Some(format!("call:{id}")),
                AgentEvent::Text(t) => Some(format!("text:{t}")),
                _ => None,
            })
            .collect();
        assert_eq!(order, ["thinking:Look it up.", "call:c1", "thinking:Found it.", "text:Alpha means something."]);
    }

    /// A host that reads the frames (mafold-api's dispatcher) gets a running
    /// tool's card while the tool still runs, marked to be pushed at once —
    /// and its result the same way when it lands: the daemon's tool-by-tool
    /// painting. 11-03: the daemon's cards appeared tool by tool, @claude's
    /// didn't.
    #[tokio::test]
    async fn a_running_tools_card_goes_out_before_it_returns_and_at_once() {
        let call = ToolCall { id: "c1".into(), name: "lookup".into(), arguments: "{}".into(), ..Default::default() };
        let script = vec![
            vec![ChatEvent::Text("Let me check.".into()), done("Let me check.", vec![call])],
            vec![ChatEvent::Text("Alpha means something.".into()), done("Alpha means something.", vec![])],
        ];
        let finish = Arc::new(tokio::sync::Notify::new());
        let plugins: Vec<Arc<dyn HarnessPlugin>> = vec![Arc::new(Slow(finish.clone()))];
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        let ctx = TurnCtx { bot: "probe".into(), ..Default::default() };
        let tools = Tools::of(&plugins, &ctx, "probe").await;
        let turn = Turn {
            ctx,
            runtime: Arc::new(Scripted(Mutex::new(script))),
            plugins,
            tools,
            messages: vec![json!({ "role": "user", "content": "hi" })],
            model: None,
            fallback: None,
            live: Arc::new(Unattended),
            stopped_line: String::new(),
            bill: None,
            tap: Some(Arc::new(move |ev: &AgentEvent| sink.lock().unwrap().push(ev.clone()))),
            keep: None,
        };
        let apply = |reply: &mut String, f: &Frame| {
            if f.rewrite { *reply = f.text.clone() } else { reply.push_str(&f.text) }
        };
        let mut frames = run(turn);
        let mut reply = String::new();
        // One deadline for the whole wait: the running tool's heartbeat sends
        // an empty frame every quarter second, so a deadline per frame would
        // never run out.
        let card = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let f = frames.next().await.expect("the turn is still going").expect("no failure");
                apply(&mut reply, &f);
                if reply.contains("looking alpha up") {
                    break f;
                }
            }
        })
        .await
        .expect("the card goes out while the tool runs");
        assert!(card.immediate, "the card is pushed at once, not behind the narration throttle");
        assert!(!reply.contains("alpha means something"), "the tool hasn't returned yet: {reply}");
        finish.notify_one();
        let mut landed = None;
        while let Some(f) = frames.next().await {
            let f = f.expect("no failure");
            apply(&mut reply, &f);
            if landed.is_none() && reply.contains("alpha means something") {
                landed = Some(f);
            }
        }
        assert!(landed.expect("the result lands").immediate, "the result is pushed at once too");
        assert!(reply.contains("Alpha means something."), "{reply}");
        // Following the running tool along didn't echo its events: the host's
        // tap heard each of them once.
        let heard = heard.lock().unwrap();
        let calls = heard.iter().filter(|e| matches!(e, AgentEvent::ToolCall { .. })).count();
        let results = heard.iter().filter(|e| matches!(e, AgentEvent::ToolResult { .. })).count();
        assert_eq!((calls, results), (1, 1), "{heard:?}");
    }
}
