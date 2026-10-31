//! A runtime this build of mafold can't run.
//!
//! An agent's runtime is chosen on the server, and the server takes any id —
//! `openclaw` among them, which this CLI can't drive (`opencode` was one until
//! Mafold Harness, `super::opencode`). Until 10-29 [`super::select`] answered
//! every such id with Claude Code, so an agent created to run OpenCode quietly
//! ran Claude Code instead: on the machine's
//! Claude login, with Claude's models in its Customize sheet, and nothing but
//! one stdout line to say so.
//!
//! This stands in for the runtime instead. It keeps the id it was asked for —
//! so `mafold status`, the computers card and the turn ledger all name the
//! runtime the agent is really set to — and it refuses every turn with a
//! sentence that says what happened. The daemon stays up on purpose: a daemon
//! that exited would be respawned by the supervisor on every tick, and the
//! person talking to the agent would get silence instead of a reason.

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use super::{AgentEvent, CommandOutcome, Harness, Turn, TurnOutcome};
use crate::client::Client;

pub struct Unsupported {
    pub id: &'static str,
}

/// How the refusal names a runtime: its brand where we know one.
pub fn display_name(id: &str) -> String {
    match id {
        "claude-code" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "kimi-code" => "Kimi Code".into(),
        "opencode" => "OpenCode".into(),
        "openclaw" => "OpenClaw".into(),
        other => other.into(),
    }
}

/// What every turn of an agent on an unrunnable runtime answers. The runtimes
/// it offers instead are asked of [`super::select`], not listed here, so the
/// sentence can't go stale when one more lands.
pub fn refusal(id: &str) -> String {
    let runnable: Vec<String> = super::KNOWN
        .iter()
        .filter(|k| super::select(k).runs())
        .map(|k| display_name(k))
        .collect();
    let list = match runnable.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    };
    format!(
        "This agent is set to run {}, which Mafold on this computer can't run, so nothing ran. \
         Mafold runs {list}: make a new agent on one of them.",
        display_name(id)
    )
}

#[async_trait]
impl Harness for Unsupported {
    fn id(&self) -> &'static str {
        self.id
    }

    /// Nothing here can run it, installed or not.
    fn available(&self) -> bool {
        false
    }

    fn runs(&self) -> bool {
        false
    }

    async fn run(&self, _turn: Turn, _sink: UnboundedSender<AgentEvent>) -> anyhow::Result<TurnOutcome> {
        anyhow::bail!(refusal(self.id))
    }

    fn discover(&self, _workdir: &str) -> Value {
        Value::Array(Vec::new())
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
}
