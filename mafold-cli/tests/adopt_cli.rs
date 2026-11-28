//! `mafold up --adopt --check` end to end — the real binary in a throwaway
//! HOME (activity center C10). It says whose the service here is and what
//! adopting it would do, and changes nothing.
//!
//! Only `--check` runs here, never the move itself: `systemctl --user` and
//! `launchctl` act on the REAL session's service whatever HOME says, so an
//! adopt run by a test would stop and re-register the developer's own
//! supervisor — and every bot on the machine. What the move does is in the
//! unit tests (`supervisor::adopt_tests`) and was driven by hand against a
//! throwaway Linux user's own systemd (the PR says how).

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BOT: &str = "t:bot";

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-adopt-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".mafold/daemons")).expect("make temp home");
    std::fs::create_dir_all(dir.join("bin")).expect("make temp PATH");
    dir
}

/// An api that turns every request away (401): a supervisor and its daemon
/// stay up against it without any Mafold.
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

/// The binary, in `home` and nowhere else (this suite runs inside a bot's
/// turn too: no token, account or daemon name from the shell).
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
        .env_remove("MAFOLD_NO_AUTO_UPDATE")
        .stdin(Stdio::null());
    c
}

fn configure_bot(home: &Path) {
    let cfg = serde_json::json!({ "daemons": [{
        "name": BOT, "token": "mb_test", "workdir": home.display().to_string(), "harness": "claude-code", "env": {}
    }] });
    std::fs::write(home.join(".mafold/daemons.json"), cfg.to_string()).unwrap();
}

/// The service file, as this platform's `ensure_autostart` writes it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn register(home: &Path, exe: &str, base: &str, no_auto_update: bool) -> PathBuf {
    #[cfg(target_os = "linux")]
    let (path, text) = (
        home.join(".config/systemd/user/mafold-supervisor.service"),
        format!(
            "[Unit]\nDescription=Mafold bot supervisor\n\n[Service]\nEnvironment=PATH=/usr/bin\nExecStart={exe} --base {base} supervise{}\nRestart=always\n\n[Install]\nWantedBy=default.target\n",
            if no_auto_update { " --no-auto-update" } else { "" }
        ),
    );
    #[cfg(target_os = "macos")]
    let (path, text) = (
        home.join("Library/LaunchAgents/com.mafold.supervisor.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n  <key>Label</key><string>com.mafold.supervisor</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{exe}</string>\n    <string>--base</string><string>{base}</string>\n    <string>supervise</string>{}\n  </array>\n</dict></plist>\n",
            if no_auto_update { "\n    <string>--no-auto-update</string>" } else { "" }
        ),
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

/// `mafold up --adopt --check --json`: exit code, its JSON.
fn check(home: &Path, base: &str) -> (Option<i32>, Value) {
    let out = mafold(home, base).args(["up", "--adopt", "--check", "--json"]).output().expect("run mafold up --adopt --check");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let json = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("up --adopt --check --json printed {text:?} ({e}); stderr {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code(), json)
}

fn reasons(v: &Value) -> Vec<String> {
    v["reasons"].as_array().expect("reasons").iter().map(|r| r.as_str().unwrap().to_string()).collect()
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn another_installs_service_is_adoptable_and_its_bots_are_named() {
    let home = temp_home("elsewhere");
    configure_bot(&home);
    let base = "https://api.mafold.com";
    let file = register(&home, "/opt/other-install/mafold", base, false);
    let before = std::fs::read(&file).unwrap();
    let (code, v) = check(&home, base);
    assert_eq!(code, Some(0), "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["code"], "adoptable");
    assert_eq!(reasons(&v), vec!["elsewhere"]);
    assert_eq!(v["registered_exe"], "/opt/other-install/mafold");
    assert_eq!(v["bots"][0]["name"], BOT, "the confirmation lists the bots it restarts: {v}");
    assert_eq!(v["auto_update"], true);
    // A check changes nothing.
    assert_eq!(std::fs::read(&file).unwrap(), before);
    // And said to a person: what moves, and which bots restart.
    let out = mafold(&home, base).args(["up", "--adopt", "--check"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("the service runs /opt/other-install/mafold"), "{text}");
    assert!(text.contains(&format!("restarts the bot once ({BOT})")), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn the_old_update_flag_on_this_binary_is_a_reason_and_the_setting_is_said() {
    let home = temp_home("flag");
    let base = "https://api.mafold.com";
    register(&home, env!("CARGO_BIN_EXE_mafold"), base, true);
    // Updates off the new way stays off: adopting drops only the flag.
    std::fs::write(home.join(".mafold/auto-update"), "off\n").unwrap();
    let (code, v) = check(&home, base);
    assert_eq!(code, Some(0), "{v}");
    assert_eq!(v["code"], "adoptable");
    assert_eq!(reasons(&v), vec!["update_flag"]);
    assert_eq!(v["auto_update"], false, "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn this_binarys_own_service_is_managed_already() {
    let home = temp_home("managed");
    let base = "https://api.mafold.com";
    register(&home, env!("CARGO_BIN_EXE_mafold"), base, false);
    let (code, v) = check(&home, base);
    assert_eq!(code, Some(0), "{v}");
    assert_eq!(v["code"], "managed");
    assert_eq!(reasons(&v), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn a_service_on_another_mafold_is_refused_not_moved() {
    let home = temp_home("base");
    register(&home, "/opt/other-install/mafold", "http://127.0.0.1:4100", false);
    let (code, v) = check(&home, "https://api.mafold.com");
    assert_eq!(code, Some(1), "{v}");
    assert_eq!(v["ok"], false);
    assert_eq!(v["code"], "base_differs");
    assert!(v["detail"].as_str().unwrap().contains("mafold --base http://127.0.0.1:4100 up --adopt"), "{v}");
    let _ = std::fs::remove_dir_all(&home);
}

// Windows reads the machine's real Task Scheduler, whatever HOME says: a
// developer's own Mafold task would answer for the temp HOME.
#[test]
#[cfg(unix)]
fn nothing_set_up_is_said_as_such() {
    let home = temp_home("none");
    let (code, v) = check(&home, "https://api.mafold.com");
    assert_eq!(code, Some(0), "{v}");
    assert_eq!(v["code"], "not_set_up");
    assert_eq!(reasons(&v), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&home);
}

/// A supervisor started in a terminal, killed with the test.
struct ByHand {
    child: Child,
    home: PathBuf,
}

impl Drop for ByHand {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Its daemons are detached: by the pids it wrote, in this home only.
        for d in std::fs::read_dir(self.home.join(".mafold/daemons")).into_iter().flatten().flatten() {
            let Some(pid) = std::fs::read_to_string(d.path().join("pid")).ok().and_then(|s| s.trim().parse::<u32>().ok()) else { continue };
            #[cfg(unix)]
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).stderr(Stdio::null()).status();
            #[cfg(windows)]
            let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
#[cfg(unix)]
fn a_supervisor_started_by_hand_is_adoptable_and_status_says_why() {
    let home = temp_home("byhand");
    configure_bot(&home);
    let base = refusing_api();
    let child = mafold(&home, &base)
        .args(["--base", &base, "--no-auto-update", "supervise"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the supervisor by hand");
    let sup = ByHand { child, home: home.clone() };
    let pid = sup.child.id();
    let t = Instant::now();
    let v = loop {
        let (code, v) = check(&home, &base);
        if v["was_running"].as_u64() == Some(pid as u64) {
            assert_eq!(code, Some(0), "{v}");
            break v;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "the supervisor never showed up: {v}");
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(v["code"], "adoptable");
    assert_eq!(reasons(&v), vec!["by_hand"]);
    // No service file says its Mafold; the supervisor's own record does.
    assert_eq!(v["service_base"], base.as_str(), "{v}");
    // Asked from another Mafold, it's refused — not moved there.
    let (code, other) = check(&home, "https://api.mafold.com");
    assert_eq!((code, other["code"].as_str()), (Some(1), Some("base_differs")), "{other}");
    let out = mafold(&home, &base).args(["status", "--json"]).output().unwrap();
    let status: Value = serde_json::from_slice(&out.stdout).expect("status --json");
    assert_eq!(status["supervisor"]["not_ours"], serde_json::json!(["by_hand"]), "{}", status["supervisor"]);
    drop(sup);
}

#[test]
fn a_binary_that_can_adopt_says_so() {
    let home = temp_home("caps");
    let out = mafold(&home, "https://api.mafold.com").arg("capabilities").output().unwrap();
    let caps: Vec<String> = serde_json::from_slice(&out.stdout).expect("capabilities");
    assert!(caps.iter().any(|c| c == "adopt"), "{caps:?}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn check_and_json_need_adopt() {
    let home = temp_home("flags");
    let out = mafold(&home, "https://api.mafold.com").args(["up", "--check"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--adopt"), "{}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&home);
}
