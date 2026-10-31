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

    /// Is anyone left in process group `pgid` (ours to signal)? A detached
    /// background task is its group (`bash_hook::spawn_detached` setsid's it):
    /// its shell can be gone while what it ran lives on.
    pub fn group_alive(pgid: u32) -> bool {
        pgid > 1 && unsafe { libc::kill(-(pgid as i32), 0) == 0 }
    }

    /// Is `pid` still running — alive and NOT a zombie? For asking about a
    /// process that is somebody else's child: a daemon the supervisor started
    /// stays a zombie from the moment it exits until the supervisor's next
    /// tick reaps it, and a zombie answers `kill(pid, 0)` — so `mafold down`,
    /// run from outside, would take a daemon that already stopped for one that
    /// won't.
    pub fn pid_running(pid: u32) -> bool {
        pid_alive(pid) && !zombie(pid)
    }

    #[cfg(target_os = "linux")]
    fn zombie(pid: u32) -> bool {
        // `pid (comm) state …` — comm may itself hold spaces and parentheses,
        // so the state is the first field after the LAST `)`.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit_once(')').map(|(_, rest)| rest.trim_start().starts_with('Z')))
            .unwrap_or(false)
    }

    #[cfg(target_os = "macos")]
    fn zombie(pid: u32) -> bool {
        // What `ps` reads: `kinfo_proc.kp_proc.p_stat`, from sysctl
        // KERN_PROC_PID. libc doesn't define the struct, but its 64-bit layout
        // is fixed: 648 bytes, `p_stat` at offset 36, near the head of
        // `kp_proc` (so a struct that ever grew at the tail reads the same).
        // `proc_pidinfo` won't say: it answers ESRCH for a zombie — measured
        // on macOS 26.
        const SIZE: usize = 648;
        const P_STAT: usize = 36;
        let mut buf = [0u8; SIZE];
        let mut len = SIZE;
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid as libc::c_int];
        // SAFETY: the kernel writes at most `len` bytes into `buf` and sets
        // `len` to what it wrote (0 when there's no such process).
        let r = unsafe {
            libc::sysctl(mib.as_mut_ptr(), mib.len() as libc::c_uint, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0)
        };
        r == 0 && len > P_STAT && u32::from(buf[P_STAT]) == libc::SZOMB
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn zombie(_pid: u32) -> bool {
        false
    }

    /// When `pid` started, as an opaque token that only has to compare equal
    /// for the same process — so a pid that has since been handed to another
    /// process reads as a different one. `None` when nobody by that pid can be
    /// asked (gone, a zombie, another user's).
    ///
    /// macOS: `proc_bsdinfo.pbi_start_tv*`, in microseconds.
    #[cfg(target_os = "macos")]
    pub fn pid_start(pid: u32) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the kernel writes at most `size` bytes into `info`.
        let n = unsafe {
            libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size)
        };
        (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    /// Linux: field 22 of `/proc/<pid>/stat`, clock ticks since boot.
    #[cfg(target_os = "linux")]
    pub fn pid_start(pid: u32) -> Option<u64> {
        // `pid (comm) state …` — comm may hold spaces and parentheses, so the
        // fields are counted from the LAST `)`; starttime is the 20th after it.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn pid_start(_pid: u32) -> Option<u64> {
        None
    }

    /// [`pid_start`] — what a journal keeps of a process it names, to tell it
    /// from a later one handed the same pid.
    pub fn start_token(pid: u32) -> Option<u64> {
        pid_start(pid)
    }

    /// The program `pid` is running, when this process may ask. A binary
    /// replaced on disk since it started still names its old path.
    #[cfg(target_os = "linux")]
    pub fn exe_of(pid: u32) -> Option<std::path::PathBuf> {
        let p = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        match p.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            Some(was) => Some(std::path::PathBuf::from(was)),
            None => Some(p),
        }
    }

    /// The program `pid` is running, when this process may ask.
    #[cfg(target_os = "macos")]
    pub fn exe_of(pid: u32) -> Option<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the kernel writes at most `buf.len()` bytes and returns how many.
        let n = unsafe { libc::proc_pidpath(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(buf)))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn exe_of(_pid: u32) -> Option<std::path::PathBuf> {
        None
    }

    /// When this machine last booted, epoch seconds.
    #[cfg(target_os = "linux")]
    pub fn boot_time() -> Option<i64> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        stat.lines().find_map(|l| l.strip_prefix("btime "))?.trim().parse().ok()
    }

    /// When this machine last booted, epoch seconds.
    #[cfg(target_os = "macos")]
    pub fn boot_time() -> Option<i64> {
        let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::timeval>();
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        // SAFETY: the kernel writes at most `len` bytes — one `timeval`.
        let r = unsafe {
            libc::sysctl(mib.as_mut_ptr(), mib.len() as libc::c_uint, (&mut tv as *mut libc::timeval).cast(), &mut len, std::ptr::null_mut(), 0)
        };
        (r == 0 && tv.tv_sec > 0).then_some(tv.tv_sec as i64)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn boot_time() -> Option<i64> {
        None
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

    /// No zombies on Windows: alive is running.
    pub fn pid_running(pid: u32) -> bool {
        pid_alive(pid)
    }

    /// Not needed here: a Windows background task's worker always leaves an
    /// exit receipt, and that answers before any pid is asked.
    pub fn pid_start(_pid: u32) -> Option<u64> {
        None
    }

    /// When `pid` started (FILETIME ticks) — what a journal keeps of a
    /// process it names, to tell it from a later one handed the same pid,
    /// which Windows does soon and often.
    pub fn start_token(pid: u32) -> Option<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let t = started_of(h);
            CloseHandle(h);
            (t != 0).then_some(t)
        }
    }

    /// The program `pid` is running, when this process may ask.
    pub fn exe_of(pid: u32) -> Option<std::path::PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::System::Threading::QueryFullProcessImageNameW;
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 32 * 1024];
            let mut len = buf.len() as u32;
            // 0 = PROCESS_NAME_WIN32: a `C:\…` path, not a device path.
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
            CloseHandle(h);
            ok.then(|| std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize])))
        }
    }

    /// No process groups to ask: as above, the receipt answers.
    pub fn group_alive(_pgid: u32) -> bool {
        false
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

    /// Every process running: its pid, its parent's, and when it started.
    pub fn processes() -> Vec<super::Proc> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        };
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snap, &mut e) != 0;
            while more {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.th32ProcessID);
                let started = if h.is_null() {
                    0
                } else {
                    let t = started_of(h);
                    CloseHandle(h);
                    t
                };
                out.push(super::Proc { pid: e.th32ProcessID, ppid: e.th32ParentProcessID, started });
                more = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    /// When the process `h` started, FILETIME ticks; 0 if it can't say.
    unsafe fn started_of(h: windows_sys::Win32::Foundation::HANDLE) -> u64 {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        if GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) == 0 {
            return 0;
        }
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
    }

    /// Now, on the clock [`processes`] says when each started on.
    fn now_ticks() -> u64 {
        use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
        let mut t = windows_sys::Win32::Foundation::FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        unsafe { GetSystemTimeAsFileTime(&mut t) };
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    }

    /// End `p` — if the pid still is that process, not one started under a
    /// reused pid since the list was taken.
    fn end_one(p: super::Proc) {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, p.pid);
            if h.is_null() {
                return;
            }
            if p.started == 0 || started_of(h) == p.started {
                TerminateProcess(h, 1);
            }
            CloseHandle(h);
        }
    }

    /// End `root` and everything it started ([`super::tree_of`]), but what
    /// `spare` names and what runs under it — and never the process calling
    /// this, which is inside that tree when a bot's own turn stops its daemon
    /// (`mafold down <itself>`): it would end itself halfway through. Swept
    /// twice more, for a process one of them started in the moment before
    /// it was ended.
    ///
    /// `by`: the latest `root` can have started (ticks, as [`super::Proc::started`])
    /// — its pid comes from a note written after it started. A process that
    /// started later took the pid since (a pid file left from before a
    /// reboot), and nothing is ended; nor when its start can't be read, as it
    /// then can't be told from one. `false`: left be, for either reason.
    pub fn end_tree(root: u32, by: u64, spare: &[super::Spare]) -> bool {
        let procs = processes();
        let Some(root) = procs.iter().find(|p| p.pid == root).copied() else { return true };
        if root.started == 0 || root.started > by {
            return false;
        }
        let mut spare = spare.to_vec();
        spare.push(super::Spare { pid: std::process::id(), by: u64::MAX });
        let spare = &spare[..];
        let mut ended: Vec<(super::Proc, u64)> = Vec::new();
        let mut doomed = super::tree_of(root, &procs, spare, u64::MAX);
        for _ in 0..3 {
            if doomed.is_empty() {
                break;
            }
            for p in doomed.drain(..) {
                let at = now_ticks();
                end_one(p);
                ended.push((p, at));
            }
            let procs = processes();
            for (p, at) in &ended {
                for c in super::tree_of(*p, &procs, spare, *at).into_iter().skip(1) {
                    let done = ended.iter().any(|(e, _)| e.pid == c.pid && e.started == c.started);
                    if !done && !doomed.contains(&c) {
                        doomed.push(c);
                    }
                }
            }
        }
        true
    }

    /// Configure `cmd` to spawn detached from the console so it survives the
    /// launching shell closing. Replaces the creation flags the constructor set:
    /// a process with NO console has no window to hide.
    pub fn configure_detached(cmd: &mut Command) {
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    /// No zombies on Windows — nothing to reap.
    pub fn reap_children() {}

    /// When this machine last booted, epoch seconds: now less the uptime
    /// (a second or so either way).
    pub fn boot_time() -> Option<i64> {
        use windows_sys::Win32::System::SystemInformation::GetTickCount64;
        let up = unsafe { GetTickCount64() } / 1000;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        now.checked_sub(up).map(|t| t as i64)
    }

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

/// A process and everything it starts, as one thing to end — Windows' process
/// group. Unix gets that from `setsid`/`process_group` + `killpg`; Windows has
/// no process group to signal, so a timeout that killed only the shell left
/// what the shell started running (a `shell.exec` ran as long as it liked; an
/// installer's download carried on). A job object is the group: everything
/// the process starts joins it, and [`JobTree::kill`] ends the lot. No
/// `KILL_ON_JOB_CLOSE` — a process that FINISHES leaves whatever it started
/// running, as on Unix. Put the process in at once, before it has started
/// anything.
#[cfg(windows)]
pub struct JobTree(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl JobTree {
    pub fn of(process: std::os::windows::io::RawHandle) -> Option<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            if AssignProcessToJobObject(job, process as _) == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }

    pub fn kill(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for JobTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

// The handle is only ever used to terminate the job, from whichever task owns
// the process.
#[cfg(windows)]
unsafe impl Send for JobTree {}

/// The user's home: `$HOME`, else (Windows, where it's usually unset)
/// `%USERPROFILE%`.
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(std::path::PathBuf::from)
}

/// Ask `pid` to stop and wait up to `max` until it has; true once it's gone.
/// What a command that stops something waits on before it says it did — a
/// "✓ stopped" printed right after the signal was a guess.
pub fn stop_and_wait(pid: u32, max: std::time::Duration) -> bool {
    terminate(pid);
    wait_until(max, || !pid_running(pid))
}

/// Check `done` every 100 ms for up to `max`; true as soon as it holds.
pub fn wait_until(max: std::time::Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    loop {
        if done() {
            return true;
        }
        if start.elapsed() >= max {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// ─────────────────────── ending a process tree ───────────────────────
// On Windows a process outlives its parent, and nothing reaches a console-less
// daemon the way SIGTERM does on unix, where the daemon's own shutdown ends
// the claude processes it started. Ending only the daemon there left those
// running — mid-turn, with the hooks, MCP servers and tool shells under them —
// next to the daemon that replaced it (activity center C7). So a daemon is
// ended with what it started (`end_tree`), but for the background tasks,
// which outlive it on purpose.

/// A process, as a list of all of them gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// When it started, in the OS's own ticks; 0 when that couldn't be asked.
    pub started: u64,
}

/// A process to leave be, and all under it: the one with this pid that
/// started no later than `by` (ticks, as [`Proc::started`]) — a pid named in
/// a note from before is somebody else's once a newer process has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spare {
    pub pid: u32,
    pub by: u64,
}

/// `root` and what it started, out of `procs`: parents before children. A
/// child names its parent's pid AND started after it — pids are reused, and
/// a process older than the one it names as parent is not its child. `root`
/// ended at `before`: a process naming it that started later is a reused
/// pid's child, not its own (`u64::MAX` while it runs). What `spare` names,
/// and all under it, stays out.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn tree_of(root: Proc, procs: &[Proc], spare: &[Spare], before: u64) -> Vec<Proc> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        let until = if i == 0 { before } else { u64::MAX };
        for p in procs {
            let child = p.ppid == parent.pid && p.pid != parent.pid && p.started != 0 && p.started >= parent.started && p.started <= until;
            let spared = spare.iter().any(|s| s.pid == p.pid && p.started <= s.by);
            if child && !spared && !out.iter().any(|o| o.pid == p.pid) {
                out.push(*p);
            }
        }
        i += 1;
    }
    out
}

/// `t` in Windows' process-time ticks (100 ns since 1601) — what a note's
/// time is compared to, as [`Spare::by`].
#[cfg_attr(not(windows), allow(dead_code))]
pub fn ticks_of(t: std::time::SystemTime) -> u64 {
    const UNIX_AT: u64 = 116_444_736_000_000_000;
    let since = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    UNIX_AT + (since.as_nanos() / 100) as u64
}

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

/// `cmd.output()` for a program this process may have JUST WRITTEN (a
/// downloaded update, `mafold.old`): asked again while it is `Text file busy`.
/// Linux won't exec a file anything still has open for writing, and a child
/// another thread forked while we wrote it holds our descriptor until it
/// execs. The supervisor forks all the time, and so do parallel tests: main's
/// cli tests went red on 10-30 when `capabilities` of a fresh `mafold.old` hit
/// it, read as "too old" (`update_cli.rs:408`). Up to a second, then the error.
pub fn output_of_new_binary(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    let step = std::time::Duration::from_millis(20);
    let mut waited = std::time::Duration::ZERO;
    loop {
        match cmd.output() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && waited < std::time::Duration::from_secs(1) => {
                std::thread::sleep(step);
                waited += step;
            }
            r => return r,
        }
    }
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

/// This process's binary, by the path that names it on disk NOW.
///
/// Linux reports a running binary whose file was replaced — what an update
/// does, renaming the new one over it — as `<path> (deleted)`, a name nothing
/// answers to: exec'ing it failed, so the supervisor that had just installed
/// an update kept running the old one, and starting a daemon from it failed
/// too (activity center C8, caught by `tests/update_cli.rs` on Linux). The
/// path without the suffix is the new binary — the one these want.
pub fn current_exe() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe().map(on_disk)
}

fn on_disk(exe: std::path::PathBuf) -> std::path::PathBuf {
    if cfg!(target_os = "linux") {
        if let Some(path) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            return std::path::PathBuf::from(path);
        }
    }
    exe
}

/// Replace the current process with a fresh `mafold` of the same args. Never
/// returns on success.
pub fn reexec() -> std::io::Error {
    let exe = current_exe().unwrap_or_else(|_| std::path::PathBuf::from("mafold"));
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

/// A child that ignores SIGTERM — what "asked to stop, still running" looks
/// like — returned once its trap is in place. Killed (SIGKILL) and reaped when
/// dropped, so a failing assertion can't leave it looping forever.
#[cfg(all(test, unix))]
pub fn stubborn_child() -> KilledOnDrop {
    use std::io::BufRead;
    use std::process::Stdio;
    let mut child = std_command("sh")
        .args(["-c", "trap '' TERM; echo ready; while :; do sleep 1; done"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a shell that ignores SIGTERM");
    let mut line = String::new();
    let stdout = child.stdout.take().expect("piped");
    let guard = KilledOnDrop(child);
    std::io::BufReader::new(stdout).read_line(&mut line).expect("its trap is set");
    guard
}

/// See [`stubborn_child`].
#[cfg(all(test, unix))]
pub struct KilledOnDrop(pub std::process::Child);

#[cfg(all(test, unix))]
impl Drop for KilledOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

    /// A program still open for writing somewhere (here: by this test, as a
    /// forked child holds it on CI) is asked again until it isn't, not
    /// reported as failed: the writer lets go 150 ms in. One that never stops
    /// being written still errors, after the second. (Linux: macOS execs a
    /// file open for writing.)
    #[cfg(target_os = "linux")]
    #[test]
    fn a_new_binary_still_being_written_is_asked_again() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mafold-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("mafold.old");
        std::fs::write(&bin, "#!/bin/sh\necho mafold 0.9.1\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let writer = std::fs::OpenOptions::new().append(true).open(&bin).unwrap();
        let busy = std_command(&bin).output().expect_err("exec of a file open for writing");
        assert_eq!(busy.kind(), std::io::ErrorKind::ExecutableFileBusy, "{busy}");
        let t0 = std::time::Instant::now();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(writer);
        });
        let out = output_of_new_binary(&mut std_command(&bin)).expect("asked again once the writer let go");
        release.join().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "mafold 0.9.1");
        assert!(t0.elapsed() >= std::time::Duration::from_millis(140), "it waited for the writer: {:?}", t0.elapsed());

        let held = std::fs::OpenOptions::new().append(true).open(&bin).unwrap();
        let t0 = std::time::Instant::now();
        let e = output_of_new_binary(&mut std_command(&bin)).expect_err("still being written");
        assert_eq!(e.kind(), std::io::ErrorKind::ExecutableFileBusy);
        assert!(t0.elapsed() < std::time::Duration::from_secs(3), "gives up: {:?}", t0.elapsed());
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A binary replaced under a running process is named by its path on disk
    /// — the new binary — not Linux's `… (deleted)`; anywhere else, and any
    /// path that merely contains the words, is left as it is.
    #[test]
    fn a_replaced_binary_is_named_by_its_path_on_disk() {
        let p = |s: &str| std::path::PathBuf::from(s);
        let replaced = on_disk(p("/home/a/.mafold/mafold (deleted)"));
        if cfg!(target_os = "linux") {
            assert_eq!(replaced, p("/home/a/.mafold/mafold"));
        } else {
            assert_eq!(replaced, p("/home/a/.mafold/mafold (deleted)"));
        }
        assert_eq!(on_disk(p("/home/a/.mafold/mafold")), p("/home/a/.mafold/mafold"));
        assert_eq!(on_disk(p("/x (deleted)/mafold")), p("/x (deleted)/mafold"));
    }

    /// Only [`current_exe`] asks the OS for this binary's path. Every other
    /// place hands that path to a child — a hook, the permission server, a
    /// daemon — and one that asked `std::env::current_exe` itself got Linux's
    /// `… (deleted)` after an update: the old daemon, still answering while
    /// the supervisor waited to restart it, started a turn whose Claude Code
    /// found no permission tool and failed its first command (10-29, DEV).
    #[test]
    fn only_current_exe_asks_for_this_binarys_path() {
        fn scan(dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() {
                    scan(&path, out);
                } else if path.extension().is_some_and(|x| x == "rs") && !path.ends_with("src/platform.rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (i, line) in text.lines().enumerate() {
                        if line.contains(concat!("std::env::", "current_exe(")) {
                            out.push(format!("{}:{}", path.display(), i + 1));
                        }
                    }
                }
            }
        }
        let mut found = Vec::new();
        scan(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
        assert!(found.is_empty(), "use crate::platform::current_exe() instead: {found:#?}");
    }

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

    /// What `mafold down` asks about a daemon the supervisor started: exited
    /// but not yet reaped by its parent is NOT running, though `kill(pid, 0)`
    /// still answers for it (activity center C3). Asked here about our own
    /// child; nothing in `pid_running` depends on being the parent.
    #[test]
    fn a_dead_unreaped_process_is_not_running() {
        let mut child = idle_child();
        let pid = child.id();
        assert!(pid_running(pid), "a live process");
        child.kill().expect("kill it");
        assert!(
            wait_until(std::time::Duration::from_secs(5), || !pid_running(pid)),
            "dead, not yet reaped: not running"
        );
        #[cfg(unix)]
        assert!(pid_alive(pid), "positive control: kill(pid, 0) still answers for the zombie");
        child.wait().expect("reap it");
        assert!(!pid_running(pid));
    }

    fn proc(pid: u32, ppid: u32, started: u64) -> Proc {
        Proc { pid, ppid, started }
    }

    /// Who goes with a daemon on Windows (`end_tree`): what it started, and
    /// theirs — but what it is told to spare, and never a process that only
    /// names a pid one of them once had.
    #[test]
    fn a_tree_is_what_its_root_started() {
        let root = proc(10, 1, 100);
        let procs = [
            root,
            proc(11, 10, 110), // claude
            proc(12, 11, 120), // a hook claude started
            proc(13, 10, 130), // a background task's worker
            proc(14, 13, 140), // …and its shell
            proc(15, 10, 90),  // names pid 10, but is older: pid 10 was another process then
            proc(16, 2, 150),  // nothing to do with it
            proc(17, 12, 0),   // when it started couldn't be asked: not shown to be a child
        ];
        let pids = |v: Vec<Proc>| v.into_iter().map(|p| p.pid).collect::<Vec<_>>();
        assert_eq!(pids(tree_of(root, &procs, &[], u64::MAX)), vec![10, 11, 13, 12, 14], "parents before children");
        let worker = Spare { pid: 13, by: 135 };
        assert_eq!(pids(tree_of(root, &procs, &[worker], u64::MAX)), vec![10, 11, 12], "a spared one keeps what is under it");
        // The note sparing pid 13 was written before this process started:
        // it names a pid since reused, by one of the daemon's own.
        assert_eq!(pids(tree_of(root, &procs, &[Spare { pid: 13, by: 125 }], u64::MAX)), vec![10, 11, 13, 12, 14]);
        // Ended at 115: a process naming pid 10 that started later has a
        // reused pid for a parent. 12's parent, 11, still ran.
        assert_eq!(pids(tree_of(root, &procs, &[], 115)), vec![10, 11, 12]);
        // A note's time, on the clock process starts are read on (Windows').
        assert_eq!(ticks_of(std::time::UNIX_EPOCH), 116_444_736_000_000_000);
        assert_eq!(ticks_of(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)), 116_444_736_010_000_000);
    }

    /// The tree `win_a_tree_ends_with_its_root_but_what_it_spares` ends, the
    /// shape of a daemon's: this process (the daemon), a `cmd` running a
    /// `ping` (a background task's worker and its shell, to be spared) and two
    /// `ping`s (its claude processes) — each started the way mafold starts
    /// one, with a console of its own. Their pids go to
    /// `MAFOLD_TREE_PROBE_DIR`; then it waits to be ended.
    #[cfg(windows)]
    #[test]
    #[ignore = "started by win_a_tree_ends_with_its_root_but_what_it_spares"]
    fn tree_probe() {
        use std::process::Stdio;
        let Some(dir) = std::env::var_os("MAFOLD_TREE_PROBE_DIR").map(std::path::PathBuf::from) else { return };
        let start = |program: &str, args: &[&str]| {
            let mut c = std_command(program);
            c.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            c.spawn().expect("start a child").id()
        };
        let worker = start("cmd.exe", &["/d", "/c", "ping -n 120 127.0.0.1 >nul"]);
        let one = start("ping", &["-n", "121", "127.0.0.1"]);
        let two = start("ping", &["-n", "122", "127.0.0.1"]);
        std::fs::write(dir.join("pids"), format!("{worker} {one} {two}")).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(120));
    }

    /// `end_tree` on a real tree (activity center C7): `TerminateProcess` on
    /// a daemon alone left everything it had started running.
    #[cfg(windows)]
    #[test]
    fn win_a_tree_ends_with_its_root_but_what_it_spares() {
        use std::process::Stdio;
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("mafold-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut probe = std_command(std::env::current_exe().unwrap());
        probe
            .args(["--exact", "platform::tests::tree_probe", "--ignored", "--nocapture"])
            .env("MAFOLD_TREE_PROBE_DIR", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut root = probe.spawn().expect("start the probe");
        let r = root.id();
        let mut pids: Vec<u32> = Vec::new();
        let started = wait_until(Duration::from_secs(30), || {
            pids = std::fs::read_to_string(dir.join("pids")).unwrap_or_default().split_whitespace().filter_map(|p| p.parse().ok()).collect();
            pids.len() == 3
        });
        assert!(started, "the probe never started its tree");
        let (worker, one, two) = (pids[0], pids[1], pids[2]);
        // What runs under the one spared: its ping (and its console host).
        let mut under: Vec<u32> = Vec::new();
        let found = wait_until(Duration::from_secs(10), || {
            let procs = processes();
            under = procs
                .iter()
                .find(|p| p.pid == worker)
                .map(|w| tree_of(*w, &procs, &[], u64::MAX).into_iter().skip(1).map(|p| p.pid).collect())
                .unwrap_or_default();
            !under.is_empty()
        });
        assert!(found, "the spared cmd's ping");
        // A pid from a note written before the root started — a pid file left
        // from before a reboot — is somebody else's now: nothing is ended.
        let earlier = ticks_of(std::time::SystemTime::now() - Duration::from_secs(3600));
        end_tree(r, earlier, &[]);
        std::thread::sleep(Duration::from_secs(1));
        assert!(pid_running(r) && pid_running(one) && pid_running(two), "not the process the note was about");
        end_tree(r, ticks_of(std::time::SystemTime::now()), &[Spare { pid: worker, by: u64::MAX }]);
        let gone = |pid: u32| wait_until(Duration::from_secs(5), || !pid_running(pid));
        assert!(gone(r), "the root");
        assert!(gone(one) && gone(two), "started by the root, ended with it");
        assert!(pid_running(worker), "spared");
        assert!(under.iter().all(|p| pid_running(*p)), "…and what runs under it");
        end_tree(worker, u64::MAX, &[]);
        assert!(gone(worker) && under.iter().all(|p| gone(*p)), "until it is ended itself");
        let _ = root.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `stop_and_wait` says a process is gone only once it is.
    #[test]
    fn stop_and_wait_sees_a_process_that_stops() {
        let mut child = idle_child();
        let pid = child.id();
        assert!(
            !wait_until(std::time::Duration::from_millis(300), || !pid_running(pid)),
            "positive control: a live process doesn't read as gone by itself"
        );
        assert!(stop_and_wait(pid, std::time::Duration::from_secs(5)), "asked to stop, it stops");
        child.wait().expect("reap it");
    }

    /// …and says so when it isn't: a process that ignores SIGTERM is still
    /// running when the wait runs out. (Windows has no such process:
    /// `terminate` is `TerminateProcess` there.)
    #[cfg(unix)]
    #[test]
    fn stop_and_wait_says_no_for_a_process_that_keeps_running() {
        let child = stubborn_child();
        let pid = child.0.id();
        let t = std::time::Instant::now();
        assert!(!stop_and_wait(pid, std::time::Duration::from_secs(1)), "still running when the wait runs out");
        assert!(t.elapsed() >= std::time::Duration::from_secs(1), "it waited the whole time");
        assert!(pid_running(pid));
    }
}
