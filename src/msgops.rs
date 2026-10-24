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
async fn current_reply() -> Result<String> {
    let env_id = std::env::var("MAFOLD_DRAFT").ok().filter(|s| !s.is_empty()).context(
        "no message given and no reply in flight — run inside an agent turn (the daemon \
         presets MAFOLD_DRAFT), or name the message id",
    )?;
    crate::agent::current_draft(env_id).await
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
        None => current_reply().await?,
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
    let body = body_from(text, file, std::io::stdin())?;
    client
        .call("editMessage", json!({ "message_id": message, "text": body }))
        .await
        .context("editMessage failed (only messages you sent can be edited)")?;
    println!("✓ edited {message} · {}", receipt(&body));
    Ok(())
}

/// The text of a message, from what the command was given: the text itself,
/// `-` to read it from stdin, or `--file <path>`. `mafold send` and
/// `mafold edit` both read theirs here, so the two can't disagree about what
/// `-` means again: `send` used to post it as written, and an agent piping its
/// text in the way `edit` documents put six bubbles reading just "-" into a
/// channel, each acknowledged "✓ sent" (2026-10-19/20, linsky's Rei).
///
/// The newline a pipe or a file ends on is not part of the message. Nothing
/// but whitespace is refused here, before anything reaches the network.
pub(crate) fn body_from(text: Option<&str>, file: Option<&str>, mut stdin: impl std::io::Read) -> Result<String> {
    let (body, empty) = match (text, file) {
        (Some("-"), None) => {
            let mut s = String::new();
            stdin.read_to_string(&mut s).context("reading the text from stdin")?;
            (s.trim_end_matches(['\n', '\r']).to_string(), "`-` reads the text from stdin, and nothing came in")
        }
        (Some(t), None) => (t.to_string(), "the text is blank"),
        (None, Some(f)) => {
            let s = std::fs::read_to_string(f).with_context(|| format!("reading {f}"))?;
            (s.trim_end_matches(['\n', '\r']).to_string(), "the file is blank")
        }
        (Some(_), Some(_)) => bail!("give the text OR --file, not both"),
        (None, None) => bail!("give the text, --file <path>, or - for stdin"),
    };
    if body.trim().is_empty() {
        bail!("refusing to send an empty message — {empty}");
    }
    Ok(body)
}

/// How much of a text a receipt quotes.
const RECEIPT_HEAD: usize = 40;

/// What a receipt says about the text that went out: how long it is and how it
/// starts, on one line. Naming only where it went is how six lone "-" passed
/// as sent (see [`body_from`]).
pub(crate) fn receipt(text: &str) -> String {
    let n = text.chars().count();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut head: String = flat.chars().take(RECEIPT_HEAD).collect();
    if flat.chars().count() > RECEIPT_HEAD {
        head.push('…');
    }
    format!("{n} char{}: “{head}”", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receipt that only named where a message went is how six lone "-"
    /// went unnoticed (2026-10-19/20): it has to say what went out.
    #[test]
    fn the_receipt_says_what_went_out() {
        assert_eq!(receipt("-"), "1 char: “-”");
        assert_eq!(receipt("好了"), "2 chars: “好了”");
        let long = format!("617.42 的安装脚本好了，现在可以开始：\n1. Win+R → 输入 cmd\n\n{}", "长".repeat(80));
        let r = receipt(&long);
        assert!(r.starts_with(&format!("{} chars: “617.42 的安装脚本好了，现在可以开始： 1. Win+R", long.chars().count())), "{r}");
        assert!(r.ends_with("…”"), "a long text is cut, and says so: {r}");
        assert!(!r.contains('\n'), "one line, whatever the text: {r}");
    }
}
