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
//! - **Its memory, kept on Mafold** (flag `harnessMemory`, for the person
//!   whose memory it is; `.docs/harness-memory-v1.md`): the server hands the
//!   memory plugin a hosted turn mounts, with its rules, index and summary as
//!   this turn's prepared `instructions` and `data`. At the end of the turn
//!   the copy under `~/.mafold/memory/<agent>/<folder>/` takes what changed —
//!   a copy for reading here; the memory itself stays on Mafold, and the
//!   Memory page in the chat menu is where it is changed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use mafold_harness::{HarnessPlugin, Prepared, ToolCall, ToolSpec, TurnCtx};
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
    /// The memory plugin's per-turn snapshot: rules as instructions, the
    /// index and summary as data.
    instructions: Vec<String>,
    data: Vec<String>,
    /// Where this turn's workspace is copied, when the turn has memory.
    memory_folder: Option<String>,
    /// Where the copies live ([`memory_root`]; a test's own).
    copies: PathBuf,
}

impl ServerTools {
    /// What the server adds to the turn writing `draft`, asked over the
    /// agent's own token. `None` when it adds nothing, or can't say.
    pub async fn ask(client: &Client, draft: &str, copies: PathBuf) -> Option<Self> {
        let offered = match client.call("turnTools", json!({ "draft": draft, "memory": true })).await {
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
        let strings = |k: &str| -> Vec<String> {
            offered[k].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect()
        };
        Some(Self {
            client: client.clone(),
            draft: draft.to_string(),
            prompt: strings("prompt"),
            tools,
            instructions: strings("instructions"),
            data: strings("data"),
            memory_folder: offered["memory_folder"].as_str().map(safe_name).filter(|f| !f.is_empty()),
            copies,
        })
    }

    /// Bring the copy of this turn's workspace up to what is on Mafold: the
    /// files whose revision it lacks, and nothing that is gone there.
    async fn refresh_copy(&self, agent: &str, folder: &str) -> anyhow::Result<()> {
        let dir = self.copies.join(safe_name(agent)).join(folder);
        let have = read_manifest(&dir);
        let copy = self.client.call("turnMemoryCopy", json!({ "draft": self.draft, "have": have })).await?;
        write_copy(&dir, &have, &copy)
    }
}

/// Where the copies live: `~/.mafold/memory`.
pub fn memory_root() -> PathBuf {
    crate::platform::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".mafold").join("memory")
}

/// A name every disk can hold: letters, digits, `.`, `-`, `_`.
fn safe_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect::<String>().trim_matches('.').to_string()
}

const MANIFEST: &str = ".mafold-copy.json";

/// The revisions a copy holds, by path.
fn read_manifest(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read(dir.join(MANIFEST)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// A memory path as a path under `dir`, when it is a plain relative one.
fn under(dir: &Path, path: &str) -> Option<PathBuf> {
    let mut out = dir.to_path_buf();
    for seg in path.split('/') {
        if seg.is_empty() || seg.starts_with('.') || seg.contains(['\\', ':']) {
            return None;
        }
        out.push(seg);
    }
    Some(out)
}

/// Apply what `turnMemoryCopy` answered to the copy at `dir`: write each file
/// it sent, delete each the server no longer has, and remember the revisions.
fn write_copy(dir: &Path, have: &BTreeMap<String, String>, copy: &Value) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let root = dir.parent().and_then(Path::parent);
    if let Some(root) = root.filter(|r| !r.join("README.md").exists()) {
        let _ = std::fs::write(
            root.join("README.md"),
            "Copies of what your Mafold Harness agents remember, one folder per agent and per person or room.\n\nThe memory itself is kept on Mafold. Change it on the Memory page in the chat menu: anything edited here is replaced the next time the agent replies.\n",
        );
    }
    let mut kept = have.clone();
    let keep: Vec<&str> = copy["keep"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    for f in copy["files"].as_array().into_iter().flatten() {
        let (Some(path), Some(rev), Some(content)) = (f["path"].as_str(), f["revision"].as_str(), f["content"].as_str()) else { continue };
        let Some(file) = under(dir, path) else { continue };
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = file.with_extension("md.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &file)?;
        kept.insert(path.to_string(), rev.to_string());
    }
    for gone in have.keys().filter(|p| !keep.contains(&p.as_str())).cloned().collect::<Vec<_>>() {
        if let Some(file) = under(dir, &gone) {
            let _ = std::fs::remove_file(file);
        }
        kept.remove(&gone);
    }
    std::fs::write(dir.join(MANIFEST), serde_json::to_vec_pretty(&kept)?)?;
    Ok(())
}

#[async_trait]
impl HarnessPlugin for ServerTools {
    fn id(&self) -> &'static str {
        "server-tools"
    }

    fn prompt_sections(&self, _ctx: &TurnCtx) -> Vec<String> {
        self.prompt.clone()
    }

    async fn prepare(&self, _ctx: &TurnCtx) -> Result<Prepared, String> {
        Ok(Prepared { instructions: self.instructions.clone(), data: self.data.clone() })
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

    /// The copy of this turn's workspace takes what the turn changed. A copy
    /// that can't be refreshed is said in the daemon's log and nowhere else:
    /// the memory itself is on Mafold, and the reply is already written.
    async fn finish(&self, ctx: &TurnCtx, _state: &mut (dyn std::any::Any + Send), _tx: &mut Transcript, _last_prose: &str) -> Option<String> {
        if let Some(folder) = &self.memory_folder {
            if let Err(e) = self.refresh_copy(&ctx.bot, folder).await {
                eprintln!("memory copy: {folder} not refreshed — {}", crate::logfile::one_line(&format!("{e:#}")));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mf-memcopy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A copy takes what it lacks, drops what the server no longer has, and
    /// never writes outside its folder whatever a path says.
    #[test]
    fn a_copy_takes_what_changed_and_drops_what_is_gone() {
        let root = tmp("apply");
        let dir = root.join("opsdu-mh").join("with-ops");
        let first = json!({
            "files": [
                { "path": "notes/paris.md", "revision": "r1", "content": "Paris in March" },
                { "path": "AGENTS.md", "revision": "r2", "content": "- Call me Ops." },
                { "path": "../escape.md", "revision": "r3", "content": "no" },
            ],
            "keep": ["notes/paris.md", "AGENTS.md"],
        });
        write_copy(&dir, &BTreeMap::new(), &first).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("notes/paris.md")).unwrap(), "Paris in March");
        assert!(!root.join("opsdu-mh").join("escape.md").exists(), "nothing outside the folder");
        assert!(root.join("README.md").exists(), "the root says what these are");
        let have = read_manifest(&dir);
        assert_eq!(have.get("AGENTS.md").map(String::as_str), Some("r2"));

        let second = json!({ "files": [], "keep": ["AGENTS.md"] });
        write_copy(&dir, &have, &second).unwrap();
        assert!(!dir.join("notes/paris.md").exists(), "deleted on Mafold, gone here");
        assert_eq!(read_manifest(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_are_ones_every_disk_holds() {
        assert_eq!(safe_name("with-ops"), "with-ops");
        assert_eq!(safe_name("a:b/c"), "a-b-c");
        assert_eq!(safe_name("..x."), "x");
    }
}
