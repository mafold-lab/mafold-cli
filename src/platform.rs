//! Cross-platform process + file-lock shims.
//!
//! The daemon/supervisor logic is identical on every OS — only the primitives
//! differ: detaching a child from the controlling terminal/console, checking a
//! pid's liveness, terminating it, reaping zombies, and holding a cross-process
//! file lock. Each is implemented once per OS here so the callers stay portable.
//!
//! Unix keeps its exact previous behavior (setsid + `libc::kill`/`waitpid` +
//! `flock`); Windows gets the native equivalents (detached creation flags +
//! `OpenProcess`/`TerminateProcess` + `LockFileEx`).

use std::process::Command;

// ───────────────────────────── Unix ─────────────────────────────
#[cfg(unix)]
mod imp {
    use super::Command;
    use std::os::unix::process::CommandExt;

    /// Is `pid` a live process? `kill(pid, 0)` probes without signalling.
    pub fn pid_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// Is `pid` — a CHILD of this process — still running?
    ///
    /// Not [`pid_alive`]: a child that exited but that nobody has reaped yet is
    /// a zombie, and a zombie answers `kill(pid, 0)`. tokio reaps a child only
    /// when its owner awaits `wait()`, which a harness does after the turn's
    /// stream ends — so between "the process died" and "the turn noticed", the
    /// zombie is all there is, and it would read as alive.
    ///
    /// `waitid(…, WNOWAIT)` asks the kernel for the child's state WITHOUT
    /// reaping it, so the exit status is still there for the `wait()` that owns
    /// it. A pid that is no longer our child — reaped already, or since reused
    /// by an unrelated process — answers `ECHILD`, so pid reuse can't fool this
    /// either.
    pub fn child_running(pid: u32) -> bool {
        loop {
            // SAFETY: `info` is a plain C struct the call fills in; zeroed first
            // so "no state change" (the WNOHANG case) reads as si_pid == 0.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            // A signal landing mid-call says nothing about the child — and a
            // "not running" answer can end its turn (`harness::ExitWatch`).
            if r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return r == 0 && siginfo_pid(&info) == 0;
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
        // SAFETY: filled in by `waitid` (or zeroed), so the union reads a pid.
        unsafe { info.si_pid() }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
        info.si_pid
    }

    /// Ask `pid` to stop (SIGTERM — the process can clean up).
    pub fn terminate(pid: u32) {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }

    // NOTE deliberately NO terminate_group here: group-killing a daemon also
    // took down the agent's legitimate cross-turn background tasks (they share
    // the pgroup) — the 2026-07-19 regression. In-flight claude children are
    // killed precisely by the daemon's own shutdown handler instead
    // (harness::live_children).

    /// Configure `cmd` to spawn in a new session, detached from the controlling
    /// terminal, so it survives the launching shell closing.
    pub fn configure_detached(cmd: &mut Command) {
        // SAFETY: setsid() is async-signal-safe and the closure only calls it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    /// Reap any exited children so a zombie pid doesn't fool the liveness check
    /// (a zombie still answers `kill(pid, 0)`).
    pub fn reap_children() {
        loop {
            let mut status = 0;
            let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if r <= 0 {
                break; // 0 = children exist but none exited; -1 = no children
            }
        }
    }

    /// Does this process run with administrative rights — root, here? Every
    /// command it starts inherits exactly that; mafold never raises or lowers it.
    pub fn elevated() -> bool {
        unsafe { libc::geteuid() == 0 }
    }
}

// ──────────────────────────── Windows ───────────────────────────
#[cfg(windows)]
mod imp {
    use super::Command;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess};

    // Process-creation flags (winbase.h). DETACHED_PROCESS gives the child no
    // console → it survives the parent console closing; CREATE_NEW_PROCESS_GROUP
    // stops a parent-console Ctrl+C from propagating into it.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    // OpenProcess access rights (processthreadsapi.h / winnt.h).
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    /// An exited process can remain openable while another process holds its
    /// handle. Query its exit state so background completion is not suppressed.
    pub fn pid_alive(pid: u32) -> bool {
        child_running(pid)
    }

    /// Is `pid` — a CHILD of this process — still running?
    ///
    /// Not [`pid_alive`]: the `Child` that spawned it holds a handle, and an
    /// exited process stays openable for as long as any handle to it is open.
    /// Its exit code is the real answer — `STILL_ACTIVE` until it exits. (The
    /// open handle also keeps the pid from being reused under us.)
    pub fn child_running(pid: u32) -> bool {
        use windows_sys::Win32::System::Threading::GetExitCodeProcess;
        const STILL_ACTIVE: u32 = 259;
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE
        }
    }

    /// Stop `pid` (TerminateProcess — there is no graceful SIGTERM analogue we
    /// can rely on for a console-less child, so this is a forceful stop).
    pub fn terminate(pid: u32) {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    /// Configure `cmd` to spawn detached from the console so it survives the
    /// launching shell closing. Replaces the creation flags the constructor set:
    /// a process with NO console has no window to hide.
    pub fn configure_detached(cmd: &mut Command) {
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    /// No zombies on Windows — nothing to reap.
    pub fn reap_children() {}

    /// Is this process attached to a console at all — visible, windowless or
    /// headless alike? `GetConsoleWindow` can't say: it is NULL both for "no
    /// console" and for a console that simply has no window. The process list
    /// can: it has at least us on it, and the call fails without a console.
    pub fn has_console() -> bool {
        use windows_sys::Win32::System::Console::GetConsoleProcessList;
        let mut pids = [0u32; 1];
        unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 1) != 0 }
    }

    /// Does this process run with administrative rights — an elevated token,
    /// here? An administrator's console that wasn't opened "as administrator"
    /// runs with the filtered (Medium) token and answers no. Every command
    /// this process starts inherits exactly what it has; mafold never raises
    /// or lowers it.
    pub fn elevated() -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }
}

pub use imp::*;

/// Hide the console window this process owns (Windows). A supervisor launched
/// by the Task Scheduler logon task is a console binary, so Windows hands it a
/// fresh visible console at sign-in; it runs headless, so `supervise --hidden`
/// hides that window right after startup. `GetConsoleWindow` returns NULL when
/// there is no console (already detached) — nothing to hide then. No-op on Unix
/// (launchd/systemd services never get a terminal).
pub fn hide_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::GetConsoleWindow;
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
        unsafe {
            let hwnd = GetConsoleWindow();
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

// ──────────────────────── starting a program ────────────────────────
// A console program started by a process with NO console of its own is handed
// a brand-new console by Windows — and on Windows 11, a Windows Terminal window
// to show it in. Every daemon runs that way (`configure_detached`), so anything
// a daemon starts without CREATE_NO_WINDOW opens a black window on the screen
// of whoever owns the machine. That flag used to be something each spawn site
// had to remember. The warm `claude` (#457) forgot, and from then on every
// message to a Windows agent opened a window titled «claude» that stayed open
// as long as the process did. So nothing outside this module builds a
// `Command`: `tests::every_spawn_goes_through_platform` fails the build when
// anything does, and every constructor here keeps the window away.

/// THE way mafold starts a program. Never a console window: on Windows the
/// child gets a console of its own that has none (CREATE_NO_WINDOW). Its output
/// is read over pipes or files, so nothing else changes. Plain on Unix.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// [`command`], blocking.
pub fn std_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// For a program that talks to the person at this terminal: it reads what they
/// type or prints straight to it (`connection run`'s stdin, the installer,
/// `stty`, esbuild's watch). It shares OUR console, which is the point. When
/// we have none (a daemon), it gets a console with no window instead of a new
/// window.
pub fn console_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = tokio::process::Command::new(program);
    #[cfg(windows)]
    if !has_console() {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// [`console_command`], blocking.
pub fn console_std_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    if !has_console() {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Process-creation flag (winbase.h): the child's console has no window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Best-effort "open this URL in the default browser" (`mafold login`'s device
/// flow, à la `gh auth login`). Headless boxes and weird shells just fail the
/// spawn — the caller always prints the URL too, so nothing depends on this.
pub fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std_command("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std_command("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        // `start` is a cmd.exe builtin; the empty "" is its window-title slot,
        // which otherwise eats a quoted URL.
        let mut c = std_command("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A worker-owned kill-on-close job. Keeping its handle in the worker (rather
/// than the daemon) makes daemon restarts harmless and worker termination end
/// the complete task tree. The handle is never inherited by children.
#[cfg(windows)]
pub struct BackgroundJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for BackgroundJob {
    fn drop(&mut self) { unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0); } }
}
#[cfg(windows)]
pub fn background_job() -> std::io::Result<BackgroundJob> {
    use windows_sys::Win32::System::{JobObjects::*, Threading::GetCurrentProcess};
    unsafe {
        let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if handle.is_null() { return Err(std::io::Error::last_os_error()); }
        let job = BackgroundJob(handle);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(handle, JobObjectExtendedLimitInformation,
            &info as *const _ as _, std::mem::size_of_val(&info) as u32) == 0
            || AssignProcessToJobObject(handle, GetCurrentProcess()) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }
}

// ────────────────────── cross-process file lock ──────────────────────
// `update.rs` holds an exclusive lock on `~/.mafold/update.lock` while it swaps
// the binary. Unix does this inline with `flock`; Windows needs `LockFileEx`.

/// Block until an exclusive lock is held on `file` (released when the file's
/// handle closes). Windows-only — Unix uses `flock` directly in `update.rs`.
#[cfg(windows)]
pub fn lock_file_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ──────────────────────────── re-exec ────────────────────────────
// Self-update restarts into the freshly-swapped binary. Unix replaces the
// process image via `exec()` (keeps the same pid). Windows has no `exec`, so we
// spawn a fresh copy with the same args/env and exit.

/// Replace the current process with a fresh `mafold` of the same args. Never
/// returns on success.
pub fn reexec() -> std::io::Error {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("mafold"));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Command::new(exe).args(std::env::args_os().skip(1)).exec()
    }
    // The successor takes over our console as it is: it shares the one we
    // have, and a daemon with none (`configure_detached`) stays without one —
    // given nothing, Windows would open the new copy a window of its own.
    #[cfg(windows)]
    {
        let mut cmd = Command::new(exe);
        cmd.args(std::env::args_os().skip(1));
        if !has_console() {
            configure_detached(&mut cmd);
        }
        match cmd.spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => e,
        }
    }
}

/// A child that sits there until it is killed — the stand-in for a harness
/// process in tests, on every OS the daemon ships to. Stdio detached: were an
/// assertion to leak one, it must not hold the test runner's output open for
/// ten minutes.
#[cfg(test)]
pub fn idle_child() -> std::process::Child {
    use std::process::Stdio;
    #[cfg(unix)]
    let mut cmd = {
        let mut c = std_command("sleep");
        c.arg("600");
        c
    };
    // No `sleep` on Windows, and `timeout` refuses to run without a console;
    // `ping` is on every install and waits a second between echoes.
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std_command("ping");
        c.args(["-n", "601", "127.0.0.1"]);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn an idle child")
}

/// A program started the way every spawn site started one before this module
/// owned spawning: no flags at all. The console-window tests' positive
/// control — what a child of a console-less process looks like when nothing
/// keeps its window away.
#[cfg(all(test, windows))]
pub fn unhidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(program)
}

/// Does `pid` have a console window? `None` when it can't be asked: gone, or
/// no console at all. It borrows `pid`'s console to ask, then lets go — and a
/// process can only be attached to one console, so a caller that has its own
/// (a test run from a terminal or CI step) asks through a console-less copy of
/// this test binary, started the way a daemon is. Retries for a few seconds,
/// because a child that was just started connects to its console a moment
/// after `spawn` returns.
#[cfg(all(test, windows))]
pub fn console_window_of(pid: u32) -> Option<bool> {
    use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole, GetConsoleWindow};
    if has_console() {
        let mut cmd = std_command(std::env::current_exe().ok()?);
        cmd.args(["--exact", "platform::tests::console_window_probe"])
            .args(["--include-ignored", "--test-threads=1", "--nocapture"])
            .env("MAFOLD_CONSOLE_PROBE_PID", pid.to_string())
            .stdin(std::process::Stdio::null());
        configure_detached(&mut cmd);
        let out = cmd.output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // libtest prints `test … ... ` and then the test's own output on the
        // same line, so the answer is found anywhere, not at a line's start.
        let answer = text.split("CONSOLE_WINDOW=").nth(1).and_then(|r| r.split_whitespace().next());
        return match answer {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => {
                eprintln!(
                    "console_window_of({pid}): the console-less probe answered nothing usable:\n{text}\n{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                None
            }
        };
    }
    let began = std::time::Instant::now();
    loop {
        // SAFETY: plain Win32 calls; FreeConsole lets go of what AttachConsole took.
        unsafe {
            if AttachConsole(pid) != 0 {
                let window = !GetConsoleWindow().is_null();
                let seen = has_console();
                FreeConsole();
                assert!(seen, "attached to {pid}'s console, yet has_console() says there is none");
                return Some(window);
            }
        }
        if began.elapsed() > std::time::Duration::from_secs(5) {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `console_window_of`'s console-less half: asks about
    /// `MAFOLD_CONSOLE_PROBE_PID` and prints the answer for the caller.
    #[cfg(windows)]
    #[test]
    #[ignore = "run by console_window_of from a process that has a console"]
    fn console_window_probe() {
        let Some(pid) = std::env::var("MAFOLD_CONSOLE_PROBE_PID").ok().and_then(|p| p.parse().ok()) else {
            return;
        };
        assert!(!has_console(), "started detached, so no console of its own");
        let answer = match console_window_of(pid) {
            Some(true) => "true",
            Some(false) => "false",
            None => "none",
        };
        println!("\nCONSOLE_WINDOW={answer}");
    }

    /// Every program mafold starts goes through [`command`] / [`std_command`]
    /// or [`console_command`] / [`console_std_command`], and only this module
    /// sets creation flags. A bare `Command::new` somewhere else is how the
    /// console window came back each time it was fixed: the fix lived at the
    /// spawn sites, and the next new spawn site didn't have it. This holds on
    /// every OS, so the Linux CI catches a Windows-only bug before any Windows
    /// machine runs it.
    #[test]
    fn every_spawn_goes_through_platform() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).expect("read src").flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        assert!(files.len() > 20, "found the sources: {}", src.display());
        let mut bare = Vec::new();
        for f in files.iter().filter(|f| !f.ends_with("platform.rs")) {
            let text = std::fs::read_to_string(f).unwrap();
            for (i, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if ["Command::new(", "Command::from(", "creation_flags("].iter().any(|n| line.contains(n)) {
                    let rel = f.strip_prefix(&src).unwrap_or(f).display().to_string();
                    bare.push(format!("  src/{}:{}  {}", rel.replace('\\', "/"), i + 1, line.trim()));
                }
            }
        }
        assert!(
            bare.is_empty(),
            "start programs with crate::platform::{{command, std_command, console_command, \
             console_std_command}}. A bare Command started by a daemon opens a console window on \
             Windows:\n{}",
            bare.join("\n")
        );
    }

    /// The daemon's heartbeat asks this about the process running a turn, and
    /// the hard case is the one in between: exited, but not yet let go of. On
    /// unix that is a zombie, which still answers `kill(pid, 0)`; on Windows it
    /// is a process object our `Child` still holds a handle to, which still
    /// answers `OpenProcess`. Both Windows liveness probes must say no, without
    /// reaping, or the harness that owns the child loses its exit
    /// status.
    #[test]
    fn child_running_sees_through_a_dead_unreleased_child() {
        let mut child = idle_child();
        let pid = child.id();
        assert!(child_running(pid), "a live child");

        child.kill().expect("kill it");
        let t = std::time::Instant::now();
        while child_running(pid) && t.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!child_running(pid), "dead, not yet reaped: not running");
        #[cfg(unix)]
        assert!(pid_alive(pid), "positive control: the Unix pid probe sees an unreaped zombie");
        #[cfg(windows)]
        {
            // The old probe confused an openable object with a running task.
            use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::OpenProcess};
            let handle = unsafe { OpenProcess(0x1000, 0, pid) };
            assert!(!handle.is_null(), "positive control: exited process is still openable");
            unsafe { CloseHandle(handle); }
            assert!(!pid_alive(pid), "background completion must see through a held handle too");
        }

        let status = child.wait().expect("the owner still reaps it");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(libc::SIGKILL), "…and still gets its real exit status");
        }
        // std's `kill` is `TerminateProcess(handle, 1)`.
        #[cfg(windows)]
        assert_eq!(status.code(), Some(1), "…and still gets its real exit status");
    }

    /// `waitid` only answers for OUR children, which is what makes a reaped or
    /// reused pid read as "not running". (Windows needs no such case: the
    /// handle a harness holds keeps its pid from being reused at all.)
    #[cfg(unix)]
    #[test]
    fn child_running_answers_only_for_our_own_children() {
        let mut child = idle_child();
        let pid = child.id();
        child.kill().expect("kill it");
        child.wait().expect("reap it");
        assert!(!child_running(pid), "reaped");
        assert!(!child_running(std::process::id()), "not our child: ourselves");
        assert!(!child_running(1), "not our child: init");
    }
}
