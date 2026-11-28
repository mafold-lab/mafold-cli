//! Where the supervisor and each bot it runs write their output — one file per
//! process on every OS, kept under [`ROTATE_AT`] (activity center C6).
//!
//! Until C6 that depended on who started the process: launchd wrote the file
//! the plist names (macOS), systemd kept it in the journal (Linux), and the
//! Windows logon task kept nothing at all — the platform where a person most
//! needs to see why their bots stopped. And nothing was ever rotated: bot logs
//! had reached 87 MB. Now a process points its own stdout and stderr at its
//! file the moment it starts, whoever started it ([`keep`]), and a small thread
//! moves the file aside to `<file>.1` once it passes the limit (the one before
//! that is dropped).
//!
//! Every file a process starts this way begins with [`KEPT`]. One that
//! doesn't was written before C6, when a message went into the log as it came,
//! line breaks and all; it is moved aside whole the first time, and
//! `mafold diagnose` leaves it out ([`Tail::left_out`]).
//!
//! Reading is here too, so `mafold logs` and `mafold diagnose` agree on what
//! «the last N lines» are: [`tail`] reaches into `<file>.1` when a rotation
//! just emptied the file, and [`follow`] carries on across one.
//!
//! And the one rule for a line that QUOTES something — a message somebody
//! sent, a reply, what a runtime printed: the line's own words come first, the
//! quote comes after [`QUOTE`] and runs to the end of the line ([`quoting`]).
//! The log on this computer keeps the quote; `mafold diagnose`, which makes a
//! file to send to somebody else, keeps the words and only the quote's length.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A log moves aside once it passes this.
pub const ROTATE_AT: u64 = 10 * 1024 * 1024;

/// How often a running process checks its own log.
const CHECK_EVERY: Duration = Duration::from_secs(30);

/// How the first line of every log file a process starts reads ([`keep`]).
pub const KEPT: &str = "── this log is kept by mafold ";

/// The variable the supervisor names a daemon's log in. The daemon takes it
/// out of its environment before it starts anything, so a program it runs —
/// a turn's own `mafold agent` — doesn't take its log over too.
pub const LOG_ENV: &str = "MAFOLD_DAEMON_LOG";

/// The file a rotation moves `log` to.
pub fn previous(log: &Path) -> PathBuf {
    let mut name = log.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// What separates a log line's own words from the text it quotes.
pub const QUOTE: &str = " ⟫ ";

/// `text` on one line: its own line breaks written as `\n`. A second line of
/// a quote would sit in the log looking like any other, with nothing to say
/// it is a quote.
pub fn one_line(text: &str) -> String {
    text.replace("\r\n", "\\n").replace(['\n', '\r'], "\\n")
}

/// One log line: `words`, then `quoted` after [`QUOTE`] ([`one_line`]).
pub fn quoting(words: &str, quoted: &str) -> String {
    format!("{words}{QUOTE}{}", one_line(quoted))
}

/// The line a bot writes for each message it receives.
pub fn said(sender: &str, text: &str) -> String {
    quoting(&format!("← @{sender}"), text)
}

/// How a runtime's own words are attached to «it failed»: claude's stderr,
/// codex's. The text the person sees in the chat says it the same way, which
/// is why this isn't [`QUOTE`]; `mafold diagnose` treats what follows it as a
/// quote all the same. Printed with [`one_line`].
pub const EXITED: &str = " exited unsuccessfully: ";

/// From now on this process's stdout and stderr go to `log` — wherever whoever
/// started it had pointed them — and the file stays under [`ROTATE_AT`].
pub fn keep(log: &Path) -> io::Result<()> {
    keep_with(log, ROTATE_AT, CHECK_EVERY)
}

/// `log`, open to append and begun with [`KEPT`], for a process about to be
/// started with it as its stdout and stderr (a bot, the supervisor `up`
/// detaches, `agent --detach`): what that process prints before it gets to
/// [`keep`] — the error of one that never does — lands in a file that is
/// ours, so `mafold diagnose` reads it, and that the next start rotates.
pub fn open_for_child(log: &Path) -> io::Result<File> {
    prepare(log, ROTATE_AT)?;
    open_kept(log)
}

/// Before `log` is written to: its directory made, and the file moved aside
/// when it isn't one to carry on writing.
fn prepare(log: &Path, limit: u64) -> io::Result<()> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    // Not begun by one of us: written before C6. Moved aside whole, whatever
    // its size (87 MB, at most once), so this file is ours from its first line.
    if fs::metadata(log).is_ok_and(|m| m.len() > 0) && !kept(log) {
        move_aside(log);
    } else {
        rotate_if_over(log, limit);
    }
    Ok(())
}

/// [`keep`] with its limit and how often it looks — a test's, small and fast.
fn keep_with(log: &Path, limit: u64, every: Duration) -> io::Result<()> {
    prepare(log, limit)?;
    point_std_at(log)?;
    let log = log.to_path_buf();
    std::thread::Builder::new().name("log-rotate".into()).spawn(move || loop {
        std::thread::sleep(every);
        release_retired();
        // Rotated: until this, lines kept landing in what is now `<log>.1`,
        // so the two files meet without a gap or a repeat. Not writing to it
        // any more: somebody moved or deleted it, and we'd be writing into a
        // file nobody sees, without a limit.
        if rotate_if_over(&log, limit) || !writing_to(&log) {
            let _ = point_std_at(&log);
        } else if fs::metadata(&log).is_ok_and(|m| m.len() > limit.saturating_mul(3)) {
            // Over the limit and it won't move (on Windows, something holds
            // `<log>.1` on a disk that can't delete a file still open): cut
            // it, rather than let it grow without end.
            cut(&log);
        }
    })?;
    Ok(())
}

/// Whether `log` begins with [`KEPT`].
pub fn kept(log: &Path) -> bool {
    let mut head = Vec::new();
    File::open(log).and_then(|f| f.take(KEPT.len() as u64).read_to_end(&mut head)).is_ok() && head == KEPT.as_bytes()
}

/// Move `log` to `<log>.1` once it has passed `limit`. True when it moved.
pub fn rotate_if_over(log: &Path, limit: u64) -> bool {
    fs::metadata(log).is_ok_and(|m| m.len() > limit) && move_aside(log)
}

/// `log` → `<log>.1`, the one before dropped. A rename that fails is tried
/// again at the next check.
fn move_aside(log: &Path) -> bool {
    let old = previous(log);
    let _ = fs::remove_file(&old);
    fs::rename(log, &old).is_ok()
}

/// Empty `log` in place: what [`keep_with`] does when it can't move it. The
/// writers append, so they carry on from its new end — and so does the mark,
/// never written at a fixed place a writer's line may already be at. A line
/// that gets there first makes it a file not begun with [`KEPT`], which
/// `mafold diagnose` leaves out rather than read half a line of.
fn cut(log: &Path) {
    let Ok(f) = OpenOptions::new().write(true).open(log) else { return };
    if f.set_len(0).is_ok() {
        if let Ok(mut f) = OpenOptions::new().append(true).open(log) {
            let _ = writeln!(f, "{} · cut at {} MB: it couldn't be moved aside", mark(), ROTATE_AT * 3 / 1024 / 1024);
        }
    }
}

/// The [`KEPT`] line this process begins a file with.
fn mark() -> String {
    format!("{KEPT}{} · pid {} ──", env!("CARGO_PKG_VERSION"), std::process::id())
}

/// `log`, open to append, begun with [`KEPT`] if it is empty — written before
/// stdout points at it, so nothing else can get there first.
fn open_kept(log: &Path) -> io::Result<File> {
    let mut file = OpenOptions::new().create(true).append(true).open(log)?;
    if file.metadata()?.len() == 0 {
        writeln!(file, "{}", mark())?;
    }
    Ok(file)
}

/// Send this process's stdout and stderr to `log` (appending), replacing
/// whatever they were — a terminal, the file launchd opened, the console the
/// Windows logon task hands over, the file a rotation just moved aside.
#[cfg(unix)]
fn point_std_at(log: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = open_kept(log)?;
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    for target in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // dup2 swaps in one step: every write goes either to the old file or
        // the new one, never nowhere. `file` itself closes on return; the two
        // duplicates keep it open, and close what they replace.
        if unsafe { libc::dup2(file.as_raw_fd(), target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether stdout is the file at `log` — not one somebody moved or deleted.
#[cfg(unix)]
fn writing_to(log: &Path) -> bool {
    use std::os::fd::FromRawFd;
    // Borrowed, never closed.
    let out = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(libc::STDOUT_FILENO) });
    matches!((identity(&out), path_identity(log)), (Some(a), Some(b)) if a == b)
}

#[cfg(unix)]
fn release_retired() {}

/// What stdout and stderr point at on Windows, where SetStdHandle borrows the
/// handle: the file it is now, and the handles it was before.
#[cfg(windows)]
struct Held {
    now: Option<File>,
    /// Closed at the next check, not at once: a write that fetched one of
    /// them a moment ago must not find it closed under it.
    retired: Vec<File>,
    /// Whether the handles this process started with were dealt with.
    started: bool,
}

#[cfg(windows)]
static HELD: std::sync::Mutex<Held> = std::sync::Mutex::new(Held { now: None, retired: Vec::new(), started: false });

#[cfg(windows)]
fn point_std_at(log: &Path) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_DISK};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
    let file = open_kept(log)?;
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    let mut held = HELD.lock().unwrap_or_else(|p| p.into_inner());
    let before = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|which| unsafe { GetStdHandle(which) });
    let started_with = if held.started { Vec::new() } else { before.to_vec() };
    for (i, which) in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].into_iter().enumerate() {
        if unsafe { SetStdHandle(which, file.as_raw_handle() as _) } == 0 {
            let e = io::Error::last_os_error();
            // Stdout already moved: put it back before `file` closes on
            // return, or it would name a handle that's gone.
            for (back, was) in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].into_iter().zip(before).take(i) {
                unsafe { SetStdHandle(back, was) };
            }
            return Err(e);
        }
    }
    if !held.started {
        held.started = true;
        // What this process was started with — the file the supervisor
        // opened on this same log, a pipe, a console — came here inheritable,
        // so every program this one starts would hold it open too: the file
        // could never be deleted under its name on a disk that waits for the
        // last handle, and the next rotation could never move this one aside.
        // Passed on to nothing from here; a file, closed at the next check.
        let mut seen: Vec<usize> = Vec::new();
        for h in started_with {
            if h.is_null() || h == INVALID_HANDLE_VALUE || seen.contains(&(h as usize)) {
                continue;
            }
            seen.push(h as usize);
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
            if unsafe { GetFileType(h) } == FILE_TYPE_DISK {
                held.retired.push(unsafe { File::from_raw_handle(h as _) });
            }
        }
    }
    if let Some(before) = held.now.replace(file) {
        held.retired.push(before);
    }
    Ok(())
}

#[cfg(windows)]
fn writing_to(log: &Path) -> bool {
    let held = HELD.lock().unwrap_or_else(|p| p.into_inner());
    matches!((held.now.as_ref().and_then(identity), path_identity(log)), (Some(a), Some(b)) if a == b)
}

#[cfg(windows)]
fn release_retired() {
    HELD.lock().unwrap_or_else(|p| p.into_inner()).retired.clear();
}

/// The end of a log, as [`tail`] reads it.
#[derive(Debug, Default)]
pub struct Tail {
    /// The last lines, oldest first, without their line endings.
    pub lines: Vec<String>,
    /// The length of the log up to its last line break — where [`follow`]
    /// picks up without repeating or splitting a line.
    pub end: u64,
    /// What the log holds past its last line break: a line still being
    /// written, or the end of one a process died writing.
    pub unfinished: Option<String>,
    /// Files [`tail`] was told to leave out ([`kept`]: from before C6).
    pub left_out: Vec<PathBuf>,
    /// The file [`tail`] read, still open: what [`follow_on`] carries on
    /// from, so a rotation between the two isn't missed.
    pub file: Option<File>,
}

/// The last `n` lines of `log`, reaching back into `<log>.1` when the current
/// file holds fewer (it was just rotated). `only_kept`: only from files that
/// begin with [`KEPT`] — the rest are named in [`Tail::left_out`].
pub fn tail(log: &Path, n: usize, only_kept: bool) -> io::Result<Tail> {
    let mut f = File::open(log)?;
    let len = f.metadata()?.len();
    let end = complete_end(&mut f, len)?;
    let unfinished = if end < len {
        f.seek(SeekFrom::Start(end))?;
        let mut rest = Vec::new();
        f.read_to_end(&mut rest)?;
        Some(String::from_utf8_lossy(&rest).trim_end_matches('\r').to_string())
    } else {
        None
    };
    let mut out = Tail { end, unfinished, ..Tail::default() };
    if only_kept && !kept(log) {
        out.left_out.push(log.to_path_buf());
        return Ok(out);
    }
    out.lines = last_lines(&mut f, end, n)?;
    out.file = f.try_clone().ok();
    if out.lines.len() < n {
        let older = previous(log);
        if let Ok(mut o) = File::open(&older) {
            if only_kept && !kept(&older) {
                out.left_out.push(older);
            } else {
                let older_end = o.metadata()?.len();
                let mut before = last_lines(&mut o, older_end, n - out.lines.len())?;
                before.append(&mut out.lines);
                out.lines = before;
            }
        }
    }
    Ok(out)
}

/// How far `f` goes up to and including its last line break.
fn complete_end(f: &mut File, len: u64) -> io::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let mut pos = len;
    while pos > 0 {
        let step = CHUNK.min(pos);
        pos -= step;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0; step as usize];
        f.read_exact(&mut chunk)?;
        if let Some(i) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(pos + i as u64 + 1);
        }
    }
    Ok(0)
}

/// The last `n` lines of `f` up to byte `end`, read backwards a chunk at a time
/// — a log from before rotation existed can be far too big to read whole.
fn last_lines(f: &mut File, end: u64, n: usize) -> io::Result<Vec<String>> {
    const CHUNK: u64 = 64 * 1024;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut breaks = 0;
    let mut pos = end;
    // A line is only known whole once the newline BEFORE it is in hand, so
    // read until there are more newlines than lines wanted (or the file starts).
    while pos > 0 && breaks <= n {
        let step = CHUNK.min(pos);
        pos -= step;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0; step as usize];
        f.read_exact(&mut chunk)?;
        breaks += chunk.iter().filter(|&&b| b == b'\n').count();
        chunks.push(chunk);
    }
    chunks.reverse();
    let buf = chunks.concat();
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if pos > 0 && !lines.is_empty() {
        lines.remove(0); // began mid-line
    }
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].iter().map(|l| l.to_string()).collect())
}

/// What [`follow`] reports.
#[derive(Debug, PartialEq)]
pub enum Tick {
    /// A line written to the log, without its line ending.
    Line(String),
    /// The log was moved aside (or cut) and a new one began; what follows is
    /// from the new file.
    Rotated,
}

/// Every complete line written to `log` from byte `from` on, handed to `each`
/// as it appears — across rotations — until `each` returns false (the reader
/// went away) or the log can't be read. Checks every `every`.
#[cfg(test)]
pub fn follow(log: &Path, from: u64, every: Duration, each: impl FnMut(Tick) -> bool) -> io::Result<()> {
    follow_on(File::open(log)?, log, from, every, each)
}

/// [`follow`] from a file already open — the one [`tail`] read
/// ([`Tail::file`]). Opening `log` again could open a newer file than the
/// one `from` counts in: a rotation between the two would skip the end of
/// the old file and the start of the new one, and say nothing.
pub fn follow_on(mut f: File, log: &Path, from: u64, every: Duration, mut each: impl FnMut(Tick) -> bool) -> io::Result<()> {
    let mut pos = from.min(f.metadata()?.len());
    f.seek(SeekFrom::Start(pos))?;
    let mut mine = identity(&f);
    let mut partial: Vec<u8> = Vec::new();
    // Rotated and said so; the new file isn't open yet.
    let mut switching = false;
    let emit = |partial: &mut Vec<u8>, each: &mut dyn FnMut(Tick) -> bool, all: bool| -> bool {
        while let Some(i) = partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = partial.drain(..=i).collect();
            if !each(Tick::Line(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_string())) {
                return false;
            }
        }
        if all && !partial.is_empty() {
            let rest = std::mem::take(partial);
            return each(Tick::Line(String::from_utf8_lossy(&rest).trim_end_matches('\r').to_string()));
        }
        true
    };
    loop {
        if switching {
            match File::open(log) {
                Ok(next) => {
                    f = next;
                    mine = identity(&f);
                    pos = 0;
                    switching = false;
                }
                // Between the rename and the new file: look again shortly.
                Err(_) => {
                    std::thread::sleep(every);
                    continue;
                }
            }
        }
        let mut chunk = Vec::new();
        f.read_to_end(&mut chunk)?;
        pos += chunk.len() as u64;
        partial.extend_from_slice(&chunk);
        if !emit(&mut partial, &mut each, false) {
            return Ok(());
        }
        // Rotated: the name now holds a different file.
        if matches!((mine, path_identity(log)), (Some(a), Some(b)) if a != b) {
            // The writer points at the new file a moment after the rename, and
            // a line written in between went to the old one: give it that
            // moment, then read the old one once more (a rotation only
            // renames; this handle still reads it).
            std::thread::sleep(every);
            let mut late = Vec::new();
            f.read_to_end(&mut late)?;
            partial.extend_from_slice(&late);
            if !emit(&mut partial, &mut each, true) || !each(Tick::Rotated) {
                return Ok(());
            }
            switching = true;
            continue;
        }
        // Cut in place (when it couldn't be moved): from the top.
        if f.metadata().is_ok_and(|m| m.len() < pos) {
            if !emit(&mut partial, &mut each, true) || !each(Tick::Rotated) {
                return Ok(());
            }
            f.seek(SeekFrom::Start(0))?;
            pos = 0;
            continue;
        }
        std::thread::sleep(every);
    }
}

/// Which file this is — not which name it has — so a rename is visible.
#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(windows)]
type FileId = (u32, u64);

#[cfg(unix)]
fn identity(f: &File) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    f.metadata().ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(unix)]
fn path_identity(p: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(windows)]
fn identity(f: &File) -> Option<FileId> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
        return None;
    }
    Some((info.dwVolumeSerialNumber, (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
}

#[cfg(windows)]
fn path_identity(p: &Path) -> Option<FileId> {
    identity(&File::open(p).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mafold-logfile-{tag}-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn append(p: &Path, text: &str) {
        OpenOptions::new().create(true).append(true).open(p).unwrap().write_all(text.as_bytes()).unwrap();
    }

    /// The test binary, run again with only `test` selected and `var` naming
    /// the log: what re-points stdout must do it in a process of its own, or
    /// it swallows the test harness's.
    fn child(test: &str, var: &str, log: &Path) -> std::process::Command {
        let mut c = crate::platform::std_command(crate::platform::current_exe().unwrap());
        c.args(["--exact", &format!("logfile::tests::{test}"), "--nocapture", "--test-threads=1"]).env(var, log);
        c
    }

    #[test]
    fn rotates_only_past_the_limit_and_keeps_one_previous() {
        let d = tmp("rotate");
        let log = d.join("supervisor.log");
        append(&log, "first\n");
        assert!(!rotate_if_over(&log, 100), "under the limit: untouched");
        append(&log, &"x".repeat(200));
        assert!(rotate_if_over(&log, 100));
        assert!(!log.exists());
        assert!(fs::read_to_string(previous(&log)).unwrap().starts_with("first\n"));
        // The next rotation replaces `.1` — never a `.2`.
        append(&log, &format!("second\n{}", "y".repeat(200)));
        assert!(rotate_if_over(&log, 100));
        assert!(fs::read_to_string(previous(&log)).unwrap().starts_with("second\n"));
        assert!(!d.join("supervisor.log.2").exists());
        assert!(!d.join("supervisor.log.1.1").exists());
    }

    #[test]
    fn tail_reads_the_last_lines_and_reaches_into_the_rotated_file() {
        let d = tmp("tail");
        let log = d.join("log");
        append(&previous(&log), "a1\na2\na3\n");
        append(&log, "b1\r\nb2\n");
        let t = tail(&log, 4, false).unwrap();
        assert_eq!(t.lines, ["a2", "a3", "b1", "b2"]);
        assert_eq!(t.end, fs::metadata(&log).unwrap().len());
        assert_eq!(t.unfinished, None);
        assert_eq!(tail(&log, 1, false).unwrap().lines, ["b2"]);
        assert!(tail(&d.join("nope"), 3, false).is_err());
    }

    /// A line still being written isn't a line yet: `follow` takes it from
    /// its start, whole, instead of a second half on its own.
    #[test]
    fn tail_stops_at_the_last_line_break() {
        let d = tmp("unfinished");
        let log = d.join("log");
        append(&log, "one\ntwo\nthr");
        let t = tail(&log, 5, false).unwrap();
        assert_eq!(t.lines, ["one", "two"]);
        assert_eq!(t.end, "one\ntwo\n".len() as u64);
        assert_eq!(t.unfinished.as_deref(), Some("thr"));
    }

    #[test]
    fn tail_reads_backwards_through_a_big_file_without_splitting_lines() {
        let d = tmp("big");
        let log = d.join("log");
        let mut text = String::new();
        for i in 0..20_000 {
            text.push_str(&format!("line {i} {}\n", "z".repeat(i % 37)));
        }
        append(&log, &text);
        let lines = tail(&log, 500, false).unwrap().lines;
        assert_eq!(lines.len(), 500);
        assert_eq!(lines[0], format!("line 19500 {}", "z".repeat(19500 % 37)));
        assert_eq!(lines[499], format!("line 19999 {}", "z".repeat(19999 % 37)));
        // And every line of it, which reads all 23 chunks once each.
        assert_eq!(tail(&log, 1_000_000, false).unwrap().lines.len(), 20_000);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn only_kept_leaves_out_a_file_written_before_c6() {
        let d = tmp("kept");
        let log = d.join("log");
        append(&previous(&log), "← @bob: a message\nwith a second line\n");
        append(&log, &format!("{}\nours\n", mark()));
        let t = tail(&log, 10, true).unwrap();
        assert_eq!(t.lines, [mark(), "ours".to_string()]);
        assert_eq!(t.left_out, [previous(&log)]);
        // Not asked to: everything.
        assert_eq!(tail(&log, 10, false).unwrap().lines.len(), 4);
        // The current file itself from before C6 (an old daemon still running).
        fs::write(&log, "old\n").unwrap();
        let t = tail(&log, 10, true).unwrap();
        assert!(t.lines.is_empty());
        assert_eq!(t.left_out, [log.clone()]);
    }

    #[test]
    fn follow_hands_over_new_lines_across_a_rotation() {
        let d = tmp("follow");
        let log = d.join("log");
        append(&log, "before\n");
        let end = tail(&log, 10, false).unwrap().end;
        let (tx, rx) = mpsc::channel();
        let path = log.clone();
        let reader = std::thread::spawn(move || {
            follow(&path, end, Duration::from_millis(20), |t| {
                let stop = t == Tick::Line("last".into());
                tx.send(t).unwrap();
                !stop
            })
            .unwrap();
        });
        let next = |rx: &mpsc::Receiver<Tick>| rx.recv_timeout(Duration::from_secs(10)).unwrap();
        append(&log, "one\ntw");
        assert_eq!(next(&rx), Tick::Line("one".into()));
        append(&log, "o\n");
        assert_eq!(next(&rx), Tick::Line("two".into()));
        // What a running process does: write a little more, then rotate.
        append(&log, &format!("three\n{}\n", "p".repeat(64)));
        assert_eq!(next(&rx), Tick::Line("three".into()));
        assert_eq!(next(&rx), Tick::Line("p".repeat(64)));
        // Moved aside — and a line still lands in the old file before the
        // writer points at the new one.
        let mut held = OpenOptions::new().append(true).open(&log).unwrap();
        assert!(rotate_if_over(&log, 10));
        held.write_all(b"late\n").unwrap();
        append(&log, "after\nlast\n");
        assert_eq!(next(&rx), Tick::Line("late".into()), "what the old file got after the rename isn't lost");
        assert_eq!(next(&rx), Tick::Rotated);
        assert_eq!(next(&rx), Tick::Line("after".into()));
        assert_eq!(next(&rx), Tick::Line("last".into()));
        reader.join().unwrap();
        assert!(rx.try_recv().is_err(), "nothing repeated, nothing before `from`, one Rotated");
    }

    /// `logs -f` carries on in the file it read the last lines of: a rotation
    /// between reading them and following still shows the old file's end,
    /// says it rotated, and starts the new one from its first line.
    #[test]
    fn following_from_the_file_tail_read_catches_a_rotation_in_between() {
        let d = tmp("between");
        let log = d.join("log");
        append(&log, "a\nb\n");
        let t = tail(&log, 10, false).unwrap();
        assert_eq!(t.lines, ["a", "b"]);
        append(&log, "c\n");
        assert!(rotate_if_over(&log, 1));
        append(&log, "d\n");
        let mut got = Vec::new();
        follow_on(t.file.unwrap(), &log, t.end, Duration::from_millis(20), |tick| {
            let done = tick == Tick::Line("d".into());
            got.push(tick);
            !done
        })
        .unwrap();
        assert_eq!(got, [Tick::Line("c".into()), Tick::Rotated, Tick::Line("d".into())]);
    }

    /// What starts a process with the log as its stdout: a file begun with
    /// the mark (so what it prints before [`keep`] is read like the rest),
    /// one from before C6 moved aside, ours carried on.
    #[test]
    fn a_log_opened_for_a_child_is_ours_from_its_first_line() {
        let d = tmp("child");
        let log = d.join("daemons").join("bot").join("log");
        writeln!(open_for_child(&log).unwrap(), "error: no bot token").unwrap();
        let first = fs::read_to_string(&log).unwrap();
        assert!(first.starts_with(KEPT) && first.ends_with("\nerror: no bot token\n"), "{first:?}");
        assert_eq!(tail(&log, 10, true).unwrap().lines.last().unwrap(), "error: no bot token");
        // Again: carried on, not begun twice.
        writeln!(open_for_child(&log).unwrap(), "error: no bot token").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap().matches(KEPT).count(), 1);
        // A file from before C6: moved aside whole.
        fs::write(&log, "← @bob: hi\nsecond line\n").unwrap();
        drop(open_for_child(&log).unwrap());
        assert!(kept(&log));
        assert!(fs::read_to_string(previous(&log)).unwrap().starts_with("← @bob: hi"));
    }

    /// Cut in place, the mark goes where an appending writer's next line would
    /// — never over the start of one already there.
    #[test]
    fn a_cut_log_begins_again_with_the_mark_and_writers_carry_on() {
        let d = tmp("cut");
        let log = d.join("log");
        let mut writer = OpenOptions::new().create(true).append(true).open(&log).unwrap();
        writer.write_all(format!("{}\n{}\n", mark(), "x".repeat(500)).as_bytes()).unwrap();
        cut(&log);
        writer.write_all(b"after the cut\n").unwrap();
        let got = fs::read_to_string(&log).unwrap();
        assert!(got.starts_with(KEPT), "{got:?}");
        assert!(got.ends_with("couldn't be moved aside\nafter the cut\n"), "{got:?}");
        assert!(!got.contains("xxx"));
    }

    #[test]
    fn keep_sends_stdout_and_stderr_to_the_file_and_moves_an_old_one_aside() {
        if let Some(log) = std::env::var_os("MAFOLD_LOGFILE_CHILD") {
            keep(Path::new(&log)).unwrap();
            println!("to stdout");
            eprintln!("to stderr");
            return;
        }
        let d = tmp("keep");
        let log = d.join("daemons").join("bot").join("log");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        // Written before C6, and far bigger than the limit.
        append(&log, &format!("← @bob: a message\nwith a second line\n{}\n", "o".repeat(ROTATE_AT as usize)));
        let run = || {
            let out = child("keep_sends_stdout_and_stderr_to_the_file_and_moves_an_old_one_aside", "MAFOLD_LOGFILE_CHILD", &log)
                .output()
                .unwrap();
            assert!(out.status.success(), "child: {}", String::from_utf8_lossy(&out.stderr));
            // Nothing reached the parent's pipes after the switch.
            assert!(!String::from_utf8_lossy(&out.stdout).contains("to stdout"));
        };
        run();
        let got = fs::read_to_string(&log).unwrap();
        assert!(got.starts_with(KEPT), "{got:?}");
        assert!(got.contains("to stdout\n") && got.contains("to stderr\n"), "{got:?}");
        assert!(fs::read_to_string(previous(&log)).unwrap().starts_with("← @bob: a message\n"));
        // Started again: ours already, small — carried on, not moved.
        run();
        let again = fs::read_to_string(&log).unwrap();
        assert!(again.starts_with(&got), "{again:?}");
        assert_eq!(again.matches("to stdout").count(), 2);
        assert!(fs::read_to_string(previous(&log)).unwrap().starts_with("← @bob: a message\n"));
        let _ = fs::remove_dir_all(&d);
    }

    /// The way the supervisor starts a bot: its stdout and stderr are a file
    /// the PARENT opened on the log, then the bot points them at the log
    /// itself and keeps writing while it rotates, again and again. On Windows
    /// that is a rename under handles still open (the inherited one, a reader)
    /// — every rotation has to succeed, not just the first, and the two files
    /// have to meet without a lost or repeated line.
    #[test]
    fn rotating_again_and_again_under_a_writer_loses_no_line_at_the_seam() {
        const LIMIT: u64 = 4_000;
        const LINES: usize = 300;
        if let Some(log) = std::env::var_os("MAFOLD_LOGFILE_ROTATE_CHILD") {
            keep_with(Path::new(&log), LIMIT, Duration::from_millis(40)).unwrap();
            for i in 0..LINES {
                println!("line {i:05} {}", "w".repeat(80));
                std::thread::sleep(Duration::from_millis(5));
            }
            return;
        }
        let d = tmp("seam");
        let log = d.join("log");
        let inherited = OpenOptions::new().create(true).append(true).open(&log).unwrap();
        let err = inherited.try_clone().unwrap();
        // A reader holding the file open across the rotations, the way
        // `mafold logs -f` or the desktop does.
        let reader = File::open(&log).unwrap();
        let out = child("rotating_again_and_again_under_a_writer_loses_no_line_at_the_seam", "MAFOLD_LOGFILE_ROTATE_CHILD", &log)
            .stdout(inherited)
            .stderr(err)
            .output()
            .unwrap();
        drop(reader);
        assert!(out.status.success(), "child failed");
        let numbers = |p: &Path| -> Vec<usize> {
            fs::read_to_string(p)
                .unwrap()
                .lines()
                .filter_map(|l| l.strip_prefix("line ")?.get(..5)?.parse().ok())
                .collect()
        };
        let (older, now) = (numbers(&previous(&log)), numbers(&log));
        assert!(!older.is_empty() && !now.is_empty(), "rotated at least once: {} then {}", older.len(), now.len());
        let seam: Vec<usize> = older.iter().chain(&now).copied().collect();
        assert!(seam.windows(2).all(|w| w[1] == w[0] + 1), "lines run on across the seam: {seam:?}");
        assert_eq!(*now.last().unwrap(), LINES - 1, "the last line is in the current file");
        // 300 lines of ~92 bytes are ~27 KB: only rotation after rotation keeps
        // both files near the limit (one check's worth of writing over it).
        for p in [log.clone(), previous(&log)] {
            let len = fs::metadata(&p).unwrap().len();
            assert!(len < LIMIT + 3_000, "{} is {len} bytes", p.display());
            assert!(kept(&p), "{} begins with the mark", p.display());
        }
        assert!(!d.join("log.2").exists() && !d.join("log.1.1").exists());
        let _ = fs::remove_dir_all(&d);
    }

    /// Somebody deletes the log while the process runs (a person tidying up,
    /// another process moving it): within a check it writes a new one, not
    /// into a file nobody can see.
    #[test]
    fn a_log_deleted_under_a_running_process_is_written_again() {
        if let Some(log) = std::env::var_os("MAFOLD_LOGFILE_GONE_CHILD") {
            let log = Path::new(&log);
            keep_with(log, ROTATE_AT, Duration::from_millis(30)).unwrap();
            println!("before");
            fs::remove_file(log).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            println!("after");
            return;
        }
        let d = tmp("gone");
        let log = d.join("log");
        let out = child("a_log_deleted_under_a_running_process_is_written_again", "MAFOLD_LOGFILE_GONE_CHILD", &log).output().unwrap();
        assert!(out.status.success(), "child: {}", String::from_utf8_lossy(&out.stderr));
        let got = fs::read_to_string(&log).unwrap();
        assert!(got.starts_with(KEPT) && got.contains("after\n") && !got.contains("before"), "{got:?}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_quote_is_one_line_after_the_lines_own_words() {
        assert_eq!(said("alice", "hi: there"), "← @alice ⟫ hi: there");
        assert_eq!(said("alice", "one\ntwo\r\nthree\rfour"), "← @alice ⟫ one\\ntwo\\nthree\\nfour");
        // A quote that holds the separator itself is still all quote.
        assert_eq!(quoting("x", "a ⟫ b"), "x ⟫ a ⟫ b");
    }
}
