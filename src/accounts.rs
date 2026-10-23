//! Claude accounts on this machine — several logins behind ONE `~/.claude`.
//!
//! Claude Code keys its OAuth credential by a single directory: with
//! `CLAUDE_SECURESTORAGE_CONFIG_DIR=<dir>` in a process's environment it reads
//! and writes the login that belongs to `<dir>` — a macOS Keychain item named
//! `Claude Code-credentials-<sha256(dir)[..8]>`, `<dir>/.credentials.json`
//! elsewhere — and touches nothing else. Skills, settings, plugins,
//! `~/.claude.json`, the session transcripts under `~/.claude/projects/` and
//! the auto-memory that lives beside them all stay `~/.claude`'s. That is
//! exactly the split a bot wants: N subscriptions behind one identity, one
//! memory, one set of resumable sessions. (`CLAUDE_CONFIG_DIR` is the OTHER
//! switch, and the wrong one — it isolates all of that too, so a bot would
//! forget everything the moment it changed accounts.)
//!
//! One account = one directory under `~/.mafold/claude-accounts/<name>/`;
//! `default` is the login `~/.claude` already holds and carries no env at all.
//! The registry (`~/.mafold/claude-accounts.json`) is machine-level: a usage
//! limit belongs to the account, not to the daemon that ran into it, so every
//! daemon on the box reads the same file. Updates lock, reread and atomically
//! rename the registry; probes run outside that lock and merge only their
//! own observations, so concurrent changes to other logins are preserved.
//!
//! Selection is sticky, not round-robin: a turn runs on the account it was
//! asked for (`/account` › the Customize sheet › `default`) until that seat's
//! window is full, then on the first other seat that is not, and it comes back
//! by itself once the window rolls over. Windows are remembered by their reset
//! time (`exhausted`), which is what makes "come back by itself" free.

use mafold_transcript::failure::{AccountIssue, FailureKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// The one environment variable this module exists to set.
pub const ENV: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";
/// The login `~/.claude` already holds — no env, no directory of its own.
pub const DEFAULT: &str = "default";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// `~/.mafold/claude-accounts.json`.
pub fn registry_path() -> PathBuf {
    home().join(".mafold/claude-accounts.json")
}

/// `~/.mafold/claude-accounts/<name>` — a named account's credential directory.
/// Only its path matters to Claude Code (the Keychain item is named after it);
/// on Linux/Windows the credential file lives inside.
pub fn account_dir(name: &str) -> PathBuf {
    home().join(".mafold/claude-accounts").join(name)
}

/// Epoch seconds now.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One login. `dir: None` is the default account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    /// The credential directory (`CLAUDE_SECURESTORAGE_CONFIG_DIR`). None =
    /// `~/.claude`'s own login, which needs no env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// The email the login reported (`/api/oauth/profile`). Display only —
    /// the credential itself never leaves Claude Code's storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Epoch seconds the account was added (0 for `default`).
    #[serde(default)]
    pub added_at: i64,
}

impl Account {
    /// The login `~/.claude` holds.
    pub fn default_login() -> Self {
        Account { name: DEFAULT.into(), dir: None, email: None, added_at: 0 }
    }

    pub fn is_default(&self) -> bool {
        self.dir.is_none()
    }

    /// The process environment that makes `claude` use THIS login. Empty for
    /// the default account — the daemon's own env already is that login.
    pub fn env(&self) -> Vec<(String, String)> {
        match &self.dir {
            Some(d) => vec![(ENV.to_string(), d.clone())],
            None => Vec::new(),
        }
    }

    /// Both places this login can be — the one thing to read it through —
    /// for a process with this one's `CLAUDE_CONFIG_DIR`: the macOS Keychain
    /// item Claude Code files it under (bare for the default login, `-<8 hex>`
    /// of the directory's sha256 otherwise) and the file it falls back to.
    pub fn store(&self) -> LoginStore {
        self.store_for(&[])
    }

    /// [`Account::store`] for a process started with `env` on top of this
    /// one's (a turn's env): its `CLAUDE_CONFIG_DIR` first, else the one this
    /// process inherited — the same inheritance [`Account::from_env`] follows
    /// for the seat variable.
    pub fn store_for(&self, env: &[(String, String)]) -> LoginStore {
        let var = |name: &str| {
            env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).or_else(|| std::env::var(name).ok())
        };
        // `from_env` reads an empty storage variable as the default seat —
        // right for picking a seat — but to Claude Code it is still PRESENT,
        // and a present one decides alone: `~/.claude`, the bare item.
        // A blank one is a directory name to Claude Code like any other.
        let storage = match &self.dir {
            Some(dir) => Some(dir.clone()),
            None => var(ENV).filter(|v| v.trim().is_empty()),
        };
        LoginStore::resolve(&home(), storage.as_deref(), var(CONFIG_DIR_ENV).as_deref())
    }

    /// [`Account::store`] with the home and `CLAUDE_CONFIG_DIR` handed in, for
    /// tests that must not depend on the machine's. A seat's own directory
    /// decides alone; the default login goes wherever the config dir sends it.
    #[cfg(test)]
    pub fn store_in(&self, home: &Path, config_dir: Option<&str>) -> LoginStore {
        LoginStore::resolve(home, self.dir.as_deref(), config_dir)
    }

    /// The account a process env describes — the inverse of [`Account::env`],
    /// for code that is handed a turn's env rather than an account. A
    /// directory the registry doesn't know (someone set the variable by hand)
    /// still resolves, named after its last path component.
    pub fn from_env(env: &[(String, String)]) -> Account {
        // A daemon pinned to one login by its OWN environment (the
        // supervisor's per-daemon `env` map) is "default" to its turns — an
        // empty turn env inherits, and what it inherits is that directory —
        // so the credential lookups must follow it too.
        Self::from_env_inheriting(env, std::env::var(ENV).ok())
    }

    /// [`Account::from_env`] with what the process would inherit passed IN
    /// rather than read: the one piece of it that depends on where it runs.
    /// Tests call this, so they say what the process env is instead of
    /// picking up whichever login the machine running them is pinned to — a
    /// test run inside a daemon turn inherits that turn's seat.
    fn from_env_inheriting(env: &[(String, String)], inherited: Option<String>) -> Account {
        let dir = env
            .iter()
            .find(|(k, _)| k == ENV)
            .map(|(_, v)| v.to_string())
            .or(inherited)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let Some(dir) = dir else { return Self::default_login() };
        let reg = load();
        reg.accounts
            .iter()
            .find(|a| a.dir.as_deref() == Some(dir.as_str()))
            .cloned()
            .unwrap_or_else(|| Account {
                name: Path::new(&dir)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "?".into()),
                dir: Some(dir),
                email: None,
                added_at: 0,
            })
    }
}

/// The Keychain service of `~/.claude`'s own login; a seat's appends a suffix.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Claude Code's whole-config switch — which also moves its login.
pub const CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
/// `~/.claude`'s own credential file, relative to `$HOME`.
const DEFAULT_CREDENTIALS_FILE: &str = ".claude/.credentials.json";

/// Reads one item out of the login Keychain: `Some(password)`, `None` when the
/// item isn't there, or why it couldn't be read (a locked Keychain). A
/// parameter rather than a call so tests can say what it holds.
pub type Keychain<'a> = &'a dyn Fn(&str) -> Result<Option<String>, String>;

/// Where Claude Code keeps ONE login, and the one reader that knows both halves.
///
/// On macOS the login is a login-Keychain item ([`Account::keychain_service`]).
/// The file is only Claude Code's fallback for a Keychain write that failed,
/// and it deletes the file once a Keychain write lands — so a Mac with a
/// working `/login` has no `.credentials.json` at all. Its own read order is
/// Keychain, then file (2.1.294: a keychain store with a plaintext fallback),
/// and this follows it: a file left beside a Keychain item is the stale one.
/// Everywhere else the file is the login.
///
/// Two readers that each knew half of this is how importing a login (the web's
/// Connect ▸ Claude Code) failed on every Mac — fei, 2026-10-14, a minute after
/// a successful `/login` — while the usage readout next to it kept working.
#[derive(Debug, Clone, PartialEq)]
pub struct LoginStore {
    pub file: PathBuf,
    pub keychain_service: String,
}

impl LoginStore {
    /// `~/.claude`'s own login under `home` — [`Account::default_login`]'s
    /// store, for a caller that holds the home rather than reading `$HOME`.
    pub fn default_in(home: &Path) -> LoginStore {
        LoginStore { file: home.join(DEFAULT_CREDENTIALS_FILE), keychain_service: KEYCHAIN_SERVICE.into() }
    }

    /// The login a `claude` started with these two variables runs on —
    /// Claude Code 2.1.294's `uw()` (where the file is) and `FF()` (what the
    /// Keychain item is called), rule for rule:
    ///
    /// * `CLAUDE_SECURESTORAGE_CONFIG_DIR` present decides alone: its
    ///   directory and `-<hash>` item, or — present but EMPTY — `~/.claude`'s
    ///   file and the bare item, whatever the config dir says.
    /// * else a non-empty `CLAUDE_CONFIG_DIR`: that directory and its
    ///   `-<hash>` item.
    /// * else `~/.claude` and the bare item.
    ///
    /// A directory is taken as given, NFC-normalised — no `~`, no resolving —
    /// because that string is what Claude Code hashes. (A RELATIVE one is
    /// therefore relative to this process's directory, where Claude Code's is
    /// its turn's.) One deliberate departure: an EMPTY `CLAUDE_CONFIG_DIR`
    /// gives Claude Code the bare item — same here — but a file at
    /// `./.credentials.json` in whatever directory it started in; this reads
    /// `~/.claude`'s, the only such file that means anything to us.
    pub fn resolve(home: &Path, securestorage: Option<&str>, config_dir: Option<&str>) -> LoginStore {
        use unicode_normalization::UnicodeNormalization;
        let named = |dir: &str| {
            let dir: String = dir.nfc().collect();
            LoginStore {
                file: Path::new(&dir).join(".credentials.json"),
                keychain_service: format!("{KEYCHAIN_SERVICE}-{}", hash8(&dir)),
            }
        };
        match (securestorage, config_dir) {
            (Some(""), _) => Self::default_in(home),
            (Some(dir), _) => named(dir),
            (None, Some(dir)) if !dir.is_empty() => named(dir),
            (None, _) => Self::default_in(home),
        }
    }


    /// The stored credential blob, from wherever this machine keeps it.
    pub fn read(&self) -> Result<String, NoStoredLogin> {
        self.read_with(keychain())
    }

    /// [`LoginStore::read`] with the Keychain handed in: `None` where there is
    /// none to ask.
    pub fn read_with(&self, keychain: Option<Keychain>) -> Result<String, NoStoredLogin> {
        let mut asked = None;
        if let Some(read) = keychain {
            match read(&self.keychain_service) {
                Ok(Some(blob)) => return Ok(blob),
                Ok(None) => asked = Some((self.keychain_service.clone(), None)),
                Err(why) => asked = Some((self.keychain_service.clone(), Some(why))),
            }
        }
        std::fs::read_to_string(&self.file).map_err(|e| NoStoredLogin {
            file: self.file.clone(),
            file_error: e,
            keychain: asked,
        })
    }
}

/// A login read from neither place. The message names both, so "sign in
/// first" is never answered with only a path the Mac never had — and is only
/// said when both places answered "not here".
#[derive(Debug)]
pub struct NoStoredLogin {
    pub file: PathBuf,
    file_error: std::io::Error,
    /// The Keychain item asked for, and why it couldn't be read — `None`
    /// when it simply isn't there. Absent where there was no Keychain to ask.
    keychain: Option<(String, Option<String>)>,
}

impl NoStoredLogin {
    /// Every place answered "not here": signed out, as opposed to a login that
    /// exists but couldn't be read (a locked Keychain, a file we may not open).
    pub fn signed_out(&self) -> bool {
        self.file_error.kind() == std::io::ErrorKind::NotFound
            && self.keychain.as_ref().is_none_or(|(_, why)| why.is_none())
    }
}

impl std::fmt::Display for NoStoredLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut places = Vec::new();
        match &self.keychain {
            Some((service, None)) => places.push(format!("no Keychain item `{service}`")),
            Some((service, Some(why))) => places.push(format!("Keychain item `{service}`: {why}")),
            None => {}
        }
        places.push(match self.file_error.kind() {
            std::io::ErrorKind::NotFound => format!("no {}", self.file.display()),
            _ => format!("{}: {}", self.file.display(), self.file_error),
        });
        if self.signed_out() {
            write!(f, "no Claude Code login on this machine — sign in with `claude` first ({})", places.join("; "))
        } else {
            write!(f, "couldn't read the Claude Code login on this machine ({})", places.join("; "))
        }
    }
}

impl std::error::Error for NoStoredLogin {}

/// This machine's login Keychain, when Claude Code keeps logins in one
/// (macOS), for readers that ask over and over — the credential sweep, the
/// usage readout: an item that just failed is left alone for a while.
pub fn keychain() -> Option<Keychain<'static>> {
    cfg!(target_os = "macos").then_some(&remembered_keychain_password as Keychain<'static>)
}

/// [`keychain`] for a read a person is waiting on (Connect ▸ Claude Code):
/// always asked now, lock and all — they may have just unlocked it.
pub fn keychain_now() -> Option<Keychain<'static>> {
    cfg!(target_os = "macos").then_some(&keychain_password_now as Keychain<'static>)
}

/// How long a Keychain item that couldn't be read is left alone.
const KEYCHAIN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// Keychain items that recently couldn't be read, and what was said.
#[derive(Default)]
struct Unanswered(HashMap<String, (Instant, String)>);

impl Unanswered {
    /// The failure to repeat instead of asking again, if one is recent.
    fn recent(&self, service: &str, now: Instant) -> Option<String> {
        self.0
            .get(service)
            .filter(|(at, _)| now.saturating_duration_since(*at) < KEYCHAIN_BACKOFF)
            .map(|(_, why)| why.clone())
    }

    /// Remember how asking about `service` went: only a failure is kept —
    /// an answer, even "not there", is asked again next time.
    fn record(&mut self, service: &str, now: Instant, got: &Result<Option<String>, String>) {
        match got {
            Err(why) => {
                self.0.insert(service.to_string(), (now, why.clone()));
            }
            Ok(_) => {
                self.0.remove(service);
            }
        }
    }
}

static UNANSWERED: OnceLock<Mutex<Unanswered>> = OnceLock::new();

/// [`keychain_password`], minus the waits a Keychain that won't answer costs.
fn remembered_keychain_password(service: &str) -> Result<Option<String>, String> {
    let memo = UNANSWERED.get_or_init(Default::default);
    if let Some(why) = memo.lock().unwrap_or_else(|p| p.into_inner()).recent(service, Instant::now()) {
        return Err(why);
    }
    let got = keychain_password(service);
    memo.lock().unwrap_or_else(|p| p.into_inner()).record(service, Instant::now(), &got);
    got
}

/// How long `security` may take — Claude Code's own bound on the same read.
/// Something that still waits on a person (a dialog) must not hold the
/// daemon's link handler or a write; a login Claude Code couldn't read in
/// that time is one it won't run on.
const KEYCHAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `security`'s exit status for "no such item" (errSecItemNotFound) — the one
/// failure that means the login isn't there rather than couldn't be read.
const SECURITY_NOT_FOUND: i32 = 44;

/// `security show-keychain-info`'s exit status for a locked login Keychain —
/// the probe Claude Code itself uses, and one that puts up no dialog.
const SECURITY_LOCKED: i32 = 36;

/// How long a lock probe's answer stands: a sweep asks about every login on
/// the machine at once, and one probe covers them all.
const LOCK_PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// What the lock probe said.
#[derive(Clone)]
enum Lock {
    Unlocked,
    Locked,
    /// It couldn't tell — `security` hung or wouldn't start.
    Unknown(String),
}

/// The login Keychain's lock state. Asked before an item is: reading an item
/// out of a locked Keychain in a GUI session raises an unlock dialog that
/// waits on whoever is at the screen — once per item, every few seconds, from
/// a daemon nobody is watching. `fresh` skips the remembered answer.
fn keychain_lock(fresh: bool) -> Lock {
    static PROBED: Mutex<Option<(Instant, Lock)>> = Mutex::new(None);
    let mut probed = PROBED.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, lock)) = probed.as_ref().filter(|_| !fresh) {
        if at.elapsed() < LOCK_PROBE_TTL {
            return lock.clone();
        }
    }
    let lock = match run_security(&["show-keychain-info"]) {
        Ok((status, _, _)) if status.code() == Some(SECURITY_LOCKED) => Lock::Locked,
        Ok(_) => Lock::Unlocked,
        // Not knowing is no reason to go and read an item, which may raise
        // the very dialog the probe is here to avoid.
        Err(why) => Lock::Unknown(why),
    };
    *probed = Some((Instant::now(), lock.clone()));
    lock
}

/// One generic password out of the login Keychain, through `security` — the
/// tool Claude Code itself writes the item with, which is why a daemon reads
/// it without a prompt. `None` when there is no such item; otherwise any
/// failure is what `security` said, or that the Keychain is locked.
fn keychain_password(service: &str) -> Result<Option<String>, String> {
    read_keychain_item(service, false)
}

/// [`keychain_password`], probing the lock afresh.
fn keychain_password_now(service: &str) -> Result<Option<String>, String> {
    read_keychain_item(service, true)
}

fn read_keychain_item(service: &str, fresh: bool) -> Result<Option<String>, String> {
    match keychain_lock(fresh) {
        Lock::Unlocked => {}
        Lock::Locked => return Err("the login Keychain is locked — unlock it and try again".to_string()),
        Lock::Unknown(why) => return Err(format!("couldn't tell whether the login Keychain is locked ({why})")),
    }
    let (status, stdout, stderr) = run_security(&["find-generic-password", "-s", service, "-w"])?;
    if status.code() == Some(SECURITY_NOT_FOUND) {
        return Ok(None);
    }
    if !status.success() {
        let said = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(if said.is_empty() { format!("security {status}") } else { said });
    }
    String::from_utf8(stdout)
        .map(|s| Some(unhex(s.trim_end())))
        .map_err(|_| "the item is not UTF-8".to_string())
}

/// `security <args>`, bounded by [`KEYCHAIN_TIMEOUT`]: how it exited, and
/// what it printed on each stream.
fn run_security(args: &[&str]) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = crate::platform::std_command("security")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run security: {e}"))?;
    // Drained beside the wait: a blob that outgrows the pipe would otherwise
    // hold `security` open until the deadline.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + KEYCHAIN_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "no answer from the Keychain in {}s — is it locked?",
                    KEYCHAIN_TIMEOUT.as_secs()
                ));
            }
            Err(e) => return Err(format!("security: {e}")),
        }
    };
    Ok((status, out.join().unwrap_or_default(), err.join().unwrap_or_default()))
}

/// `security -w`'s output as the text it stands for: a password that isn't
/// plain printable text comes out as hex. Claude Code's store is a JSON
/// object, so hex that decodes to one is read as that, and anything else is
/// left exactly as it came.
fn unhex(s: &str) -> String {
    let hexy = !s.is_empty() && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !hexy {
        return s.to_string();
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|text| serde_json::from_str::<Value>(text).is_ok_and(|v| v.is_object()))
        .unwrap_or_else(|| s.to_string())
}

/// First 8 hex chars of `sha256(dir)` — Claude Code's suffix for the Keychain
/// item of a non-default storage directory. Verbatim from the 2.1.260 binary:
/// `sha256(process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR.normalize("NFC"))
/// .digest("hex").substring(0, 8)`, with an unset OR EMPTY variable meaning
/// the bare (default) item. Hashed exactly as handed over in the env — not
/// resolved, not canonicalized — so the daemon must always pass the same
/// string it hashes (the registry stores the absolute path for that reason).
pub fn hash8(dir: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(dir.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An account name: lowercase, digits, `-` `_` `.`, starts alphanumeric, ≤32.
/// It becomes a directory name, an env value and a Customize option — one
/// shape that is safe in all three.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// A window that was found full, remembered until it rolls over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exhausted {
    /// Epoch seconds the window resets — the account is skipped until then.
    pub until: i64,
    /// The window, as Claude names it (`five_hour`, `seven_day`, …).
    pub kind: String,
    /// When the wall was hit.
    pub at: i64,
    /// The model this hold applies to, when the window was scoped to one
    /// (see [`Wall::scope`]). None = the whole account. Absent in files
    /// written by an earlier build, which read back as account-wide — the
    /// safe direction: it can cost one avoidable switch, never a wrong claim
    /// that a full window is fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Credential that hit this wall. Older registries have no fingerprint;
    /// those marks remain until reset, an explicit login, or a live probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl Exhausted {
    /// Does this hold stop a turn running `model`? Same rule as [`Wall::stops`].
    pub fn stops(&self, model: Option<&str>) -> bool {
        let Some(scope) = &self.scope else { return true };
        model.is_some_and(|m| model_matches(m, scope))
    }
}

/// `~/.mafold/claude-accounts.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    /// In selection order; `default` is always first.
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// account name → the window that is full right now.
    #[serde(default)]
    pub exhausted: BTreeMap<String, Exhausted>,
    /// account name → Anthropic refused its sign-in on a real run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub signed_out: BTreeMap<String, SignedOut>,
}

/// A seat whose sign-in was refused mid-turn — a revoked refresh token, a
/// login undone from claude.ai. Its credential file still LOOKS renewable
/// (lapsed access token + refresh token = [`SeatState::Idle`]), so without
/// this mark every turn would pick it, fail on "Please run /login", and do it
/// again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedOut {
    /// When the refusal happened.
    pub at: i64,
    /// Fingerprint of the credential that was refused
    /// ([`crate::commands::credential_fingerprint`]). A different one on file
    /// means someone signed in again — `/login`, a terminal, anywhere — and
    /// the mark no longer applies. None = it couldn't be read; the mark then
    /// holds until a probe gets a live answer from the seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl Registry {
    /// `default` first and exactly once; every other row needs a directory
    /// (a row without one is a hand edit gone wrong, not an account).
    fn normalize(&mut self) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::with_capacity(self.accounts.len() + 1);
        out.push(Account::default_login());
        seen.insert(DEFAULT.to_string());
        for a in self.accounts.drain(..) {
            if a.dir.is_none() || !valid_name(&a.name) {
                continue;
            }
            if seen.insert(a.name.clone()) {
                out.push(a);
            }
        }
        self.accounts = out;
    }

    pub fn get(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.name == name)
    }

    /// Register `name` (idempotent), creating its credential directory. The
    /// login itself is Claude Code's job (`/login <name>`); this only gives it
    /// a place to keep the result.
    pub fn add(&mut self, name: &str) -> Result<Account, String> {
        let name = name.trim().to_ascii_lowercase();
        if !valid_name(&name) {
            return Err(format!(
                "`{name}` isn't a usable account name — lowercase letters, digits, `-` `_` `.`, up to 32 characters"
            ));
        }
        if let Some(a) = self.get(&name) {
            return Ok(a.clone());
        }
        let dir = account_dir(&name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        let a = Account {
            name: name.clone(),
            dir: Some(dir.to_string_lossy().into_owned()),
            email: None,
            added_at: now(),
        };
        self.accounts.push(a.clone());
        Ok(a)
    }

    /// Forget `name` (never `default`). The credential directory stays — a
    /// `/logout` clears the login; this only stops offering the seat.
    pub fn remove(&mut self, name: &str) -> bool {
        if name == DEFAULT {
            return false;
        }
        let n = self.accounts.len();
        self.accounts.retain(|a| a.name != name);
        self.exhausted.remove(name);
        self.signed_out.remove(name);
        self.accounts.len() != n
    }

    /// Remember that `name`'s sign-in was refused, against the credential
    /// that was refused.
    pub fn mark_signed_out(&mut self, name: &str, credential: Option<String>) {
        self.signed_out.insert(name.to_string(), SignedOut { at: now(), credential });
    }

    /// Is `name` still the login that was refused? `now_fp` is the credential
    /// on file now. The mark applies while that is the credential that was
    /// refused (or either side couldn't be read); a different one means a
    /// fresh sign-in happened. A live answer from the seat clears the mark
    /// outright — see `first_usable`.
    pub fn refused(&self, name: &str, now_fp: Option<&str>) -> bool {
        match self.signed_out.get(name) {
            None => false,
            Some(m) => match (&m.credential, now_fp) {
                (Some(was), Some(is)) => was == is,
                (None, _) => true,
                (Some(_), None) => true,
            },
        }
    }

    pub fn mark_exhausted(&mut self, name: &str, kind: &str, until: i64, scope: Option<String>, credential: Option<String>) {
        self.exhausted
            .insert(name.to_string(), Exhausted { until, kind: kind.to_string(), at: now(), scope, credential });
    }

    pub fn clear_exhausted(&mut self, name: &str) -> bool {
        self.exhausted.remove(name).is_some()
    }

    /// The window that is still full at `now`, if any.
    pub fn exhausted_at(&self, name: &str, now: i64) -> Option<&Exhausted> {
        self.exhausted.get(name).filter(|x| x.until > now)
    }

    /// Drop marks whose window has rolled over. True when anything changed.
    pub fn prune(&mut self, now: i64) -> bool {
        let n = self.exhausted.len();
        self.exhausted.retain(|_, x| x.until > now);
        self.exhausted.len() != n
    }

    /// Candidate order for a turn: the preferred account first (when this
    /// machine has it), then everyone else in registry order.
    pub fn ordered(&self, preferred: Option<&str>) -> Vec<Account> {
        let mut out: Vec<Account> = Vec::with_capacity(self.accounts.len());
        if let Some(p) = preferred.and_then(|p| self.get(p)) {
            out.push(p.clone());
        }
        for a in &self.accounts {
            if !out.iter().any(|x| x.name == a.name) {
                out.push(a.clone());
            }
        }
        out
    }
}

pub fn load() -> Registry {
    load_from(&registry_path())
}

pub fn load_from(path: &Path) -> Registry {
    let mut r: Registry = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    r.normalize();
    r
}

/// Update the machine registry under one cross-process lock. Never run network
/// requests inside the closure; asynchronous probes use `save_observations`.
pub fn update_registry<T>(update: impl FnOnce(&mut Registry) -> T) -> std::io::Result<T> {
    update_registry_at(&registry_path(), update)
}

fn update_registry_at<T>(path: &Path, update: impl FnOnce(&mut Registry) -> T) -> std::io::Result<T> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A separate lock inode survives renames of the registry itself.
    let lock = std::fs::OpenOptions::new().create(true).write(true).truncate(false)
        .open(path.with_extension("lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    crate::platform::lock_file_exclusive(&lock)?;
    // The forgiving reader is for display/selection. A writer must not turn
    // a damaged or unreadable registry into a fresh one and destroy evidence.
    let mut reg: Registry = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry::default(),
        Err(error) => return Err(error),
    };
    reg.normalize();
    let result = update(&mut reg);
    save_to(path, &reg)?;
    Ok(result)
}

fn same_account_identity(current: Option<&Account>, previous: Option<&Account>) -> bool {
    matches!((current, previous), (Some(a), Some(b)) if a.name == b.name && a.dir == b.dir && a.added_at == b.added_at)
}

/// Probes happen without the file lock. Merge each changed mark only if it
/// has not changed since the probe began; a newer refusal/login wins.
fn merge_observations(current: &mut Registry, before: &Registry, after: &Registry) {
    // A name can have been removed or reassigned while a probe was in flight.
    // Its old result belongs only to the original credential directory/seat.
    // Display metadata (email) does not change that identity.
    let unchanged_accounts: HashSet<&str> = before.accounts.iter()
        .filter(|a| same_account_identity(current.get(&a.name), Some(a)))
        .map(|a| a.name.as_str())
        .collect();
    fn merge<T: Clone + PartialEq>(current: &mut BTreeMap<String, T>, before: &BTreeMap<String, T>, after: &BTreeMap<String, T>, unchanged_accounts: &HashSet<&str>) {
        for key in before.keys().chain(after.keys()).collect::<HashSet<_>>() {
            if unchanged_accounts.contains(key.as_str()) && before.get(key) != after.get(key) && current.get(key) == before.get(key) {
                match after.get(key) {
                    Some(value) => { current.insert(key.clone(), value.clone()); }
                    None => { current.remove(key); }
                }
            }
        }
    }
    merge(&mut current.exhausted, &before.exhausted, &after.exhausted, &unchanged_accounts);
    merge(&mut current.signed_out, &before.signed_out, &after.signed_out, &unchanged_accounts);
}

fn save_observations(before: &Registry, after: &Registry) {
    if let Err(e) = update_registry(|current| merge_observations(current, before, after)) {
        eprintln!("couldn't remember Claude account observations: {e}");
    }
}

/// Atomic: a per-process scratch file renamed over the real one, so two
/// daemons writing at once can't interleave and a crash can't leave a torn file.
pub fn save_to(path: &Path, reg: &Registry) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(reg)?)?;
    std::fs::rename(&tmp, path)
}

/// "resets in 2h 05m", or "resets soon" once the moment has passed.
pub fn reset_hint(until: i64, now: i64) -> String {
    if until > now {
        format!("resets in {}", crate::commands::fmt_dur(until - now))
    } else {
        "resets soon".to_string()
    }
}

/// How far ahead a listing starts warning that a seat's sign-in is about to
/// end ([`crate::commands::login_expiry_from`]).
pub const EXPIRY_WARN_SECS: i64 = 3 * 86400;

/// The command that signs `name` in (again): the default login has no name.
pub fn login_command(name: &str) -> String {
    if name == DEFAULT { "/login".into() } else { format!("/login {name}") }
}

/// What a listing says about a seat's sign-in deadline, `end` in epoch
/// seconds: the time left, a ⚠️ once it is inside [`EXPIRY_WARN_SECS`], and
/// when it ran out once it has. None when the login has no deadline on file.
pub fn login_expiry_note(name: &str, end: Option<i64>, now: i64) -> Option<String> {
    let end = end?;
    let fix = login_command(name);
    Some(if end <= now {
        format!("⚠️ stored credential deadline passed {} ago — {fix} renews it", crate::commands::fmt_dur(now - end))
    } else if end - now <= EXPIRY_WARN_SECS {
        format!("⚠️ stored credential deadline in {} — {fix} renews it", crate::commands::fmt_dur(end - now))
    } else {
        format!("stored credential deadline in {} (not an authentication check)", crate::commands::fmt_dur(end - now))
    })
}

/// Why a seat with no usable credential is passed over — and, when its
/// sign-in simply ran out, that it did and when.
fn signed_out_reason(acct: &Account, now: i64) -> String {
    let fix = login_command(&acct.name);
    match crate::commands::login_expiry(&acct.env()).map(|ms| ms / 1000) {
        Some(end) if end <= now => format!(
            "isn't logged in — its sign-in expired {} ago; `{fix}` fixes that",
            crate::commands::fmt_dur(now - end)
        ),
        _ => format!("isn't logged in — `{fix}` fixes that"),
    }
}

/// A run's error text says Anthropic refused the seat's SIGN-IN — the shape a
/// revoked refresh token takes once the turn is already running on it.
/// Phrasings taken from the Claude Code 2.1.282 binary:
/// `API Error: 401 Invalid API key · Please run /login` and
/// `Not logged in · Please run /login`, plus the API's own
/// `authentication_error`.
///
/// NOT `Failed to refresh OAuth token: another Claude Code process is
/// refreshing it…` — that one is transient by its own account ("retry in a
/// minute"), and marking a seat signed out over it would hide a good login.
pub fn signed_out_error(text: &str) -> bool {
    mafold_transcript::failure::is_auth_refusal(text)
}

/// A run's error text says the seat's usage limit was hit — `Some(the reset
/// epoch, when the text carried one)`. Claude Code prints
/// `Claude usage limit reached|<epoch>` as its whole reason on stdout, or ends
/// the turn with `You've hit your session limit · resets 6:30pm (…)` (seen
/// 2026-09-06, no epoch); other phrasings carry no timestamp and come back as
/// `Some(None)`.
pub fn usage_limit_reset(text: &str) -> Option<Option<i64>> {
    let lower = text.to_ascii_lowercase();
    let is_limit = lower.contains("usage limit")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || (lower.contains("hit your") && lower.contains("limit"));
    if !is_limit {
        return None;
    }
    let epoch = text
        .split('|')
        .nth(1)
        .map(str::trim)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<i64>().ok())
        .filter(|t| *t > 1_000_000_000);
    Some(epoch)
}

// ───────────────────────── seat state (live) ─────────────────────────

/// What a seat can be right now, as far as a quota-free probe can tell.
#[derive(Debug, Clone, PartialEq)]
pub enum SeatState {
    Ok,
    /// An ACCOUNT-WIDE window is full and nothing (extra usage) is covering
    /// the overflow. A window that is full for one MODEL only is not this —
    /// see [`Wall::scope`].
    Exhausted { kind: String, until: i64 },
    /// No stored credential or no way to renew it.
    NoCredential,
    /// Upstream explicitly refused the credential (HTTP 401 or a run refusal).
    AuthRefused,
    /// A locked keychain / inaccessible credential file, not a missing login.
    CredentialUnreadable(String),
    /// Upstream said 403 — re-logging-in does not fix it.
    Rejected(u16),
    /// Couldn't tell (network, an unexpected status): let the turn try.
    Unknown,
    /// Signed in, but nobody has run Claude Code on it for 8 hours: the
    /// access token has lapsed and the refresh token is intact
    /// ([`crate::commands::StoredLogin::Stale`]). Claude Code renews it on
    /// the turn's own run, so the seat is usable — its windows just can't be
    /// read until then. Every idle backup seat is in this state.
    Idle,
}

/// A usage window found full.
#[derive(Debug, Clone, PartialEq)]
pub struct Wall {
    /// Claude's own name for the window (`session`, `weekly_all`,
    /// `weekly_scoped`, `seven_day_opus`, …).
    pub kind: String,
    /// Epoch seconds it rolls over.
    pub until: i64,
    /// The MODEL this wall applies to, lowercased, when the window is scoped
    /// to one (`weekly_scoped` carries `scope.model.display_name`; a kind
    /// like `seven_day_opus` names it in the kind). None = the whole account
    /// is out, whatever it is asked to run.
    ///
    /// This distinction is load-bearing, not cosmetic: on 2026-09-06 this
    /// machine's account sat at `weekly_scoped` 100% (Fable) with `session`
    /// at 29% and `weekly_all` at 54%. Treating that as an exhausted ACCOUNT
    /// would hand every turn — Sonnet, Opus, Haiku, all fine — to another
    /// login for five days.
    pub scope: Option<String>,
}

impl Wall {
    /// Does this wall stop a turn running `model`? An account-wide wall stops
    /// everything. A scoped one stops only its own model — and when the turn
    /// has no model pinned (the harness's own default, which we cannot know
    /// from here) it does NOT stop it: guessing wrong would hand away a
    /// healthy seat, while letting the turn try costs at most one refusal,
    /// which the runtime failover already handles.
    pub fn stops(&self, model: Option<&str>) -> bool {
        let Some(scope) = &self.scope else { return true };
        model.is_some_and(|m| model_matches(m, scope))
    }
}

/// Is `model` (a `/model` value or Customize选项 — "fable", "opus",
/// "claude-fable-5-1", …) the model a window is scoped to ("fable")? Compared
/// on the scope's name appearing in the model id, both lowercased, because
/// the two come from different vocabularies: the scope is a display name and
/// the model is whatever the user or the sheet wrote.
fn model_matches(model: &str, scope: &str) -> bool {
    let (m, s) = (model.to_ascii_lowercase(), scope.to_ascii_lowercase());
    !s.is_empty() && (m.contains(&s) || s.contains(&m))
}

/// The model a window's own name scopes it to, for the kinds that carry it
/// there instead of in `scope` (`seven_day_opus` → `opus`). None for the
/// account-wide kinds and for `weekly_scoped`, whose model rides in `scope`.
fn runtime_scope(kind: &str, model: Option<&str>) -> Option<String> {
    model_in_kind(kind).or_else(|| kind.ends_with("_scoped").then(|| model.map(str::to_ascii_lowercase)).flatten())
}

fn model_in_kind(kind: &str) -> Option<String> {
    const KNOWN: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "omelette"];
    let k = kind.to_ascii_lowercase();
    KNOWN.iter().find(|m| k.ends_with(&format!("_{m}"))).map(|m| (*m).to_string())
}

#[derive(Debug, Clone)]
pub struct SeatSnapshot {
    pub state: SeatState,
    /// Login measured by this observation (never displayed or sent upstream).
    pub credential: Option<String>,
    /// The fullest window — (Claude's kind, percent, resets_at) — for listings.
    pub worst: Option<(String, f64, Option<i64>)>,
    /// Every window found full, account-wide ones first (so the first one
    /// that stops a turn is also the most useful reason to report).
    pub walls: Vec<Wall>,
}

impl SeatSnapshot {
    fn bare(state: SeatState) -> Self {
        SeatSnapshot { state, credential: None, worst: None, walls: Vec::new() }
    }

    /// The wall that stops a turn running `model`, if any.
    pub fn blocks(&self, model: Option<&str>) -> Option<&Wall> {
        self.walls.iter().find(|w| w.stops(model))
    }

    /// One phrase for a listing: "ok · session 90%", "exhausted (weekly_all)
    /// — resets in 3h", "ok · Fable out (resets in 5d) · session 29%".
    pub fn describe(&self, now: i64) -> String {
        match &self.state {
            SeatState::Ok => {
                let mut out = String::new();
                // A model that is out is the first thing to say even though
                // the seat is "ok" — it is the difference between "this login
                // can take my turn" and "…but not the model I asked for".
                for w in self.walls.iter().filter(|w| w.scope.is_some()) {
                    let m = w.scope.as_deref().unwrap_or("scoped");
                    out.push_str(&format!("{m} out ({}) · ", reset_hint(w.until, now)));
                }
                match &self.worst {
                    Some((kind, pct, _)) => format!("ok · {out}{}: {}% used / {}% remaining", kind.replace('_', " "), pct.round() as i64, (100.0 - pct).clamp(0.0, 100.0).round() as i64),
                    None if out.is_empty() => "ok".into(),
                    None => format!("ok · {}", out.trim_end_matches(" · ")),
                }
            }
            SeatState::Exhausted { kind, until } => format!("exhausted ({kind}) — {}", reset_hint(*until, now)),
            SeatState::NoCredential => "not logged in — no renewable credential stored".into(),
            SeatState::AuthRefused => "authentication refused — sign in again".into(),
            SeatState::CredentialUnreadable(why) => format!("credential unreadable — {why}"),
            SeatState::Rejected(s) => format!("rejected upstream ({s})"),
            SeatState::Unknown => "unknown — no current usage measurement".into(),
            SeatState::Idle => "idle — stored credential needs refresh on its next run (authentication and usage unverified)".into(),
        }
    }
}

/// Read Claude's `/api/oauth/usage` payload as a seat verdict.
///
/// A window at 100% is only a wall when extra usage is not paying for the
/// overflow — with it enabled and its own spend limit not reached, requests
/// still go through. And a full window that is scoped to ONE MODEL leaves the
/// seat usable for every other one, so it is recorded as a scoped [`Wall`]
/// rather than as an exhausted account.
///
/// Payload shape verified live 2026-09-06: `limits[]` of
/// `{kind, group, percent, severity, resets_at (ISO), scope, is_active}`
/// where `scope` is `null` or `{"model":{"display_name":"Fable"}}`, plus
/// `extra_usage.{is_enabled, spend_limit_reached}`.
pub fn snapshot_from_usage(v: &Value, now: i64) -> SeatSnapshot {
    let extra = &v["extra_usage"];
    let overage_covers = extra["is_enabled"].as_bool() == Some(true)
        && extra["spend_limit_reached"].as_bool() != Some(true);
    let mut worst: Option<(String, f64, Option<i64>)> = None;
    let mut walls: Vec<Wall> = Vec::new();
    let mut stale_full = false;
    for l in v["limits"].as_array().into_iter().flatten() {
        let Some(pct) = l["percent"].as_f64().filter(|pct| pct.is_finite() && *pct >= 0.0) else { continue };
        let kind = l["kind"].as_str().unwrap_or("usage").to_string();
        let resets = l["resets_at"].as_str().and_then(crate::commands::iso_epoch_secs);
        if worst.as_ref().is_none_or(|w| pct > w.1) {
            worst = Some((kind.clone(), pct, resets));
        }
        if pct >= 100.0 && !overage_covers && resets.is_some_and(|t| t <= now) {
            stale_full = true;
        }
        if pct >= 100.0 && !overage_covers && resets.is_none_or(|t| t > now) {
            let scope = l["scope"]["model"]["display_name"]
                .as_str().filter(|s| !s.is_empty())
                .or_else(|| l["scope"]["model"]["id"].as_str().filter(|s| !s.is_empty()))
                .map(|s| s.to_ascii_lowercase())
                .or_else(|| model_in_kind(&kind));
            // A missing reset gets a bounded hold. A known past reset is
            // stale evidence and must not create a new hour-long hold.
            let until = resets.unwrap_or(now + 3600);
            walls.push(Wall { kind, until, scope });
        }
    }
    // Account-wide first: it is both the stronger verdict and the better
    // explanation when several windows are full at once.
    walls.sort_by_key(|w| w.scope.is_some());
    let state = match walls.first() {
        Some(w) if w.scope.is_none() => SeatState::Exhausted { kind: w.kind.clone(), until: w.until },
        _ if stale_full || worst.is_none() => SeatState::Unknown,
        _ => SeatState::Ok,
    };
    SeatSnapshot { state, credential: None, worst, walls }
}

/// Probe results live a minute: a turn start must not cost a round trip every
/// time, and a window does not empty between two messages anyway. Keyed by
/// account name; process-local (each daemon keeps its own).
const FRESH: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone)]
struct CachedSeat {
    at: Instant,
    snapshot: SeatSnapshot,
    /// Hash of storage identity + credential blob; never persisted or logged.
    /// None is reserved for an injected unit-test result.
    generation: Option<String>,
    marks: (Option<Exhausted>, Option<SignedOut>),
}

impl CachedSeat {
    fn fresh(&self, generation: &str, instant: Instant, now: i64) -> bool {
        self.generation.as_deref() == Some(generation)
            && instant.saturating_duration_since(self.at) < FRESH
            && self.snapshot.walls.iter().all(|w| w.until > now)
            && self.snapshot.worst.as_ref().and_then(|w| w.2).is_none_or(|reset| reset > now)
    }
}

fn cache() -> &'static Mutex<HashMap<String, CachedSeat>> {
    static C: OnceLock<Mutex<HashMap<String, CachedSeat>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Forget local observations after login/logout or a runtime refusal. Other
/// daemons notice changed credentials on their next read, independently.
pub fn forget_seat(name: &str) {
    cache().lock().unwrap_or_else(|p| p.into_inner()).remove(name);
}

#[cfg(test)]
fn seed_seat(name: &str, snap: SeatSnapshot) {
    cache().lock().unwrap().insert(name.to_string(), CachedSeat {
        at: Instant::now(), snapshot: snap, generation: None, marks: (None, None),
    });
}

/// Credential reads do not spend quota. Checking the generation before using
/// a cached result lets a login in another daemon invalidate stale auth/quota.
fn credential_generation(acct: &Account) -> String {
    let store = acct.store();
    let state = match store.read() {
        Ok(raw) => hash8(&raw),
        Err(e) if e.signed_out() => "missing".into(),
        Err(e) => format!("unreadable:{}", hash8(&e.to_string())),
    };
    format!("{}:{}:{state}", store.file.display(), store.keychain_service)
}

pub async fn check_seat(acct: &Account) -> SeatSnapshot {
    let cached = cache().lock().unwrap_or_else(|p| p.into_inner()).get(&acct.name).cloned();
    #[cfg(test)]
    if let Some(c) = &cached {
        if c.generation.is_none() { return c.snapshot.clone(); }
    }
    let generation = credential_generation(acct);
    let registry = load();
    if let Some(c) = cached {
        if c.marks == (registry.exhausted.get(&acct.name).cloned(), registry.signed_out.get(&acct.name).cloned())
            && c.fresh(&generation, Instant::now(), now()) { return c.snapshot; }
    }
    let marks = (registry.exhausted.get(&acct.name).cloned(), registry.signed_out.get(&acct.name).cloned());
    let mut snap = probe_seat(acct).await;
    let after = credential_generation(acct);
    if after != generation {
        // The request may have measured a login that no longer occupies the
        // slot. Discard its auth/quota evidence; let the new login try once.
        snap = SeatSnapshot::bare(SeatState::Unknown);
    }
    cache().lock().unwrap_or_else(|p| p.into_inner()).insert(acct.name.clone(), CachedSeat {
        at: Instant::now(), snapshot: snap.clone(), generation: Some(generation), marks,
    });
    snap
}

async fn probe_seat(acct: &Account) -> SeatSnapshot {
    let credential = crate::commands::credential_fingerprint(&acct.env());
    let mut snapshot = snapshot_from_probe(crate::commands::probe_utilization(&acct.env()).await, now());
    snapshot.credential = credential;
    snapshot
}

fn snapshot_from_probe(probe: crate::commands::UtilizationProbe, now: i64) -> SeatSnapshot {
    use crate::commands::UtilizationProbe as P;
    match probe {
        P::Ok(v) => snapshot_from_usage(&v, now),
        P::NoCredential => SeatSnapshot::bare(SeatState::NoCredential),
        P::Http(401) => SeatSnapshot::bare(SeatState::AuthRefused),
        P::Unreadable(why) => SeatSnapshot::bare(SeatState::CredentialUnreadable(why)),
        P::Stale => SeatSnapshot::bare(SeatState::Idle),
        P::Http(403) => SeatSnapshot::bare(SeatState::Rejected(403)),
        P::Http(_) | P::Unreachable => SeatSnapshot::bare(SeatState::Unknown),
    }
}

/// Reconcile a probe with runtime evidence once, for BOTH listing and choice.
/// Only a live answer can clear a remembered wall; missing/unreadable storage
/// and network failures cannot prove that the account has recovered.
fn reconcile_snapshot(reg: &mut Registry, acct: &Account, snap: &mut SeatSnapshot, now: i64) -> bool {
    let old_wall = reg.exhausted.get(&acct.name).cloned();
    let old_auth = reg.signed_out.get(&acct.name).cloned();
    match &snap.state {
        SeatState::Ok | SeatState::Exhausted { .. } => {
            reg.signed_out.remove(&acct.name);
            match snap.walls.first() {
                Some(w) => {
                    let matches = reg.exhausted.get(&acct.name).is_some_and(|x|
                        x.until == w.until && x.kind == w.kind && x.scope == w.scope && x.credential == snap.credential);
                    if !matches { reg.mark_exhausted(&acct.name, &w.kind, w.until, w.scope.clone(), snap.credential.clone()); }
                }
                None => { reg.clear_exhausted(&acct.name); }
            }
        }
        SeatState::Unknown | SeatState::Idle => {
            let fp = snap.credential.clone().or_else(|| {
                (reg.signed_out.contains_key(&acct.name) || reg.exhausted.get(&acct.name).is_some_and(|w| w.credential.is_some()))
                    .then(|| crate::commands::credential_fingerprint(&acct.env())).flatten()
            });
            if reg.exhausted.get(&acct.name).is_some_and(|w| {
                matches!((&w.credential, &fp), (Some(old), Some(current)) if old != current)
            }) {
                reg.clear_exhausted(&acct.name);
            }
            if reg.signed_out.contains_key(&acct.name) {
                if reg.refused(&acct.name, fp.as_deref()) {
                    snap.state = SeatState::AuthRefused;
                } else {
                    reg.signed_out.remove(&acct.name);
                }
            }
            if let Some(w) = reg.exhausted_at(&acct.name, now) {
                snap.walls.push(Wall { kind: w.kind.clone(), until: w.until, scope: w.scope.clone() });
                // Auth refusal remains visible when both facts are known.
                if w.scope.is_none() && !matches!(snap.state, SeatState::AuthRefused) {
                    snap.state = SeatState::Exhausted { kind: w.kind.clone(), until: w.until };
                }
            }
        }
        _ => {}
    }
    old_wall != reg.exhausted.get(&acct.name).cloned() || old_auth != reg.signed_out.get(&acct.name).cloned()
}

/// Every account with the same verdict a turn's chooser uses.
pub async fn list_states() -> Vec<(Account, SeatSnapshot, Option<Exhausted>)> {
    let mut reg = load();
    let before = reg.clone();
    let now = now();
    let mut dirty = reg.prune(now);
    let accounts = reg.accounts.clone();
    let probes = futures_util::future::join_all(accounts.iter().map(check_seat)).await;
    let rows = accounts.into_iter().zip(probes).map(|(a, mut s)| {
        dirty |= reconcile_snapshot(&mut reg, &a, &mut s, now);
        let held = reg.exhausted_at(&a.name, now).cloned();
        (a, s, held)
    }).collect();
    if dirty { save_observations(&before, &reg); }
    rows
}

// ───────────────────────── choosing a seat ─────────────────────────

/// The seat a turn runs on, and why it is not the one asked for (when it isn't).
#[derive(Debug, Clone)]
pub struct Choice {
    pub account: Option<Account>,
    pub preferred: String,
    pub skipped: Vec<AccountIssue>,
}

impl Choice {
    pub fn switched(&self) -> bool {
        self.account.as_ref().is_some_and(|a| a.name != self.preferred)
    }

    pub fn note(&self) -> Option<String> {
        if !self.switched() { return None; }
        let account = self.account.as_ref()?;
        let why = self.skipped.iter().map(|i| format!("`{}` {}", i.account, i.detail)).collect::<Vec<_>>().join("; ");
        Some(format!("↻ Using account `{}` — {why}", account.name))
    }
}

fn quota_issue(acct: &Account, wall: &Wall, now: i64) -> AccountIssue {
    let what = match &wall.scope {
        Some(m) => format!("is out of {m}"),
        None => format!("is exhausted ({})", wall.kind),
    };
    let mut issue = AccountIssue::new(&acct.name, FailureKind::RateLimit, format!("{what} — {}", reset_hint(wall.until, now)));
    issue.window = Some(wall.kind.clone());
    issue.resets_at = Some(wall.until);
    issue
}

async fn first_usable(
    reg: &mut Registry,
    order: &[Account],
    now: i64,
    model: Option<&str>,
    skipped: &mut Vec<AccountIssue>,
) -> (Option<Account>, bool) {
    let mut dirty = false;
    for acct in order {
        let mut snap = check_seat(acct).await;
        dirty |= reconcile_snapshot(reg, acct, &mut snap, now);
        let issue = match &snap.state {
            SeatState::NoCredential => Some(AccountIssue::new(&acct.name, FailureKind::Auth, signed_out_reason(acct, now))),
            SeatState::AuthRefused => Some(AccountIssue::new(&acct.name, FailureKind::Auth,
                format!("authentication refused — `{}` signs it in again", login_command(&acct.name)))),
            SeatState::CredentialUnreadable(why) => Some(AccountIssue::new(&acct.name, FailureKind::Connection,
                format!("credential unreadable — {why}"))),
            SeatState::Rejected(s) => Some(AccountIssue::new(&acct.name, FailureKind::Unavailable,
                format!("was rejected upstream ({s})"))),
            _ => snap.blocks(model).map(|w| quota_issue(acct, w, now)),
        };
        match issue {
            Some(issue) => skipped.push(issue),
            // Unknown/idle are unproven and may get one bounded attempt.
            // The caller's tried set prevents a second attempt this turn.
            None => return (Some(acct.clone()), dirty),
        }
    }
    (None, dirty)
}

/// Pick only a seat that has not been proved unusable. When all seats are
/// blocked the caller receives every reason and MUST NOT launch a process.
pub async fn choose(preferred: Option<&str>, model: Option<&str>) -> Choice {
    let mut reg = load();
    let before = reg.clone();
    let now = now();
    let mut dirty = reg.prune(now);
    let preferred_name = preferred.unwrap_or(DEFAULT).to_string();
    let mut skipped = Vec::new();
    if preferred.is_some() && reg.get(&preferred_name).is_none() {
        skipped.push(AccountIssue::new(&preferred_name, FailureKind::Auth,
            format!("isn't on this machine — `{}` adds it", login_command(&preferred_name))));
    }
    let order = reg.ordered(preferred);
    let (account, changed) = first_usable(&mut reg, &order, now, model, &mut skipped).await;
    dirty |= changed;
    if dirty { save_observations(&before, &reg); }
    Choice { account, preferred: preferred_name, skipped }
}

/// The seat behind a running turn just hit its wall: remember it until the
/// window resets and hand back the next seat that can take the turn over —
/// None when there is none, in which case the turn reports the wall. The
/// second slot says why each other seat was passed over.
///
/// The refusal names its window but not its model, so the scope is read out
/// of the window's own name (`seven_day_opus` → `opus`): a per-model wall
/// must not hold the whole account for a week.
fn remember_quota(
    reg: &mut Registry, current: &str, kind: &str, until: i64, model: Option<&str>,
    failed_credential: Option<&str>, observed: Option<&str>,
) {
    if let Some(failed) = failed_credential.filter(|failed| Some(*failed) == observed) {
        reg.mark_exhausted(current, kind, until, runtime_scope(kind, model), Some(failed.to_string()));
    }
}

pub async fn failover(
    current: &str,
    kind: &str,
    resets_at: Option<i64>,
    model: Option<&str>,
    tried: &[String],
    failed_credential: Option<&str>,
) -> (Option<Account>, Vec<AccountIssue>) {
    let now = now();
    let until = resets_at.filter(|t| *t > now).unwrap_or(now + 3600);
    let observed_account = load().get(current).cloned();
    let observed = observed_account.as_ref().and_then(|a| crate::commands::credential_fingerprint(&a.env()));
    let record = |reg: &mut Registry| {
        if same_account_identity(reg.get(current), observed_account.as_ref()) {
            remember_quota(reg, current, kind, until, model, failed_credential, observed.as_deref());
        }
        reg.clone()
    };
    let mut reg = update_registry(record).unwrap_or_else(|error| {
        eprintln!("couldn't remember Claude account failure: {error}");
        record(&mut load())
    });
    hand_over(&mut reg, current, now, model, tried).await
}

/// Pin a refusal to the credential the process actually used, never to a
/// replacement written by another session while the failing run was alive.
fn remember_refusal(reg: &mut Registry, current: &str, failed_credential: Option<&str>, observed: Option<&str>) {
    if let Some(failed) = failed_credential.filter(|failed| Some(*failed) == observed) {
        reg.mark_signed_out(current, Some(failed.to_string()));
    }
}

pub async fn failover_signed_out(
    current: &str,
    model: Option<&str>,
    tried: &[String],
    failed_credential: Option<&str>,
) -> (Option<Account>, Vec<AccountIssue>) {
    // Credential I/O happens outside the registry lock. Even if storage
    // changes again after this read, the mark carries the FAILED fingerprint,
    // so a replacement credential cannot inherit this refusal.
    let observed_account = load().get(current).cloned();
    let observed = observed_account.as_ref().and_then(|a| crate::commands::credential_fingerprint(&a.env()));
    let record = |reg: &mut Registry| {
        if same_account_identity(reg.get(current), observed_account.as_ref()) {
            remember_refusal(reg, current, failed_credential, observed.as_deref());
        }
        reg.clone()
    };
    let mut reg = update_registry(record).unwrap_or_else(|error| {
        eprintln!("couldn't remember Claude account failure: {error}");
        record(&mut load())
    });
    hand_over(&mut reg, current, now(), model, tried).await
}

fn untried_accounts(reg: &Registry, current: &str, tried: &[String]) -> Vec<Account> {
    reg.ordered(None).into_iter().filter(|a| a.name != current && !tried.contains(&a.name)).collect()
}

async fn hand_over(
    reg: &mut Registry,
    current: &str,
    now: i64,
    model: Option<&str>,
    tried: &[String],
) -> (Option<Account>, Vec<AccountIssue>) {
    forget_seat(current);
    let before = reg.clone();
    let order = untried_accounts(reg, current, tried);
    let mut skipped = Vec::new();
    let (pick, dirty) = first_usable(reg, &order, now, model, &mut skipped).await;
    if dirty { save_observations(&before, reg); }
    (pick, skipped)
}

/// Display metadata uses the same locked update as runtime marks.
pub fn set_email(name: &str, email: Option<String>) {
    let _ = update_registry(|reg| {
        if let Some(a) = reg.accounts.iter_mut().find(|a| a.name == name) { a.email = email; }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry's seats, in selection order.
    fn names(r: &Registry) -> Vec<String> {
        r.accounts.iter().map(|a| a.name.clone()).collect()
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mafold-accounts-{}-{name}.json", std::process::id()))
    }

    /// The suffix formula Claude Code names its Keychain item by, pinned to a
    /// pair OBSERVED on a real machine rather than to our own arithmetic:
    /// `/tmp/cc-acct-probe` was handed to Claude Code 2.1.x as
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` on 2026-09-02, and the item it wrote
    /// into the login Keychain was named `Claude Code-credentials-94718db4`.
    /// (Recovered the other way round — by searching for the directory whose
    /// digest produced that suffix — which is what makes it evidence and not
    /// a restatement of the code below.)
    ///
    /// Get this wrong and nothing fails loudly: the daemon simply reads the
    /// WRONG account's credential, or none at all, and reports a healthy seat
    /// as logged out.
    #[test]
    fn keychain_suffix_is_the_first_eight_hex_of_sha256() {
        assert_eq!(hash8("/tmp/cc-acct-probe"), "94718db4");
        let a = Account { name: "probe".into(), dir: Some("/tmp/cc-acct-probe".into()), email: None, added_at: 0 };
        assert_eq!(a.store_in(Path::new("/h"), None).keychain_service, "Claude Code-credentials-94718db4");
        // No directory ⇒ the BARE item: Claude Code appends the suffix only
        // when the variable is set and non-empty.
        assert_eq!(Account::default_login().store_in(Path::new("/h"), None).keychain_service, "Claude Code-credentials");
    }

    /// The import reads `~/.claude`'s login through [`LoginStore::default_in`]
    /// and the usage readout through [`Account::store`]; they must be the
    /// same two places, or one of them is reading somebody else's login.
    #[test]
    fn the_default_store_is_the_bare_item_and_the_home_file() {
        let home = Path::new("/Users/someone");
        let store = LoginStore::default_in(home);
        assert_eq!(store.file, Path::new("/Users/someone/.claude/.credentials.json"));
        assert_eq!(store.keychain_service, "Claude Code-credentials");
        assert_eq!(Account::default_login().store_in(home, None), LoginStore::default_in(home));
        let seat = Account { name: "probe".into(), dir: Some("/tmp/cc-acct-probe".into()), email: None, added_at: 0 };
        assert_eq!(
            seat.store(),
            LoginStore {
                file: "/tmp/cc-acct-probe/.credentials.json".into(),
                keychain_service: "Claude Code-credentials-94718db4".into(),
            }
        );
    }

    /// Claude Code's own read order on macOS: the Keychain, then the file. A
    /// file is only consulted when the Keychain has nothing, and a missing
    /// login names both places; with no Keychain the file is all there is.
    #[test]
    fn a_login_is_read_from_the_keychain_first_then_the_file() {
        let dir = std::env::temp_dir().join(format!("mafold-login-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = LoginStore { file: dir.join(".credentials.json"), keychain_service: "Claude Code-credentials".into() };
        let held = |s: &str| Ok((s == "Claude Code-credentials").then(|| "from-keychain".to_string()));
        let empty = |_: &str| Ok(None);
        let locked = |_: &str| Err("no answer from the Keychain in 2s — is it locked?".to_string());

        // Nothing anywhere: signed out, and the message says where it looked.
        let none = store.read_with(Some(&empty)).unwrap_err();
        assert!(none.signed_out());
        let none = none.to_string();
        assert!(none.contains("sign in with `claude` first"), "{none}");
        assert!(none.contains("no Keychain item `Claude Code-credentials`"), "{none}");
        assert!(none.contains(&store.file.display().to_string()), "{none}");
        let no_keychain = store.read_with(None).unwrap_err();
        assert!(no_keychain.signed_out());
        assert!(!no_keychain.to_string().contains("Keychain"), "{no_keychain}");

        // A Keychain that couldn't answer is NOT a signed-out machine: sending
        // that person to `/login` again sends them to the wrong fix.
        let unread = store.read_with(Some(&locked)).unwrap_err();
        assert!(!unread.signed_out());
        let unread = unread.to_string();
        assert!(unread.starts_with("couldn't read the Claude Code login"), "{unread}");
        assert!(unread.contains("is it locked?") && !unread.contains("sign in"), "{unread}");

        // Keychain only — every Mac signed in with `/login`.
        assert_eq!(store.read_with(Some(&held)).unwrap(), "from-keychain");

        // File only — Linux/Windows, or a Mac whose Keychain write failed.
        std::fs::write(&store.file, "from-file").unwrap();
        assert_eq!(store.read_with(Some(&empty)).unwrap(), "from-file");
        assert_eq!(store.read_with(Some(&locked)).unwrap(), "from-file");
        assert_eq!(store.read_with(None).unwrap(), "from-file");

        // Both: the Keychain is the one Claude Code runs on.
        assert_eq!(store.read_with(Some(&held)).unwrap(), "from-keychain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real `security`, asked for an item nobody has: "not there", never
    /// an empty blob that reads as a login. (`Err` is allowed for a machine
    /// with no login Keychain at all, a CI runner.)
    #[cfg(target_os = "macos")]
    #[test]
    fn an_absent_keychain_item_reads_as_not_there() {
        let got = keychain_password(&format!("mafold-test-absent-{}", std::process::id()));
        println!("absent item → {got:?}");
        assert!(!matches!(got, Ok(Some(_))), "{got:?}");
    }

    /// The wiring the whole fix rests on: a Mac asks its Keychain, nothing
    /// else has one to ask. A `None` here on macOS is fei's error on every Mac.
    #[test]
    fn only_a_mac_has_a_keychain_to_ask() {
        assert_eq!(keychain().is_some(), cfg!(target_os = "macos"));
    }

    /// `CLAUDE_CONFIG_DIR` moves the login along with the rest of the config.
    /// Claude Code 2.1.294 (`uw()`/`FF()`): with no storage override the file
    /// is `<config>/.credentials.json`, and a SET config dir suffixes the
    /// Keychain item by the same `sha256(dir)[..8]` a seat uses — hashed as
    /// given (`we()` = the variable, NFC), not resolved, no `~` expansion.
    #[test]
    fn claude_config_dir_moves_the_login_and_renames_its_item() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            LoginStore::resolve(home, None, Some("/tmp/cc-acct-probe")),
            LoginStore {
                file: "/tmp/cc-acct-probe/.credentials.json".into(),
                keychain_service: "Claude Code-credentials-94718db4".into(),
            }
        );
        assert_eq!(
            Account::default_login().store_in(home, Some("/tmp/cc-acct-probe")),
            LoginStore::resolve(home, None, Some("/tmp/cc-acct-probe"))
        );
        // A seat's own variable wins outright: its login is its own whatever
        // the config dir says.
        let seat = Account { name: "w".into(), dir: Some("/tmp/seat".into()), email: None, added_at: 0 };
        assert_eq!(seat.store_in(home, Some("/tmp/cc-acct-probe")), LoginStore::resolve(home, Some("/tmp/seat"), None));
        assert_eq!(seat.store_in(home, Some("/tmp/cc-acct-probe")).file, Path::new("/tmp/seat/.credentials.json"));
    }

    /// Empty is not unset: an empty STORAGE dir is "no suffix" and `~/.claude`
    /// even when a config dir is set — the storage variable, once present,
    /// decides alone. An empty config dir is "no suffix" too; its file is
    /// `~/.claude`'s here where Claude Code would look in its working
    /// directory (the one departure, see [`LoginStore::resolve`]).
    #[test]
    fn empty_variables_mean_what_claude_code_says() {
        let home = Path::new("/Users/someone");
        let default = LoginStore::default_in(home);
        assert_eq!(LoginStore::resolve(home, None, None), default);
        assert_eq!(LoginStore::resolve(home, None, Some("")), default);
        assert_eq!(LoginStore::resolve(home, Some(""), Some("/tmp/cfg")), default);
    }

    /// Claude Code hashes the NFC form, so a directory spelled decomposed (as
    /// a path pasted out of Finder often is) names the same item.
    #[test]
    fn the_directory_is_hashed_as_claude_code_normalises_it() {
        let home = Path::new("/Users/someone");
        let (composed, decomposed) = ("/tmp/caf\u{e9}", "/tmp/cafe\u{301}");
        assert_ne!(composed, decomposed);
        assert_eq!(LoginStore::resolve(home, None, Some(decomposed)), LoginStore::resolve(home, None, Some(composed)));
        assert_eq!(LoginStore::resolve(home, Some(decomposed), None), LoginStore::resolve(home, Some(composed), None));
        // And it is NFC that both land on (NFD would agree with itself too).
        assert_eq!(
            LoginStore::resolve(home, None, Some(decomposed)).keychain_service,
            format!("Claude Code-credentials-{}", hash8(composed))
        );
    }

    /// `from_env` reads an empty storage variable as "the default seat", and
    /// for picking a SEAT that is right — but to Claude Code a present-empty
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` still decides alone: `~/.claude` and
    /// the bare item, whatever `CLAUDE_CONFIG_DIR` says.
    #[test]
    fn an_empty_storage_variable_decides_alone_even_beside_a_config_dir() {
        let env = vec![
            (ENV.to_string(), String::new()),
            (CONFIG_DIR_ENV.to_string(), "/tmp/cfg".to_string()),
        ];
        let acct = Account::from_env_inheriting(&env, None);
        assert!(acct.is_default());
        assert_eq!(acct.store_for(&env), LoginStore::default_in(&super::home()));
        // Without the storage variable the config dir does move it.
        let env = vec![(CONFIG_DIR_ENV.to_string(), "/tmp/cfg".to_string())];
        assert_eq!(Account::default_login().store_for(&env), LoginStore::resolve(&super::home(), None, Some("/tmp/cfg")));
    }

    /// A blank (all-space) storage variable is, to Claude Code, a directory
    /// name like any other — hashed as given — even though `from_env` reads
    /// it as the default seat.
    #[test]
    fn a_blank_storage_variable_is_a_directory_name_to_claude_code() {
        let env = vec![(ENV.to_string(), " ".to_string()), (CONFIG_DIR_ENV.to_string(), "/tmp/cfg".to_string())];
        let acct = Account::from_env_inheriting(&env, None);
        assert!(acct.is_default());
        assert_eq!(acct.store_for(&env), LoginStore::resolve(&super::home(), Some(" "), None));
        assert_eq!(acct.store_for(&env).keychain_service, format!("Claude Code-credentials-{}", hash8(" ")));
    }

    /// `security -w` prints a password that isn't plain printable text as
    /// hex. Claude Code's store is a JSON object, so hex that decodes to one
    /// is read as that; anything else is left exactly as it came.
    #[test]
    fn hex_from_security_is_read_as_the_json_it_encodes() {
        let json = r#"{"mcpOAuth":{"文档|x":{"accessToken":"t"}}}"#;
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(unhex(&hex), json);
        assert_eq!(unhex(json), json);
        for not_json in ["deadbeef", "abc", "", "7b"] {
            assert_eq!(unhex(not_json), not_json);
        }
    }

    /// The readers that ask over and over — the sweep, the usage readout —
    /// go through the memory; the one a person is waiting on does not.
    #[cfg(target_os = "macos")]
    #[test]
    fn repeat_readers_go_through_the_memory_and_a_person_does_not() {
        let service = format!("mafold-test-remembered-{}", std::process::id());
        UNANSWERED
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .record(&service, Instant::now(), &Err("remembered".into()));
        assert_eq!(keychain().unwrap()(&service), Err("remembered".to_string()));
        assert!(!matches!(keychain_now().unwrap()(&service), Err(why) if why == "remembered"));
    }

    /// A Keychain that won't answer (locked: every ask is a 2s wait for a
    /// dialog nobody is looking at) is not asked again for a while — the
    /// usage readout and the credential sweep ask on every turn and every few
    /// seconds. "Not there" and a real answer are never remembered: a fresh
    /// `/login` must be seen on the next read.
    #[test]
    fn a_keychain_that_wont_answer_is_not_asked_again_for_a_while() {
        let t0 = Instant::now();
        let mut memo = Unanswered::default();
        let locked: Result<Option<String>, String> = Err("no answer from the Keychain in 2s — is it locked?".into());
        memo.record("svc", t0, &locked);
        assert_eq!(memo.recent("svc", t0 + std::time::Duration::from_secs(30)).as_deref(), locked.as_ref().err().map(String::as_str));
        assert_eq!(memo.recent("other", t0 + std::time::Duration::from_secs(30)), None);
        assert_eq!(memo.recent("svc", t0 + KEYCHAIN_BACKOFF + std::time::Duration::from_secs(1)), None);

        memo.record("svc", t0, &locked);
        memo.record("svc", t0 + std::time::Duration::from_secs(1), &Ok(None));
        assert_eq!(memo.recent("svc", t0 + std::time::Duration::from_secs(2)), None);
        memo.record("svc", t0, &locked);
        memo.record("svc", t0 + std::time::Duration::from_secs(1), &Ok(Some("blob".into())));
        assert_eq!(memo.recent("svc", t0 + std::time::Duration::from_secs(2)), None);
    }

    /// The whole premise: a seat moves the CREDENTIAL and nothing else. The
    /// credential file/Keychain item is per-account, while the transcripts,
    /// the auto-memory beside them, skills and settings all stay in
    /// `~/.claude` — which is why `--resume` carries a conversation across an
    /// account switch. (Verified live on 2026-09-06: `claude auth status
    /// --json` under a non-default seat reports `loggedIn:false` for that
    /// seat while still reporting `projectsDirectory: ~/.claude/projects`.)
    #[test]
    fn a_seat_moves_the_credential_and_nothing_else() {
        let a = Account { name: "work".into(), dir: Some("/tmp/seat".into()), email: None, added_at: 0 };
        assert_eq!(a.store_in(Path::new("/h"), None).file, Path::new("/tmp/seat/.credentials.json"));
        assert_eq!(a.env(), vec![(ENV.to_string(), "/tmp/seat".to_string())]);
        // The only variable a seat sets. `CLAUDE_CONFIG_DIR` would isolate
        // sessions, memory and skills too — the one thing this must never do.
        assert_eq!(a.env().len(), 1);
        assert!(!a.env().iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        assert_eq!(Account::default_login().store_in(Path::new("/h"), None).file, Path::new("/h/.claude/.credentials.json"));
    }

    /// The default account carries no env: Claude Code reads an unset (or
    /// empty) variable as its own `~/.claude` login, and only a non-empty
    /// directory names a different one — so "default" is spelled by saying
    /// nothing, never by setting the variable to something.
    #[test]
    fn default_account_carries_no_env() {
        assert!(Account::default_login().env().is_empty());
        let a = Account { name: "x".into(), dir: Some("/tmp/x".into()), email: None, added_at: 0 };
        assert_eq!(a.env(), vec![(ENV.to_string(), "/tmp/x".to_string())]);
        // The process env is stated, never read: inside a daemon turn it
        // carries that turn's seat, and this test used to fail there.
        let bare = || None::<String>;
        assert_eq!(Account::from_env_inheriting(&a.env(), bare()).dir.as_deref(), Some("/tmp/x"));
        assert!(Account::from_env_inheriting(&[], bare()).is_default());
        assert!(Account::from_env_inheriting(&[(ENV.into(), "  ".into())], bare()).is_default());
    }

    /// A daemon pinned to one login by its own environment: a turn with no
    /// seat of its own inherits that login — but a turn that names one, even
    /// as blank, is not overruled by it.
    #[test]
    fn an_empty_turn_env_inherits_the_process_login() {
        let pinned = || Some("/tmp/pinned-seat".to_string());
        assert_eq!(
            Account::from_env_inheriting(&[], pinned()).dir.as_deref(),
            Some("/tmp/pinned-seat")
        );
        assert_eq!(
            Account::from_env_inheriting(&[(ENV.into(), "/tmp/x".into())], pinned()).dir.as_deref(),
            Some("/tmp/x")
        );
        assert!(Account::from_env_inheriting(&[(ENV.into(), " ".into())], pinned()).is_default());
    }

    #[test]
    fn names_are_directory_env_and_option_safe() {
        for ok in ["default", "acct2", "work-max", "a.b_c", "x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "Acct", "-x", ".hidden", "a b", "a/b", "über", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_missing_registry_is_just_the_default_login() {
        let r = load_from(&tmp("missing-nope"));
        assert_eq!(names(&r), vec!["default"]);
        assert!(r.exhausted.is_empty());
    }

    #[test]
    fn registry_roundtrips_and_keeps_default_first() {
        let p = tmp("roundtrip");
        let mut r = Registry::default();
        r.normalize();
        r.accounts.push(Account { name: "b".into(), dir: Some("/tmp/b".into()), email: Some("b@x".into()), added_at: 5 });
        r.accounts.push(Account { name: "a".into(), dir: Some("/tmp/a".into()), email: None, added_at: 6 });
        r.mark_exhausted("b", "five_hour", 4_000_000_000, None, None);
        save_to(&p, &r).unwrap();
        let back = load_from(&p);
        assert_eq!(names(&back), vec!["default", "b", "a"], "registry order is selection order");
        assert_eq!(back.exhausted["b"].kind, "five_hour");
        assert_eq!(back.get("b").unwrap().email.as_deref(), Some("b@x"));
        let _ = std::fs::remove_file(&p);
    }

    /// Hand edits and older files: a second `default`, a row without a dir, a
    /// duplicate — none of them may become a seat a turn tries to run on.
    #[test]
    fn normalize_drops_malformed_rows() {
        let mut r = Registry {
            accounts: vec![
                Account { name: "default".into(), dir: Some("/nope".into()), email: None, added_at: 0 },
                Account { name: "nodir".into(), dir: None, email: None, added_at: 0 },
                Account { name: "a".into(), dir: Some("/tmp/a".into()), email: None, added_at: 0 },
                Account { name: "a".into(), dir: Some("/tmp/a2".into()), email: None, added_at: 0 },
                Account { name: "Bad Name".into(), dir: Some("/tmp/bad".into()), email: None, added_at: 0 },
            ],
            ..Default::default()
        };
        r.normalize();
        assert_eq!(names(&r), vec!["default", "a"]);
        assert!(r.get("default").unwrap().is_default(), "default is never a directory");
        assert_eq!(r.get("a").unwrap().dir.as_deref(), Some("/tmp/a"), "first wins");
    }

    #[test]
    fn ordered_puts_the_preferred_seat_first_then_registry_order() {
        let mut r = Registry::default();
        r.normalize();
        for n in ["a", "b", "c"] {
            r.accounts.push(Account { name: n.into(), dir: Some(format!("/tmp/{n}")), email: None, added_at: 0 });
        }
        let names = |v: Vec<Account>| v.into_iter().map(|a| a.name).collect::<Vec<_>>();
        assert_eq!(names(r.ordered(Some("b"))), vec!["b", "default", "a", "c"]);
        assert_eq!(names(r.ordered(None)), vec!["default", "a", "b", "c"]);
        assert_eq!(names(r.ordered(Some("zzz"))), vec!["default", "a", "b", "c"], "unknown preference is ignored");
    }

    #[test]
    fn exhaustion_expires_at_its_reset_time() {
        let mut r = Registry::default();
        r.normalize();
        r.mark_exhausted("default", "seven_day", 1_000, None, None);
        assert!(r.exhausted_at("default", 999).is_some());
        assert!(r.exhausted_at("default", 1_000).is_none(), "the reset moment itself is usable");
        assert!(r.prune(1_000));
        assert!(r.exhausted.is_empty());
        assert!(!r.remove("default"), "the default login can't be forgotten");
    }

    #[test]
    fn usage_limit_text_yields_its_reset_epoch() {
        assert_eq!(usage_limit_reset("Claude usage limit reached|1785900000"), Some(Some(1785900000)));
        assert_eq!(
            usage_limit_reset("claude exited unsuccessfully: Claude usage limit reached|1785900000"),
            Some(Some(1785900000))
        );
        assert_eq!(usage_limit_reset("You've hit your usage limit for this window"), Some(None));
        // The 2026-09-06 field shape: no "usage", no epoch — still the wall.
        assert_eq!(usage_limit_reset("You've hit your session limit · resets 6:30pm (Asia/Shanghai)"), Some(None));
        assert_eq!(usage_limit_reset("API Error: 429 rate_limit_error"), Some(None));
        assert_eq!(usage_limit_reset("No conversation found with session ID"), None);
        assert_eq!(usage_limit_reset("working directory does not exist"), None);
    }

    /// The live payload shape (captured 2026-09-06): `limits[]` with percent /
    /// resets_at ISO / kind, plus `extra_usage`. 100% with extra usage off is
    /// a wall; 100% with extra usage paying is not.
    #[test]
    fn a_full_window_is_a_wall_unless_extra_usage_covers_it() {
        let now = 1_788_000_000;
        let payload = |pct: f64, extra: bool| {
            serde_json::json!({
                "extra_usage": { "is_enabled": extra, "spend_limit_reached": false },
                "limits": [
                    { "kind": "session", "percent": pct, "resets_at": "2026-09-06T10:29:59.853207+00:00" },
                    { "kind": "weekly_all", "percent": 47, "resets_at": "2026-09-11T12:59:59.853228+00:00" }
                ]
            })
        };
        let s = snapshot_from_usage(&payload(90.0, false), now);
        assert_eq!(s.state, SeatState::Ok);
        assert_eq!(s.worst.as_ref().map(|w| (w.0.as_str(), w.1)), Some(("session", 90.0)));
        assert_eq!(s.blocks(Some("opus")), None);

        let s = snapshot_from_usage(&payload(100.0, false), now);
        let SeatState::Exhausted { kind, until } = s.state.clone() else { panic!("{:?}", s.state) };
        assert_eq!(kind, "session");
        // 2026-09-06T10:29:59Z — the window's own reset time, not a guess.
        assert_eq!(until, crate::commands::iso_epoch_secs("2026-09-06T10:29:59.853207+00:00").unwrap());
        // Account-wide ⇒ it stops every model, and a turn with no model set.
        assert!(s.blocks(Some("opus")).is_some());
        assert!(s.blocks(None).is_some());

        assert_eq!(snapshot_from_usage(&payload(100.0, true), now).state, SeatState::Ok);
    }

    /// The state this machine was ACTUALLY in on 2026-09-06, verbatim off
    /// `/api/oauth/usage`: `weekly_scoped` full for Fable while `session`
    /// (29%) and `weekly_all` (54%) are fine. Calling that an exhausted
    /// ACCOUNT would hand every Sonnet/Opus/Haiku turn to another login for
    /// five days — so a scoped window walls only its own model.
    #[test]
    fn a_full_window_scoped_to_one_model_does_not_exhaust_the_account() {
        let now = 1_788_600_000;
        let v = serde_json::json!({
            "extra_usage": { "is_enabled": false, "spend_limit_reached": false },
            "limits": [
                { "kind": "session", "group": "session", "percent": 29, "severity": "normal",
                  "resets_at": "2026-09-06T15:29:59.640541+00:00", "scope": null, "is_active": false },
                { "kind": "weekly_all", "group": "weekly", "percent": 54, "severity": "normal",
                  "resets_at": "2026-09-11T12:59:59.640566+00:00", "scope": null, "is_active": false },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 100, "severity": "critical",
                  "resets_at": "2026-09-11T12:59:59.640811+00:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" } }, "is_active": true }
            ]
        });
        let s = snapshot_from_usage(&v, now);
        assert_eq!(s.state, SeatState::Ok, "the ACCOUNT is not out — one model is");
        assert_eq!(s.walls.len(), 1);
        assert_eq!(s.walls[0].scope.as_deref(), Some("fable"));

        assert!(s.blocks(Some("fable")).is_some(), "the model that is out");
        assert!(s.blocks(Some("claude-fable-5-1")).is_some(), "…by full slug too");
        assert_eq!(s.blocks(Some("sonnet")), None, "every other model still runs here");
        assert_eq!(s.blocks(Some("opus")), None);
        // No model pinned = the harness's own default, which we can't know.
        // Letting it try costs one refusal (the runtime failover catches it);
        // guessing would give away a seat that is fine.
        assert_eq!(s.blocks(None), None);
        assert!(s.describe(now).starts_with("ok · fable out"), "{}", s.describe(now));
    }

    /// Both kinds full at once: the account-wide one is the verdict AND the
    /// reason, because it is the one that explains every refusal.
    #[test]
    fn an_account_wide_wall_outranks_a_scoped_one() {
        let now = 1_788_600_000;
        let v = serde_json::json!({ "limits": [
            { "kind": "weekly_scoped", "percent": 100, "resets_at": "2026-09-11T12:59:59Z",
              "scope": { "model": { "display_name": "Fable" } } },
            { "kind": "session", "percent": 100, "resets_at": "2026-09-06T15:29:59Z", "scope": null }
        ]});
        let s = snapshot_from_usage(&v, now);
        assert!(matches!(&s.state, SeatState::Exhausted { kind, .. } if kind == "session"));
        assert_eq!(s.blocks(Some("sonnet")).map(|w| w.kind.as_str()), Some("session"));
    }

    /// A window that names its model in its KIND (`seven_day_opus` — the
    /// shape the refusal event reports, where there is no `scope` object).
    #[test]
    fn a_model_scoped_kind_is_read_out_of_its_name() {
        assert_eq!(model_in_kind("seven_day_opus").as_deref(), Some("opus"));
        assert_eq!(model_in_kind("seven_day_sonnet").as_deref(), Some("sonnet"));
        assert_eq!(model_in_kind("seven_day"), None);
        assert_eq!(model_in_kind("five_hour"), None);
        assert_eq!(model_in_kind("weekly_all"), None);
        // …and a hold carrying it stops only that model.
        let x = Exhausted { until: 9, kind: "seven_day_opus".into(), at: 0, scope: Some("opus".into()), credential: None };
        assert!(x.stops(Some("opus")));
        assert!(!x.stops(Some("sonnet")));
        assert!(!x.stops(None));
        // A file written before scopes existed reads back as account-wide.
        let old: Exhausted = serde_json::from_str(r#"{"until":9,"kind":"seven_day","at":0}"#).unwrap();
        assert!(old.stops(Some("sonnet")) && old.stops(None));
    }

    /// Known reset boundaries end old evidence instead of extending the hold.
    #[test]
    fn a_past_reset_time_does_not_extend_the_hold() {
        let now = 4_000_000_000i64;
        let v = serde_json::json!({ "limits": [ { "kind": "session", "percent": 100, "resets_at": "2026-09-06T10:29:59Z" } ] });
        assert_eq!(snapshot_from_usage(&v, now).state, SeatState::Unknown);
        let no_reset = serde_json::json!({ "limits": [{ "kind": "session", "percent": 100 }] });
        let SeatState::Exhausted { until, .. } = snapshot_from_usage(&no_reset, now).state else { panic!() };
        assert_eq!(until, now + 3600, "missing reset evidence remains bounded");
    }

    /// The probe cache is process-global — one per daemon, which is the whole
    /// point of it — so the tests that seed `default` have to take turns or
    /// they read each other's answers.
    async fn cache_turn() -> tokio::sync::MutexGuard<'static, ()> {
        static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        L.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
    }

    /// A registry of `default` + the named seats, with a probe result already
    /// cached for each — the shape `first_usable` walks.
    fn seeded(seats: &[(&str, SeatSnapshot)]) -> Registry {
        let mut r = Registry::default();
        r.normalize();
        for (n, snap) in seats {
            if *n != DEFAULT {
                r.accounts.push(Account { name: (*n).into(), dir: Some(format!("/tmp/{n}")), email: None, added_at: 0 });
            }
            seed_seat(n, snap.clone());
        }
        r
    }

    fn full(kind: &str, until: i64, scope: Option<&str>) -> SeatSnapshot {
        let w = Wall { kind: kind.into(), until, scope: scope.map(str::to_string) };
        let state = match scope {
            None => SeatState::Exhausted { kind: kind.into(), until },
            Some(_) => SeatState::Ok,
        };
        SeatSnapshot { state, credential: None, worst: None, walls: vec![w] }
    }

    fn healthy() -> SeatSnapshot {
        SeatSnapshot { state: SeatState::Ok, credential: None, worst: None, walls: vec![] }
    }

    /// The whole point of the feature: the login a turn was aimed at is out,
    /// so the turn goes to the next one — and the registry remembers the wall
    /// so the next turn doesn't have to rediscover it.
    #[tokio::test]
    async fn a_full_seat_hands_the_turn_to_the_next_login() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[
            ("default", full("session", now + 3600, None)),
            ("acct-b", healthy()),
        ]);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, dirty) = first_usable(&mut reg, &order, now, Some("opus"), &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("acct-b"));
        assert!(dirty, "the wall it found is worth remembering");
        assert_eq!(reg.exhausted["default"].kind, "session");
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].detail.contains("exhausted (session)"), "{:?}", skipped);
    }

    /// A window scoped to ONE model doesn't move a turn running a different
    /// one — the seat stays where it is, and the hold is still recorded so a
    /// turn on the walled model can act on it.
    #[tokio::test]
    async fn a_model_scoped_wall_only_moves_that_models_turns() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[
            ("default", full("weekly_scoped", now + 5 * 86400, Some("fable"))),
            ("acct-c", healthy()),
        ]);
        let order = reg.ordered(None);

        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("sonnet"), &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("default"), "sonnet is fine here");
        assert!(skipped.is_empty());
        assert_eq!(reg.exhausted["default"].scope.as_deref(), Some("fable"), "the hold is still recorded");

        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("fable"), &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("acct-c"), "fable has to move");
        assert!(skipped[0].detail.contains("is out of fable"), "{:?}", skipped);
    }

    /// Every seat is proved blocked: no launch, and a reason for each seat.
    #[tokio::test]
    async fn when_every_seat_is_out_none_is_selected() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[
            ("default", full("session", now + 60, None)),
            ("acct-d", full("weekly_all", now + 60, None)),
        ]);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("opus"), &mut skipped).await;
        assert!(pick.is_none());
        assert_eq!(skipped.len(), 2, "and it can say why about each: {skipped:?}");
    }

    /// A remembered hold that the probe no longer sees is CLEARED, not
    /// obeyed: a transient refusal must not lock a login out for the length
    /// of the window it claimed.
    #[tokio::test]
    async fn a_hold_the_probe_contradicts_is_dropped() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[("default", healthy()), ("acct-e", healthy())]);
        reg.mark_exhausted("default", "session", now + 3600, None, None);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, dirty) = first_usable(&mut reg, &order, now, None, &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("default"));
        assert!(dirty && !reg.exhausted.contains_key("default"), "stale hold must be forgotten");
    }

    /// 2026-09-27, Muse: `default` hit its five-hour window while a second
    /// login on the machine sat at 0%, and the turn reported "no other Claude
    /// account on this machine could take over". That login just hadn't run
    /// for 8 hours, so its access token had lapsed — the refresh token was
    /// intact, and Claude Code renews on its next run. Read as "not logged
    /// in", it was skipped by every turn, so it never ran and never renewed.
    ///
    /// Probed for real here — through the credential file, not a seeded
    /// answer — because the bug lived in reading that file.
    #[tokio::test]
    async fn a_backup_login_idle_past_its_token_lifetime_still_takes_the_turn() {
        let _turn = cache_turn().await;
        let now = now();
        let dir = std::env::temp_dir().join(format!("mafold-idle-seat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lapsed_ms = (now - 9 * 3600) * 1000;
        let cred = serde_json::json!({ "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-lapsed", "refreshToken": "sk-ant-ort01-intact",
            "expiresAt": lapsed_ms, "scopes": ["user:inference", "user:profile"] } });
        std::fs::write(dir.join(".credentials.json"), cred.to_string()).unwrap();

        let mut reg = seeded(&[("default", full("five_hour", now + 1800, None))]);
        let idle = Account { name: "idle-backup".into(), dir: Some(dir.to_string_lossy().into_owned()), email: None, added_at: 0 };
        forget_seat(&idle.name);
        reg.accounts.push(idle);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("opus"), &mut skipped).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("idle-backup"), "passed over as: {skipped:?}");
    }

    /// 2026-10-05: `work` and `personal` passed their `refreshTokenExpiresAt`
    /// (10-05 00:35 and 10-04 00:51, UTC+8) with the refresh tokens still on
    /// file. Read as idle seats they stayed choosable; at 02:12 a run on them
    /// was refused ("OAuth session expired and could not be refreshed") and
    /// Claude Code wiped both logins. A seat nothing can renew is signed out
    /// — the turn goes to the next login, and the skip says why.
    ///
    /// Probed for real, through the credential file.
    #[tokio::test]
    async fn a_seat_whose_refresh_token_has_expired_is_not_chosen() {
        let _turn = cache_turn().await;
        let now = now();
        let dir = std::env::temp_dir().join(format!("mafold-expired-seat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred = serde_json::json!({ "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-lapsed", "refreshToken": "sk-ant-ort01-past-its-end",
            "expiresAt": (now - 9 * 3600) * 1000, "refreshTokenExpiresAt": (now - 3600) * 1000,
            "scopes": ["user:inference", "user:profile"] } });
        std::fs::write(dir.join(".credentials.json"), cred.to_string()).unwrap();

        let mut reg = seeded(&[("default", healthy())]);
        let expired = Account { name: "expired".into(), dir: Some(dir.to_string_lossy().into_owned()), email: None, added_at: 0 };
        forget_seat(&expired.name);
        reg.accounts.push(expired);
        let order = reg.ordered(Some("expired"));
        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, None, &mut skipped).await;
        let _ = std::fs::remove_dir_all(&dir);
        forget_seat("expired");
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("default"), "{skipped:?}");
        assert!(
            skipped.iter().any(|i| i.account == "expired"
                && i.detail.contains("isn't logged in")
                && i.detail.contains("sign-in expired 1h")
                && i.detail.contains("`/login expired`")),
            "{skipped:?}"
        );
    }

    /// What `/account` says about each seat's deadline: the time left, a
    /// warning inside three days, and when a signed-out seat ran out.
    #[test]
    fn a_listing_counts_down_to_the_sign_in_deadline() {
        let now = 1_790_450_000;
        assert_eq!(login_expiry_note("work", None, now), None);
        assert_eq!(
            login_expiry_note("work", Some(now + 27 * 86400 + 3 * 3600), now).as_deref(),
            Some("stored credential deadline in 27d 3h (not an authentication check)")
        );
        let soon = login_expiry_note("work", Some(now + 2 * 86400), now).unwrap();
        assert!(soon.starts_with("⚠️ stored credential deadline in 2d —") && soon.contains("/login work renews it"), "{soon}");
        let gone = login_expiry_note("work", Some(now - 5 * 3600), now).unwrap();
        assert!(gone.starts_with("⚠️ stored credential deadline passed 5h ago") && gone.contains("/login work"), "{gone}");
        // The default login is signed in again by a bare `/login`.
        assert!(login_expiry_note(DEFAULT, Some(now + 3600), now).unwrap().ends_with("— /login renews it"));
    }

    /// Idle is not a free pass: a wall the registry remembers for that seat
    /// still stops a turn on the walled model, exactly as for Unknown.
    #[tokio::test]
    async fn an_idle_seat_with_a_remembered_wall_is_still_passed_over() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[
            ("default", full("five_hour", now + 1800, None)),
            ("idle-held", SeatSnapshot { state: SeatState::Idle, credential: None, worst: None, walls: vec![] }),
        ]);
        reg.mark_exhausted("idle-held", "weekly_all", now + 86400, None, None);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("opus"), &mut skipped).await;
        assert!(pick.is_none(), "{skipped:?}");
        assert!(skipped[1].detail.contains("is exhausted (weekly_all)"), "{skipped:?}");
    }

    #[test]
    fn a_choice_explains_why_it_moved() {
        let c = Choice {
            account: Some(Account { name: "b".into(), dir: Some("/tmp/b".into()), email: None, added_at: 0 }),
            preferred: "default".into(),
            skipped: vec![AccountIssue::new("default", FailureKind::RateLimit, "is exhausted (five_hour) — resets in 1h")],
        };
        assert!(c.switched());
        let n = c.note().unwrap();
        assert!(n.contains("`b`") && n.contains("`default` is exhausted"), "{n}");
        let same = Choice { preferred: "b".into(), ..c };
        assert_eq!(same.note(), None);
    }

    /// An Idle seat (lapsed access token + refresh token) on disk, whose
    /// refresh token is `refresh` — the credential a refusal is pinned to.
    fn idle_on_disk(name: &str, refresh: &str) -> Account {
        let dir = std::env::temp_dir().join(format!("mafold-refused-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred = serde_json::json!({ "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-lapsed", "refreshToken": refresh,
            "expiresAt": (now() - 9 * 3600) * 1000 } });
        std::fs::write(dir.join(".credentials.json"), cred.to_string()).unwrap();
        Account { name: name.into(), dir: Some(dir.to_string_lossy().into_owned()), email: None, added_at: 0 }
    }

    /// The phrasings a refused sign-in comes back in (Claude Code 2.1.282),
    /// and the transient refresh race that must NOT count as one.
    #[test]
    fn a_refused_sign_in_is_told_apart_from_a_transient_refresh_race() {
        assert!(signed_out_error("API Error: 401 Invalid API key · Please run /login"));
        assert!(signed_out_error("Not logged in · Please run /login"));
        assert!(signed_out_error(r#"{"type":"error","error":{"type":"authentication_error","message":"..."}}"#));
        assert!(!signed_out_error(
            "Failed to refresh OAuth token: another Claude Code process is refreshing it or exited mid-refresh. \
             This is usually transient; retry in a minute, and if it persists close other Claude Code processes or sign in again"
        ));
        assert!(!signed_out_error("You've hit your session limit · resets 11:30am (America/Los_Angeles)"));
        assert!(!signed_out_error("API Error: 529 overloaded"));
    }

    /// The owner's rule (2026-09-27): a seat whose refresh token was revoked
    /// still looks renewable on disk, so once Anthropic refuses it the seat
    /// is marked signed out and later turns step over it instead of failing
    /// on it again — while the other seat takes the turn.
    #[tokio::test]
    async fn a_seat_whose_sign_in_was_refused_is_stepped_over_until_it_signs_in_again() {
        let _turn = cache_turn().await;
        let now = now();
        let refused = idle_on_disk("revoked", "sk-ant-ort01-revoked");
        let mut reg = seeded(&[("default", healthy())]);
        reg.accounts.push(refused.clone());
        forget_seat("revoked");
        let fp = crate::commands::credential_fingerprint(&refused.env());
        assert!(fp.is_some(), "the refused credential is fingerprinted");
        reg.mark_signed_out("revoked", fp);

        // Preferred, and on disk it looks like any idle backup — still passed over.
        let order = reg.ordered(Some("revoked"));
        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, None, &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("default"), "{skipped:?}");
        assert!(
            skipped.iter().any(|i| i.account == "revoked" && i.kind == FailureKind::Auth && i.detail.contains("refused")),
            "{skipped:?}"
        );

        // Signed in again (a new refresh token on file): the mark lifts and
        // the seat takes turns again.
        let dir = refused.dir.clone().unwrap();
        let fresh = serde_json::json!({ "claudeAiOauth": {
            "accessToken": "sk-ant-oat01-lapsed", "refreshToken": "sk-ant-ort01-new-login",
            "expiresAt": (now - 9 * 3600) * 1000 } });
        std::fs::write(std::path::Path::new(&dir).join(".credentials.json"), fresh.to_string()).unwrap();
        forget_seat("revoked");
        let mut skipped = vec![];
        let (pick, dirty) = first_usable(&mut reg, &order, now, None, &mut skipped).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("revoked"), "{skipped:?}");
        assert!(dirty && !reg.signed_out.contains_key("revoked"), "the stale mark is dropped and saved");
    }

    /// A live answer from the seat is proof it is signed in: the mark goes,
    /// whatever credential it was pinned to.
    #[tokio::test]
    async fn a_live_answer_clears_a_refusal() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[("default", full("five_hour", now + 1800, None)), ("back", healthy())]);
        reg.mark_signed_out("back", None);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, dirty) = first_usable(&mut reg, &order, now, None, &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("back"), "{skipped:?}");
        assert!(dirty && reg.signed_out.is_empty());
    }

    #[tokio::test]
    async fn mixed_blockers_return_every_reason_without_selecting_an_account() {
        let _turn = cache_turn().await;
        let now = now();
        let mut reg = seeded(&[
            ("default", full("seven_day", now + 3600, None)),
            ("work", full("seven_day", now + 7200, None)),
            ("personal", SeatSnapshot::bare(SeatState::AuthRefused)),
            ("locked", SeatSnapshot::bare(SeatState::CredentialUnreadable("login Keychain is locked".into()))),
        ]);
        let order = reg.ordered(None);
        let mut issues = Vec::new();
        let (account, _) = first_usable(&mut reg, &order, now, None, &mut issues).await;
        assert!(account.is_none());
        assert_eq!(issues.iter().map(|i| i.kind).collect::<Vec<_>>(),
            [FailureKind::RateLimit, FailureKind::RateLimit, FailureKind::Auth, FailureKind::Connection]);
        assert_eq!(issues[0].resets_at, Some(now + 3600));
        assert_eq!(issues[1].window.as_deref(), Some("seven_day"));
        assert!(!issues[3].detail.contains("/login"), "locked storage must not recommend login");
        assert!(Choice { account, preferred: DEFAULT.into(), skipped: issues }.note().is_none());
    }

    #[tokio::test]
    async fn failover_visits_each_unknown_or_idle_account_only_once() {
        let _turn = cache_turn().await;
        let mut reg = seeded(&[
            ("default", SeatSnapshot::bare(SeatState::Unknown)),
            ("backup", SeatSnapshot::bare(SeatState::Idle)),
            ("third", healthy()),
        ]);
        let mut tried = vec![DEFAULT.to_string()];
        for expected in ["backup", "third"] {
            let current = tried.last().unwrap().clone();
            let order = untried_accounts(&reg, &current, &tried);
            let (account, _) = first_usable(&mut reg, &order, now(), None, &mut Vec::new()).await;
            let account = account.unwrap();
            assert_eq!(account.name, expected);
            tried.push(account.name);
        }
        assert!(untried_accounts(&reg, "third", &tried).is_empty());
    }

    #[test]
    fn missing_refused_and_unreadable_credentials_have_distinct_verdicts() {
        use crate::commands::UtilizationProbe as P;
        assert_eq!(snapshot_from_probe(P::NoCredential, 0).state, SeatState::NoCredential);
        assert_eq!(snapshot_from_probe(P::Http(401), 0).state, SeatState::AuthRefused);
        let locked = snapshot_from_probe(P::Unreadable("Keychain locked".into()), 0);
        assert_eq!(locked.state, SeatState::CredentialUnreadable("Keychain locked".into()));
        assert!(locked.describe(0).contains("Keychain locked"));
        assert!(!locked.describe(0).contains("not logged in"));
    }

    #[test]
    fn a_failed_run_cannot_mark_a_replacement_login_signed_out() {
        let mut reg = Registry::default();
        remember_refusal(&mut reg, "work", Some("old-login"), Some("new-login"));
        assert!(reg.signed_out.is_empty());
        remember_refusal(&mut reg, "work", None, Some("new-login"));
        assert!(reg.signed_out.is_empty(), "unknown failed credentials cannot poison readable ones");
        remember_refusal(&mut reg, "work", Some("old-login"), Some("old-login"));
        assert!(reg.refused("work", Some("old-login")));
        assert!(!reg.refused("work", Some("new-login")));
        assert_eq!(runtime_scope("weekly_scoped", Some("Claude-Opus-5")), Some("claude-opus-5".into()));
        assert_eq!(runtime_scope("weekly_scoped", None), None);
        assert_eq!(runtime_scope("weekly_all", Some("opus")), None);
    }

    #[test]
    fn a_quota_hold_is_bound_to_the_login_that_hit_it() {
        let account = Account { name: "work".into(), dir: Some("/synthetic/work".into()), email: None, added_at: 0 };
        for state in [SeatState::Idle, SeatState::Unknown] {
            let mut reg = Registry::default();
            reg.accounts.push(account.clone());
            reg.mark_exhausted("work", "seven_day", 2000, None, Some("old-login".into()));
            let mut snap = SeatSnapshot::bare(state.clone());
            snap.credential = Some("new-login".into());
            assert!(reconcile_snapshot(&mut reg, &account, &mut snap, 1000));
            assert!(!reg.exhausted.contains_key("work"));
            assert_eq!(snap.state, state);
            assert!(snap.blocks(None).is_none());

            // Old registry files have no generation evidence to compare.
            reg.mark_exhausted("work", "seven_day", 2000, None, None);
            assert!(!reconcile_snapshot(&mut reg, &account, &mut snap, 1000));
            assert!(reg.exhausted.contains_key("work"));
            assert!(snap.blocks(None).is_some());
        }
        let mut reg = Registry::default();
        remember_quota(&mut reg, "work", "seven_day", 2000, None, Some("old-login"), Some("new-login"));
        assert!(reg.exhausted.is_empty(), "a late old run cannot block the new login");
        remember_quota(&mut reg, "work", "seven_day", 2000, None, None, Some("new-login"));
        assert!(reg.exhausted.is_empty(), "unknown failed generation is not persisted");
        remember_quota(&mut reg, "work", "seven_day", 2000, None, Some("same-login"), Some("same-login"));
        assert_eq!(reg.exhausted["work"].credential.as_deref(), Some("same-login"));
        let old: Exhausted = serde_json::from_str(r#"{"until":2000,"kind":"seven_day","at":0}"#).unwrap();
        assert!(old.credential.is_none());
    }

    #[test]
    fn malformed_usage_is_unknown_but_zero_percent_used_is_healthy() {
        let account = Account { name: "work".into(), dir: Some("/synthetic/work".into()), email: None, added_at: 0 };
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"limits": []}),
            serde_json::json!({"limits": [{"kind":"session"}]}),
            serde_json::json!({"limits": [{"kind":"session", "percent":"unknown"}]}),
            serde_json::json!({"limits": [{"kind":"session", "percent":-1}]}),
            serde_json::json!({"limits": [{"kind":"session", "percent":null}]}),
        ] {
            let mut snap = snapshot_from_usage(&payload, 1000);
            assert_eq!(snap.state, SeatState::Unknown, "{payload}");
            let mut reg = Registry::default();
            reg.accounts.push(account.clone());
            reg.mark_exhausted("work", "seven_day", 2000, None, Some("same-login".into()));
            snap.credential = Some("same-login".into());
            reconcile_snapshot(&mut reg, &account, &mut snap, 1000);
            assert!(snap.blocks(None).is_some(), "an invalid payload cannot clear a known wall");
        }
        let zero = snapshot_from_usage(&serde_json::json!({"limits":[{"kind":"session","percent":0}]}), 1000);
        assert_eq!(zero.state, SeatState::Ok);
        assert!(zero.describe(1000).contains("0% used / 100% remaining"));
        assert!(zero.blocks(None).is_none());
    }

    #[test]
    fn cached_probe_expires_on_credential_change_and_reset_boundary() {
        let instant = Instant::now();
        let now = 10_000;
        let cached = CachedSeat {
            at: instant, snapshot: full("week", now + 30, None), generation: Some("old-login".into()),
            marks: (None, None),
        };
        assert!(cached.fresh("old-login", instant, now));
        assert!(!cached.fresh("new-login", instant, now), "another daemon logged in");
        assert!(!cached.fresh("old-login", instant, now + 30), "reset itself invalidates cache");
        assert!(!cached.fresh("old-login", instant + FRESH, now), "TTL itself invalidates cache");
    }

    #[test]
    fn registry_updates_preserve_corrupt_or_unreadable_input() {
        let corrupt = tmp("corrupt-write-preserved");
        let raw = "{broken-but-must-survive";
        std::fs::write(&corrupt, raw).unwrap();
        let mut called = false;
        let error = update_registry_at(&corrupt, |_| { called = true; }).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!called, "no mutation may run on a replacement default registry");
        assert_eq!(std::fs::read_to_string(&corrupt).unwrap(), raw);
        std::fs::remove_file(&corrupt).unwrap();
        let _ = std::fs::remove_file(corrupt.with_extension("lock"));

        // A directory gives a deterministic read error on every platform,
        // unlike permission bits under administrator/root test runners.
        let unreadable = tmp("unreadable-write-preserved");
        std::fs::create_dir_all(&unreadable).unwrap();
        let mut called = false;
        assert!(update_registry_at(&unreadable, |_| { called = true; }).is_err());
        assert!(!called);
        assert!(unreadable.is_dir());
        std::fs::remove_dir(&unreadable).unwrap();
        let _ = std::fs::remove_file(unreadable.with_extension("lock"));
    }

    #[test]
    fn concurrent_registry_updates_preserve_every_account_mark() {
        let path = tmp("concurrent-writes");
        let _ = std::fs::remove_file(&path);
        let count = 12;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(count));
        let threads: Vec<_> = (0..count).map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                update_registry_at(&path, |reg| {
                    let name = format!("seat-{i}");
                    reg.accounts.push(Account { name: name.clone(), dir: Some(format!("/synthetic/{i}")), email: None, added_at: 0 });
                    reg.mark_exhausted(&name, "seven_day", 4_000_000_000, None, None);
                    reg.mark_signed_out(&name, Some(format!("fingerprint-{i}")));
                }).unwrap();
            })
        }).collect();
        for thread in threads { thread.join().unwrap(); }
        let reg = load_from(&path);
        assert_eq!(reg.accounts.len(), count + 1);
        assert_eq!(reg.exhausted.len(), count);
        assert_eq!(reg.signed_out.len(), count);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("lock"));
    }

    #[test]
    fn stale_probe_results_do_not_recreate_marks_for_a_removed_or_replaced_seat() {
        let mut before = Registry::default();
        before.accounts.push(Account {
            name: "work".into(), dir: Some("/synthetic/old-login".into()), email: None, added_at: 10,
        });
        let mut probe = before.clone();
        probe.mark_exhausted("work", "seven_day", 1000, None, None);
        probe.mark_signed_out("work", Some("old-credential".into()));

        let mut removed = before.clone();
        assert!(removed.remove("work"));
        merge_observations(&mut removed, &before, &probe);
        assert!(removed.exhausted.is_empty() && removed.signed_out.is_empty());

        let mut replaced = before.clone();
        replaced.accounts[0].dir = Some("/synthetic/new-login".into());
        merge_observations(&mut replaced, &before, &probe);
        assert!(replaced.exhausted.is_empty() && replaced.signed_out.is_empty());

        let mut readded = before.clone();
        readded.accounts[0].added_at = 11;
        merge_observations(&mut readded, &before, &probe);
        assert!(readded.exhausted.is_empty() && readded.signed_out.is_empty());

        let mut same_login = before.clone();
        same_login.accounts[0].email = Some("updated@example.test".into());
        merge_observations(&mut same_login, &before, &probe);
        assert_eq!(same_login.exhausted["work"], probe.exhausted["work"]);
        assert_eq!(same_login.signed_out["work"], probe.signed_out["work"]);
    }

    #[test]
    fn stale_probe_results_do_not_overwrite_a_concurrent_refusal_or_login() {
        let mut before = Registry::default();
        for name in ["a", "b"] {
            before.accounts.push(Account { name: name.into(), dir: Some(format!("/synthetic/{name}")), email: None, added_at: 0 });
        }
        let mut probe = before.clone();
        probe.mark_exhausted("a", "session", 200, None, None);
        let mut newer = before.clone();
        newer.mark_exhausted("a", "seven_day", 1000, None, None);
        newer.mark_signed_out("b", Some("rejected-credential".into()));
        merge_observations(&mut newer, &before, &probe);
        assert_eq!(newer.exhausted["a"].kind, "seven_day");
        assert!(newer.signed_out.contains_key("b"));

        let before = newer.clone();
        let mut probe = before.clone();
        probe.signed_out.remove("b");
        newer.mark_signed_out("b", Some("another-rejected-credential".into()));
        merge_observations(&mut newer, &before, &probe);
        assert_eq!(newer.signed_out["b"].credential.as_deref(), Some("another-rejected-credential"));
    }

    /// Files written before the field existed read back with no marks.
    #[test]
    fn a_registry_from_an_earlier_build_has_no_refusals() {
        let r: Registry = serde_json::from_str(r#"{"accounts":[],"exhausted":{}}"#).unwrap();
        assert!(r.signed_out.is_empty());
        assert!(!serde_json::to_string(&r).unwrap().contains("signed_out"), "empty marks aren't written");
    }
}
