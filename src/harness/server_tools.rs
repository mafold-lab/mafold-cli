//! What Mafold Harness on its owner's computer asks of the server for a turn:
//! searching the web and reading a page — the hosted harness's own
//! `web_search` and `open_url` (mafold-api `turn_tools_api`, `turnTools` /
//! `turnToolCall`), billed to the agent's owner (ops 11-20, "Search: Owner's
//! wallet").
//!
//! - **The server's words.** The prompt section and the tool specs are the
//!   server's for this turn, prices included: nothing here says what a search
//!   costs or how to use one, so it can't drift from the hosted turn's.
//! - **Never in the way.** A server that can't say (an older one, no search
//!   set up there, the network) adds nothing to the turn, which goes on as it
//!   would have.
//! - **The owner's circle only**, as the shell is ([`super::computer`]): a
//!   guest's turn is never handed this, so nobody else spends the owner's
//!   wallet ([`super::mafold_harness`]).

use async_trait::async_trait;
use mafold_harness::{HarnessPlugin, ToolCall, ToolSpec, TurnCtx};
use mafold_transcript::{AgentEvent, Transcript};
use serde_json::{json, Value};

use crate::client::Client;

/// The server's part of one turn.
pub struct ServerTools {
    client: Client,
    /// The reply being written, which the server checks is the agent's own.
    draft: String,
    prompt: Vec<String>,
    tools: Vec<ToolSpec>,
}

impl ServerTools {
    /// What the server adds to the turn writing `draft`, asked over the
    /// agent's own token. `None` when it adds nothing, or can't say.
    pub async fn ask(client: &Client, draft: &str) -> Option<Self> {
        let offered = match client.call("turnTools", json!({ "draft": draft })).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("server tools: none for reply {draft} — {}", crate::logfile::one_line(&format!("{e:#}")));
                return None;
            }
        };
        let tools: Vec<ToolSpec> = offered["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                Some(ToolSpec {
                    name: t["name"].as_str()?.to_string(),
                    description: t["description"].as_str().unwrap_or_default().to_string(),
                    parameters: t["parameters"].clone(),
                    read_only: t["read_only"].as_bool().unwrap_or(true),
                })
            })
            .collect();
        if tools.is_empty() {
            return None;
        }
        let prompt = offered["prompt"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect();
        Some(Self { client: client.clone(), draft: draft.to_string(), prompt, tools })
    }
}

#[async_trait]
impl HarnessPlugin for ServerTools {
    fn id(&self) -> &'static str {
        "server-tools"
    }

    fn prompt_sections(&self, _ctx: &TurnCtx) -> Vec<String> {
        self.prompt.clone()
    }

    async fn tools(&self, _ctx: &TurnCtx) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    async fn execute(
        &self,
        _ctx: &TurnCtx,
        _state: &mut (dyn std::any::Any + Send),
        tx: &mut Transcript,
        call: &ToolCall,
    ) -> String {
        let input: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
        tx.push(&AgentEvent::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            input: super::model_tool_input(&call.name, &input),
        });
        let asked = self
            .client
            .call("turnToolCall", json!({ "draft": self.draft, "name": call.name, "arguments": call.arguments }))
            .await;
        let (text, line, failed) = match asked {
            Ok(v) => (
                v["text"].as_str().unwrap_or_default().to_string(),
                v["card"]["result"].as_str().unwrap_or_default().to_string(),
                v["card"]["failed"].as_bool().unwrap_or(false),
            ),
            // Said as the hosted tool says a failure: a line for the model,
            // nothing spent (the server bills a search only once it answered).
            Err(e) => {
                let why = format!("{} couldn't be reached on the server: {}", call.name, crate::logfile::one_line(&format!("{e:#}")));
                (json!({ "error": why }).to_string(), why, true)
            }
        };
        tx.push(&AgentEvent::ToolResult { id: call.id.clone(), text: line });
        tx.push(&AgentEvent::ToolStatus { id: call.id.clone(), failed });
        text
    }
}
