//! The local mirror of this bot's drive (`.docs/bot-drive-v1.md` §5).
//!
//! The drive lives on the server; this is a CACHE of it at
//! `~/.mafold/drives/<id>/` that Claude Code is pointed at:
//!
//! ```text
//! <root>/.claude-plugin/plugin.json   {"name":"<bot label>"} → skills show as <label>:<skill>
//! <root>/skills/<skill>/…             --plugin-dir <root>
//! <root>/memory/…                     --settings {"autoMemoryDirectory":"<root>/memory"}
//! <root>/.mafold-drive.json           what this mirror last agreed with the server
//! ```
//!
//! * **Pull** brings the mirror to the server's revision: every file is
//!   verified against its sha256 and written through a temp file + rename, so
//!   a half-written file is never seen and a file held open by the agent
//!   (Windows) is retried rather than lost.
//! * **Push** sends what the agent changed in `memory/` — nothing else: skills
//!   are read-only for the bot, and a local edit to one is put back on the
//!   next pull. Every change is a compare-and-swap; a file changed on both
//!   sides keeps both (the local one as a `.conflict-…` copy). Nothing is lost.
//! * **Revert** puts `memory/` back to what the server has, for a turn that
//!   wasn't the owner's (only the owner's turns may change the bot's memory —
//!   owner call 2026-09-29).
//!
//! Every server path is checked with the same rule the server commits with
//! (`mafold_types::drive::validate_path`) before it touches the disk.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use mafold_core::mafold_types::drive::{self as wire, DriveChange, DriveCommitResult, DriveListing};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const STATE: &str = ".mafold-drive.json";

/// What the mirror needs from the server — a trait so the sync logic is tested
/// against a fake, not against a network.
#[async_trait]
pub trait DriveApi: Send + Sync {
    async fn list(&self, since: Option<i64>) -> Result<DriveListing>;
    async fn get(&self, path: &str) -> Result<Vec<u8>>;
    async fn have(&self, shas: &[String]) -> Result<Vec<String>>;
    async fn upload(&self, bytes: &[u8]) -> Result<String>;
    async fn commit(&self, changes: Vec<DriveChange>) -> Result<DriveCommitResult>;
    /// Report the old memory this bot found (`reportDriveCandidates`).
    async fn report_candidates(&self, r: &wire::ReportCandidates) -> Result<()>;
    /// That report as the server holds it — with its verdict on every file.
    async fn candidates(&self) -> Result<wire::CandidateListing>;
    /// Send the owner the offer card, in their chat with the bot.
    async fn offer(&self, owner: &str, text: &str) -> Result<()>;
}

/// The real server, as the bot itself (the daemon's token): `account` is left
/// out, which the drive endpoints read as "my own drive".
pub struct Remote(pub crate::client::Client);

#[async_trait]
impl DriveApi for Remote {
    async fn list(&self, since: Option<i64>) -> Result<DriveListing> {
        let v = self.0.call("listDrive", json!({ "since_rev": since })).await?;
        Ok(serde_json::from_value(v)?)
    }
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let v = self.0.call("getDriveFile", json!({ "path": path })).await?;
        let b64 = v["content_b64"].as_str().context("getDriveFile: no content")?;
        Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
    }
    async fn have(&self, shas: &[String]) -> Result<Vec<String>> {
        let v = self.0.call("haveDriveBlobs", json!({ "shas": shas })).await?;
        Ok(serde_json::from_value(v["have"].clone()).unwrap_or_default())
    }
    async fn upload(&self, bytes: &[u8]) -> Result<String> {
        let v = self
            .0
            .call("uploadDriveBlob", json!({ "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes) }))
            .await?;
        Ok(v["sha"].as_str().context("uploadDriveBlob: no sha")?.to_string())
    }
    async fn commit(&self, changes: Vec<DriveChange>) -> Result<DriveCommitResult> {
        let v = self
            .0
            .call("commitDrive", json!({ "account": "", "op_id": uuid::Uuid::new_v4(), "changes": changes }))
            .await?;
        Ok(serde_json::from_value(v)?)
    }
    async fn report_candidates(&self, r: &wire::ReportCandidates) -> Result<()> {
        self.0.call("reportDriveCandidates", serde_json::to_value(r)?).await?;
        Ok(())
    }
    async fn candidates(&self) -> Result<wire::CandidateListing> {
        Ok(serde_json::from_value(self.0.call("listDriveCandidates", json!({})).await?)?)
    }
    async fn offer(&self, owner: &str, text: &str) -> Result<()> {
        let chat = self.0.resolve_chat(&format!("@{owner}")).await?;
        self.0.send_to(crate::client::Dest::chat(&chat), text).await?;
        Ok(())
    }
}

/// One file as the mirror last saw it on BOTH sides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Known {
    rev: i64,
    sha: String,
    size: u64,
    /// Local mtime (ms) right after we wrote or pushed it — a cheap "did the
    /// agent touch it" check before hashing.
    mtime_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    id: String,
    rev: i64,
    memory_mounted: bool,
    files: BTreeMap<String, Known>,
    /// Working directories this bot has run in while its memory wasn't in the
    /// drive yet: their Claude Code memory folders are what it offers its
    /// owner (`offer_old_memory`).
    #[serde(default)]
    dirs: std::collections::BTreeSet<String>,
    /// Digest of the last old-memory report the server accepted.
    #[serde(default)]
    reported: Option<String>,
    /// Digest of what the last offer card showed, and when it was sent.
    #[serde(default)]
    offered: Option<(String, i64)>,
}

/// What a pull changed.
#[derive(Debug, Default, PartialEq)]
pub struct Pulled {
    pub skills: bool,
    pub memory: bool,
}

pub struct Mirror {
    api: Arc<dyn DriveApi>,
    root: PathBuf,
    /// The bot, as its offer card names it.
    bot: String,
    /// Plugin name the skills are listed under (`<label>:<skill>`).
    label: String,
    /// The bot's owner (lowercase): whose turns may change its memory.
    owner: Option<String>,
    state: tokio::sync::Mutex<State>,
    /// Held while an old-memory offer is being made: the startup check and a
    /// finishing turn can both reach it, and must not both send a card.
    offering: tokio::sync::Mutex<()>,
}

fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Write through a temp file in the same folder and rename it into place — a
/// reader never sees half a file. On Windows the rename fails while another
/// process holds the target open (the agent reading it, an antivirus scan):
/// retried with backoff, never "delete, then write".
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("no parent folder")?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.mafold-tmp"));
    std::fs::write(&tmp, bytes)?;
    let mut last = None;
    for i in 0..6u64 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50 * (1 << i)));
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("rename failed")))
}

/// `~/.mafold/drives` — the folder every mirror on this machine lives in.
pub fn drives_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".mafold").join("drives")
}

/// The plugin name for a bot: its label, kept to what a plugin name holds, and
/// never `mafold` (that one is the daemon's own plugin).
pub fn plugin_label(bot: &str) -> String {
    let label = bot.rsplit(':').next().unwrap_or(bot);
    let s: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    match s.as_str() {
        "" => "bot".into(),
        "mafold" => "mafold-bot".into(),
        _ => s,
    }
}

impl Mirror {
    /// Find (or create) the mirror for the drive the API answers for, and
    /// bring it current.
    pub async fn open(api: Arc<dyn DriveApi>, drives: &Path, bot: &str, owner: Option<String>) -> Result<Arc<Self>> {
        let first = api.list(None).await?;
        anyhow::ensure!(!first.id.is_empty() && first.id.chars().all(|c| c.is_ascii_alphanumeric()), "drive has no usable id");
        let root = drives.join(&first.id);
        std::fs::create_dir_all(&root)?;
        let state: State = std::fs::read(root.join(STATE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(|s: &State| s.id == first.id)
            .unwrap_or_else(|| State { id: first.id.clone(), ..Default::default() });
        let m = Arc::new(Self {
            api,
            root,
            bot: bot.trim_start_matches('@').to_string(),
            label: plugin_label(bot),
            owner: owner.map(|o| o.to_lowercase()),
            state: tokio::sync::Mutex::new(state),
            offering: tokio::sync::Mutex::new(()),
        });
        m.pull().await?;
        // Empty shells left under skills/ before removals pruned their folders
        // (cli 0.9.130), or made by hand: a restart clears them.
        remove_empty_dirs_under(&m.root.join(wire::AREA_SKILLS));
        Ok(m)
    }

    /// The plugin folder to hand the agent (`--plugin-dir`), as a plain path —
    /// never the `\\?\` form Windows `canonicalize` produces, which Claude Code
    /// refuses for its memory folder.
    pub fn root(&self) -> String {
        crate::commands::strip_extended_prefix(&self.root.to_string_lossy()).to_string()
    }

    /// The agent's memory folder, when this bot's memory lives in its drive;
    /// `None` = keep the agent's own default (the owner hasn't moved it yet).
    pub async fn memory_dir(&self) -> Option<String> {
        self.state.lock().await.memory_mounted.then(|| {
            crate::commands::strip_extended_prefix(&self.root.join(wire::AREA_MEMORY).to_string_lossy()).to_string()
        })
    }

    /// Was this turn triggered by the bot's owner?
    pub fn is_owner(&self, sender: &str) -> bool {
        self.owner.as_deref().is_some_and(|o| o.eq_ignore_ascii_case(sender.trim_start_matches('@')))
    }

    fn local(&self, path: &str) -> PathBuf {
        let mut p = self.root.clone();
        for seg in path.split('/') {
            p.push(seg);
        }
        p
    }

    /// After a file left the mirror: remove the folders it leaves empty, from
    /// its own upward, stopping at the first that still holds something and
    /// never touching an area root (`skills/`, `memory/` — the latter is the
    /// agent's memory directory). Only that one chain is looked at, so a folder
    /// the agent just made elsewhere is never swept from under it.
    fn prune_empty_parents(&self, file: &Path) {
        let roots = [self.root.join(wire::AREA_SKILLS), self.root.join(wire::AREA_MEMORY)];
        let mut dir = file.parent();
        while let Some(d) = dir {
            if !d.starts_with(&self.root) || d == self.root || roots.iter().any(|r| r == d) {
                break;
            }
            // Fails on a folder that isn't empty (or is already gone): stop there.
            if std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    async fn save(&self, s: &State) -> Result<()> {
        write_atomic(&self.root.join(STATE), &serde_json::to_vec_pretty(s)?)
    }

    fn write_plugin_manifest(&self) -> Result<()> {
        let p = self.root.join(".claude-plugin").join("plugin.json");
        let body = serde_json::to_vec(&json!({ "name": self.label }))?;
        if std::fs::read(&p).ok().as_deref() != Some(&body[..]) {
            write_atomic(&p, &body)?;
        }
        Ok(())
    }

    /// Does the local copy of a known file still match what we last agreed?
    fn unchanged(&self, path: &str, k: &Known) -> bool {
        let p = self.local(path);
        let Ok(meta) = std::fs::metadata(&p) else { return false };
        if meta.len() == k.size && mtime_ms(&p) == k.mtime_ms {
            return true;
        }
        std::fs::read(&p).map(|b| sha_hex(&b) == k.sha).unwrap_or(false)
    }

    /// Keep a local file that is about to be overwritten or dropped: moved
    /// beside itself as `<stem>.conflict-<host>-<epoch>.<ext>`, so the next
    /// push sends it up as a new file.
    fn keep_conflict(&self, path: &str) -> Result<()> {
        let p = self.local(path);
        if !p.exists() {
            return Ok(());
        }
        let host: String = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "here".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(12)
            .collect();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
        let name = match p.extension().and_then(|e| e.to_str()) {
            Some(ext) => format!("{stem}.conflict-{host}-{stamp}.{ext}"),
            None => format!("{stem}.conflict-{host}-{stamp}"),
        };
        std::fs::rename(&p, p.with_file_name(name))?;
        Ok(())
    }

    /// Bring the mirror to the server's current revision.
    pub async fn pull(&self) -> Result<Pulled> {
        let mut s = self.state.lock().await;
        let listing = self.api.list(Some(s.rev)).await?;
        let mut out = Pulled::default();
        let full = listing.reset || s.rev == 0;
        s.memory_mounted = listing.memory_mounted;
        self.write_plugin_manifest()?;

        // A skills file the agent edited locally goes back to the server's
        // version: skills are read-only for the bot.
        let tampered: Vec<String> = s
            .files
            .iter()
            .filter(|(p, k)| wire::area(p) == Some(wire::AREA_SKILLS) && !self.unchanged(p, k))
            .map(|(p, _)| p.clone())
            .collect();

        let mut listed = std::collections::BTreeSet::new();
        for e in &listing.entries {
            listed.insert(e.path.clone());
            if wire::validate_path(&e.path).is_err() {
                eprintln!("drive: skipping `{}` — not a path this machine can hold", e.path);
                continue;
            }
            let Some(sha) = e.sha.clone() else { continue };
            let known = s.files.get(&e.path).cloned();
            if known.as_ref().is_some_and(|k| k.sha == sha) && !tampered.contains(&e.path) && self.local(&e.path).exists() {
                // Same bytes; just remember the newer rev.
                if let Some(k) = s.files.get_mut(&e.path) {
                    k.rev = e.rev;
                }
                continue;
            }
            // The agent changed a memory file the server ALSO changed: keep ours
            // as a conflict copy rather than overwrite it.
            if wire::area(&e.path) == Some(wire::AREA_MEMORY) {
                let dirty = match &known {
                    Some(k) => !self.unchanged(&e.path, k),
                    None => self.local(&e.path).exists(),
                };
                if dirty {
                    let local = std::fs::read(self.local(&e.path)).ok();
                    if local.as_deref().map(sha_hex) != Some(sha.clone()) {
                        self.keep_conflict(&e.path)?;
                    }
                }
            }
            let bytes = self.api.get(&e.path).await?;
            anyhow::ensure!(sha_hex(&bytes) == sha, "drive: `{}` arrived corrupted", e.path);
            let p = self.local(&e.path);
            write_atomic(&p, &bytes)?;
            s.files.insert(e.path.clone(), Known { rev: e.rev, sha, size: bytes.len() as u64, mtime_ms: mtime_ms(&p) });
            match wire::area(&e.path) {
                Some(wire::AREA_SKILLS) => out.skills = true,
                _ => out.memory = true,
            }
        }
        // Deleted on the server (or, after a reset, simply not there any more).
        let gone: Vec<String> = if full {
            s.files.keys().filter(|p| !listed.contains(*p)).cloned().collect()
        } else {
            listing.removed.clone()
        };
        for path in gone {
            let Some(k) = s.files.remove(&path) else { continue };
            if wire::area(&path) == Some(wire::AREA_MEMORY) && !self.unchanged(&path, &k) {
                // Deleted there, edited here: the edit survives as a new file.
                self.keep_conflict(&path)?;
            } else {
                let p = self.local(&path);
                let _ = std::fs::remove_file(&p);
                self.prune_empty_parents(&p);
            }
            match wire::area(&path) {
                Some(wire::AREA_SKILLS) => out.skills = true,
                _ => out.memory = true,
            }
        }
        // Skills files nobody knows about (left over, or dropped in by hand) go,
        // and so do the empty folders that leaves: the folder is the server's.
        if full {
            for f in walk(&self.root.join(wire::AREA_SKILLS)) {
                if let Some(rel) = self.rel(&f) {
                    if !s.files.contains_key(&rel) {
                        let _ = std::fs::remove_file(&f);
                        out.skills = true;
                    }
                }
            }
            remove_empty_dirs_under(&self.root.join(wire::AREA_SKILLS));
        }
        s.rev = listing.rev;
        self.save(&s).await?;
        Ok(out)
    }

    /// `<root>/memory/a/b.md` → `memory/a/b.md` (NFC, `/`-separated).
    fn rel(&self, p: &Path) -> Option<String> {
        let r = p.strip_prefix(&self.root).ok()?;
        let parts: Vec<String> = r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        Some(wire::normalize(&parts.join("/")))
    }

    /// Send what the agent changed in `memory/`. Returns how many files went up.
    pub async fn push_memory(&self) -> Result<usize> {
        for attempt in 0..2 {
            let mut s = self.state.lock().await;
            if !s.memory_mounted {
                return Ok(0);
            }
            let mut changes = Vec::new();
            let mut bodies: Vec<(String, Vec<u8>)> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for f in walk(&self.root.join(wire::AREA_MEMORY)) {
                let Some(rel) = self.rel(&f) else { continue };
                if f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.contains(".mafold-tmp")) {
                    continue;
                }
                if let Err(e) = wire::validate_path(&rel) {
                    eprintln!("drive: not syncing `{rel}` — {e}");
                    continue;
                }
                seen.insert(rel.clone());
                let known = s.files.get(&rel).cloned();
                if known.as_ref().is_some_and(|k| self.unchanged(&rel, k)) {
                    continue;
                }
                let bytes = std::fs::read(&f)?;
                let sha = sha_hex(&bytes);
                if known.as_ref().is_some_and(|k| k.sha == sha) {
                    continue;
                }
                changes.push(DriveChange { path: rel.clone(), sha: Some(sha), expected_rev: known.map(|k| k.rev), mime: None });
                bodies.push((rel, bytes));
            }
            let deleted: Vec<(String, i64)> = s
                .files
                .iter()
                .filter(|(p, _)| wire::area(p) == Some(wire::AREA_MEMORY) && !seen.contains(*p))
                .map(|(p, k)| (p.clone(), k.rev))
                .collect();
            for (p, rev) in &deleted {
                changes.push(DriveChange { path: p.clone(), sha: None, expected_rev: Some(*rev), mime: None });
            }
            if changes.is_empty() {
                return Ok(0);
            }
            let want: Vec<String> = changes.iter().filter_map(|c| c.sha.clone()).collect();
            let have = self.api.have(&want).await?;
            for (_, bytes) in &bodies {
                if !have.contains(&sha_hex(bytes)) {
                    self.api.upload(bytes).await?;
                }
            }
            let n = changes.len();
            let r = self.api.commit(changes).await?;
            if r.applied {
                for e in &r.entries {
                    match &e.sha {
                        Some(sha) => {
                            let p = self.local(&e.path);
                            s.files.insert(e.path.clone(), Known { rev: e.rev, sha: sha.clone(), size: e.size, mtime_ms: mtime_ms(&p) });
                        }
                        None => {
                            s.files.remove(&e.path);
                        }
                    }
                }
                self.save(&s).await?;
                return Ok(n);
            }
            // Someone else changed some of these first. Pull (which keeps our
            // side as conflict copies) and try once more with fresh versions.
            drop(s);
            if attempt == 0 {
                self.pull().await?;
            }
        }
        anyhow::bail!("drive: memory changed on the server twice while syncing; will retry next turn")
    }

    /// Put `memory/` back to what the server has: files the agent added go,
    /// files it changed or deleted come back. Returns how many were undone.
    pub async fn revert_memory(&self) -> Result<usize> {
        let s = self.state.lock().await;
        if !s.memory_mounted {
            return Ok(0);
        }
        let mut undone = 0;
        let mut seen = std::collections::BTreeSet::new();
        for f in walk(&self.root.join(wire::AREA_MEMORY)) {
            let Some(rel) = self.rel(&f) else { continue };
            seen.insert(rel.clone());
            match s.files.get(&rel) {
                None => {
                    std::fs::remove_file(&f)?;
                    self.prune_empty_parents(&f);
                    undone += 1;
                }
                Some(k) if !self.unchanged(&rel, k) => {
                    let bytes = self.api.get(&rel).await?;
                    write_atomic(&f, &bytes)?;
                    undone += 1;
                }
                Some(_) => {}
            }
        }
        for (rel, _) in s.files.iter().filter(|(p, _)| wire::area(p) == Some(wire::AREA_MEMORY) && !seen.contains(*p)) {
            let bytes = self.api.get(rel).await?;
            write_atomic(&self.local(rel), &bytes)?;
            undone += 1;
        }
        drop(s);
        if undone > 0 {
            // Mtimes moved: record them so the next push sees no change.
            let mut s = self.state.lock().await;
            for (rel, k) in s.files.iter_mut() {
                if wire::area(rel) == Some(wire::AREA_MEMORY) {
                    k.mtime_ms = mtime_ms(&self.local(rel));
                }
            }
            self.save(&s).await?;
        }
        Ok(undone)
    }

    // ---- an existing bot's old memory (`.docs/bot-drive-v1.md` §5.6) ----

    /// A turn is about to run in `dir`: until the memory is the drive's, its
    /// Claude Code memory folder is part of what this bot offers its owner.
    pub async fn note_workdir(&self, dir: &str) {
        let dir = dir.trim();
        if dir.is_empty() {
            return;
        }
        let mut s = self.state.lock().await;
        if s.memory_mounted || !s.dirs.insert(dir.to_string()) {
            return;
        }
        let _ = self.save(&s).await;
    }

    /// Offer the owner the memory this bot kept before it had a drive.
    pub async fn offer_old_memory(&self) -> Result<()> {
        self.offer_old_memory_in(&claude_home()).await
    }

    /// Read (never change) the memory folders of the directories this bot has
    /// worked in; when that differs from the last report, upload what the
    /// server lacks and report it; send the owner the offer card the first
    /// time — and again only when there is something new AND a day has
    /// passed, so a bot that keeps writing its old memory doesn't keep
    /// knocking. The owner answers on the card; until then nothing changes.
    pub(crate) async fn offer_old_memory_in(&self, claude_home: &Path) -> Result<()> {
        let Ok(_one_at_a_time) = self.offering.try_lock() else { return Ok(()) };
        let (dirs, reported, offered) = {
            let s = self.state.lock().await;
            if s.memory_mounted {
                return Ok(());
            }
            (s.dirs.clone(), s.reported.clone(), s.offered.clone())
        };
        let home = claude_home.to_path_buf();
        let (sources, bodies, previews) = tokio::task::spawn_blocking(move || scan_old_memory(&home, &dirs)).await?;
        if sources.is_empty() {
            return Ok(()); // nothing was ever remembered: nothing to ask about
        }
        let digest = sha_hex(
            sources
                .iter()
                .flat_map(|s| s.files.iter().map(move |f| format!("{}\0{}\0{}\n", s.dir, f.path, f.sha)))
                .collect::<String>()
                .as_bytes(),
        );
        if reported.as_deref() != Some(digest.as_str()) {
            let shas: Vec<String> = bodies.keys().cloned().collect();
            let mut have = std::collections::BTreeSet::new();
            for chunk in shas.chunks(500) {
                have.extend(self.api.have(chunk).await?);
            }
            for (sha, bytes) in &bodies {
                if !have.contains(sha) {
                    self.api.upload(bytes).await?;
                }
            }
            self.api.report_candidates(&wire::ReportCandidates { sources }).await?;
            let mut s = self.state.lock().await;
            s.reported = Some(digest.clone());
            self.save(&s).await?;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let due = match &offered {
            None => true,
            Some((seen, at)) => *seen != digest && now - at >= OFFER_AGAIN_SECS,
        };
        let Some(owner) = self.owner.as_deref().filter(|_| due) else { return Ok(()) };
        let listing = self.api.candidates().await?;
        if listing.memory_mounted {
            return Ok(());
        }
        let body = wire::offer_body(&listing.sources, |d, p| previews.get(&(d.to_string(), p.to_string())).cloned());
        let card = wire::OFFER_CARD;
        let text = format!("{{% {card} bot=\"{}\" %}}\n{body}{{% /{card} %}}", self.bot);
        self.api.offer(owner, &text).await?;
        let mut s = self.state.lock().await;
        s.offered = Some((digest, now));
        self.save(&s).await?;
        println!("☁ drive: offered {} old memory file(s) to @{owner}", listing.sources.iter().map(|x| x.files.len()).sum::<usize>());
        Ok(())
    }
}

/// A memory file bigger than this can't go into a drive's `memory/`: it isn't
/// offered (nor uploaded — no point shipping bytes that will be refused).
const MAX_OFFER_FILE_BYTES: usize = 256 << 10;
/// What one report may carry (the server's limits).
const MAX_OFFER_DIRS: usize = 16;
const MAX_OFFER_FILES: usize = 1_000;

type Scanned = (Vec<wire::CandidateSource>, BTreeMap<String, Vec<u8>>, BTreeMap<(String, String), String>);

/// Read the old memory folders of `dirs` (never write them). Leaves out what
/// the drive would refuse by NAME — so one odd file can't sink the whole
/// offer — and what a card line couldn't show unchanged; what the drive may
/// refuse by CONTENT (a credential in a note) is the server's verdict.
fn scan_old_memory(claude_home: &Path, dirs: &std::collections::BTreeSet<String>) -> Scanned {
    let mut sources = Vec::new();
    let mut bodies: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut previews: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut count = 0usize;
    for dir in dirs.iter().filter(|d| wire::offer_safe(d)) {
        if sources.len() == MAX_OFFER_DIRS {
            eprintln!("drive: more than {MAX_OFFER_DIRS} old memory folders; offering the first {MAX_OFFER_DIRS}");
            break;
        }
        let root = cc_memory_dir(claude_home, dir);
        let mut files = Vec::new();
        for f in walk(&root) {
            let Ok(r) = f.strip_prefix(&root) else { continue };
            let parts: Vec<String> = r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            if parts.iter().any(|p| p.starts_with('.')) {
                continue; // .DS_Store and friends
            }
            let path = wire::normalize(&parts.join("/"));
            if wire::validate_path(&format!("{}/{path}", wire::AREA_MEMORY)).is_err() || !wire::offer_safe(&path) {
                eprintln!("drive: not offering {dir}: `{path}` (a name the drive or the card can't hold)");
                continue;
            }
            let Ok(bytes) = std::fs::read(&f) else { continue };
            if bytes.len() > MAX_OFFER_FILE_BYTES {
                continue;
            }
            if count == MAX_OFFER_FILES {
                eprintln!("drive: more than {MAX_OFFER_FILES} old memory files; offering the first {MAX_OFFER_FILES}");
                break;
            }
            count += 1;
            let sha = sha_hex(&bytes);
            previews.insert((dir.clone(), path.clone()), String::from_utf8_lossy(&bytes[..bytes.len().min(600)]).into_owned());
            files.push(wire::CandidateFile { path, sha: sha.clone(), size: bytes.len() as u64, mtime_ms: mtime_ms(&f), blocked: None });
            bodies.insert(sha, bytes);
        }
        if !files.is_empty() {
            files.sort_by(|a, b| a.path.cmp(&b.path));
            sources.push(wire::CandidateSource { dir: dir.clone(), files });
        }
    }
    (sources, bodies, previews)
}
/// Something new in the old memory re-offers it at most this often.
const OFFER_AGAIN_SECS: i64 = 24 * 3600;

/// Where Claude Code keeps its config: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default().join(".claude")
    })
}

/// The memory folder Claude Code keeps for working directory `cwd`: under
/// `projects/`, named after the path with every character that isn't an
/// ASCII letter or digit turned into `-`.
pub fn cc_memory_dir(claude_home: &Path, cwd: &str) -> PathBuf {
    let name: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    claude_home.join("projects").join(name).join("memory")
}

/// Remove every empty folder under `dir`, deepest first — `dir` itself stays.
/// Only for `skills/`, which is the server's: an empty folder there is never
/// anybody's work in progress (the agent can't write skills).
fn remove_empty_dirs_under(dir: &Path) {
    let mut dirs = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
                dirs.push(e.path());
            }
        }
    }
    // Longest paths first: a child always goes before its parent is tried.
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in dirs {
        let _ = std::fs::remove_dir(&d); // fails (and stays) unless empty
    }
}

/// Every regular file under `dir`, recursively (none if it doesn't exist).
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

// MARK: - The daemon's mirror

static CURRENT: OnceLock<Arc<Mirror>> = OnceLock::new();

/// Install this daemon's mirror (one bot per daemon process).
pub fn install(m: Arc<Mirror>) {
    let _ = CURRENT.set(m);
}

pub fn current() -> Option<Arc<Mirror>> {
    CURRENT.get().cloned()
}

/// The daemon's own plugin (Mafold's skills, e.g. `mafold-room`), next to the
/// bots' drives: `~/.mafold/plugins/mafold`.
pub fn mafold_plugin_dir() -> PathBuf {
    drives_dir().parent().map(|p| p.join("plugins").join("mafold")).unwrap_or_else(|| PathBuf::from("plugins/mafold"))
}

/// What a turn's agent process is pointed at: Mafold's own plugin (always,
/// once installed), the bot's drive (when mirrored), and the drive's memory
/// folder (once the owner has moved the bot's memory there).
pub async fn mount(m: Option<&Mirror>) -> crate::harness::Mount {
    let mut plugin_dirs = Vec::new();
    let own = mafold_plugin_dir();
    if own.join(".claude-plugin").join("plugin.json").exists() {
        plugin_dirs.push(crate::commands::strip_extended_prefix(&own.to_string_lossy()).to_string());
    }
    let mut memory_dir = None;
    if let Some(m) = m {
        plugin_dirs.push(m.root());
        memory_dir = m.memory_dir().await;
    }
    crate::harness::Mount { plugin_dirs, memory_dir }
}

/// `mafold drive-hook` — the command form of the memory guard, for a claude
/// that can't take control-channel hooks: reads the PreToolUse JSON on stdin,
/// prints a refusal when the write lands in the guarded folder, nothing
/// otherwise ("no opinion").
pub fn run_hook() -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Some(guarded) = crate::turnenv::memory_ro() else { return Ok(()) };
    let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    if let Some(out) = guard_response(&v["tool_input"], &guarded) {
        println!("{out}");
    }
    Ok(())
}

// MARK: - Turns that may not change the bot's memory

/// Owner turns in flight right now. A non-owner turn's leftovers are only
/// reverted when none is running — otherwise the revert could undo what the
/// owner's concurrent turn just wrote.
static OWNER_TURNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct OwnerTurn(());
impl OwnerTurn {
    pub fn begin() -> Self {
        OWNER_TURNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(())
    }
}
impl Drop for OwnerTurn {
    fn drop(&mut self) {
        OWNER_TURNS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Held for a turn's whole run; when it drops — however the turn ended — the
/// memory is settled ([`after_turn`]) in the background.
pub struct AfterTurn {
    owner: bool,
}
impl AfterTurn {
    pub fn new(owner: bool) -> Self {
        Self { owner }
    }
}
impl Drop for AfterTurn {
    fn drop(&mut self) {
        if current().is_some() {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(after_turn(self.owner));
            }
        }
    }
}

/// After a turn: the owner's changes go up; anyone else's are undone (unless
/// an owner turn is still running, in which case they're left and logged —
/// never guessed apart). Then catch up with the server.
pub async fn after_turn(owner_turn: bool) {
    let Some(m) = current() else { return };
    if owner_turn {
        match m.push_memory().await {
            Ok(0) => {}
            Ok(n) => println!("☁ drive: {n} memory change(s) saved"),
            Err(e) => eprintln!("drive: memory not saved yet ({e:#})"),
        }
    } else if OWNER_TURNS.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        match m.revert_memory().await {
            Ok(0) => {}
            Ok(n) => println!("☁ drive: undid {n} memory change(s) from a turn that wasn't the owner's"),
            Err(e) => eprintln!("drive: couldn't undo memory changes ({e:#})"),
        }
    } else {
        eprintln!("drive: a non-owner turn ended while an owner turn runs — its memory changes (if any) are left for that turn to push");
    }
    refresh(&m).await;
    // Memory not in the drive yet: keep the owner's offer current (a no-op
    // when nothing changed, or for a harness that noted no directories).
    if let Err(e) = m.offer_old_memory().await {
        eprintln!("drive: old memory not offered yet ({e:#})");
    }
}

/// Catch up with the server; when skills changed, retire warm agent processes
/// that loaded the old set (they come back cold, with the new one).
pub async fn refresh(m: &Mirror) {
    match m.pull().await {
        Ok(p) if p.skills => {
            let n = crate::harness::cc_conn::drop_idle();
            println!("☁ drive: skills changed{}", if n > 0 { format!(" — {n} warm process(es) retired") } else { String::new() });
        }
        Ok(_) => {}
        Err(e) => eprintln!("drive: sync failed ({e:#})"),
    }
}

/// PreToolUse answer for a file write while [`crate::harness::Turn::memory_guard`]
/// is set: refused if it lands in the guarded folder. Shared by the in-process
/// hook (`cc_conn`) and the `mafold drive-hook` command a CLI without control
/// hooks runs, so both say exactly the same thing.
pub fn guard_response(tool_input: &serde_json::Value, guarded: &str) -> Option<serde_json::Value> {
    if guarded.is_empty() {
        return None;
    }
    let target = ["file_path", "notebook_path"].iter().find_map(|k| tool_input.get(*k).and_then(|v| v.as_str()))?;
    let norm = |p: &str| crate::commands::strip_extended_prefix(p).replace('\\', "/").trim_end_matches('/').to_lowercase();
    let (t, g) = (norm(target), norm(guarded));
    if t != g && !t.starts_with(&format!("{g}/")) {
        return None;
    }
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "Your memory is read-only in this conversation: only your owner's turns can change it. Answer from what you remember, and don't save anything to memory now.",
        }
    }))
}

#[cfg(test)]
mod tests;
