//! `mafold bash-hook` — Claude Code PreToolUse hook for Bash background tasks.
//!
//! `claude -p` SIGTERMs its background shells' process groups the moment the
//! process exits (verified 2026-07-19, with and without the daemon in the
//! loop), so a `run_in_background` task can never outlive its turn on its own.
//! This hook intercepts Bash calls with `run_in_background: true` BEFORE they
//! run and moves the work out of claude's kill radius:
//!   1. the command is written to `~/.mafold/bgtasks/<surface>.<ts>.sh`,
//!   2. the hook ITSELF spawns it in a new session (fork + setsid — macOS has
//!      no `setsid` utility) with output to the sibling `.log`; the hook exits
//!      right after, so init adopts the task,
//!   3. the pid lands in the sibling `.pid`; `.meta` records the exact cwd and
//!      scoped surface — forensics only (the filename IS the key restart
//!      recovery reads), so that `ls ~/.mafold/bgtasks` plus one `cat` answers
//!      "whose task is this, and where did it run",
//!   4. the tool input is rewritten (`updatedInput`) to a foreground `echo`
//!      telling the model the task is detached and reported next turn.
//! Anything that isn't a background Bash — or any internal failure — produces
//! NO output, so claude proceeds with the original call untouched.

use anyhow::Result;
use serde_json::Value;
use std::io::Read;
use std::path::Path;

pub fn run() -> Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    if let Some(out) = rewrite(&input) {
        println!("{out}");
    }
    Ok(())
}

fn rewrite(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    if v["tool_name"].as_str()? != "Bash" {
        return None;
    }
    let ti = &v["tool_input"];
    if ti["run_in_background"].as_bool() != Some(true) {
        return None;
    }
    // Off = the call goes through untouched, exactly as before detaching
    // existed — never a half-on state where the task survives but the agent
    // was told nothing does. The hook is claude's child and holds no flags, so
    // on Windows it asks the daemon that told the agent (`background_windows`).
    #[cfg(windows)]
    if !crate::background_windows::daemon_says_on() {
        return None;
    }
    detach(&v, ti)
}

/// Can a `run_in_background` Bash actually be moved out of claude's kill radius
/// on this platform? THE SINGLE SOURCE OF TRUTH for the whole promise chain —
/// `agent.rs` states it to the model in the system prompt, and only emits the
/// `{% mafold/bgtasks %}` card ("结果会出现在下一条回复里") for tasks this returned true
/// for. When it is false the agent must not claim a follow-up is coming.
///
/// Windows: only where the `windowsBackground` flag is on for this bot and its
/// broker is up — read live in the daemon, so the server switches it per
/// person (`background_windows::enabled`).
pub fn bg_detach_supported() -> bool {
    #[cfg(unix)]
    return true;
    #[cfg(windows)]
    return crate::background_windows::enabled();
    #[cfg(not(any(unix, windows)))]
    return false;
}

fn detach(v: &Value, ti: &Value) -> Option<String> {
    let command = ti["command"].as_str()?;
    let home = std::env::var("HOME").ok()?;
    let dir = Path::new(&home).join(".mafold").join("bgtasks");
    std::fs::create_dir_all(&dir).ok()?;
    // This turn's tasks watched per task (`crate::bgtasks`): the daemon read
    // the flag for it, and the turn says so (`turnenv::bg_events`).
    let events = crate::turnenv::bg_events();
    sweep_old(&dir, events);

    // Same registry key the daemon scans for (agent::bgtasks_scan): the SURFACE
    // claude was launched on — the bot, the conversation, and in a forum the
    // channel (`agent::surface_tag`). Every scope this ONE machine-wide
    // directory is shared across has to be in the key: leave out the channel
    // and #b's monitor collects #a's tasks; leave out the bot and every daemon
    // on the machine collects every other daemon's. Re-sanitized here (not
    // re-derived) so the two sides cannot drift — `agent::surface_tag` already
    // produced this alphabet, including the empty channel component that keeps
    // the key's arity readable.
    //
    // The fallbacks are deliberately unattributable: a daemon too old to export
    // MAFOLD_SURFACE writes a key no current daemon will adopt, which is the
    // safe direction to fail — one unreported task beats six bots reporting it.
    let tag: String = std::env::var("MAFOLD_SURFACE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("MAFOLD_CONV").ok())
        .unwrap_or_else(|| "untagged".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    // join, not with_extension — with_extension would eat `.{ts}` as the
    // extension and collide every task of the conversation onto one filename.
    let script = dir.join(format!("{tag}.{ts}.sh"));
    let log = dir.join(format!("{tag}.{ts}.log"));
    #[cfg(unix)]
    let pidf = dir.join(format!("{tag}.{ts}.pid"));
    let meta = dir.join(format!("{tag}.{ts}.meta"));
    // The Bash tool's own `timeout` (ms), which detaching used to throw away —
    // the model would ask for a 20-minute cap and get a task that could run
    // until the monitor gave up two hours later. Honoured only when the model
    // set one EXPLICITLY: claude's 2-minute default is for foreground commands,
    // and quietly applying it here would shoot every build in the head.
    let timeout_secs = ti["timeout"].as_u64().map(|ms| (ms / 1000).max(1));
    // Watched per task (`bgtaskEvents`, as the daemon read it for this turn):
    // the task leaves an exit receipt where `crate::bgtasks::life` looks.
    let receipt = events.then(|| dir.join(format!("{tag}.{ts}.exit")));
    std::fs::write(&script, script_body(command, timeout_secs, receipt.as_deref())).ok()?;

    // The tool call's cwd (claude passes it in the hook input); fall back to
    // the hook's own cwd (claude spawns hooks in the session cwd).
    let cwd = v["cwd"].as_str().map(String::from).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    })?;
    std::fs::write(
        &meta,
        serde_json::json!({
            "version": 3,
            "surface": tag,
            "cwd": cwd,
            // The command VERBATIM. The card used to reconstruct it by stripping
            // the shebang off the `.sh`, which stopped being the command the
            // moment the timeout watchdog joined it in there.
            "cmd": command,
            "timeout_secs": timeout_secs,
        })
        .to_string(),
    )
    .ok()?;
    #[cfg(unix)]
    let pid = spawn_detached(&script, &log, &cwd)?;
    #[cfg(windows)]
    let pid = match crate::background_windows::request(crate::background_windows::Task {
        script: script.clone(), log: log.clone(), cwd: cwd.into(), bash: true, timeout_secs,
    }) {
        Ok(pid) => pid,
        Err(e) => return Some(serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "deny",
            "permissionDecisionReason": format!("Mafold could not confirm background startup: {e}. Do not retry or start an unmanaged copy; inspect {} first.", log.display())
        }}).to_string()),
    };
    #[cfg(unix)]
    std::fs::write(&pidf, pid.to_string()).ok()?;
    // Watched per task: when that pid started, beside the command — a pid is
    // handed to another process soon after this one exits (~80 minutes on a
    // busy Mac), and a task that died without leaving its receipt must not
    // live on in the stranger (`crate::bgtasks::life`) — and that the task
    // leaves a receipt, so one that ends without any was killed outright
    // (`crate::bgtasks::Meta::receipt`). The `.pid` itself stays a bare
    // number — daemons from before this read it as one. Through a temp file
    // and a rename: a watcher reading `.meta` mid-write would otherwise find
    // it empty and fall back to the script.
    #[cfg(unix)]
    if events {
        let fresh = serde_json::json!({
            "version": 4,
            "surface": tag,
            "cwd": cwd,
            "cmd": command,
            "timeout_secs": timeout_secs,
            "pid_start": crate::platform::pid_start(pid),
            "receipt": true,
        });
        let tmp = meta.with_extension("meta.tmp");
        if std::fs::write(&tmp, fresh.to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &meta);
        }
    }

    let cap = match timeout_secs {
        Some(s) => format!(" It is capped at {s}s (the timeout you passed) and killed if it runs over."),
        None => String::new(),
    };
    let msg = if events {
        // The task is its process group (`receipt_script`); the Windows
        // worker has its own way.
        let stop = if cfg!(unix) { format!("; `kill -- -{pid}` stops all of it") } else { String::new() };
        format!(
            "[mafold] Background task detached (pid {pid}{stop}) — it runs \
             independently and SURVIVES this turn and daemon restarts.{cap} Its output streams to \
             {} — do NOT wait for it or poll it this turn: THIS task finishes on its own schedule \
             and the daemon brings what happens to it back to you as an update in a NEW turn — \
             read the log then and decide what to tell people. (If the user asks how it is going \
             before then, just read that file — it is the live log.)",
            log.display()
        )
    } else {
        format!(
            "[mafold] Background task detached (pid {pid}) — it runs independently and \
             SURVIVES this turn and daemon restarts.{cap} Its output streams to {} — do NOT wait \
             for it or poll it this turn: THIS task finishes on its own schedule and the daemon \
             opens a NEW turn for you to read that log and report the results. (If the user asks \
             how it is going before then, just read that file — it is the live log.)",
            log.display()
        )
    };
    let mut updated = ti.clone();
    updated["command"] = Value::String(format!("echo {}", shell_single_quote(&msg)));
    updated["run_in_background"] = Value::Bool(false);
    Some(
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": "background task detached by mafold so it survives the turn",
                "updatedInput": updated,
            }
        })
        .to_string(),
    )
}

/// The script a detached task actually runs: the command, plus a self-kill
/// watchdog when the Bash call carried a `timeout` — and, when this turn's
/// tasks are watched per task (`receipt`, the `bgtaskEvents` flag), an exit
/// receipt ([`receipt_script`]). Without one it is the script as it always
/// was.
///
/// The watchdog works off process GROUPS, twice over:
///   - `spawn_detached` setsid's this script, so its pgid is its own pid and
///     `kill -- -$$` from the watchdog reaches exactly this task's tree —
///     children, grandchildren, anything that didn't run away to a new session.
///   - the watchdog itself is started under `set -m`, which puts it in a group
///     of its OWN. That keeps it out of the TERM it sends (no `trap` needed),
///     so it can escalate to KILL five seconds later for a command that won't
///     take the hint — and it lets the ordinary exit path `kill -- -$wd` take
///     the watchdog AND its pending `sleep` down together. Killing the subshell
///     alone left that `sleep` orphaned, which for a four-hour cap meant a
///     four-hour stray process after a task that finished in ten seconds.
/// `set +m` goes back on before the command runs, so the command's own
/// background jobs behave exactly as they would in any other shell.
#[cfg(unix)]
pub(crate) fn script_body(command: &str, timeout_secs: Option<u64>, receipt: Option<&Path>) -> String {
    if let Some(receipt) = receipt {
        return receipt_script(command, timeout_secs, receipt);
    }
    let Some(secs) = timeout_secs else {
        return format!("#!/bin/bash\n{command}\n");
    };
    format!(
        "#!/bin/bash\n\
         # [mafold] Bash 工具带了 timeout，这里把它兑现：超时就端掉整棵任务树。\n\
         set -m\n\
         ( sleep {secs}\n\
         \x20 echo \"[mafold] 超时：{secs} 秒到了，终止这个后台任务\" >&2\n\
         \x20 kill -TERM -- -$$ 2>/dev/null\n\
         \x20 sleep 5\n\
         \x20 kill -KILL -- -$$ 2>/dev/null\n\
         ) & __mf_wd=$!\n\
         set +m\n\
         {command}\n\
         __mf_rc=$?\n\
         kill -KILL -- -\"$__mf_wd\" 2>/dev/null\n\
         exit \"$__mf_rc\"\n"
    )
}

/// The script of a task watched per task: the same watchdog, and an exit
/// RECEIPT — how the daemon knows the task ended (`crate::bgtasks::life`). A
/// pid probe can't tell "exited" from "exited, and the pid went to someone
/// else".
///
/// - The `EXIT` trap writes the exit code to `receipt` (atomically: `.tmp`
///   then rename) once the command is done. The command runs in a subshell,
///   in the FOREGROUND, with every signal as it would have it anywhere else;
///   its own `trap … EXIT` or an `exec` can't take the receipt away
///   (subshells start with the traps reset, so it doesn't write one either).
/// - The task is its process GROUP (`spawn_detached` setsid's this script):
///   `kill -- -<pid>` stops all of it, and the hook tells the agent so. A
///   TERM, INT or HUP to the shell ALONE is held until the command is done
///   (bash runs a trap only after the foreground command) — the shell must
///   not die and leave the command running without anyone to write its
///   receipt — and the receipt then carries the command's own code. (Trapped,
///   too, because bash 3.2, `/bin/bash` on a Mac, hands the `EXIT` trap
///   `$?` = 0 for a shell killed by an untrapped TERM.)
/// - The timeout watchdog marks the timeout (`.timeout`) before its TERM, and
///   the shell's `EXIT` trap writes `124` + `timeout` (the code the Windows
///   worker uses) when it finds the mark — so a task killed for running over
///   says so instead of «exited 143», and the receipt goes down only once the
///   command is gone. Without a mark, the shell takes the watchdog with it:
///   left alive, it would later TERM a group nobody owns. A shell killed
///   outright (no `EXIT` trap) leaves the mark behind: the watchdog writes the
///   receipt itself, after its KILL. It checks the GROUP, not the shell: the
///   shell can die alone while the command runs on, and the cap still holds.
#[cfg(unix)]
fn receipt_script(command: &str, timeout_secs: Option<u64>, receipt: &Path) -> String {
    let watchdog = match timeout_secs {
        None => String::new(),
        Some(secs) => format!(
            "# [mafold] Bash 工具带了 timeout，这里把它兑现：超时就端掉整棵任务树。\n\
             set -m\n\
             ( sleep {secs}\n\
             \x20 kill -0 -- -$$ 2>/dev/null || exit 0\n\
             \x20 echo \"[mafold] 超时：{secs} 秒到了，终止这个后台任务\" >&2\n\
             \x20 : > \"$__mf_exit.timeout\"\n\
             \x20 kill -TERM -- -$$ 2>/dev/null\n\
             \x20 sleep 5\n\
             \x20 kill -KILL -- -$$ 2>/dev/null\n\
             \x20 sleep 1\n\
             \x20 [ -e \"$__mf_exit.timeout\" ] && printf '124\\ntimeout\\n' > \"$__mf_exit.wd\" && mv -f \"$__mf_exit.wd\" \"$__mf_exit\" && rm -f \"$__mf_exit.timeout\"\n\
             ) & __mf_wd=$!\n\
             set +m\n"
        ),
    };
    format!(
        "#!/bin/bash\n\
         # [mafold] 退出时留一张回执（退出码），daemon 凭它知道这个任务结束了。\n\
         __mf_exit={}\n\
         __mf_wd=\n\
         trap '__mf_rc=$?; __mf_to=; if [ -e \"$__mf_exit.timeout\" ]; then __mf_rc=124; __mf_to=timeout; elif [ -n \"$__mf_wd\" ]; then kill -KILL -- -\"$__mf_wd\" 2>/dev/null; fi; printf \"%s\\n\" \"$__mf_rc\" $__mf_to > \"$__mf_exit.tmp\" && mv -f \"$__mf_exit.tmp\" \"$__mf_exit\"; rm -f \"$__mf_exit.timeout\"' EXIT\n\
         # [mafold] 单独发给这个 shell 的 TERM/INT/HUP 等命令自己结束；整个任务要停用 kill -- -<pid>。\n\
         trap : HUP INT TERM\n\
         {watchdog}\
         (\n{command}\n)\n",
        shell_single_quote(&receipt.to_string_lossy())
    )
}

// Timeout and exit accounting live in the Windows worker, outside the shell:
// `exit`, syntax errors and commands that leave descendants all get a receipt.
#[cfg(windows)]
pub(crate) fn script_body(command: &str, _timeout_secs: Option<u64>, _receipt: Option<&Path>) -> String {
    format!("{command}\n")
}

/// Spawn `bash <script>` in a NEW SESSION with stdout/stderr appended to `log`.
/// The child leaves claude's process group entirely, so claude's exit-time
/// killpg can't reach it; when this hook exits, init adopts it.
#[cfg(unix)]
fn spawn_detached(script: &Path, log: &Path, cwd: &str) -> Option<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .ok()?;
    let err = out.try_clone().ok()?;
    let mut cmd = crate::platform::std_command("bash");
    cmd.arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // SAFETY: setsid() is async-signal-safe and the closure only calls it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().ok().map(|c| c.id())
}

/// POSIX single-quote `s` for safe embedding in a shell command.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Best-effort GC: registry artifacts older than 7 days (logs/scripts of
/// long-reported tasks) — keeps ~/.mafold/bgtasks from growing forever.
///
/// `alive_too` (per-task watching, which watches a task however long it
/// runs): never the files of a task still running, nor of one that ended
/// less than a week ago — a week-old `.pid` swept from under either would
/// leave its end untold. (A registration whose end is that old has nobody
/// coming for it.)
fn sweep_old(dir: &Path, alive_too: bool) {
    const WEEK: u64 = 7 * 24 * 3600;
    let old = |p: &Path| {
        std::fs::metadata(p)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age.as_secs() > WEEK)
    };
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if old(&e.path()) && !(alive_too && still_owed(&e.path(), old)) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Is the task this registry file belongs to (`<surface>.<ns>.<ext>`) still
/// to be told about — running (its `.pid` says, as the watcher reads it:
/// `crate::bgtasks::life`), or ended with a receipt younger than `old`?
fn still_owed(file: &Path, old: impl Fn(&Path) -> bool) -> bool {
    let Some(name) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else { return false };
    // The stem is everything up to the `<ns>` part — the extension may hold
    // dots of its own (`exit.tmp`).
    let parts: Vec<&str> = name.split('.').collect();
    let Some(ns) = parts.iter().position(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) else {
        return false;
    };
    let pid = file.with_file_name(format!("{}.pid", parts[..=ns].join(".")));
    if !pid.exists() {
        return false;
    }
    let receipt = pid.with_extension("exit");
    crate::bgtasks::life(&pid, &crate::bgtasks::meta(&pid)) == crate::bgtasks::Life::Running
        || (receipt.exists() && !old(&receipt))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// No timeout on the call → the script is exactly what it always was.
    /// A watchdog nobody asked for would kill long builds. (Flag off: no
    /// receipt either.)
    #[test]
    fn no_timeout_means_no_watchdog() {
        let s = script_body("cargo build --release", None, None);
        assert_eq!(s, "#!/bin/bash\ncargo build --release\n");
        assert!(!s.contains("__mf_wd"));
        let s = script_body("cargo build --release", None, Some(Path::new("/r/t.1.exit")));
        assert!(s.ends_with("(\ncargo build --release\n)\n"), "in the foreground: {s}");
        assert!(s.contains("__mf_exit='/r/t.1.exit'"), "{s}");
        assert!(!s.contains("sleep 5"), "no watchdog: {s}");
    }

    /// The timeout the model passed is honoured, in SECONDS, against the whole
    /// process group — and the command is still in there verbatim. With the
    /// flag off, byte for byte the script it always was.
    #[test]
    fn timeout_wraps_the_command_in_a_group_killing_watchdog() {
        let s = script_body("sleep 900", Some(30), None);
        assert!(s.contains("sleep 30"), "{s}");
        assert!(s.contains("kill -TERM -- -$$"), "{s}");
        assert!(s.contains("kill -KILL -- -$$"), "{s}");
        assert!(s.contains("\nsleep 900\n__mf_rc=$?\n"), "{s}");
        assert!(s.contains("exit \"$__mf_rc\""), "the task's own exit code wins: {s}");
        // The watchdog gets its own process group (`set -m`) — that is what
        // keeps it out of the TERM it sends, and what lets the exit path take
        // its pending `sleep` down with it instead of orphaning one.
        assert!(s.contains("set -m"), "{s}");
        assert!(s.contains("kill -KILL -- -\"$__mf_wd\""), "{s}");
        assert!(
            s.find("set +m") < s.find("sleep 900"),
            "job control must be back off before the command runs: {s}"
        );
        assert!(!s.contains("__mf_exit"), "no receipt with the flag off: {s}");

        let s = script_body("sleep 900", Some(30), Some(Path::new("/r/t.1.exit")));
        assert!(s.contains("sleep 30") && s.contains("set -m") && s.ends_with("(\nsleep 900\n)\n"), "{s}");
        assert!(s.find("set +m") < s.find("sleep 900"), "{s}");
        assert!(
            s.find(": > \"$__mf_exit.timeout\"") < s.find("kill -TERM -- -$$"),
            "the timeout is marked before the kill, so the receipt says «timeout», not «143»: {s}"
        );
    }

    /// How a test pokes a running task once it is up.
    #[derive(Clone, Copy)]
    enum Poke {
        /// A signal to its whole group — `kill -- -<pid>`, as the watchdog sends.
        Group(i32),
        /// A signal to the pid the agent was given, alone — `kill <pid>`.
        Pid(i32),
    }

    /// What became of a task: its receipt, how long its shell took to go once
    /// poked, and whether anything of it outlived that.
    struct Ran {
        receipt: Option<String>,
        took: std::time::Duration,
        stray: bool,
    }

    /// Run a detached task's script the way `spawn_detached` does (its own
    /// session) and read the receipt it leaves. `kill` = a signal to send the
    /// task's whole group once it is up, as the watchdog or a person would.
    fn receipt_of(command: &str, timeout: Option<u64>, kill: Option<i32>) -> Option<String> {
        run_task(command, timeout, kill.map(Poke::Group), std::time::Duration::ZERO).receipt
    }

    /// Anything of the task left: someone in its process group (its command),
    /// or a process still running its script (a watchdog — a group of its
    /// own, but a fork of the script, so the script's path is in its argv).
    fn leftovers(pid: u32, script: &Path) -> bool {
        let ps = crate::platform::std_command("ps").args(["-A", "-o", "command="]).output().unwrap();
        crate::platform::group_alive(pid) || String::from_utf8_lossy(&ps.stdout).contains(&*script.to_string_lossy())
    }

    /// …poked once it is up — its command's first output (`up`) is in the
    /// log: a signal before the script has set its traps proves nothing — and,
    /// once its shell is gone, given up to `wait` for a receipt to land (a
    /// watchdog writes one after its shell, when the shell was killed alone).
    fn run_task(command: &str, timeout: Option<u64>, poke: Option<Poke>, wait: std::time::Duration) -> Ran {
        use std::time::{Duration, Instant};
        let dir = std::env::temp_dir().join(format!("mf-receipt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (script, log, receipt) = (dir.join("t.1.sh"), dir.join("t.1.log"), dir.join("t.1.exit"));
        std::fs::write(&script, script_body(command, timeout, Some(&receipt))).unwrap();
        let pid = spawn_detached(&script, &log, &dir.to_string_lossy()).unwrap();
        if let Some(poke) = poke {
            let began = Instant::now();
            while !std::fs::read_to_string(&log).is_ok_and(|s| s.contains("up")) {
                assert!(began.elapsed() < Duration::from_secs(10), "the task never came up");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(leftovers(pid, &script), "control: a running task is seen running");
            match poke {
                Poke::Group(sig) => unsafe { libc::kill(-(pid as i32), sig) },
                Poke::Pid(sig) => unsafe { libc::kill(pid as i32, sig) },
            };
        }
        let began = Instant::now();
        while crate::platform::pid_running(pid) && began.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let took = began.elapsed();
        // Reaped (it is this test's child): a zombie shell still counts as
        // its group on Linux.
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
        let began = Instant::now();
        while !receipt.exists() && began.elapsed() < wait {
            std::thread::sleep(Duration::from_millis(50));
        }
        let got = std::fs::read_to_string(&receipt).ok();
        // Whatever is going away goes within a moment (init reaps the orphans).
        let began = Instant::now();
        let mut stray = leftovers(pid, &script);
        while stray && began.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(50));
            stray = leftovers(pid, &script);
        }
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        let _ = std::fs::remove_dir_all(&dir);
        Ran { receipt: got, took, stray }
    }

    /// `kill -- -<pid>` — what the agent is told — stops all of it at once,
    /// and the receipt says so.
    #[test]
    fn the_group_kill_stops_all_of_it() {
        let r = run_task("echo up; sleep 30; echo never", None, Some(Poke::Group(libc::SIGTERM)), std::time::Duration::ZERO);
        assert_eq!(r.receipt.as_deref(), Some("143\n"));
        assert!(r.took < std::time::Duration::from_secs(10), "{:?}", r.took);
        assert!(!r.stray, "nothing of it left running");
    }

    /// A signal to the shell ALONE (`kill <pid>`) is held until the command
    /// is done: the shell must not die and leave the command running with
    /// nobody to write its receipt — and the receipt is the command's own.
    #[test]
    fn a_signal_to_the_shell_alone_waits_for_the_command() {
        let r = run_task("echo up; sleep 2; exit 7", None, Some(Poke::Pid(libc::SIGTERM)), std::time::Duration::ZERO);
        assert_eq!(r.receipt.as_deref(), Some("7\n"));
        assert!(r.took >= std::time::Duration::from_millis(1000), "it ran to its end: {:?}", r.took);
        assert!(!r.stray);
    }

    /// Every way a task ends leaves a receipt with what happened in it —
    /// except SIGKILL, which no shell outlives (the daemon has the pid, and
    /// the group, for that). `exec` and a command's own `EXIT` trap used to
    /// take the only signal of the end with them.
    #[test]
    fn every_ending_leaves_a_receipt_saying_how() {
        assert_eq!(receipt_of("true", None, None).as_deref(), Some("0\n"));
        assert_eq!(receipt_of("echo hi; exit 7", None, None).as_deref(), Some("7\n"));
        assert_eq!(receipt_of("false", None, None).as_deref(), Some("1\n"));
        assert_eq!(receipt_of("exec sh -c 'exit 3'", None, None).as_deref(), Some("3\n"), "exec");
        assert_eq!(receipt_of("trap 'echo mine' EXIT; exit 5", None, None).as_deref(), Some("5\n"), "its own trap");
        assert_eq!(receipt_of("echo up; sleep 30", None, Some(libc::SIGTERM)).as_deref(), Some("143\n"), "TERM to the group");
        assert_eq!(receipt_of("sleep 30", Some(1), None).as_deref(), Some("124\ntimeout\n"), "ran over its timeout");
        assert_eq!(receipt_of("echo up; sleep 30", None, Some(libc::SIGKILL)), None, "KILL: nothing can write one");
        // Its own `timeout` running out is its own exit 124, not Mafold's cap.
        assert_eq!(receipt_of("exit 124", Some(60), None).as_deref(), Some("124\n"), "no `timeout` line");
        // A command that ignores INT still gets one: it runs in the
        // foreground, with signals as it would have them anywhere.
        assert_eq!(
            receipt_of("trap 'echo got-int; exit 9' INT; echo up; sleep 30 & wait", None, Some(libc::SIGINT)).as_deref(),
            Some("9\n"),
            "its own INT trap ran"
        );
    }

    /// A task that ends before its timeout takes its watchdog with it — no
    /// sleeping watchdog left behind to TERM a group nobody owns by then.
    #[test]
    fn an_early_end_takes_the_watchdog_with_it() {
        let r = run_task("echo up; sleep 30", Some(30), Some(Poke::Group(libc::SIGTERM)), std::time::Duration::ZERO);
        assert_eq!(r.receipt.as_deref(), Some("143\n"));
        assert!(!r.stray, "the watchdog went with it");
    }

    /// The shell killed alone while its command runs on: the cap the model
    /// asked for still holds (the watchdog asks the group, not the shell), and
    /// with no shell left to write the receipt, the watchdog does.
    #[test]
    fn the_cap_holds_when_the_shell_is_killed_alone() {
        let r = run_task("echo up; sleep 30", Some(2), Some(Poke::Pid(libc::SIGKILL)), std::time::Duration::from_secs(15));
        assert_eq!(r.receipt.as_deref(), Some("124\ntimeout\n"));
        assert!(!r.stray, "the command was stopped at its cap");
    }

    /// …and until then the task is RUNNING to the watcher: the task is its
    /// group, and the group lives on without its shell
    /// (`crate::bgtasks::life`).
    #[test]
    fn a_task_is_its_group_not_its_shell() {
        use crate::bgtasks::{life, meta, Life};
        use std::time::{Duration, Instant};
        let dir = std::env::temp_dir().join(format!("mf-group-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (script, log, pidf) = (dir.join("t.1.sh"), dir.join("t.1.log"), dir.join("t.1.pid"));
        std::fs::write(&script, script_body("echo up; sleep 30", None, Some(&dir.join("t.1.exit")))).unwrap();
        let pid = spawn_detached(&script, &log, &dir.to_string_lossy()).unwrap();
        std::fs::write(&pidf, pid.to_string()).unwrap();
        let start = crate::platform::pid_start(pid);
        std::fs::write(dir.join("t.1.meta"), serde_json::json!({ "cmd": "sleep 30", "pid_start": start }).to_string()).unwrap();
        let began = Instant::now();
        while !std::fs::read_to_string(&log).is_ok_and(|s| s.contains("up")) {
            assert!(began.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        while crate::platform::pid_running(pid) {
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(life(&pidf, &meta(&pidf)), Life::Running, "the shell is gone, its command is not");
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        let began = Instant::now();
        while life(&pidf, &meta(&pidf)) == Life::Running {
            assert!(began.elapsed() < Duration::from_secs(5), "never seen gone");
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file of a task still running is never swept, however old (per-task
    /// watching watches it to its end); one of a task that ended is.
    #[test]
    fn the_sweep_leaves_a_running_tasks_files() {
        let dir = std::env::temp_dir().join(format!("mf-sweep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let week_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        let file = |name: &str, body: &str| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(week_ago).unwrap();
            p
        };
        // Alive: this very process.
        let me = std::process::id().to_string();
        let live = [file("c__ch__b.1.pid", &me), file("c__ch__b.1.log", "…"), file("c__ch__b.1.exit.tmp", "")];
        let dead = [file("c__ch__b.2.pid", "999999"), file("c__ch__b.2.log", "…")];
        let swept_alone = file("c__ch__b.3.log", "…");
        // Ran over a week and ended just now: its end is still to be told.
        let just_ended = [file("c__ch__b.4.pid", "999999"), file("c__ch__b.4.meta", "{}")];
        std::fs::write(dir.join("c__ch__b.4.exit"), "0\n").unwrap();
        sweep_old(&dir, true);
        assert!(live.iter().all(|p| p.exists()), "a running task's files stay");
        assert!(just_ended.iter().all(|p| p.exists()), "so do those of one whose end is still to be told");
        assert!(dead.iter().all(|p| !p.exists()) && !swept_alone.exists(), "the rest go");
        // The flag off: the sweep as it always was.
        sweep_old(&dir, false);
        assert!(live.iter().all(|p| !p.exists()) && just_ended.iter().all(|p| !p.exists()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ms → s, and a sub-second timeout still gets a whole second rather than
    /// `sleep 0` (which would kill the task before it started).
    #[test]
    fn sub_second_timeouts_round_up_to_one() {
        let ti = serde_json::json!({ "timeout": 400 });
        let secs = ti["timeout"].as_u64().map(|ms| (ms / 1000).max(1));
        assert_eq!(secs, Some(1));
    }
}

#[cfg(all(test, windows))]
mod windows_switch_tests {
    /// Flag off (every bot today), a background Bash goes through untouched —
    /// nothing detached, nothing written to the registry, nothing promised —
    /// even with the daemon's broker up and answering. The control: the same
    /// broker says on once the server turns the flag on.
    #[test]
    fn win_flag_off_leaves_background_bash_alone() {
        use crate::background_windows::{daemon_says_on, enabled, start_broker, FLAG};
        start_broker().unwrap();
        crate::flags::ingest(&serde_json::json!({ "values": { FLAG: false }, "version": 0 }));
        assert!(!enabled() && !super::bg_detach_supported(), "the prompt says nothing survives");
        assert!(!daemon_says_on(), "and so does the broker, to the hook");
        let call = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "echo hi", "run_in_background": true },
        });
        assert_eq!(super::rewrite(&call.to_string()), None);

        crate::flags::ingest(&serde_json::json!({ "values": { FLAG: true }, "version": 0 }));
        assert!(super::bg_detach_supported() && daemon_says_on(), "control: on is on, both ways");
        crate::flags::ingest(&serde_json::json!({ "values": { FLAG: false }, "version": 0 }));
    }
}
