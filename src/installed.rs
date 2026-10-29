//! The per-turn "which mini-apps are installed here" block.
//!
//! A compact list of the apps installed in THIS conversation and, for each app
//! that keeps its data in its own backend, the methods the agent can call with
//! `mafold app call`. Injected into the bot's prompt so it knows what is in
//! play. GENERIC — it reflects whatever is installed and whatever each app
//! declares, with zero per-app knowledge. `None` when nothing is installed (no
//! prompt overhead).
//!
//! (It used to list each app's shared CRDT room too, for `mafold room`; the room
//! was retired on 2026-10-29, `.docs/card-state-v1.md` §3.8.)

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value as J;

use crate::client::Client;

/// The installed-apps block for `conv`, or `None` when nothing is installed.
pub async fn context_block(client: &Client, conv: &str) -> Result<Option<String>> {
    // Fresh enough → answer without touching the network. `None` is cached the
    // same way: a conversation with no apps is the common case and deserves the
    // same skip, not a round trip that re-learns "still nothing" every turn.
    {
        let cache = installs_cache().lock().unwrap();
        if let Some((at, block)) = cache.get(conv) {
            if at.elapsed() < INSTALLS_TTL {
                return Ok(block.clone());
            }
        }
    }
    let resp = match client.list_installs(conv).await {
        Ok(r) => r,
        Err(e) => {
            // Holding a previous answer we can do better than silence: the last
            // known list beats no list, and the staleness is bounded by how long
            // the API has been unreachable — precisely the window in which
            // nobody is installing anything either.
            if let Some((_, block)) = installs_cache().lock().unwrap().get(conv) {
                return Ok(block.clone());
            }
            return Err(e);
        }
    };
    let block = render_installs(&resp);
    installs_cache()
        .lock()
        .unwrap()
        .insert(conv.to_string(), (Instant::now(), block.clone()));
    Ok(block)
}

/// How long a fetched block stays good. The list changes only when someone
/// installs or removes a mini-app — rare next to "every turn" — while the fetch
/// sits on the turn's critical path, ahead of the draft the bot opens to say it
/// is alive. A minute surfaces a freshly installed app almost at once and still
/// takes the round trip off nearly every turn.
const INSTALLS_TTL: Duration = Duration::from_secs(60);

/// Per-conversation cache for `context_block`. Only the prompt block — read by
/// the next turn, not by a command — trades freshness for latency.
static INSTALLS: OnceLock<Mutex<HashMap<String, (Instant, Option<String>)>>> = OnceLock::new();

fn installs_cache() -> &'static Mutex<HashMap<String, (Instant, Option<String>)>> {
    INSTALLS.get_or_init(Default::default)
}

/// `listInstalls` response → the prompt block, or `None` when nothing is
/// installed. Pure, so `context_block` has exactly one place that caches.
fn render_installs(resp: &J) -> Option<String> {
    let items = match resp.get("items").and_then(|i| i.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return None,
    };
    let mut apps: Vec<String> = Vec::new();
    let mut any_methods = false;
    for it in items {
        let id = it.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let manifest = it.get("manifest");
        let name = manifest
            .and_then(|m| m.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        // What its own backend lets you do — one line per method.
        let methods: Vec<String> = manifest
            .and_then(|m| m.get("methods"))
            .and_then(|m| m.as_object())
            .map(|m| {
                m.iter()
                    .map(|(n, spec)| format!("    {n} — {}", spec.get("description").and_then(|d| d.as_str()).unwrap_or("")))
                    .collect()
            })
            .unwrap_or_default();
        if methods.is_empty() {
            apps.push(format!("• {name} ({id})"));
        } else {
            any_methods = true;
            apps.push(format!("• {name} ({id}) — methods:\n{}", methods.join("\n")));
        }
    }
    if apps.is_empty() {
        return None;
    }
    let mut s = String::from("[AVAILABLE APPS — mini-apps installed in THIS conversation.]\n");
    for a in &apps {
        s.push_str(a);
        s.push('\n');
    }
    if any_methods {
        s.push_str(
            "An app that lists methods keeps its data in its OWN backend — act on it with `mafold app \
call <app> <method> --params '<json>'` (you call as yourself, from this conversation, and the app \
decides what you may do; `mafold app methods <app> --schema` shows the parameters).\n",
        );
    }
    s.push_str("[END AVAILABLE APPS]");
    Some(s)
}

/// The `mafold-room` skill an older daemon installed into its own plugin
/// (`~/.mafold/plugins/mafold/skills/mafold-room`) taught a command that no
/// longer exists. Removed on startup; the folder is ours, so it goes whole.
pub fn remove_room_skill() {
    let dir = crate::drive::mafold_plugin_dir().join("skills").join("mafold-room");
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lists_apps_and_their_methods_and_nothing_about_rooms() {
        let resp = json!({ "items": [
            { "id": "mafold/linear", "manifest": { "name": "Linear", "room": { "issue:*": "write" },
              "methods": { "board.list": { "description": "Every issue on the board" } } } },
            { "id": "opsdu/xiangqi", "manifest": { "name": "象棋", "room": { "board": "write" } } },
            { "id": "" },
        ]});
        let block = render_installs(&resp).unwrap();
        assert!(block.starts_with("[AVAILABLE APPS"), "{block}");
        assert!(block.ends_with("[END AVAILABLE APPS]"), "{block}");
        assert!(block.contains("• Linear (mafold/linear) — methods:\n    board.list — Every issue on the board"), "{block}");
        assert!(block.contains("• 象棋 (opsdu/xiangqi)\n"), "{block}");
        assert!(block.contains("mafold app call"), "{block}");
        assert!(!block.contains("room"), "the retired room is not taught: {block}");
        assert!(render_installs(&json!({ "items": [] })).is_none());
        assert!(render_installs(&json!({ "items": [{ "id": "" }] })).is_none());
    }

    /// The apps block sits in front of the draft the bot opens to say it is
    /// alive, so the round trip behind it is paid before the user sees anything.
    /// It may be paid ONCE per conversation per TTL, never once per turn.
    #[tokio::test]
    async fn the_apps_block_is_fetched_once_then_served_from_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Counted rather than "serve one then stop listening": a refused second
        // connection would fall into the stale-on-error path and return the same
        // block, so the assertion has to be on round trips, not on the answer.
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len: usize = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(str::to_string)
                            })
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        if request.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                seen.fetch_add(1, Ordering::SeqCst);
                let reply = r#"{"ok":true,"result":{"items":[{"id":"mafold/standup","manifest":{"name":"Standup"}}]}}"#;
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{reply}",
                            reply.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });

        let client = Client::new(format!("http://{addr}"), "dev:test".into());
        // Keyed per conversation, and the cache is process-global — so this test
        // owns a conv id nothing else uses.
        let conv = "conv-for-the-installs-cache-test";

        let first = context_block(&client, conv).await.unwrap().unwrap();
        assert!(first.contains("Standup"), "app name is rendered: {first}");

        let second = context_block(&client, conv).await.unwrap().unwrap();
        assert_eq!(first, second, "cached answer is byte-identical");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "second call must not re-ask the server — that round trip is what used to delay every turn's generating card",
        );
    }
}
