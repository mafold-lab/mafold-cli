//! `mafold update` end to end (activity center C8) — the real binary, COPIED
//! into a throwaway HOME so an install replaces that copy and nothing else,
//! against a stand-in api that offers a release and counts every check and
//! every download. Linux runs these with the rest of the suite, macOS and
//! Windows in `cli-log-tests.yml`.
//!
//! - `--check` says what is out and downloads nothing;
//! - turning updates off is a machine setting: the service registration is
//!   left exactly as it was (a registration with `--no-auto-update` is what made
//!   the desktop app show the computer read-only);
//! - the supervisor reads that setting on every pass;
//! - an install asks the supervisor to move onto it, and it does once the
//!   replies in progress are written.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BOT: &str = "t:bot";
/// The release the stand-in offers while it offers one.
const NEWER: &str = "9.9.9";

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-update-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".mafold/daemons")).expect("make temp home");
    std::fs::create_dir_all(dir.join("bin")).expect("make temp PATH");
    dir
}

/// This build of mafold, copied into `home/bin` — the binary every command
/// below runs, so `update` installs over the copy.
fn install_copy(home: &Path) -> PathBuf {
    let name = if cfg!(windows) { "mafold.exe" } else { "mafold" };
    let to = home.join("bin").join(name);
    std::fs::copy(env!("CARGO_BIN_EXE_mafold"), &to).expect("copy the binary");
    to
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Default)]
struct Seen {
    checks: usize,
    downloads: usize,
    /// Offers `NEWER` while true; else this very build, which isn't installed.
    offering: bool,
}

/// A stand-in Mafold api: `cliUpdateCheck` answers with a release whose
/// download is this build's own bytes, and stops offering it once it has been
/// downloaded (so the supervisor that moves onto it doesn't fetch it again).
/// Everything else is turned away (401) — a daemon asking who it is waits ten
/// seconds and asks again, up for the length of a test without any Mafold.
struct Api {
    base: String,
    seen: Arc<Mutex<Seen>>,
}

impl Api {
    fn start(offering: bool) -> Api {
        let bytes = Arc::new(std::fs::read(env!("CARGO_BIN_EXE_mafold")).expect("read the binary"));
        let sha = sha256_hex(&bytes);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stand-in api");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen { offering, ..Seen::default() }));
        let (b, s) = (base.clone(), seen.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (bytes, sha, base, seen) = (bytes.clone(), sha.clone(), b.clone(), s.clone());
                std::thread::spawn(move || serve(stream, &bytes, &sha, &base, &seen));
            }
        });
        Api { base, seen }
    }

    fn checks(&self) -> usize {
        self.seen.lock().unwrap().checks
    }

    fn downloads(&self) -> usize {
        self.seen.lock().unwrap().downloads
    }
}

fn serve(mut s: TcpStream, bytes: &[u8], sha: &str, base: &str, seen: &Mutex<Seen>) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    };
    let text = String::from_utf8_lossy(&got[..end]).to_string();
    let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
    // Read the body to its end (what the check asks doesn't change the answer
    // here): a socket closed with unread bytes is reset, not answered.
    let length = text
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    while got.len() < end + length {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    let (status, ctype, body): (&str, &str, Vec<u8>) = if path == "/api/cliUpdateCheck" {
        let mut seen = seen.lock().unwrap();
        seen.checks += 1;
        let version = if seen.offering { NEWER } else { env!("CARGO_PKG_VERSION") };
        let answer = json!({ "ok": true, "result": {
            "known": true, "channel": "stable", "version": version,
            "url": format!("{base}/dl/mafold"), "sha256": sha,
        } });
        ("200 OK", "application/json", answer.to_string().into_bytes())
    } else if path == "/dl/mafold" {
        let mut seen = seen.lock().unwrap();
        seen.downloads += 1;
        seen.offering = false;
        ("200 OK", "application/octet-stream", bytes.to_vec())
    } else {
        ("401 Unauthorized", "application/json", b"{}".to_vec())
    };
    let _ = write!(s, "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
    let _ = s.write_all(&body);
}

/// `bin`, in `home` and nowhere else: no runtime of the developer's machine
/// on PATH, no token, account, channel or daemon name from the shell that
/// runs the tests (this suite runs inside a bot's turn too).
fn mafold(bin: &Path, home: &Path, base: &str) -> Command {
    let mut c = Command::new(bin);
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

/// `mafold update <args> --json`: exit code, its JSON, its stderr.
fn update(bin: &Path, home: &Path, base: &str, args: &[&str]) -> (Option<i32>, Value, String) {
    let out = mafold(bin, home, base).arg("update").args(args).arg("--json").output().expect("run mafold update");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let json = serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("update --json printed {text:?} ({e}); stderr {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code(), json, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn status(bin: &Path, home: &Path, base: &str) -> Value {
    let out = mafold(bin, home, base).args(["status", "--json"]).output().expect("run mafold status");
    serde_json::from_slice(&out.stdout).expect("status --json")
}

fn bot_pid(bin: &Path, home: &Path, base: &str) -> Option<u64> {
    let v = status(bin, home, base);
    v["daemons"].as_array()?.iter().find(|d| d["name"] == BOT)?["pid"].as_u64()
}

fn configure_bot(home: &Path) {
    let cfg = json!({ "daemons": [{
        "name": BOT, "token": "mb_test", "workdir": home.display().to_string(), "harness": "claude-code", "env": {}
    }] });
    std::fs::write(home.join(".mafold/daemons.json"), cfg.to_string()).unwrap();
}

/// The bot's in-flight-reply count, as its daemon writes it for the drain.
fn busy(home: &Path, n: usize) {
    let dir = home.join(".mafold/daemons/t_bot");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("busy"), n.to_string()).unwrap();
}

/// A supervisor running from `bin` in `home`, and every daemon it started,
/// stopped when the test ends however it ends — by the pids it wrote (an
/// update re-execs it: a new process on Windows), in this home only.
struct Supervisor {
    child: Child,
    home: PathBuf,
}

impl Supervisor {
    fn start(bin: &Path, home: &Path, base: &str, updates: bool) -> Supervisor {
        let mut cmd = mafold(bin, home, base);
        cmd.args(["--base", base]);
        if !updates {
            cmd.arg("--no-auto-update");
        }
        let child = cmd.arg("supervise").stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("start the supervisor");
        Supervisor { child, home: home.to_path_buf() }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.join(".mafold/supervisor.log")).unwrap_or_default()
    }

    /// How many times a supervisor has started in this home.
    fn starts(&self) -> usize {
        self.log().matches("supervisor up (pid").count()
    }

    /// Wait for `f` to say something, up to `max` — and when it doesn't, say
    /// what the supervisor logged.
    fn wait<T>(&self, what: &str, max: Duration, mut f: impl FnMut() -> Option<T>) -> T {
        let t = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(t.elapsed() < max, "{what}: not within {max:?}\n--- supervisor log ---\n{}", self.log());
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let pid_of = |p: PathBuf| std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<u32>().ok());
        if let Some(pid) = pid_of(self.home.join(".mafold/supervisor.pid")) {
            kill(pid);
        }
        let dir = self.home.join(".mafold/daemons");
        for d in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if let Some(pid) = pid_of(d.path().join("pid")) {
                kill(pid);
            }
        }
        std::thread::sleep(Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn kill(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).stderr(Stdio::null()).status();
    #[cfg(windows)]
    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

fn alive(pid: u64) -> bool {
    #[cfg(unix)]
    {
        Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().is_ok_and(|o| {
            let stat = String::from_utf8_lossy(&o.stdout);
            !stat.trim().is_empty() && !stat.trim().starts_with('Z')
        })
    }
    #[cfg(windows)]
    {
        Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
        })
    }
}

#[test]
fn a_check_says_what_is_out_and_downloads_nothing() {
    let home = temp_home("check");
    let bin = install_copy(&home);
    let before = std::fs::read(&bin).unwrap();
    let api = Api::start(true);

    let (code, r, err) = update(&bin, &home, &api.base, &["--check"]);
    assert_eq!(code, Some(0), "{r} {err}");
    assert_eq!((r["ok"].as_bool(), r["code"].as_str()), (Some(true), Some("available")), "{r}");
    assert_eq!(r["latest"], NEWER, "{r}");
    assert_eq!(r["version"], env!("CARGO_PKG_VERSION"), "{r}");
    assert_eq!((r["channel"].as_str(), r["auto_update"].as_bool()), (Some("stable"), Some(true)), "{r}");
    // In words, for a person.
    let out = mafold(&bin, &home, &api.base).args(["update", "--check"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains(&format!("v{NEWER} is available on the stable channel")), "{said}");

    assert_eq!(api.checks(), 2);
    assert_eq!(api.downloads(), 0, "a check downloads nothing");
    assert!(std::fs::read(&bin).unwrap() == before, "the binary is untouched");
    let left: Vec<_> = std::fs::read_dir(home.join("bin")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(left.len(), 1, "no backup, no download left beside it: {left:?}");

    // Nothing newer: current — and the release it compared with is said.
    api.seen.lock().unwrap().offering = false;
    let (code, r, _) = update(&bin, &home, &api.base, &["--check"]);
    assert_eq!((code, r["code"].as_str()), (Some(0), Some("current")), "{r}");
    assert_eq!(r["latest"], env!("CARGO_PKG_VERSION"), "{r}");
    assert_eq!(api.downloads(), 0);
    let _ = std::fs::remove_dir_all(&home);
}

/// Off is a setting the supervisor reads — not a flag the service is
/// registered with, so the service is left byte for byte as it was and the
/// desktop app still reads it as its own.
#[test]
fn turning_updates_off_is_a_setting_and_leaves_the_service_as_it_was() {
    let home = temp_home("setting");
    let bin = install_copy(&home);
    let api = Api::start(true);
    // A service registered the way the desktop app registers it.
    let registered: Option<(PathBuf, String)> = if cfg!(target_os = "macos") {
        let exe = bin.display();
        Some((
            home.join("Library/LaunchAgents/com.mafold.supervisor.plist"),
            format!("<plist version=\"1.0\"><dict>\n  <key>Label</key><string>com.mafold.supervisor</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{exe}</string>\n    <string>--base</string>\n    <string>{}</string>\n    <string>supervise</string>\n  </array>\n</dict></plist>\n", api.base),
        ))
    } else if cfg!(target_os = "linux") {
        Some((
            home.join(".config/systemd/user/mafold-supervisor.service"),
            format!("[Service]\nExecStart={} --base {} supervise\n", bin.display(), api.base),
        ))
    } else {
        None // a scheduled task, which this test can't make; the setting is still checked
    };
    if let Some((p, text)) = &registered {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        assert_eq!(status(&bin, &home, &api.base)["supervisor"]["no_auto_update"], false);
    }

    let (code, r, err) = update(&bin, &home, &api.base, &["--auto-update", "off"]);
    assert_eq!(code, Some(0), "{r} {err}");
    assert_eq!((r["ok"].as_bool(), r["code"].as_str(), r["auto_update"].as_bool()), (Some(true), Some("set"), Some(false)), "{r}");
    assert_eq!(std::fs::read_to_string(home.join(".mafold/auto-update")).unwrap(), "off");
    assert_eq!((api.checks(), api.downloads()), (0, 0), "setting it asks for nothing and installs nothing");

    let s = status(&bin, &home, &api.base);
    assert_eq!(s["update"]["auto_update_setting"], false, "{}", s["update"]);
    if let Some((p, text)) = &registered {
        assert_eq!(&std::fs::read_to_string(p).unwrap(), text, "the service registration is untouched");
        // What the desktop app's `isForeign` reads.
        assert_eq!(s["supervisor"]["no_auto_update"], false, "{}", s["supervisor"]);
        assert_eq!(s["update"]["auto_update"], false, "registered to update, but the setting says off: {}", s["update"]);
    }
    // A check still says what's out: off holds back installs, not knowing.
    let (_, r, _) = update(&bin, &home, &api.base, &["--check"]);
    assert_eq!((r["code"].as_str(), r["auto_update"].as_bool()), (Some("available"), Some(false)), "{r}");

    let (code, r, _) = update(&bin, &home, &api.base, &["--auto-update", "on"]);
    assert_eq!((code, r["auto_update"].as_bool()), (Some(0), Some(true)), "{r}");
    assert_eq!(status(&bin, &home, &api.base)["update"]["auto_update_setting"], true);
    assert_eq!(api.downloads(), 0);
    let _ = std::fs::remove_dir_all(&home);
}

/// A setting that can't be written fails — and with `--json` still as the one
/// object a program reads, not an error line on stderr alone.
/// `mafold rollback <args> --json`: exit code and its JSON.
fn rollback(bin: &Path, home: &Path, base: &str, args: &[&str]) -> (Option<i32>, Value) {
    let out = mafold(bin, home, base).arg("rollback").args(args).arg("--json").output().expect("run mafold rollback");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let json = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("rollback --json printed {text:?} ({e}); stderr {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code(), json)
}

/// Activity center C9. A rollback used to hold for one check: ~10 minutes
/// later the updater found the release rolled back from newer than the binary
/// it was rolled back to, and put it back. Now the version rolled back from is
/// skipped — and a backup too old to know that is refused without `--force`.
#[test]
fn a_rollback_sticks_and_one_that_cant_is_refused() {
    let home = temp_home("rollback");
    let bin = install_copy(&home);
    let api = Api::start(false);
    let here = env!("CARGO_PKG_VERSION");
    let backup = bin.with_file_name("mafold.old");

    let (code, r) = rollback(&bin, &home, &api.base, &["--check"]);
    assert_eq!((code, r["code"].as_str()), (Some(0), Some("no_backup")), "{r}");
    let (code, r) = rollback(&bin, &home, &api.base, &[]);
    assert_eq!((code, r["ok"].as_bool(), r["code"].as_str()), (Some(1), Some(false), Some("no_backup")), "{r}");

    // A backup from before the skip: it says its version, it fails `capabilities`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&backup, "#!/bin/sh\n[ \"$1\" = --version ] && echo 'mafold 0.9.140' && exit 0\nexit 2\n").unwrap();
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (code, r) = rollback(&bin, &home, &api.base, &["--check"]);
        assert_eq!((code, r["code"].as_str(), r["sticks"].as_bool()), (Some(0), Some("too_old"), Some(false)), "{r}");
        assert_eq!(r["to"], "0.9.140", "{r}");
        let (code, r) = rollback(&bin, &home, &api.base, &[]);
        assert_eq!((code, r["code"].as_str()), (Some(1), Some("too_old")), "refused without --force: {r}");
        assert_eq!(std::fs::read(&bin).unwrap(), std::fs::read(env!("CARGO_BIN_EXE_mafold")).unwrap(), "nothing was swapped");
        assert!(!home.join(".mafold/update-skip.json").exists(), "nor a skip written");
    }

    // A backup that knows the skip (this very build): it rolls back, and sticks.
    std::fs::copy(env!("CARGO_BIN_EXE_mafold"), &backup).unwrap();
    let (code, r) = rollback(&bin, &home, &api.base, &["--check"]);
    assert_eq!((code, r["code"].as_str(), r["sticks"].as_bool()), (Some(0), Some("possible"), Some(true)), "{r}");
    let (code, r) = rollback(&bin, &home, &api.base, &[]);
    assert_eq!((code, r["ok"].as_bool(), r["code"].as_str()), (Some(0), Some(true), Some("rolled_back")), "{r}");
    let skip: Value = serde_json::from_slice(&std::fs::read(home.join(".mafold/update-skip.json")).unwrap()).unwrap();
    assert_eq!(skip["version"], here, "the version rolled back from is the one skipped: {skip}");
    assert_eq!(status(&bin, &home, &api.base)["update"]["skipped"]["version"], here);
    let _ = std::fs::remove_dir_all(&home);
}

/// The skip is honoured by the supervisor's own updates: the version rolled
/// back from is checked and left alone, every time; a NEWER release ends the
/// skip and installs.
#[test]
fn the_version_rolled_back_from_is_not_put_back_until_a_newer_one_is_out() {
    let home = temp_home("skip");
    let bin = install_copy(&home);
    let api = Api::start(true);
    configure_bot(&home);
    let skip = home.join(".mafold/update-skip.json");
    std::fs::write(&skip, json!({ "version": NEWER, "at": 1_760_000_000 }).to_string()).unwrap();
    let sup = Supervisor::start(&bin, &home, &api.base, true);
    sup.wait("the daemon started", Duration::from_secs(30), || bot_pid(&bin, &home, &api.base));

    let nudge = home.join(".mafold/update-nudge");
    std::fs::write(&nudge, "").unwrap();
    sup.wait("checked", Duration::from_secs(25), || (api.checks() >= 1).then_some(()));
    sup.wait("the skip said", Duration::from_secs(10), || {
        sup.log().contains(&format!("skipped: this computer rolled back from v{NEWER}")).then_some(())
    });
    assert_eq!(api.downloads(), 0, "the version rolled back from was put back\n{}", sup.log());
    assert!(skip.exists());

    // Rolled back from an OLDER release than the one now out: that newer one is the fix.
    std::fs::write(&skip, json!({ "version": "9.9.8", "at": 1_760_000_000 }).to_string()).unwrap();
    std::fs::write(&nudge, "").unwrap();
    sup.wait("installed", Duration::from_secs(30), || (api.downloads() == 1).then_some(()));
    assert!(!skip.exists(), "a newer release ends the skip\n{}", sup.log());
    drop(sup);
}

#[test]
fn a_setting_that_cant_be_written_fails_as_one_json_object() {
    let home = temp_home("unwritable");
    let bin = install_copy(&home);
    let api = Api::start(true);
    std::fs::create_dir_all(home.join(".mafold/auto-update")).unwrap(); // a directory where the file goes
    let (code, r, _) = update(&bin, &home, &api.base, &["--auto-update", "off"]);
    assert_eq!((code, r["ok"].as_bool(), r["code"].as_str()), (Some(1), Some(false), Some("failed")), "{r}");
    assert!(r["detail"].as_str().unwrap_or("").contains("auto-update"), "{r}");
    let _ = std::fs::remove_dir_all(&home);
}

/// The supervisor reads the setting on every pass: off, a nudge (what a
/// daemon writes when Mafold announces a release) checks nothing; on again,
/// the next nudge installs and the supervisor moves onto it — no restart in
/// between, nothing re-registered.
#[test]
fn the_supervisor_reads_the_setting_on_every_pass() {
    let home = temp_home("pass");
    let bin = install_copy(&home);
    let api = Api::start(true);
    configure_bot(&home);
    std::fs::write(home.join(".mafold/auto-update"), "off").unwrap();
    let sup = Supervisor::start(&bin, &home, &api.base, true);
    sup.wait("the daemon started", Duration::from_secs(30), || bot_pid(&bin, &home, &api.base));
    sup.wait("the setting said in the log", Duration::from_secs(15), || {
        sup.log().contains("automatic updates are off on this computer").then_some(())
    });

    let nudge = home.join(".mafold/update-nudge");
    std::fs::write(&nudge, "").unwrap();
    sup.wait("the nudge taken", Duration::from_secs(25), || (!nudge.exists()).then_some(()));
    sup.wait("the nudge said", Duration::from_secs(5), || sup.log().contains("so not checking").then_some(()));
    assert_eq!((api.checks(), api.downloads()), (0, 0), "off: not even a check\n{}", sup.log());

    let (code, r, _) = update(&bin, &home, &api.base, &["--auto-update", "on"]);
    assert_eq!((code, r["auto_update"].as_bool()), (Some(0), Some(true)), "{r}");
    std::fs::write(&nudge, "").unwrap();
    sup.wait("installed", Duration::from_secs(30), || (api.downloads() == 1).then_some(()));
    sup.wait("moved onto it", Duration::from_secs(30), || (sup.starts() >= 2).then_some(()));
    let log = sup.log();
    assert!(log.contains("automatic updates are on again"), "{log}");
    assert!(log.contains(&format!("update v{NEWER} available — applying")), "{log}");
    assert_eq!(api.downloads(), 1, "{log}");
    drop(sup);
}

/// `mafold update` installs, then asks the supervisor to move onto it — which
/// it does only once the reply in progress is written: the bot's daemon runs
/// on, untouched, while it is.
#[test]
fn a_manual_update_waits_for_the_reply_in_progress() {
    let home = temp_home("manual");
    let bin = install_copy(&home);
    let api = Api::start(true);
    configure_bot(&home);
    // Started with updates off: what moves it is the command alone.
    let sup = Supervisor::start(&bin, &home, &api.base, false);
    let first = sup.wait("the daemon started", Duration::from_secs(30), || bot_pid(&bin, &home, &api.base));
    busy(&home, 1);

    let t = Instant::now();
    let cmd = mafold(&bin, &home, &api.base).args(["update", "--json"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    sup.wait("the supervisor waiting on the reply", Duration::from_secs(20), || {
        sup.log().contains(&format!("waiting for in-flight turns before restart: {BOT}=1")).then_some(())
    });
    assert_eq!(api.downloads(), 1);
    // While the reply is being written: the same daemon, the same supervisor.
    for _ in 0..10 {
        assert!(alive(first), "the daemon writing the reply was stopped\n{}", sup.log());
        assert_eq!(sup.starts(), 1, "{}", sup.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    busy(&home, 0);
    let out = cmd.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let r: Value = serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{text:?} ({e}) {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!((out.status.code(), r["code"].as_str(), r["latest"].as_str()), (Some(0), Some("updated"), Some(NEWER)), "{r}");
    assert_eq!(r["moving"], true, "{r}");
    // Read back within the command's own wait, or said to be on its way —
    // either way the move itself is checked below.
    let restart = r["restart"].as_str().unwrap_or("");
    assert!(
        restart.contains("restarted onto it") || restart.contains("as soon as the replies in progress are written"),
        "after {:?}: {r}\n{}",
        t.elapsed(),
        sup.log()
    );

    let second = sup.wait("the daemon on the new binary", Duration::from_secs(30), || {
        bot_pid(&bin, &home, &api.base).filter(|p| *p != first)
    });
    assert!(!alive(first), "the old daemon is gone");
    assert!(alive(second));
    assert_eq!(sup.starts(), 2, "{}", sup.log());
    // On the binary on disk now — not still the replaced one, which Linux
    // names `… (deleted)`.
    #[cfg(target_os = "linux")]
    {
        let pid = std::fs::read_to_string(home.join(".mafold/supervisor.pid")).unwrap();
        let exe = std::fs::read_link(format!("/proc/{}/exe", pid.trim())).unwrap();
        assert_eq!(exe, bin, "{}", sup.log());
    }
    drop(sup);
}
