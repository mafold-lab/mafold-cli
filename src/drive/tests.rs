use super::*;
use mafold_core::mafold_types::drive::{DriveConflict, DriveEntry, DriveOrigin, DriveQuota};
use std::sync::Mutex;

/// A drive server in memory, with the real one's semantics where the mirror
/// depends on them: revisions, `since_rev` deltas, per-file compare-and-swap.
#[derive(Default)]
struct Fake {
    s: Mutex<FakeState>,
}
#[derive(Default)]
struct FakeState {
    rev: i64,
    mounted: bool,
    files: BTreeMap<String, (i64, Vec<u8>)>,
    deleted: BTreeMap<String, i64>,
    blobs: BTreeMap<String, Vec<u8>>,
    commits: usize,
    reset_next: bool,
}
impl Fake {
    fn put(&self, path: &str, body: &str) {
        let mut s = self.s.lock().unwrap();
        s.rev += 1;
        let r = s.rev;
        s.deleted.remove(path);
        s.files.insert(path.into(), (r, body.as_bytes().to_vec()));
    }
    fn del(&self, path: &str) {
        let mut s = self.s.lock().unwrap();
        s.rev += 1;
        let r = s.rev;
        s.files.remove(path);
        s.deleted.insert(path.into(), r);
    }
    fn body(&self, path: &str) -> Option<String> {
        self.s.lock().unwrap().files.get(path).map(|(_, b)| String::from_utf8_lossy(b).into_owned())
    }
}
fn entry(path: &str, rev: i64, b: &[u8]) -> DriveEntry {
    DriveEntry {
        path: path.into(),
        rev,
        sha: Some(sha_hex(b)),
        size: b.len() as u64,
        mime: None,
        origin: DriveOrigin::Owner,
        author: "x".into(),
        updated_at: 0,
    }
}
#[async_trait]
impl DriveApi for Fake {
    async fn list(&self, since: Option<i64>) -> Result<DriveListing> {
        let mut s = self.s.lock().unwrap();
        let reset = std::mem::take(&mut s.reset_next);
        let since = if reset { None } else { since };
        let cut = since.unwrap_or(-1);
        Ok(DriveListing {
            account: "a:bot".into(),
            id: "0123456789ab".into(),
            memory_mounted: s.mounted,
            rev: s.rev,
            entries: s.files.iter().filter(|(_, (r, _))| *r > cut).map(|(p, (r, b))| entry(p, *r, b)).collect(),
            removed: match since {
                None => vec![],
                Some(c) => s.deleted.iter().filter(|(_, r)| **r > c).map(|(p, _)| p.clone()).collect(),
            },
            reset,
            quota: DriveQuota { bytes: 0, max_bytes: 0, files: 0, max_files: 0, owner_bytes: 0, max_owner_bytes: 0 },
        })
    }
    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        self.s.lock().unwrap().files.get(path).map(|(_, b)| b.clone()).context("missing")
    }
    async fn have(&self, shas: &[String]) -> Result<Vec<String>> {
        let s = self.s.lock().unwrap();
        Ok(shas.iter().filter(|x| s.blobs.contains_key(*x)).cloned().collect())
    }
    async fn upload(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha_hex(bytes);
        self.s.lock().unwrap().blobs.insert(sha.clone(), bytes.to_vec());
        Ok(sha)
    }
    async fn commit(&self, changes: Vec<DriveChange>) -> Result<DriveCommitResult> {
        let mut s = self.s.lock().unwrap();
        let conflicts: Vec<DriveConflict> = changes
            .iter()
            .filter_map(|c| {
                let cur = s.files.get(&c.path).map(|(r, _)| *r);
                (cur != c.expected_rev).then(|| DriveConflict { path: c.path.clone(), current_rev: cur })
            })
            .collect();
        if !conflicts.is_empty() {
            return Ok(DriveCommitResult { applied: false, rev: s.rev, entries: vec![], conflicts });
        }
        s.rev += 1;
        s.commits += 1;
        let r = s.rev;
        let mut entries = vec![];
        for c in changes {
            match c.sha {
                Some(sha) => {
                    let b = s.blobs.get(&sha).cloned().context("not uploaded")?;
                    entries.push(entry(&c.path, r, &b));
                    s.deleted.remove(&c.path);
                    s.files.insert(c.path, (r, b));
                }
                None => {
                    s.files.remove(&c.path);
                    s.deleted.insert(c.path.clone(), r);
                    entries.push(DriveEntry { sha: None, size: 0, ..entry(&c.path, r, b"") });
                }
            }
        }
        Ok(DriveCommitResult { applied: true, rev: r, entries, conflicts: vec![] })
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mafold-drive-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn read(m: &Mirror, rel: &str) -> Option<String> {
    std::fs::read_to_string(m.local(rel)).ok()
}
fn touch(m: &Mirror, rel: &str, body: &str) {
    let p = m.local(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    // Distinct mtime from whatever the mirror recorded.
    std::thread::sleep(std::time::Duration::from_millis(15));
    std::fs::write(p, body).unwrap();
}

#[tokio::test]
async fn a_mirror_materialises_the_drive_as_a_plugin_folder() {
    let f = Arc::new(Fake::default());
    f.put("skills/tea/SKILL.md", "---\nname: tea\n---");
    f.put("skills/tea/ref/temps.md", "80C");
    f.put("memory/MEMORY.md", "facts");
    let dir = tmp("materialise");
    let m = Mirror::open(f.clone(), &dir, "ada:tea-bot", Some("ada".into())).await.unwrap();
    assert!(m.root().ends_with("0123456789ab"), "{}", m.root());
    assert_eq!(read(&m, "skills/tea/ref/temps.md").as_deref(), Some("80C"));
    assert_eq!(read(&m, "memory/MEMORY.md").as_deref(), Some("facts"));
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("0123456789ab/.claude-plugin/plugin.json")).unwrap()).unwrap();
    assert_eq!(manifest["name"], "tea-bot");
    // Memory isn't the agent's until the owner has moved it.
    assert_eq!(m.memory_dir().await, None);
    f.s.lock().unwrap().mounted = true;
    m.pull().await.unwrap();
    assert!(m.memory_dir().await.unwrap().ends_with("memory"));
    // A second daemon start reuses the same state: nothing to fetch.
    let again = Mirror::open(f.clone(), &dir, "ada:tea-bot", None).await.unwrap();
    assert_eq!(again.pull().await.unwrap(), Pulled::default());
}

#[tokio::test]
async fn server_changes_arrive_and_a_local_skill_edit_is_put_back() {
    let f = Arc::new(Fake::default());
    f.put("skills/tea/SKILL.md", "v1");
    f.put("skills/tea/old.md", "old");
    let dir = tmp("changes");
    let m = Mirror::open(f.clone(), &dir, "ada:bot", None).await.unwrap();
    f.put("skills/tea/SKILL.md", "v2");
    f.del("skills/tea/old.md");
    let p = m.pull().await.unwrap();
    assert!(p.skills && !p.memory);
    assert_eq!(read(&m, "skills/tea/SKILL.md").as_deref(), Some("v2"));
    assert!(read(&m, "skills/tea/old.md").is_none());
    // The agent (or someone) edits a skill on disk: skills are the server's.
    touch(&m, "skills/tea/SKILL.md", "hacked");
    touch(&m, "skills/tea/planted.md", "planted");
    f.s.lock().unwrap().reset_next = true; // a full listing also sweeps strays
    m.pull().await.unwrap();
    assert_eq!(read(&m, "skills/tea/SKILL.md").as_deref(), Some("v2"));
    assert!(read(&m, "skills/tea/planted.md").is_none());
}

#[tokio::test]
async fn memory_changes_go_up_as_compare_and_swap() {
    let f = Arc::new(Fake::default());
    f.s.lock().unwrap().mounted = true;
    f.put("memory/MEMORY.md", "one");
    f.put("memory/old.md", "old");
    let dir = tmp("push");
    let m = Mirror::open(f.clone(), &dir, "ada:bot", None).await.unwrap();
    assert_eq!(m.push_memory().await.unwrap(), 0, "nothing changed yet");
    touch(&m, "memory/MEMORY.md", "one, two");
    touch(&m, "memory/topic.md", "new file");
    std::fs::remove_file(m.local("memory/old.md")).unwrap();
    // A name no Windows mirror could hold is not sent (and not lost locally).
    touch(&m, "memory/nul.md", "nope");
    assert_eq!(m.push_memory().await.unwrap(), 3);
    assert_eq!(f.body("memory/MEMORY.md").as_deref(), Some("one, two"));
    assert_eq!(f.body("memory/topic.md").as_deref(), Some("new file"));
    assert!(f.body("memory/old.md").is_none());
    assert!(f.body("memory/nul.md").is_none());
    assert!(read(&m, "memory/nul.md").is_some());
    // Pushed files aren't pushed again.
    let commits = f.s.lock().unwrap().commits;
    assert_eq!(m.push_memory().await.unwrap(), 0);
    assert_eq!(f.s.lock().unwrap().commits, commits);
    // And coming back down in the next pull changes nothing.
    assert_eq!(m.pull().await.unwrap(), Pulled::default());
}

#[tokio::test]
async fn a_file_changed_on_both_sides_keeps_both() {
    let f = Arc::new(Fake::default());
    f.s.lock().unwrap().mounted = true;
    f.put("memory/MEMORY.md", "base");
    let dir = tmp("conflict");
    let m = Mirror::open(f.clone(), &dir, "ada:bot", None).await.unwrap();
    // Another machine running the same bot got there first.
    f.put("memory/MEMORY.md", "theirs");
    touch(&m, "memory/MEMORY.md", "ours");
    // The push is refused (stale version) → pull keeps ours as a conflict copy →
    // the retry sends that copy up as a new file.
    assert_eq!(m.push_memory().await.unwrap(), 1);
    assert_eq!(read(&m, "memory/MEMORY.md").as_deref(), Some("theirs"));
    let s = f.s.lock().unwrap();
    let copies: Vec<_> = s.files.iter().filter(|(p, _)| p.starts_with("memory/MEMORY.conflict-")).collect();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].1 .1, b"ours");
}

#[tokio::test]
async fn nothing_is_pushed_while_memory_is_not_the_drives() {
    let f = Arc::new(Fake::default());
    let dir = tmp("unmounted");
    let m = Mirror::open(f.clone(), &dir, "ada:bot", None).await.unwrap();
    touch(&m, "memory/MEMORY.md", "local only");
    assert_eq!(m.push_memory().await.unwrap(), 0);
    assert!(f.body("memory/MEMORY.md").is_none());
}

#[tokio::test]
async fn a_turn_that_was_not_the_owners_is_undone() {
    let f = Arc::new(Fake::default());
    f.s.lock().unwrap().mounted = true;
    f.put("memory/MEMORY.md", "truth");
    f.put("memory/keep.md", "keep");
    let dir = tmp("revert");
    let m = Mirror::open(f.clone(), &dir, "ada:bot", Some("ada".into())).await.unwrap();
    assert!(m.is_owner("ada") && m.is_owner("@ADA") && !m.is_owner("mallory"));
    touch(&m, "memory/MEMORY.md", "poisoned");
    touch(&m, "memory/planted.md", "planted");
    std::fs::remove_file(m.local("memory/keep.md")).unwrap();
    assert_eq!(m.revert_memory().await.unwrap(), 3);
    assert_eq!(read(&m, "memory/MEMORY.md").as_deref(), Some("truth"));
    assert_eq!(read(&m, "memory/keep.md").as_deref(), Some("keep"));
    assert!(read(&m, "memory/planted.md").is_none());
    // …and nothing of it reaches the server.
    assert_eq!(m.push_memory().await.unwrap(), 0);
    assert_eq!(f.body("memory/MEMORY.md").as_deref(), Some("truth"));
}

/// The hook that keeps a non-owner turn out of the bot's memory: refuses a
/// write into the guarded folder however the path is spelled, lets everything
/// else through, and says nothing at all on a turn that isn't guarded.
#[test]
fn the_memory_guard_refuses_writes_into_memory_and_nothing_else() {
    let g = "/Users/ada/.mafold/drives/0123456789ab/memory";
    let deny = |p: &str| guard_response(&json!({ "file_path": p }), g).is_some();
    assert!(deny("/Users/ada/.mafold/drives/0123456789ab/memory/MEMORY.md"));
    assert!(deny("/Users/ada/.mafold/drives/0123456789ab/memory/sub/x.md"));
    assert!(deny("/users/ADA/.mafold/drives/0123456789ab/Memory/MEMORY.md"), "case-insensitive filesystems");
    assert!(guard_response(&json!({ "notebook_path": format!("{g}/n.ipynb") }), g).is_some());
    assert!(!deny("/Users/ada/.mafold/drives/0123456789ab/memory-notes.md"), "a sibling that merely starts the same");
    assert!(!deny("/Users/ada/project/src/main.rs"));
    // Windows: backslashes and the `\\?\` form canonicalize hands back.
    let wg = r"C:\Users\ada\.mafold\drives\0123456789ab\memory";
    assert!(guard_response(&json!({ "file_path": r"\\?\C:\Users\ada\.mafold\drives\0123456789ab\memory\MEMORY.md" }), wg).is_some());
    // Unguarded turn (the owner's): no opinion, whatever the path.
    assert!(guard_response(&json!({ "file_path": format!("{g}/MEMORY.md") }), "").is_none());
    let out = guard_response(&json!({ "file_path": format!("{g}/MEMORY.md") }), g).unwrap();
    assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn a_process_with_another_mount_is_another_pool_key() {
    use crate::harness::{cc_conn::PoolKey, Mount};
    let k = || PoolKey::new("c", "s", "/w", None, None, None, None, &[]);
    let none = Mount::default();
    let skills = Mount { plugin_dirs: vec!["/d".into()], memory_dir: None };
    let memory = Mount { plugin_dirs: vec!["/d".into()], memory_dir: Some("/d/memory".into()) };
    assert_eq!(k().with_mount(&skills), k().with_mount(&skills));
    assert_ne!(k().with_mount(&none), k().with_mount(&skills));
    assert_ne!(k().with_mount(&skills), k().with_mount(&memory), "memory moving into the drive needs a new process");
    assert_eq!(memory.settings(r#"{"hooks":{}}"#), r#"{"autoMemoryDirectory":"/d/memory","hooks":{}}"#);
    assert_eq!(skills.settings(r#"{"hooks":{}}"#), r#"{"hooks":{}}"#);
}

#[test]
fn plugin_labels_are_plain_and_never_mafold() {
    assert_eq!(plugin_label("opsdu:claude-code"), "claude-code");
    assert_eq!(plugin_label("fei_pota:Study Bot"), "study-bot");
    assert_eq!(plugin_label("x:mafold"), "mafold-bot");
    assert_eq!(plugin_label("x:__"), "bot");
}
