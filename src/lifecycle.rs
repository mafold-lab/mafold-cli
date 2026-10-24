//! What the supervisor and its daemons leave on disk for `mafold status` — a
//! separate process — to read back (activity center C5):
//!
//! - `~/.mafold/supervisor.run.json` — the supervisor running now: since
//!   when, on which version, whether it updates itself, and the restarts of
//!   the last ten minutes that followed a crash.
//! - `~/.mafold/supervisor.exit.json` — why the one before it stopped.
//! - `~/.mafold/daemons/<name>/deaths` — when a daemon died without anyone
//!   stopping it, so a bot that falls over right after every start reads as
//!   crashing instead of as running. `stopping` beside it is the stop a
//!   command asked for, so that one isn't counted.
//! - `~/.mafold/daemons/<name>/turns.json` — the replies a daemon is writing,
//!   since when, and the runtime it actually runs.
//! - `~/.mafold/runtimes.json` — the runtimes as the supervisor's heartbeat
//!   last saw them, with the supervisor's PATH, not a caller's.
//!
//! A record names the process that wrote it, and a reader believes it only
//! while that process is the one running — the rule `supervisor.doing.json`
//! already follows. Everything here is best effort: a record that can't be
//! written or read is a fact `status` doesn't know, never a failure.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How far back restarts after a crash count towards a crash loop.
pub const CRASH_WINDOW: i64 = 600;

/// Restarts after a crash inside [`CRASH_WINDOW`] that make a crash loop.
pub const CRASH_LOOP: usize = 3;

/// Why the supervisor stopped.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// It re-exec'd into a new version.
    Update,
    /// `mafold down`, or `mafold up` replacing it.
    Stopped,
    /// The system or someone else stopped it (logout, shutdown, `kill`).
    Signal,
    /// It panicked.
    Crashed,
    /// It left no word, and the machine has booted since it started: it went
    /// down with the machine (shutdown, restart, power).
    Shutdown,
    /// It's gone and left no word: killed outright, or it died before it
    /// could say. Also whatever a newer mafold wrote that this one can't read.
    #[serde(other)]
    Unknown,
}

impl Why {
    /// Did it fall over, as opposed to being stopped or moved on?
    pub fn is_crash(self) -> bool {
        matches!(self, Why::Crashed | Why::Unknown)
    }
}

/// The supervisor running now.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Run {
    pub pid: u32,
    /// Epoch seconds. A unix update re-execs in place (same pid), and the new
    /// version writes its own record: this is "on this version since".
    pub started_at: i64,
    pub version: String,
    /// Whether it updates itself (`supervise` without `--no-auto-update`).
    pub auto_update: bool,
    /// The starts inside [`CRASH_WINDOW`] that followed a crash, epoch
    /// seconds — this one's included when it did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crashes: Vec<i64>,
    /// The requests it takes from `~/.mafold/control/` ([`crate::control::VERSION`]);
    /// 0 — left out — for a supervisor from before them, which a command
    /// asking it to restart a bot would wait on for nothing.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub control: u32,
    /// It reads the machine's update setting on every pass
    /// ([`crate::update::auto_update_on`]); false — left out — for one from
    /// before it, which updates whatever the setting says.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update_setting: bool,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Why a supervisor stopped.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Exit {
    pub pid: u32,
    pub reason: Why,
    /// Epoch seconds; none when nothing was there to note it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    /// The version that stopped.
    pub version: String,
    /// The version it moved to (`update`), the signal (`signal`), the panic
    /// (`crashed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

pub fn run_path() -> PathBuf {
    home().join(".mafold/supervisor.run.json")
}

pub fn exit_path() -> PathBuf {
    home().join(".mafold/supervisor.exit.json")
}

pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Whole-file replace (write aside, rename over), so a reader in another
/// process never sees half a file.
fn write<T: Serialize>(path: &Path, v: &T) {
    let Ok(json) = serde_json::to_string(v) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let aside = path.with_file_name(format!(".{name}.{}", std::process::id()));
    if std::fs::write(&aside, json).is_ok() && std::fs::rename(&aside, path).is_err() {
        let _ = std::fs::remove_file(&aside);
    }
}

/// At supervisor start: write down why the one before stopped and start this
/// run's record. Returns both — the exit is put back if an update's re-exec
/// fails and this process carries on ([`restore_exit`]).
pub fn begin(pid: u32, auto_update: bool) -> (Run, Option<Exit>) {
    let now = crate::accounts::now();
    let at = Clock { now, boot: crate::platform::boot_time() };
    begin_at(&run_path(), &exit_path(), pid, crate::update::current_version(), auto_update, at)
}

/// Now, and when this machine last booted (epoch seconds) — what tells a
/// supervisor that went down with the machine from one that fell over.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub now: i64,
    pub boot: Option<i64>,
}

pub(crate) fn begin_at(run_p: &Path, exit_p: &Path, pid: u32, version: &str, auto_update: bool, at: Clock) -> (Run, Option<Exit>) {
    let now = at.now;
    let prev: Option<Run> = read(run_p);
    let last = ended(prev.as_ref(), read(exit_p), at.boot);
    let mut crashes: Vec<i64> =
        prev.map(|r| r.crashes).unwrap_or_default().into_iter().filter(|t| now - t < CRASH_WINDOW).collect();
    if last.as_ref().is_some_and(|x| x.reason.is_crash()) {
        crashes.push(now);
    }
    let run = Run { pid, started_at: now, version: version.to_string(), auto_update, crashes, control: crate::control::VERSION, update_setting: true };
    if let Some(x) = &last {
        write(exit_p, x);
    }
    write(run_p, &run);
    (run, last)
}

/// How the run `run` ended, given the exit last noted: that note when it is
/// about this run; otherwise it went without a word — with the machine, when
/// the machine has booted since it started (`boot`). No run on record — a
/// supervisor from before these records — leaves the note as it is.
fn ended(run: Option<&Run>, noted: Option<Exit>, boot: Option<i64>) -> Option<Exit> {
    let Some(r) = run else { return noted };
    match noted {
        Some(x) if x.pid == r.pid && x.at.is_some_and(|at| at >= r.started_at) => Some(x),
        _ => {
            // Ten seconds of slack: Windows derives the boot time from the
            // uptime, a second or so either way, and a supervisor that
            // started right after this boot didn't go down with the last one.
            let reason = if boot.is_some_and(|b| b > r.started_at + 10) { Why::Shutdown } else { Why::Unknown };
            Some(Exit { pid: r.pid, reason, at: None, version: r.version.clone(), detail: None })
        }
    }
}

/// Note why the supervisor `pid` is stopping (or just stopped) — written by
/// that supervisor, or by the `down` / `up` that stopped it. The version is
/// its own when its run is on record.
pub fn note_exit(pid: u32, reason: Why, detail: Option<&str>) {
    note_exit_at(&run_path(), &exit_path(), pid, reason, detail, crate::accounts::now())
}

pub(crate) fn note_exit_at(run_p: &Path, exit_p: &Path, pid: u32, reason: Why, detail: Option<&str>, now: i64) {
    let version = read::<Run>(run_p)
        .filter(|r| r.pid == pid)
        .map(|r| r.version)
        .unwrap_or_else(|| crate::update::current_version().to_string());
    let detail = detail.map(|d| d.chars().take(300).collect());
    write(exit_p, &Exit { pid, reason, at: Some(now), version, detail });
}

/// An update's re-exec failed and this process carries on: the exit it noted
/// on the way out isn't true, the one before it is.
pub fn restore_exit(last: Option<&Exit>) {
    match last {
        Some(x) => write(&exit_path(), x),
        None => {
            let _ = std::fs::remove_file(exit_path());
        }
    }
}

/// What `status` says about the supervisor — its run, why the last one
/// stopped, and how many of the last ten minutes' starts came after a crash.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Service {
    /// The running supervisor's record; none when it isn't running or wrote
    /// none (a version from before these records).
    pub run: Option<Run>,
    /// Running: why the one before it stopped. Not running: why it stopped.
    pub last_exit: Option<Exit>,
    /// Starts after a crash inside [`CRASH_WINDOW`], the stop of a supervisor
    /// that is down now included when it fell over.
    pub crashes: Vec<i64>,
}

/// [`Service`] for the supervisor `running` (its pid, from the pid file).
pub fn service(running: Option<u32>) -> Service {
    let at = Clock { now: crate::accounts::now(), boot: crate::platform::boot_time() };
    service_at(&run_path(), &exit_path(), running, at)
}

pub(crate) fn service_at(run_p: &Path, exit_p: &Path, running: Option<u32>, at: Clock) -> Service {
    let now = at.now;
    let run: Option<Run> = read(run_p);
    let noted: Option<Exit> = read(exit_p);
    let recent = |r: &Run| r.crashes.iter().copied().filter(|t| now - t < CRASH_WINDOW).collect::<Vec<_>>();
    match running {
        Some(pid) => {
            let run = run.filter(|r| r.pid == pid);
            // The exit `begin` wrote down for this run: from before it started.
            let last_exit = run
                .as_ref()
                .and_then(|r| noted.filter(|x| x.at.is_none_or(|at| at <= r.started_at)));
            let crashes = run.as_ref().map(recent).unwrap_or_default();
            Service { run, last_exit, crashes }
        }
        None => {
            let last_exit = ended(run.as_ref(), noted, at.boot);
            let mut crashes = run.as_ref().map(recent).unwrap_or_default();
            // Down after falling over: that fall counts too, when it's recent
            // enough to say (an unnoted stop has no time — its run's start
            // stands in, the latest it can't have been before).
            if let (Some(r), Some(x)) = (&run, &last_exit) {
                let fell = x.at.unwrap_or(r.started_at);
                if x.reason.is_crash() && now - fell < CRASH_WINDOW {
                    crashes.push(fell);
                }
            }
            Service { run: None, last_exit, crashes }
        }
    }
}

// ── daemons ──

/// A command is about to stop the daemon `pid` on purpose (`mafold down
/// <bot>`, `rm`, `stop`): noted BEFORE the signal, because a bot that
/// restarts itself this way takes the command down with it.
pub fn note_stop(path: &Path, pid: u32) {
    let _ = std::fs::write(path, pid.to_string());
}

/// Was the daemon `pid` stopped on purpose? Takes the note.
pub fn take_stop(path: &Path, pid: u32) -> bool {
    let noted = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse::<u32>().ok());
    if noted.is_some() {
        let _ = std::fs::remove_file(path);
    }
    noted == Some(pid)
}

/// Note that a daemon died without anyone stopping it — one the supervisor
/// had seen running, gone with no stop asked for. Keeps the last few.
pub fn note_death(path: &Path, now: i64) {
    let mut deaths = read_deaths(path);
    deaths.push(now);
    let keep = deaths.len().saturating_sub(5);
    let text: String = deaths[keep..].iter().map(|t| format!("{t}\n")).collect();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

/// When a daemon died by itself lately, oldest first, epoch seconds.
pub fn read_deaths(path: &Path) -> Vec<i64> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// The replies a daemon is writing — `turns.json` next to its `busy` count
/// (which stays a bare number: an older supervisor's drain reads it).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Turns {
    /// The daemon that wrote it.
    pub pid: u32,
    /// Replies in flight.
    pub active: usize,
    /// When the longest-running of them began, epoch seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// The runtime it runs — what Mafold told it (`getMe`), resolved, not
    /// the hint the local config was written with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

pub fn write_turns(path: &Path, t: &Turns) {
    write(path, t)
}

/// The turns record of the daemon running as `pid`, if it wrote one.
pub fn read_turns(path: &Path, pid: u32) -> Option<Turns> {
    read::<Turns>(path).filter(|t| t.pid == pid)
}

// ── runtimes, as the supervisor sees them ──

/// The supervisor's last `harness::report_rows`. A caller of `mafold status`
/// — the desktop app, a terminal — may well have another PATH than the
/// supervisor that runs the bots, and probing with its own would call a
/// runtime the bots use every day missing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Runtimes {
    pub pid: u32,
    pub at: i64,
    pub rows: Vec<serde_json::Value>,
}

pub fn runtimes_path() -> PathBuf {
    home().join(".mafold/runtimes.json")
}

pub fn write_runtimes(path: &Path, rows: &serde_json::Value, now: i64) {
    let rows = rows.as_array().cloned().unwrap_or_default();
    write(path, &Runtimes { pid: std::process::id(), at: now, rows })
}

/// The rows the supervisor `running` reported within `fresh` seconds.
pub fn read_runtimes(path: &Path, running: u32, now: i64, fresh: i64) -> Option<Vec<serde_json::Value>> {
    read::<Runtimes>(path).filter(|r| r.pid == running && now - r.at <= fresh).map(|r| r.rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mafold-life-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.join("supervisor.run.json"), dir.join("supervisor.exit.json"))
    }

    fn done(p: &Path) {
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// On a machine that hasn't booted since any of this (no boot time).
    fn begin_at(r: &Path, x: &Path, pid: u32, version: &str, auto: bool, now: i64) -> (Run, Option<Exit>) {
        super::begin_at(r, x, pid, version, auto, Clock { now, boot: None })
    }

    fn service_at(r: &Path, x: &Path, running: Option<u32>, now: i64) -> Service {
        super::service_at(r, x, running, Clock { now, boot: None })
    }

    #[test]
    fn a_supervisor_that_went_down_with_the_machine_did_not_crash() {
        let (r, x) = tmp("boot");
        begin_at(&r, &x, 100, "0.9.150", true, 1_000);
        // Nothing noted, and the machine booted at 5_000: shutdown, restart, power.
        let after_boot = Clock { now: 5_060, boot: Some(5_000) };
        let s = super::service_at(&r, &x, None, after_boot);
        assert_eq!(s.last_exit.as_ref().map(|x| x.reason), Some(Why::Shutdown));
        assert!(s.crashes.is_empty());
        let (run, last) = super::begin_at(&r, &x, 200, "0.9.150", true, Clock { now: 5_100, boot: Some(5_000) });
        assert_eq!(last.map(|x| x.reason), Some(Why::Shutdown));
        assert!(run.crashes.is_empty(), "a reboot is no crash");
        // Gone a few seconds into THIS boot, still without a word: that's no shutdown.
        let s = super::service_at(&r, &x, None, Clock { now: 5_200, boot: Some(5_095) });
        assert_eq!(s.last_exit.map(|x| x.reason), Some(Why::Unknown));
        done(&r);
    }

    #[test]
    fn a_stop_asked_for_is_taken_once_and_only_for_its_pid() {
        let (r, _) = tmp("stop");
        let p = r.with_file_name("stopping");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        assert!(!take_stop(&p, 42), "nothing asked");
        note_stop(&p, 42);
        assert!(!take_stop(&p, 41), "a stop of another process");
        assert!(!take_stop(&p, 42), "…and the note went with that look");
        note_stop(&p, 42);
        assert!(take_stop(&p, 42));
        assert!(!take_stop(&p, 42));
        done(&r);
    }

    #[test]
    fn the_supervisors_runtimes_are_read_only_while_fresh_and_its_own() {
        let (r, _) = tmp("runtimes");
        let p = r.with_file_name("runtimes.json");
        let me = std::process::id();
        write_runtimes(&p, &serde_json::json!([{ "id": "claude-code", "available": true }]), 1_000);
        assert_eq!(read_runtimes(&p, me, 1_060, 120).map(|r| r.len()), Some(1));
        assert_eq!(read_runtimes(&p, me + 1, 1_060, 120), None, "another supervisor's");
        assert_eq!(read_runtimes(&p, me, 1_200, 120), None, "too old to stand for now");
        done(&r);
    }

    #[test]
    fn the_first_start_knows_no_exit() {
        let (r, x) = tmp("first");
        let (run, last) = begin_at(&r, &x, 100, "0.9.150", true, 1_000);
        assert_eq!(last, None);
        assert_eq!(run, Run { pid: 100, started_at: 1_000, version: "0.9.150".into(), auto_update: true, crashes: vec![], control: crate::control::VERSION, update_setting: true });
        // Said in the record a command reads before it asks anything.
        assert_eq!(read::<Run>(&r).map(|r| r.control), Some(crate::control::VERSION));
        // A supervisor from before the requests wrote none: it takes nothing.
        let old: Run = serde_json::from_str(r#"{"pid":1,"started_at":2,"version":"0.9.148","auto_update":true}"#).unwrap();
        assert_eq!(old.control, 0);
        let s = service_at(&r, &x, Some(100), 1_030);
        assert_eq!(s.run.as_ref().map(|r| r.started_at), Some(1_000));
        assert_eq!(s.last_exit, None);
        done(&r);
    }

    #[test]
    fn a_noted_stop_is_why_the_next_one_started() {
        let (r, x) = tmp("noted");
        begin_at(&r, &x, 100, "0.9.149", true, 1_000);
        // `mafold down` stopped it (the supervisor's own signal note first, then down's).
        note_exit_at(&r, &x, 100, Why::Signal, Some("SIGTERM"), 2_000);
        note_exit_at(&r, &x, 100, Why::Stopped, None, 2_001);
        // Down: it says why, and it's no crash.
        let s = service_at(&r, &x, None, 2_010);
        assert_eq!(s.run, None);
        let last = s.last_exit.clone().unwrap();
        assert_eq!((last.reason, last.at, last.version.as_str()), (Why::Stopped, Some(2_001), "0.9.149"));
        assert!(s.crashes.is_empty());
        // Up again: the stop is the last exit, still.
        let (run, last) = begin_at(&r, &x, 200, "0.9.149", true, 3_000);
        assert_eq!(last.map(|x| x.reason), Some(Why::Stopped));
        assert!(run.crashes.is_empty());
        let s = service_at(&r, &x, Some(200), 3_005);
        assert_eq!(s.last_exit.map(|x| (x.reason, x.pid)), Some((Why::Stopped, 100)));
        done(&r);
    }

    #[test]
    fn a_supervisor_gone_without_a_word_ended_unknown() {
        let (r, x) = tmp("unknown");
        begin_at(&r, &x, 100, "0.9.150", true, 1_000);
        // An old note about some other process says nothing about this run.
        note_exit_at(&r, &x, 7, Why::Stopped, None, 500);
        let s = service_at(&r, &x, None, 1_100);
        let last = s.last_exit.unwrap();
        assert_eq!((last.reason, last.pid, last.at), (Why::Unknown, 100, None));
        assert_eq!(s.crashes, vec![1_000], "a fall stands in at its run's start when nothing noted the time");
        done(&r);
    }

    #[test]
    fn an_update_reexec_in_place_is_an_update_not_a_crash() {
        let (r, x) = tmp("update");
        begin_at(&r, &x, 100, "0.9.149", true, 1_000);
        note_exit_at(&r, &x, 100, Why::Update, Some("0.9.150"), 2_000);
        // Same pid (unix exec), new version.
        let (run, last) = begin_at(&r, &x, 100, "0.9.150", true, 2_001);
        let last = last.unwrap();
        assert_eq!((last.reason, last.version.as_str(), last.detail.as_deref()), (Why::Update, "0.9.149", Some("0.9.150")));
        assert!(run.crashes.is_empty());
        assert_eq!(service_at(&r, &x, Some(100), 2_005).run.map(|r| r.version), Some("0.9.150".into()));
        done(&r);
    }

    #[test]
    fn a_failed_reexec_puts_the_real_last_exit_back() {
        let (r, x) = tmp("restore");
        begin_at(&r, &x, 100, "0.9.149", true, 1_000);
        note_exit_at(&r, &x, 100, Why::Stopped, None, 1_500);
        let (_, last) = begin_at(&r, &x, 200, "0.9.149", true, 2_000);
        note_exit_at(&r, &x, 200, Why::Update, Some("0.9.150"), 2_500);
        // While it runs, a note newer than its start isn't its last exit.
        assert_eq!(service_at(&r, &x, Some(200), 2_501).last_exit, None);
        // `restore_exit` writes `last` back at the real path; here, by hand.
        write(&x, last.as_ref().unwrap());
        assert_eq!(service_at(&r, &x, Some(200), 2_502).last_exit.map(|x| x.reason), Some(Why::Stopped));
        done(&r);
    }

    #[test]
    fn starts_after_crashes_add_up_to_a_loop_and_age_out() {
        let (r, x) = tmp("loop");
        begin_at(&r, &x, 1, "v", true, 1_000);
        // Three restarts after falls, a minute apart.
        let (run, _) = begin_at(&r, &x, 2, "v", true, 1_060);
        assert_eq!(run.crashes, vec![1_060]);
        note_exit_at(&r, &x, 2, Why::Crashed, Some("panicked at x"), 1_100);
        let (run, last) = begin_at(&r, &x, 3, "v", true, 1_120);
        assert_eq!(last.map(|x| x.reason), Some(Why::Crashed));
        assert_eq!(run.crashes, vec![1_060, 1_120]);
        let (run, _) = begin_at(&r, &x, 4, "v", true, 1_180);
        assert_eq!(run.crashes.len(), CRASH_LOOP);
        assert_eq!(service_at(&r, &x, Some(4), 1_200).crashes.len(), CRASH_LOOP);
        // Ten minutes on, they've aged out of the window.
        assert!(service_at(&r, &x, Some(4), 1_060 + CRASH_WINDOW + 200).crashes.is_empty());
        // A stop on purpose doesn't count.
        note_exit_at(&r, &x, 4, Why::Stopped, None, 1_200);
        let (run, _) = begin_at(&r, &x, 5, "v", true, 1_210);
        assert_eq!(run.crashes.len(), CRASH_LOOP, "the earlier ones stay, this start adds none");
        done(&r);
    }

    #[test]
    fn a_reason_from_a_newer_mafold_reads_as_unknown() {
        let (r, x) = tmp("newer");
        std::fs::create_dir_all(x.parent().unwrap()).unwrap();
        std::fs::write(&x, r#"{"pid":9,"reason":"paused","at":5,"version":"1.0.0"}"#).unwrap();
        let s = service_at(&r, &x, None, 10);
        assert_eq!(s.last_exit.map(|x| x.reason), Some(Why::Unknown));
        done(&r);
    }

    #[test]
    fn a_broken_or_missing_record_is_nothing_known() {
        let (r, x) = tmp("broken");
        assert_eq!(service_at(&r, &x, Some(1), 10), Service::default());
        std::fs::create_dir_all(r.parent().unwrap()).unwrap();
        std::fs::write(&r, "{not json").unwrap();
        assert_eq!(service_at(&r, &x, Some(1), 10), Service::default());
        // A supervisor from before these records: running, nothing on file about it.
        std::fs::write(&r, r#"{"pid":5,"started_at":1,"version":"v","auto_update":true}"#).unwrap();
        assert_eq!(service_at(&r, &x, Some(6), 10).run, None);
        done(&r);
    }

    #[test]
    fn deaths_keep_the_last_five() {
        let (r, _) = tmp("deaths");
        let p = r.with_file_name("deaths");
        assert!(read_deaths(&p).is_empty());
        for t in 1..=7 {
            note_death(&p, t);
        }
        assert_eq!(read_deaths(&p), vec![3, 4, 5, 6, 7]);
        done(&r);
    }

    #[test]
    fn a_turns_record_is_believed_only_for_its_own_daemon() {
        let (r, _) = tmp("turns");
        let p = r.with_file_name("turns.json");
        let t = Turns { pid: 42, active: 2, since: Some(1_000), runtime: Some("codex".into()) };
        write_turns(&p, &t);
        assert_eq!(read_turns(&p, 42), Some(t));
        assert_eq!(read_turns(&p, 43), None, "a record left by a daemon that is gone");
        done(&r);
    }
}
