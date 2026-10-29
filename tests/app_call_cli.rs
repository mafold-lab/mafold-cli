//! `mafold app call` end to end — the real binary against a stub api — for the
//! rule it shares with `read`, `channels` and `tap` (#1186): a turn outside the
//! owner's circle calls an app only from a room its people could open
//! themselves. Until room was retired, `mafold room --conv` kept that rule for
//! apps' data; `app call --conv` is how an agent reaches an app's data now, and
//! the token it mints names the room — linear answers with that room's board.
//! So the owner's DM with the agent must not be one `--conv` away from a
//! stranger in some group.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// `chat::EXIT_REFUSED`: what a turn hears when a room is not its to open.
const REFUSED: i32 = 4;

/// The agent's chat list: the owner's DM with it, and a group alice is in.
const CHATS: &str = r#"{"ok":true,"result":{"items":[
  {"id":"dm-owner","participants":[{"username":"opsdu"},{"username":"opsdu:claude-code"}]},
  {"id":"group-alice","participants":[{"username":"opsdu"},{"username":"alice"},{"username":"opsdu:claude-code"}]}
]}}"#;

/// A stub api on a free port: `getChats` answers [`CHATS`], everything else is
/// refused — so a call the guard lets through ends at `getAppMethods` (exit 1).
/// Returns its base URL and the path of every request it got.
fn stub() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Headers, then exactly Content-Length bytes of body.
            let path = loop {
                match sock.read(&mut chunk) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let Some(end) = text.find("\r\n\r\n") else { continue };
                let want: usize = text[..end]
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                    .unwrap_or(0);
                if buf.len() < end + 4 + want {
                    continue;
                }
                break text.lines().next().and_then(|l| l.split(' ').nth(1)).map(str::to_string);
            };
            let Some(path) = path else { continue };
            log.lock().unwrap().push(path.clone());
            let reply = if path.ends_with("/getChats") { CHATS } else { r#"{"ok":false,"description":"stub: not here"}"# };
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (base, seen)
}

/// `mafold app call acme/board list` in a turn for `reads_for` (None: a person
/// at their own terminal), happening in room `here`, with `conv` as `--conv`.
fn call(base: &str, reads_for: Option<&str>, here: Option<&str>, conv: Option<&str>) -> Output {
    static N: AtomicUsize = AtomicUsize::new(0);
    let home = std::env::temp_dir().join(format!(
        "mafold-app-call-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&home).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mafold"));
    cmd.env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("MAFOLD_BASE", base)
        .env("MAFOLD_BOT_TOKEN", "t")
        .env_remove("MAFOLD_ACCOUNT")
        .env_remove("MAFOLD_CHANNEL")
        .env_remove("MAFOLD_READS_FOR")
        .env_remove("MAFOLD_CONV")
        .args(["app", "call", "acme/board", "list"]);
    if let Some(r) = reads_for {
        cmd.env("MAFOLD_READS_FOR", r);
    }
    if let Some(h) = here {
        cmd.env("MAFOLD_CONV", h);
    }
    if let Some(c) = conv {
        cmd.args(["--conv", c]);
    }
    let out = cmd.output().unwrap();
    let _ = std::fs::remove_dir_all(&home);
    out
}

fn app_was_asked(seen: &Arc<Mutex<Vec<String>>>) -> usize {
    seen.lock().unwrap().iter().filter(|p| p.ends_with("/getAppMethods") || p.ends_with("/getAppLaunch")).count()
}

#[test]
fn a_strangers_turn_does_not_call_an_app_from_the_owners_dm() {
    let (base, seen) = stub();
    let out = call(&base, Some("alice"), Some("group-alice"), Some("dm-owner"));
    assert_eq!(out.status.code(), Some(REFUSED), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(app_was_asked(&seen), 0, "nothing about the app is asked: {:?}", seen.lock().unwrap());
}

#[test]
fn a_strangers_turn_without_a_room_does_not_call() {
    let (base, seen) = stub();
    let out = call(&base, Some("alice"), None, None);
    assert_eq!(out.status.code(), Some(REFUSED), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(seen.lock().unwrap().is_empty(), "nothing is asked at all: {:?}", seen.lock().unwrap());
}

/// The control for the two above: the same call, from rooms the turn may
/// open, gets past the guard to the app (which the stub then refuses: exit 1).
#[test]
fn a_turn_calls_from_rooms_its_people_can_open() {
    let (base, seen) = stub();
    let cases = [
        ("its own room", call(&base, Some("alice"), Some("group-alice"), None)),
        ("another room alice is in", call(&base, Some("alice"), Some("elsewhere"), Some("group-alice"))),
        ("the owner's circle", call(&base, Some("*"), Some("group-alice"), Some("dm-owner"))),
        ("a person at their own terminal", call(&base, None, None, Some("dm-owner"))),
    ];
    for (what, out) in &cases {
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{what}: {err}");
        assert!(err.contains("getAppMethods"), "{what} gets as far as the app: {err}");
    }
    assert_eq!(app_was_asked(&seen), cases.len(), "{:?}", seen.lock().unwrap());
}
