//! `mafold logs` and `mafold diagnose` end to end — the real binary in a
//! throwaway HOME (activity center C6). These run on Linux with the rest of
//! the suite, and on macOS and Windows in `cli-log-tests.yml`: until C6 the
//! supervisor's log depended on who started it (launchd's file, the systemd
//! journal, nothing at all under the Windows logon task), so the one claim
//! worth checking on every OS is that the process writes it itself.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Nothing is listening here, so nothing can reach the real api.
const DEAD_API: &str = "http://127.0.0.1:1";

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-logs-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".mafold/daemons")).expect("make temp home");
    std::fs::create_dir_all(dir.join("bin")).expect("make temp PATH");
    dir
}

fn put(home: &Path, rel: &str, text: &str) {
    let p = home.join(".mafold").join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn append(p: &Path, text: &str) {
    std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap().write_all(text.as_bytes()).unwrap();
}

/// The binary, in `home` and nowhere else: no runtime of the developer's
/// machine on PATH, no token or account from the shell that runs the tests.
fn mafold(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mafold"));
    c.env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", home.join("bin"))
        .env("MAFOLD_BASE", DEAD_API)
        .env_remove("MAFOLD_ACCOUNT")
        .env_remove("MAFOLD_BOT_TOKEN")
        .env_remove("MAFOLD_CHANNEL")
        .env_remove("MAFOLD_DAEMON_NAME");
    c
}

fn run(home: &Path, args: &[&str]) -> std::process::Output {
    mafold(home).args(args).output().expect("run mafold")
}

fn stdout_ok(home: &Path, args: &[&str]) -> String {
    let out = run(home, args);
    assert!(out.status.success(), "mafold {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// Each line of a child's stdout, as it comes.
fn lines_of(child: &mut Child) -> mpsc::Receiver<String> {
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

fn next(rx: &mpsc::Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(20)).expect("a line within 20s")
}

/// All a child writes to one of its pipes, once it closes — every byte of it:
/// one that isn't UTF-8 must not empty the lot.
fn drain(mut r: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = r.read_to_end(&mut bytes);
        let _ = tx.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    rx
}

/// Every string in `v`, however deep.
fn strings(v: &Value) -> Vec<&str> {
    match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(items) => items.iter().flat_map(strings).collect(),
        Value::Object(map) => map.values().flat_map(strings).collect(),
        _ => vec![],
    }
}

/// A path as any spelling of it compares: one kind of slash, never doubled,
/// any case.
fn plain_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars().map(|c| if c == '\\' { '/' } else { c.to_ascii_lowercase() }) {
        if !(c == '/' && out.ends_with('/')) {
            out.push(c);
        }
    }
    out
}

/// What a test that waited in vain for `child` found, for its failure
/// message: whether the process was still there, every file under
/// `home/.mafold` (the last 4 KB of each), and where each of its threads was —
/// `sample` on macOS, each thread's state and wait channel on Linux. Only
/// ever after a wait ran out: one look, never a loop.
fn scene(home: &Path, child: &mut Child) -> String {
    let mut s = String::new();
    let alive = match child.try_wait() {
        Ok(Some(status)) => {
            s += &format!("the process had exited: {status}\n");
            false
        }
        Ok(None) => {
            s += "the process was still running\n";
            true
        }
        Err(e) => {
            s += &format!("couldn't ask after the process: {e}\n");
            false
        }
    };
    fn walk(dir: &Path, s: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk(&p, s);
                continue;
            }
            let bytes = std::fs::read(&p).unwrap_or_default();
            let from = bytes.len().saturating_sub(4096);
            let shown = if from > 0 { format!(", the last {} of them", bytes.len() - from) } else { String::new() };
            *s += &format!("── {} ({} bytes{shown})\n", p.display(), bytes.len());
            *s += &String::from_utf8_lossy(&bytes[from..]);
            if !s.ends_with('\n') {
                s.push('\n');
            }
        }
    }
    walk(&home.join(".mafold"), &mut s);
    if alive {
        s += &threads(child.id(), home);
    }
    s
}

#[cfg(target_os = "macos")]
fn threads(pid: u32, home: &Path) -> String {
    let file = home.join("sample.txt");
    let ran = Command::new("/usr/bin/sample").args([&pid.to_string(), "1", "-file"]).arg(&file).output();
    match (ran, std::fs::read_to_string(&file)) {
        (_, Ok(report)) => format!("── sample of pid {pid}\n{report}"),
        (Ok(o), Err(_)) => format!("── sample of pid {pid} wrote nothing: {}\n", String::from_utf8_lossy(&o.stderr)),
        (Err(e), Err(_)) => format!("── couldn't run sample: {e}\n"),
    }
}

#[cfg(target_os = "linux")]
fn threads(pid: u32, _home: &Path) -> String {
    let mut s = format!("── threads of pid {pid} (name, state, waiting in)\n");
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return s };
    for t in tasks.flatten().map(|e| e.path()) {
        let read = |f: &str| std::fs::read_to_string(t.join(f)).unwrap_or_default().trim().to_string();
        // `stat` is `tid (name) S …`: the state is the field after the name.
        let state = read("stat").rsplit_once(") ").map(|(_, rest)| rest.chars().take(1).collect::<String>()).unwrap_or_default();
        s += &format!("{} {:?} {state} {}\n", t.file_name().unwrap().to_string_lossy(), read("comm"), read("wchan"));
    }
    s
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn threads(_pid: u32, _home: &Path) -> String {
    String::new()
}

/// A child this test started, ended when the test is — pass or fail. Only
/// ever the handle `spawn` returned: never a pid read from a file.
struct Started(Child);

impl Drop for Started {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn logs_shows_the_supervisors_log_with_no_name_and_a_bots_by_name() {
    let home = temp_home("read");
    put(&home, "supervisor.log", "supervisor up (pid 7) · base x\n↑ started a:one (pid 8)\n");
    put(&home, "daemons/a_one/log.1", "older 1\nolder 2\n");
    put(&home, "daemons/a_one/log", "mafold agent ✓ connected as @one\r\nturn done\n");

    assert_eq!(stdout_ok(&home, &["logs"]), "supervisor up (pid 7) · base x\n↑ started a:one (pid 8)\n");
    // A name goes through the same `safe` spelling the supervisor files it under.
    assert_eq!(stdout_ok(&home, &["logs", "a:one", "-n", "1"]), "turn done\n");
    // Fewer lines than asked for since the last rotation: the rest come from `.1`.
    assert_eq!(
        stdout_ok(&home, &["logs", "a:one", "-n", "3"]),
        "older 2\nmafold agent ✓ connected as @one\nturn done\n"
    );

    let json: Vec<Value> = stdout_ok(&home, &["logs", "a:one", "--json", "-n", "2"])
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(json[0]["type"], "open");
    assert_eq!(json[0]["source"], "bot");
    assert_eq!(json[0]["name"], "a:one");
    assert!(json[0]["path"].as_str().unwrap().ends_with("log"), "{}", json[0]);
    assert_eq!(json[1], serde_json::json!({ "type": "line", "text": "mafold agent ✓ connected as @one" }));
    assert_eq!(json[2], serde_json::json!({ "type": "line", "text": "turn done" }));
    assert_eq!(json.len(), 3);

    let sup: Value = serde_json::from_str(stdout_ok(&home, &["logs", "--json", "-n", "0"]).trim()).unwrap();
    assert_eq!(sup["source"], "supervisor");
    assert!(sup["name"].is_null());

    // No log there: says where it looked, and fails.
    let missing = run(&home, &["logs", "nobody"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no log for `nobody`"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn logs_follow_carries_on_across_a_rotation() {
    let home = temp_home("follow");
    let log = home.join(".mafold/daemons/a_one/log");
    put(&home, "daemons/a_one/log", "first\nsecond\n");
    let mut child = mafold(&home)
        .args(["logs", "a:one", "-f", "--json", "-n", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let rx = lines_of(&mut child);
    let reader = Started(child);
    let text = |l: String| -> Value { serde_json::from_str(&l).unwrap() };

    assert_eq!(text(next(&rx))["type"], "open");
    assert_eq!(text(next(&rx))["text"], "second");
    append(&log, "third\n");
    assert_eq!(text(next(&rx))["text"], "third");
    // What a running process does past the limit: the file moves to `.1`
    // while the reader still has it open, and a new one starts.
    append(&log, "fourth\n");
    std::fs::rename(&log, home.join(".mafold/daemons/a_one/log.1")).unwrap();
    append(&log, "after\n");
    assert_eq!(text(next(&rx))["text"], "fourth", "what the old file got before it moved isn't lost");
    assert_eq!(text(next(&rx)), serde_json::json!({ "type": "rotated" }));
    assert_eq!(text(next(&rx))["text"], "after");
    append(&log, "last\n");
    assert_eq!(text(next(&rx))["text"], "last");
    drop(reader); // on Windows its open file would keep the directory
    let _ = std::fs::remove_dir_all(&home);
}

/// Whoever starts the supervisor, its output ends up in its own file: here its
/// stdout and stderr are pipes — what a systemd unit hands it (the journal),
/// or the Windows logon task (nothing anyone reads) — and the pipes stay
/// empty while `mafold logs` shows what it said.
#[test]
fn the_supervisor_writes_its_own_log_wherever_its_output_was_pointed() {
    let home = temp_home("supervise");
    let mut child = mafold(&home)
        .args(["--base", DEAD_API, "--no-auto-update", "supervise"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (out, err) = (drain(child.stdout.take().unwrap()), drain(child.stderr.take().unwrap()));
    let mut started = Started(child);
    let piped = |rx: &mpsc::Receiver<String>| rx.recv_timeout(Duration::from_secs(10));

    let log = home.join(".mafold/supervisor.log");
    let said = format!("supervisor up (pid {pid})");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // Whether it is gone, asked BEFORE the log is read: one that said it
        // and then exited between the two would otherwise read as one that
        // never said it.
        let exited = matches!(started.0.try_wait(), Ok(Some(_)));
        if std::fs::read_to_string(&log).is_ok_and(|t| t.contains(&said)) {
            break;
        }
        // Once in a full local run (10-23) it never said it, with the log's
        // first line, the pid file and run.json all written: what it was
        // doing goes in the message, so the next time says why.
        if exited || Instant::now() >= deadline {
            let seen = scene(&home, &mut started.0);
            drop(started);
            panic!(
                "no «{said}» in {} {}\n{seen}\nstdout {:?}\nstderr {:?}\n(left in place: {})",
                log.display(),
                if exited { "before it exited" } else { "within 30s" },
                piped(&out),
                piped(&err),
                home.display(),
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let shown = stdout_ok(&home, &["logs", "-n", "50"]);
    assert!(shown.contains(&said), "{shown}");

    let status: Value = serde_json::from_str(&stdout_ok(&home, &["status", "--json"])).unwrap();
    assert_eq!(Path::new(status["supervisor"]["log"].as_str().unwrap()), log.as_path());
    assert!(status["supervisor"]["journal"].is_null());

    drop(started);
    let (out, err) = (piped(&out).expect("stdout closed"), piped(&err).expect("stderr closed"));
    assert!(!out.contains("supervisor up") && !err.contains("supervisor up"), "stdout {out:?} stderr {err:?}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The bundle as `mafold diagnose` writes it, from a HOME that holds what a
/// real one does: bot tokens and a pinned API key in the config, two logins,
/// and logs that mention a token, an email and somebody's message.
#[test]
fn diagnose_writes_a_bundle_without_the_secrets_it_was_handed() {
    let home = temp_home("diagnose");
    let bot_token = "mb_e2eBotToken_7f6e5d4c3b2a";
    let other_bot = "mb_e2eOtherBot_1a2b3c4d5e6f";
    let session = "s_e2eSession_0123456789abcdef";
    let second = "s_e2eSecond_fedcba9876543210";
    let pinned = "sk-proj-e2ePinnedKey0123456789";
    let stranger = "ghp_e2eStrangerTokenAbCdEf012345";
    put(
        &home,
        "daemons.json",
        &serde_json::json!({ "daemons": [
            { "name": "a:one", "token": bot_token, "workdir": home.join("work").display().to_string(),
              "env": { "OPENAI_API_KEY": pinned } },
            { "name": "a:two", "token": other_bot, "workdir": "/tmp", "harness": "codex" },
        ]})
        .to_string(),
    );
    put(
        &home,
        "session.json",
        &serde_json::json!({ "token": session, "username": "alice", "device_id": "dev-1", "device_name": "mac",
            "accounts": [{ "username": "alice", "token": session }, { "username": "bob", "token": second }] })
        .to_string(),
    );
    // How a file a cli since C6 started begins (`logfile::KEPT`).
    let kept = "── this log is kept by mafold 0.9.149 · pid 7 ──";
    put(&home, "supervisor.log", &format!("{kept}\nsupervisor up (pid 7) · base {DEAD_API}\nheartbeat for alice failed: 401 with {session}\n"));
    put(
        &home,
        "daemons/a_one/log",
        &format!(
            "{kept}\n\
             mafold agent ✓ connected as @one\n\
             ws reconnect wss://api.mafold.com/ws?token={bot_token}\n\
             export OPENAI_API_KEY={pinned}\n\
             git push with {stranger}\n\
             invite sent to alice.smith@example.com\n\
             handle error: turn failed: codex exited unsuccessfully: Error reading memo.md: the merger closes Friday\n\
             ← @alice ⟫ the launch plan is in the shared drive, ping me\n"
        ),
    );
    // What a cli before C6 left: a message copied in as it came, across lines.
    put(&home, "daemons/a_one/log.1", "← @bob: the board meets Monday\nand the price is 41 a share\n");

    let out_path = home.join("bundle.json");
    let printed: Value = serde_json::from_str(
        stdout_ok(&home, &["diagnose", "--out", out_path.to_str().unwrap(), "--json"]).trim(),
    )
    .unwrap();
    let text = std::fs::read_to_string(&out_path).unwrap();
    // The five values in the logs above. More where the binary itself sits
    // under somebody's home — the account name in `exe` and `exe_canonical`
    // (a developer's machine, `/home/runner` on CI) — but not on a Windows
    // runner, which builds it in `D:\a\…`: only these five are certain.
    assert!(printed["masked"].as_u64().unwrap() >= 5, "{printed}\n{text}");
    for gone in [
        bot_token, other_bot, session, second, pinned, stranger, "alice.smith@example.com", "launch plan", "merger closes",
        "board meets", "41 a share",
    ] {
        assert!(!text.contains(gone), "{gone} is in the bundle:\n{text}");
    }
    // Nor a prefix long enough to be worth anything.
    for gone in [&bot_token[..12], &session[..12], &pinned[..14]] {
        assert!(!text.contains(gone), "{gone}… is in the bundle:\n{text}");
    }
    let b: Value = serde_json::from_str(&text).unwrap();
    // In each string as it reads, not in the file: JSON doubles a Windows
    // path's backslashes, and `C:\Users\…` is never in `C:\\Users\\…`.
    let home_text = plain_path(&home.display().to_string());
    for s in strings(&b) {
        assert!(!plain_path(s).contains(&home_text), "the home directory carries the account name: {s}\n{text}");
    }

    let sup_lines: Vec<&str> =
        b["logs"]["supervisor"]["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    assert_eq!(sup_lines[2], "heartbeat for alice failed: 401 with ••••••••");
    assert_eq!(b["diagnose"], 1);
    assert!(b["status"]["supervisor"].is_object(), "{b}");
    let daemons = b["config"]["daemons"].as_array().unwrap();
    assert_eq!(daemons.len(), 2);
    assert_eq!(daemons[0]["name"], "a:one");
    assert_eq!(daemons[0]["env"], serde_json::json!(["OPENAI_API_KEY"]), "names of pinned variables, never values");
    let bot = &b["logs"]["bots"]["a:one"];
    let bot_lines: Vec<&str> = bot["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
    assert_eq!(bot_lines[..2], [kept, "mafold agent ✓ connected as @one"]);
    // Each of the five masked in place, the rest of its line as it was.
    assert_eq!(
        bot_lines[2..6],
        [
            "ws reconnect wss://api.mafold.com/ws?token=••••••••",
            "export OPENAI_API_KEY=••••••••",
            "git push with ••••••••",
            "invite sent to ••••@••••",
        ]
    );
    assert_eq!(bot_lines[6], "handle error: turn failed: codex exited unsuccessfully: [47 chars]");
    assert_eq!(*bot_lines.last().unwrap(), "← @alice ⟫ [47 chars]");
    // The file from before C6 is named, not read.
    let left_out = bot["left_out"].as_array().unwrap();
    assert_eq!(left_out.len(), 1, "{bot}");
    assert!(left_out[0]["path"].as_str().unwrap().ends_with("log.1"), "{bot}");
    assert!(sup_lines[1].starts_with("supervisor up (pid 7)"));
    let _ = std::fs::remove_dir_all(&home);
}
