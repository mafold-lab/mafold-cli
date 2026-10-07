//! Windows detached work is launched by the daemon, never by a Claude hook:
//! a hook may be inside a job that forbids breakaway. The loopback broker is
//! authenticated with a per-daemon capability inherited only by its children.
//! A small worker owns the task's job and exit receipt; it survives the turn,
//! the broker and daemon restarts. Killing the worker closes its job (tree kill).
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
pub struct Task {
    pub script: PathBuf,
    pub log: PathBuf,
    pub cwd: PathBuf,
    pub bash: bool,
    pub timeout_secs: Option<u64>,
}
#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    /// None: "is it on?" — the hook asks before it writes anything.
    #[serde(default)]
    task: Option<Task>,
    env: HashMap<String, String>,
}

/// The server flag that switches this on, per person (`crate::flags`).
pub const FLAG: &str = "windowsBackground";

static BROKER_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// In the daemon: will a background Bash outlive this turn? The flag is on for
/// this bot AND the broker is up — a broker that failed to start leaves this
/// daemon turn-scoped, whatever the server says. The system prompt, the card
/// and (through the broker) the hook all read this one answer.
pub fn enabled() -> bool {
    BROKER_UP.load(std::sync::atomic::Ordering::SeqCst) && crate::flags::enabled(FLAG)
}

/// In the hook: ask the daemon. Anything short of its "on" — no broker in the
/// environment, no answer, a bad reply — is off, and the call goes through
/// untouched.
pub fn daemon_says_on() -> bool {
    let ask = || -> Result<bool> {
        let address: std::net::SocketAddr = std::env::var("MAFOLD_BG_BROKER")?.parse()?;
        if !address.ip().is_loopback() {
            bail!("background broker must be local");
        }
        let mut socket = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        let request = Request { token: std::env::var("MAFOLD_BG_CAPABILITY")?, task: None, env: HashMap::new() };
        writeln!(socket, "{}", serde_json::to_string(&request)?)?;
        let mut reply = String::new();
        BufReader::new(socket).read_line(&mut reply)?;
        Ok(serde_json::from_str::<serde_json::Value>(&reply)?["on"] == true)
    };
    ask().unwrap_or(false)
}

/// Must start before any harness is prewarmed. Never accept remote clients or
/// unauthenticated spawn requests. A bounded connection lifetime/size keeps a
/// broken hook from wedging the broker. Started whether or not the flag is on,
/// so the server can switch it on without the processes already running
/// having to be restarted to find the broker.
pub fn start_broker() -> Result<()> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let token = uuid::Uuid::new_v4().to_string();
    std::env::set_var("MAFOLD_BG_BROKER", listener.local_addr()?.to_string());
    std::env::set_var("MAFOLD_BG_CAPABILITY", &token);
    std::thread::spawn(move || {
        for socket in listener.incoming().flatten() {
            let token = token.clone();
            std::thread::spawn(move || {
                let mut socket = socket;
                let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = socket.set_write_timeout(Some(Duration::from_secs(10)));
                let result = (|| -> Result<serde_json::Value> {
                    let mut line = String::new();
                    use std::io::Read;
                    BufReader::new((&socket).take(2 * 1024 * 1024)).read_line(&mut line)?;
                    let request: Request = serde_json::from_str(&line)?;
                    if request.token != token {
                        bail!("invalid background capability");
                    }
                    let Some(task) = request.task else {
                        return Ok(serde_json::json!({"on": enabled()}));
                    };
                    if !enabled() {
                        bail!("Windows background tasks are switched off for this bot");
                    }
                    Ok(serde_json::json!({"pid": start(&task, Some(&request.env))?}))
                })();
                let reply = match result {
                    Ok(reply) => reply,
                    Err(e) => serde_json::json!({"error": e.to_string()}),
                };
                let _ = writeln!(socket, "{reply}");
            });
        }
    });
    BROKER_UP.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

pub fn request(task: Task) -> Result<u32> {
    let address: std::net::SocketAddr = std::env::var("MAFOLD_BG_BROKER")?.parse()?;
    if !address.ip().is_loopback() {
        bail!("background broker must be local");
    }
    let mut socket = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    socket.set_read_timeout(Some(Duration::from_secs(20)))?;
    socket.set_write_timeout(Some(Duration::from_secs(10)))?;
    let request = Request {
        token: std::env::var("MAFOLD_BG_CAPABILITY")?,
        task: Some(task),
        env: std::env::vars().collect(),
    };
    writeln!(socket, "{}", serde_json::to_string(&request)?)?;
    let mut reply = String::new();
    BufReader::new(socket).read_line(&mut reply)?;
    let value: serde_json::Value = serde_json::from_str(&reply)?;
    value["pid"].as_u64().map(|p| p as u32).context(
        value["error"]
            .as_str()
            .unwrap_or("background broker returned no pid")
            .to_owned(),
    )
}

/// Called directly by computer's executor (already outside the harness job),
/// or by the broker on behalf of a hook. No turn env is persisted on disk.
pub fn start(task: &Task, env: Option<&HashMap<String, String>>) -> Result<u32> {
    let config = task.script.with_extension("worker");
    let ready = task.script.with_extension("ready");
    std::fs::write(&config, serde_json::to_vec(task)?)?;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&task.log)?;
    let mut cmd = crate::platform::std_command(std::env::current_exe()?);
    #[cfg(not(test))]
    cmd.arg("background-worker").arg(&config);
    #[cfg(test)]
    cmd.args([
        "--exact",
        "background_windows::tests::worker_entry",
        "--ignored",
        "--nocapture",
    ]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(out.try_clone()?))
        .stderr(Stdio::from(out));
    if let Some(env) = env {
        cmd.env_clear().envs(env);
    }
    #[cfg(test)]
    cmd.env("MAFOLD_TEST_WORKER_CONFIG", &config);
    // CREATE_NO_WINDOW is sufficient here: the daemon, not Claude, is parent.
    // DETACHED_PROCESS would leave descendants with no console to inherit.
    let mut child = cmd.spawn()?;
    let began = Instant::now();
    loop {
        if let Ok(result) = std::fs::read_to_string(&ready) {
            let _ = std::fs::remove_file(&ready);
            if result != "ok" {
                let _ = child.wait();
                bail!("{result}");
            }
            let pid = child.id();
            // Register before replying: a hook that dies while reading the
            // acknowledgement must not orphan an unreportable task.
            if let Err(error) = receipt(&task.script.with_extension("pid"), &pid.to_string()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(pid);
        }
        if child.try_wait()?.is_some() {
            bail!(
                "background worker exited before starting task; see {}",
                task.log.display()
            );
        }
        if began.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("background worker startup timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn receipt(path: &Path, content: &str) -> Result<()> {
    let temp = path.with_extension(format!(
        "{}.tmp",
        path.extension().unwrap().to_string_lossy()
    ));
    std::fs::write(&temp, content)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

pub fn worker(config: &Path) -> Result<()> {
    let task: Task = serde_json::from_slice(&std::fs::read(config)?)?;
    let _ = std::fs::remove_file(config);
    let result = run_task(&task);
    if let Err(e) = &result {
        let _ = receipt(&task.script.with_extension("ready"), &format!("{e:#}"));
        let _ = receipt(&task.script.with_extension("exit"), "1");
    }
    result
}

/// Match Claude's explicit Git Bash override, then locate bash in the Git
/// installation. Never fall through to System32/bash.exe (the WSL launcher).
fn git_bash() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!("CLAUDE_CODE_GIT_BASH_PATH does not name a file");
    }
    let mut roots = Vec::new();
    if let Some(git) = crate::harness::resolve("git") {
        roots.extend(git.ancestors().skip(1).take(3).map(Path::to_path_buf));
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            let base = PathBuf::from(base);
            roots.push(base.join("Git"));
            roots.push(base.join("Programs/Git"));
        }
    }
    for root in roots {
        for relative in ["bin/bash.exe", "usr/bin/bash.exe"] {
            let path = root.join(relative);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    bail!("Git Bash was not found; set CLAUDE_CODE_GIT_BASH_PATH to its bash.exe")
}

fn run_task(task: &Task) -> Result<()> {
    // Assign THIS worker before spawning any descendants: no race where a
    // fast shell starts grandchildren before being assigned to the job.
    let _job = crate::platform::background_job()?;
    // Closing this job terminates the worker too. Publish every result while
    // its handle is still alive, including startup failures (missing shell).
    let result = run_shell(task);
    match &result {
        Ok(code) => receipt(&task.script.with_extension("exit"), &code.to_string())?,
        Err(error) => {
            eprintln!("[mafold] Background task failed: {error:#}");
            let _ = receipt(&task.script.with_extension("ready"), &format!("{error:#}"));
            receipt(&task.script.with_extension("exit"), "1")?;
        }
    }
    result.map(|_| ())
}

fn run_shell(task: &Task) -> Result<i32> {
    let mut cmd = if task.bash {
        let mut cmd = crate::platform::std_command(git_bash()?);
        cmd.arg(task.script.to_string_lossy().replace('\\', "/"));
        cmd
    } else {
        // std routes .cmd through cmd.exe with hardened batch-file quoting;
        // hand-writing /C quoting breaks paths containing spaces or &.
        crate::platform::std_command(&task.script)
    };
    // Pipes, with this worker appending them to the log — not the log file
    // handed straight to the shell: Git Bash exits 1 without a word when its
    // stdout/stderr is a file handle, and runs fine on a pipe (every row of
    // `tests::win_git_bash_ways_to_start_it`). `.cmd` takes the same road.
    let mut child = cmd
        .current_dir(&task.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (done, drained) = std::sync::mpsc::channel();
    if let Some(out) = child.stdout.take() {
        pump(out, &task.log, done.clone())?;
    }
    if let Some(err) = child.stderr.take() {
        pump(err, &task.log, done.clone())?;
    }
    drop(done);
    receipt(&task.script.with_extension("ready"), "ok")?;
    let started = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code().unwrap_or(1);
        }
        if task
            .timeout_secs
            .is_some_and(|s| started.elapsed() >= Duration::from_secs(s))
        {
            eprintln!("[mafold] Background task timed out");
            // The exit receipt is persisted BEFORE closing the job kills us.
            break 124;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // What the shell wrote just before it exited may still be in the pipes;
    // let it reach the log before the receipt says the task is done. A
    // descendant still holding a pipe ends with the job, so this is short —
    // and skipped on a timeout, where the shell itself is still running.
    if code != 124 {
        let until = Instant::now() + Duration::from_secs(2);
        while drained.recv_timeout(until.saturating_duration_since(Instant::now())).is_ok() {}
    }
    Ok(code)
}

/// Append one of the shell's pipes to the log until it closes, then say so.
fn pump(mut from: impl std::io::Read + Send + 'static, log: &Path, done: std::sync::mpsc::Sender<()>) -> Result<()> {
    let mut to = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut from, &mut to);
        let _ = done.send(());
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn win_background_broker_rejects_invalid_capability() {
        use std::io::{BufRead, Write};
        super::start_broker().unwrap();
        let root = std::env::temp_dir().join(format!("mf-denied-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("never.cmd");
        std::fs::write(&script, "@echo must-not-run").unwrap();
        let task = super::Task {
            script: script.clone(),
            log: root.join("never.log"),
            cwd: root.clone(),
            bash: false,
            timeout_secs: None,
        };
        let request = super::Request {
            token: "wrong".into(),
            task: Some(task),
            env: Default::default(),
        };
        let mut socket =
            std::net::TcpStream::connect(std::env::var("MAFOLD_BG_BROKER").unwrap()).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        writeln!(socket, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        let mut reply = String::new();
        std::io::BufReader::new(socket)
            .read_line(&mut reply)
            .unwrap();
        assert!(reply.contains("invalid background capability"));
        assert!(!script.with_extension("pid").exists());
        assert!(!script.with_extension("log").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn win_background_timeout_records_failure_and_ends_tree() {
        let root = std::env::temp_dir().join(format!("mf-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("timeout.cmd");
        std::fs::write(
            &script,
            "@echo before-timeout\r\nping -n 60 127.0.0.1 >nul\r\n",
        )
        .unwrap();
        let task = super::Task {
            script: script.clone(),
            log: root.join("timeout.log"),
            cwd: root.clone(),
            bash: false,
            timeout_secs: Some(1),
        };
        let pid = super::start(&task, None).unwrap();
        let began = std::time::Instant::now();
        while crate::platform::child_running(pid) {
            assert!(began.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(
            std::fs::read_to_string(script.with_extension("exit")).unwrap(),
            "124"
        );
        assert!(std::fs::read_to_string(&task.log)
            .unwrap()
            .contains("before-timeout"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Which ways of starting Git Bash actually run a command. Prints one line
    /// per way (exit code, what it wrote, its stderr). Measured on CI: every
    /// pipe row OK, every file row exit 1 with nothing written — why the worker
    /// hands the shell pipes. Asserted: the pipe rows the worker relies on.
    #[test]
    fn win_git_bash_ways_to_start_it() {
        use std::process::Stdio;
        let root = std::env::temp_dir().join(format!("mf-bashways-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("say.sh");
        std::fs::write(&script, "echo said-it\n").unwrap();
        let wrapper = super::git_bash().unwrap();
        let msys = wrapper.parent().and_then(|b| b.parent()).map(|g| g.join("usr").join("bin").join("bash.exe")).unwrap();
        let fwd = script.to_string_lossy().replace('\\', "/");
        let back = script.to_string_lossy().into_owned();
        let mut rows = Vec::new();
        // The way `run_shell` starts it: the wrapper, a script, hidden, pipes.
        let mut worker_way_ok = false;
        let cases: Vec<(&str, std::path::PathBuf, Vec<String>, bool, bool)> = vec![
            // (label, program, args, hidden (CREATE_NO_WINDOW), to a file)
            ("wrapper script fwd  hidden file", wrapper.clone(), vec![fwd.clone()], true, true),
            ("wrapper script fwd  hidden pipe", wrapper.clone(), vec![fwd.clone()], true, false),
            ("wrapper script fwd  plain  pipe", wrapper.clone(), vec![fwd.clone()], false, false),
            ("wrapper script back hidden pipe", wrapper.clone(), vec![back.clone()], true, false),
            ("wrapper -c         hidden pipe", wrapper.clone(), vec!["-c".into(), "echo said-it".into()], true, false),
            ("wrapper -l -c      hidden pipe", wrapper.clone(), vec!["-l".into(), "-c".into(), "echo said-it".into()], true, false),
            ("wrapper --version  hidden pipe", wrapper.clone(), vec!["--version".into()], true, false),
            ("msys    script fwd  hidden pipe", msys.clone(), vec![fwd.clone()], true, false),
            ("msys    script fwd  hidden file", msys.clone(), vec![fwd.clone()], true, true),
            ("msys    -c         hidden pipe", msys.clone(), vec!["-c".into(), "echo said-it".into()], true, false),
        ];
        for (i, (label, program, args, hidden, to_file)) in cases.into_iter().enumerate() {
            let mut cmd = if hidden { crate::platform::std_command(&program) } else { crate::platform::unhidden_command(&program) };
            cmd.args(&args).current_dir(&root).stdin(Stdio::null());
            let (code, out, err) = if to_file {
                let log = root.join(format!("case-{i}.log"));
                let f = std::fs::OpenOptions::new().create(true).read(true).append(true).open(&log).unwrap();
                let status = cmd.stdout(Stdio::from(f.try_clone().unwrap())).stderr(Stdio::from(f)).status();
                (status.ok().and_then(|s| s.code()), std::fs::read_to_string(&log).unwrap_or_default(), String::new())
            } else {
                match cmd.output() {
                    Ok(o) => (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned()),
                    Err(e) => (None, String::new(), format!("spawn: {e}")),
                }
            };
            let ok = code == Some(0) && (out.contains("said-it") || label.contains("--version"));
            worker_way_ok |= ok && label == "wrapper script fwd  hidden pipe";
            rows.push(format!("{} {label}: exit {code:?} out {:?} err {:?}", if ok { "OK  " } else { "FAIL" }, out.trim(), err.trim()));
        }
        println!("git bash ways ({} | {}):\n{}", wrapper.display(), msys.display(), rows.join("\n"));
        let _ = std::fs::remove_dir_all(&root);
        assert!(worker_way_ok, "the way the worker starts Git Bash ran nothing:\n{}", rows.join("\n"));
    }

    /// What a hook's background Bash becomes: a `.sh` run by Git Bash in the
    /// worker. Exit 0, its output in the log.
    #[test]
    fn win_background_bash_task_runs_and_logs() {
        let root = std::env::temp_dir().join(format!("mf-bash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("task.sh");
        std::fs::write(&script, "echo bash-ok\n").unwrap();
        let task = super::Task {
            script: script.clone(),
            log: root.join("task.log"),
            cwd: root.clone(),
            bash: true,
            timeout_secs: None,
        };
        let bash = super::git_bash().map(|p| p.display().to_string());
        let pid = super::start(&task, None).unwrap_or_else(|e| panic!("start: {e:#} (git bash: {bash:?})"));
        let began = std::time::Instant::now();
        while !script.with_extension("exit").exists() {
            assert!(began.elapsed() < std::time::Duration::from_secs(20), "no exit receipt (worker {pid})");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let code = std::fs::read_to_string(script.with_extension("exit")).unwrap();
        let log = std::fs::read_to_string(&task.log).unwrap_or_default();
        assert!(code == "0" && log.contains("bash-ok"), "git bash {bash:?}: exit {code}, log:\n{log}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "worker subprocess entry, invoked by start() in Windows tests"]
    fn worker_entry() {
        let path = std::env::var_os("MAFOLD_TEST_WORKER_CONFIG").expect("worker config");
        super::worker(std::path::Path::new(&path)).unwrap();
    }
}
