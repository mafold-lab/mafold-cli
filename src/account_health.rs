//! What Mafold last said to this machine's report, per account logged in here
//! — `~/.mafold/account-health.json`.
//!
//! The supervisor reports this machine for every account every 30 seconds
//! (`supervisor::heartbeat`), and that report is what keeps the machine
//! "online" for that account. Until activity center C4 the answer was thrown
//! away: a login Mafold had signed out — expired, or revoked in Settings ▸
//! Active Sessions — got a 401 every 30 seconds forever, the machine read as
//! offline for that person, and nothing on the machine knew why.
//!
//! Now each report's answer lands here: when Mafold last took it, and whether
//! Mafold refused the login outright. `mafold status` reads it back (a
//! separate process, hence a file). A refusal is pinned to the token it was
//! about (by fingerprint — the token itself never lands here), so a fresh
//! `mafold login` reads as signed in at once, and a report that was still in
//! flight on the old token can't make the new one read signed out.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use mafold_core::mafold_types::units::EN;

/// One account's record.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Health {
    /// When Mafold last accepted this account's report, epoch seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok_at: Option<i64>,
    /// Mafold refused the report (401) — the login was signed out. Cleared by
    /// the next accepted report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Refused>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Refused {
    /// The first refusal since the last accepted report, epoch seconds.
    pub at: i64,
    /// [`fingerprint`] of the token that was refused.
    pub token: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Disk {
    #[serde(default)]
    accounts: BTreeMap<String, Health>,
}

/// How one report went.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    /// Mafold took it.
    Accepted,
    /// Mafold answered 401: this login is signed out.
    SignedOut,
    /// Anything else — unreachable, timed out, a 5xx. Says nothing about the
    /// login, so nothing is recorded.
    Failed,
}

impl Outcome {
    /// From the HTTP status the api answered with.
    pub fn of_status(status: u16) -> Outcome {
        match status {
            200..=299 => Outcome::Accepted,
            401 => Outcome::SignedOut,
            _ => Outcome::Failed,
        }
    }

    /// From an RPC call's error (`Client::call`): an `{ok:false}` envelope
    /// with `error_code` 401 is a sign-out; anything else is a failure.
    pub fn of_error(e: &anyhow::Error) -> Outcome {
        let code = e.downcast_ref::<mafold_core::RpcError>().and_then(|re| match re {
            mafold_core::RpcError::Api(env) => serde_json::from_str::<serde_json::Value>(env)
                .ok()
                .and_then(|v| v.get("error_code").and_then(serde_json::Value::as_u64)),
            _ => None,
        });
        if code == Some(401) {
            Outcome::SignedOut
        } else {
            Outcome::Failed
        }
    }
}

/// What a recorded report changed, so the caller says it once instead of
/// every 30 seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    /// Nothing worth a line.
    None,
    /// This login was just signed out (it wasn't, a report ago).
    SignedOut,
    /// It was signed out and is taken again.
    SignedBackIn,
}

pub(crate) fn path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".mafold/account-health.json")
}

/// First 16 hex chars of the token's sha256 — enough to tell two tokens apart,
/// useless for signing in.
pub fn fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn read(path: &Path) -> Disk {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Whole-file replace (write aside, rename over), so the reader in another
/// process never sees half a file.
fn write(path: &Path, d: &Disk) {
    let Ok(json) = serde_json::to_string_pretty(d) else { return };
    let aside = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&aside, json).is_ok() && std::fs::rename(&aside, path).is_err() {
        let _ = std::fs::remove_file(&aside);
    }
}

/// Read, change, write back under one cross-process lock (like the Claude
/// account registry, `accounts::update_registry`): the heartbeat, a
/// post-install report, `mafold report` / `login` / `account rm` all write
/// this file, and without the lock one would put back what another just
/// changed. Best effort — a lock that can't be had still records.
fn update<T>(path: &Path, change: impl FnOnce(&mut Disk) -> (T, bool)) -> T {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // A separate lock inode survives the renames of the file itself.
    let lock = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path.with_extension("lock"));
    if let Ok(lock) = &lock {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        }
        #[cfg(windows)]
        let _ = crate::platform::lock_file_exclusive(lock);
    }
    let mut d = read(path);
    let (out, dirty) = change(&mut d);
    if dirty {
        write(path, &d);
    }
    out // the lock goes with `lock`
}

/// Record how `username`'s report on `token` went.
pub fn record(username: &str, token: &str, outcome: Outcome) -> Change {
    record_at(&path(), username, token, outcome, crate::accounts::now())
}

pub(crate) fn record_at(path: &Path, username: &str, token: &str, outcome: Outcome, now: i64) -> Change {
    if outcome == Outcome::Failed {
        return Change::None;
    }
    let fp = fingerprint(token);
    update(path, |d| {
        let h = d.accounts.entry(username.to_lowercase()).or_default();
        let change = if outcome == Outcome::Accepted {
            h.ok_at = Some(now);
            // On this login or a new one (`mafold login` again): either way
            // the account is signed in here again.
            if h.refused.take().is_some() {
                Change::SignedBackIn
            } else {
                Change::None
            }
        } else if h.refused.as_ref().is_some_and(|r| r.token == fp) {
            // Already said for this token — keep the first time it happened.
            Change::None
        } else {
            h.refused = Some(Refused { at: now, token: fp });
            Change::SignedOut
        };
        (change, true)
    })
}

/// `username`'s record, with a refusal only if it is about `token` — the
/// login this machine holds for them now.
pub fn of(username: &str, token: &str) -> Health {
    of_at(&path(), username, token)
}

pub(crate) fn of_at(path: &Path, username: &str, token: &str) -> Health {
    let mut h = read(path).accounts.remove(&username.to_lowercase()).unwrap_or_default();
    if h.refused.as_ref().is_some_and(|r| r.token != fingerprint(token)) {
        h.refused = None;
    }
    h
}

/// Drop `username`'s record (the account was removed from this machine).
pub fn forget(username: &str) {
    forget_at(&path(), username)
}

pub(crate) fn forget_at(path: &Path, username: &str) {
    update(path, |d| ((), d.accounts.remove(&username.to_lowercase()).is_some()))
}

/// `heartbeat just now` / `✗ signed out by Mafold 3h ago …`, for `mafold status`.
pub fn line(username: &str, h: &Health, now: i64) -> String {
    if let Some(r) = &h.refused {
        return format!(
            "✗ signed out by Mafold {} (sign-in expired or revoked) — this machine reads offline for @{username}; `mafold login` as @{username} signs it back in",
            EN.ago((now - r.at) as f64)
        );
    }
    match h.ok_at {
        Some(at) => format!("heartbeat {}", EN.ago((now - at) as f64)),
        None => "no heartbeat taken yet".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mafold-health-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("account-health.json")
    }

    #[test]
    fn a_refused_report_is_kept_and_the_next_accepted_one_clears_it() {
        let p = tmp("cycle");
        assert_eq!(record_at(&p, "Ops", "tok-a", Outcome::Accepted, 100), Change::None);
        assert_eq!(of_at(&p, "ops", "tok-a"), Health { ok_at: Some(100), refused: None });

        assert_eq!(record_at(&p, "ops", "tok-a", Outcome::SignedOut, 130), Change::SignedOut);
        // Every 30s after that: still signed out, said once, first time kept.
        assert_eq!(record_at(&p, "ops", "tok-a", Outcome::SignedOut, 160), Change::None);
        let h = of_at(&p, "ops", "tok-a");
        assert_eq!(h.ok_at, Some(100), "the last accepted report stays");
        assert_eq!(h.refused.as_ref().map(|r| r.at), Some(130));
        assert!(!std::fs::read_to_string(&p).unwrap().contains("tok-a"), "only a fingerprint lands on disk");

        assert_eq!(record_at(&p, "ops", "tok-a", Outcome::Accepted, 190), Change::SignedBackIn);
        assert_eq!(of_at(&p, "ops", "tok-a"), Health { ok_at: Some(190), refused: None });
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_failure_that_is_not_a_401_says_nothing_about_the_login() {
        let p = tmp("failed");
        record_at(&p, "ops", "tok-a", Outcome::Accepted, 100);
        assert_eq!(record_at(&p, "ops", "tok-a", Outcome::Failed, 130), Change::None);
        assert_eq!(of_at(&p, "ops", "tok-a"), Health { ok_at: Some(100), refused: None });
        record_at(&p, "ops", "tok-a", Outcome::SignedOut, 160);
        assert_eq!(record_at(&p, "ops", "tok-a", Outcome::Failed, 190), Change::None);
        assert!(of_at(&p, "ops", "tok-a").refused.is_some(), "a network blip doesn't clear a sign-out");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_new_login_is_not_signed_out_by_the_old_tokens_refusal() {
        let p = tmp("relogin");
        record_at(&p, "ops", "old", Outcome::SignedOut, 100);
        assert!(of_at(&p, "ops", "old").refused.is_some());
        // `mafold login` swapped the token: signed in at once, before any report.
        assert_eq!(of_at(&p, "ops", "new").refused, None);
        assert_eq!(record_at(&p, "ops", "new", Outcome::Accepted, 130), Change::SignedBackIn);
        // A report that was in flight on the old token lands late: the new
        // login still reads signed in, and its next report clears the mark.
        record_at(&p, "ops", "old", Outcome::SignedOut, 131);
        assert_eq!(of_at(&p, "ops", "new"), Health { ok_at: Some(130), refused: None });
        record_at(&p, "ops", "new", Outcome::Accepted, 160);
        assert_eq!(of_at(&p, "ops", "old"), Health { ok_at: Some(160), refused: None });
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn accounts_are_kept_apart_and_forgotten_one_at_a_time() {
        let p = tmp("two");
        record_at(&p, "ops", "a", Outcome::SignedOut, 100);
        record_at(&p, "fei", "b", Outcome::Accepted, 110);
        assert!(of_at(&p, "ops", "a").refused.is_some());
        assert_eq!(of_at(&p, "fei", "b"), Health { ok_at: Some(110), refused: None });
        forget_at(&p, "OPS");
        assert_eq!(of_at(&p, "ops", "a"), Health::default());
        assert_eq!(of_at(&p, "fei", "b").ok_at, Some(110));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// Writers at once (the heartbeat, a post-install report, `mafold report`
    /// in another process) each read, change and write the whole file; the
    /// lock keeps any of them from putting back what another just changed.
    #[test]
    fn writers_at_once_lose_nothing() {
        let p = tmp("race");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for n in 1..=25 {
                        record_at(&p, &format!("u{i}"), "t", Outcome::Accepted, n);
                    }
                    record_at(&p, &format!("u{i}"), "t", Outcome::SignedOut, 100);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        for i in 0..8 {
            let h = of_at(&p, &format!("u{i}"), "t");
            assert_eq!(h.ok_at, Some(25), "u{i}: {h:?}");
            assert_eq!(h.refused.map(|r| r.at), Some(100), "u{i}");
        }
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_missing_or_broken_file_reads_as_nothing_known() {
        let p = tmp("broken");
        assert_eq!(of_at(&p, "ops", "a"), Health::default());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(of_at(&p, "ops", "a"), Health::default());
        assert_eq!(record_at(&p, "ops", "a", Outcome::Accepted, 5), Change::None);
        assert_eq!(of_at(&p, "ops", "a").ok_at, Some(5), "and is rewritten whole");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn outcomes_from_the_wire() {
        assert_eq!(Outcome::of_status(200), Outcome::Accepted);
        assert_eq!(Outcome::of_status(401), Outcome::SignedOut);
        // A 403 in front of the api (a CDN challenge) is not Mafold signing
        // anyone out.
        assert_eq!(Outcome::of_status(403), Outcome::Failed);
        assert_eq!(Outcome::of_status(503), Outcome::Failed);
        let api = |code: u16| {
            anyhow::Error::new(mafold_core::RpcError::Api(format!(
                r#"{{"ok":false,"error_code":{code},"description":"x"}}"#
            )))
            .context("reportHarnesses failed")
        };
        assert_eq!(Outcome::of_error(&api(401)), Outcome::SignedOut);
        assert_eq!(Outcome::of_error(&api(500)), Outcome::Failed);
        assert_eq!(
            Outcome::of_error(&anyhow::Error::new(mafold_core::RpcError::Connect("refused".into()))),
            Outcome::Failed
        );
    }

    #[test]
    fn status_lines() {
        let now = 10_000;
        assert_eq!(line("ops", &Health { ok_at: Some(now - 12), refused: None }, now), "heartbeat just now");
        assert_eq!(line("ops", &Health::default(), now), "no heartbeat taken yet");
        let out = Health { ok_at: Some(1), refused: Some(Refused { at: now - 3 * 3600, token: "x".into() }) };
        let l = line("ops", &out, now);
        assert!(l.starts_with("✗ signed out by Mafold 3h ago"), "{l}");
        assert!(l.contains("`mafold login` as @ops"), "{l}");
    }
}
