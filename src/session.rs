//! Human (control-plane) sessions for mafold-cli. `mafold login` stores each
//! account's `s_…` session token in ~/.mafold/session.json. This lets the cli
//! report which coding-agent harnesses are available on THIS machine (→ New-Bot
//! recommendation) and auto-provision bots so the user never pastes a
//! `mafold add --token mb_…`.
//!
//! MANY accounts, one machine. A laptop is not one person's: the owner and a
//! test account, or two people sharing a box, each need their own connections,
//! their own provisions and their own harness report. The server already models
//! it that way — harness reports and provisions upsert on `(account,
//! device_id)` — so the only thing that was single was this file, and a second
//! `mafold login` silently overwrote the first.
//!
//! The DEVICE id stays single and shared, and it always was: it lives in its
//! own `~/.mafold/device-id` so a daemon with no login can still name this box.
//! It identifies the machine, not the person — giving each account its own
//! would tell the server this is two laptops, which is a lie the user reads
//! back in their own device list.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

/// One logged-in account, flattened with the machine it lives on — the shape
/// every caller wants. Unchanged: `load()` still hands back exactly this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// `s_…` human session token.
    pub token: String,
    pub username: String,
    /// Stable per-install id (generated once), so the server can tell this
    /// machine apart from the user's other devices. Shared by every account
    /// on it — see the module note.
    pub device_id: String,
    /// Human-readable machine name (hostname).
    pub device_name: String,
}

/// What is actually on disk.
///
/// The four flat fields ARE the current account, written out of `accounts` on
/// every save. That duplication earns its keep twice over. A binary from before
/// multi-account reads those four keys and ignores what it doesn't know, so
/// `mafold rollback` after a bad update still finds a login instead of quietly
/// becoming logged-out. And it is the whole migration: a file with no
/// `accounts` is one account whose name is already sitting in `username`, so
/// there is no version field and no upgrade step to forget.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Disk {
    token: String,
    username: String,
    device_id: String,
    device_name: String,
    /// Every account logged in here, the current one included.
    #[serde(default)]
    accounts: Vec<Cred>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cred {
    username: String,
    token: String,
}

impl Disk {
    /// An old single-account file has no `accounts`; fold the flat fields in so
    /// the rest of this module never has to know which era it is reading.
    fn normalized(mut self) -> Self {
        if !self.accounts.iter().any(|a| a.username.eq_ignore_ascii_case(&self.username)) {
            self.accounts
                .insert(0, Cred { username: self.username.clone(), token: self.token.clone() });
        }
        self
    }

    fn session(&self, c: &Cred) -> Session {
        Session {
            token: c.token.clone(),
            username: c.username.clone(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
        }
    }

    fn find(&self, username: &str) -> Option<Session> {
        self.accounts
            .iter()
            .find(|a| a.username.eq_ignore_ascii_case(username))
            .map(|c| self.session(c))
    }

    /// Add or refresh an account and make it current. Re-logging in an account
    /// already here replaces its token in place instead of stacking a second
    /// entry under the same name.
    fn upsert(&mut self, s: &Session) {
        self.accounts.retain(|a| !a.username.eq_ignore_ascii_case(&s.username));
        self.accounts.push(Cred { username: s.username.clone(), token: s.token.clone() });
        self.set_current(&s.username, &s.token);
        self.device_id = s.device_id.clone();
        self.device_name = s.device_name.clone();
    }

    fn set_current(&mut self, username: &str, token: &str) {
        self.username = username.to_string();
        self.token = token.to_string();
    }

    /// Drop an account; `false` = there was no such account. Leaving the list
    /// empty is the caller's cue to delete the file rather than keep a login
    /// belonging to nobody.
    fn forget(&mut self, username: &str) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|a| !a.username.eq_ignore_ascii_case(username));
        if self.accounts.len() == before {
            return false;
        }
        // Removing the CURRENT one promotes whoever is left, so the machine is
        // never in the state "logged in as nobody, but with sessions on disk".
        if self.username.eq_ignore_ascii_case(username) {
            if let Some(next) = self.accounts.first().cloned() {
                self.set_current(&next.username, &next.token);
            }
        }
        true
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}
fn path() -> PathBuf {
    home().join(".mafold/session.json")
}

fn read() -> Option<Disk> {
    let d: Disk = std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())?;
    Some(d.normalized())
}

fn write(d: &Disk) -> Result<()> {
    std::fs::create_dir_all(home().join(".mafold")).ok();
    std::fs::write(path(), serde_json::to_string_pretty(d)?).context("write session.json")?;
    Ok(())
}

/// `--account` / `$MAFOLD_ACCOUNT`, resolved once in `main` and read from the
/// handful of places that just want "the session". Process-wide on purpose: a
/// flag only some subcommands honoured would be a flag nobody trusts.
static WANT: OnceLock<String> = OnceLock::new();

/// Point this process at one account. Idempotent; the first call wins.
pub fn select(username: &str) {
    let _ = WANT.set(username.to_lowercase());
}

/// Which account this process is pointed at, if it was told.
pub fn selected() -> Option<String> {
    WANT.get()
        .cloned()
        .or_else(|| std::env::var("MAFOLD_ACCOUNT").ok().filter(|s| !s.trim().is_empty()))
        .map(|s| s.trim().to_lowercase())
}

/// The session this process should act as: `--account` if given, else current.
pub fn load() -> Option<Session> {
    let d = read()?;
    let who = selected().unwrap_or_else(|| d.username.clone());
    d.find(&who)
}

/// A specific account's session, ignoring what this process is pointed at —
/// for the caller that already knows whose credentials it needs (a bot daemon
/// publishing config as its OWNER, say).
pub fn load_named(username: &str) -> Option<Session> {
    read()?.find(username)
}

/// Every account logged in on this machine, current one first.
pub fn all() -> Vec<Session> {
    let Some(d) = read() else { return Vec::new() };
    let mut out: Vec<Session> = d.accounts.iter().map(|c| d.session(c)).collect();
    out.sort_by_key(|s| !s.username.eq_ignore_ascii_case(&d.username));
    out
}

/// The account new commands act as unless told otherwise.
pub fn current_username() -> Option<String> {
    read().map(|d| d.username)
}

/// Add or refresh an account AND make it current — what a fresh `login` means.
pub fn save(s: &Session) -> Result<()> {
    let mut d = read().unwrap_or(Disk {
        token: s.token.clone(),
        username: s.username.clone(),
        device_id: s.device_id.clone(),
        device_name: s.device_name.clone(),
        accounts: Vec::new(),
    });
    d.upsert(s);
    write(&d)
}

/// Switch which account is current. Errors rather than logging in for you:
/// "use an account you haven't added" is a typo far more often than a wish.
pub fn use_account(username: &str) -> Result<Session> {
    let mut d = read().context("not logged in — run `mafold login` first")?;
    let found = d.find(username).with_context(|| {
        format!("no account @{username} on this machine — `mafold account` lists them")
    })?;
    d.set_current(&found.username, &found.token);
    write(&d)?;
    Ok(found)
}

/// Forget one account's session here. The account itself is untouched; the
/// machine-wide `~/.mafold/device-id` is too, so logging back in keeps this
/// box's identity instead of appearing as a new device.
pub fn remove(username: &str) -> Result<bool> {
    let Some(mut d) = read() else { return Ok(false) };
    if !d.forget(username) {
        return Ok(false);
    }
    if d.accounts.is_empty() {
        std::fs::remove_file(path()).context("remove session.json")?;
    } else {
        write(&d)?;
    }
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(json: &str) -> Disk {
        serde_json::from_str::<Disk>(json).unwrap().normalized()
    }

    fn sess(u: &str, t: &str) -> Session {
        Session {
            token: t.into(),
            username: u.into(),
            device_id: "d1".into(),
            device_name: "mac".into(),
        }
    }

    /// The pre-multi-account file must read as one account, with no migration
    /// step anyone has to remember to run.
    #[test]
    fn old_single_account_file_reads_as_one_account() {
        let d = disk(r#"{"token":"s_a","username":"opsdu","device_id":"d1","device_name":"mac"}"#);
        assert_eq!(d.accounts.len(), 1);
        let s = d.find("OPSDU").expect("case-insensitive lookup");
        assert_eq!(s.token, "s_a");
        assert_eq!(s.device_id, "d1");
    }

    /// The flat pair is the CURRENT account, already present in `accounts` —
    /// normalizing must not invent a duplicate for it.
    #[test]
    fn flat_fields_do_not_become_a_phantom_entry() {
        let d = disk(
            r#"{"token":"s_a","username":"a","device_id":"d1","device_name":"mac",
                "accounts":[{"username":"a","token":"s_a"},{"username":"b","token":"s_b"}]}"#,
        );
        assert_eq!(d.accounts.len(), 2);
        assert_eq!(d.find("b").unwrap().token, "s_b");
        assert!(d.find("nobody").is_none());
    }

    /// Every account on one machine shares the device id — the server upserts
    /// harness reports on (account, device_id), so two ids would read as two
    /// laptops in the user's own device list.
    #[test]
    fn accounts_share_one_device_id() {
        let d = disk(
            r#"{"token":"s_a","username":"a","device_id":"d1","device_name":"mac",
                "accounts":[{"username":"a","token":"s_a"},{"username":"b","token":"s_b"}]}"#,
        );
        assert_eq!(d.find("a").unwrap().device_id, d.find("b").unwrap().device_id);
    }

    /// A second `mafold login` used to overwrite the first in silence. It must
    /// stack — and the newcomer becomes current, because that is what someone
    /// who just typed their password expects.
    #[test]
    fn second_login_stacks_instead_of_clobbering() {
        let mut d = disk(r#"{"token":"s_a","username":"a","device_id":"d1","device_name":"mac"}"#);
        d.upsert(&sess("b", "s_b"));
        assert_eq!(d.accounts.len(), 2);
        assert_eq!(d.username, "b", "the fresh login is current");
        assert_eq!(d.find("a").unwrap().token, "s_a", "the first login survives");
    }

    /// Re-logging in the SAME account is a token refresh, not a duplicate row —
    /// otherwise the list grows by one every time a session expires.
    #[test]
    fn relogin_replaces_the_token_in_place() {
        let mut d = disk(r#"{"token":"s_old","username":"a","device_id":"d1","device_name":"mac"}"#);
        d.upsert(&sess("A", "s_new"));
        assert_eq!(d.accounts.len(), 1, "case-insensitive: @A is @a");
        assert_eq!(d.find("a").unwrap().token, "s_new");
    }

    /// Forgetting the current account must promote someone, never leave the
    /// file pointing at a name that is no longer in it.
    #[test]
    fn forgetting_the_current_account_promotes_the_next() {
        let mut d = disk(
            r#"{"token":"s_b","username":"b","device_id":"d1","device_name":"mac",
                "accounts":[{"username":"a","token":"s_a"},{"username":"b","token":"s_b"}]}"#,
        );
        assert!(d.forget("b"));
        assert_eq!(d.username, "a");
        assert_eq!(d.token, "s_a", "the flat token follows the promotion");
        assert!(d.find("b").is_none());
    }

    #[test]
    fn forgetting_another_account_leaves_current_alone() {
        let mut d = disk(
            r#"{"token":"s_b","username":"b","device_id":"d1","device_name":"mac",
                "accounts":[{"username":"a","token":"s_a"},{"username":"b","token":"s_b"}]}"#,
        );
        assert!(d.forget("a"));
        assert_eq!(d.username, "b");
        assert!(!d.forget("nobody"), "unknown name is not a removal");
    }

    #[test]
    fn forgetting_the_last_account_empties_the_list() {
        let mut d = disk(r#"{"token":"s_a","username":"a","device_id":"d1","device_name":"mac"}"#);
        assert!(d.forget("a"));
        assert!(d.accounts.is_empty());
    }

    /// Round-trip through serde: what a new binary writes, an OLD binary reads
    /// as a plain single session — that is what makes `mafold rollback` safe.
    #[test]
    fn written_file_is_still_readable_as_the_old_flat_shape() {
        let mut d = disk(r#"{"token":"s_a","username":"a","device_id":"d1","device_name":"mac"}"#);
        d.upsert(&sess("b", "s_b"));
        let json = serde_json::to_string(&d).unwrap();
        let old: Session = serde_json::from_str(&json).expect("old binary parses it");
        assert_eq!(old.username, "b");
        assert_eq!(old.token, "s_b");
        assert_eq!(old.device_id, "d1");
    }

    #[test]
    fn device_id_is_reused_when_one_already_exists() {
        assert_eq!(device_id(Some("keepme")), "keepme");
    }
}
