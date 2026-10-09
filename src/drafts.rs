//! Locally owned drafts and durable completion delivery. Presence is never a
//! verdict on a turn: only its producer (or recovery of that producer) closes it.
//!
//! A draft whose producer died mid-turn is normally recovered by finalizing the
//! partial transcript the server already holds. That is what every cli update
//! used to leave behind: the supervisor waits five minutes for running turns,
//! then restarts the daemons anyway, and a coding agent's turn routinely runs
//! longer than that — on 2026-10-06 one update froze seven replies at once,
//! each ending on «edited 9 files, ran 7 shell commands» and nothing after it.
//! So a turn that has a [`Journal`] is instead handed back to the daemon that
//! starts next, which picks it up in the same bubble ([`Outbox::take_resumable`]).

use std::{collections::HashMap, path::PathBuf, sync::Mutex, time::SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::Client;

/// A turn killed longer ago than this is finalized as it stands rather than
/// picked back up. Restarts the journal exists for — an update, a token swap,
/// a crash, a reboot — come back within minutes; a laptop that was shut for the
/// night should not wake up and carry on with work from yesterday. Same window
/// the api gives a restart to backfill missed mentions (`since_at`).
const RESUME_WITHIN: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// A turn that has already been picked back up this many times is finalized
/// instead: something in it is taking the daemon down, and resuming it again
/// would turn one crash into a crash loop.
const MAX_RESUMES: u32 = 2;

/// Everything needed to run a turn again from where it was cut off, written by
/// the turn itself as it learns each part.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Journal {
    pub chat: String,
    pub channel: Option<String>,
    pub thread: Option<String>,
    /// The incoming message the turn answers — to settle the reply against it
    /// when it finishes (`botFinalize`'s `success_for`; the draft was billed
    /// when it was opened). None for a turn nobody's message started: an
    /// introduction or a background-task wrap-up, which is never picked back
    /// up — each is armed again at startup from its own records, and picking
    /// it up as well would say the same thing twice.
    pub trigger: Option<String>,
    /// Lowercased handle of whoever the turn is for (`TurnHandle::owner`).
    pub sender: String,
    /// The person another bot's message was relaying, when `sender` is that
    /// bot (`agent::trusted_turn`) — a turn picked back up is held to the same.
    pub relayed_for: Option<String>,
    pub pays: bool,
    pub workdir: String,
    pub workdir_ns: bool,
    /// The harness running it (`Harness::id`): its session ids mean nothing to
    /// another one, and the bot may have been switched across the restart.
    pub harness: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub thinking: Option<u32>,
    pub system: Option<String>,
    pub account: Option<String>,
    /// The session the turn started from.
    pub prior: Option<String>,
    /// The prompt exactly as the harness got it. None until it has been built;
    /// a turn killed before that never asked the model anything.
    pub prompt: Option<String>,
    /// The session this turn's work is being written into, as soon as the
    /// harness names it. Continuing needs it: the session the turn STARTED
    /// from knows nothing of what it did since.
    pub session: Option<String>,
    /// The harness names its session before it shows anything of its own
    /// (`Harness::names_session_first`) — so whatever reached the bubble
    /// before that name ([`Self::shown`]) is the daemon's narration, not work.
    pub session_first: bool,
    /// The agent's own work reached the bubble — content that came after the
    /// harness named its session, so that session holds it.
    pub produced: bool,
    /// Something reached the bubble with no session named yet. For a harness
    /// that names its session first that is the daemon's own narration (the
    /// seat note); for one that names it only when the turn ends it may be the
    /// agent's work, and such a turn is not started over under it.
    pub shown: bool,
    /// The harness process working on the turn when it was last journaled.
    /// While that process is still alive the turn is not picked back up: two
    /// processes writing one session, in one working tree, is the one outcome
    /// worse than a reply cut short.
    pub child: Option<u32>,
    /// When the turn began, so the generating card's clock runs on instead of
    /// starting over.
    pub started_ms: u64,
    /// How many times this turn has already been picked back up.
    pub resumes: u32,
    /// The once-only thing this turn's reply IS, when it is one — an
    /// introduction ledger key (`boot`). Rides with the draft from the moment
    /// the turn starts, so whichever process ends up delivering the reply —
    /// this one, or the next one finishing it from here — also records it as
    /// done ([`Outbox::take_settled`]). Otherwise a daemon killed between the
    /// reply and the ledger write had its report delivered by the next start
    /// AND reported again by it (2026-10-06).
    pub settles: Option<String>,
}

/// What a dead turn's draft gets at the next start.
#[derive(Debug, PartialEq)]
enum Verdict {
    /// Run it again in the same bubble.
    Resume,
    /// Finalize the partial transcript as it stands — the recovery every draft
    /// got before there was a journal.
    Finalize,
    /// Throw the draft away: the turn died before it asked the model anything,
    /// so the bubble holds nothing but a generating card, and the message it
    /// answers is still journaled in `crate::pending`, which replays it.
    Discard,
}

fn verdict(turn: Option<&Journal>, at: u64, now: SystemTime) -> Verdict {
    let Some(t) = turn else { return Verdict::Finalize };
    if t.prompt.is_none() && !t.produced {
        return Verdict::Discard;
    }
    let beat = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(at);
    let stale = now.duration_since(beat).unwrap_or_default() > RESUME_WITHIN;
    // A turn with work on screen can only go on in the session that holds what
    // it did; without that id, starting it over would redo the work under a
    // bubble that already shows half of it.
    let continuable = if t.produced { t.session.is_some() } else { t.session_first || !t.shown };
    if stale || t.resumes >= MAX_RESUMES || t.prompt.is_none() || t.trigger.is_none() || !continuable {
        return Verdict::Finalize;
    }
    Verdict::Resume
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    pid: u32,
    ready: bool,
    /// None means finalize the server's existing partial transcript. Once the
    /// final snapshot is acknowledged, clear it so retries cannot overwrite a
    /// later live-card edit to the delivered message.
    content: Option<String>,
    #[serde(default)]
    success_for: Option<String>,
    /// How the turn ended when it didn't end cleanly (`botFinalize`'s `outcome`).
    /// A word this build doesn't know (a later cli's, before a rollback) reads
    /// as nothing said — never as an unreadable entry whose draft nobody finishes.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_outcome")]
    outcome: Option<crate::client::TurnEnd>,
    /// The live turn behind this draft, while it runs ([`Journal`]).
    #[serde(default)]
    turn: Option<Journal>,
    /// Last sign of life from the producer, epoch seconds — what tells a turn
    /// killed a minute ago from one killed last night.
    #[serde(default)]
    at: u64,
    /// Recovery throws the draft away instead of finalizing it ([`Verdict::Discard`]).
    #[serde(default)]
    discard: bool,
    /// [`Journal::settles`], kept once the turn is finished and its journal gone.
    #[serde(default)]
    settles: Option<String>,
    #[serde(skip)]
    delivering: bool,
}

impl Entry {
    /// [`Journal::settles`], from the journal while the turn runs, from the
    /// entry once it is finished.
    fn settles(&self) -> Option<String> {
        self.settles.clone().or_else(|| self.turn.as_ref().and_then(|t| t.settles.clone()))
    }
}

fn lenient_outcome<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<crate::client::TurnEnd>, D::Error> {
    Ok(Option::<serde_json::Value>::deserialize(d)?.and_then(|v| serde_json::from_value(v).ok()))
}

pub struct Outbox {
    dir: PathBuf,
    entries: Mutex<HashMap<String, Entry>>,
    /// Dead turns this process is to pick back up — taken once, at startup.
    resumable: Mutex<Vec<(String, Journal)>>,
    /// What the dead turns this process is now delivering settle — taken once,
    /// at startup, before anything decides whether it still needs saying.
    settled: Mutex<Vec<String>>,
}

impl Outbox {
    pub fn open(base: &str, username: &str) -> Result<Self> {
        let scope = format!("{base}\n{username}");
        let key = format!("{:x}", Sha256::digest(scope.as_bytes()));
        let dir = PathBuf::from(std::env::var("HOME").context("HOME is unset")?)
            .join(".mafold/drafts")
            .join(key);
        Self::load(dir, crate::platform::pid_alive)
    }

    fn load(dir: PathBuf, alive: impl Fn(u32) -> bool) -> Result<Self> {
        Self::load_at(dir, alive, SystemTime::now())
    }

    fn load_at(dir: PathBuf, alive: impl Fn(u32) -> bool, now: SystemTime) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut entries = HashMap::new();
        let mut resumable = Vec::new();
        let mut settled = Vec::new();
        for file in std::fs::read_dir(&dir)? {
            let path = file?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if uuid::Uuid::parse_str(id).is_err() {
                continue;
            }
            let mut entry: Entry = match std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|s| Ok(serde_json::from_slice(&s)?))
            {
                Ok(entry) => entry,
                Err(e) => {
                    eprintln!("draft journal {} unreadable: {e:#}", path.display());
                    continue;
                }
            };
            // Another daemon on this machine may own the same account. Its
            // drafts stay with it; never sweep a username's remote history.
            // An exec-based self-update keeps our PID. At startup no current
            // turns exist yet, so entries carrying our own PID are recoverable.
            if entry.pid != std::process::id() && alive(entry.pid) {
                continue;
            }
            entry.pid = std::process::id();
            // A finished turn whose delivery didn't get through is just
            // delivered. A turn that was still running is the one to judge.
            if !entry.ready {
                match verdict(entry.turn.as_ref(), entry.at, now) {
                    Verdict::Resume => {
                        // Ours again, live: nothing finalizes it while the
                        // picked-up turn runs, and if THAT one dies too, the
                        // count it carries is what stops the loop.
                        let turn = entry.turn.as_mut().expect("Resume needs a journal");
                        turn.resumes += 1;
                        let turn = turn.clone();
                        entry.at = now
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        resumable.push((id.to_string(), turn));
                        entries.insert(id.to_string(), entry);
                        continue;
                    }
                    Verdict::Discard => entry.discard = true,
                    // Cut off, not finished — what the server's own boot sweep
                    // says of a hosted reply a restart interrupted.
                    Verdict::Finalize => entry.outcome = Some(crate::client::TurnEnd::Failed),
                }
            }
            // Delivered from here, or finalized as it stands: either way it has
            // been said. Thrown away, it hasn't — whatever it settles is still
            // owed. (A turn that settles something is never picked back up —
            // nobody's message started it — so Resume above never carries one.)
            if !entry.discard {
                settled.extend(entry.settles());
            }
            entry.ready = true;
            entries.insert(id.to_string(), entry);
        }
        let outbox = Self {
            dir,
            entries: Mutex::new(entries),
            resumable: Mutex::new(resumable),
            settled: Mutex::new(settled),
        };
        for (id, entry) in outbox.entries.lock().unwrap().iter() {
            outbox.persist(id, entry)?;
        }
        Ok(outbox)
    }

    fn persist(&self, id: &str, entry: &Entry) -> Result<()> {
        uuid::Uuid::parse_str(id)?;
        let path = self.dir.join(format!("{id}.json"));
        let tmp = self.dir.join(format!("{id}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&tmp)?;
        serde_json::to_writer(&file, entry)?;
        file.sync_all()?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn track(&self, id: &str) -> Result<()> {
        let entry = Entry {
            pid: std::process::id(),
            ready: false,
            content: None,
            success_for: None,
            outcome: None,
            turn: None,
            at: now_secs(),
            discard: false,
            settles: None,
            delivering: false,
        };
        let mut entries = self.entries.lock().unwrap();
        self.persist(id, &entry)?;
        entries.insert(id.to_string(), entry);
        Ok(())
    }

    /// Attach the running turn to its draft, so a restart can pick it back up.
    /// Best-effort like the rest of the journal: a turn whose journal could not
    /// be written is finalized as it stands, which is what every turn got before.
    pub fn journal(&self, id: &str, turn: Journal) {
        self.note(id, |t| {
            *t = Some(turn);
            true
        });
    }

    /// Update the running turn's journal. `f` says whether it changed anything,
    /// so a turn reporting the same session twice costs no disk write.
    pub fn note(&self, id: &str, f: impl FnOnce(&mut Option<Journal>) -> bool) {
        let mut entries = self.entries.lock().unwrap();
        let Some(entry) = entries.get_mut(id) else { return };
        if entry.ready || !f(&mut entry.turn) {
            return;
        }
        entry.at = now_secs();
        if let Err(e) = self.persist(id, entry) {
            eprintln!("could not journal turn {id} for restart recovery: {e:#}");
        }
    }

    /// A steer moved the reply into a fresh draft: its turn goes with it.
    pub fn carry(&self, from: &str, to: &str) {
        let turn = self.entries.lock().unwrap().get(from).and_then(|e| e.turn.clone());
        if let Some(turn) = turn {
            self.journal(to, turn);
        }
    }

    /// Every live turn of this process says it is still alive. Called on the
    /// retry tick, so a restart can tell how long ago the producer died.
    pub fn beat(&self) {
        let now = now_secs();
        let mut entries = self.entries.lock().unwrap();
        for (id, entry) in entries.iter_mut() {
            if entry.ready || entry.turn.is_none() || entry.pid != std::process::id() {
                continue;
            }
            entry.at = now;
            let _ = self.persist(id, entry);
        }
    }

    /// What the dead turns this process now owns settle ([`Journal::settles`]):
    /// each is being delivered from here or finalized as it stands, so it has
    /// been said. Taken once, at startup, by the daemon — which records them
    /// before it decides whether anything still needs saying.
    pub fn take_settled(&self) -> Vec<String> {
        std::mem::take(&mut *self.settled.lock().unwrap())
    }

    /// The dead turns this process is to pick back up, each with its draft id.
    /// Taken once: a second call returns nothing.
    pub fn take_resumable(&self) -> Vec<(String, Journal)> {
        std::mem::take(&mut *self.resumable.lock().unwrap())
    }

    /// A turn handed out by [`Self::take_resumable`] could not be started: its
    /// draft goes back to plain recovery — the partial transcript, finalized.
    pub fn give_up(&self, id: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(id) {
            entry.ready = true;
            entry.outcome = Some(crate::client::TurnEnd::Failed); // cut off, not finished
            let _ = self.persist(id, entry);
        }
    }

    pub fn complete(
        &self,
        id: &str,
        content: &str,
        success_for: Option<&str>,
        outcome: Option<crate::client::TurnEnd>,
    ) -> Result<()> {
        let mut entries = self.entries.lock().unwrap();
        // The journal goes; what the reply settles stays with it.
        let settles = entries.get(id).and_then(Entry::settles);
        let entry = Entry {
            pid: std::process::id(),
            ready: true,
            content: Some(content.into()),
            success_for: success_for.map(str::to_string),
            outcome,
            turn: None,
            at: now_secs(),
            discard: false,
            settles,
            delivering: false,
        };
        // Keep an in-memory retry even if the disk fills up; report the loss of
        // restart durability to the caller instead of claiming persistence.
        let saved = self.persist(id, &entry);
        entries.insert(id.to_string(), entry);
        saved
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        let mut entries = self.entries.lock().unwrap();
        match std::fs::remove_file(self.dir.join(format!("{id}.json"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        entries.remove(id);
        Ok(())
    }

    pub async fn deliver(&self, client: &Client, id: &str) -> Result<bool> {
        let (content, success_for, outcome, discard) = {
            let mut entries = self.entries.lock().unwrap();
            let Some(entry) = entries.get_mut(id) else {
                return Ok(true);
            };
            if !entry.ready || entry.delivering {
                return Ok(false);
            }
            entry.delivering = true;
            (entry.content.clone(), entry.success_for.clone(), entry.outcome, entry.discard)
        };
        let result = async {
            if discard {
                // Forgets the entry itself once the server agrees. A refusal
                // means there is no draft left to throw away — finalized, or
                // gone — and asking again every 30 seconds won't change that.
                match client.discard_draft(id).await {
                    Err(e) if matches!(e.downcast_ref::<mafold_core::RpcError>(), Some(mafold_core::RpcError::Api(_))) => {
                        self.forget(id)?
                    }
                    r => r?,
                }
                return Ok(true);
            }
            if let Some(content) = content {
                client.edit_draft(id, &content).await?;
                let mut entries = self.entries.lock().unwrap();
                if let Some(entry) = entries.get_mut(id) {
                    entry.content = None;
                    self.persist(id, entry)?;
                }
            }
            client.finalize(id, success_for.as_deref(), outcome).await?;
            self.forget(id)?;
            Ok(true)
        }
        .await;
        if let Some(entry) = self.entries.lock().unwrap().get_mut(id) {
            entry.delivering = false;
        }
        result
    }

    pub async fn retry(&self, client: &Client) {
        let ids: Vec<String> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| e.ready && !e.delivering)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            match self.deliver(client, &id).await {
                Ok(true) => println!("→ recovered draft completion {id}"),
                Ok(false) => {}
                Err(e) => eprintln!("draft {id} completion still pending: {e:#}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch journal directory of this test's OWN — per test and per
    /// process, because these tests end by deleting the directory they used and
    /// cargo runs them side by side. (It used to borrow `session::device_id`,
    /// which minted a fresh random id on every call and gave isolation by
    /// accident; the id is now the stable one this machine reports, which is
    /// what a device id ought to be — and would have handed all four tests the
    /// same directory.)
    fn dir(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mafold-drafts-test-{}-{test}", std::process::id()))
    }

    /// 2026-10-06: the first-boot report was written (or half-written) when its
    /// daemon was killed. The next start delivered it from here — and, finding
    /// no mark in the intro ledger, reported again. What a recovered reply
    /// settles is handed over, before anything decides whether it still needs
    /// saying; a reply that never said anything still owes it.
    #[test]
    fn a_recovered_reply_hands_over_what_it_settles() {
        let dir = dir("settles");
        let old = Outbox::load(dir.clone(), |_| false).unwrap();
        let done = "00000000-0000-0000-0000-000000000001";
        let cut = "00000000-0000-0000-0000-000000000002";
        let silent = "00000000-0000-0000-0000-000000000003";
        let plain = "00000000-0000-0000-0000-000000000004";
        for id in [done, cut, silent, plain] {
            old.track(id).unwrap();
        }
        let settles = |key: &str| Some(key.to_string());
        // Finished, never delivered.
        old.journal(done, Journal { settles: settles("boot"), prompt: Some("p".into()), ..Default::default() });
        old.complete(done, "INTRO-OK", None, None).unwrap();
        // Cut off with words on screen: finalized as it stands.
        old.journal(cut, Journal { settles: settles("cut"), prompt: Some("p".into()), produced: true, ..Default::default() });
        // Killed before it asked the model anything: thrown away, still owed.
        old.journal(silent, Journal { settles: settles("silent"), ..Default::default() });
        // An ordinary reply settles nothing.
        old.journal(plain, Journal { prompt: Some("p".into()), ..Default::default() });
        for (id, entry) in old.entries.lock().unwrap().iter_mut() {
            entry.pid = std::process::id() + 1;
            old.persist(id, entry).unwrap();
        }
        let next = Outbox::load(dir.clone(), |_| false).unwrap();
        let mut settled = next.take_settled();
        settled.sort();
        assert_eq!(settled, vec!["boot".to_string(), "cut".to_string()]);
        assert!(next.take_settled().is_empty(), "taken once");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_recovers_only_owned_drafts_from_dead_processes() {
        let dir = dir("restart-owned");
        let old = Outbox::load(dir.clone(), |_| false).unwrap();
        let a = "00000000-0000-0000-0000-000000000001";
        let b = "00000000-0000-0000-0000-000000000002";
        let c = "00000000-0000-0000-0000-000000000003";
        old.track(a).unwrap();
        old.track(b).unwrap();
        old.track(c).unwrap();
        old.complete(b, "the final answer", Some(a), None).unwrap();
        old.complete(c, "⏹ Stopped.", None, Some(crate::client::TurnEnd::Stopped)).unwrap();
        // Simulate a different live daemon; this test process's own PID is
        // deliberately recoverable across an exec-based update.
        for (id, entry) in old.entries.lock().unwrap().iter_mut() {
            entry.pid = std::process::id() + 1;
            old.persist(id, entry).unwrap();
        }
        assert!(Outbox::load(dir.clone(), |_| true)
            .unwrap()
            .entries
            .lock()
            .unwrap()
            .is_empty());
        let recovered = Outbox::load(dir.clone(), |_| false).unwrap();
        let entries = recovered.entries.lock().unwrap();
        assert!(entries[a].ready);
        assert!(entries[a].content.is_none());
        assert!(entries[a].success_for.is_none(), "recovered partial output is not success");
        assert_eq!(entries[b].content.as_deref(), Some("the final answer"));
        assert_eq!(entries[b].success_for.as_deref(), Some(a));
        assert_eq!(entries[a].outcome, Some(crate::client::TurnEnd::Failed), "a turn the crash cut off didn't finish");
        assert_eq!(entries[b].outcome, None);
        assert_eq!(entries[c].outcome, Some(crate::client::TurnEnd::Stopped), "how it ended goes out with the retry");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exec_restart_recovers_entries_with_the_same_pid() {
        let dir = dir("exec-restart");
        let old = Outbox::load(dir.clone(), |_| false).unwrap();
        let id = "00000000-0000-0000-0000-000000000005";
        old.track(id).unwrap();
        let recovered = Outbox::load(dir.clone(), |_| true).unwrap();
        assert!(recovered.entries.lock().unwrap()[id].ready);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_delivery_retains_final_snapshot_and_ignores_active_drafts() {
        let dir = dir("failed-delivery");
        let outbox = Outbox::load(dir.clone(), |_| false).unwrap();
        let id = "00000000-0000-0000-0000-000000000003";
        let client = Client::new("http://127.0.0.1:1".into(), "dev:test".into());
        outbox.track(id).unwrap();
        outbox.deliver(&client, id).await.unwrap(); // active: no network call
        outbox
            .complete(id, "complete text, no footer needed", None, None)
            .unwrap();
        assert!(outbox.deliver(&client, id).await.is_err());
        assert!(!outbox.entries.lock().unwrap()[id].delivering);
        let recovered = Outbox::load(dir.clone(), |_| false).unwrap();
        assert_eq!(
            recovered.entries.lock().unwrap()[id].content.as_deref(),
            Some("complete text, no footer needed")
        );
        outbox.forget(id).unwrap();
        assert!(Outbox::load(dir.clone(), |_| false)
            .unwrap()
            .entries
            .lock()
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn journal(prompt: Option<&str>, session: Option<&str>, produced: bool) -> Journal {
        Journal {
            chat: "c".into(),
            trigger: Some("m".into()),
            prompt: prompt.map(str::to_string),
            session: session.map(str::to_string),
            produced,
            ..Default::default()
        }
    }

    /// Kill the process that owns `dir`'s entries: same files, a dead pid.
    fn die(outbox: &Outbox) {
        for (id, entry) in outbox.entries.lock().unwrap().iter_mut() {
            entry.pid = std::process::id() + 1;
            outbox.persist(id, entry).unwrap();
        }
    }

    /// The incident this exists for: a turn mid-work when the update restarted
    /// its daemon comes back to the next process to run — not finalized as it
    /// stood — and only for as long as resuming it isn't the thing killing it.
    #[test]
    fn a_turn_killed_mid_work_is_picked_back_up_not_finalized() {
        let dir = dir("resume");
        let id = "00000000-0000-0000-0000-000000000010";
        let now = SystemTime::now();
        let old = Outbox::load_at(dir.clone(), |_| false, now).unwrap();
        old.track(id).unwrap();
        old.journal(id, journal(Some("fix the bug"), Some("s-live"), true));
        die(&old);

        let next = Outbox::load_at(dir.clone(), |_| false, now).unwrap();
        assert!(!next.entries.lock().unwrap()[id].ready, "still a live turn: nothing may finalize it");
        let taken = next.take_resumable();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, id);
        assert_eq!(taken[0].1.session.as_deref(), Some("s-live"));
        assert_eq!(taken[0].1.prompt.as_deref(), Some("fix the bug"));
        assert_eq!(taken[0].1.resumes, 1);
        assert!(next.take_resumable().is_empty(), "handed out once");

        // It dies again while picked up: one more try…
        die(&next);
        let third = Outbox::load_at(dir.clone(), |_| false, now).unwrap();
        assert_eq!(third.take_resumable()[0].1.resumes, 2);
        // …and the next death finalizes it, so a turn that kills its daemon
        // can't loop.
        die(&third);
        let fourth = Outbox::load_at(dir.clone(), |_| false, now).unwrap();
        assert!(fourth.take_resumable().is_empty());
        assert!(fourth.entries.lock().unwrap()[id].ready);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verdicts() {
        let now = SystemTime::now();
        let at = now_secs();
        let long_ago = at - RESUME_WITHIN.as_secs() - 60;
        // Nothing on screen yet: run the prompt again on the session it started from.
        assert_eq!(verdict(Some(&journal(Some("p"), None, false)), at, now), Verdict::Resume);
        // On screen, with the session that holds the work: continue it.
        assert_eq!(verdict(Some(&journal(Some("p"), Some("s"), true)), at, now), Verdict::Resume);
        // On screen with no session named (a harness that names its session
        // only at the end): starting over could redo visible work.
        let shown = Journal { shown: true, ..journal(Some("p"), None, false) };
        assert_eq!(verdict(Some(&shown), at, now), Verdict::Finalize);
        // …but from a harness that names its session first, whatever is on
        // screen before that name is the daemon's own narration: run it again.
        let narrated = Journal { session_first: true, ..shown.clone() };
        assert_eq!(verdict(Some(&narrated), at, now), Verdict::Resume);
        // Picked up twice already and killed again: something in the turn is
        // what takes the daemon down.
        let looping = Journal { resumes: MAX_RESUMES, ..journal(Some("p"), Some("s"), true) };
        assert_eq!(verdict(Some(&looping), at, now), Verdict::Finalize);
        // Nobody's message started it — an introduction, a background-task
        // wrap-up: those are armed again at startup on their own.
        let unasked = Journal { trigger: None, ..journal(Some("p"), Some("s"), true) };
        assert_eq!(verdict(Some(&unasked), at, now), Verdict::Finalize);
        // Died before it asked the model anything: the bubble is empty and the
        // trigger replays from `pending`.
        assert_eq!(verdict(Some(&journal(None, None, false)), at, now), Verdict::Discard);
        // Too long ago.
        assert_eq!(verdict(Some(&journal(Some("p"), Some("s"), true)), long_ago, now), Verdict::Finalize);
        // No journal: what every draft got before.
        assert_eq!(verdict(None, at, now), Verdict::Finalize);
    }

    /// An entry written by a daemon from before the journal has none of its
    /// fields — it is recovered exactly the way that daemon expected.
    #[test]
    fn an_entry_from_an_older_daemon_is_finalized_as_before() {
        let dir = dir("older");
        std::fs::create_dir_all(&dir).unwrap();
        let id = "00000000-0000-0000-0000-000000000011";
        std::fs::write(
            dir.join(format!("{id}.json")),
            br#"{"pid":1,"ready":false,"content":null,"success_for":null}"#,
        )
        .unwrap();
        let o = Outbox::load_at(dir.clone(), |_| false, SystemTime::now()).unwrap();
        assert!(o.take_resumable().is_empty());
        let e = &o.entries.lock().unwrap()[id];
        assert!(e.ready && !e.discard && e.content.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An entry from a later daemon (rolled back past it) whose `outcome` is a
    /// word this build doesn't know is still delivered — saying nothing about
    /// how the turn ended, never left unreadable with its draft unfinished.
    #[test]
    fn an_outcome_this_build_doesnt_know_is_nothing_said() {
        let dir = dir("later");
        std::fs::create_dir_all(&dir).unwrap();
        let id = "00000000-0000-0000-0000-000000000014";
        std::fs::write(
            dir.join(format!("{id}.json")),
            br#"{"pid":1,"ready":true,"content":"done","success_for":null,"outcome":"superseded"}"#,
        )
        .unwrap();
        let o = Outbox::load_at(dir.clone(), |_| false, SystemTime::now()).unwrap();
        let e = &o.entries.lock().unwrap()[id];
        assert!(e.ready && e.content.as_deref() == Some("done") && e.outcome.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A steer moves the reply to a fresh draft; the turn's journal moves with
    /// it, or a restart after the steer would have nothing to pick up.
    #[test]
    fn a_steer_carries_the_journal_to_the_new_draft() {
        let dir = dir("carry");
        let (a, b) = ("00000000-0000-0000-0000-000000000012", "00000000-0000-0000-0000-000000000013");
        let o = Outbox::load(dir.clone(), |_| false).unwrap();
        o.track(a).unwrap();
        o.journal(a, journal(Some("p"), Some("s"), true));
        o.track(b).unwrap();
        o.carry(a, b);
        o.forget(a).unwrap();
        die(&o);
        let taken = Outbox::load(dir.clone(), |_| false).unwrap().take_resumable();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, b);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A finished turn's journal is not a reason to run it again: completing
    /// drops it, and a restart only delivers.
    #[test]
    fn a_completed_turn_is_delivered_not_resumed() {
        let dir = dir("completed");
        let id = "00000000-0000-0000-0000-000000000014";
        let o = Outbox::load(dir.clone(), |_| false).unwrap();
        o.track(id).unwrap();
        o.journal(id, journal(Some("p"), Some("s"), true));
        o.complete(id, "the answer", None, None).unwrap();
        die(&o);
        let next = Outbox::load(dir.clone(), |_| false).unwrap();
        assert!(next.take_resumable().is_empty());
        assert_eq!(next.entries.lock().unwrap()[id].content.as_deref(), Some("the answer"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn retry_after_snapshot_ack_only_finalizes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(
            format!("http://{}", listener.local_addr().unwrap()),
            "dev:test".into(),
        );
        let server = tokio::spawn(async move {
            let mut paths = Vec::new();
            for attempt in 0..3 {
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
                let request = String::from_utf8(request).unwrap();
                paths.push(
                    request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_string(),
                );
                // First finalize fails after the body was acknowledged. The
                // recovered entry must never re-send that body on its retry.
                let reply = if attempt == 1 {
                    r#"{"ok":false,"error_code":503,"description":"unavailable"}"#
                } else {
                    r#"{"ok":true,"result":{"ok":true}}"#
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
            }
            paths
        });
        let dir = dir("retry-after-ack");
        let id = "00000000-0000-0000-0000-000000000004";
        let outbox = Outbox::load(dir.clone(), |_| false).unwrap();
        outbox.track(id).unwrap();
        outbox.complete(id, "full final answer", None, None).unwrap();
        assert!(outbox.deliver(&client, id).await.is_err());
        let recovered = Outbox::load(dir.clone(), |_| false).unwrap();
        assert!(recovered.entries.lock().unwrap()[id].content.is_none());
        assert!(recovered.deliver(&client, id).await.unwrap());
        assert!(recovered.entries.lock().unwrap().is_empty());
        assert_eq!(
            server.await.unwrap(),
            ["/api/botEditDraft", "/api/botFinalize", "/api/botFinalize"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
