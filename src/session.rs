//! Human (control-plane) session for mafold-cli. `mafold login` stores the
//! account's `s_…` session token + a stable device id/name in
//! ~/.mafold/session.json. This lets the cli report which coding-agent harnesses
//! are available on THIS machine (→ New-Bot recommendation) and, later,
//! auto-provision bots so the user never pastes a `mafold add --token mb_…`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// `s_…` human session token.
    pub token: String,
    pub username: String,
    /// Stable per-install id (generated once), so the server can tell this
    /// machine apart from the user's other devices.
    pub device_id: String,
    /// Human-readable machine name (hostname).
    pub device_name: String,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}
fn path() -> PathBuf {
    home().join(".mafold/session.json")
}

pub fn load() -> Option<Session> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn save(s: &Session) -> Result<()> {
    std::fs::create_dir_all(home().join(".mafold")).ok();
    std::fs::write(path(), serde_json::to_string_pretty(s)?).context("write session.json")?;
    Ok(())
}

/// `hostname`, best-effort.
pub fn device_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this machine".into())
}

fn device_id_path() -> PathBuf {
    home().join(".mafold/device-id")
}

/// Reuse the existing device id if present, else the one this machine already
/// minted, else mint one. No `uuid` dep — 16 hex chars from hostname + pid +
/// nanos, persisted to `~/.mafold/device-id` the first time.
///
/// Persisted, and not re-derived per call, because a DAEMON needs to name this
/// machine too (`reportHarnessCaps`) and it has no login session to read the id
/// out of. Both doors mint the same id whichever runs first, so one machine
/// never shows up as two.
pub fn device_id(existing: Option<&str>) -> String {
    if let Some(id) = existing {
        if !id.is_empty() {
            return id.to_string();
        }
    }
    if let Some(id) = std::fs::read_to_string(device_id_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return id;
    }
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{}-{}-{}", device_name(), std::process::id(), nanos);
    let h = Sha256::digest(seed.as_bytes());
    let id: String = h[..8].iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(home().join(".mafold")).ok();
    std::fs::write(device_id_path(), &id).ok();
    id
}

/// This machine, for a report that has to name it — `(stable id, hostname)`.
///
/// The id is the one `mafold login` reports in `reportHarnesses`, so a device
/// row and a harness-caps report name the same box: the session's when this
/// machine has one, otherwise the same id a later login will adopt (they share
/// `~/.mafold/device-id`). A bot daemon runs on its own token and must not have
/// to wait for anyone to log in before it can say where it is.
pub fn machine() -> (String, String) {
    match load() {
        Some(s) if !s.device_id.is_empty() => (s.device_id, s.device_name),
        _ => (device_id(None), device_name()),
    }
}
