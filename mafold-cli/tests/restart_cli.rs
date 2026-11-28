//! `mafold restart <bot>` end to end — the real binary, a real supervisor and
//! a real daemon, in a throwaway HOME (activity center C7). Linux runs these
//! with the rest of the suite, macOS and Windows in `cli-log-tests.yml`.
//!
//! The command asks the supervisor (a request in `~/.mafold/control/`) and
//! counts the restart only once the bot's pid file names a new process that
//! is running. A supervisor that takes no requests, or none at all, is said
//! as such — never a success.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BOT: &str = "t:bot";

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-restart-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".mafold/daemons")).expect("make temp home");
    std::fs::create_dir_all(dir.join("bin")).expect("make temp PATH");
    dir
}

/// An api that turns every request away (401). A daemon asking it who it is
/// waits ten seconds and asks again, five times (`agent::run`): up, and
/// staying up for the length of a test, without any Mafold.
fn refusing_api() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stand-in api");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 8192];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
            });
        }
    });
    format!("http://{addr}")
}

/// The binary, in `home` and nowhere else: no runtime of the developer's
/// machine on PATH, no token, account or daemon name from the shell that runs
/// the tests (this suite runs inside a bot's turn too).
fn mafold(home: &Path, base: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mafold"));
    c.env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", home.join("bin"))
        .env("MAFOLD_BASE", base)
        .env_remove("MAFOLD_ACCOUNT")
        .env_remove("MAFOLD_BOT_TOKEN")
        .env_remove("MAFOLD_CHANNEL")
        .env_remove("MAFOLD_DAEMON_NAME")
        .stdin(Stdio::null());
    c
}

fn configure_bot(home: &Path) {
    let cfg = serde_json::json!({ "daemons": [{
        "name": BOT, "token": "mb_test", "workdir": home.display().to_string(), "harness": "claude-code", "env": {}
    }] });
    std::fs::write(home.join(".mafold/daemons.json"), cfg.to_string()).unwrap();
}

/// `mafold restart` and what it said: exit code, its JSON, its stderr.
fn restart(home: &Path, base: &str, name: &str) -> (Option<i32>, Value, String) {
    let t = Instant::now();
    let out = mafold(home, base).args(["restart", name, "--json"]).output().expect("run mafold restart");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let json = serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("restart --json printed {text:?} ({e}) after {:?}", t.elapsed()));
    (out.status.code(), json, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// The bot's daemon as `mafold status --json` reads it: its pid while it runs.
fn bot_pid(home: &Path, base: &str) -> Option<u64> {
    let out = mafold(home, base).args(["status", "--json"]).output().expect("run mafold status");
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v["daemons"].as_array()?.iter().find(|d| d["name"] == BOT)?["pid"].as_u64()
}

fn status(home: &Path, base: &str) -> Value {
    let out = mafold(home, base).args(["status", "--json"]).output().expect("run mafold status");
    serde_json::from_slice(&out.stdout).expect("status --json")
}

fn running(home: &Path, base: &str, pid: u64) -> bool {
    bot_pid(home, base) == Some(pid)
}

/// A supervisor running in `home`, and every daemon it started, stopped
/// when the test ends however it ends — they live in `home`, nowhere else.
struct Supervisor {
    child: Child,
    home: PathBuf,
}

impl Supervisor {
    fn start(home: &Path, base: &str) -> Supervisor {
        let child = mafold(home, base)
            .args(["--base", base, "--no-auto-update", "supervise"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the supervisor");
        Supervisor { child, home: home.to_path_buf() }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.join(".mafold/supervisor.log")).unwrap_or_default()
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Its daemons are detached: by the pids it wrote, in this home only.
        let dir = self.home.join(".mafold/daemons");
        for d in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Some(pid) = std::fs::read_to_string(d.path().join("pid")).ok().and_then(|s| s.trim().parse::<u32>().ok()) else { continue };
            kill(pid);
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn kill(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).stderr(Stdio::null()).status();
    #[cfg(windows)]
    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn wait_for<T>(what: &str, max: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let t = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t.elapsed() < max, "{what}: not within {max:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_bot_is_restarted_and_read_back_from_its_new_pid() {
    let home = temp_home("restart");
    let api = refusing_api();
    configure_bot(&home);
    let sup = Supervisor::start(&home, &api);
    let first = wait_for("the daemon started", Duration::from_secs(30), || bot_pid(&home, &api));
    // Said in what `status` reads: this supervisor takes requests.
    assert_eq!(status(&home, &api)["supervisor"]["control"], 1, "{}", sup.log());

    let (code, r, err) = restart(&home, &api, BOT);
    assert_eq!(code, Some(0), "{r} {err}\n{}", sup.log());
    assert_eq!((r["ok"].as_bool(), r["code"].as_str()), (Some(true), Some("restarted")), "{r}");
    assert_eq!(r["old_pid"].as_u64(), Some(first), "{r}");
    let second = r["new_pid"].as_u64().expect("the new pid");
    assert_ne!(second, first);
    assert!(running(&home, &api, second), "read back here too: the new one runs");
    assert!(sup.log().contains(&format!("↻ restarted {BOT} as asked (pid {first} → pid {second})")), "{}", sup.log());

    // And for a person: the same restart, in words.
    let out = mafold(&home, &api).args(["restart", BOT]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said} {}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains(&format!("✓ restarted {BOT} (pid {second} → ")), "{said}");
    let third = bot_pid(&home, &api).expect("running after the second restart");
    assert!(third != second && third != first);
    // Every request was taken and answered, and every answer collected.
    let left: Vec<_> = std::fs::read_dir(home.join(".mafold/control")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "{left:?}");
    drop(sup);
}

/// A name no bot here goes by is a typo — said, with the handle it most
/// likely meant — and nothing is asked of the supervisor.
#[test]
fn a_name_that_isnt_configured_is_said_and_nothing_is_asked() {
    let home = temp_home("unknown");
    let api = refusing_api();
    configure_bot(&home);
    let (code, r, _) = restart(&home, &api, "bot");
    assert_eq!(code, Some(1));
    assert_eq!(r["code"], "unknown_bot", "{r}");
    assert!(r["detail"].as_str().unwrap_or_default().contains("did you mean `t:bot`"), "{r}");
    assert!(!home.join(".mafold/control").exists(), "nothing asked");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn with_no_supervisor_running_nothing_is_restarted() {
    let home = temp_home("nosup");
    let api = refusing_api();
    configure_bot(&home);
    let (code, r, err) = restart(&home, &api, BOT);
    assert_eq!(code, Some(1));
    assert_eq!((r["ok"].as_bool(), r["code"].as_str()), (Some(false), Some("no_supervisor")), "{r}");
    let human = mafold(&home, &api).args(["restart", BOT]).output().unwrap();
    assert!(String::from_utf8_lossy(&human.stderr).contains("`mafold up` starts it"), "{err}");
    let _ = std::fs::remove_dir_all(&home);
}

/// A supervisor from before restart requests would leave one lying there:
/// the command looks first, and says so at once — not a success, and no
/// request left behind for it to come across later.
#[test]
fn a_supervisor_from_before_restart_requests_is_never_a_success() {
    let home = temp_home("old");
    let api = refusing_api();
    configure_bot(&home);
    // A running process stands in for it: this test's own.
    let me = std::process::id();
    std::fs::write(home.join(".mafold/supervisor.pid"), me.to_string()).unwrap();
    for run in [Some(format!(r#"{{"pid":{me},"started_at":1,"version":"0.9.148","auto_update":true}}"#)), None] {
        match &run {
            Some(r) => std::fs::write(home.join(".mafold/supervisor.run.json"), r).unwrap(),
            None => {
                let _ = std::fs::remove_file(home.join(".mafold/supervisor.run.json"));
            }
        }
        let t = Instant::now();
        let (code, r, _) = restart(&home, &api, BOT);
        assert!(t.elapsed() < Duration::from_secs(5), "said at once: {:?}", t.elapsed());
        assert_eq!(code, Some(1));
        assert_eq!((r["ok"].as_bool(), r["code"].as_str()), (Some(false), Some("supervisor_too_old")), "{r}");
        let detail = r["detail"].as_str().unwrap_or_default();
        assert!(detail.contains("nothing was restarted") && detail.contains("`mafold down t:bot`"), "{detail}");
        if run.is_some() {
            assert!(detail.contains("(v0.9.148)"), "{detail}");
        }
        assert!(!home.join(".mafold/control").exists(), "nothing asked");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// A supervisor that says it takes requests but doesn't take this one: the
/// request is withdrawn, so it can't happen later out of the blue.
#[test]
fn a_request_nobody_takes_is_withdrawn_and_said() {
    let home = temp_home("untaken");
    let api = refusing_api();
    configure_bot(&home);
    let me = std::process::id();
    std::fs::write(home.join(".mafold/supervisor.pid"), me.to_string()).unwrap();
    std::fs::write(
        home.join(".mafold/supervisor.run.json"),
        format!(r#"{{"pid":{me},"started_at":1,"version":"0.9.150","auto_update":true,"control":1}}"#),
    )
    .unwrap();
    let (code, r, _) = restart(&home, &api, BOT);
    assert_eq!(code, Some(1));
    assert_eq!(r["code"], "not_taken", "{r}");
    assert!(r["detail"].as_str().unwrap_or_default().contains("nothing was restarted"), "{r}");
    let left: Vec<_> = std::fs::read_dir(home.join(".mafold/control")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "withdrawn: {left:?}");
    let _ = std::fs::remove_dir_all(&home);
}
