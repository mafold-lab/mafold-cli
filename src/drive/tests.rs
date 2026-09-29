use super::*;
use mafold_core::mafold_types::drive::{DriveEntry, DriveOrigin, DriveQuota};
use std::sync::Mutex;

/// A drive server in memory, with the real one's semantics where the mirror
/// depends on them: revisions and `since_rev` deltas.
#[derive(Default)]
struct Fake {
    s: Mutex<FakeState>,
}
#[derive(Default)]
struct FakeState {
    rev: i64,
    /// What an api from before the memory withdrawal (≤ 0.0.182) still said
    /// for a bot whose memory had moved in.
    mounted: bool,
    files: BTreeMap<String, (i64, Vec<u8>)>,
    deleted: BTreeMap<String, i64>,
    reset_next: bool,
    gets: usize,
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
    fn gets(&self) -> usize {
        self.s.lock().unwrap().gets
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
        let mut s = self.s.lock().unwrap();
        s.gets += 1;
        s.files.get(path).map(|(_, b)| b.clone()).context("missing")
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
    let dir = tmp("materialise");
    let m = Mirror::open(f.clone(), &dir, "ada:tea-bot").await.unwrap();
    assert!(m.root().ends_with("0123456789ab"), "{}", m.root());
    assert_eq!(read(&m, "skills/tea/ref/temps.md").as_deref(), Some("80C"));
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("0123456789ab/.claude-plugin/plugin.json")).unwrap()).unwrap();
    assert_eq!(manifest["name"], "tea-bot");
    assert_eq!(mount(Some(m.as_ref())).await.plugin_dirs.last().map(String::as_str), Some(m.root().as_str()));
    // A second daemon start reuses the same state: nothing to fetch.
    let again = Mirror::open(f.clone(), &dir, "ada:tea-bot").await.unwrap();
    assert_eq!(again.pull().await.unwrap(), Pulled::default());
}

/// Drives hold no memory (2026-09-30). An api from before that still lists
/// `memory/` for a bot whose memory had moved in, and says it's mounted: the
/// mirror writes none of it, and nothing about the agent's memory changes.
#[tokio::test]
async fn memory_an_older_api_still_lists_never_reaches_the_disk() {
    let f = Arc::new(Fake::default());
    f.s.lock().unwrap().mounted = true;
    f.put("skills/tea/SKILL.md", "t");
    f.put("memory/MEMORY.md", "moved in");
    let dir = tmp("old-api-memory");
    let m = Mirror::open(f.clone(), &dir, "ada:bot").await.unwrap();
    assert_eq!(read(&m, "skills/tea/SKILL.md").as_deref(), Some("t"));
    assert!(!m.local("memory").exists());
    assert_eq!(f.gets(), 1, "only the skill was fetched");
    let s = std::fs::read_to_string(dir.join("0123456789ab").join(STATE)).unwrap();
    assert!(!s.contains("memory"), "{s}");
}

#[tokio::test]
async fn server_changes_arrive_and_a_local_skill_edit_is_put_back() {
    let f = Arc::new(Fake::default());
    f.put("skills/tea/SKILL.md", "v1");
    f.put("skills/tea/old.md", "old");
    let dir = tmp("changes");
    let m = Mirror::open(f.clone(), &dir, "ada:bot").await.unwrap();
    f.put("skills/tea/SKILL.md", "v2");
    f.del("skills/tea/old.md");
    let p = m.pull().await.unwrap();
    assert!(p.skills);
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

/// A mirror left by cli 0.9.130–0.9.133 — memory moved in (`memory_mounted`,
/// `memory/…` in `files`) and, from 0.9.133, the withdrawn old-memory offer's
/// `dirs`/`reported`/`offered` — still reads as THIS mirror's state (an
/// unreadable one starts over and refetches the whole drive). Its memory is
/// forgotten by the mirror but left on disk: the user's files, maybe the only
/// copy of something.
#[tokio::test]
async fn a_mirror_that_held_memory_reopens_without_it_and_keeps_the_files() {
    let f = Arc::new(Fake::default());
    f.put("skills/tea/SKILL.md", "t");
    let dir = tmp("held-memory");
    let m = Mirror::open(f.clone(), &dir, "ada:bot").await.unwrap();
    drop(m);
    let state = dir.join("0123456789ab").join(STATE);
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    v["memory_mounted"] = json!(true);
    v["files"]["memory/MEMORY.md"] = json!({ "rev": 1, "sha": sha_hex(b"mine"), "size": 4, "mtime_ms": 0 });
    v["dirs"] = json!(["/Users/someone/project"]);
    v["reported"] = json!("8f411cd4");
    v["offered"] = json!(["0fadc5fa", 1790700906]);
    std::fs::write(&state, serde_json::to_vec(&v).unwrap()).unwrap();
    let mem = dir.join("0123456789ab").join("memory").join("MEMORY.md");
    std::fs::create_dir_all(mem.parent().unwrap()).unwrap();
    std::fs::write(&mem, "mine").unwrap();

    let gets = f.gets();
    let m = Mirror::open(f.clone(), &dir, "ada:bot").await.unwrap();
    assert_eq!(f.gets(), gets, "the state was read: nothing refetched");
    assert_eq!(read(&m, "skills/tea/SKILL.md").as_deref(), Some("t"));
    assert_eq!(std::fs::read_to_string(&mem).unwrap(), "mine", "left on disk");
    f.put("skills/tea/SKILL.md", "t2");
    m.pull().await.unwrap();
    let s = std::fs::read_to_string(&state).unwrap();
    for gone in ["memory", "dirs", "reported", "offered"] {
        assert!(!s.contains(gone), "{gone}: {s}");
    }
    assert!(mem.exists(), "a pull doesn't touch it either");
}

/// Uninstalling a skill takes its folder with it, not just the files: an
/// empty `skills/<slug>/examples/` shell left behind looks like a broken
/// skill. Every way a file leaves the mirror (a server delete, the sweep of a
/// full listing) prunes only the folders it emptied — never one still holding
/// something, never the area root.
#[tokio::test]
async fn a_removed_file_leaves_no_empty_folders_behind() {
    let f = Arc::new(Fake::default());
    f.put("skills/comms/SKILL.md", "s");
    f.put("skills/comms/examples/a.md", "a");
    f.put("skills/comms/examples/b.md", "b");
    f.put("skills/tea/SKILL.md", "t");
    let dir = tmp("prune");
    let m = Mirror::open(f.clone(), &dir, "ada:bot").await.unwrap();
    f.del("skills/comms/examples/a.md");
    m.pull().await.unwrap();
    assert!(m.local("skills/comms/examples/b.md").exists(), "a folder still holding a file stays");
    // Uninstall = the server deletes every file of the skill.
    f.del("skills/comms/SKILL.md");
    f.del("skills/comms/examples/b.md");
    m.pull().await.unwrap();
    assert!(!m.local("skills/comms").exists(), "the uninstalled skill's folder is gone");
    assert!(m.local("skills/tea/SKILL.md").exists());
    // A stray swept by a full listing goes with its folder…
    touch(&m, "skills/tea/stray/deep/x.md", "x");
    f.s.lock().unwrap().reset_next = true;
    m.pull().await.unwrap();
    assert!(!m.local("skills/tea/stray").exists());
    assert!(m.local("skills/tea/SKILL.md").exists());
    // The last skill going empties `skills/` — the root itself stays.
    f.del("skills/tea/SKILL.md");
    m.pull().await.unwrap();
    assert!(!m.local("skills/tea").exists());
    assert!(m.local("skills").is_dir());
}

/// Empty folders already left under `skills/` — by a daemon before the prune
/// above existed (cli 0.9.130), or made by hand — go when the mirror opens:
/// `skills/` is the server's, so an empty folder there is never anybody's
/// work in progress. Anything outside it is left alone.
#[tokio::test]
async fn opening_the_mirror_clears_empty_skill_folders_left_from_before() {
    let f = Arc::new(Fake::default());
    f.put("skills/tea/SKILL.md", "t");
    let dir = tmp("stale-empty");
    let m = Mirror::open(f.clone(), &dir, "ada:bot").await.unwrap();
    for d in ["skills/comms/examples", "skills/tea/empty", "memory/drafts"] {
        std::fs::create_dir_all(m.local(d)).unwrap();
    }
    let m = Mirror::open(f.clone(), &dir, "ada:bot").await.unwrap();
    assert!(!m.local("skills/comms").exists(), "the shell a 0.9.130 uninstall left");
    assert!(!m.local("skills/tea/empty").exists());
    assert!(m.local("skills/tea/SKILL.md").exists());
    assert!(m.local("memory/drafts").is_dir(), "not the mirror's to sweep");
}

#[test]
fn a_process_with_another_mount_is_another_pool_key() {
    use crate::harness::{cc_conn::PoolKey, Mount};
    let k = || PoolKey::new("c", "s", "/w", None, None, None, None, &[]);
    let none = Mount::default();
    let skills = Mount { plugin_dirs: vec!["/d".into()] };
    let two = Mount { plugin_dirs: vec!["/m".into(), "/d".into()] };
    assert_eq!(k().with_mount(&skills), k().with_mount(&skills));
    assert_ne!(k().with_mount(&none), k().with_mount(&skills));
    assert_ne!(k().with_mount(&skills), k().with_mount(&two));
}

#[test]
fn plugin_labels_are_plain_and_never_mafold() {
    assert_eq!(plugin_label("opsdu:claude-code"), "claude-code");
    assert_eq!(plugin_label("fei_pota:Study Bot"), "study-bot");
    assert_eq!(plugin_label("x:mafold"), "mafold-bot");
    assert_eq!(plugin_label("x:__"), "bot");
}
