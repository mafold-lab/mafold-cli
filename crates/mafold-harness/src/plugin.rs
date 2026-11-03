//! The plugin surface: everything bot-specific — persona, tools, commands,
//! Customize fields — arrives through [`HarnessPlugin`]. The kernel itself
//! knows no tool names and carries no prompt words (native-harness-v1
//! ruling 3).

use async_trait::async_trait;
use mafold_transcript::Transcript;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::fields::FieldSpec;
use crate::runtime::ToolCall;

// ───────────────────────────── plugin surface ─────────────────────────────

/// One tool a plugin offers the model this turn.
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
    /// Provider hint (MCP `readOnlyHint` semantics): `false` ⇒ the call goes
    /// through the write confirmation path before executing.
    pub read_only: bool,
}

impl ToolSpec {
    /// The OpenAI-compatible `tools` array entry for this spec.
    pub(crate) fn wire(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// Per-turn facts every plugin may need. The SENDER is the authorization
/// boundary — a plugin that reaches a credential resolves it per `user`,
/// never per conversation.
#[derive(Clone, Default)]
pub struct TurnCtx {
    pub trigger_id: Option<Uuid>,
    pub channel_id: Option<Uuid>,
    pub thread_id: Option<Uuid>,
    /// The bot this kernel is answering as (lowercase handle).
    pub bot: String,
    pub conv_id: Option<Uuid>,
    /// The newest message's sender (lowercase) — the requester.
    pub requester: Option<String>,
    /// Who this turn acts for (lowercase): the trigger's sender, or its owner
    /// when the trigger came from another agent — grants and credentials
    /// resolve against this identity.
    pub user: Option<String>,
    /// The trigger's content — the request this turn answers (see
    /// `trigger_index`). For plugins that file the request somewhere (the CTO
    /// report).
    pub request: Option<String>,
    /// The bot's own in-flight draft (the trailing empty placeholder the
    /// dispatcher appends to history — the same message every brain's
    /// "skip empty content" line steps over). Plugins that produce MEDIA
    /// attach to it via `store.attach_to_draft`, exactly like a daemon bot's
    /// `botAttach` — one mechanism, no in-API special case.
    pub draft_id: Option<Uuid>,
    /// The model this turn actually runs on, once resolved (plugin pick ⇒
    /// billing's answer). `None` until the kernel has settled it — plugins
    /// that describe the turn must read it rather than re-deriving, or they
    /// describe a different turn than the one being billed.
    pub model: Option<String>,
    /// This turn was fired by the bot's OWN arrival in the room (a
    /// `member_joined` notice naming it), not by anything anyone said — the
    /// cue for an introduction, the way a self-hosted agent greets a group it
    /// was just added to.
    pub joined: bool,
}

#[derive(Default)]
pub struct Prepared {
    pub instructions: Vec<String>,
    pub data: Vec<String>,
}

/// A capability package: prompt sections, tools, and per-turn hooks.
/// Everything bot-specific lives behind this trait — the kernel itself knows
/// no tool names and carries no prompt words (native-harness-v1 ruling 3).
#[async_trait]
pub trait HarnessPlugin: Send + Sync {
    /// Stable id, used in logs and tool-routing diagnostics.
    fn id(&self) -> &'static str;

    /// Async, per-turn snapshot, after billing and before any provider call.
    /// Ordinary retrieved data is deliberately separate from instructions.
    async fn prepare(&self, _ctx: &TurnCtx) -> Result<Prepared, String> { Ok(Prepared::default()) }

    /// Ordered system-prompt sections. Joined across plugins (profile order)
    /// with blank lines into the turn's system message.
    fn prompt_sections(&self, _ctx: &TurnCtx) -> Vec<String> {
        Vec::new()
    }

    /// Tools this plugin offers for this turn. May depend on ctx (e.g. the
    /// connection source answers per-sender grants).
    async fn tools(&self, _ctx: &TurnCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// Which model this TURN runs on, when the plugin knows better than the
    /// runtime's built-in default (the sender's Customize pick). First `Some`
    /// across plugins wins; `None` everywhere = the runtime default. The
    /// kernel stays ignorant of where the answer comes from — field names and
    /// allowed sets are the plugin's business, same as tool names.
    fn model_override(&self, _ctx: &TurnCtx) -> Option<String> {
        None
    }

    /// Customize-sheet fields this plugin owns. Concatenated across plugins in
    /// profile order (first key wins a collision) into the brain's
    /// `config_schema` (mafold-api `Brain`). Resolved on every READ — which is the
    /// point: an option set derived from live data must not be frozen at boot.
    fn config_fields(&self) -> Vec<FieldSpec> {
        Vec::new()
    }

    /// What the runner behind this plugin's choice says it can run (see
    /// mafold-api `Brain::runner_caps`). The model pick answers it, because the
    /// pick is what the answer constrains.
    fn runner_caps(&self) -> Option<mafold_types::HarnessCaps> {
        None
    }

    /// Fresh per-turn scratch state, handed back to `execute`/`finish`.
    fn new_turn(&self) -> Box<dyn std::any::Any + Send> {
        Box::new(())
    }

    /// Execute one of THIS plugin's tools. `tx` is the live transcript — push
    /// `AgentEvent::ToolCall` / `ToolResult` so the step renders as cards.
    /// The returned string is the `role:"tool"` message the model reads next.
    async fn execute(
        &self,
        _ctx: &TurnCtx,
        _state: &mut (dyn std::any::Any + Send),
        _tx: &mut Transcript,
        call: &ToolCall,
    ) -> String {
        format!("unknown tool: {}", call.name)
    }

    /// Runs once after the loop, before the final flush. May append prose
    /// (platform-authored closing lines — the appbuilder deploy confirmations
    /// are the precedent). `last_prose` is the model's final non-empty prose,
    /// for plugins that audit what the model CLAIMED against what happened.
    async fn finish(
        &self,
        _ctx: &TurnCtx,
        _state: &mut (dyn std::any::Any + Send),
        _tx: &mut Transcript,
        _last_prose: &str,
    ) -> Option<String> {
        None
    }
}

/// The simplest plugin: a fixed persona / instruction section. Every profile
/// mounts at least one of these — the kernel itself has no words.
pub struct PersonaPlugin {
    section: String,
}

impl PersonaPlugin {
    pub fn new(section: impl Into<String>) -> Self {
        Self { section: section.into() }
    }
}

#[async_trait]
impl HarnessPlugin for PersonaPlugin {
    fn id(&self) -> &'static str {
        "persona"
    }
    fn prompt_sections(&self, _ctx: &TurnCtx) -> Vec<String> {
        vec![self.section.clone()]
    }
}
