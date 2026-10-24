//! One background task's own account of itself: when it started, whether it
//! has ended and how, and what the agent has already been told about it.
//!
//! `mafold bash-hook` files every detached task under `~/.mafold/bgtasks` as
//! `<surface>.<ns>.{sh,log,pid,meta}`, and the task leaves `<…>.exit` behind
//! when it ends (`bash_hook::script_body`, the Windows worker). This module
//! reads that registry PER TASK. The watcher before it kept one clock per
//! channel: the first task started it, later ones rode it, and two hours after
//! the FIRST it said «超过 2 小时» about tasks five minutes old and stopped
//! watching — so their exits went unreported until some later turn found the
//! leftovers and reported them again (10-22, #desktop优化 / #数字单位).
//!
//! Here each task is judged on its own: [`life`] by its receipt (a pid only
//! when no receipt can exist, and then held to the pid's start time, so a pid
//! handed to another process doesn't keep a dead task alive), its age from its
//! own start, its log's last change for «gone quiet». What the agent has been
//! told is written beside it ([`Ledger`], `.state`), so neither a second
//! watcher nor a restarted daemon tells it twice.

use mafold_core::mafold_types::units::EN;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The server switch (`mafold-types` registry): on = this module's watcher
/// and events, off = the per-channel watcher with its own messages.
pub const FLAG: &str = "bgtaskEvents";

pub fn events_on() -> bool {
    #[cfg(test)]
    if let Some(on) = *TEST_EVENTS.lock().unwrap() {
        return on;
    }
    crate::flags::enabled(FLAG)
}

/// The watcher tests run both watchers, one at a time, without touching the
/// process-wide flags other tests read.
#[cfg(test)]
pub static TEST_EVENTS: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

/// The machine-wide registry every daemon files its tasks in.
pub fn registry_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(d) = TEST_REGISTRY.get() {
        return Some(d.clone());
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".mafold").join("bgtasks"))
}

/// The watcher tests' registry, so they never touch the machine's real one
/// (each test files under a surface of its own).
#[cfg(test)]
pub static TEST_REGISTRY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The tasks in `dir` still running: a `<task>.pid` receipt with no
/// `<task>.exit` beside it yet — the pid, and when the receipt was written.
/// On Windows every task leaves both, and the pid is its worker's — the
/// process that outlives the daemon that started it, which ending that
/// daemon has to leave be (`platform::end_tree`). A worker killed outright
/// leaves no exit receipt: its pid, once reused, belongs to a process that
/// started after the receipt — which is why the time comes with it.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn running_in(dir: &Path) -> Vec<(u32, std::time::SystemTime)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pid") && !p.with_extension("exit").exists())
        .filter_map(|p| {
            let pid = std::fs::read_to_string(&p).ok()?.trim().parse().ok()?;
            Some((pid, std::fs::metadata(&p).and_then(|m| m.modified()).ok()?))
        })
        .collect()
}

/// How often a watcher looks. A test's minutes are milliseconds.
pub const TICK: Duration = if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(10) };

fn env_secs(key: &str) -> Option<Duration> {
    std::env::var(key).ok()?.trim().parse().ok().map(Duration::from_secs)
}

/// A log that hasn't changed for this long, on a task still running, is worth
/// the agent's look (once). `MAFOLD_BGTASK_QUIET_SECS` sets it, for a run on
/// dev that can't wait half an hour.
pub fn quiet_after() -> Duration {
    if cfg!(test) {
        return Duration::from_millis(2000);
    }
    env_secs("MAFOLD_BGTASK_QUIET_SECS").unwrap_or(Duration::from_secs(30 * 60))
}

/// A task running this long, from ITS OWN start, is worth the agent's look
/// (once) — not a deadline: it is watched until it exits.
/// `MAFOLD_BGTASK_LONG_SECS` sets it, as above.
pub fn long_after() -> Duration {
    if cfg!(test) {
        return Duration::from_millis(3000);
    }
    env_secs("MAFOLD_BGTASK_LONG_SECS").unwrap_or(Duration::from_secs(2 * 60 * 60))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How a task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// The shell's exit code. `None` when nothing could say: a SIGKILL, the
    /// machine going down — the task is gone and left no receipt.
    pub code: Option<i32>,
    /// Killed for running past the `timeout` its Bash call carried.
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Life {
    Running,
    Exited(Exit),
}

/// A receipt's text: the code on the first line (`124` + a `timeout` line
/// from the unix watchdog; a bare code from the Windows worker, which uses
/// 124 for its timeout too). Codes past 255 (bash 3.2 hands its `EXIT` trap
/// 258 for a syntax error) are what the shell actually exits with: mod 256.
///
/// On unix only the `timeout` line says Mafold's cap killed it: a bare 124 is
/// the command's own (`timeout 60 cargo test` running out inside it).
pub fn parse_receipt(s: &str, has_timeout: bool) -> Exit {
    let code = s
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<i64>().ok())
        .map(|n| n.rem_euclid(256) as i32);
    let marked = s.lines().any(|l| l.trim() == "timeout");
    Exit { code, timed_out: marked || (cfg!(windows) && has_timeout && code == Some(124)) }
}

/// What the hook wrote down about the task (`.meta`).
#[derive(Debug, Default, Clone)]
pub struct Meta {
    pub cmd: String,
    pub timeout_secs: Option<u64>,
    /// `platform::pid_start` of the task's pid, right after it was spawned.
    pub pid_start: Option<u64>,
    /// It leaves an exit receipt (`bash_hook::receipt_script`; the Windows
    /// worker always does). One registered without — started while the flag
    /// was off — says nothing about how it ended, which is not the same as
    /// being killed.
    pub receipt: bool,
}

pub fn meta(pid_path: &Path) -> Meta {
    let v: serde_json::Value = std::fs::read_to_string(pid_path.with_extension("meta"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let cmd = v["cmd"].as_str().map(str::to_string).unwrap_or_else(|| {
        // Registrations from before `.meta` carried the command: the script,
        // minus its shebang.
        std::fs::read_to_string(pid_path.with_extension("sh"))
            .map(|s| {
                s.lines()
                    .filter(|l| !l.starts_with("#!") && !l.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" ; ")
            })
            .unwrap_or_default()
    });
    Meta {
        cmd,
        timeout_secs: v["timeout_secs"].as_u64(),
        pid_start: v["pid_start"].as_u64(),
        receipt: cfg!(windows) || v["receipt"].as_bool().unwrap_or(false),
    }
}

fn read_receipt(pid_path: &Path, m: &Meta) -> Option<Exit> {
    let s = std::fs::read_to_string(pid_path.with_extension("exit")).ok()?;
    Some(parse_receipt(&s, m.timeout_secs.is_some()))
}

/// Is the task behind this registration still running?
///
/// The receipt answers first: the task wrote it on its way out. Without one,
/// the pid — but only the SAME process: a pid alive under a different start
/// time was handed to someone else after the task died without a receipt
/// (and its group with it: a pid isn't handed out while a group by that id
/// lives). A pid that is gone leaves its GROUP to ask — the task is its
/// process group, and the shell can be killed alone while what it ran goes
/// on. And a task found gone gets one more look for a receipt, which may have
/// landed in between.
pub fn life(pid_path: &Path, m: &Meta) -> Life {
    if let Some(e) = read_receipt(pid_path, m) {
        return Life::Exited(e);
    }
    let pid = std::fs::read_to_string(pid_path).ok().and_then(|s| s.trim().parse::<u32>().ok());
    if let Some(pid) = pid {
        if crate::platform::pid_alive(pid) {
            match (m.pid_start, crate::platform::pid_start(pid)) {
                (Some(want), Some(got)) if want != got => {}
                _ => return Life::Running,
            }
        } else if crate::platform::group_alive(pid) {
            return Life::Running;
        }
    }
    Life::Exited(read_receipt(pid_path, m).unwrap_or(Exit { code: None, timed_out: false }))
}

/// What the agent has been told about one task, kept beside it (`.state`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// Told its log had gone quiet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
    /// Told it was running long.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub long: bool,
    /// Its exit was put in the mailbox of a turn that was already running:
    /// the files go once that turn has taken it (`agent::wrapup_claimed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handed: Option<Handed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handed {
    pub mailbox: String,
    pub needle: String,
}

pub fn ledger(pid_path: &Path) -> Ledger {
    std::fs::read_to_string(pid_path.with_extension("state"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_ledger(pid_path: &Path, l: &Ledger) {
    let path = pid_path.with_extension("state");
    let tmp = pid_path.with_extension("state.tmp");
    if let Ok(s) = serde_json::to_string(l) {
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Exits handed to a turn whose mailbox held nothing else when it ended — the
/// agent never read them: handed back, for the watcher to tell again
/// (`agent::handle`'s last drain).
pub fn unhand(tag: &str, mailbox: &str) {
    for t in list(tag) {
        if t.ledger.handed.as_ref().is_some_and(|h| h.mailbox == mailbox) {
            let mut l = t.ledger.clone();
            l.handed = None;
            write_ledger(&t.pid_path, &l);
        }
    }
}

/// One registration, read whole.
#[derive(Debug, Clone)]
pub struct Task {
    /// `<surface>.<ns>.pid` — what names the task everywhere.
    pub pid_path: PathBuf,
    pub started_ms: u64,
    pub life: Life,
    pub meta: Meta,
    /// How long its log has sat unchanged (since its start, with no log yet).
    pub quiet_ms: u64,
    pub ledger: Ledger,
}

impl Task {
    pub fn log(&self) -> PathBuf {
        self.pid_path.with_extension("log")
    }
    pub fn running(&self) -> bool {
        self.life == Life::Running
    }
}

/// Every task registered under `tag`, oldest first.
pub fn list(tag: &str) -> Vec<Task> {
    let Some(dir) = registry_dir() else { return vec![] };
    list_in(&dir, tag, now_ms())
}

pub fn list_in(dir: &Path, tag: &str, now: u64) -> Vec<Task> {
    let prefix = format!("{tag}.");
    let mut tasks: Vec<Task> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_prefix(&prefix)?.strip_suffix(".pid")?;
            let started_ms = stem.parse::<u128>().ok().map(|ns| (ns / 1_000_000) as u64)?;
            let pid_path = e.path();
            let meta = meta(&pid_path);
            let changed = std::fs::metadata(pid_path.with_extension("log"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(started_ms);
            Some(Task {
                life: life(&pid_path, &meta),
                ledger: ledger(&pid_path),
                quiet_ms: now.saturating_sub(changed.max(started_ms)),
                started_ms,
                meta,
                pid_path,
            })
        })
        .collect();
    tasks.sort_by_key(|t| t.started_ms);
    tasks
}

/// Remove a task's registration, everything that can sit beside it included.
pub fn cleanup(pid_paths: &[PathBuf]) {
    for p in pid_paths {
        for ext in ["pid", "log", "sh", "exit", "meta", "state", "ready", "exit.tmp", "exit.wd", "exit.timeout", "state.tmp"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
    }
}

/// A task whose end the agent has been told: its registration goes, its LOG
/// stays. The update only names the log, and a handed one counts as taken the
/// moment the turn's hook picks its mailbox up — before the agent's first
/// Read of it; a later turn may want it again too. The 7-day sweep
/// (`bash_hook::sweep_old`) collects it.
pub fn retire(pid_paths: &[PathBuf]) {
    for p in pid_paths {
        for ext in ["pid", "sh", "exit", "meta", "state", "ready", "exit.tmp", "exit.wd", "exit.timeout", "state.tmp"] {
            let _ = std::fs::remove_file(p.with_extension(ext));
        }
    }
}

/// Something about one task the agent hasn't been told yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Exited(Exit),
    /// Still running, log unchanged for `quiet_after`.
    Quiet,
    /// Still running at `long_after` from its own start.
    Long,
}

/// What, if anything, this task has to say now. At most one thing per look:
/// the end outranks everything, and a quiet log gives way to «long» (both
/// come round, one look apart).
pub fn due(t: &Task, now: u64, quiet: Duration, long: Duration) -> Option<Kind> {
    match &t.life {
        Life::Exited(e) => t.ledger.handed.is_none().then(|| Kind::Exited(e.clone())),
        Life::Running => {
            let age = now.saturating_sub(t.started_ms);
            if !t.ledger.long && age >= long.as_millis() as u64 {
                Some(Kind::Long)
            } else if !t.ledger.quiet && t.quiet_ms >= quiet.as_millis() as u64 && age >= quiet.as_millis() as u64 {
                Some(Kind::Quiet)
            } else {
                None
            }
        }
    }
}

/// One line of a command, for a prompt or a card: newlines folded, cut long.
pub fn one_line(s: &str, max: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        format!("{}…", one.chars().take(max).collect::<String>())
    } else {
        one
    }
}

/// How a task's update reads to the agent: one bullet.
pub fn describe(t: &Task, kind: &Kind, now: u64) -> String {
    let cmd = one_line(&t.meta.cmd, 200);
    let age = EN.elapsed(now.saturating_sub(t.started_ms) as f64);
    let log = t.log().display().to_string();
    match kind {
        Kind::Exited(Exit { timed_out: true, .. }) => {
            let cap = t.meta.timeout_secs.map(|s| format!(" {s}s")).unwrap_or_default();
            format!("• `{cmd}` — killed by its{cap} timeout after {age}. Log: {log}")
        }
        Kind::Exited(Exit { code: Some(c), .. }) => format!("• `{cmd}` — exited {c} after {age}. Log: {log}"),
        Kind::Exited(Exit { code: None, .. }) if t.meta.receipt => format!(
            "• `{cmd}` — ended after about {age} without an exit code (killed outright, or the machine \
             restarted). Log: {log}"
        ),
        Kind::Exited(Exit { code: None, .. }) => format!(
            "• `{cmd}` — finished after about {age}. It was started before exit codes were kept, so how it \
             ended isn't recorded — the log will say. Log: {log}"
        ),
        Kind::Quiet => format!(
            "• `{cmd}` — still running ({age}), but its log hasn't changed in {} — it may be stuck, or \
             waiting on input it will never get. Log: {log}",
            EN.elapsed(t.quiet_ms as f64)
        ),
        Kind::Long => format!(
            "• `{cmd}` — still running, {age} so far. Nothing stops it; it is watched until it exits, \
             and this is the only time you're told it is long. Log: {log}"
        ),
    }
}

/// The line a RUNNING turn's reply shows where an update was handed to it
/// (`Seam::Notice`): the model gets [`prompt`] through the mailbox, the reader
/// gets this.
pub fn notice(events: &[(Task, Kind)]) -> String {
    let ended = events.iter().filter(|(_, k)| matches!(k, Kind::Exited(_))).count();
    match (ended, events.len()) {
        (1, 1) => "后台任务跑完了 —— 结果交给正在进行的这一轮。".to_string(),
        (n, all) if n == all => format!("{n} 个后台任务跑完了 —— 结果交给正在进行的这一轮。"),
        _ => "后台任务有新情况 —— 交给正在进行的这一轮。".to_string(),
    }
}

/// How every update to the agent begins and ends ([`prompt`]) — what tells a
/// mailbox holding only updates from one holding someone's words too
/// ([`only_updates`]).
const UPDATE_HEAD: &str = "(background task update — from the daemon, not from anyone in the chat.";
const UPDATE_TAIL: &str = "Read the log(s) you need. Then it is your call: tell the people here what they \
                           need to know, carry on with the work this was part of, or both.)";

/// What the agent is told: what happened to which task, what else is still
/// running, and that the call on what to say is its own. (As a turn of its
/// own, `agent::handle` adds how to say there is nothing — `agent::Quiet`.)
pub fn prompt(events: &[(Task, Kind)], still: &[&Task], now: u64) -> String {
    let mut s = format!("{UPDATE_HEAD} Tasks you started in the background earlier:\n");
    for (t, k) in events {
        s.push_str(&describe(t, k, now));
        s.push('\n');
    }
    let others: Vec<&&Task> = still
        .iter()
        .filter(|t| !events.iter().any(|(e, _)| e.pid_path == t.pid_path))
        .collect();
    if !others.is_empty() {
        s.push_str(&format!(
            "Still running, nothing new: {} — you'll hear when one ends or looks wrong.\n",
            others
                .iter()
                .map(|t| format!("`{}` ({})", one_line(&t.meta.cmd, 80), EN.elapsed(now.saturating_sub(t.started_ms) as f64)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s.push_str(UPDATE_TAIL);
    s
}

/// Is what a turn left in its mailbox nothing but background-task updates
/// (and the room around them)? Then the follow-up it buys is an update turn —
/// the agent's call whether to say anything — not a reply someone is owed:
/// a turn that reported a task by hand and ended before reading the update
/// that came for it must not have to report it again (10-22 03:39).
pub fn only_updates(raw_mailbox: &str) -> bool {
    use mafold_core::mafold_types::steer::mailbox;
    let mut rest = raw_mailbox.to_string();
    let mut found = false;
    while let Some(at) = rest.find(UPDATE_HEAD) {
        let Some(len) = rest[at..].find(UPDATE_TAIL) else { break };
        rest.replace_range(at..at + len + UPDATE_TAIL.len(), "");
        found = true;
    }
    found && mailbox::followup(&rest).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(dir: &Path, ns: u128, cmd: &str) -> PathBuf {
        let pid_path = dir.join(format!("c__ch__bot.{ns}.pid"));
        std::fs::write(pid_path.with_extension("meta"), serde_json::json!({ "cmd": cmd }).to_string()).unwrap();
        pid_path
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mf-bgtasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// What ending a daemon on Windows leaves be: the workers of tasks that
    /// haven't left their exit receipt yet.
    #[test]
    fn the_running_tasks_are_those_without_an_exit_receipt() {
        let dir = tmp();
        let file = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        file("c__ch__bot.1.pid", "4100\n");
        file("c__ch__bot.2.pid", "4200");
        file("c__ch__bot.2.exit", "0");
        file("c__ch__bot.3.pid", "not a pid");
        file("c__ch__bot.3.log", "4300");
        file("7f3a.pid", "4400");
        file("7f3a.exit.tmp", "");
        let mut running = running_in(&dir);
        running.sort();
        assert_eq!(running.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(), vec![4100, 4400], "ended (2), unreadable (3) and not a receipt left out; an exit half-written is no exit");
        let written = std::fs::metadata(dir.join("c__ch__bot.1.pid")).unwrap().modified().unwrap();
        assert_eq!(running[0].1, written, "with when the receipt was written");
        assert!(running_in(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_receipt_says_how_it_ended() {
        assert_eq!(parse_receipt("0\n", false), Exit { code: Some(0), timed_out: false });
        assert_eq!(parse_receipt("7", false), Exit { code: Some(7), timed_out: false });
        assert_eq!(parse_receipt("124\ntimeout\n", true), Exit { code: Some(124), timed_out: true });
        // A bare 124 is a timeout only from the Windows worker; on unix it is
        // the command's own exit (its own `timeout` running out).
        assert_eq!(parse_receipt("124", true), Exit { code: Some(124), timed_out: cfg!(windows) });
        assert_eq!(parse_receipt("124", false), Exit { code: Some(124), timed_out: false }, "no timeout was set");
        assert_eq!(parse_receipt("258\n", false).code, Some(2), "bash 3.2's syntax error");
        assert_eq!(parse_receipt("", false).code, None);
    }

    /// The receipt is the answer while the pid still lives (the watchdog's
    /// receipt lands before its kill does), and a pid that is gone is ended
    /// even without one.
    #[test]
    fn the_receipt_answers_before_the_pid() {
        let dir = tmp();
        let me = std::process::id();
        let p = task(&dir, 1, "x");
        std::fs::write(&p, me.to_string()).unwrap();
        assert_eq!(life(&p, &meta(&p)), Life::Running, "my own pid, no receipt");
        std::fs::write(p.with_extension("exit"), "3\n").unwrap();
        assert_eq!(life(&p, &meta(&p)), Life::Exited(Exit { code: Some(3), timed_out: false }));
        let q = task(&dir, 2, "y");
        std::fs::write(&q, "999999").unwrap();
        assert_eq!(life(&q, &meta(&q)), Life::Exited(Exit { code: None, timed_out: false }), "no such pid");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 10-22 regression (pid reuse): a task that died WITHOUT a receipt, whose
    /// pid now belongs to another live process, used to read as running for
    /// as long as that stranger lived. Its recorded start time gives it away.
    #[cfg(unix)]
    #[test]
    fn a_reused_pid_is_not_the_task() {
        let dir = tmp();
        let me = std::process::id();
        let mine = crate::platform::pid_start(me).expect("this process can be asked");
        let p = dir.join(format!("c__ch__bot.{}.pid", 1u128));
        std::fs::write(&p, me.to_string()).unwrap();
        std::fs::write(p.with_extension("meta"), serde_json::json!({ "cmd": "x", "pid_start": mine + 1 }).to_string()).unwrap();
        assert_eq!(life(&p, &meta(&p)), Life::Exited(Exit { code: None, timed_out: false }));
        std::fs::write(p.with_extension("meta"), serde_json::json!({ "cmd": "x", "pid_start": mine }).to_string()).unwrap();
        assert_eq!(life(&p, &meta(&p)), Life::Running, "control: the same start is the same process");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn running(started_ms: u64, quiet_ms: u64) -> Task {
        Task {
            pid_path: PathBuf::from("/r/c__ch__bot.1.pid"),
            started_ms,
            life: Life::Running,
            meta: Meta { cmd: "make".into(), ..Default::default() },
            quiet_ms,
            ledger: Ledger::default(),
        }
    }

    /// 10-22 regression (#desktop优化 02:58): a task 16 minutes old, riding a
    /// watcher armed two hours earlier, was told «超过 2 小时» and dropped.
    /// Age is the TASK's own, and a long one is told once, then still watched.
    #[test]
    fn age_is_the_tasks_own_and_long_is_said_once() {
        let (q, l) = (Duration::from_secs(30 * 60), Duration::from_secs(2 * 3600));
        let start = 10_000_000_000u64;
        let sixteen_min = start + 16 * 60_000;
        assert_eq!(due(&running(start, 0), sixteen_min, q, l), None);
        let two_h = start + 2 * 3_600_000;
        let mut t = running(start, 0);
        assert_eq!(due(&t, two_h, q, l), Some(Kind::Long));
        t.ledger.long = true;
        assert_eq!(due(&t, two_h + 3_600_000, q, l), None, "said once");
    }

    #[test]
    fn a_quiet_log_is_said_once_and_not_on_a_young_task() {
        let (q, l) = (Duration::from_secs(30 * 60), Duration::from_secs(2 * 3600));
        let start = 10_000_000_000u64;
        assert_eq!(due(&running(start, 31 * 60_000), start + 31 * 60_000, q, l), Some(Kind::Quiet));
        assert_eq!(due(&running(start, 10 * 60_000), start + 31 * 60_000, q, l), None, "log moved 10 min ago");
        let mut t = running(start, 40 * 60_000);
        t.ledger.quiet = true;
        assert_eq!(due(&t, start + 40 * 60_000, q, l), None);
    }

    /// An exit handed to a running turn waits for that turn to take it; it is
    /// not handed again in the meantime.
    #[test]
    fn a_handed_exit_is_not_due_again() {
        let mut t = running(0, 0);
        t.life = Life::Exited(Exit { code: Some(0), timed_out: false });
        let (q, l) = (Duration::from_secs(1), Duration::from_secs(1));
        assert!(matches!(due(&t, 5, q, l), Some(Kind::Exited(_))));
        t.ledger.handed = Some(Handed { mailbox: "m".into(), needle: "n".into() });
        assert_eq!(due(&t, 5, q, l), None);
    }

    #[test]
    fn the_ledger_round_trips_and_cleanup_takes_everything() {
        let dir = tmp();
        let p = task(&dir, 5, "x");
        for ext in ["pid", "log", "sh", "exit"] {
            std::fs::write(p.with_extension(ext), "1").unwrap();
        }
        let l = Ledger { quiet: true, long: false, handed: Some(Handed { mailbox: "m".into(), needle: "n".into() }) };
        write_ledger(&p, &l);
        assert_eq!(ledger(&p), l);
        cleanup(std::slice::from_ref(&p));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing left beside it");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Told its end, a task's registration goes and its log stays: the turn
    /// told reads it after that.
    #[test]
    fn retiring_keeps_only_the_log() {
        let dir = tmp();
        let p = task(&dir, 6, "x");
        for ext in ["log", "sh", "exit", "state", "exit.wd"] {
            std::fs::write(p.with_extension(ext), "1").unwrap();
        }
        retire(std::slice::from_ref(&p));
        let left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, vec![p.with_extension("log").file_name().unwrap().to_string_lossy().into_owned()]);
        let _ = std::fs::remove_dir_all(dir);
    }


    #[test]
    fn list_is_per_surface_and_oldest_first() {
        let dir = tmp();
        for (tag, ns) in [("c__ch__bot", 3_000_000_000_000u128), ("c__ch__bot", 1_000_000_000_000), ("c__ch__bot--guest", 2_000_000_000_000), ("c__other__bot", 4)] {
            let p = dir.join(format!("{tag}.{ns}.pid"));
            std::fs::write(&p, "999999").unwrap();
        }
        let got = list_in(&dir, "c__ch__bot", 0);
        assert_eq!(got.iter().map(|t| t.started_ms).collect::<Vec<_>>(), vec![1_000_000, 3_000_000]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 10-22 03:38–03:39: an update handed to a running turn that ended before
    /// reading it comes back as the follow-up — which, made of nothing but
    /// updates, is the agent's call to speak or not; with someone's words in
    /// it, it is a reply they are owed.
    #[test]
    fn a_mailbox_of_only_updates_buys_no_reply_of_its_own() {
        use mafold_core::mafold_types::steer::mailbox;
        let mut t = running(10_000_000_000 - 60_000, 0);
        t.life = Life::Exited(Exit { code: Some(0), timed_out: false });
        let update = prompt(&[(t.clone(), Kind::Exited(Exit { code: Some(0), timed_out: false }))], &[], 10_000_000_000);
        let two = format!("{}\n{}\n", mailbox::said(&update), mailbox::said(&update));
        assert!(only_updates(&format!("{}\n", mailbox::said(&update))));
        assert!(only_updates(&two), "two updates");
        let room = format!("{}\n{}\n", mailbox::background("@linsky: 看起来还行"), mailbox::said(&update));
        assert!(only_updates(&room), "the room around it is not someone asking");
        let asked = format!("{}\n{}\n", mailbox::said(&update), mailbox::said("@ops: 好了没？"));
        assert!(!only_updates(&asked), "someone's words with it");
        assert!(!only_updates(&format!("{}\n", mailbox::said("@ops: 好了没？"))));
        assert!(!only_updates(""));
    }

    #[test]
    fn the_prompt_names_what_happened_and_hands_the_call_over() {
        let now = 10_000_000_000u64;
        let mut done = running(now - 12 * 60_000, 0);
        done.life = Life::Exited(Exit { code: Some(1), timed_out: false });
        let mut timed = running(now - 20 * 60_000, 0);
        timed.meta.timeout_secs = Some(1200);
        timed.life = Life::Exited(Exit { code: Some(124), timed_out: true });
        let mut other = running(now - 40 * 60_000, 0);
        other.pid_path = PathBuf::from("/r/c__ch__bot.2.pid");
        other.meta.cmd = "cargo build".into();
        let events = vec![(done.clone(), Kind::Exited(Exit { code: Some(1), timed_out: false })), (timed.clone(), Kind::Exited(Exit { code: Some(124), timed_out: true }))];
        let p = prompt(&events, &[&other], now);
        assert!(p.contains("exited 1 after 12m"), "{p}");
        assert!(p.contains("killed by its 1200s timeout after 20m"), "{p}");
        assert!(p.contains("Still running, nothing new: `cargo build` (40m)"), "{p}");
        assert!(p.contains("/r/c__ch__bot.1.log"), "{p}");
        assert!(!p.contains("2 小时") && !p.contains("2 hours"), "no deadline: {p}");
        assert_eq!(notice(&events), "2 个后台任务跑完了 —— 结果交给正在进行的这一轮。");
        assert_eq!(notice(&[(other, Kind::Quiet)]), "后台任务有新情况 —— 交给正在进行的这一轮。");
    }

    /// No exit code: killed outright, for a task that leaves a receipt — but a
    /// task started while the flag was off never could, and must not be
    /// reported as killed (the review's flag-flip case).
    #[test]
    fn no_code_is_killed_only_where_a_receipt_was_due() {
        let now = 10_000_000_000u64;
        let mut t = running(now - 5 * 60_000, 0);
        let gone = Kind::Exited(Exit { code: None, timed_out: false });
        t.meta.receipt = true;
        assert!(describe(&t, &gone, now).contains("killed outright"));
        t.meta.receipt = false;
        let d = describe(&t, &gone, now);
        assert!(d.contains("started before exit codes were kept") && !d.contains("killed"), "{d}");
    }
}
