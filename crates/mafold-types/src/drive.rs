//! The bot drive — its wire shapes, and the ONE rule for what a path in it may be.
//!
//! A drive is a small file tree that belongs to an account (bots have one by
//! default) and lives on the server, so it follows the bot across machines and
//! reinstalls: `skills/` is what the bot knows how to do, `memory/` is what it
//! remembers. Daemons mirror it to a local folder; hosted bots read it in
//! process. See `.docs/bot-drive-v1.md`.
//!
//! The path rule lives HERE, not in the api or the daemon, for the same reason
//! the provider-pack digest does: the server refuses a path when it is
//! committed and the daemon refuses it again before it touches a disk, and the
//! two must never disagree. A name that a Mac or Linux box can store but NTFS
//! cannot (`nul.md`, `a:b`, two files that differ only in case) would otherwise
//! sit on the server and break every Windows mirror of that bot, for good.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The two top-level folders a drive has. Nothing lives outside them.
pub const AREA_SKILLS: &str = "skills";
pub const AREA_MEMORY: &str = "memory";
pub const AREAS: &[&str] = &[AREA_SKILLS, AREA_MEMORY];

/// Longest drive-relative path, in UTF-16 code units — the unit Windows counts
/// MAX_PATH in. Chosen so a mirror at
/// `C:\Users\<name>\.mafold\drives\<id>\` (≈30 + name) plus this stays near
/// 200, leaving a skill's own scripts room to create files beside themselves
/// under the 260 a default Windows install still enforces.
pub const MAX_PATH_UNITS: usize = 150;
/// Longest single path segment, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 64;
/// Deepest nesting, counting the area itself.
pub const MAX_SEGMENTS: usize = 8;

/// Why a path was refused. The `Display` text is what a user or an agent sees,
/// so it says what to change, not which check fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    /// Not under `skills/` or `memory/`, or names only the area itself.
    Area,
    TooLong,
    TooDeep,
    /// `.`/`..`, an empty segment, or one over [`MAX_SEGMENT_BYTES`].
    Segment(String),
    /// A character NTFS refuses, a control character, or an invisible one.
    Char(char),
    /// A segment that ends in a space or a dot, or starts with a space.
    Edge(String),
    /// A Windows device name (`CON`, `nul.md`, `COM1.txt`, …).
    Reserved(String),
    /// Not in Unicode NFC. APFS hands back decomposed names; the sender must
    /// normalise ([`normalize`]) so one file has one spelling everywhere.
    NotNfc,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Empty => write!(f, "the path is empty"),
            PathError::Area => write!(f, "a drive path must be a file inside skills/ or memory/"),
            PathError::TooLong => write!(f, "the path is longer than {MAX_PATH_UNITS} characters"),
            PathError::TooDeep => write!(f, "the path is nested deeper than {MAX_SEGMENTS} folders"),
            PathError::Segment(s) => write!(f, "`{s}` is not a usable file or folder name"),
            PathError::Char(c) => write!(f, "file names can't contain {c:?}"),
            PathError::Edge(s) => write!(f, "`{s}` starts or ends with a space or ends with a dot"),
            PathError::Reserved(s) => write!(f, "`{s}` is a reserved name on Windows"),
            PathError::NotNfc => write!(f, "the path is not in Unicode NFC form"),
        }
    }
}

impl std::error::Error for PathError {}

/// Rewrite `path` into the one spelling the drive stores: NFC, `/`-separated.
/// Senders call this on names they read off a local disk before committing.
pub fn normalize(path: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    path.replace('\\', "/").nfc().collect()
}

/// Is `path` a name every mirror can hold? `Ok` means yes on Windows, macOS
/// and Linux alike; the checks are the union of what each refuses.
pub fn validate_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if !unicode_normalization::is_nfc(path) {
        return Err(PathError::NotNfc);
    }
    if path.encode_utf16().count() > MAX_PATH_UNITS {
        return Err(PathError::TooLong);
    }
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() > MAX_SEGMENTS {
        return Err(PathError::TooDeep);
    }
    if segs.len() < 2 || !AREAS.contains(&segs[0]) {
        return Err(PathError::Area);
    }
    for s in &segs {
        if s.is_empty() || *s == "." || *s == ".." || s.len() > MAX_SEGMENT_BYTES {
            return Err(PathError::Segment((*s).to_string()));
        }
        if let Some(c) = s.chars().find(|c| forbidden(*c)) {
            return Err(PathError::Char(c));
        }
        if s.starts_with(' ') || s.ends_with(' ') || s.ends_with('.') {
            return Err(PathError::Edge((*s).to_string()));
        }
        if reserved(s) {
            return Err(PathError::Reserved((*s).to_string()));
        }
    }
    Ok(())
}

/// The key two paths collide on. NTFS and APFS (by default) fold case, so
/// `Notes.md` and `notes.md` are one file there even though ext4 keeps both;
/// a drive holds at most one path per fold key.
pub fn fold_key(path: &str) -> String {
    path.to_lowercase()
}

/// Which area a (valid) path is in.
pub fn area(path: &str) -> Option<&str> {
    path.split('/').next().filter(|a| AREAS.contains(a))
}

fn forbidden(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
        || (c as u32) < 0x20
        || c == '\u{7f}'
        // Invisible and direction-changing characters: a name that renders as
        // `report.md` but isn't is how a file hides from the person reviewing it.
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

/// Windows device names. Windows reads everything before the FIRST dot as the
/// name, so `nul.md` and `con.tar.gz` are devices too, and trailing spaces
/// before that dot don't save you.
fn reserved(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p).is_some_and(|n| {
                matches!(n, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
            })
        })
}

// MARK: - Skill library: what a skill needs, and what a bot can give it

/// The skill executes code (Python, shell, …). Only a bot whose runtime
/// provides it can use such a skill: a hosted bot is one model call per turn on
/// Mafold's servers with nowhere to run a script; a daemon-driven bot runs on
/// its owner's machine and can.
pub const NEED_EXEC: &str = "exec";
/// Every capability a library skill may say it needs. A need outside this list
/// is refused at publish, so a typo can't silently make a skill fit everywhere.
pub const NEEDS: &[&str] = &[NEED_EXEC];

/// **The** rule for "can this bot use this skill" — the library listing, the
/// installer, `createBot`'s seed and the onboarding recommender all call this
/// one function with the skill's `needs` and the bot runtime's `provides`
/// (declared in its template manifest). No second copy anywhere.
pub fn skill_fits(needs: &[String], provides: &[String]) -> bool {
    needs.iter().all(|n| provides.iter().any(|p| p == n))
}

/// Licenses under which Mafold may copy a skill into users' drives —
/// installing IS redistribution. SPDX ids. A skill with no license, or a
/// "source-available, do not distribute" one, can't enter the library.
pub const REDISTRIBUTABLE_LICENSES: &[&str] =
    &["MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "0BSD", "CC0-1.0", "CC-BY-4.0", "Unlicense"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStatus {
    /// Reviewed; installable wherever it fits.
    Approved,
    /// Installable wherever it fits, with a caveat the listing shows
    /// (`condition`) — e.g. it expects LibreOffice on the machine.
    Conditional,
    /// Listed so a recommender can say "coming", never installable.
    ComingSoon,
}

/// Where a vendored skill came from, pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSource {
    pub repo: String,
    #[serde(default)]
    pub path: String,
    pub commit: String,
}

/// One library skill as listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibrarySkill {
    /// `owner/slug` — `mafold/pdf`. The same naming cards use.
    pub id: String,
    /// The folder it installs into: `skills/<slug>/`.
    pub slug: String,
    /// Which published version this is (upstream commit, or our own tag).
    pub version: String,
    /// Display text by language tag (`en`, `zh-Hans`). Data, like a card's
    /// description — not UI chrome, so not the langpack.
    pub title: std::collections::BTreeMap<String, String>,
    pub summary: std::collections::BTreeMap<String, String>,
    pub license: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SkillSource>,
    /// Capabilities it needs from the bot's runtime ([`NEEDS`]).
    #[serde(default)]
    pub needs: Vec<String>,
    /// Connections a bot using it should have (`notion`).
    #[serde(default)]
    pub requires_connection: Vec<String>,
    pub status: SkillStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// It ships code files (whether or not it strictly needs to run them).
    #[serde(default)]
    pub has_scripts: bool,
    pub files: u32,
    pub bytes: u64,
    pub published_by: String,
    pub published_at: i64,
    /// When listed for a particular bot: does it fit that bot ([`skill_fits`])?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fits: Option<bool>,
    /// When listed for a particular bot: the version installed there, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
}

/// One file of a skill being published: `path` is relative to the skill's own
/// folder (`SKILL.md`, `scripts/fill.py`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishSkillFile {
    pub path: String,
    pub content_b64: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishSkill {
    pub id: String,
    pub version: String,
    pub title: std::collections::BTreeMap<String, String>,
    pub summary: std::collections::BTreeMap<String, String>,
    pub license: String,
    #[serde(default)]
    pub source: Option<SkillSource>,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub requires_connection: Vec<String>,
    pub status: SkillStatus,
    #[serde(default)]
    pub condition: Option<String>,
    /// The skill ships scripts but is useful without running them. Without
    /// this, code files and an empty `needs` are refused together: that pair
    /// is how a skill that can't work on a hosted bot would be offered to one.
    #[serde(default)]
    pub scripts_optional: bool,
    #[serde(default)]
    pub files: Vec<PublishSkillFile>,
}

/// File extensions that make a skill "ship code".
pub const SCRIPT_EXTENSIONS: &[&str] = &["py", "sh", "bash", "zsh", "js", "mjs", "cjs", "ts", "rb", "pl", "ps1", "bat", "cmd", "exe"];

pub fn is_script(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, ext)| SCRIPT_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext)))
}

// MARK: - Wire

/// Where a file's current version came from. Decided by the server from who
/// committed it — never taken from the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriveOrigin {
    /// Put there by the account that owns the drive's account (a bot's owner).
    Owner,
    /// Written by the drive's own account (a bot writing its memory).
    Agent,
    /// Installed from the official skill library, pinned to `commit`.
    Library { skill: String, commit: String },
}

/// One file as it is now (or, in a revision list, as it was).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveEntry {
    pub path: String,
    /// The drive revision this version was committed at. Commits name it back
    /// as `expected_rev` to change or delete this exact version.
    pub rev: i64,
    /// Hex sha256 of the bytes. `None` only in a revision list, for a delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    pub origin: DriveOrigin,
    /// Username that committed this version.
    pub author: String,
    /// Unix seconds.
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveQuota {
    pub bytes: u64,
    pub max_bytes: u64,
    pub files: u64,
    pub max_files: u64,
    /// Everything the owning person has across all of their accounts' drives.
    pub owner_bytes: u64,
    pub max_owner_bytes: u64,
}

/// `listDrive`. With `since_rev`, `entries` holds only what changed after it
/// and `removed` the paths deleted after it — a mirror applies both and is
/// current at `rev`.
///
/// `reset: true` means the server no longer has the history to answer "since
/// then" (deletions older than the retention window are compacted away):
/// `entries` is then the WHOLE drive, and a mirror must also drop every local
/// file that isn't in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveListing {
    pub account: String,
    /// Stable short id of this drive — the local mirror's folder name
    /// (`~/.mafold/drives/<id>/`). Survives renames (the drive is keyed by the
    /// account, not its name) and is short on purpose: every character here is
    /// one a Windows path can't spend on the files inside.
    #[serde(default)]
    pub id: String,
    /// Whether a daemon should point its agent's memory at this drive's
    /// `memory/`. False for a bot that already had memory on some machine
    /// until its owner has picked which of those files come along (owner call
    /// 2026-09-29: 没勾完继续用老目录); true for a bot born with its drive.
    #[serde(default)]
    pub memory_mounted: bool,
    pub rev: i64,
    pub entries: Vec<DriveEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
    pub quota: DriveQuota,
}

/// One change inside a commit. `sha: None` deletes the path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveChange {
    pub path: String,
    #[serde(default)]
    pub sha: Option<String>,
    /// The version this change replaces: `None` = the path must not exist yet,
    /// `Some(rev)` = its current version must be `rev`. Every change is a
    /// compare-and-swap, so two machines editing different files never block
    /// each other and two editing the same file never lose a write silently.
    #[serde(default)]
    pub expected_rev: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveCommit {
    pub account: String,
    /// Retrying with the same id returns the first result instead of applying
    /// twice.
    pub op_id: Uuid,
    pub changes: Vec<DriveChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveConflict {
    pub path: String,
    /// What the path is at now; `None` = it doesn't exist.
    pub current_rev: Option<i64>,
}

/// A commit is all or nothing: `applied: false` means not one change landed,
/// and `conflicts` says which expectations were stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveCommitResult {
    pub applied: bool,
    pub rev: i64,
    #[serde(default)]
    pub entries: Vec<DriveEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<DriveConflict>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveFile {
    pub entry: DriveEntry,
    pub content_b64: String,
}

/// Payload of `events.driveChanged`: fetch `listDrive { since_rev }` to catch up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveChanged {
    pub account: String,
    pub rev: i64,
}

// MARK: - Moving an existing bot's memory into its drive

/// One memory file the bot's daemon found in a folder it used before its
/// memory lived in the drive (`.docs/bot-drive-v1.md` §5.6). `path` is
/// relative to that memory folder (`MEMORY.md`, `topics/x.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateFile {
    pub path: String,
    pub sha: String,
    pub size: u64,
    #[serde(default)]
    pub mtime_ms: i64,
    /// Why it can't go into the drive (looks like a credential, not text…);
    /// set by the server, which re-checks every file. None = it may.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// One old memory folder: `dir` is the working directory it belongs to, so
/// the owner recognises which project's memory this is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateSource {
    pub dir: String,
    #[serde(default)]
    pub files: Vec<CandidateFile>,
}

/// `reportDriveCandidates` — the bot, about its own old memory: the whole
/// current set (it replaces the last one).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReportCandidates {
    #[serde(default)]
    pub sources: Vec<CandidateSource>,
}

/// `listDriveCandidates` — what the owner can pick from.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CandidateListing {
    pub memory_mounted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<i64>,
    #[serde(default)]
    pub sources: Vec<CandidateSource>,
}

/// One file the owner ticked: which folder, which file in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AdoptPick {
    pub dir: String,
    pub path: String,
    /// The start of the sha the owner was SHOWN (a card carries it). When
    /// the bot has since reported a newer version, the pick is refused rather
    /// than bringing in text the owner never saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

/// `adoptDriveMemory` — the owner's decision: these files go into
/// `memory/`, and from then on the bot's memory lives in the drive. Picking
/// none is a decision too (start the drive's memory empty).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AdoptMemory {
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub picks: Vec<AdoptPick>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdoptResult {
    pub adopted: u32,
    pub rev: i64,
}

/// The card a bot sends its owner to offer its old memory:
/// `{% mafold/drive-memory bot="…" %}` with one line per folder and file —
///
/// ```text
/// d|/path/of/the/working/dir
/// f|MEMORY.md|1234|9f86d081884c|first words of the file…   (can be ticked; sha start)
/// x|keys.md|300|looks like it contains a credential         (listed, can't)
/// ```
///
/// Written by the daemon ([`offer_body`]) and read back by the SERVER when the
/// owner taps ([`offer_picks`]): the tap sends only the positions of the
/// ticked `f|` lines, so it can name nothing the card didn't show.
pub const OFFER_CARD: &str = "mafold/drive-memory";

/// One line of text, safe inside a card body: no line breaks, no Markdoc tag
/// delimiters, at most `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ").replace("{%", "{ %").replace("%}", "% }");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// Whether a folder or file name survives being one line of an offer card
/// unchanged. The card's text IS what a tap is resolved against, so a name
/// the card would reshape (whitespace runs, tag delimiters, over-long) could
/// be shown but never picked: the daemon doesn't offer it, the server
/// refuses it in a report.
pub fn offer_safe(name: &str) -> bool {
    !name.is_empty() && one_line(name, 400) == name && !name.contains('|')
}

/// Characters of a file's sha an offer card carries.
pub const OFFER_SHA_CHARS: usize = 12;

/// The card body for `sources`. `preview(dir, path)` gives a file's opening
/// words (the daemon has the files); previews are dropped when the list is
/// long, so the card stays a readable size.
pub fn offer_body(sources: &[CandidateSource], preview: impl Fn(&str, &str) -> Option<String>) -> String {
    let count: usize = sources.iter().map(|s| s.files.len()).sum();
    let room = if count > 120 { 0 } else if count > 40 { 60 } else { 140 };
    let mut out = String::new();
    for s in sources {
        out.push_str(&format!("d|{}\n", one_line(&s.dir, 400)));
        for f in &s.files {
            match &f.blocked {
                Some(why) => out.push_str(&format!("x|{}|{}|{}\n", f.path, f.size, one_line(why, 120))),
                None => {
                    let p = if room == 0 { String::new() } else { preview(&s.dir, &f.path).map(|t| one_line(&t, room)).unwrap_or_default() };
                    let sha: String = f.sha.chars().take(OFFER_SHA_CHARS).collect();
                    out.push_str(&format!("f|{}|{}|{sha}|{p}\n", f.path, f.size));
                }
            }
        }
    }
    out
}

/// The body of the last not-yet-answered offer card in `content`.
fn open_offer(content: &str) -> Option<(usize, usize, &str)> {
    let open = format!("{{% {OFFER_CARD}");
    let close = format!("{{% /{OFFER_CARD} %}}");
    let start = content.rfind(&open)?;
    let tag_end = start + content[start..].find("%}")? + 2;
    // The ATTRIBUTE, not the word: a bot may well be called `approver`.
    if content[start..tag_end].contains(" approve=") {
        return None;
    }
    let body_end = tag_end + content[tag_end..].find(&close)?;
    Some((start, tag_end, &content[tag_end..body_end]))
}

/// The tickable files of the offer card in `content`, in card order — what a
/// tap's positions index into. None = no unanswered offer card there.
pub fn offer_picks(content: &str) -> Option<Vec<AdoptPick>> {
    let (_, _, body) = open_offer(content)?;
    let mut dir = String::new();
    let mut out = Vec::new();
    for line in body.lines() {
        if let Some(d) = line.strip_prefix("d|") {
            dir = d.to_string();
        } else if let Some(rest) = line.strip_prefix("f|") {
            let mut it = rest.split('|');
            let path = it.next().unwrap_or_default();
            let sha = it.nth(1).filter(|s| !s.is_empty()).map(str::to_string);
            if !dir.is_empty() && !path.is_empty() {
                out.push(AdoptPick { dir: dir.clone(), path: path.to_string(), sha });
            }
        }
    }
    Some(out)
}

/// The `bot="…"` the unanswered offer card in `content` names.
pub fn offer_bot(content: &str) -> Option<String> {
    let (start, tag_end, _) = open_offer(content)?;
    let tag = &content[start..tag_end];
    let from = tag.find(" bot=\"")? + 6;
    let len = tag[from..].find('"')?;
    Some(tag[from..from + len].to_string())
}

/// `content` with its offer card marked answered (`approve="<n brought in>"`).
pub fn stamp_offer(content: &str, adopted: u32) -> Option<String> {
    let (_, tag_end, _) = open_offer(content)?;
    let mut out = content.to_string();
    out.insert_str(tag_end - 2, &format!("approve=\"{adopted}\" "));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_paths_pass() {
        for p in [
            "memory/MEMORY.md",
            "memory/用户偏好.md",
            "skills/pdf/SKILL.md",
            "skills/docx/scripts/office/schemas/ISO-IEC29500-4_2016/shared-documentPropertiesVariantTypes.xsd",
            "skills/theme-factory/themes/modern-minimalist.md",
            "skills/x/.gitignore",
            "memory/notes v2.md",
        ] {
            assert_eq!(validate_path(p), Ok(()), "{p}");
        }
    }

    #[test]
    fn paths_outside_the_two_areas_are_refused() {
        for p in ["MEMORY.md", "skills", "memory", "other/a.md", "Skills/a.md", "/memory/a.md", "memory/a.md/"] {
            assert!(validate_path(p).is_err(), "{p}");
        }
        assert_eq!(validate_path("notes/a.md"), Err(PathError::Area));
    }

    #[test]
    fn windows_device_names_are_refused_with_any_extension() {
        for p in [
            "memory/nul.md", "memory/CON", "memory/aux.tar.gz", "skills/x/com1.py", "skills/x/LPT9",
            "skills/x/com¹.txt", "memory/nul .md", "skills/prn/SKILL.md",
        ] {
            assert!(matches!(validate_path(p), Err(PathError::Reserved(_))), "{p}");
        }
        // Only the exact stem is a device: these are ordinary names.
        for p in ["memory/null.md", "memory/console.md", "skills/x/com10.txt", "skills/comx/a.md", "memory/nu.l"] {
            assert_eq!(validate_path(p), Ok(()), "{p}");
        }
    }

    #[test]
    fn characters_ntfs_refuses_are_refused() {
        for c in ['<', '>', ':', '"', '\\', '|', '?', '*', '\u{0}', '\u{1f}', '\u{7f}', '\u{202e}', '\u{200b}', '\u{feff}'] {
            let p = format!("memory/a{c}b.md");
            assert_eq!(validate_path(&p), Err(PathError::Char(c)), "{c:?}");
        }
        // The handle separator is the one people will actually hit.
        assert!(validate_path("memory/fei_pota:claude-code.md").is_err());
    }

    #[test]
    fn segment_edges_windows_strips_are_refused() {
        for p in ["memory/a.md.", "memory/a.md ", "memory/ a.md", "memory/dir./a.md", "skills/x /a.md"] {
            assert!(matches!(validate_path(p), Err(PathError::Edge(_))), "{p}");
        }
        for p in ["memory//a.md", "memory/./a.md", "memory/../a.md"] {
            assert!(matches!(validate_path(p), Err(PathError::Segment(_))), "{p}");
        }
    }

    #[test]
    fn length_depth_and_segment_budgets_hold() {
        let ok = format!("memory/{}", "a".repeat(MAX_PATH_UNITS - "memory/".len()));
        assert_eq!(ok.encode_utf16().count(), MAX_PATH_UNITS);
        assert!(matches!(validate_path(&ok), Err(PathError::Segment(_))), "segment is over 64 bytes");
        // "memory/" + 60 + "/" + 60 + "/" = 129 units before the file name.
        let at_limit = format!("memory/{}/{}/{}", "a".repeat(60), "b".repeat(60), "c".repeat(MAX_PATH_UNITS - 129));
        assert_eq!(at_limit.encode_utf16().count(), MAX_PATH_UNITS);
        assert_eq!(validate_path(&at_limit), Ok(()));
        let long = format!("memory/{}/{}/{}", "a".repeat(60), "b".repeat(60), "c".repeat(MAX_PATH_UNITS - 128));
        assert_eq!(validate_path(&long), Err(PathError::TooLong));
        // Counted in UTF-16 units, as Windows counts: every CJK char is one.
        let cjk = format!("memory/{}.md", "记".repeat(20));
        assert_eq!(validate_path(&cjk), Ok(()));
        let deep = "skills/a/b/c/d/e/f/g/h.md";
        assert_eq!(validate_path(deep), Err(PathError::TooDeep));
        assert_eq!(validate_path("skills/a/b/c/d/e/g.md"), Ok(()));
    }

    #[test]
    fn decomposed_names_are_refused_and_normalize_fixes_them() {
        let nfd = "memory/cafe\u{301}.md";
        assert_eq!(validate_path(nfd), Err(PathError::NotNfc));
        let fixed = normalize(nfd);
        assert_eq!(fixed, "memory/caf\u{e9}.md");
        assert_eq!(validate_path(&fixed), Ok(()));
        assert_eq!(normalize("memory\\sub\\a.md"), "memory/sub/a.md");
    }

    #[test]
    fn case_variants_share_one_fold_key() {
        assert_eq!(fold_key("memory/Notes.md"), fold_key("memory/notes.MD"));
        assert_ne!(fold_key("memory/a.md"), fold_key("memory/b.md"));
        assert_eq!(area("skills/pdf/SKILL.md"), Some("skills"));
        assert_eq!(area("memory/MEMORY.md"), Some("memory"));
    }

    #[test]
    fn origin_and_commit_shapes_round_trip() {
        let e = DriveEntry {
            path: "skills/pdf/SKILL.md".into(),
            rev: 3,
            sha: Some("ab".into()),
            size: 2,
            mime: None,
            origin: DriveOrigin::Library { skill: "mafold/pdf".into(), commit: "3337".into() },
            author: "mafold".into(),
            updated_at: 1,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["origin"]["kind"], "library");
        assert_eq!(serde_json::from_value::<DriveEntry>(v).unwrap(), e);
        let c: DriveChange = serde_json::from_str(r#"{"path":"memory/a.md"}"#).unwrap();
        assert_eq!((c.sha, c.expected_rev), (None, None));
    }
}

#[cfg(test)]
mod library_tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// A pure-instructions skill fits every bot; one that runs code fits only
    /// a runtime that can run code. One rule for listing, install and
    /// recommendation.
    #[test]
    fn a_skill_fits_when_the_runtime_provides_everything_it_needs() {
        assert!(skill_fits(&[], &[]));
        assert!(skill_fits(&[], &v(&[NEED_EXEC])));
        assert!(skill_fits(&v(&[NEED_EXEC]), &v(&[NEED_EXEC])));
        assert!(!skill_fits(&v(&[NEED_EXEC]), &[]), "a hosted bot can't run a Python skill");
        assert!(!skill_fits(&v(&["exec", "gpu"]), &v(&["exec"])));
    }

    #[test]
    fn only_redistributable_licenses_are_listed() {
        for l in ["MIT", "Apache-2.0", "BSD-3-Clause"] {
            assert!(REDISTRIBUTABLE_LICENSES.contains(&l));
        }
        for l in ["", "Proprietary", "LicenseRef-Anthropic", "UNLICENSED", "NOASSERTION"] {
            assert!(!REDISTRIBUTABLE_LICENSES.contains(&l), "{l}");
        }
    }

    #[test]
    fn code_files_are_recognised_by_extension() {
        assert!(is_script("scripts/fill_pdf.py"));
        assert!(is_script("run.PS1"));
        assert!(!is_script("SKILL.md"));
        assert!(!is_script("fonts/Inter.ttf"));
        assert!(!is_script("Makefile"));
    }

    /// The offer card round-trips: what the daemon writes is exactly what the
    /// server reads back on a tap — tickable files only, in card order — and a
    /// card, once answered, offers nothing more.
    #[test]
    fn an_offer_card_names_exactly_the_files_it_shows() {
        let f = |p: &str, blocked: Option<&str>| CandidateFile {
            path: p.into(),
            sha: "s".into(),
            size: 10,
            mtime_ms: 0,
            blocked: blocked.map(str::to_string),
        };
        let sources = vec![
            CandidateSource { dir: "/w/app".into(), files: vec![f("MEMORY.md", None), f("keys.md", Some("looks like it contains a credential"))] },
            CandidateSource { dir: "/w/old".into(), files: vec![f("notes/a.md", None)] },
        ];
        let body = offer_body(&sources, |_, p| Some(format!("line one\n{{% html %}} of {p} | with a pipe")));
        assert!(!body.contains("{%"), "a preview can't open a tag inside the card: {body}");
        assert_eq!(body.lines().count(), 5);
        // A bot whose NAME contains the attribute's word is still answerable.
        let msg = format!("Pick what comes with me:\n{{% {OFFER_CARD} bot=\"ada:approver\" %}}\n{body}{{% /{OFFER_CARD} %}}\n");
        assert_eq!(offer_bot(&msg).as_deref(), Some("ada:approver"));
        let picks = offer_picks(&msg).unwrap();
        let s = Some("s".to_string());
        assert_eq!(
            picks,
            [
                AdoptPick { dir: "/w/app".into(), path: "MEMORY.md".into(), sha: s.clone() },
                AdoptPick { dir: "/w/old".into(), path: "notes/a.md".into(), sha: s }
            ]
        );
        let stamped = stamp_offer(&msg, 2).unwrap();
        assert!(stamped.contains("bot=\"ada:approver\" approve=\"2\" %}"), "{stamped}");
        assert!(offer_picks(&stamped).is_none() && stamp_offer(&stamped, 1).is_none(), "answered once");
        assert!(offer_picks("no card here").is_none());
        // Names a card line would reshape can't be offered (the tap could never find them).
        assert!(offer_safe("/Users/ada/My Project") && offer_safe("笔记/周报.md"));
        for bad in ["/w/two  spaces", "/w/x{%y", "/w/a|b", "/w/tab\there", ""] {
            assert!(!offer_safe(bad), "{bad:?}");
        }
        // A long list drops previews rather than growing a huge card.
        let many = vec![CandidateSource { dir: "/w".into(), files: (0..150).map(|i| f(&format!("m{i}.md"), None)).collect() }];
        assert!(offer_body(&many, |_, _| Some("x".repeat(200))).lines().all(|l| l.len() < 40));
    }
}
