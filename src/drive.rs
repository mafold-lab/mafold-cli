//! The local mirror of this bot's drive (`.docs/bot-drive-v1.md` §5).
//!
//! The drive lives on the server; this is a CACHE of it at
//! `~/.mafold/drives/<id>/` that Claude Code is pointed at:
//!
//! ```text
//! <root>/.claude-plugin/plugin.json   {"name":"<bot label>"} → skills show as <label>:<skill>
//! <root>/skills/<skill>/…             --plugin-dir <root>
//! <root>/.mafold-drive.json           what this mirror last agreed with the server
//! ```
//!
//! **Pull** brings the mirror to the server's revision: every file is verified
//! against its sha256 and written through a temp file + rename, so a
//! half-written file is never seen and a file held open by the agent (Windows)
//! is retried rather than lost. Nothing goes the other way: skills are
//! read-only for the bot, and a local edit to one is put back on the next pull.
//!
//! A drive holds no memory (2026-09-30): the agent keeps its own memory
//! folder. A `<root>/memory/` left by cli 0.9.130–0.9.133 is not read, not
//! sent anywhere and not deleted — it is the user's disk, and may hold the
//! only copy of something.
//!
//! Every server path is checked with the same rule the server commits with
//! (`mafold_types::drive::validate_path`) before it touches the disk.

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use mafold_core::mafold_types::drive::{self as wire, DriveListing};
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

/// (Files older mirrors kept here besides `id`/`rev`/`files` — `memory_mounted`
/// and, from cli 0.9.133, `dirs`/`reported`/`offered` — are ignored on read and
/// gone on the next save.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    id: String,
    rev: i64,
    files: BTreeMap<String, Known>,
}

/// What a pull changed.
#[derive(Debug, Default, PartialEq)]
pub struct Pulled {
    pub skills: bool,
}

pub struct Mirror {
    api: Arc<dyn DriveApi>,
    root: PathBuf,
    /// Plugin name the skills are listed under (`<label>:<skill>`).
    label: String,
    state: tokio::sync::Mutex<State>,
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
    pub async fn open(api: Arc<dyn DriveApi>, drives: &Path, bot: &str) -> Result<Arc<Self>> {
        let first = api.list(None).await?;
        anyhow::ensure!(!first.id.is_empty() && first.id.chars().all(|c| c.is_ascii_alphanumeric()), "drive has no usable id");
        let root = drives.join(&first.id);
        std::fs::create_dir_all(&root)?;
        let mut state: State = std::fs::read(root.join(STATE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(|s: &State| s.id == first.id)
            .unwrap_or_else(|| State { id: first.id.clone(), ..Default::default() });
        // What an older mirror knew outside `skills/` (its `memory/`) is no
        // longer the server's to track: forgotten here, left on disk.
        state.files.retain(|p, _| wire::area(p).is_some());
        let m = Arc::new(Self { api, root, label: plugin_label(bot), state: tokio::sync::Mutex::new(state) });
        m.pull().await?;
        // Empty shells left under skills/ before removals pruned their folders
        // (cli 0.9.130), or made by hand: a restart clears them.
        remove_empty_dirs_under(&m.root.join(wire::AREA_SKILLS));
        Ok(m)
    }

    /// The plugin folder to hand the agent (`--plugin-dir`), as a plain path —
    /// never the `\\?\` form Windows `canonicalize` produces.
    pub fn root(&self) -> String {
        crate::commands::strip_extended_prefix(&self.root.to_string_lossy()).to_string()
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
    /// never touching an area root (`skills/`). Only that one chain is looked
    /// at, so a folder made elsewhere is never swept from under anyone.
    fn prune_empty_parents(&self, file: &Path) {
        let roots: Vec<PathBuf> = wire::AREAS.iter().map(|a| self.root.join(a)).collect();
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

    /// Bring the mirror to the server's current revision.
    pub async fn pull(&self) -> Result<Pulled> {
        let mut s = self.state.lock().await;
        let listing = self.api.list(Some(s.rev)).await?;
        let mut out = Pulled::default();
        let full = listing.reset || s.rev == 0;
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
            // `validate_path` also refuses anything outside the areas a drive
            // has: an api from before the memory withdrawal may still list
            // `memory/` files, and they are not this mirror's to write.
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
            let bytes = self.api.get(&e.path).await?;
            anyhow::ensure!(sha_hex(&bytes) == sha, "drive: `{}` arrived corrupted", e.path);
            let p = self.local(&e.path);
            write_atomic(&p, &bytes)?;
            s.files.insert(e.path.clone(), Known { rev: e.rev, sha, size: bytes.len() as u64, mtime_ms: mtime_ms(&p) });
            out.skills = true;
        }
        // Deleted on the server (or, after a reset, simply not there any more).
        let gone: Vec<String> = if full {
            s.files.keys().filter(|p| !listed.contains(*p)).cloned().collect()
        } else {
            listing.removed.clone()
        };
        for path in gone {
            if s.files.remove(&path).is_none() {
                continue;
            }
            let p = self.local(&path);
            let _ = std::fs::remove_file(&p);
            self.prune_empty_parents(&p);
            out.skills = true;
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

    /// `<root>/skills/a/b.md` → `skills/a/b.md` (NFC, `/`-separated).
    fn rel(&self, p: &Path) -> Option<String> {
        let r = p.strip_prefix(&self.root).ok()?;
        let parts: Vec<String> = r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        Some(wire::normalize(&parts.join("/")))
    }

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

/// The plugin name a folder goes by — its manifest's `name`, which is what
/// the agent puts before each of its skills (`<name>:<skill>`).
pub fn plugin_name(dir: &Path) -> Option<String> {
    let b = std::fs::read(dir.join(".claude-plugin").join("plugin.json")).ok()?;
    serde_json::from_slice::<serde_json::Value>(&b).ok()?["name"].as_str().map(str::to_string)
}

/// What a turn's agent process is pointed at: Mafold's own plugin (always,
/// once installed) and the bot's drive (when mirrored).
pub async fn mount(m: Option<&Mirror>) -> crate::harness::Mount {
    let mut plugin_dirs = Vec::new();
    let own = mafold_plugin_dir();
    if own.join(".claude-plugin").join("plugin.json").exists() {
        plugin_dirs.push(crate::commands::strip_extended_prefix(&own.to_string_lossy()).to_string());
    }
    if let Some(m) = m {
        plugin_dirs.push(m.root());
    }
    crate::harness::Mount { plugin_dirs, guest: false }
}

/// The skill gate: what a guest's turn — one someone outside the owner's
/// circle started — may invoke with the Skill tool.
///
/// The agent is the OWNER's Claude Code, and their skills are instructions
/// they wrote for themselves; the model reaches for one by its description,
/// for whoever is asking. A guest's process is started without them
/// (`Mount::guest`), and this holds its Skill calls to `plugins`, the ones the
/// daemon mounted (this bot's drive, `<label>:…`, and Mafold's own,
/// `mafold:…`) all the same; anything else is refused, and the reason — which
/// the model reads as the tool's result — says what it may use instead.
/// `None` = no opinion.
///
/// This holds the tool, not the files: the agent runs as the owner's OS user,
/// so a file tool can still open a skill's folder.
pub fn skill_gate(tool_name: &str, tool_input: &serde_json::Value, plugins: &[String]) -> Option<serde_json::Value> {
    if tool_name != "Skill" {
        return None;
    }
    // `skill` today; `command` on older Claude Code (the transcript reads both).
    let name = ["skill", "command", "name"].iter().find_map(|k| tool_input[*k].as_str()).unwrap_or_default();
    let name = name.trim().trim_start_matches('/');
    if may_use_skill(name, plugins) {
        return None;
    }
    let open = if plugins.is_empty() {
        "none are mounted here".to_string()
    } else {
        plugins.iter().map(|p| format!("`{p}:…`")).collect::<Vec<_>>().join(" and ")
    };
    let reason = format!(
        "`{name}` isn't available on this turn: it is one of this agent's owner's own skills, and only the owner and people they've whitelisted may use those. Skills anyone here may use: {open}. Do what you can without it, and say that this part needs the owner."
    );
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }))
}

/// Does `skill` (`<plugin>:<name>`) come from one of `plugins`?
pub fn may_use_skill(skill: &str, plugins: &[String]) -> bool {
    skill.split_once(':').is_some_and(|(p, _)| plugins.iter().any(|q| q == p.trim()))
}

/// The plugins a guest's turn may use skills from: the ones in
/// `mount`, by manifest name — less any name one of the owner's own installed
/// plugins also goes by (`installed`). A skill only says `<name>:<skill>`, so
/// a bot named like one of those would let the owner's whole plugin through;
/// the shared name is held instead, the bot's own skills with it.
pub fn gated_plugins(mount: &crate::harness::Mount, installed: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for name in mount.plugin_dirs.iter().filter_map(|d| plugin_name(Path::new(d))) {
        if installed.contains(&name) {
            eprintln!("skills: `{name}` is also one of the owner's installed plugins — held on guests' turns");
        } else {
            out.push(name);
        }
    }
    out
}

/// `mafold drive-hook` — the command form of [`skill_gate`], for a claude that
/// can't take control-channel hooks. Whose turn it is comes from the turn
/// file (`crate::turnenv::skill_plugins`), never the environment: that names
/// the turn that spawned the process.
///
/// cli 0.9.130–0.9.133 registered it for file writes (the memory guard); a
/// claude one of those started still calls it that way, and it says nothing
/// about anything but the Skill tool — which a PreToolUse hook reads as "go
/// ahead". An unknown subcommand would exit 2: "block this tool call".
pub fn run_hook() -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Some(plugins) = crate::turnenv::skill_plugins() else { return Ok(()) };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&input) else { return Ok(()) };
    if let Some(out) = skill_gate(v["tool_name"].as_str().unwrap_or_default(), &v["tool_input"], &plugins) {
        println!("{out}");
    }
    Ok(())
}

/// Held for a turn's whole run; when it drops — however the turn ended — the
/// mirror catches up with the server in the background, so a skills change
/// whose event was missed lands by the next turn.
pub struct AfterTurn;
impl Drop for AfterTurn {
    fn drop(&mut self) {
        let Some(m) = current() else { return };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { refresh(&m).await });
        }
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

#[cfg(test)]
mod tests;
