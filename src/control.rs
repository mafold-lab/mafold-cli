//! What a command asks of the supervisor running on this machine — one kind
//! of request for every «supervisor, do this», in `~/.mafold/control/`
//! (activity center C7): restart a bot (`mafold restart <bot>`, the desktop
//! app's 重启), or restart the supervisor itself onto the binary on disk once
//! `mafold update` / `mafold rollback` has put a new one there.
//!
//! - `<id>.json` — a request, written aside and renamed in, so the supervisor
//!   never reads half of one.
//! - `<id>.taken` — the supervisor took it. Taking is a rename, so there is one
//!   taker, and a request withdrawn before it was taken is never acted on.
//! - `<id>.answer.json` — what came of it, for the command waiting on it.
//!
//! A supervisor that takes these says so in its run record
//! ([`crate::lifecycle::Run::control`]). One from before them would leave a
//! request lying there and the command waiting for nothing — so the command
//! looks first, and says the supervisor is too old instead.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a supervisor that takes requests writes in its run record. Raised
/// when a request it can't carry out the same way joins [`Action`].
pub const VERSION: u32 = 1;

/// A request older than this when the supervisor gets to it is answered
/// `expired` and not carried out: whoever asked has long given up waiting.
pub const EXPIRES: i64 = 60;

/// An answer nobody collected is cleared after this.
const ANSWER_KEPT: i64 = 600;

/// How often the supervisor looks for requests.
pub const POLL: Duration = Duration::from_millis(300);

/// What a command asks for.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Action {
    /// Stop the bot's daemon and start it again. The replies it was writing
    /// are cut off and picked back up by the new one (`agent::resume_turns`).
    RestartBot { name: String },
    /// Re-exec onto the binary on disk — `why` it changed — letting the
    /// replies in progress finish first (at most 5 minutes).
    RestartSupervisor {
        why: Moved,
        /// The version on disk, when the command knows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

/// Why the binary on disk is another one.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Moved {
    Update,
    Rollback,
}

/// What came of a request.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// The bot's daemon was stopped and a new one started.
    Restarted,
    /// The supervisor restarts itself as soon as the replies in progress are
    /// written — a re-exec can't answer once it has happened.
    Accepted,
    /// No bot of that name is configured here.
    UnknownBot,
    /// The daemon was still running after being asked to stop: left alone.
    StillRunning,
    /// It couldn't be done (`detail` says why).
    Failed,
    /// Too old by the time the supervisor got to it: not carried out.
    Expired,
    /// The supervisor that took it stopped before it could answer.
    Interrupted,
    /// A request this supervisor can't read — a newer mafold's.
    #[serde(other)]
    Unknown,
}

/// The answer to a request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Answer {
    pub id: String,
    pub code: Code,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The daemon that was running before (a bot restart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_pid: Option<u32>,
    /// The daemon that was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_pid: Option<u32>,
    /// Replies the bot was writing when it was stopped.
    #[serde(default)]
    pub interrupted: usize,
    /// Epoch seconds.
    pub at: i64,
}

/// What the supervisor's handler says; [`serve_once`] puts the id and time on it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reply {
    pub detail: Option<String>,
    pub old_pid: Option<u32>,
    pub new_pid: Option<u32>,
    pub interrupted: usize,
}

impl Reply {
    pub fn detail(d: impl Into<String>) -> Reply {
        Reply { detail: Some(d.into()), ..Reply::default() }
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    id: String,
    /// Epoch seconds.
    at: i64,
    #[serde(flatten)]
    action: Action,
}

/// What came of asking.
#[derive(Debug, Clone, PartialEq)]
pub enum Asked {
    /// The supervisor answered.
    Answered(Answer),
    /// Nobody took it in time. Withdrawn: nothing will come of it.
    Untaken,
    /// Taken, but not answered in time — it may still happen.
    Unanswered,
}

pub fn dir() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".mafold").join("control")
}

/// Ask the supervisor, and wait: up to `take` for it to take the request,
/// then up to `answer` for what came of it.
pub fn ask(action: Action, take: Duration, answer: Duration) -> Result<Asked> {
    ask_in(&dir(), action, take, answer)
}

pub(crate) fn ask_in(dir: &Path, action: Action, take: Duration, answer: Duration) -> Result<Asked> {
    let id = submit_in(dir, action, crate::accounts::now())?;
    let request = dir.join(format!("{id}.json"));
    let reply = answer_path(dir, &id);
    if !crate::platform::wait_until(take, || !request.exists()) {
        // Withdraw it — unless the supervisor took it this very moment (on
        // Windows a delete racing the rename fails, rather than finding it gone).
        match std::fs::remove_file(&request) {
            Ok(()) => return Ok(Asked::Untaken),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) if !request.exists() => {}
            Err(e) => return Err(e).with_context(|| format!("withdrawing {}", request.display())),
        }
    }
    let mut got = None;
    crate::platform::wait_until(answer, || {
        got = read_answer(&reply);
        got.is_some()
    });
    match got {
        Some(a) => {
            let _ = std::fs::remove_file(&reply);
            Ok(Asked::Answered(a))
        }
        None => Ok(Asked::Unanswered),
    }
}

/// Leave a request, fully written, and return its id.
pub(crate) fn submit_in(dir: &Path, action: Action, now: i64) -> Result<String> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("{now}-{}-{n}", std::process::id());
    let json = serde_json::to_string(&Request { id: id.clone(), at: now, action })?;
    let aside = dir.join(format!(".{id}.tmp"));
    std::fs::write(&aside, json).with_context(|| format!("writing {}", aside.display()))?;
    std::fs::rename(&aside, dir.join(format!("{id}.json"))).with_context(|| format!("placing the request in {}", dir.display()))?;
    Ok(id)
}

fn answer_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.answer.json"))
}

fn read_answer(path: &Path) -> Option<Answer> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Whole-file replace, like every record a reader polls for.
fn write_answer(dir: &Path, a: &Answer) {
    let Ok(json) = serde_json::to_string(a) else { return };
    let aside = dir.join(format!(".{}.answer.tmp", a.id));
    if std::fs::write(&aside, json).is_ok() && std::fs::rename(&aside, answer_path(dir, &a.id)).is_err() {
        let _ = std::fs::remove_file(&aside);
    }
}

/// The ids of the requests waiting: `<id>.json`, nothing else.
fn waiting(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.') && !n.ends_with(".answer.json"))
        .filter_map(|n| n.strip_suffix(".json").map(str::to_string))
        .collect();
    // Oldest first: the id begins with the time it was asked.
    ids.sort_by_key(|id| (id.split('-').next().and_then(|t| t.parse::<i64>().ok()).unwrap_or(0), id.clone()));
    ids
}

/// One pass over the requests waiting in `dir`: take each, carry it out
/// with `handle` (unless it is too old, or can't be read), answer it.
/// Answers left uncollected past [`ANSWER_KEPT`] are cleared.
pub fn serve_once(dir: &Path, now: i64, handle: &mut dyn FnMut(&Action) -> (Code, Reply)) {
    // Every request waiting is taken before any is carried out, and those
    // asked meanwhile as soon as each is done: one waiting behind a restart
    // that takes its full ten seconds to stop a bot would otherwise be
    // withdrawn by its asker as never taken.
    let take = || -> Vec<String> {
        waiting(dir)
            .into_iter()
            // Gone already: withdrawn, or another supervisor took it.
            .filter(|id| std::fs::rename(dir.join(format!("{id}.json")), dir.join(format!("{id}.taken"))).is_ok())
            .collect()
    };
    let mut queue: std::collections::VecDeque<String> = take().into();
    while let Some(id) = queue.pop_front() {
        let taken = dir.join(format!("{id}.taken"));
        let read: Option<Request> = std::fs::read_to_string(&taken).ok().and_then(|s| serde_json::from_str(&s).ok());
        let (code, reply) = match &read {
            None => (Code::Unknown, Reply::detail("a request this version of mafold can't read")),
            Some(r) if now - r.at > EXPIRES => (Code::Expired, Reply::default()),
            Some(r) => handle(&r.action),
        };
        write_answer(dir, &Answer {
            id: id.clone(),
            code,
            detail: reply.detail,
            old_pid: reply.old_pid,
            new_pid: reply.new_pid,
            interrupted: reply.interrupted,
            at: now,
        });
        let _ = std::fs::remove_file(&taken);
        queue.extend(take());
    }
    sweep(dir, now);
}

/// At supervisor start: a request the one before took and never answered —
/// it stopped mid-way — is answered as such, so nobody waits on it.
pub fn recover(dir: &Path, now: i64) {
    abandon(dir, now, |_| true);
}

/// A taken request no supervisor carries out any more — one that stopped
/// mid-way, or (Windows, where a re-exec starts the successor before the old
/// one exits) took it on its way out — is answered `interrupted`. Unless it
/// was answered: then only the taken file is left over.
fn abandon(dir: &Path, now: i64, left: impl Fn(&Path) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for name in entries.flatten().filter_map(|e| e.file_name().into_string().ok()) {
        let Some(id) = name.strip_suffix(".taken") else { continue };
        let taken = dir.join(&name);
        if !left(&taken) {
            continue;
        }
        if !answer_path(dir, id).exists() {
            write_answer(dir, &Answer {
                id: id.to_string(),
                code: Code::Interrupted,
                detail: Some("the supervisor stopped before it finished".into()),
                old_pid: None,
                new_pid: None,
                interrupted: 0,
                at: now,
            });
        }
        let _ = std::fs::remove_file(&taken);
    }
}

/// No request is carried out for longer than this — a bot restart is its
/// stop (≤10 s) and a start — so one taken longer ago was dropped.
const TAKEN_FOR: Duration = Duration::from_secs(120);

/// Clear answers nobody came for, and requests taken and dropped.
fn sweep(dir: &Path, now: i64) {
    abandon(dir, now, |taken| {
        // A rename keeps the file's time: when it was asked.
        std::fs::metadata(taken).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > TAKEN_FOR)
    });
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".answer.json")) {
            continue;
        }
        if read_answer(&p).is_none_or(|a| now - a.at > ANSWER_KEPT) {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mafold-control-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn restart(name: &str) -> Action {
        Action::RestartBot { name: name.into() }
    }

    /// The supervisor half on a thread of its own, the way `supervise` runs it.
    fn serving(dir: PathBuf, mut handle: impl FnMut(&Action) -> (Code, Reply) + Send + 'static) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::spawn(move || {
            while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
                serve_once(&dir, crate::accounts::now(), &mut handle);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        stop
    }

    #[test]
    fn a_request_is_taken_carried_out_and_answered() {
        let dir = tmp("answered");
        let stop = serving(dir.clone(), |a| match a {
            Action::RestartBot { name } if name == "a:bot" => {
                (Code::Restarted, Reply { old_pid: Some(10), new_pid: Some(11), interrupted: 2, ..Reply::default() })
            }
            _ => (Code::UnknownBot, Reply::default()),
        });
        let got = ask_in(&dir, restart("a:bot"), Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        let Asked::Answered(a) = got else { panic!("{got:?}") };
        assert_eq!((a.code, a.old_pid, a.new_pid, a.interrupted), (Code::Restarted, Some(10), Some(11), 2));
        let Asked::Answered(a) = ask_in(&dir, restart("a:other"), Duration::from_secs(5), Duration::from_secs(5)).unwrap() else { panic!() };
        assert_eq!(a.code, Code::UnknownBot);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(60));
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.is_empty(), "the asker took its answers, nothing lies about: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A supervisor from before these requests never takes one: the asker
    /// withdraws it, so it can't happen later out of the blue.
    #[test]
    fn a_request_nobody_takes_is_withdrawn() {
        let dir = tmp("untaken");
        let got = ask_in(&dir, restart("a:bot"), Duration::from_millis(300), Duration::from_secs(5)).unwrap();
        assert_eq!(got, Asked::Untaken);
        let mut ran = false;
        serve_once(&dir, crate::accounts::now(), &mut |_| {
            ran = true;
            (Code::Restarted, Reply::default())
        });
        assert!(!ran, "withdrawn means never carried out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Taken but never answered: the asker can't say it happened, or that it
    /// didn't.
    #[test]
    fn a_request_taken_and_not_answered_is_unanswered() {
        let dir = tmp("unanswered");
        let d = dir.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                for id in waiting(&d) {
                    let _ = std::fs::rename(d.join(format!("{id}.json")), d.join(format!("{id}.taken")));
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let got = ask_in(&dir, restart("a:bot"), Duration::from_secs(5), Duration::from_millis(300)).unwrap();
        assert_eq!(got, Asked::Unanswered);
        // The next supervisor answers what this one took and dropped.
        recover(&dir, 100);
        let answers: Vec<Answer> = std::fs::read_dir(&dir).unwrap().flatten().filter_map(|e| read_answer(&e.path())).collect();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].code, Code::Interrupted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_request_is_answered_expired_and_not_carried_out() {
        let dir = tmp("expired");
        let id = submit_in(&dir, restart("a:bot"), 1_000).unwrap();
        let mut ran = false;
        serve_once(&dir, 1_000 + EXPIRES + 1, &mut |_| {
            ran = true;
            (Code::Restarted, Reply::default())
        });
        assert!(!ran);
        assert_eq!(read_answer(&answer_path(&dir, &id)).map(|a| a.code), Some(Code::Expired));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A newer mafold's request: answered, so its asker doesn't wait for
    /// nothing, and not guessed at.
    #[test]
    fn a_request_this_version_cant_read_is_answered_unknown() {
        let dir = tmp("unknown");
        std::fs::write(dir.join("5-1-0.json"), r#"{"id":"5-1-0","at":5,"do":"pause_bot","name":"a:bot"}"#).unwrap();
        let mut ran = false;
        serve_once(&dir, 6, &mut |_| {
            ran = true;
            (Code::Restarted, Reply::default())
        });
        assert!(!ran);
        assert_eq!(read_answer(&answer_path(&dir, "5-1-0")).map(|a| a.code), Some(Code::Unknown));
        // …and an answer with a code a newer mafold wrote reads as unknown.
        let a: Answer = serde_json::from_str(r#"{"id":"x","code":"paused","at":1}"#).unwrap();
        assert_eq!(a.code, Code::Unknown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a command leaves is all the supervisor reads: files mid-write and
    /// answers are not requests.
    #[test]
    fn only_whole_requests_are_waiting() {
        let dir = tmp("waiting");
        let first = submit_in(&dir, restart("a:one"), 7).unwrap();
        let second = submit_in(&dir, Action::RestartSupervisor { why: Moved::Update, version: Some("0.9.200".into()) }, 9).unwrap();
        std::fs::write(dir.join(".8-1-0.tmp"), "{").unwrap();
        std::fs::write(dir.join("3-1-0.answer.json"), "{}").unwrap();
        std::fs::write(dir.join("4-1-0.taken"), "{}").unwrap();
        assert_eq!(waiting(&dir), vec![first, second.clone()], "oldest first, nothing else");
        let raw = std::fs::read_to_string(dir.join(format!("{second}.json"))).unwrap();
        assert!(raw.contains(r#""do":"restart_supervisor""#) && raw.contains(r#""why":"update""#), "{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One waiting behind a restart that takes its whole stop time is taken
    /// at once — not withdrawn by an asker that saw nobody take it.
    #[test]
    fn every_waiting_request_is_taken_before_any_is_carried_out() {
        let dir = tmp("all-taken");
        let first = submit_in(&dir, restart("a:slow"), 10).unwrap();
        let second = submit_in(&dir, restart("a:next"), 10).unwrap();
        let d = dir.clone();
        let mut second_taken = false;
        serve_once(&dir, 11, &mut |a| {
            if matches!(a, Action::RestartBot { name } if name == "a:slow") {
                second_taken = !d.join(format!("{second}.json")).exists() && d.join(format!("{second}.taken")).exists();
            }
            (Code::Restarted, Reply::default())
        });
        assert!(second_taken, "taken while the first was being carried out");
        for id in [first, second] {
            assert_eq!(read_answer(&answer_path(&dir, &id)).map(|a| a.code), Some(Code::Restarted));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request asked while another is being carried out is taken as soon
    /// as that one is done — in the same pass, not a poll later, which can be
    /// past the time its asker waits for it to be taken.
    #[test]
    fn a_request_asked_meanwhile_is_taken_in_the_same_pass() {
        let dir = tmp("meanwhile");
        let first = submit_in(&dir, restart("a:slow"), 10).unwrap();
        let d = dir.clone();
        let mut later = None;
        serve_once(&dir, 11, &mut |a| {
            if matches!(a, Action::RestartBot { name } if name == "a:slow") {
                later = Some(submit_in(&d, restart("a:next"), 11).unwrap());
            }
            (Code::Restarted, Reply::default())
        });
        let later = later.expect("asked while the first was carried out");
        for id in [first, later] {
            assert_eq!(read_answer(&answer_path(&dir, &id)).map(|a| a.code), Some(Code::Restarted));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request taken by a supervisor that never answered it — stopped
    /// mid-way, or one on its way out — is answered, once: an answer it did
    /// write is not overwritten.
    #[test]
    fn a_request_taken_and_dropped_is_answered_once() {
        let dir = tmp("dropped");
        let taken = |id: &str, age: u64| {
            let p = dir.join(format!("{id}.taken"));
            std::fs::write(&p, "{}").unwrap();
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(std::time::SystemTime::now() - Duration::from_secs(age)).unwrap();
        };
        taken("1-1-0", 600);
        taken("2-1-0", 1);
        serve_once(&dir, 1_000, &mut |_| (Code::Restarted, Reply::default()));
        assert_eq!(read_answer(&answer_path(&dir, "1-1-0")).map(|a| a.code), Some(Code::Interrupted), "long dropped");
        assert!(!dir.join("1-1-0.taken").exists());
        assert!(dir.join("2-1-0.taken").exists() && !answer_path(&dir, "2-1-0").exists(), "may still be under way elsewhere");
        // Answered, and then the supervisor stopped before it cleared its taken file.
        write_answer(&dir, &Answer { id: "2-1-0".into(), code: Code::Accepted, detail: None, old_pid: None, new_pid: None, interrupted: 0, at: 1_000 });
        recover(&dir, 1_001);
        assert_eq!(read_answer(&answer_path(&dir, "2-1-0")).map(|a| a.code), Some(Code::Accepted), "its answer stands");
        assert!(!dir.join("2-1-0.taken").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uncollected_answers_are_cleared_in_time() {
        let dir = tmp("sweep");
        let a = |id: &str, at: i64| Answer { id: id.into(), code: Code::Restarted, detail: None, old_pid: None, new_pid: None, interrupted: 0, at };
        write_answer(&dir, &a("old", 100));
        write_answer(&dir, &a("new", 100 + ANSWER_KEPT));
        serve_once(&dir, 101 + ANSWER_KEPT, &mut |_| (Code::Failed, Reply::default()));
        assert!(!answer_path(&dir, "old").exists());
        assert!(answer_path(&dir, "new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
