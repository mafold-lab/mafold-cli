//! `mafold pin` / `mafold edit` — the two message primitives an agent needs to
//! keep ONE message as the conversation's record (pin it once, then rewrite it
//! as things change) instead of re-posting state every turn. The server always
//! had them (`pinMessage`, `editMessage`); the agent's CLI did not, so a skill
//! that said "pin the record and edit it later" asked for something the agent
//! could not do.

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::client::Client;

/// The reply being written right now, following the daemon's forwarding
/// pointer when a steered turn re-opened its draft (same rule as `attach`).
fn current_reply() -> Result<String> {
    let env_id = std::env::var("MAFOLD_DRAFT").ok().filter(|s| !s.is_empty()).context(
        "no message given and no reply in flight — run inside an agent turn (the daemon \
         presets MAFOLD_DRAFT), or name the message id",
    )?;
    Ok(std::fs::read_to_string(crate::agent::draft_ptr_path(&env_id))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(env_id))
}

async fn chat_of(client: &Client, chat: Option<&str>) -> Result<String> {
    match chat {
        Some(c) => client.resolve_chat(c).await,
        None => std::env::var("MAFOLD_CONV").ok().filter(|s| !s.is_empty()).context(
            "no --chat given and no conversation in scope (MAFOLD_CONV is set inside an agent turn)",
        ),
    }
}

pub async fn pin(client: &Client, message: Option<&str>, chat: Option<&str>, unpin: bool) -> Result<()> {
    let msg = match message {
        Some(m) => m.to_string(),
        None => current_reply()?,
    };
    let chat_id = chat_of(client, chat).await?;
    let method = if unpin { "unpinMessage" } else { "pinMessage" };
    client
        .call(method, json!({ "chat_id": chat_id, "message_id": msg }))
        .await
        .with_context(|| format!("{method} failed"))?;
    println!("✓ {} {msg}", if unpin { "unpinned" } else { "pinned" });
    Ok(())
}

pub async fn edit(client: &Client, message: &str, text: Option<&str>, file: Option<&str>) -> Result<()> {
    let body = match (text, file) {
        (Some(t), None) if t == "-" => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        }
        (Some(t), None) => t.to_string(),
        (None, Some(f)) => std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?,
        (Some(_), Some(_)) => bail!("give the new text OR --file, not both"),
        (None, None) => bail!("give the new text, --file <path>, or - for stdin"),
    };
    if body.trim().is_empty() {
        bail!("refusing to blank a message — send the full new text");
    }
    client
        .call("editMessage", json!({ "message_id": message, "text": body }))
        .await
        .context("editMessage failed (only messages you sent can be edited)")?;
    println!("✓ edited {message}");
    Ok(())
}
