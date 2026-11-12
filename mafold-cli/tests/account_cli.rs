//! `mafold account` end to end, against the real binary and a real
//! ~/.mafold/session.json — in a throwaway HOME.
//!
//! The unit tests in `session.rs` cover the state machine; these cover the
//! wiring nothing else does: clap actually routing the subcommand, the file
//! landing at the path the rest of the cli reads, `--account` refusing a name
//! this machine has never logged in, and `rm` refusing to forget a session it
//! could not revoke. Every case runs the binary as a CHILD with its own `HOME`,
//! so the suite can't reach the developer's own session no matter how it's run,
//! and parallel tests can't fight over one process-global env var.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Nothing is listening here, so any command that reaches for the network
/// fails fast instead of hanging — or, worse, talking to production.
const DEAD_API: &str = "http://127.0.0.1:1";

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-acct-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".mafold")).expect("make temp home");
    dir
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mafold"))
        .env("HOME", home)
        // The env of whoever runs `cargo test` must not steer the child, and
        // MAFOLD_BASE especially must not point it at the real api.
        .env("MAFOLD_BASE", DEAD_API)
        .env_remove("MAFOLD_ACCOUNT")
        .env_remove("MAFOLD_BOT_TOKEN")
        .args(args)
        .output()
        .expect("run mafold")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn write_session(home: &Path, json: &str) {
    std::fs::write(home.join(".mafold/session.json"), json).expect("seed session.json");
}

const TWO: &str = r#"{"token":"s_a","username":"a","device_id":"d1","device_name":"mac",
    "accounts":[{"username":"a","token":"s_a"},{"username":"b","token":"s_b"}]}"#;
const ONE: &str = r#"{"token":"s_a","username":"opsdu","device_id":"d1","device_name":"mac"}"#;

/// The file written before multi-account existed must list as one account,
/// with no migration command anyone has to run first.
#[test]
fn lists_a_pre_multi_account_file() {
    let home = temp_home("legacy");
    write_session(&home, ONE);
    let out = run(&home, &["account"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s = stdout(&out);
    assert!(s.contains("@opsdu"), "{s}");
    assert!(s.contains('*'), "the only account is the current one: {s}");
}

/// No login at all is a signpost, not an error — `mafold account` is the thing
/// you run when you don't know where you stand.
#[test]
fn says_what_to_do_when_nothing_is_logged_in() {
    let home = temp_home("empty");
    let out = run(&home, &["account"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("mafold login"), "{}", stdout(&out));
}

/// Listing and switching must work with no network at all — they are what you
/// reach for when something is already broken.
#[test]
fn list_and_use_work_offline() {
    let home = temp_home("switch");
    write_session(&home, TWO);

    let out = run(&home, &["account", "use", "b"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout(&out).contains("@b"), "{}", stdout(&out));

    // The star must have moved in the FILE, not just in that one message.
    let listed = stdout(&run(&home, &["account"]));
    let starred = listed
        .lines()
        .find(|l| l.trim_start().starts_with('*'))
        .unwrap_or_default()
        .to_string();
    assert!(starred.contains("@b"), "current should be @b: {listed}");
}

/// THE invariant behind revoke-on-rm: if the session can't be revoked, nothing
/// is forgotten. The stored token is the only thing that can kill its own
/// session, so dropping it on a failed revoke would strand a live session
/// nobody here can reach again.
#[test]
fn rm_that_cannot_revoke_forgets_nothing() {
    let home = temp_home("revoke-fail");
    write_session(&home, TWO);

    let out = run(&home, &["account", "rm", "b"]);
    assert!(!out.status.success(), "unreachable api must fail the command");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--local"), "must name the escape hatch: {err}");

    // Still here, and still exactly two.
    let listed = stdout(&run(&home, &["account"]));
    assert!(listed.contains("@b"), "nothing forgotten: {listed}");
    assert!(listed.contains("@a"), "{listed}");
}

/// `--local` is the offline escape hatch: forget it here, and SAY that the
/// session is still alive rather than implying a clean sign-out.
#[test]
fn rm_local_forgets_without_the_network_and_says_so() {
    let home = temp_home("local");
    write_session(&home, TWO);

    let out = run(&home, &["account", "rm", "b", "--local"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s = stdout(&out);
    assert!(s.contains("ALIVE"), "must not imply it was revoked: {s}");
    assert!(!stdout(&run(&home, &["account"])).contains("@b"));
}

/// Forgetting the current account promotes the survivor; forgetting the last
/// one leaves the machine logged-out rather than holding a nameless session.
#[test]
fn rm_promotes_then_clears_the_machine() {
    let home = temp_home("last");
    write_session(&home, TWO);

    assert!(run(&home, &["account", "rm", "a", "--local"]).status.success());
    assert!(stdout(&run(&home, &["account"])).contains("@b"), "survivor is current");

    assert!(run(&home, &["account", "rm", "b", "--local"]).status.success());
    assert!(!home.join(".mafold/session.json").exists(), "the file goes with it");
    assert!(stdout(&run(&home, &["account"])).contains("mafold login"));
}

/// `--account` naming someone this machine has never logged in must FAIL, and
/// say who is here. Falling back to whoever is current would run as the wrong
/// person — which looks like it worked until it writes something.
#[test]
fn unknown_account_flag_fails_loudly() {
    let home = temp_home("unknown");
    write_session(&home, ONE);
    let out = run(&home, &["--account", "ghost", "report"]);
    assert!(!out.status.success(), "must not fall back to @opsdu");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ghost"), "{err}");
    assert!(err.contains("@opsdu"), "names who IS here: {err}");
}

/// …but naming a known account is accepted — the guard is about typos, not
/// about making the flag hard to use.
#[test]
fn known_account_flag_is_accepted() {
    let home = temp_home("known");
    write_session(&home, TWO);
    let out = run(&home, &["--account", "b", "account", "rm", "b", "--local"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!stdout(&run(&home, &["account"])).contains("@b"));
}
