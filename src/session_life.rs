//! When a chat's agent session ends and the next turn starts a new one — and
//! what a session already knows, so it isn't told again every turn.
//!
//! The daemon used to `--resume` one session per surface forever, leaving the
//! context to grow until Claude Code compacted it near a million tokens.
//! Measured on 2026-10-20: the sessions answering in DEV carried 580k–775k
//! tokens, 89–95% of it old tool output; a person scrolling the same chat sees
//! each of those runs as one folded line. So a session now ends once its
//! context passes [`CONTEXT_LIMIT`] or it has sat idle for [`IDLE_SECS`] (the
//! prompt cache is long gone by then, so a new session costs nothing extra),
//! and the next turn starts fresh with the chat itself as its memory: the
//! history folded the way a person sees it, every line carrying its message id,
//! `mafold msg <id>` to open one (`agent::recent_group_context`, `seed`).
//!
//! The old session is not deleted — `/resume` can still go back to it.
//!
//! The second half is the per-turn blocks (installed apps, read tickets, an
//! untitled channel): prepended to every prompt, they piled up in a resumed
//! session one copy per turn — 6k characters of the same ticket list on each
//! of a channel's messages. A block now goes in when the session hasn't seen
//! that exact text yet.
//!
//! Both live in `~/.mafold/session_life.json`, keyed like `sessions.json`.
//! Lost or stale entries are harmless: no record means "keep resuming" and
//! "tell it again".

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Past this many tokens of context, the session's next turn starts a new one
/// (owner, 2026-10-20: 20万).
pub const CONTEXT_LIMIT: u64 = 200_000;
/// Idle this long, the same: a person picking a chat back up after two hours
/// reads it from the top, not from a cache that expired after one.
pub const IDLE_SECS: i64 = 2 * 3600;
/// A new session's history: up to this many messages…
pub const SEED_MESSAGES: usize = 200;
/// …within this many characters, newest kept (≈30k tokens — owner, 2026-10-20).
pub const SEED_CHARS: usize = 60_000;

/// What the daemon knows about the session a surface last ran on.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Life {
    /// The session this record is about. A surface now resuming another one
    /// (`/resume`, `/clear`, a fork) makes the record moot.
    pub session: String,
    /// Context size at the end of its last turn: the last model request's
    /// whole input. None when the harness doesn't report one.
    #[serde(default)]
    pub context: Option<u64>,
    /// When its last turn ended, unix seconds.
    pub at: i64,
    /// Per-turn blocks it has been given: name → hash of the exact text.
    #[serde(default)]
    pub blocks: HashMap<String, u64>,
}

/// Why a surface starts a new session.
#[derive(Debug, Clone, PartialEq)]
pub enum Why {
    Context(u64),
    Idle(i64),
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::Context(n) => write!(f, "context {n} tokens ≥ {CONTEXT_LIMIT}"),
            Why::Idle(s) => write!(f, "idle {}m", s / 60),
        }
    }
}

/// Should the turn about to resume `session` start a new one instead?
pub fn verdict_in(map: &HashMap<String, Life>, key: &str, session: &str, now: i64) -> Option<Why> {
    let life = map.get(key).filter(|l| l.session == session)?;
    if let Some(n) = life.context.filter(|n| *n >= CONTEXT_LIMIT) {
        return Some(Why::Context(n));
    }
    let idle = now - life.at;
    (idle >= IDLE_SECS).then_some(Why::Idle(idle))
}

/// A turn on `key` ended on `session`. Its context (when reported) and the
/// blocks it was given are added to what that session already had; a
/// different session than last time starts its record over.
pub fn record_in(
    map: &mut HashMap<String, Life>,
    key: &str,
    session: &str,
    context: Option<u64>,
    now: i64,
    blocks: HashMap<String, u64>,
) {
    let life = map.entry(key.to_string()).or_default();
    if life.session != session {
        *life = Life { session: session.to_string(), ..Default::default() };
    }
    if context.is_some() {
        life.context = context;
    }
    life.at = now;
    life.blocks.extend(blocks);
}

/// Has `session` on `key` already been given this exact block?
pub fn known_in(map: &HashMap<String, Life>, key: &str, session: Option<&str>, name: &str, hash: u64) -> bool {
    let Some(session) = session else { return false };
    map.get(key).is_some_and(|l| l.session == session && l.blocks.get(name) == Some(&hash))
}

pub fn hash_of(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── the daemon's copy, persisted ────────────────────────────────────────────

fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".mafold").join("session_life.json")
}

fn state() -> &'static Mutex<HashMap<String, Life>> {
    static STATE: OnceLock<Mutex<HashMap<String, Life>>> = OnceLock::new();
    STATE.get_or_init(|| {
        let map = std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Mutex::new(map)
    })
}

fn save(map: &HashMap<String, Life>) {
    let Ok(s) = serde_json::to_string(map) else { return };
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

pub fn verdict(key: &str, session: &str) -> Option<Why> {
    let map = state().lock().unwrap_or_else(|e| e.into_inner());
    verdict_in(&map, key, session, now())
}

pub fn record(key: &str, session: &str, context: Option<u64>, blocks: HashMap<String, u64>) {
    let mut map = state().lock().unwrap_or_else(|e| e.into_inner());
    record_in(&mut map, key, session, context, now(), blocks);
    save(&map);
}

/// The per-turn blocks of one turn: which went in, and what the session
/// already has. Built before the prompt; handed to [`record`] after.
pub struct Blocks {
    key: String,
    session: Option<String>,
    given: Mutex<HashMap<String, u64>>,
}

impl Blocks {
    /// `session` is the one this turn resumes — None for a new session, which
    /// is given every block.
    pub fn new(key: &str, session: Option<&str>) -> Self {
        Self { key: key.to_string(), session: session.map(str::to_string), given: Mutex::new(HashMap::new()) }
    }

    /// `block` ahead of `prompt` — unless the session was already given this
    /// exact text under `name`.
    pub fn prepend(&self, name: &str, block: String, prompt: String) -> String {
        let h = hash_of(&block);
        self.given.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string(), h);
        let known = {
            let map = state().lock().unwrap_or_else(|e| e.into_inner());
            known_in(&map, &self.key, self.session.as_deref(), name, h)
        };
        if known {
            prompt
        } else {
            format!("{block}\n\n{prompt}")
        }
    }

    pub fn into_given(self) -> HashMap<String, u64> {
        self.given.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with(context: Option<u64>, at: i64) -> HashMap<String, Life> {
        let mut m = HashMap::new();
        record_in(&mut m, "c1", "s1", context, at, HashMap::new());
        m
    }

    #[test]
    fn a_session_ends_past_the_context_limit() {
        let m = map_with(Some(CONTEXT_LIMIT), 1000);
        assert_eq!(verdict_in(&m, "c1", "s1", 1001), Some(Why::Context(CONTEXT_LIMIT)));
        let m = map_with(Some(CONTEXT_LIMIT - 1), 1000);
        assert_eq!(verdict_in(&m, "c1", "s1", 1001), None);
    }

    #[test]
    fn a_session_ends_after_two_idle_hours() {
        let m = map_with(Some(10), 1000);
        assert_eq!(verdict_in(&m, "c1", "s1", 1000 + IDLE_SECS - 1), None);
        assert_eq!(verdict_in(&m, "c1", "s1", 1000 + IDLE_SECS), Some(Why::Idle(IDLE_SECS)));
    }

    #[test]
    fn no_record_or_another_session_means_keep_resuming() {
        let m = map_with(Some(CONTEXT_LIMIT * 2), 0);
        assert_eq!(verdict_in(&m, "c2", "s1", 99_999), None);
        assert_eq!(verdict_in(&m, "c1", "s9", 99_999), None);
    }

    #[test]
    fn an_unreported_context_keeps_the_last_one_and_a_new_session_starts_over() {
        let mut m = map_with(Some(150_000), 0);
        record_in(&mut m, "c1", "s1", None, 10, HashMap::new());
        assert_eq!(m["c1"].context, Some(150_000));
        assert_eq!(m["c1"].at, 10);
        record_in(&mut m, "c1", "s2", None, 20, HashMap::new());
        assert_eq!(m["c1"], Life { session: "s2".into(), context: None, at: 20, blocks: HashMap::new() });
    }

    #[test]
    fn a_block_is_known_only_to_the_session_that_was_given_that_text() {
        let mut m = HashMap::new();
        let h = hash_of("[CHAT ACCESS …]");
        record_in(&mut m, "c1", "s1", None, 0, HashMap::from([("access".to_string(), h)]));
        assert!(known_in(&m, "c1", Some("s1"), "access", h));
        assert!(!known_in(&m, "c1", Some("s1"), "access", hash_of("[CHAT ACCESS changed]")));
        assert!(!known_in(&m, "c1", Some("s2"), "access", h), "another session");
        assert!(!known_in(&m, "c1", None, "access", h), "a new session is told everything");
    }
}
