//! Triggers a turn was started for that have not opened their reply draft yet —
//! the part of "handled" the event cursor cannot vouch for.
//!
//! The cursor is pinned the moment a message frame is read (`agent.rs`, the
//! 2026-08-11 replay storms), so a turn that never got its draft open used to
//! be simply gone: on 2026-10-01 the 08:00 routine reached the daemon in a
//! 28-second dark wake of a sleeping laptop, the draft could not be opened, the
//! notice about it could not be sent either, and nothing ever replayed it.
//!
//! An entry is written before the turn's task starts and removed once its draft
//! exists (from then on the drafts outbox owns recovery) or the task ends
//! without needing one. Whatever is left when a connection opens belongs to no
//! running task: a trigger whose process died is replayed, one that has waited
//! past [`LINK_WAIT`] is owed the lost-turn notice instead.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::client::{Client, Dest, LINK_WAIT};

/// Entries a task in THIS process is still working on. A connection opening
/// mid-wait must not mistake them for orphans and start a second turn.
static LIVE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The turn's notice could not be delivered either: keep the entry so the next
/// connection sends it. Carried as context on `handle()`'s error.
#[derive(Debug)]
pub struct NoticeOwed;

impl std::fmt::Display for NoticeOwed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the lost-turn notice is still owed")
    }
}

/// One journaled trigger, and where its answer (or notice) belongs.
pub struct Entry {
    path: PathBuf,
    chat_id: String,
    channel_id: Option<String>,
    thread_root_id: Option<String>,
}

/// What a connection finds when it opens.
#[derive(Default)]
pub struct Orphans {
    /// Frames to run through the message arms again, `seq` removed so the
    /// cursor step does not drop them as already seen.
    pub replay: Vec<Value>,
    /// Waited past [`LINK_WAIT`]: too stale to answer, owed a notice.
    pub owed: Vec<Entry>,
}

/// Removes its entry when dropped — every way a task can end without a turn
/// (a quiet floor seat, a slash command, a steer) — unless [`Guard::keep`].
/// A killed process drops nothing, which is the point.
pub struct Guard {
    path: PathBuf,
    kept: bool,
}

impl Guard {
    pub fn keep(&mut self) {
        self.kept = true;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        live_remove(&self.path);
        if !self.kept {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn dir(bot: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let tag: String = bot
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    PathBuf::from(home).join(".mafold").join("pending-turns").join(tag)
}

fn entry_path(dir: &Path, trigger_id: &str) -> PathBuf {
    let safe: String = trigger_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    dir.join(format!("{safe}.json"))
}

fn live_remove(path: &Path) {
    let mut live = LIVE.lock().unwrap();
    if let Some(i) = live.iter().position(|p| p == path) {
        live.swap_remove(i);
    }
}

/// Journal `frame` as a trigger about to be handled.
pub fn record(
    bot: &str,
    trigger_id: &str,
    chat_id: &str,
    channel_id: Option<&str>,
    thread_root_id: Option<&str>,
    frame: &Value,
) -> Guard {
    record_in(&dir(bot), trigger_id, chat_id, channel_id, thread_root_id, frame, SystemTime::now())
}

fn record_in(
    dir: &Path,
    trigger_id: &str,
    chat_id: &str,
    channel_id: Option<&str>,
    thread_root_id: Option<&str>,
    frame: &Value,
    now: SystemTime,
) -> Guard {
    let path = entry_path(dir, trigger_id);
    let body = json!({
        "at": now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "chat_id": chat_id,
        "channel_id": channel_id,
        "thread_root_id": thread_root_id,
        "frame": frame,
    });
    // Best effort: a journal that cannot be written costs exactly what the
    // daemon had before it — the turn still runs, it just isn't replayable.
    let written = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&path, serde_json::to_vec(&body).unwrap_or_default()));
    if let Err(e) = written {
        eprintln!("⚠ pending: could not journal trigger {trigger_id}: {e}");
    }
    LIVE.lock().unwrap().push(path.clone());
    Guard { path, kept: false }
}

/// The draft exists — from here a restart is the drafts outbox's to recover,
/// and replaying the trigger would start a second turn.
pub fn clear(bot: &str, trigger_id: &str) {
    let _ = std::fs::remove_file(entry_path(&dir(bot), trigger_id));
}

/// Collect what no running task owns. Replayed entries are removed here: the
/// frame journals itself again if it still becomes a turn.
pub fn orphans(bot: &str) -> Orphans {
    orphans_in(&dir(bot), SystemTime::now(), LINK_WAIT)
}

fn orphans_in(dir: &Path, now: SystemTime, max_age: Duration) -> Orphans {
    let mut out = Orphans::default();
    let live = LIVE.lock().unwrap().clone();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !live.contains(p))
        .collect();
    files.sort();
    let mut fresh: Vec<(u64, Value)> = Vec::new();
    for path in files {
        let Some(v) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
            let _ = std::fs::remove_file(&path); // corrupt → unrecoverable
            continue;
        };
        let at = v["at"].as_u64().unwrap_or(0);
        let age = now
            .duration_since(UNIX_EPOCH + Duration::from_secs(at))
            .unwrap_or(Duration::ZERO);
        if age < max_age {
            let mut frame = v["frame"].clone();
            if let Some(o) = frame.as_object_mut() {
                o.remove("seq");
                o.remove("prev");
            }
            fresh.push((at, frame));
            let _ = std::fs::remove_file(&path);
        } else {
            let s = |k: &str| v[k].as_str().map(str::to_string);
            out.owed.push(Entry {
                chat_id: s("chat_id").unwrap_or_default(),
                channel_id: s("channel_id"),
                thread_root_id: s("thread_root_id"),
                path,
            });
        }
    }
    // In the order they were said, not the order their ids sort in.
    fresh.sort_by_key(|(at, _)| *at);
    out.replay = fresh.into_iter().map(|(_, f)| f).collect();
    out
}

/// Tell the sender an owed trigger was never processed, then drop it. Left on
/// disk if the link is still down — the next connection tries again.
pub async fn settle(client: &Client, e: Entry) -> Result<()> {
    if e.chat_id.is_empty() {
        let _ = std::fs::remove_file(&e.path);
        return Ok(());
    }
    LIVE.lock().unwrap().push(e.path.clone());
    let dest = Dest::chat(&e.chat_id)
        .channel(e.channel_id.as_deref())
        .thread(e.thread_root_id.as_deref());
    let sent = client
        .announce_lost_turn(dest, "this machine could not reach Mafold for hours after it arrived")
        .await;
    live_remove(&e.path);
    if sent.is_ok() {
        let _ = std::fs::remove_file(&e.path);
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mafold-pending-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn frame(id: &str, seq: u64) -> Value {
        json!({ "method": "events.messageNew", "seq": seq, "prev": seq - 1, "params": { "message": { "id": id } } })
    }

    fn files(d: &Path) -> usize {
        std::fs::read_dir(d).map(|r| r.count()).unwrap_or(0)
    }

    /// The process died while the trigger waited: the next connection runs it
    /// through the arms again, without the seq that would get it dropped as
    /// already consumed.
    #[test]
    fn a_trigger_whose_process_died_is_replayed_without_its_seq() {
        let d = tmp("replay");
        let now = SystemTime::now();
        let g = record_in(&d, "m1", "c1", None, None, &frame("m1", 7), now - Duration::from_secs(60));
        std::mem::forget(g); // a killed process runs no destructors
        live_remove(&entry_path(&d, "m1"));
        let o = orphans_in(&d, now, LINK_WAIT);
        assert_eq!(o.replay.len(), 1);
        assert!(o.owed.is_empty());
        assert_eq!(o.replay[0]["params"]["message"]["id"], "m1");
        assert!(o.replay[0].get("seq").is_none(), "a seq at or below the cursor is dropped as a duplicate");
        assert_eq!(files(&d), 0, "taken for replay; it journals itself again if it becomes a turn");
    }

    /// A trigger a task in this process is still waiting on is not an orphan —
    /// replaying it on reconnect would start a second turn for one message.
    #[test]
    fn a_trigger_still_being_waited_on_is_left_alone() {
        let d = tmp("live");
        let now = SystemTime::now();
        let _g = record_in(&d, "m2", "c1", None, None, &frame("m2", 8), now);
        let o = orphans_in(&d, now, LINK_WAIT);
        assert!(o.replay.is_empty() && o.owed.is_empty());
        assert_eq!(files(&d), 1);
    }

    /// Past the wait it is too stale to answer: owed a notice, and kept until
    /// the notice is delivered.
    #[test]
    fn a_trigger_older_than_the_wait_is_owed_a_notice_not_a_turn() {
        let d = tmp("owed");
        let now = SystemTime::now();
        let mut g = record_in(&d, "m3", "c9", Some("ch"), Some("root"), &frame("m3", 9), now - LINK_WAIT - Duration::from_secs(1));
        g.keep();
        drop(g);
        let o = orphans_in(&d, now, LINK_WAIT);
        assert!(o.replay.is_empty());
        assert_eq!(o.owed.len(), 1);
        assert_eq!(o.owed[0].chat_id, "c9");
        assert_eq!(o.owed[0].channel_id.as_deref(), Some("ch"));
        assert_eq!(o.owed[0].thread_root_id.as_deref(), Some("root"));
        assert_eq!(files(&d), 1, "kept until the notice gets through");
    }

    /// Every way a task ends without a turn removes its entry; `keep` is the
    /// one way to leave it for the next connection.
    #[test]
    fn a_finished_task_leaves_nothing_behind_unless_it_kept_it() {
        let d = tmp("drop");
        let now = SystemTime::now();
        drop(record_in(&d, "m4", "c1", None, None, &frame("m4", 10), now));
        assert_eq!(files(&d), 0);
        let mut g = record_in(&d, "m5", "c1", None, None, &frame("m5", 11), now);
        g.keep();
        drop(g);
        assert_eq!(files(&d), 1);
        assert_eq!(orphans_in(&d, now, LINK_WAIT).replay.len(), 1, "no longer live once its task ended");
    }

    /// Replayed in the order they were said.
    #[test]
    fn orphans_replay_oldest_first() {
        let d = tmp("order");
        let now = SystemTime::now();
        for (id, ago) in [("zz", 300), ("aa", 100), ("mm", 200)] {
            let g = record_in(&d, id, "c1", None, None, &frame(id, 5), now - Duration::from_secs(ago));
            std::mem::forget(g);
            live_remove(&entry_path(&d, id));
        }
        let ids: Vec<String> = orphans_in(&d, now, LINK_WAIT)
            .replay
            .iter()
            .map(|f| f["params"]["message"]["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["zz", "mm", "aa"]);
    }

    #[test]
    fn a_corrupt_entry_is_dropped() {
        let d = tmp("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("bad.json"), b"{nope").unwrap();
        let o = orphans_in(&d, SystemTime::now(), LINK_WAIT);
        assert!(o.replay.is_empty() && o.owed.is_empty());
        assert_eq!(files(&d), 0);
    }
}
