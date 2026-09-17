//! The per-TURN values a harness child needs, in a file it can re-read.
//!
//! A process's environment cannot be rewritten after it is spawned. That was
//! already a problem for one value — a steer re-opens the reply in a new draft,
//! so `mafold attach` follows [`crate::agent::draft_ptr_path`] — and a connection
//! that serves MANY turns makes it true of three: the draft id, the ask file and
//! the steer file all change from one turn to the next while the child's env
//! still names the first turn's.
//!
//! So the child is handed ONE stable path (`MAFOLD_TURN`) and the harness
//! rewrites what is behind it before each turn. Readers prefer the file and fall
//! back to the old env vars, which keeps a one-shot child, and an older `mafold`
//! binary on `$PATH`, working unchanged.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TurnEnv {
    /// The in-flight reply's message id (`mafold attach` hangs media on it).
    #[serde(default)]
    pub draft: String,
    /// Where the daemon writes the user's answer to an interactive ask.
    #[serde(default)]
    pub ask: String,
    /// Where the daemon appends what the user said mid-turn.
    #[serde(default)]
    pub steer: String,
    /// Where the permission MCP appends a request it needs a human to answer.
    /// Per-turn for the same reason `ask` is, and read through the same door:
    /// the MCP server is a child of the CONNECTION, so its own environment
    /// names whichever turn happened to spawn the process.
    #[serde(default)]
    pub perm: String,
    /// The scoped surface (conversation + forum channel) this turn runs on —
    /// the key a detached background task is filed under. Carried here because
    /// the control-channel hook handler runs inside the daemon, which has no
    /// per-turn `MAFOLD_SURFACE` of its own.
    #[serde(default)]
    pub surface: String,
}

/// The stable path for one connection. Named by the connection, not the turn —
/// that is the whole point.
pub fn path_for(conn_id: &str) -> PathBuf {
    let safe: String = conn_id.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    std::env::temp_dir().join(format!("mafold-turn-{safe}.json"))
}

pub fn write(path: &std::path::Path, t: &TurnEnv) {
    if let Ok(s) = serde_json::to_string(t) {
        let _ = std::fs::write(path, s);
    }
}

pub fn remove(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

fn load() -> Option<TurnEnv> {
    let p = std::env::var("MAFOLD_TURN").ok().filter(|s| !s.is_empty())?;
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// File first, env second. The file is authoritative because it is the only one
/// of the two that can be current.
fn resolve(pick: impl Fn(&TurnEnv) -> &String, env_key: &str) -> Option<String> {
    if let Some(t) = load() {
        let v = pick(&t);
        if !v.is_empty() {
            return Some(v.clone());
        }
    }
    std::env::var(env_key).ok().filter(|s| !s.is_empty())
}

pub fn draft() -> Option<String> {
    resolve(|t| &t.draft, "MAFOLD_DRAFT")
}

pub fn ask_file() -> Option<String> {
    resolve(|t| &t.ask, "MAFOLD_ASK_FILE")
}

pub fn steer_file() -> Option<String> {
    resolve(|t| &t.steer, "MAFOLD_STEER_FILE")
}

pub fn perm_file() -> Option<String> {
    resolve(|t| &t.perm, "MAFOLD_PERM_FILE")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_wins_over_env_and_env_is_the_fallback() {
        let dir = std::env::temp_dir().join(format!("mafold-turnenv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.json");
        write(
            &p,
            &TurnEnv {
                draft: "m_new".into(),
                ask: String::new(),
                steer: "s".into(),
                perm: "/tmp/ask.perm".into(),
                surface: "c1".into(),
            },
        );
        std::env::set_var("MAFOLD_TURN", &p);
        std::env::set_var("MAFOLD_DRAFT", "m_old");
        std::env::set_var("MAFOLD_ASK_FILE", "/tmp/ask-from-env");
        assert_eq!(draft().as_deref(), Some("m_new"), "the file is the current turn");
        assert_eq!(
            ask_file().as_deref(),
            Some("/tmp/ask-from-env"),
            "an empty field falls through to the env"
        );
        assert_eq!(steer_file().as_deref(), Some("s"));
        assert_eq!(perm_file().as_deref(), Some("/tmp/ask.perm"), "the permission mailbox rides here too");
        std::env::remove_var("MAFOLD_TURN");
        assert_eq!(draft().as_deref(), Some("m_old"), "no file → the env still works");
        std::env::remove_var("MAFOLD_DRAFT");
        std::env::remove_var("MAFOLD_ASK_FILE");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
