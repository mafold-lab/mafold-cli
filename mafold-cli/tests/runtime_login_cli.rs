//! `mafold runtime login claude-code` end to end — the real binary in a
//! throwaway HOME, a stand-in `claude` on PATH (activity center C11). The same
//! sign-in a bot's `/login` runs in chat, without a chat: the link handed out,
//! the Authentication Code read back from stdin.
//!
//! Linux only: on macOS the machine's own login lives in the keychain, not
//! under HOME, so a throwaway HOME would still read the developer's.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LINK: &str = "https://claude.ai/oauth/authorize?code=true&client_id=test";

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-rtlogin-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("make temp PATH");
    // A stand-in `claude`: `auth login` prints the link, reads the code and
    // takes only `good-code`; `auth status` says nobody is signed in. Every
    // call is written down.
    let claude = dir.join("bin/claude");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\necho \"$*\" >> \"$HOME/claude-calls\"\ncase \"$2\" in\n  status) echo 'Not logged in'; exit 0 ;;\nesac\necho \"Browser didn't open? Use the url below to sign in: {LINK}\"\nread code\n[ \"$code\" = good-code ]\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// Run it, answer the link with `code` (None: close stdin instead), and
/// collect what it printed: every JSON event, and its exit code.
fn sign_in(home: &Path, args: &[&str], code: Option<&str>) -> (Vec<Value>, Option<i32>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mafold"))
        .args(["runtime", "login", "claude-code", "--json"])
        .args(args)
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", home.join("bin").display()))
        .env_remove("MAFOLD_ACCOUNT")
        .env_remove("MAFOLD_BOT_TOKEN")
        .env_remove("MAFOLD_DAEMON_NAME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run mafold runtime login");
    let mut stdin = child.stdin.take();
    let mut events = Vec::new();
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let v: Value = serde_json::from_str(&line.unwrap()).expect("one JSON object per line");
        if v["event"] == "link" {
            match code {
                Some(c) => {
                    let mut si = stdin.take().unwrap();
                    writeln!(si, "{c}").unwrap();
                }
                None => drop(stdin.take()),
            }
        }
        events.push(v);
    }
    (events, child.wait().unwrap().code())
}

#[test]
fn the_link_goes_out_the_code_comes_back_and_it_signs_in() {
    let home = temp_home("own");
    let (ev, code) = sign_in(&home, &[], Some("good-code"));
    assert_eq!(code, Some(0), "{ev:?}");
    assert_eq!(ev[0], serde_json::json!({ "event": "started", "account": "default", "adding": false }));
    assert_eq!(ev[1], serde_json::json!({ "event": "link", "url": LINK }));
    let done = &ev[2];
    assert_eq!(done["event"], "done");
    assert_eq!(done["account"], "default");
    assert_eq!(done["added"], false);
    // Nobody signed in before; who it is now can't be read without a real
    // credential, so `claude auth status` is said instead.
    assert_eq!(done["before"]["state"], "signed_out");
    assert_eq!(done["status"], "Not logged in");
    assert_eq!(ev.len(), 3);
    // `claude` ran as `auth login --claudeai`, with no browser of its own.
    let calls = std::fs::read_to_string(home.join("claude-calls")).unwrap();
    assert!(calls.lines().any(|l| l == "auth login --claudeai"), "{calls}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_named_login_is_added_next_to_the_machines_own() {
    let home = temp_home("named");
    let (ev, code) = sign_in(&home, &["--name", "Work", "--console"], Some("good-code"));
    assert_eq!(code, Some(0), "{ev:?}");
    assert_eq!(ev[0], serde_json::json!({ "event": "started", "account": "work", "adding": true }));
    assert_eq!(ev[2]["added"], true);
    assert!(ev[2]["logins"].as_array().unwrap().iter().any(|n| n == "work"), "{}", ev[2]);
    let calls = std::fs::read_to_string(home.join("claude-calls")).unwrap();
    assert!(calls.lines().any(|l| l == "auth login --console"), "{calls}");
    // Signed in again under the same name: not added a second time.
    let (ev, _) = sign_in(&home, &["--name", "work"], Some("good-code"));
    assert_eq!(ev[0]["adding"], false);
    assert_eq!(ev[2]["added"], false);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_code_claude_refuses_fails_and_says_so() {
    let home = temp_home("bad");
    let (ev, code) = sign_in(&home, &[], Some("wrong"));
    assert_eq!(code, Some(1));
    assert_eq!(ev.last().unwrap()["event"], "failed");
    assert_eq!(ev.last().unwrap()["code"], "rejected");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn nobody_answering_cancels_it() {
    let home = temp_home("eof");
    let (ev, code) = sign_in(&home, &[], None);
    assert_eq!(code, Some(1));
    assert_eq!(ev.last().unwrap()["code"], "cancelled", "{ev:?}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_binary_that_signs_runtimes_in_says_so() {
    let home = temp_home("caps");
    let out = Command::new(env!("CARGO_BIN_EXE_mafold")).arg("capabilities").env("HOME", &home).output().unwrap();
    let caps: Vec<String> = serde_json::from_slice(&out.stdout).expect("capabilities");
    assert!(caps.iter().any(|c| c == "runtime-login"), "{caps:?}");
    let _ = std::fs::remove_dir_all(&home);
}
