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
//! One that can't be picked up ends on the failure card saying it was cut off
//! ([`cut_off`]) — never left on its generating card.
//!
//! «Died» is asked of the producer itself, not of its pid: on 2026-11-11 an
//! update's restart started the next daemon while the one it had just stopped
//! was still going (a zombie answers `kill(pid, 0)` until it is reaped), the
//! next daemon took the reply for a live one's and never looked again, and the
//! reply showed «generating» for 12 hours. A producer still on its way out is
//! now taken over once it has gone ([`Outbox::take_over_dead`]).

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Mutex,
    time::SystemTime,
};

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
    /// [`Self::child`]'s start (`platform::start_token`), so a restart can
    /// tell that process from a later one handed the same pid before it ends
    /// one (`harness::opencode::end_orphan`).
    pub child_start: Option<u64>,
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
    /// Close it as cut off, and why it couldn't be picked back up: what it
    /// says so far, then the failure card ([`cut_off`]).
    Finalize(&'static str),
    /// Throw the draft away: the turn died before it asked the model anything,
    /// so the bubble holds nothing but a generating card, and the message it
    /// answers is still journaled in `crate::pending`, which replays it.
    Discard,
}

fn verdict(turn: Option<&Journal>, at: u64, now: SystemTime) -> Verdict {
    let Some(t) = turn else { return Verdict::Finalize("it was written by a mafold that couldn't pick replies back up") };
    if t.prompt.is_none() && !t.produced {
        return Verdict::Discard;
    }
    let beat = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(at);
    if now.duration_since(beat).unwrap_or_default() > RESUME_WITHIN {
        return Verdict::Finalize("it stopped more than 30 minutes before the agent came back");
    }
    if t.resumes >= MAX_RESUMES {
        return Verdict::Finalize("it had been picked back up twice already, and stopped again");
    }
    if t.prompt.is_none() {
        return Verdict::Finalize("it stopped before what it was asked was saved");
    }
    if t.trigger.is_none() {
        return Verdict::Finalize("no message started it (a background-task update or an introduction)");
    }
    // A turn with work on screen can only go on in the session that holds what
    // it did; without that id, starting it over would redo the work under a
    // bubble that already shows half of it.
    let continuable = if t.produced { t.session.is_some() } else { t.session_first || !t.shown };
    if !continuable {
        return Verdict::Finalize("the session holding its work wasn't known yet");
    }
    Verdict::Resume
}

/// Is the process that wrote an entry still running: alive, not a zombie, and
/// not a later process handed the same pid ([`Entry::pid_start`])? A daemon an
/// update has just stopped is a zombie until the supervisor reaps it, and
/// answers `kill(pid, 0)` all the same.
fn owner_running(pid: u32, start: Option<u64>) -> bool {
    crate::platform::pid_running(pid)
        && match (start, crate::platform::start_token(pid)) {
            (Some(was), Some(is)) => was == is,
            _ => true,
        }
}

/// This process's start ([`crate::platform::start_token`]), read once.
fn my_start() -> Option<u64> {
    static START: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *START.get_or_init(|| crate::platform::start_token(std::process::id()))
}

/// A reply cut off for good — its producer died and it couldn't be picked back
/// up (`why`) — as it is left: what it says, its live generating card gone, a
/// code block it left open closed, then the failure card that says it was cut
/// off. The card a server restart leaves on a hosted reply
/// (`close_interrupted_server_turns`), so one reads like the other. `reader`:
/// whom the card's details are for when the bot has no owner (a house bot).
pub(crate) fn cut_off(content: &str, why: &str, reader: Option<String>) -> (String, Option<mafold_transcript::render::PrivateItem>) {
    use mafold_transcript::{failure, render};
    // Built, not classified: `why` may be an error some other kind's words
    // match (a 502 on reading the reply back), and it was still cut off.
    let mut f = failure::Failure::of(failure::FailureKind::Interrupted, failure::agent_restarted(why));
    f.reader = reader;
    let (card, private) = render::failure_card_private(&f);
    let said = render::strip_trailing_generating(content);
    let closed = if said.trim().is_empty() {
        card.trim().to_string()
    } else {
        format!("{}\n\n{}", render::heal_open_code(said.trim_end()), card.trim())
    };
    (closed, private)
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
    /// [`Self::pid`]'s start ([`crate::platform::start_token`]): a pid handed
    /// to another process since — a reboot — doesn't pass for its producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid_start: Option<u64>,
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
    /// Cut off for good, and why: before it is finalized the reply gets the
    /// card that says so ([`cut_off`]), written into `content` on delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cut: Option<String>,
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

/// How a reply cut off for good is closed ([`Outbox::close_cut`]).
enum Closing {
    /// With what it says, and the card saying it was cut off.
    With(String),
    /// As it stands: it couldn't be read back.
    AsItStands,
    /// Not at all: it was finished meanwhile (a Stop from a client).
    Finished,
}

/// Is the process that wrote an entry — `(pid, its start)` — still running?
/// [`owner_running`]; a stand-in in tests.
type OwnerRunning = Box<dyn Fn(u32, Option<u64>) -> bool + Send + Sync>;

pub struct Outbox {
    dir: PathBuf,
    /// The bot these drafts are written as (`owner:label`, or a house bot's
    /// bare handle) — whom a cut-off reply's details are for ([`cut_off`]).
    bot: String,
    entries: Mutex<HashMap<String, Entry>>,
    /// Dead turns this process is to pick back up — taken at startup, and
    /// again whenever [`Self::take_over_dead`] took over another.
    resumable: Mutex<Vec<(String, Journal)>>,
    /// What the dead turns this process is now delivering settle — taken
    /// before anything decides whether it still needs saying.
    settled: Mutex<Vec<String>>,
    /// The processes the dead turns this process took over were running in
    /// (`Journal::child`, with its start).
    orphans: Mutex<Vec<(u32, Option<u64>)>>,
    owner_running: OwnerRunning,
    /// Entries already said to be unreadable — said once, not on every look.
    unreadable: Mutex<HashSet<String>>,
}

impl Outbox {
    pub fn open(base: &str, username: &str) -> Result<Self> {
        let scope = format!("{base}\n{username}");
        let key = format!("{:x}", Sha256::digest(scope.as_bytes()));
        let dir = PathBuf::from(std::env::var("HOME").context("HOME is unset")?)
            .join(".mafold/drafts")
            .join(key);
        let mut outbox = Self::load(dir, owner_running)?;
        outbox.bot = username.to_string();
        Ok(outbox)
    }

    fn load(dir: PathBuf, owner_running: impl Fn(u32, Option<u64>) -> bool + Send + Sync + 'static) -> Result<Self> {
        Self::load_at(dir, owner_running, SystemTime::now())
    }

    fn load_at(
        dir: PathBuf,
        owner_running: impl Fn(u32, Option<u64>) -> bool + Send + Sync + 'static,
        now: SystemTime,
    ) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let outbox = Self {
            dir,
            bot: String::new(),
            entries: Mutex::default(),
            resumable: Mutex::default(),
            settled: Mutex::default(),
            orphans: Mutex::default(),
            owner_running: Box::new(owner_running),
            unreadable: Mutex::default(),
        };
        outbox.take_over(now)?;
        Ok(outbox)
    }

    /// Take over the drafts of producers that have since gone — after
    /// startup's: one still on its way out when this process started (an
    /// update's restart doesn't wait for it), another daemon of this account
    /// that has died since. Each is judged as startup judges them; how many
    /// were taken over. Those to pick back up are in [`Self::take_resumable`].
    pub fn take_over_dead(&self) -> Result<usize> {
        self.take_over(SystemTime::now())
    }

    fn take_over(&self, now: SystemTime) -> Result<usize> {
        // Held throughout: a draft this process starts meanwhile is written
        // to disk under this lock (`track`), so it is either in the map
        // already or not on disk yet — never taken for a stranger's.
        let mut entries = self.entries.lock().unwrap();
        let mut taken = 0;
        for file in std::fs::read_dir(&self.dir)? {
            let path = file?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if uuid::Uuid::parse_str(id).is_err() || entries.contains_key(id) {
                continue;
            }
            let mut entry: Entry = match std::fs::read(&path)
                .map_err(anyhow::Error::from)
                .and_then(|s| Ok(serde_json::from_slice(&s)?))
            {
                Ok(entry) => entry,
                Err(e) => {
                    if self.unreadable.lock().unwrap().insert(id.to_string()) {
                        eprintln!("draft journal {} unreadable: {e:#}", path.display());
                    }
                    continue;
                }
            };
            // Another daemon on this machine may own the same account. Its
            // drafts stay with it while it runs; never sweep a username's
            // remote history. An exec-based self-update keeps our PID: no turn
            // of this process is missing from the map, so an entry carrying
            // our own PID is one the image before this one wrote.
            if entry.pid != std::process::id() && (self.owner_running)(entry.pid, entry.pid_start) {
                continue;
            }
            entry.pid = std::process::id();
            entry.pid_start = my_start();
            // The process it was running in may have outlived its daemon:
            // handed to the daemon to look at, however the turn is judged.
            if let Some(child) = entry.turn.as_ref().and_then(|t| t.child.map(|c| (c, t.child_start))) {
                self.orphans.lock().unwrap().push(child);
            }
            taken += 1;
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
                        self.resumable.lock().unwrap().push((id.to_string(), turn));
                        self.persist(id, &entry)?;
                        entries.insert(id.to_string(), entry);
                        continue;
                    }
                    Verdict::Discard => entry.discard = true,
                    // Cut off, not finished — what the server's own boot sweep
                    // says of a hosted reply a restart interrupted — and the
                    // reply says so.
                    Verdict::Finalize(why) => {
                        entry.outcome = Some(crate::client::TurnEnd::Failed);
                        entry.cut = Some(why.to_string());
                    }
                }
            }
            // Delivered from here, or finalized as it stands: either way it has
            // been said. Thrown away, it hasn't — whatever it settles is still
            // owed. (A turn that settles something is never picked back up —
            // nobody's message started it — so Resume above never carries one.)
            if !entry.discard {
                self.settled.lock().unwrap().extend(entry.settles());
            }
            entry.ready = true;
            self.persist(id, &entry)?;
            entries.insert(id.to_string(), entry);
        }
        Ok(taken)
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
            pid_start: my_start(),
            ready: false,
            content: None,
            success_for: None,
            outcome: None,
            turn: None,
            at: now_secs(),
            discard: false,
            settles: None,
            cut: None,
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

    /// The processes the dead turns this process took over were running in,
    /// each with its start — whether each turn is picked back up or finalized
    /// as it stands. Taken once, at startup, by the daemon: one of them may
    /// have outlived its daemon and still be at work on a reply nobody reads.
    pub fn take_orphans(&self) -> Vec<(u32, Option<u64>)> {
        std::mem::take(&mut *self.orphans.lock().unwrap())
    }

    /// The dead turns this process is to pick back up, each with its draft id.
    /// Taken once: a second call returns nothing.
    pub fn take_resumable(&self) -> Vec<(String, Journal)> {
        std::mem::take(&mut *self.resumable.lock().unwrap())
    }

    /// A turn handed out by [`Self::take_resumable`] could not be started
    /// (`why`): its draft is closed as cut off — the partial transcript, and
    /// the card that says so ([`cut_off`]).
    pub fn give_up(&self, id: &str, why: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(id) {
            entry.ready = true;
            entry.outcome = Some(crate::client::TurnEnd::Failed); // cut off, not finished
            entry.cut = Some(why.to_string());
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
            pid_start: my_start(),
            ready: true,
            // Masked now, while the turn still holds its live secrets (a
            // wallet reply's key): a delivery that fails is retried after the
            // turn has ended, when nothing masks it on the way out any more —
            // and this is also what goes to disk.
            content: Some(crate::conceal::conceal_live(content)),
            success_for: success_for.map(str::to_string),
            outcome,
            turn: None,
            at: now_secs(),
            discard: false,
            settles,
            cut: None,
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
        let (content, success_for, outcome, discard, cut) = {
            let mut entries = self.entries.lock().unwrap();
            let Some(entry) = entries.get_mut(id) else {
                return Ok(true);
            };
            if !entry.ready || entry.delivering {
                return Ok(false);
            }
            entry.delivering = true;
            // A house bot has no owner to read a failure's details: whoever
            // the turn was for does (as on the server's own cut-off replies).
            let reader = entry.cut.as_ref().filter(|_| !self.bot.contains(':')).and_then(|_| {
                entry.turn.as_ref().map(|t| t.sender.clone()).filter(|s| !s.is_empty())
            });
            (entry.content.clone(), entry.success_for.clone(), entry.outcome, entry.discard, entry.cut.clone().map(|why| (why, reader)))
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
            let content = match (content, cut) {
                (None, Some((why, reader))) => match self.close_cut(client, id, &why, reader).await? {
                    Closing::With(closed) => Some(closed),
                    Closing::AsItStands => None,
                    Closing::Finished => return Ok(true),
                },
                (content, _) => content,
            };
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

    /// What a reply cut off for good is closed with ([`cut_off`]), from what
    /// its bubble says now — kept as the entry's content before anything is
    /// sent, so a retry sends the same and never appends a second card.
    async fn close_cut(&self, client: &Client, id: &str, why: &str, reader: Option<String>) -> Result<Closing> {
        let msg = match client.get_message(id).await {
            Ok(msg) => msg,
            // An api that won't hand it back (one from before `getMessage`):
            // finalized as it stands, as every cut-off reply used to be.
            Err(e) if matches!(e.downcast_ref::<mafold_core::RpcError>(), Some(mafold_core::RpcError::Api(_))) => {
                eprintln!("draft {id}: couldn't read it back to say it was cut off ({e:#}) — finalizing it as it stands");
                self.set_closing(id, None)?;
                return Ok(Closing::AsItStands);
            }
            Err(e) => return Err(e),
        };
        if !msg["finalized_at"].is_null() {
            self.forget(id)?;
            return Ok(Closing::Finished);
        }
        let (closed, private) = cut_off(msg["content"].as_str().unwrap_or(""), why, reader);
        self.set_closing(id, Some(closed.clone()))?;
        // The details only the card's reader may open, as the reply's state
        // (`agent::write_private`): best-effort, like there.
        if let Some(item) = private {
            if let Err(e) = client.set_card_state(id, &item.key, &item.v, &item.read, &["sender".to_string()]).await {
                eprintln!("card state {} on {id} not written: {e:#}", item.key);
            }
        }
        Ok(Closing::With(closed))
    }

    /// What a cut-off entry was decided to be closed with — its card no longer
    /// to be written, whatever comes of sending it.
    fn set_closing(&self, id: &str, content: Option<String>) -> Result<()> {
        if let Some(entry) = self.entries.lock().unwrap().get_mut(id) {
            entry.content = content;
            entry.cut = None;
            self.persist(id, entry)?;
        }
        Ok(())
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
        let old = Outbox::load(dir.clone(), |_, _| false).unwrap();
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
        let next = Outbox::load(dir.clone(), |_, _| false).unwrap();
        let mut settled = next.take_settled();
        settled.sort();
        assert_eq!(settled, vec!["boot".to_string(), "cut".to_string()]);
        assert!(next.take_settled().is_empty(), "taken once");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The processes a dead daemon's turns were running in are handed to the
    /// next start, once — whichever way each turn is judged (picked back up,
    /// or finalized as it stands) — with the start that tells each from a
    /// later process given the same pid. Another live daemon's are its own.
    #[test]
    fn a_dead_processs_journaled_children_are_handed_over_once() {
        let dir = dir("orphans");
        let old = Outbox::load(dir.clone(), |_, _| false).unwrap();
        let resumed = "00000000-0000-0000-0000-000000000011";
        let cut = "00000000-0000-0000-0000-000000000012";
        let none = "00000000-0000-0000-0000-000000000013";
        let live = "00000000-0000-0000-0000-000000000014";
        for id in [resumed, cut, none, live] {
            old.track(id).unwrap();
        }
        let p = || Some("p".to_string());
        old.journal(resumed, Journal { prompt: p(), trigger: p(), session_first: true, child: Some(111), child_start: Some(7), ..Default::default() });
        old.journal(cut, Journal { prompt: p(), produced: true, child: Some(222), child_start: Some(8), ..Default::default() });
        old.journal(none, Journal { prompt: p(), ..Default::default() });
        old.journal(live, Journal { prompt: p(), child: Some(444), child_start: Some(9), ..Default::default() });
        let (dead_owner, live_owner) = (std::process::id() + 1, std::process::id() + 2);
        for (id, entry) in old.entries.lock().unwrap().iter_mut() {
            entry.pid = if id == live { live_owner } else { dead_owner };
            old.persist(id, entry).unwrap();
        }
        let next = Outbox::load(dir.clone(), move |pid, _| pid == live_owner).unwrap();
        assert_eq!(next.take_resumable().len(), 1, "one of them is picked back up");
        let mut orphans = next.take_orphans();
        orphans.sort();
        assert_eq!(orphans, vec![(111, Some(7)), (222, Some(8))]);
        assert!(next.take_orphans().is_empty(), "taken once");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A wallet reply's key is masked out of what a turn writes only while the
    /// turn holds it (`conceal::hide_while_live`). A completion whose delivery
    /// failed is retried by the outbox after the turn is over — so it is
    /// masked when it is queued, on disk as well as in memory, or the retry
    /// would post a key that is still live until the finalize lands.
    #[test]
    fn a_completion_queued_for_retry_keeps_a_live_key_masked_after_its_turn() {
        let dir = dir("live-key");
        let outbox = Outbox::load(dir.clone(), |_, _| false).unwrap();
        let id = "00000000-0000-0000-0000-00000000000a";
        outbox.track(id).unwrap();
        let key = format!("mrk_live_{}", uuid::Uuid::new_v4().simple());
        {
            let _live = crate::conceal::hide_while_live(&key);
            outbox.complete(id, &format!("ran env:\nMAFOLD_WALLET_KEY={key}\nPATH=/usr/bin"), None, None).unwrap();
        }
        let queued = outbox.entries.lock().unwrap()[id].content.clone().unwrap();
        assert!(!queued.contains(&key), "{queued}");
        assert!(queued.contains("MAFOLD_WALLET_KEY=") && queued.contains("PATH=/usr/bin"), "the rest is kept: {queued}");
        let on_disk = std::fs::read_to_string(dir.join(format!("{id}.json"))).unwrap();
        assert!(!on_disk.contains(&key), "nor on disk: {on_disk}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restart_recovers_only_owned_drafts_from_dead_processes() {
        let dir = dir("restart-owned");
        let old = Outbox::load(dir.clone(), |_, _| false).unwrap();
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
        assert!(Outbox::load(dir.clone(), |_, _| true)
            .unwrap()
            .entries
            .lock()
            .unwrap()
            .is_empty());
        let recovered = Outbox::load(dir.clone(), |_, _| false).unwrap();
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
        let old = Outbox::load(dir.clone(), |_, _| false).unwrap();
        let id = "00000000-0000-0000-0000-000000000005";
        old.track(id).unwrap();
        let recovered = Outbox::load(dir.clone(), |_, _| true).unwrap();
        assert!(recovered.entries.lock().unwrap()[id].ready);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_delivery_retains_final_snapshot_and_ignores_active_drafts() {
        let dir = dir("failed-delivery");
        let outbox = Outbox::load(dir.clone(), |_, _| false).unwrap();
        let id = "00000000-0000-0000-0000-000000000003";
        let client = Client::new("http://127.0.0.1:1".into(), "dev:test".into());
        outbox.track(id).unwrap();
        outbox.deliver(&client, id).await.unwrap(); // active: no network call
        outbox
            .complete(id, "complete text, no footer needed", None, None)
            .unwrap();
        assert!(outbox.deliver(&client, id).await.is_err());
        assert!(!outbox.entries.lock().unwrap()[id].delivering);
        let recovered = Outbox::load(dir.clone(), |_, _| false).unwrap();
        assert_eq!(
            recovered.entries.lock().unwrap()[id].content.as_deref(),
            Some("complete text, no footer needed")
        );
        outbox.forget(id).unwrap();
        assert!(Outbox::load(dir.clone(), |_, _| false)
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
        let old = Outbox::load_at(dir.clone(), |_, _| false, now).unwrap();
        old.track(id).unwrap();
        old.journal(id, journal(Some("fix the bug"), Some("s-live"), true));
        die(&old);

        let next = Outbox::load_at(dir.clone(), |_, _| false, now).unwrap();
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
        let third = Outbox::load_at(dir.clone(), |_, _| false, now).unwrap();
        assert_eq!(third.take_resumable()[0].1.resumes, 2);
        // …and the next death finalizes it, so a turn that kills its daemon
        // can't loop.
        die(&third);
        let fourth = Outbox::load_at(dir.clone(), |_, _| false, now).unwrap();
        assert!(fourth.take_resumable().is_empty());
        assert!(fourth.entries.lock().unwrap()[id].ready);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verdicts() {
        let now = SystemTime::now();
        let at = now_secs();
        let long_ago = at - RESUME_WITHIN.as_secs() - 60;
        let closed = |v: Verdict| matches!(v, Verdict::Finalize(_));
        // Nothing on screen yet: run the prompt again on the session it started from.
        assert_eq!(verdict(Some(&journal(Some("p"), None, false)), at, now), Verdict::Resume);
        // On screen, with the session that holds the work: continue it.
        assert_eq!(verdict(Some(&journal(Some("p"), Some("s"), true)), at, now), Verdict::Resume);
        // On screen with no session named (a harness that names its session
        // only at the end): starting over could redo visible work.
        let shown = Journal { shown: true, ..journal(Some("p"), None, false) };
        assert!(closed(verdict(Some(&shown), at, now)));
        // …but from a harness that names its session first, whatever is on
        // screen before that name is the daemon's own narration: run it again.
        let narrated = Journal { session_first: true, ..shown.clone() };
        assert_eq!(verdict(Some(&narrated), at, now), Verdict::Resume);
        // Picked up twice already and killed again: something in the turn is
        // what takes the daemon down.
        let looping = Journal { resumes: MAX_RESUMES, ..journal(Some("p"), Some("s"), true) };
        assert!(closed(verdict(Some(&looping), at, now)));
        // Nobody's message started it — an introduction, a background-task
        // wrap-up: those are armed again at startup on their own.
        let unasked = Journal { trigger: None, ..journal(Some("p"), Some("s"), true) };
        assert!(closed(verdict(Some(&unasked), at, now)));
        // Died before it asked the model anything: the bubble is empty and the
        // trigger replays from `pending`.
        assert_eq!(verdict(Some(&journal(None, None, false)), at, now), Verdict::Discard);
        // Too long ago.
        assert_eq!(
            verdict(Some(&journal(Some("p"), Some("s"), true)), long_ago, now),
            Verdict::Finalize("it stopped more than 30 minutes before the agent came back"),
            "and the card's details say why"
        );
        // No journal: what every draft got before.
        assert!(closed(verdict(None, at, now)));
    }

    /// 2026-11-11: an update's restart started the next daemon while the one
    /// it had stopped was still there (a zombie answers `kill(pid, 0)`). The
    /// next one passed its reply over as another live daemon's, never looked
    /// again, and the reply said «generating» for 12 hours. A producer still
    /// running at startup keeps its drafts — and once it has gone, they are
    /// taken over: picked back up, or closed as cut off.
    #[test]
    fn a_reply_left_by_a_process_still_going_is_taken_over_once_it_has_gone() {
        let dir = dir("taken-over");
        let (resumed, cut, done) = (
            "00000000-0000-0000-0000-000000000021",
            "00000000-0000-0000-0000-000000000022",
            "00000000-0000-0000-0000-000000000023",
        );
        let old = Outbox::load(dir.clone(), |_, _| false).unwrap();
        for id in [resumed, cut, done] {
            old.track(id).unwrap();
        }
        old.journal(resumed, journal(Some("fix the bug"), Some("s-live"), true));
        old.journal(cut, Journal { trigger: None, ..journal(Some("wrap up"), Some("s"), true) });
        old.complete(done, "the answer", None, None).unwrap();
        die(&old);
        let dying = std::process::id() + 1;
        let gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let next = {
            let gone = gone.clone();
            Outbox::load(dir.clone(), move |pid, _| pid == dying && !gone.load(std::sync::atomic::Ordering::SeqCst)).unwrap()
        };
        assert!(next.entries.lock().unwrap().is_empty(), "still going: its drafts are its own");
        assert!(next.take_resumable().is_empty());
        assert_eq!(next.take_over_dead().unwrap(), 0, "and stay so while it runs");

        gone.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(next.take_over_dead().unwrap(), 3);
        let taken = next.take_resumable();
        assert_eq!(taken.iter().map(|(id, j)| (id.as_str(), j.resumes)).collect::<Vec<_>>(), [(resumed, 1)]);
        {
            let entries = next.entries.lock().unwrap();
            let e = &entries[cut];
            assert!(e.ready && e.content.is_none());
            assert_eq!(e.outcome, Some(crate::client::TurnEnd::Failed));
            assert_eq!(e.cut.as_deref(), Some("no message started it (a background-task update or an introduction)"));
            assert_eq!(entries[done].content.as_deref(), Some("the answer"), "a finished one is just delivered");
            assert!(entries[done].cut.is_none());
            assert!(entries.values().all(|e| e.pid == std::process::id()), "ours now");
        }
        assert_eq!(next.take_over_dead().unwrap(), 0, "taken over once");
        assert!(next.take_resumable().is_empty());
        // What it now holds is its own on disk too: the start after it judges
        // them as this process's.
        let on_disk: Entry = serde_json::from_slice(&std::fs::read(dir.join(format!("{cut}.json"))).unwrap()).unwrap();
        assert_eq!((on_disk.pid, on_disk.pid_start), (std::process::id(), my_start()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The process an entry names is asked for by its start too: a pid handed
    /// to another process since (a reboot) is not the producer, and a zombie —
    /// a daemon stopped and not yet reaped — is not running.
    #[cfg(unix)]
    #[test]
    fn the_producer_is_asked_for_by_pid_and_start() {
        let mut child = crate::platform::idle_child();
        let pid = child.id();
        let start = crate::platform::start_token(pid);
        assert!(owner_running(pid, start), "itself");
        assert!(owner_running(pid, None), "an entry from before the start was kept");
        if let Some(s) = start {
            assert!(!owner_running(pid, Some(s + 1)), "another process given its pid");
        }
        child.kill().unwrap();
        assert!(crate::platform::wait_until(std::time::Duration::from_secs(5), || !owner_running(pid, start)), "a zombie isn't running");
        assert!(crate::platform::pid_alive(pid), "though it still answers kill(pid, 0)");
        child.wait().unwrap();
        assert!(!owner_running(pid, start));
        assert!(owner_running(std::process::id(), my_start()), "this process");
    }

    /// A reply closed as cut off keeps what it said, loses the generating card
    /// it was left on, and ends on the shared failure card: «This turn was cut
    /// off» — the reason in its details, for whoever reads those.
    #[test]
    fn a_reply_cut_off_for_good_says_so() {
        let live = "{% mafold/trace summary=\"Ran 3 shell commands\" steps=\"3\" %}\n{% /mafold/trace %}\n\nChecking dark mode next:\n```sh\nnpm test\n\n{% mafold/generating started=1 beat=2 beatAt=3 tokens=4 shells=0 /%}";
        let (closed, private) = cut_off(live, "it stopped more than 30 minutes before the agent came back", None);
        assert!(closed.starts_with("{% mafold/trace summary=\"Ran 3 shell commands\""), "what it did stays: {closed}");
        assert!(!closed.contains("mafold/generating"), "{closed}");
        assert!(closed.contains("npm test\n```"), "the code block it left open is closed before the card: {closed}");
        assert!(closed.trim_end().ends_with("/%}") && closed.contains("{% mafold/error kind=\"interrupted\""), "{closed}");
        let item = private.expect("the reason, for the owner");
        assert!(item.key.starts_with("mafold/error/") && item.key.ends_with(".detail"), "{}", item.key);
        let raw = item.v["raw"].as_str().unwrap();
        assert!(raw.contains("turn_interrupted") && raw.contains("more than 30 minutes"), "{raw}");
        assert!(closed.contains("for=\"owner\""), "{closed}");

        // Nothing on screen but the card it was left on: only the failure card.
        let (empty, _) = cut_off("{% mafold/generating started=1 beat=2 beatAt=3 tokens=0 shells=0 /%}", "x", None);
        assert!(empty.starts_with("{% mafold/error kind=\"interrupted\""), "{empty}");
        // A house bot's: the details are for whoever asked.
        let (house, _) = cut_off("half an answer", "x", Some("ann".into()));
        assert!(house.starts_with("half an answer\n\n{% mafold/error") && house.contains("for=\"@ann\""), "{house}");
        // An error some other kind's words match is still a cut-off reply.
        let (odd, _) = cut_off("x", "it couldn't be read back (HTTP 429 rate limit)", None);
        assert!(odd.contains("kind=\"interrupted\"") && !odd.contains("status="), "{odd}");
    }

    /// Delivering a cut-off reply reads it back, closes it with the card, and
    /// finalizes it as failed — and a retry after the edit went through sends
    /// the same text again, never a second card. A reply finished meanwhile
    /// (a Stop from a client) is left as it is.
    #[tokio::test]
    async fn delivering_a_cut_off_reply_closes_it_once() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(format!("http://{}", listener.local_addr().unwrap()), "dev:test".into());
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let mut request = Vec::new();
                let (head, body) = loop {
                    let mut buf = [0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break (String::new(), String::new());
                    }
                    request.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len: usize = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        if request.len() >= end + 4 + len {
                            break (text[..end].to_string(), text[end + 4..].to_string());
                        }
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                let reply = match path.as_str() {
                    "/api/getMessage" if body["message_id"] == "00000000-0000-0000-0000-000000000032" => {
                        serde_json::json!({ "ok": true, "result": { "content": "done", "finalized_at": "2026-11-11T12:00:00Z" } })
                    }
                    "/api/getMessage" => serde_json::json!({ "ok": true, "result": {
                        "content": "half of it\n\n{% mafold/generating started=1 beat=2 beatAt=3 tokens=4 shells=0 /%}",
                        "finalized_at": null,
                    } }),
                    // The first finalize is lost on the way back.
                    "/api/botFinalize" if !seen.iter().any(|(p, _): &(String, serde_json::Value)| p == "/api/botFinalize") => {
                        serde_json::json!({ "ok": false, "error_code": 503, "description": "unavailable" })
                    }
                    _ => serde_json::json!({ "ok": true, "result": { "ok": true } }),
                };
                seen.push((path.clone(), body));
                let reply = reply.to_string();
                let _ = socket
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                    .await;
                if path == "/api/botFinalize" && seen.iter().filter(|(p, _)| p == "/api/botFinalize").count() == 2 {
                    break;
                }
            }
            seen
        });
        let dir = dir("cut-delivery");
        let (cut, stopped) = ("00000000-0000-0000-0000-000000000031", "00000000-0000-0000-0000-000000000032");
        let outbox = Outbox::load(dir.clone(), |_, _| false).unwrap();
        outbox.track(cut).unwrap();
        outbox.journal(cut, journal(Some("p"), Some("s"), true));
        outbox.track(stopped).unwrap();
        outbox.give_up(cut, "picking it back up failed (boom)");
        outbox.give_up(stopped, "picking it back up failed (boom)");
        assert!(outbox.deliver(&client, stopped).await.unwrap(), "finished meanwhile: nothing to close");
        assert!(!outbox.entries.lock().unwrap().contains_key(stopped));
        assert!(outbox.deliver(&client, cut).await.is_err(), "the finalize didn't get through");
        let recovered = Outbox::load(dir.clone(), |_, _| false).unwrap();
        assert!(recovered.deliver(&client, cut).await.unwrap());
        assert!(recovered.entries.lock().unwrap().is_empty());
        let seen = server.await.unwrap();
        let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            ["/api/getMessage", "/api/getMessage", "/api/setCardState", "/api/botEditDraft", "/api/botFinalize", "/api/botFinalize"],
            "read back once, edited once"
        );
        let edit = &seen[3].1;
        let text = edit["content"].as_str().unwrap();
        assert!(text.starts_with("half of it\n\n{% mafold/error kind=\"interrupted\""), "{text}");
        assert!(!text.contains("mafold/generating"), "{text}");
        let fin = &seen[5].1;
        assert_eq!(fin["outcome"], "failed", "{fin}");
        std::fs::remove_dir_all(dir).unwrap();
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
        let o = Outbox::load_at(dir.clone(), |_, _| false, SystemTime::now()).unwrap();
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
        let o = Outbox::load_at(dir.clone(), |_, _| false, SystemTime::now()).unwrap();
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
        let o = Outbox::load(dir.clone(), |_, _| false).unwrap();
        o.track(a).unwrap();
        o.journal(a, journal(Some("p"), Some("s"), true));
        o.track(b).unwrap();
        o.carry(a, b);
        o.forget(a).unwrap();
        die(&o);
        let taken = Outbox::load(dir.clone(), |_, _| false).unwrap().take_resumable();
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
        let o = Outbox::load(dir.clone(), |_, _| false).unwrap();
        o.track(id).unwrap();
        o.journal(id, journal(Some("p"), Some("s"), true));
        o.complete(id, "the answer", None, None).unwrap();
        die(&o);
        let next = Outbox::load(dir.clone(), |_, _| false).unwrap();
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
        let outbox = Outbox::load(dir.clone(), |_, _| false).unwrap();
        outbox.track(id).unwrap();
        outbox.complete(id, "full final answer", None, None).unwrap();
        assert!(outbox.deliver(&client, id).await.is_err());
        let recovered = Outbox::load(dir.clone(), |_, _| false).unwrap();
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
