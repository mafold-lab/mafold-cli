//! `mafold status --json` end to end — the real binary reading a throwaway
//! HOME laid out the way the supervisor, its daemons and the heartbeat leave
//! it (activity center C5).
//!
//! Version 2 only adds: every field version 1 had keeps its name and shape,
//! because the desktop app already reads them. And `issues` is THE list of
//! what needs a person here; with the supervisor stopped it says that, and
//! nothing a running one would have to be there for.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Nothing is listening here, so nothing can reach the real api.
const DEAD_API: &str = "http://127.0.0.1:1";

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-status-test-{tag}-{}", std::process::id()));
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

/// A pid nothing is running as: a child that has already exited. Keep the
/// `Child` for the whole test — on Windows its open handle is what stops the
/// pid from going to the next process started.
fn dead_pid(home: &Path) -> (u32, std::process::Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mafold"))
        .env("HOME", home)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child.wait().unwrap();
    (child.id(), child)
}

fn status(home: &Path) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_mafold"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        // Only what the test put there: no runtime of the developer's machine
        // is found, so none is probed.
        .env("PATH", home.join("bin"))
        .env("MAFOLD_BASE", DEAD_API)
        .env_remove("MAFOLD_ACCOUNT")
        .env_remove("MAFOLD_BOT_TOKEN")
        .env_remove("MAFOLD_CHANNEL")
        .args(["status", "--json"])
        .output()
        .expect("run mafold");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

/// `account_health::fingerprint`: what a refusal is pinned to.
fn fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Two logins: `a` (current, heard from ten minutes ago) and `b` (refused).
fn two_logins(home: &Path, now: i64) {
    put(
        home,
        "session.json",
        r#"{"token":"s_a","username":"a","device_id":"dev-1","device_name":"mac",
            "accounts":[{"username":"a","token":"s_a"},{"username":"b","token":"s_b"}]}"#,
    );
    put(
        home,
        "account-health.json",
        &serde_json::json!({ "accounts": {
            "a": { "ok_at": now - 600 },
            "b": { "ok_at": now - 900, "refused": { "at": now - 800, "token": fingerprint("s_b") } },
        }})
        .to_string(),
    );
}

fn codes(v: &Value) -> Vec<(String, Option<String>)> {
    v["issues"]
        .as_array()
        .expect("issues is a list")
        .iter()
        .map(|i| (i["code"].as_str().unwrap().to_string(), i["subject"].as_str().map(str::to_string)))
        .collect()
}

/// Every version-1 field, with the type it had.
fn assert_version_one_shape(v: &Value) {
    assert!(v["version"].is_string());
    assert!(v["exe"].is_string() && v["exe_canonical"].is_string());
    let s = &v["supervisor"];
    assert!(s["running"].is_boolean() && s["autostart"].is_boolean());
    for k in ["pid", "registered_exe", "registered_exe_canonical", "no_auto_update", "doing"] {
        assert!(s.get(k).is_some(), "supervisor.{k} is still there");
    }
    assert!(v["accounts"].as_array().unwrap().iter().all(Value::is_string), "accounts stays a list of usernames");
    for d in v["daemons"].as_array().unwrap() {
        assert!(d["name"].is_string() && d["harness"].is_string());
        assert!(d["running"].is_boolean() && d["busy"].is_boolean());
    }
    for h in v["harnesses"].as_array().unwrap() {
        assert!(h["id"].is_string() && h["available"].is_boolean());
    }
    assert!(v["vault"].is_string());
}

#[test]
fn a_stopped_supervisor_is_the_one_thing_said_besides_a_refused_login() {
    let home = temp_home("stopped");
    let t = now();
    let (gone, _held) = dead_pid(&home);
    two_logins(&home, t);
    put(
        &home,
        "daemons.json",
        r#"{"daemons":[{"name":"a:one","token":"mb_1","workdir":"/tmp"},
                       {"name":"a:two","token":"mb_2","workdir":"/tmp","harness":"opencode"}]}"#,
    );
    // Everything that would be news with the supervisor up: a daemon that
    // keeps dying, a runtime that isn't there, an update that keeps failing,
    // a login Mafold hasn't heard from in ten minutes.
    put(&home, "daemons/a_one/pid", &format!("{gone}\n"));
    put(&home, "daemons/a_one/deaths", &format!("{}\n{}\n{}\n", t - 300, t - 200, t - 100));
    put(&home, "daemons/a_one/busy", "1");
    put(&home, "supervisor.pid", &format!("{gone}\n"));
    put(
        &home,
        "supervisor.run.json",
        &serde_json::json!({ "pid": gone, "started_at": t - 3600, "version": "0.9.149", "auto_update": true }).to_string(),
    );
    put(
        &home,
        "supervisor.exit.json",
        &serde_json::json!({ "pid": gone, "reason": "stopped", "at": t - 60, "version": "0.9.149" }).to_string(),
    );
    put(
        &home,
        "update.json",
        &serde_json::json!({ "failed": { "version": "99.0.0", "since": t - 900, "at": t - 100, "streak": 3, "error": "x" } })
            .to_string(),
    );

    let v = status(&home);
    assert_version_one_shape(&v);
    assert_eq!(v["schema"], 2);
    assert_eq!(v["device_id"], "dev-1");
    assert_eq!(v["supervisor"]["running"], false);
    assert_eq!(v["supervisor"]["last_exit"]["reason"], "stopped");
    assert_eq!(v["supervisor"]["last_exit"]["at"], t - 60);
    assert_eq!(v["supervisor"]["started_at"], Value::Null, "a supervisor that isn't running has no «since»");
    assert_eq!(v["daemons"][0]["busy"], false, "a count left by a dead daemon isn't a reply being written");
    assert_eq!(
        codes(&v),
        vec![("service_stopped".into(), None), ("mafold_signed_out".into(), Some("b".into()))],
        "{:#}",
        v["issues"]
    );
    assert_eq!(v["issues"][0]["bots"], serde_json::json!(["a:one", "a:two"]));
    assert_eq!(v["issues"][0]["since"], t - 60);
    let _ = std::fs::remove_dir_all(&home);
}

/// No login, no bot: a machine not set up yet has nothing to fix.
#[test]
fn a_fresh_machine_has_no_issues() {
    let home = temp_home("fresh");
    let v = status(&home);
    assert_version_one_shape(&v);
    assert_eq!(v["issues"], serde_json::json!([]));
    assert_eq!(v["logins"], serde_json::json!([]));
    assert_eq!(v["device_id"], Value::Null, "status never mints an id");
    assert!(!home.join(".mafold/device-id").exists());
    let _ = std::fs::remove_dir_all(&home);
}

/// The test process stands in for a running supervisor and a daemon writing
/// a reply. PATH holds no runtime at all: the runtimes come from the
/// supervisor's own report (`runtimes.json`), as on a machine where the
/// caller's PATH isn't the bots'.
#[test]
fn a_running_supervisor_says_since_when_and_what_each_bot_is_doing() {
    let home = temp_home("running");
    let t = now();
    let me = std::process::id();
    let (gone, _held) = dead_pid(&home);
    two_logins(&home, t);
    // Configured with no runtime (Claude Code); a:two was told by Mafold to run Codex.
    put(
        &home,
        "daemons.json",
        r#"{"daemons":[{"name":"a:one","token":"mb_1","workdir":"/tmp"},
                       {"name":"a:two","token":"mb_2","workdir":"/tmp"},
                       {"name":"a:three","token":"mb_3","workdir":"/tmp"}]}"#,
    );
    // a:one keeps dying; a:two is writing a reply; a:three isn't running.
    put(&home, "daemons/a_one/pid", &format!("{gone}\n"));
    put(&home, "daemons/a_one/deaths", &format!("{}\n{}\n{}\n", t - 300, t - 200, t - 100));
    put(&home, "daemons/a_two/pid", &format!("{me}\n"));
    put(&home, "daemons/a_two/busy", "1");
    put(
        &home,
        "daemons/a_two/turns.json",
        &serde_json::json!({ "pid": me, "active": 1, "since": t - 120, "runtime": "codex" }).to_string(),
    );
    let seat = |state: &str| serde_json::json!({ "state": state, "limits": [], "checked_at": t - 20 });
    put(
        &home,
        "runtimes.json",
        &serde_json::json!({ "pid": me, "at": t - 20, "rows": [
            { "id": "claude-code", "available": true, "version": "2.1.0", "health": seat("ok"), "accounts": [] },
            { "id": "codex", "available": true, "version": "0.50.0", "health": seat("unauthenticated"), "accounts": [] },
        ]})
        .to_string(),
    );
    put(&home, "supervisor.pid", &format!("{me}\n"));
    put(
        &home,
        "supervisor.run.json",
        &serde_json::json!({ "pid": me, "started_at": t - 3600, "version": "0.9.149", "auto_update": true }).to_string(),
    );
    put(
        &home,
        "supervisor.exit.json",
        &serde_json::json!({ "pid": 1, "reason": "update", "at": t - 3601, "version": "0.9.148", "detail": "0.9.149" })
            .to_string(),
    );
    put(
        &home,
        "update.json",
        &serde_json::json!({ "checked_at": t - 30, "offered": "99.0.0",
            "failed": { "version": "99.0.0", "since": t - 900, "at": t - 100, "streak": 3, "error": "x" } })
        .to_string(),
    );

    let v = status(&home);
    assert_version_one_shape(&v);
    let s = &v["supervisor"];
    assert_eq!((s["running"].clone(), s["pid"].clone()), (Value::Bool(true), me.into()));
    assert_eq!(s["started_at"], t - 3600);
    assert_eq!(s["version"], "0.9.149");
    assert_eq!(s["auto_update"], true);
    assert_eq!(s["last_exit"]["reason"], "update");
    assert_eq!(s["last_exit"]["detail"], "0.9.149");

    let two = &v["daemons"][1];
    assert_eq!((two["running"].clone(), two["pid"].clone(), two["busy"].clone()), (Value::Bool(true), me.into(), Value::Bool(true)));
    assert_eq!((two["turns"].clone(), two["turn_since"].clone()), (1.into(), (t - 120).into()));
    assert_eq!((two["harness"].clone(), two["runtime"].clone()), ("claude-code".into(), "codex".into()), "configured vs what it runs");
    assert!(two["started_at"].is_i64());
    let log = Path::new(two["log"].as_str().unwrap()).to_path_buf();
    assert!(log.ends_with(Path::new("daemons").join("a_two").join("log")), "{log:?}");
    // The supervisor's report, not a probe with the empty PATH here.
    assert_eq!(v["harnesses"].as_array().map(Vec::len), Some(2));
    assert_eq!(v["harnesses"][1]["available"], true);

    let logins = v["logins"].as_array().unwrap();
    assert_eq!((logins[0]["username"].clone(), logins[0]["current"].clone(), logins[0]["state"].clone()), ("a".into(), true.into(), "offline".into()));
    assert_eq!((logins[1]["state"].clone(), logins[1]["signed_out_at"].clone()), ("signed_out".into(), (t - 800).into()));

    assert_eq!(v["update"]["available"], "99.0.0");
    assert_eq!(v["update"]["failed"]["streak"], 3);
    assert_eq!(
        codes(&v),
        vec![
            ("mafold_signed_out".into(), Some("b".into())),
            ("heartbeat_stale".into(), None),
            ("runtime_signed_out".into(), Some("codex".into())),
            ("bot_crash_loop".into(), Some("a:one".into())),
            ("bot_down".into(), Some("a:three".into())),
            ("update_failing".into(), Some("99.0.0".into())),
        ],
        "{:#}",
        v["issues"]
    );
    assert_eq!(v["issues"][1]["accounts"], serde_json::json!(["a"]));
    assert_eq!(v["issues"][2]["bots"], serde_json::json!(["a:two"]), "only the bot that runs Codex");
    let _ = std::fs::remove_dir_all(&home);
}
