//! The one OpenCode build Mafold runs (Mafold Harness), and how it gets here.
//!
//! Every other runtime is the vendor's own install on the user's PATH, signed
//! in to the user's own account. OpenCode is driven differently: Mafold runs it
//! on the owner's wallet, with a key that lives for one reply (`super::opencode`),
//! so the program that holds that key is ours to choose. One version, checked
//! against a hash written here, in a folder of its own
//! (`~/.mafold/runtimes/opencode/<version>/`) that an OpenCode the user installed
//! themselves never shares. The server API and event stream `super::opencode`
//! speaks were measured against this build; a newer OpenCode is a new pin, not
//! a silent upgrade.
//!
//! Where it comes from: OpenCode's own per-platform npm package
//! (`opencode-<os>-<arch>`, the binary at `package/bin/opencode[.exe]`), from
//! whichever of registry.npmjs.org and registry.npmmirror.com (reachable from
//! China) starts delivering first.
//! Each tarball must match the package's published `dist.integrity` (SHA-512)
//! before anything in it is unpacked, and the unpacked binary must say it is
//! this version before it is put in place.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The pinned OpenCode release.
pub const VERSION: &str = "1.19.5";

/// One platform's build of [`VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    /// `<os>-<arch>` in npm's words (`darwin-arm64`, `windows-x64`).
    pub target: &'static str,
    /// The npm package holding the binary.
    pub package: &'static str,
    /// The package tarball's `dist.integrity` as npm publishes it.
    pub integrity: &'static str,
    /// Where the binary sits inside the tarball.
    pub path: &'static str,
}

/// Every platform OpenCode ships [`VERSION`] for that Mafold runs on. The
/// integrity values are npm's own for each tarball (`npm view
/// opencode-<target>@1.19.5 dist.integrity`, 10-30); the linux-x64 binary was
/// also compared byte for byte with the GitHub release.
pub const BUILDS: &[Build] = &[
    Build {
        target: "darwin-arm64",
        package: "opencode-darwin-arm64",
        integrity: "sha512-0MbSbKkmwVHtgP31GJyyECe6QVy4T7W4Wp7s2TsmXHTPPyf01ne3dWSX97X9dkNK28Lc6kt+zMPQUP/9GgDLZg==",
        path: "package/bin/opencode",
    },
    Build {
        target: "darwin-x64",
        package: "opencode-darwin-x64",
        integrity: "sha512-+DO7IMPTCrX12UJXnL4pL3ckcwR6BR6f6tA9QIlVQ5nZx66T5U7FRMJT4n5rsOnJxLM/pZ2vfX1G8vrnvXO8FA==",
        path: "package/bin/opencode",
    },
    Build {
        target: "linux-x64",
        package: "opencode-linux-x64",
        integrity: "sha512-i1qh7VooLGML9xX6ud93y0JA8j/1aPeDYRKjsmYXrJX1TjIWHgBBMkEbEJPZthpWbsJg//XfBgR6Kb7+NPaJiQ==",
        path: "package/bin/opencode",
    },
    Build {
        target: "linux-arm64",
        package: "opencode-linux-arm64",
        integrity: "sha512-u2HFRI6v1nNMVKiV34An8XrGBxJeZ+RCg2tYu3SqlGODKqKsr5RoEbaX1tnsfGb+rETh9AJVX9kS8dSZXPufsA==",
        path: "package/bin/opencode",
    },
    Build {
        target: "windows-x64",
        package: "opencode-windows-x64",
        integrity: "sha512-xCVbrA8A/qQTAdBIKyhK5tx3/mcq18yF82Mi8phry0MJb2631nnnpxF9m6FFfWAVHyer4qFGrO7hbWXesHQ1Gg==",
        path: "package/bin/opencode.exe",
    },
];

/// Registries asked at once (`race`), then in this order. Both serve the same
/// tarball at the same path, so the pinned hash is the trust root, not the
/// registry.
pub const REGISTRIES: &[&str] = &["https://registry.npmjs.org", "https://registry.npmmirror.com"];

/// Tries per registry: a dropped connection is common enough on an 80 MB
/// download that one failure shouldn't give up on a registry. The tries take
/// turns ([`download`]): a failed attempt goes to the next registry before
/// the same one is asked again.
const TRIES_PER_REGISTRY: usize = 2;

/// A download that goes quiet this long is dead, however long the whole may
/// take. Short: a registry that stalls is a registry to leave (from China,
/// npmjs.org does), and there is another one to go to.
const READ_STALL: Duration = Duration::from_secs(20);

/// The most one attempt at the tarball may take.
const ATTEMPT_LIMIT: Duration = Duration::from_secs(15 * 60);

/// How much of the tarball a registry must deliver to be the one the install
/// goes on with ([`race`]): enough to tell a working link from one that only
/// answers.
const RACE_BYTES: usize = 1 << 20;

/// The slowest a download may go once its first MB is in, on average over
/// each `window`, before it counts as stalled and the next registry is
/// asked. [`READ_STALL`] only catches a link that goes quiet; from China,
/// registry.npmjs.org can win the race with a quick first MB and then
/// trickle at 30–80 KB/s without ever going quiet — 80 MB at that pace runs
/// past `mafold add`'s 20-minute cap before the mirror is ever tried.
#[derive(Debug, Clone, Copy)]
struct Floor {
    bytes_per_sec: u64,
    window: Duration,
}

/// 200 KB/s over 30 s: 80 MB in under seven minutes at the slowest pace
/// kept, and a trickling registry left within about 30 s of its first MB.
const FLOOR: Floor = Floor { bytes_per_sec: 200_000, window: Duration::from_secs(30) };

impl Floor {
    /// `bytes` came in `spent`: an error when that is under the floor.
    fn check(&self, bytes: u64, spent: Duration) -> Result<()> {
        let rate = bytes as f64 / spent.as_secs_f64().max(1e-3);
        if rate < self.bytes_per_sec as f64 {
            bail!(
                "slower than {} KB/s over {} s ({:.0} KB/s) — left it for the next registry",
                self.bytes_per_sec / 1000,
                self.window.as_secs(),
                rate / 1000.0
            );
        }
        Ok(())
    }
}

/// How far a download has got: bytes so far, and the whole when the registry
/// said (`content-length`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub got: u64,
    pub total: Option<u64>,
}

impl Progress {
    /// `34/79 MB` — `0.8/3.0 MB` under ten, `34 MB` with no total.
    pub fn mb(&self) -> String {
        let total = self.total.filter(|t| *t > 0);
        let big = total.unwrap_or(self.got) >= 10_000_000;
        let mb = |b: u64| if big { format!("{:.0}", b as f64 / 1e6) } else { format!("{:.1}", b as f64 / 1e6) };
        match total {
            Some(t) => format!("{}/{} MB", mb(self.got.min(t)), mb(t)),
            None => format!("{} MB", mb(self.got)),
        }
    }

    /// Which step of the download this is: its quarter (0–3) when the total
    /// is known, else each 20 MB. What says something once per step reads it.
    pub fn step(&self) -> u64 {
        match self.total.filter(|t| *t > 0) {
            Some(t) => (self.got.saturating_mul(4) / t).min(3),
            None => self.got / 20_000_000,
        }
    }
}

/// The download running for each pinned root right now, for whoever waits on
/// it: a turn's reply, `mafold add`'s terminal (see [`progress_in`]).
static PROGRESS: std::sync::Mutex<Vec<(PathBuf, Progress)>> = std::sync::Mutex::new(Vec::new());

fn set_progress(root: &Path, p: Option<Progress>) {
    let mut all = PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(r, _)| r != root);
    if let Some(p) = p {
        all.push((root.to_path_buf(), p));
    }
}

/// How far the install of the pinned build under `root` has got, while one
/// runs in this process.
pub fn progress_in(root: &Path) -> Option<Progress> {
    PROGRESS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(r, _)| r == root).map(|(_, p)| *p)
}

/// This machine's npm target, when OpenCode ships one.
pub fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("darwin-arm64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("darwin-x64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("linux-arm64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("windows-x64")
    } else {
        None
    }
}

/// The build for `target`.
pub fn build_for(target: &str) -> Option<&'static Build> {
    BUILDS.iter().find(|b| b.target == target)
}

/// The build for this machine, or why there is none.
pub fn this_build() -> Result<&'static Build> {
    let t = target().context(format!(
        "OpenCode {VERSION} has no build for this computer ({} {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))?;
    build_for(t).with_context(|| format!("no pinned OpenCode {VERSION} build for {t}"))
}

/// Where `b` is fetched from on `registry`.
pub fn tarball_url(registry: &str, b: &Build) -> String {
    format!("{registry}/{p}/-/{p}-{VERSION}.tgz", p = b.package)
}

/// `~/.mafold/runtimes/opencode` — everything Mafold's OpenCode keeps.
pub fn root() -> PathBuf {
    crate::platform::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mafold")
        .join("runtimes")
        .join("opencode")
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "opencode.exe"
    } else {
        "opencode"
    }
}

/// The pinned binary under `root`.
pub fn binary_in(root: &Path) -> PathBuf {
    root.join(VERSION).join(exe_name())
}

/// The pinned binary — the only OpenCode Mafold ever starts.
pub fn binary() -> PathBuf {
    binary_in(&root())
}

/// Is the pinned build installed here?
///
/// Its version is [`VERSION`] by construction — nothing is renamed into that
/// folder that didn't match the pinned hash and say so itself (`place`) — so
/// whoever reports it reads it here instead of starting a 200 MB binary
/// every heartbeat, which would also make OpenCode's folders in the user's
/// home (see `version_of`).
pub fn installed() -> bool {
    super::launchable(&binary())
}

/// One agent's own OpenCode home under `root` (its config, sessions, caches),
/// named so any bot name is a safe single path component.
pub fn agent_home_in(root: &Path, bot: &str) -> PathBuf {
    let safe: String = bot
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    let safe = safe.trim_matches('.').to_string();
    root.join("home").join(if safe.is_empty() { "default".to_string() } else { safe })
}

/// Where a pinned build lives and where it is fetched from: this computer's
/// ([`Source::here`]), or a scratch root and a stand-in registry (tests).
#[derive(Debug, Clone)]
pub struct Source {
    pub root: PathBuf,
    pub build: Build,
    pub registries: Vec<String>,
}

/// One install (or repair) at a time in this process: two turns that both
/// find the binary missing share one download instead of racing two. Across
/// processes each stages its own copy and the rename into place is atomic,
/// so the loser only replaces the file with the same bytes.
fn one() -> &'static tokio::sync::Mutex<()> {
    static ONE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    ONE.get_or_init(Default::default)
}

impl Source {
    /// This computer's pinned build, from the real registries.
    pub fn here() -> Result<Self> {
        Ok(Self { root: root(), build: *this_build()?, registries: REGISTRIES.iter().map(|r| r.to_string()).collect() })
    }

    pub fn binary(&self) -> PathBuf {
        binary_in(&self.root)
    }

    fn registries(&self) -> Vec<&str> {
        self.registries.iter().map(String::as_str).collect()
    }

    /// The pinned binary, installed first when it isn't here yet.
    pub async fn ensure(&self) -> Result<PathBuf> {
        let bin = self.binary();
        if super::launchable(&bin) {
            return Ok(bin);
        }
        let _one = one().lock().await;
        if super::launchable(&bin) {
            return Ok(bin);
        }
        install(&self.root, &self.build, &self.registries()).await
    }

    /// See [`repair_in`].
    pub async fn repair(&self) -> Result<Option<PathBuf>> {
        repair_in(&self.root, &self.build, &self.registries()).await
    }
}

/// The pinned binary, installed first when it isn't here yet.
pub async fn ensure() -> Result<PathBuf> {
    Source::here()?.ensure().await
}

/// Where an install records the SHA-256 of the binary it put in place:
/// beside it.
pub fn digest_path(bin: &Path) -> PathBuf {
    let mut name = bin.file_name().unwrap_or_default().to_os_string();
    name.push(".sha256");
    bin.with_file_name(name)
}

/// The SHA-256 of the file at `p`, as lowercase hex.
fn sha256_of(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(p).with_context(|| format!("couldn't read {}", p.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Is the binary at `bin` still the bytes its install recorded
/// ([`digest_path`])? `false` when it was changed or cut short — and when
/// there is no record, as nothing can vouch for it then.
///
/// Not asked on every start ([`installed`] only asks that the file is
/// there): hashing 200 MB costs a second or so, and is only worth it once the
/// binary has failed to start.
pub fn intact(bin: &Path) -> bool {
    let Ok(want) = std::fs::read_to_string(digest_path(bin)) else { return false };
    sha256_of(bin).is_ok_and(|got| got == want.trim())
}

/// After the pinned binary under `root` wouldn't start: install it again when
/// it isn't the bytes its install recorded. `Ok(None)`: it is, so whatever
/// stopped it is something else and a new copy wouldn't help; `Ok(Some)`:
/// the binary in place now is the pinned one (installed again here, or by
/// another turn a moment ago).
pub async fn repair_in(root: &Path, b: &Build, registries: &[&str]) -> Result<Option<PathBuf>> {
    let bin = binary_in(root);
    let check = || {
        let bin = bin.clone();
        async move { tokio::task::spawn_blocking(move || intact(&bin)).await.unwrap_or(false) }
    };
    if check().await {
        return Ok(None);
    }
    let _one = one().lock().await;
    if check().await {
        return Ok(Some(bin));
    }
    println!("⚠️  OpenCode at {} isn't the build its install recorded — installing it again", bin.display());
    install(root, b, registries).await.map(Some)
}

/// [`ensure`] for a caller that isn't async (`mafold install`, the supervisor's
/// one-click install), stopped once it has run for `limit`. On a thread of its
/// own with its own runtime, because the caller may itself be running on one.
///
/// Says how far the download is as it goes: rewriting one line at a
/// terminal, a line per quarter otherwise (the supervisor's log).
pub fn ensure_blocking(limit: Option<Duration>) -> Result<PathBuf> {
    std::thread::spawn(move || -> Result<PathBuf> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("couldn't start the installer's runtime")?;
        rt.block_on(async move {
            let install = async {
                match limit {
                    Some(limit) => tokio::time::timeout(limit, ensure()).await.map_err(|_| {
                        anyhow::anyhow!("the download was still running after {} minutes — stopped it", limit.as_secs() / 60)
                    })?,
                    None => ensure().await,
                }
            };
            let shown = std::sync::atomic::AtomicBool::new(false);
            let out = tokio::select! {
                out = install => out,
                _ = print_progress(root(), &shown) => unreachable!("never ends"),
            };
            if shown.load(std::sync::atomic::Ordering::Relaxed) && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                println!();
            }
            out
        })
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the installer thread panicked"))?
}

/// The terminal's (or the log's) line for an install under way.
pub fn progress_line(p: Progress) -> String {
    format!("  downloading OpenCode {VERSION} — {}", p.mb())
}

/// Print how far the install under `root` has got, until dropped; `shown`
/// once anything was.
async fn print_progress(root: PathBuf, shown: &std::sync::atomic::AtomicBool) {
    use std::io::{IsTerminal, Write};
    let tty = std::io::stdout().is_terminal();
    let mut told: Option<u64> = None;
    loop {
        tokio::time::sleep(Duration::from_millis(if tty { 500 } else { 2000 })).await;
        let Some(p) = progress_in(&root) else { continue };
        shown.store(true, std::sync::atomic::Ordering::Relaxed);
        if tty {
            print!("\r{}   ", progress_line(p));
            let _ = std::io::stdout().flush();
        } else if told != Some(p.step()) {
            told = Some(p.step());
            println!("{}", progress_line(p));
        }
    }
}

/// `mafold add <name> --harness opencode`: put the pinned build in place now,
/// at the terminal where its progress shows, before the daemon is registered
/// — so the agent's first reply starts at once instead of downloading 80 MB
/// first. Not having it is not fatal: the first reply installs it then.
pub fn install_before_add(harness: Option<&str>) {
    if !harness.is_some_and(runs_pinned) || installed() {
        return;
    }
    println!("Installing OpenCode {VERSION} for Mafold Harness — a one-time download of about 80 MB, checked against its pinned SHA-512.");
    match ensure_blocking(Some(Duration::from_secs(20 * 60))) {
        Ok(bin) => println!("✓ OpenCode {VERSION} installed ({})", bin.display()),
        Err(e) => eprintln!("⚠️  couldn't install OpenCode {VERSION} now ({e:#}) — the agent's first reply will try again."),
    }
}

/// Is `harness` the one that runs this pinned build?
fn runs_pinned(harness: &str) -> bool {
    super::select(harness).id() == "opencode"
}

/// Fetch `b` from the first of `registries` that serves the pinned bytes and
/// install it under `root`. Nothing is left under `root` but the finished
/// binary: every download is staged in a private folder that goes on every
/// exit path.
pub async fn install(root: &Path, b: &Build, registries: &[&str]) -> Result<PathBuf> {
    std::fs::create_dir_all(root).with_context(|| format!("couldn't create {}", root.display()))?;
    sweep_stages(root);
    let stage = Stage::new(root)?;
    let tgz = stage.0.join("opencode.tgz");
    {
        // Published for whoever waits on this install (`progress_in`), and
        // taken back however the download ends.
        struct Reporting<'a>(&'a Path);
        impl Drop for Reporting<'_> {
            fn drop(&mut self) {
                set_progress(self.0, None);
            }
        }
        let _reporting = Reporting(root);
        download(b, registries, &tgz, &|p| set_progress(root, Some(p)), FLOOR).await?;
    }
    let (root, b) = (root.to_path_buf(), *b);
    // The stage moves into the blocking task: a future dropped while `place`
    // runs (Stop mid-install) leaves that task unpacking into the folder, so
    // the folder must go when the task is done with it, not before.
    tokio::task::spawn_blocking(move || {
        let placed = place(&root, &b, &stage.0, &tgz);
        drop(stage);
        placed
    })
    .await
    .context("the install task failed")?
}

/// The staging folders installs of this process are using right now — the
/// ones [`sweep_stages`] must never take, though their pid is running.
static HELD: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// A staging folder no other install, in this process or another, shares —
/// removed when this drops, however the install ends: done, failed, or its
/// future dropped mid-download (a Stop during the first reply's install,
/// `ensure_blocking`'s timeout, the daemon going down). Up to ~300 MB (the
/// tarball and the unpacked binary) would otherwise stay behind for good.
struct Stage(PathBuf);

impl Stage {
    fn new(root: &Path) -> Result<Self> {
        let path = root.join(format!(".stage-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        // Held before it exists, so a sweep in another thread never sees it unheld.
        HELD.lock().unwrap_or_else(|p| p.into_inner()).push(path.clone());
        let stage = Self(path);
        std::fs::create_dir_all(&stage.0).with_context(|| format!("couldn't create {}", stage.0.display()))?;
        Ok(stage)
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        HELD.lock().unwrap_or_else(|p| p.into_inner()).retain(|p| *p != self.0);
    }
}

/// Take the staging folders of installs that are gone: a process killed
/// mid-download never ran [`Stage`]'s drop. Gone = its pid isn't running, or
/// it is this process's pid and no install here holds it (an exec-based
/// self-update keeps the pid). Another process's live install keeps its own.
fn sweep_stages(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let held = HELD.lock().unwrap_or_else(|p| p.into_inner()).clone();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(".stage-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        let path = e.path();
        let gone = if pid == std::process::id() { !held.contains(&path) } else { !crate::platform::pid_alive(pid) };
        if gone && std::fs::remove_dir_all(&path).is_ok() {
            println!("🧹 removed {}, left by an OpenCode install that is gone", path.display());
        }
    }
}

/// Download `b`'s tarball to `to`: first from whichever registry starts
/// delivering first ([`race`]), then from each in turn, until one serves
/// bytes that match the pinned integrity. A mismatch is a failed source like
/// any other — the next registry may serve the real thing — and what every
/// source said is in the error when none did. `report` hears how far it is.
///
/// The registries take turns: a failed attempt (cut off, gone quiet, under
/// the speed `floor`, the wrong bytes) goes to the next one before the same
/// one is asked again. A link that won the race and then went bad — the
/// China case — loses about 30 s, not two whole tries.
async fn download(b: &Build, registries: &[&str], to: &Path, report: &(dyn Fn(Progress) + Sync), floor: Floor) -> Result<()> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(READ_STALL)
        .user_agent(concat!("mafold-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("couldn't build the download client")?;
    let urls: Vec<String> = registries.iter().map(|r| tarball_url(r, b)).collect();
    let mut failures: Vec<String> = Vec::new();
    // The winner goes first, already under way; the rest after it in their
    // order, then round again. With no winner (every registry refused or
    // stalled) each is still tried in turn: a link can come back.
    let mut head = match race(&http, &urls, RACE_BYTES).await {
        Ok(h) => Some(h),
        Err(e) => {
            failures.push(format!("no registry started the download ({e:#})"));
            None
        }
    };
    let first = head.as_ref().map(|h| h.index);
    if let Some(i) = first {
        // Which link actually worked, for whoever reads the log later.
        println!("OpenCode {VERSION}: downloading from {}", registries[i]);
    }
    let order: Vec<usize> = first.into_iter().chain((0..urls.len()).filter(|i| Some(*i) != first)).collect();
    for attempt in 1..=TRIES_PER_REGISTRY {
        for &i in &order {
            let url = &urls[i];
            let started = head.take_if(|h| h.index == i);
            let got = match tokio::time::timeout(ATTEMPT_LIMIT, fetch(&http, url, to, started, report, floor)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("still downloading after {} minutes", ATTEMPT_LIMIT.as_secs() / 60)),
            };
            match got.and_then(|digest| check_integrity(&digest, b.integrity)) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let _ = std::fs::remove_file(to);
                    // The log says where the install went next, as it said
                    // where it began.
                    println!("OpenCode {VERSION}: {} (try {attempt}) failed: {e:#}", registries[i]);
                    failures.push(format!("{url} (try {attempt}): {e:#}"));
                }
            }
        }
    }
    bail!("couldn't download OpenCode {VERSION} ({}): {}", b.target, failures.join("; "))
}

/// A download one registry has begun: the response, what it has sent, and
/// the size it said the whole is — read before any of the body, because
/// reqwest's `content_length` shrinks as the body is read.
struct Head {
    index: usize,
    resp: reqwest::Response,
    bytes: Vec<u8>,
    total: Option<u64>,
}

/// Ask every registry at once; the first to deliver `enough` bytes (or its
/// whole tarball, if smaller) wins, and the others are dropped mid-request.
/// npmjs.org first, the mirror second was a fixed order that cost a link to
/// China up to two read limits per try before the mirror was ever asked;
/// whichever link is actually working wins here in the time its first MB
/// takes. The pinned hash still decides what is installed, so which registry
/// it came from doesn't matter.
async fn race(http: &reqwest::Client, urls: &[String], enough: usize) -> Result<Head> {
    use futures_util::stream::{FuturesUnordered, StreamExt};
    let mut runners: FuturesUnordered<_> = urls
        .iter()
        .enumerate()
        .map(|(index, url)| async move {
            let begun = async {
                let mut resp = http.get(url).send().await?.error_for_status()?;
                let total = resp.content_length();
                let mut bytes = Vec::new();
                while bytes.len() < enough {
                    match resp.chunk().await? {
                        Some(c) => bytes.extend_from_slice(&c),
                        None => break,
                    }
                }
                anyhow::Ok(Head { index, resp, bytes, total })
            };
            begun.await.map_err(|e| format!("{url}: {e:#}"))
        })
        .collect();
    let mut why = Vec::new();
    while let Some(r) = runners.next().await {
        match r {
            Ok(h) => return Ok(h),
            Err(e) => why.push(e),
        }
    }
    bail!("{}", if why.is_empty() { "no registries to ask".to_string() } else { why.join("; ") })
}

/// Stream `url` into `to` — or go on with the download [`race`] began —
/// hashing as it goes; the SHA-512 of what was written. Once the first MB
/// is in, each `floor.window` must bring the floor's worth, or the attempt
/// ends there.
async fn fetch(
    http: &reqwest::Client,
    url: &str,
    to: &Path,
    begun: Option<Head>,
    report: &(dyn Fn(Progress) + Sync),
    floor: Floor,
) -> Result<[u8; 64]> {
    use sha2::{Digest, Sha512};
    let (mut resp, first, total) = match begun {
        Some(h) => (h.resp, h.bytes, h.total),
        None => {
            let resp = http.get(url).send().await?.error_for_status()?;
            let total = resp.content_length();
            (resp, Vec::new(), total)
        }
    };
    let mut file = std::fs::File::create(to).with_context(|| format!("couldn't write {}", to.display()))?;
    let mut hash = Sha512::new();
    let mut progress = Progress { got: 0, total };
    let mut take = |chunk: &[u8], progress: &mut Progress| -> Result<()> {
        hash.update(chunk);
        file.write_all(chunk)?;
        progress.got += chunk.len() as u64;
        report(*progress);
        Ok(())
    };
    take(&first, &mut progress)?;
    // The floor's current window: when it began, and how much was in by
    // then. Opened once the first MB is in (the race already judged how
    // long that took); judged when it is up, whether or not a chunk came.
    let mut window: Option<(tokio::time::Instant, u64)> = None;
    loop {
        if window.is_none() && progress.got >= RACE_BYTES as u64 {
            window = Some((tokio::time::Instant::now(), progress.got));
        }
        let next = match window {
            // Up with nothing new in: the check below says what that was.
            Some((since, _)) => tokio::time::timeout_at(since + floor.window, resp.chunk()).await.ok(),
            None => Some(resp.chunk().await),
        };
        if let Some(chunk) = next {
            match chunk? {
                Some(chunk) => take(&chunk, &mut progress)?,
                None => break,
            }
        }
        if let Some((since, from)) = window {
            let spent = since.elapsed();
            if spent >= floor.window {
                floor.check(progress.got - from, spent)?;
                window = Some((tokio::time::Instant::now(), progress.got));
            }
        }
    }
    file.sync_all()?;
    Ok(hash.finalize().into())
}

/// Does `digest` match an npm `sha512-<base64>` integrity string?
fn check_integrity(digest: &[u8; 64], integrity: &str) -> Result<()> {
    use base64::Engine;
    let want = integrity
        .strip_prefix("sha512-")
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .context("the pinned integrity isn't a sha512 value")?;
    if want.as_slice() != digest.as_slice() {
        bail!(
            "the download doesn't match the pinned SHA-512 (got sha512-{}) — refused it",
            base64::engine::general_purpose::STANDARD.encode(digest)
        );
    }
    Ok(())
}

/// Unpack the verified tarball `tgz` in `stage`, check the binary is
/// [`VERSION`], and move it into place under `root` — chmodded before the
/// rename, because once it lands another turn may start it.
fn place(root: &Path, b: &Build, stage: &Path, tgz: &Path) -> Result<PathBuf> {
    let status = crate::platform::std_command("tar")
        .arg("-xzf")
        .arg(tgz)
        .arg("-C")
        .arg(stage)
        .arg(b.path)
        .status()
        .context("couldn't run `tar` to unpack OpenCode")?;
    if !status.success() {
        bail!("`tar` couldn't unpack {} from the OpenCode package ({status})", b.path);
    }
    let staged = stage.join(b.path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let said = version_of(&staged, &stage.join("xdg")).context("the unpacked OpenCode wouldn't start")?;
    if said != VERSION {
        bail!("the unpacked OpenCode says it is {said:?}, not {VERSION}");
    }
    // On disk before it has the pinned name: only the tarball was synced, so
    // a power loss soon after the rename could leave a cut-short binary in
    // place — which `installed` trusts, and which would then fail every reply.
    std::fs::File::open(&staged).and_then(|f| f.sync_all()).context("couldn't write the unpacked OpenCode to disk")?;
    let bin = binary_in(root);
    let dir = bin.parent().expect("the binary has a folder");
    std::fs::create_dir_all(dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    // Its digest goes in first, so a binary in place always has one to be
    // checked against when it won't start (`repair_in`).
    let record = stage.join("opencode.sha256");
    std::fs::write(&record, sha256_of(&staged)?)
        .and_then(|_| std::fs::File::open(&record)?.sync_all())
        .context("couldn't record the unpacked OpenCode's digest")?;
    std::fs::rename(&record, digest_path(&bin)).with_context(|| format!("couldn't record OpenCode's digest in {}", dir.display()))?;
    if let Err(e) = std::fs::rename(&staged, &bin) {
        // Another install got there first and its copy may already be
        // running — which Windows won't rename over. Same pinned bytes.
        if super::launchable(&bin) {
            return Ok(bin);
        }
        return Err(e).with_context(|| format!("couldn't move OpenCode into {}", bin.display()));
    }
    // The renames themselves, durable.
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(bin)
}

/// What `bin --version` prints, trimmed. Even `--version` makes OpenCode
/// create its config, data, cache and state folders, so `scratch` stands in
/// for all four — never the user's own `~/.config/opencode` and the rest.
fn version_of(bin: &Path, scratch: &Path) -> Result<String> {
    let mut cmd = crate::platform::std_command(bin);
    cmd.arg("--version").stdin(std::process::Stdio::null());
    for (var, dir) in [("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")] {
        cmd.env(var, scratch.join(dir));
    }
    // And its `~/.opencode` (see `opencode::opencode_env`).
    cmd.env("OPENCODE_TEST_HOME", scratch);
    let out = crate::platform::output_of_new_binary(&mut cmd)?;
    if !out.status.success() {
        bail!("`--version` exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mafold-oc-pin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Every platform Mafold ships on has its build, each pinned to a real
    /// SHA-512 and fetched from the version's own tarball.
    #[test]
    fn every_target_has_a_pinned_build() {
        for t in ["darwin-arm64", "darwin-x64", "linux-x64", "linux-arm64", "windows-x64"] {
            let b = build_for(t).unwrap_or_else(|| panic!("no build for {t}"));
            assert_eq!(b.package, format!("opencode-{t}"));
            let digest = b.integrity.strip_prefix("sha512-").expect("an npm sha512 integrity");
            use base64::Engine;
            assert_eq!(base64::engine::general_purpose::STANDARD.decode(digest).unwrap().len(), 64, "{t}");
            let exe = if t.starts_with("windows") { "package/bin/opencode.exe" } else { "package/bin/opencode" };
            assert_eq!(b.path, exe, "{t}");
        }
        assert_eq!(BUILDS.len(), 5);
        if let Some(t) = target() {
            assert!(build_for(t).is_some(), "this machine ({t}) has a build");
        }
        assert_eq!(
            tarball_url(REGISTRIES[1], build_for("linux-x64").unwrap()),
            "https://registry.npmmirror.com/opencode-linux-x64/-/opencode-linux-x64-1.19.5.tgz"
        );
        assert_eq!(REGISTRIES[0], "https://registry.npmjs.org", "npm first, the mirror second");
    }

    /// The binary lives under its version, and each agent's home beside it —
    /// one path component per bot, whatever its name.
    #[test]
    fn the_layout_keeps_one_version_and_one_home_per_agent() {
        let r = Path::new("/h/.mafold/runtimes/opencode");
        let exe = if cfg!(windows) { "opencode.exe" } else { "opencode" };
        assert_eq!(binary_in(r), r.join("1.19.5").join(exe));
        assert_eq!(agent_home_in(r, "ops_wallet-bot"), r.join("home/ops_wallet-bot"));
        assert_eq!(agent_home_in(r, "ops:wallet/../x"), r.join("home/ops_wallet_.._x"));
        assert_eq!(agent_home_in(r, ".."), r.join("home/default"));
        assert_eq!(agent_home_in(r, ""), r.join("home/default"));
        assert!(root().ends_with(".mafold/runtimes/opencode"));
        assert_eq!(binary(), binary_in(&root()));
    }

    #[test]
    fn the_integrity_check_takes_only_the_pinned_bytes() {
        use sha2::{Digest, Sha512};
        let digest: [u8; 64] = Sha512::digest(b"the real tarball").into();
        use base64::Engine;
        let pinned = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(digest));
        assert!(check_integrity(&digest, &pinned).is_ok());
        let other: [u8; 64] = Sha512::digest(b"something else").into();
        let e = check_integrity(&other, &pinned).unwrap_err().to_string();
        assert!(e.contains("doesn't match") && e.contains("refused"), "{e}");
        assert!(check_integrity(&digest, "sha256-abc").is_err(), "only sha512 is a pin");
    }

    /// A tarball shaped like OpenCode's npm package, holding a stand-in binary
    /// that prints `says` for `--version`.
    #[cfg(unix)]
    pub(crate) fn fake_package(dir: &Path, says: &str) -> (Vec<u8>, String) {
        use base64::Engine;
        use sha2::{Digest, Sha512};
        use std::os::unix::fs::PermissionsExt;
        let pkg = dir.join("pkg");
        std::fs::create_dir_all(pkg.join("package/bin")).unwrap();
        let bin = pkg.join("package/bin/opencode");
        std::fs::write(&bin, format!("#!/bin/sh\necho {says}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tgz = dir.join("pkg.tgz");
        let ok = crate::platform::std_command("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&pkg)
            .arg("package")
            .status()
            .unwrap();
        assert!(ok.success());
        let bytes = std::fs::read(&tgz).unwrap();
        let integrity = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&bytes)));
        (bytes, integrity)
    }

    /// A registry on this machine serving `body` at every path, until dropped.
    #[cfg(unix)]
    pub(crate) async fn serve(body: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let task = tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        (url, task)
    }

    /// The pin is the trust root: a tarball that isn't the pinned one is
    /// refused before it is unpacked, and nothing is left installed — no
    /// binary, no version folder, no staging folder.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_tarball_that_isnt_the_pinned_one_installs_nothing() {
        let dir = temp("mismatch");
        let (bytes, _) = fake_package(&dir, VERSION);
        let (url, task) = serve(bytes).await;
        let root = dir.join("root");
        let b = Build { integrity: build_for("linux-x64").unwrap().integrity, ..*build_for("linux-x64").unwrap() };
        let e = install(&root, &b, &[url.as_str()]).await.unwrap_err();
        task.abort();
        let said = format!("{e:#}");
        assert!(said.contains("doesn't match the pinned SHA-512"), "{said}");
        assert_eq!(said.matches("(try ").count(), TRIES_PER_REGISTRY, "each try is named: {said}");
        assert!(!binary_in(&root).exists());
        assert!(!root.join(VERSION).exists());
        let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.is_empty(), "nothing left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pinned bytes install: unpacked, version-checked, in place and
    /// executable — and the next registry is only asked when one fails.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_pinned_tarball_installs_into_its_version_folder() {
        let dir = temp("ok");
        let (bytes, integrity) = fake_package(&dir, VERSION);
        let (url, task) = serve(bytes).await;
        let root = dir.join("root");
        let b = Build { target: "test", package: "opencode-test", integrity: Box::leak(integrity.into_boxed_str()), path: "package/bin/opencode" };
        // A registry that refuses the connection first: the second serves it.
        let dead = "http://127.0.0.1:9";
        let bin = install(&root, &b, &[dead, url.as_str()]).await.expect("installs from the second registry");
        task.abort();
        assert_eq!(bin, binary_in(&root));
        assert!(super::super::launchable(&bin));
        assert_eq!(version_of(&bin, &dir.join("scratch")).unwrap(), VERSION);
        let left: Vec<String> =
            std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, vec![VERSION.to_string()], "only the version folder remains");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A registry that starts the tarball and then goes quiet, holding the
    /// connection open: an install is left mid-download.
    #[cfg(unix)]
    async fn serve_stalled(first: Vec<u8>, claimed: usize) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let head = format!("HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {claimed}\r\n\r\n");
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&first).await;
                open.push(s);
            }
        });
        (url, task)
    }

    fn stages_in(root: &Path) -> Vec<String> {
        std::fs::read_dir(root)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(".stage-")).collect())
            .unwrap_or_default()
    }

    /// An install whose future is dropped mid-download — Stop during the
    /// first reply's download, `ensure_blocking`'s timeout, a daemon going
    /// down — takes its staging folder with it instead of leaving up to
    /// ~300 MB behind for good.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_install_dropped_mid_download_leaves_no_stage_behind() {
        let dir = temp("dropped");
        let root = dir.join("root");
        // More than the race's first MB, so the download is writing its file.
        let (url, server) = serve_stalled(vec![7u8; 2 << 20], 80 * 1024 * 1024).await;
        let b = *build_for("linux-x64").unwrap();
        let task = tokio::spawn({
            let root = root.clone();
            async move { install(&root, &b, &[url.as_str()]).await }
        });
        let mut downloading = false;
        for _ in 0..100 {
            if stages_in(&root).iter().any(|s| root.join(s).join("opencode.tgz").metadata().is_ok_and(|m| m.len() > 0)) {
                downloading = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(downloading, "the download got under way: {:?}", stages_in(&root));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        server.abort();
        assert_eq!(stages_in(&root), Vec::<String>::new(), "the stage went with the install");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What an install that is gone left behind (killed mid-download, a
    /// crash; or this very process before an exec-based update, which keeps
    /// its pid) is swept when the next install starts. A live install's
    /// stage, and everything that isn't a stage, is left alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn stale_stages_are_swept_when_an_install_starts() {
        let dir = temp("sweep");
        let root = dir.join("root");
        let mut gone = crate::platform::std_command("true").spawn().unwrap();
        gone.wait().unwrap();
        let dead = gone.id();
        let live = std::os::unix::process::parent_id();
        let me = std::process::id();
        for name in [format!(".stage-{dead}-aa"), format!(".stage-{me}-bb"), format!(".stage-{live}-cc")] {
            std::fs::create_dir_all(root.join(&name)).unwrap();
            std::fs::write(root.join(&name).join("opencode.tgz"), b"partial").unwrap();
        }
        std::fs::create_dir_all(root.join("home/ops_wallet")).unwrap();
        let (bytes, _) = fake_package(&dir, VERSION);
        let (url, server) = serve(bytes).await;
        // Refused (not the pinned bytes): the sweep happens before the download.
        let b = *build_for("linux-x64").unwrap();
        assert!(install(&root, &b, &[url.as_str()]).await.is_err());
        server.abort();
        assert_eq!(stages_in(&root), vec![format!(".stage-{live}-cc")], "only a live install's stage stays");
        assert!(root.join("home/ops_wallet").is_dir(), "an agent's home is not a stage");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The installed binary carries the SHA-256 it was put in place with, and
    /// a damaged one (cut short by a power loss, overwritten) is installed
    /// again by `repair_in` — once; an intact one is left as it is.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_damaged_pinned_binary_is_installed_again() {
        use sha2::{Digest, Sha256};
        let dir = temp("damaged");
        let (bytes, integrity) = fake_package(&dir, VERSION);
        let (url, server) = serve(bytes).await;
        let root = dir.join("root");
        let b = Build { target: "test", package: "opencode-test", integrity: Box::leak(integrity.into_boxed_str()), path: "package/bin/opencode" };
        let bin = install(&root, &b, &[url.as_str()]).await.unwrap();
        let good = std::fs::read(&bin).unwrap();
        let recorded = std::fs::read_to_string(digest_path(&bin)).expect("the install recorded its digest");
        assert_eq!(recorded.trim(), format!("{:x}", Sha256::digest(&good)));
        assert!(intact(&bin));
        assert_eq!(repair_in(&root, &b, &[url.as_str()]).await.unwrap(), None, "an intact binary isn't reinstalled");

        std::fs::write(&bin, &good[..good.len() / 2]).unwrap();
        assert!(!intact(&bin));
        assert_eq!(repair_in(&root, &b, &[url.as_str()]).await.unwrap(), Some(bin.clone()));
        assert_eq!(std::fs::read(&bin).unwrap(), good, "the pinned bytes are back");
        assert!(intact(&bin));

        // No record at all (put there by hand, or a build before records):
        // it can't be vouched for, so it is installed again too.
        std::fs::remove_file(digest_path(&bin)).unwrap();
        assert!(!intact(&bin));
        assert_eq!(repair_in(&root, &b, &[url.as_str()]).await.unwrap(), Some(bin.clone()));
        assert!(intact(&bin));
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A registry that answers `delay` late and then sends `body` in `pieces`
    /// parts, `every` apart — a slow link.
    #[cfg(unix)]
    pub(crate) async fn serve_slowly(body: Vec<u8>, delay: Duration, pieces: usize, every: Duration) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let task = tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    for part in body.chunks(body.len().div_ceil(pieces.max(1)).max(1)) {
                        if s.write_all(part).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(every).await;
                    }
                });
            }
        });
        (url, task)
    }

    /// From China, registry.npmjs.org can stall or crawl for as long as the
    /// read limit allows, twice, before the mirror was ever asked — about 30
    /// minutes of a reply that said nothing. The registry that starts
    /// delivering first is the one the install goes on with, whichever comes
    /// first in the list: here the first one stalls with its connection open,
    /// and the install still finishes at once from the second.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_registry_that_delivers_first_is_the_one_used() {
        let dir = temp("race");
        let (bytes, integrity) = fake_package(&dir, VERSION);
        let (stalled, stall_task) = serve_stalled(Vec::new(), bytes.len()).await;
        let (good, good_task) = serve(bytes).await;
        let b = Build { target: "test", package: "opencode-test", integrity: Box::leak(integrity.into_boxed_str()), path: "package/bin/opencode" };
        let root = dir.join("root");
        let t0 = std::time::Instant::now();
        let got = tokio::time::timeout(Duration::from_secs(10), install(&root, &b, &[stalled.as_str(), good.as_str()])).await;
        stall_task.abort();
        good_task.abort();
        let bin = got.expect("not held up by the stalled registry").expect("installed from the one that delivered");
        assert!(super::super::launchable(&bin));
        println!("installed in {:?} past a stalled first registry", t0.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both registries are held to the same pinned SHA-512: the one that
    /// delivers first but serves other bytes is refused, and the slower one
    /// with the pinned bytes installs.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_fastest_registry_is_still_held_to_the_pin() {
        let dir = temp("race-pin");
        let (bytes, integrity) = fake_package(&dir, VERSION);
        let (fast_wrong, a) = serve(b"not the pinned tarball".to_vec()).await;
        let (slow_right, b_task) = serve_slowly(bytes.clone(), Duration::from_millis(300), 2, Duration::from_millis(50)).await;
        let b = Build { target: "test", package: "opencode-test", integrity: Box::leak(integrity.into_boxed_str()), path: "package/bin/opencode" };
        let root = dir.join("root");
        let bin = install(&root, &b, &[slow_right.as_str(), fast_wrong.as_str()]).await.expect("the pinned bytes install");
        a.abort();
        b_task.abort();
        assert!(super::super::launchable(&bin));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A registry that goes quiet is given up on within 20 s of its last
    /// byte, not a minute (and twice that per registry).
    #[test]
    fn a_quiet_download_is_dead_within_twenty_seconds() {
        assert!(READ_STALL <= Duration::from_secs(20), "{READ_STALL:?}");
        // And a crawling one within 30 s of its first MB; at the slowest pace
        // kept, the ~80 MB tarball still fits `mafold add`'s 20 minutes.
        assert!(FLOOR.window <= Duration::from_secs(30) && FLOOR.bytes_per_sec >= 200_000, "{FLOOR:?}");
        const { assert!(80_000_000 / FLOOR.bytes_per_sec < 20 * 60 / 2, "80 MB at the floor fits half of mafold add's 20 minutes") };
    }

    /// The pick itself: whichever registry gets its first `enough` bytes in
    /// first, in either order, with what it sent kept for the download to go
    /// on from; and every registry's reason when none does.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_race_picks_the_registry_that_delivers_first() {
        let body: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        let (slow, a) = serve_slowly(body.clone(), Duration::from_millis(800), 1, Duration::ZERO).await;
        let (fast, b) = serve(body.clone()).await;
        let http = reqwest::Client::new();
        for (urls, want) in [([slow.clone(), fast.clone()], 1), ([fast.clone(), slow.clone()], 0)] {
            let h = race(&http, &urls, 100_000).await.unwrap();
            assert_eq!(h.index, want, "{urls:?}");
            assert!(h.bytes.len() >= 100_000 && body.starts_with(&h.bytes));
        }
        // Smaller than `enough`: the whole tarball wins.
        let h = race(&http, std::slice::from_ref(&fast), 10 << 20).await.unwrap();
        assert_eq!(h.bytes, body);
        a.abort();
        b.abort();
        let Err(e) = race(&http, &["http://127.0.0.1:9".to_string(), "http://127.0.0.1:9/x".to_string()], 1).await else {
            panic!("nobody listens there");
        };
        let said = format!("{e:#}");
        assert!(said.contains("http://127.0.0.1:9: ") && said.contains("http://127.0.0.1:9/x: "), "each one's reason: {said}");
    }

    /// A registry that answers `delay` late, sends the first `fast` bytes of
    /// `body` at once and the rest `piece` bytes at a time, `every` apart — a
    /// link that slows down after its first MB. Counts the connections it
    /// takes.
    #[cfg(unix)]
    async fn serve_paced(
        body: Vec<u8>,
        delay: Duration,
        fast: usize,
        piece: usize,
        every: Duration,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>, tokio::task::JoinHandle<()>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    tokio::time::sleep(delay).await;
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let fast = fast.min(body.len());
                    if s.write_all(head.as_bytes()).await.is_err() || s.write_all(&body[..fast]).await.is_err() {
                        return;
                    }
                    for part in body[fast..].chunks(piece.max(1)) {
                        tokio::time::sleep(every).await;
                        if s.write_all(part).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (url, hits, task)
    }

    /// A build pinned to `body`'s own SHA-512, for a download that is never
    /// unpacked.
    fn pinned_to(body: &[u8]) -> Build {
        use base64::Engine;
        use sha2::{Digest, Sha512};
        let integrity = format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(body)));
        Build { target: "test", package: "opencode-test", integrity: Box::leak(integrity.into_boxed_str()), path: "package/bin/opencode" }
    }

    /// From China, registry.npmjs.org can win the race with a quick first MB
    /// and then trickle — never quiet long enough for `READ_STALL` — and the
    /// install stayed on it for two 15-minute tries: `mafold add`'s 20-minute
    /// cap ran out before the mirror was ever asked. Under the speed floor
    /// (the real one, 200 KB/s over 30 s) the trickling winner gives way to
    /// the other registry about 30 s after its first MB, and is not asked
    /// again before it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_winner_that_trickles_after_its_first_mb_gives_way_to_the_other() {
        use std::sync::atomic::Ordering;
        let dir = temp("trickle");
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(7) ^ (i >> 9)) as u8).collect();
        let b = pinned_to(&body);
        // Its first MB at once, then 20 KB/s: 100 s more for the rest.
        let (trickle, trickle_hits, a) = serve_paced(body.clone(), Duration::ZERO, RACE_BYTES, 2_000, Duration::from_millis(100)).await;
        // A second late to answer, so it loses the race; then all of it.
        let (late, _, c) = serve_paced(body.clone(), Duration::from_secs(1), usize::MAX, 1, Duration::ZERO).await;
        let to = dir.join("opencode.tgz");
        let t0 = std::time::Instant::now();
        let got =
            tokio::time::timeout(Duration::from_secs(45), download(&b, &[trickle.as_str(), late.as_str()], &to, &|_| {}, FLOOR)).await;
        let took = t0.elapsed();
        a.abort();
        c.abort();
        got.expect("held up by the trickling registry").expect("downloaded from the other one");
        println!("gave way to the other registry in {took:?}");
        assert_eq!(std::fs::read(&to).unwrap(), body);
        assert!(took < FLOOR.window + Duration::from_secs(5), "gave way in {took:?}");
        assert_eq!(trickle_hits.load(Ordering::SeqCst), 1, "the trickling registry isn't asked again before the other");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An attempt that fails sends the install to the next registry before
    /// the same one is tried again: the race's winner, the other, then each
    /// once more — and in list order when no registry won the race.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_attempt_goes_to_the_other_registry_before_trying_again() {
        let dir = temp("alternate");
        let b = pinned_to(b"the pinned tarball");
        let other = b"not the pinned tarball".to_vec();
        // Both serve other bytes; the first answers late, so the second wins the race.
        let (slow, _, a) = serve_paced(other.clone(), Duration::from_millis(300), usize::MAX, 1, Duration::ZERO).await;
        let (fast, _, c) = serve_paced(other, Duration::ZERO, usize::MAX, 1, Duration::ZERO).await;
        let to = dir.join("opencode.tgz");
        let e = download(&b, &[slow.as_str(), fast.as_str()], &to, &|_| {}, FLOOR).await.unwrap_err();
        a.abort();
        c.abort();
        let said = format!("{e:#}");
        let at = |reg: &str, attempt: usize| {
            said.find(&format!("{} (try {attempt})", tarball_url(reg, &b))).unwrap_or_else(|| panic!("no try {attempt} of {reg}: {said}"))
        };
        let tried = [at(&fast, 1), at(&slow, 1), at(&fast, 2), at(&slow, 2)];
        assert!(tried.windows(2).all(|w| w[0] < w[1]), "the winner, the other, then each again: {said}");

        // Nobody listening, so no winner: list order, taking turns.
        let (one, two) = ("http://127.0.0.1:9", "http://127.0.0.1:9/mirror");
        let e = download(&b, &[one, two], &to, &|_| {}, FLOOR).await.unwrap_err();
        let said = format!("{e:#}");
        let at = |reg: &str, attempt: usize| {
            said.find(&format!("{} (try {attempt})", tarball_url(reg, &b))).unwrap_or_else(|| panic!("no try {attempt} of {reg}: {said}"))
        };
        let tried = [at(one, 1), at(two, 1), at(one, 2), at(two, 2)];
        assert!(tried.windows(2).all(|w| w[0] < w[1]), "each in turn: {said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both registries trickling after their first MB: each is left at the
    /// floor in turn, and the install ends with an error naming both and
    /// why, instead of crawling on for the better part of an hour. (A 2 s
    /// window here; the real one is 30 s.)
    #[cfg(unix)]
    #[tokio::test]
    async fn both_registries_too_slow_is_an_error_naming_both() {
        let dir = temp("both-slow");
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(13) ^ (i >> 11)) as u8).collect();
        let b = pinned_to(&body);
        let (one, _, a) = serve_paced(body.clone(), Duration::ZERO, RACE_BYTES, 2_000, Duration::from_millis(100)).await;
        let (two, _, c) = serve_paced(body, Duration::from_millis(300), RACE_BYTES, 2_000, Duration::from_millis(100)).await;
        let floor = Floor { bytes_per_sec: 200_000, window: Duration::from_secs(2) };
        let to = dir.join("opencode.tgz");
        let got = tokio::time::timeout(Duration::from_secs(40), download(&b, &[one.as_str(), two.as_str()], &to, &|_| {}, floor)).await;
        a.abort();
        c.abort();
        let e = got.expect("left at the floor, not crawled on").expect_err("neither is fast enough");
        let said = format!("{e:#}");
        for reg in [&one, &two] {
            assert!(said.contains(&tarball_url(reg, &b)), "names {reg}: {said}");
        }
        assert_eq!(said.matches("slower than 200 KB/s").count(), 2 * TRIES_PER_REGISTRY, "{said}");
        assert!(!to.exists(), "nothing half-downloaded is left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a reply and a terminal say about an install under way.
    #[test]
    fn progress_reads_in_megabytes_and_steps_by_quarters() {
        let p = |got, total| Progress { got, total };
        assert_eq!(p(34_200_000, Some(79_136_190)).mb(), "34/79 MB");
        assert_eq!(p(800_000, Some(3_000_000)).mb(), "0.8/3.0 MB");
        assert_eq!(p(12_000_000, None).mb(), "12 MB");
        assert_eq!(progress_line(p(34_200_000, Some(79_136_190))), "  downloading OpenCode 1.19.5 — 34/79 MB");
        let steps: Vec<u64> = [0, 19, 25, 49, 50, 75, 99, 100].iter().map(|pct| p(pct * 1000, Some(100_000)).step()).collect();
        assert_eq!(steps, [0, 0, 1, 1, 2, 3, 3, 3]);
        assert_eq!(p(41_000_000, None).step(), 2, "every 20 MB without a total");
    }

    /// `mafold add <name> --harness opencode` installs the pinned build first;
    /// any other runtime — or none named — leaves the machine as it is.
    #[test]
    fn only_an_opencode_agent_installs_the_pin_on_add() {
        for h in ["opencode", "open-code", " OpenCode "] {
            assert!(runs_pinned(h), "{h}");
        }
        for h in ["claude-code", "codex", "kimi", "", "something-else"] {
            assert!(!runs_pinned(h), "{h}");
        }
    }

    /// A binary that is the pinned bytes but another version is still not ours.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_binary_claiming_another_version_is_not_put_in_place() {
        let dir = temp("wrongver");
        let (bytes, integrity) = fake_package(&dir, "1.20.0");
        let (url, task) = serve(bytes).await;
        let root = dir.join("root");
        let b = Build { target: "test", package: "opencode-test", integrity: Box::leak(integrity.into_boxed_str()), path: "package/bin/opencode" };
        let e = install(&root, &b, &[url.as_str()]).await.unwrap_err();
        task.abort();
        assert!(format!("{e:#}").contains("not 1.19.5"), "{e:#}");
        assert!(!binary_in(&root).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
