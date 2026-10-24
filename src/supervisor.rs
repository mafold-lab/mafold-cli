//! Multi-daemon supervisor — run one daemon PER BOT (separate process, pid, and
//! log), driven by a local config (`~/.mafold/daemons.json`, the "local cache"
//! of which bots to run). Each daemon reads its harness from the server
//! (cloud-first via `getMe`); the local config supplies the token + workdir the
//! server can't know.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;

use crate::platform;
use mafold_core::mafold_types::units::EN;

/// One configured daemon = one bot presence.
#[derive(Serialize, Deserialize, Clone)]
pub struct DaemonCfg {
    /// The bot username — also the per-daemon pid/log key.
    pub name: String,
    /// The bot's `mb_` token.
    pub token: String,
    pub workdir: String,
    /// Local harness hint; the server (`getMe`) is authoritative at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Extra environment for this daemon's process and everything it spawns —
    /// the way to pin ONE daemon to a particular login without touching the
    /// others: `CLAUDE_SECURESTORAGE_CONFIG_DIR` for Claude Code (its turns
    /// then treat that login as their `default`, see `accounts.rs`),
    /// `CODEX_HOME` for Codex. Generic on purpose — no per-harness field.
    /// `mafold add … --env KEY=VALUE`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

/// `KEY=VALUE` pairs from `--env`, as the map [`DaemonCfg::env`] holds. A
/// leading `~/` in a value is the home directory (shells only expand it at the
/// start of a word, so `KEY=~/x` reaches us verbatim).
pub fn parse_env(pairs: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    let mut m = std::collections::BTreeMap::new();
    for p in pairs {
        let Some((k, v)) = p.split_once('=') else {
            anyhow::bail!("--env expects KEY=VALUE, got `{p}`");
        };
        let k = k.trim();
        if k.is_empty() || k.contains(|c: char| c.is_whitespace() || c == '=') {
            anyhow::bail!("--env: `{p}` is not a valid variable name");
        }
        let v = match v.strip_prefix("~/") {
            Some(rest) => home().join(rest).to_string_lossy().into_owned(),
            None => v.to_string(),
        };
        m.insert(k.to_string(), v);
    }
    Ok(m)
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    daemons: Vec<DaemonCfg>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}
fn cfg_path() -> PathBuf {
    home().join(".mafold/daemons.json")
}
fn daemons_dir() -> PathBuf {
    home().join(".mafold/daemons")
}
fn safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}
fn pid_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("pid")
}

/// Per-daemon in-flight-turn marker: the daemon writes its active-turn count here
/// (via `TurnGuard`), and the supervisor reads it to DRAIN — wait a live turn out
/// before a cliUpdate restart instead of killing the reply mid-flight.
pub(crate) fn busy_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("busy")
}

fn busy_count(name: &str) -> usize {
    fs::read_to_string(busy_path(name)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Wait (up to `max`) for every ALIVE daemon to finish its in-flight turns before
/// a restart, so the daemon watching its own release doesn't kill a turn. A dead
/// daemon's stale marker is ignored; the cap stops a forever-busy daemon from
/// blocking updates indefinitely.
async fn drain_daemons(daemons: &[DaemonCfg], max: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        let busy: Vec<String> = daemons
            .iter()
            .filter_map(|d| {
                let live = read_pid(&d.name).map(alive).unwrap_or(false);
                let n = busy_count(&d.name);
                (live && n > 0).then(|| format!("{}={n}", d.name))
            })
            .collect();
        if busy.is_empty() {
            return;
        }
        if start.elapsed() >= max {
            println!("↻ drain capped at {max:?} — restarting anyway (still busy: {})", busy.join(", "));
            return;
        }
        println!("↻ waiting for in-flight turns before restart: {}", busy.join(", "));
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
/// Where a bot's daemon writes its output ([`crate::logfile::keep`]).
pub(crate) fn bot_log(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("log")
}

/// Where the supervisor writes its own, on every OS ([`crate::logfile::keep`]).
pub(crate) fn supervisor_log() -> PathBuf {
    home().join(".mafold/supervisor.log")
}

/// When the daemon died without anyone stopping it (`lifecycle::note_death`).
fn deaths_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("deaths")
}

/// The stop a command asked for (`lifecycle::note_stop`), beside the pid file.
fn stopping_path(name: &str) -> PathBuf {
    pid_path(name).with_file_name("stopping")
}

/// Did the daemon whose pid file still names `had` — gone now — die by
/// itself? Only if it is the very process this supervisor saw running
/// (`seen`), and no command asked it to stop. A pid file from before this
/// supervisor started — a reboot, a supervisor that took its daemons down
/// with it — says nothing.
fn died_by_itself(had: Option<u32>, seen: Option<u32>, stopping: &std::path::Path) -> bool {
    match had {
        Some(pid) if seen == Some(pid) => !crate::lifecycle::take_stop(stopping, pid),
        _ => false,
    }
}

/// The replies the daemon is writing, beside its `busy` count.
fn turns_path(name: &str) -> PathBuf {
    busy_path(name).with_file_name("turns.json")
}

/// One at a time starts daemons — the loop keeping them up, a restart a
/// command asked for (`control`), an update stopping them all — so none
/// starts one another has just stopped, or a second of one just started.
static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn spawning() -> std::sync::MutexGuard<'static, ()> {
    SPAWNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// The daemons this supervisor has seen running, by pid — the loop's and
/// the ones a restart started. Only one of those vanishing is news: a pid
/// file left from before it started (a reboot, a supervisor that took its
/// daemons down with it — systemd stops the whole unit) says nothing about
/// the daemon. Taken after [`SPAWNING`], never before.
static SEEN: std::sync::Mutex<std::collections::BTreeMap<String, u32>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

fn seen() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<String, u32>> {
    SEEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the control thread takes requests: not once a re-exec is under
/// way — on Windows the successor already runs and takes them.
static SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn load() -> Config {
    fs::read_to_string(cfg_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// The owner changed `name`'s token (`revokeBotToken`) and the server left the
/// new one for this machine. If this machine runs that bot, swap the token into
/// its entry — workdir, harness and env untouched — and say so. If it doesn't,
/// change nothing: the same notice goes to every machine the owner is signed
/// into, and only one of them is where the bot lives.
fn take_rotation(c: &mut Config, name: &str, token: &str) -> bool {
    match c.daemons.iter_mut().find(|d| d.name.eq_ignore_ascii_case(name)) {
        Some(d) if !token.is_empty() => {
            d.token = token.to_string();
            true
        }
        _ => false,
    }
}
fn store(c: &Config) -> Result<()> {
    let p = cfg_path();
    if let Some(dir) = p.parent() { fs::create_dir_all(dir)?; }
    fs::write(p, serde_json::to_string_pretty(c)?)?;
    Ok(())
}

fn read_pid(name: &str) -> Option<u32> {
    fs::read_to_string(pid_path(name)).ok()?.trim().parse().ok()
}
fn alive(pid: u32) -> bool {
    platform::pid_alive(pid)
}

/// Reap any exited child daemons so they don't linger as zombies — a zombie pid
/// still answers `kill(pid, 0)`, which would make the liveness check (and thus
/// respawn) wrongly believe a crashed daemon is still running. (No-op where the
/// OS has no zombies, e.g. Windows.)
fn reap() {
    platform::reap_children();
}

/// Wire a daemon into the config — the shared write both doors use. The
/// supervisor's own provision claim calls THIS (its next tick starts the
/// daemon); routing it through `add()` would have the running supervisor call
/// `up()`, which stops the supervisor already running — itself, mid-claim.
pub fn wire(
    name: String,
    token: String,
    workdir: String,
    harness: Option<String>,
    env: std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let workdir = fs::canonicalize(&workdir).map(|p| p.to_string_lossy().into_owned()).unwrap_or(workdir);
    let mut c = load();
    c.daemons.retain(|d| d.name != name);
    c.daemons.push(DaemonCfg { name: name.clone(), token, workdir, harness: harness.filter(|s| !s.is_empty()), env });
    store(&c)?;
    println!("✓ added daemon `{name}`");
    Ok(())
}

/// `mafold add <name> --workdir … [--harness …] [--env K=V …]` (token from
/// global --token). A daemon should simply exist — so adding one brings the
/// boot-persistent supervisor up: it runs now AND after every reboot, no
/// extra step.
pub fn add(
    name: String,
    token: String,
    workdir: String,
    harness: Option<String>,
    env: std::collections::BTreeMap<String, String>,
    base: &str,
    no_auto_update: bool,
) -> Result<()> {
    wire(name, token, workdir, harness, env)?;
    up(base, no_auto_update)
}

/// Tombstone a daemon writes (via `deprovision_and_exit`) when its bot was
/// deleted server-side: "remove me from the config, don't respawn me".
fn deprovision_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("deprovision")
}

/// Token of a configured daemon, if any. A child uses this to verify that an
/// (inherited) MAFOLD_DAEMON_NAME really refers to ITSELF before tombstoning —
/// a subprocess of a daemon inherits the parent's env, and a tombstone under
/// the parent's name would deprovision the wrong bot.
pub fn daemon_token(name: &str) -> Option<String> {
    load().daemons.iter().find(|d| d.name == name).map(|d| d.token.clone())
}

/// Called by a daemon child right before it exits because its bot no longer
/// exists. The supervisor owns daemons.json, so the child only leaves this
/// marker; the next tick removes the daemon from the config.
pub fn request_deprovision(name: &str, reason: &str) {
    let p = deprovision_path(name);
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(p, reason);
}

/// Drop every tombstoned daemon from the config (and make sure it's dead).
/// Runs at the top of each supervise tick, BEFORE the respawn pass — so a
/// deprovisioned daemon can't be brought back by the liveness check.
fn sweep_deprovisioned() {
    let mut c = load();
    let mut dropped = Vec::new();
    c.daemons.retain(|d| {
        let p = deprovision_path(&d.name);
        if !p.exists() {
            return true;
        }
        let reason = fs::read_to_string(&p).unwrap_or_default();
        let _ = fs::remove_file(&p);
        dropped.push((d.name.clone(), reason));
        false
    });
    if dropped.is_empty() {
        return;
    }
    if let Err(e) = store(&c) {
        eprintln!("✗ deprovision sweep couldn't rewrite config: {e}");
        return;
    }
    for (name, reason) in dropped {
        let _ = stop_one(&name);
        println!("✂ deprovisioned `{name}` — {}", if reason.is_empty() { "bot deleted server-side" } else { &reason });
    }
}

/// `mafold rm <name>` — drop from config (and stop it if running).
pub fn rm(name: &str) -> Result<()> {
    let mut c = load();
    let before = c.daemons.len();
    c.daemons.retain(|d| d.name != name);
    if c.daemons.len() == before {
        anyhow::bail!("no daemon named `{name}`");
    }
    store(&c)?;
    match stop_confirmed(name) {
        Ok(Some(pid)) => println!("✓ removed daemon `{name}` (stopped pid {pid})"),
        Ok(None) => println!("✓ removed daemon `{name}`"),
        Err(e) => anyhow::bail!("removed `{name}` from the config, but {e:#}"),
    }
    Ok(())
}

fn sup_pid_path() -> PathBuf {
    home().join(".mafold/supervisor.pid")
}
fn read_pid_file(p: &PathBuf) -> Option<u32> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}
/// The running supervisor's pid. Zombie-aware: a detached supervisor that
/// crashed at startup is an unreaped child of the `up` that spawned it, and
/// would otherwise read as running.
fn sup_running() -> Option<u32> {
    read_pid_file(&sup_pid_path()).filter(|p| platform::pid_running(*p))
}

/// Is a supervisor running on this machine right now?
pub fn supervisor_running() -> bool {
    sup_running().is_some()
}

/// Was the supervisor here started to update itself — without
/// `--no-auto-update`? From its own record, else from how its service is
/// registered; `None` when neither says. Whether it does is this AND the
/// machine setting ([`crate::update::auto_update_on`]).
pub(crate) fn started_to_update() -> Option<bool> {
    let service = crate::lifecycle::service(sup_running());
    service.run.as_ref().map(|r| r.auto_update).or_else(|| registration().map(|r| !r.no_auto_update))
}

/// The version the supervisor here runs, from its own record.
pub(crate) fn running_version() -> Option<String> {
    crate::lifecycle::service(sup_running()).run.map(|r| r.version)
}

/// The supervisor's log line when the update setting is, or turns, `on`.
fn setting_note(on: bool) -> &'static str {
    if on {
        "↻ automatic updates are on again on this computer"
    } else {
        "automatic updates are off on this computer — nothing is installed until they're on again (`mafold update --auto-update on`) or someone runs `mafold update`"
    }
}

/// What happens to the running supervisor after `mafold update` swapped the
/// binary under it. Its own 10-minute check compares the latest release with
/// the version it RUNS, so it moves onto the new one by itself — draining
/// replies first; one started with `--no-auto-update` never does. (Telling
/// the caller to `mafold down && mafold up` instead was a trap: a bot that
/// ran it from its own turn died at `down`, and `up` never ran.)
pub fn update_pickup_note() -> &'static str {
    // Only one that reads the setting is held back by it.
    let reads = crate::lifecycle::service(sup_running()).run.is_some_and(|r| r.update_setting);
    if reads && !crate::update::auto_update_on() {
        return "automatic updates are off on this computer, so the supervisor here stays on the version it started with until it's restarted";
    }
    match registration() {
        Some(r) if r.no_auto_update => {
            "the supervisor here runs with --no-auto-update, so it stays on the version it started with until it's restarted"
        }
        Some(_) => "the supervisor here moves onto it by itself within ~10 minutes, letting replies in progress finish first",
        None => {
            "the supervisor here moves onto it by itself within ~10 minutes, letting replies in progress finish first (unless it was started with --no-auto-update)"
        }
    }
}

/// How long a command that stops something waits for it to be gone. A
/// daemon's shutdown only kills its in-flight turns and exits; the
/// supervisor's is the default action of SIGTERM.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long `up` waits for the supervisor it started to be running.
const START_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Wait for a supervisor to be running — its pid file names a live process,
/// which `supervise` writes first thing — and return its pid.
fn await_supervisor() -> Result<u32> {
    let mut pid = None;
    platform::wait_until(START_WAIT, || {
        pid = sup_running();
        pid.is_some()
    });
    pid.with_context(|| {
        format!("the supervisor didn't start within {}s — {}", START_WAIT.as_secs(), supervisor_log_hint())
    })
}

/// Where to look when the supervisor won't run, as said to a person: its own
/// log since C6 — except what it says before it gets that far (a binary that
/// won't start, a bad argument), which stays wherever the service manager put
/// it: the plist's file on macOS (the same one), the journal on Linux, nowhere
/// under the Windows logon task.
fn supervisor_log_hint() -> &'static str {
    #[cfg(target_os = "linux")]
    return "see `mafold logs` (~/.mafold/supervisor.log) — or, if it never got that far, `journalctl --user -u mafold-supervisor`";
    #[cfg(not(target_os = "linux"))]
    return "see `mafold logs` (~/.mafold/supervisor.log)";
}

// ── boot-persistence (a daemon must survive reboot — autostart is part of the
// semantics, not an opt-in) ── the supervisor runs as a per-user service:
// launchd LaunchAgent on macOS, systemd --user unit on Linux. RunAtLoad/WantedBy
// → starts at login; KeepAlive/Restart=always → relaunches on crash. (Self-update
// re-exec keeps the same PID, so the service manager sees no exit → no fight.)
#[cfg(target_os = "macos")]
const SERVICE_LABEL: &str = "com.mafold.supervisor";

/// The PATH the supervised service must run with: the installing shell's PATH
/// (which found `claude` etc.) plus the usual user bins. A service manager
/// (launchd/systemd) otherwise hands out a minimal PATH (`/usr/bin:/bin:…`) that
/// lacks them — which is exactly what breaks `claude` under launchd.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn service_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let extra = format!("{}/.local/bin:/opt/homebrew/bin", home().display());
    if base.is_empty() { extra } else { format!("{extra}:{base}") }
}

#[cfg(any(target_os = "macos", windows, test))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn launchagent_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{SERVICE_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn ensure_autostart(base: &str, no_auto_update: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    // Only what the process says before it points its output at this same file
    // itself (`supervise` → `logfile::keep`): a crash at startup.
    let log = supervisor_log();
    fs::create_dir_all(home().join(".mafold"))?;
    fs::create_dir_all(home().join("Library/LaunchAgents"))?;
    let plist = launchagent_path();
    // Carry `--no-auto-update` into the boot-persistent service so a locally-patched
    // install isn't clobbered by the supervisor after the next login (matches the
    // Windows task path + the detached-fallback in start_supervisor).
    let update_arg = if no_auto_update { "\n    <string>--no-auto-update</string>" } else { "" };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n    <string>{exe}</string>\n    <string>--base</string><string>{base}</string>\n    <string>supervise</string>{update_arg}\n  </array>\n\
  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key><string>{path}</string>\n  </dict>\n\
  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ProcessType</key><string>Background</string>\n\
  <key>StandardOutPath</key><string>{log}</string>\n  <key>StandardErrorPath</key><string>{log}</string>\n\
</dict></plist>\n",
        label = SERVICE_LABEL, exe = exe.display(), base = base, log = log.display(), path = xml_escape(&service_path()),
    );
    fs::write(&plist, xml)?;
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    // Re-load cleanly: bootout if already present (ignore errors), then bootstrap.
    let _ = platform::std_command("launchctl").arg("bootout").arg(format!("{domain}/{SERVICE_LABEL}")).output();
    let out = platform::std_command("launchctl").arg("bootstrap").arg(&domain).arg(&plist).output()
        .context("launchctl not available")?;
    if !out.status.success() {
        anyhow::bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_autostart() {
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = platform::std_command("launchctl").arg("bootout").arg(format!("{domain}/{SERVICE_LABEL}")).output();
    let _ = fs::remove_file(launchagent_path());
}

#[cfg(target_os = "macos")]
fn autostart_loaded() -> bool {
    launchagent_path().exists() && service_registered()
}

/// Does launchd still hold the job — plist on disk or not? What `down` reads
/// back after removing it: while the job is loaded, KeepAlive brings the
/// supervisor straight back.
#[cfg(target_os = "macos")]
fn service_registered() -> bool {
    platform::std_command("launchctl")
        .arg("print").arg(format!("gui/{}/{SERVICE_LABEL}", unsafe { libc::getuid() }))
        .output().map(|o| o.status.success()).unwrap_or(false)
}

/// Run the loaded job now. `kickstart` goes past launchd's crash throttle;
/// `-k` replaces an instance launchd still holds whose pid file is gone.
#[cfg(target_os = "macos")]
fn start_registered(_base: &str, _no_auto_update: bool) -> Result<()> {
    let out = platform::std_command("launchctl")
        .args(["kickstart", "-k"])
        .arg(format!("gui/{}/{SERVICE_LABEL}", unsafe { libc::getuid() }))
        .output()
        .context("launchctl not available")?;
    if !out.status.success() {
        anyhow::bail!("launchctl kickstart failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> PathBuf {
    home().join(".config/systemd/user/mafold-supervisor.service")
}

#[cfg(target_os = "linux")]
fn ensure_autostart(base: &str, no_auto_update: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    fs::create_dir_all(home().join(".config/systemd/user"))?;
    // Carry `--no-auto-update` into the boot-persistent unit (see the macOS/Windows
    // paths) so a locally-patched install survives a reboot un-clobbered.
    let update_arg = if no_auto_update { " --no-auto-update" } else { "" };
    let unit = format!(
        "[Unit]\nDescription=Mafold bot supervisor\nAfter=network-online.target\n\n\
[Service]\nEnvironment=PATH={path}\nExecStart={exe} --base {base} supervise{update_arg}\nRestart=always\nRestartSec=3\n\n\
[Install]\nWantedBy=default.target\n",
        exe = exe.display(), base = base, path = service_path(),
    );
    fs::write(systemd_unit_path(), unit)?;
    let _ = platform::std_command("systemctl").args(["--user", "daemon-reload"]).output();
    let out = platform::std_command("systemctl").args(["--user", "enable", "--now", "mafold-supervisor"]).output()
        .context("systemctl not available")?;
    if !out.status.success() {
        anyhow::bail!("systemctl enable failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_autostart() {
    let _ = platform::std_command("systemctl").args(["--user", "disable", "--now", "mafold-supervisor"]).output();
    let _ = fs::remove_file(systemd_unit_path());
}

#[cfg(target_os = "linux")]
fn autostart_loaded() -> bool {
    platform::std_command("systemctl").args(["--user", "is-enabled", "mafold-supervisor"])
        .output().map(|o| o.status.success()).unwrap_or(false)
}

/// Is the unit still enabled or running? What `down` reads back after
/// removing it.
#[cfg(target_os = "linux")]
fn service_registered() -> bool {
    let says = |verb: &str| {
        platform::std_command("systemctl").args(["--user", verb, "mafold-supervisor"])
            .output().map(|o| o.status.success()).unwrap_or(false)
    };
    autostart_loaded() || says("is-active")
}

/// Start the enabled unit now. One that crashed past its start limit sits
/// `failed` and refuses to start until it's reset.
#[cfg(target_os = "linux")]
fn start_registered(_base: &str, _no_auto_update: bool) -> Result<()> {
    let _ = platform::std_command("systemctl").args(["--user", "reset-failed", "mafold-supervisor"]).output();
    let out = platform::std_command("systemctl").args(["--user", "restart", "mafold-supervisor"]).output()
        .context("systemctl not available")?;
    if !out.status.success() {
        anyhow::bail!("systemctl restart failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

// Windows: a per-user Task Scheduler logon task. Registered from an XML file
// (not `/TR`) because /TR mangles quoted paths AND can't express what we need:
// no execution time limit (the default kills the supervisor after 72h!),
// battery guards off, and a hidden task entry. There is no true launchd
// KeepAlive / systemd Restart=always on Windows — RestartOnFailure only covers
// the task FAILING TO START, not the process crashing later (verified on Win11)
// — so a watchdog TimeTrigger re-fires the task every 30 minutes instead:
// alive → IgnoreNew makes it a no-op; dead → relaunched within half an hour.
//
// The action must NOT exec the console binary directly. Whenever the user's
// default terminal is Windows Terminal (the Win11 default), Task Scheduler
// starting a console exe materializes a real WT window that the supervisor
// cannot hide from the inside: GetConsoleWindow() under defterm returns the
// pseudoconsole's hidden hwnd, so `supervise --hidden`'s ShowWindow(SW_HIDE)
// hides the wrong window and a log console squats on the desktop at every
// sign-in (verified on Win11 26200). The task therefore runs
// `conhost.exe --headless <mafold …>` — a windowless console host regardless
// of the default-terminal setting, alive exactly as long as the supervisor,
// so the task-instance semantics above (IgnoreNew watchdog, RestartOnFailure)
// are unchanged. A GUI-subsystem shim exe would be the documented equivalent
// but means shipping a second binary; a wscript/VBS wrapper trips Defender's
// behavioral ML (which already flagged the bare schtasks registration once).
#[cfg(windows)]
const TASK_NAME: &str = "MafoldSupervisor";

#[cfg(windows)]
fn task_xml_path() -> PathBuf {
    home().join(".mafold/supervisor-task.xml")
}

/// Task Scheduler requires its XML in UTF-16; write it with a BOM so schtasks
/// accepts the file regardless of the system code page.
#[cfg(windows)]
fn write_utf16le(path: &PathBuf, text: &str) -> std::io::Result<()> {
    let mut bytes = vec![0xFF_u8, 0xFE_u8]; // UTF-16LE BOM
    for u in text.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    fs::write(path, bytes)
}

/// Decode a console tool's captured stdout. schtasks prints in the CONSOLE
/// code page (CP936/GBK on Chinese Windows — verified on this machine), so a
/// plain UTF-8 read garbles any non-ASCII in the task XML (e.g. an exe path
/// under `C:\Users\中文名\…`) into U+FFFD and every `contains` check on it
/// silently fails. Try strict UTF-8 first (covers ASCII and CP65001), then
/// convert from the real console/OEM code page.
#[cfg(windows)]
fn decode_console_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
    use windows_sys::Win32::System::Console::GetConsoleOutputCP;
    unsafe {
        let mut cp = GetConsoleOutputCP();
        if cp == 0 {
            cp = GetOEMCP();
        }
        let n = MultiByteToWideChar(cp, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; n as usize];
        let n = MultiByteToWideChar(cp, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), n);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::from_utf16_lossy(&wide[..n as usize])
    }
}

/// The registered task's XML, decoded — `None` when the task doesn't exist (or
/// schtasks is unavailable). Shared by the "is it loaded / is it current"
/// checks so they judge the same bytes.
#[cfg(windows)]
fn query_task_xml() -> Option<String> {
    let mut cmd = platform::std_command("schtasks");
    cmd.args(["/Query", "/TN", TASK_NAME, "/XML"]);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(decode_console_bytes(&out.stdout))
}

/// The exact action the logon task must run for THIS binary + settings.
/// `--hidden` is belt-and-braces: under `conhost --headless` no window ever
/// exists, but tasks written by older installs exec the binary directly and
/// rely on it, so the flag stays supported forever.
#[cfg(windows)]
fn task_command() -> String {
    // Absolute path — Task Scheduler actions get no PATH you can trust, and
    // conhost has lived in System32 since NT.
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\conhost.exe")
}

#[cfg(any(windows, test))]
fn task_arguments(exe: &str, base: &str, no_auto_update: bool) -> String {
    // The exe is quoted because conhost re-parses this as a command line and
    // profile paths ("C:\Users\John Smith\…") contain spaces.
    let mut args = format!("--headless \"{exe}\" --base {base} supervise --hidden");
    if no_auto_update {
        args.push_str(" --no-auto-update");
    }
    args
}

#[cfg(windows)]
fn ensure_autostart(base: &str, no_auto_update: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    fs::create_dir_all(home().join(".mafold"))?;
    // Scope the logon trigger + run-as principal to the current user (the task
    // is per-user, like a launchd gui-domain agent / systemd --user unit).
    let username = std::env::var("USERNAME").context("USERNAME not set — can't scope the logon task")?;
    let user = match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{username}"),
        _ => username,
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo>\n\
    <Description>Mafold bot supervisor — keeps your Mafold agent daemons running and restarts them at sign-in.</Description>\n\
    <URI>\\{task}</URI>\n\
  </RegistrationInfo>\n\
  <Triggers>\n\
    <LogonTrigger>\n\
      <Enabled>true</Enabled>\n\
      <UserId>{user}</UserId>\n\
    </LogonTrigger>\n\
    <TimeTrigger>\n\
      <StartBoundary>2020-01-01T00:00:00</StartBoundary>\n\
      <Repetition>\n\
        <Interval>PT30M</Interval>\n\
        <StopAtDurationEnd>false</StopAtDurationEnd>\n\
      </Repetition>\n\
      <Enabled>true</Enabled>\n\
    </TimeTrigger>\n\
  </Triggers>\n\
  <Principals>\n\
    <Principal id=\"Author\">\n\
      <UserId>{user}</UserId>\n\
      <LogonType>InteractiveToken</LogonType>\n\
      <RunLevel>LeastPrivilege</RunLevel>\n\
    </Principal>\n\
  </Principals>\n\
  <Settings>\n\
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
    <AllowHardTerminate>true</AllowHardTerminate>\n\
    <StartWhenAvailable>true</StartWhenAvailable>\n\
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n\
    <IdleSettings>\n\
      <StopOnIdleEnd>false</StopOnIdleEnd>\n\
      <RestartOnIdle>false</RestartOnIdle>\n\
    </IdleSettings>\n\
    <AllowStartOnDemand>true</AllowStartOnDemand>\n\
    <Enabled>true</Enabled>\n\
    <Hidden>true</Hidden>\n\
    <RunOnlyIfIdle>false</RunOnlyIfIdle>\n\
    <WakeToRun>false</WakeToRun>\n\
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
    <Priority>7</Priority>\n\
    <RestartOnFailure>\n\
      <Interval>PT1M</Interval>\n\
      <Count>999</Count>\n\
    </RestartOnFailure>\n\
  </Settings>\n\
  <Actions Context=\"Author\">\n\
    <Exec>\n\
      <Command>{cmd}</Command>\n\
      <Arguments>{args}</Arguments>\n\
    </Exec>\n\
  </Actions>\n\
</Task>\n",
        task = TASK_NAME,
        user = xml_escape(&user),
        cmd = xml_escape(&task_command()),
        args = xml_escape(&task_arguments(&exe.display().to_string(), base, no_auto_update)),
    );
    let xml_path = task_xml_path();
    write_utf16le(&xml_path, &xml)?;
    // /F re-registers in place — same idempotence as launchctl bootout→bootstrap.
    let mut create = platform::std_command("schtasks");
    create.args(["/Create", "/TN", TASK_NAME, "/XML"]).arg(&xml_path).arg("/F");
    let out = create.output().context("schtasks not available")?;
    if !out.status.success() {
        anyhow::bail!(
            "schtasks /Create failed: {}",
            decode_console_bytes(&out.stderr).trim()
        );
    }
    // A logon task only fires at the NEXT sign-in; `up` promises a supervisor
    // running NOW (launchctl bootstrap / systemctl enable --now semantics), so
    // kick the task immediately. If that fails, fall back to a plain detached
    // spawn — boot-persistence is already in place either way.
    if !run_task() && sup_running().is_none() {
        let _ = start_supervisor(base, no_auto_update);
    }
    Ok(())
}

/// Start the registered task NOW (so the supervisor runs under the task, where
/// the watchdog trigger and restart settings govern it). True on success.
#[cfg(windows)]
fn run_task() -> bool {
    let mut cmd = platform::std_command("schtasks");
    cmd.args(["/Run", "/TN", TASK_NAME]);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(windows)]
fn remove_autostart() {
    for args in [&["/End", "/TN", TASK_NAME][..], &["/Delete", "/TN", TASK_NAME, "/F"][..]] {
        let mut cmd = platform::std_command("schtasks");
        cmd.args(args);
        let _ = cmd.output();
    }
    let _ = fs::remove_file(task_xml_path());
}

/// Loaded = the task exists AND still points at THIS binary. The pointing check
/// matters: a task registered by an older install keeps its old command line
/// forever, and a binary/task drift is exactly how boot-persistence silently
/// broke once (task said `--hidden`, the reinstalled binary didn't know the
/// flag → exit code 2 at every logon). Any mismatch → not loaded → `up`
/// re-registers the task against the current binary.
#[cfg(windows)]
fn autostart_loaded() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Some(xml) = query_task_xml() else { return false };
    xml.contains(&xml_escape(&exe.display().to_string())) && xml.contains("supervise")
}

/// Current = loaded AND the task's arguments are exactly what `up` would
/// register right now. `up`'s idempotence fast-path must use THIS, not
/// `autostart_loaded`: the task command line encodes `--base`/`--no-auto-update`,
/// so gating on "loaded" alone would silently ignore a re-run with changed
/// flags (e.g. `mafold up --no-auto-update` on a machine whose task was
/// registered without it — the user's intent would never reach the logon task).
#[cfg(windows)]
fn autostart_current(base: &str, no_auto_update: bool) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Some(xml) = query_task_xml() else { return false };
    // The expected arguments embed the (quoted) exe path, so this one check
    // also rejects a task pointing at another binary — and a pre-conhost task
    // (no `--headless`) mismatches too, which is exactly how `up` migrates
    // old visible-window registrations to the windowless form.
    xml.contains(&format!("<Arguments>{}</Arguments>", xml_escape(&task_arguments(&exe.display().to_string(), base, no_auto_update))))
}

/// Does the logon task still exist (pointing anywhere)? What `down` reads
/// back after removing it: its watchdog trigger would bring the supervisor
/// back within half an hour.
#[cfg(windows)]
fn service_registered() -> bool {
    query_task_xml().is_some()
}

/// Run the registered task now — Windows has no KeepAlive, so a crashed
/// supervisor stays down until the next sign-in or watchdog tick. Failing
/// that, at least run it detached.
#[cfg(windows)]
fn start_registered(base: &str, no_auto_update: bool) -> Result<()> {
    if !run_task() {
        start_supervisor(base, no_auto_update)?;
    }
    Ok(())
}

/// The registered plist/unit now carries `--no-auto-update`, so "current" must
/// compare that flag too — otherwise `up --no-auto-update` after a plain `up` (or
/// vice-versa) would see a loaded service and skip re-registration, leaving the
/// wrong update policy persisted across the next reboot.
#[cfg(not(windows))]
fn autostart_current(_base: &str, no_auto_update: bool) -> bool {
    if !autostart_loaded() { return false; }
    #[cfg(target_os = "macos")]
    let registered = fs::read_to_string(launchagent_path()).map(|s| s.contains("--no-auto-update")).unwrap_or(false);
    #[cfg(target_os = "linux")]
    let registered = fs::read_to_string(systemd_unit_path()).map(|s| s.contains("--no-auto-update")).unwrap_or(false);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let registered = no_auto_update; // unsupported OS: unreachable (autostart_loaded() == false)
    registered == no_auto_update
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn ensure_autostart(_base: &str, _no_auto_update: bool) -> Result<()> { anyhow::bail!("boot-persistence not supported on this OS") }
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn remove_autostart() {}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn autostart_loaded() -> bool { false }
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn service_registered() -> bool { false }
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn start_registered(_base: &str, _no_auto_update: bool) -> Result<()> { anyhow::bail!("boot-persistence not supported on this OS") }

/// What the registered service does by itself, said the way it is on this
/// platform.
fn autostart_promise() -> &'static str {
    #[cfg(windows)]
    return "starts at sign-in; if it stops, it's back within 30 minutes";
    #[cfg(not(windows))]
    return "starts on login + relaunches on crash";
}


/// `mafold up` — bring the supervisor up as a boot-persistent service (one per
/// machine): it keeps every configured daemon running and OWNS updates. Autostart
/// is the default — once up, it starts on login and relaunches on crash, until
/// `mafold down`. Idempotent: re-running just confirms (the running supervisor
/// picks up add/rm within ~10s).
pub fn up(base: &str, no_auto_update: bool) -> Result<()> {
    let c = load();
    // ZERO daemons is not a reason to stay down: after `mafold login` the
    // supervisor is also this machine's control plane — it keeps the harness
    // report fresh (the api's online window is 90s, so a one-shot report goes
    // dark before the user even finishes naming their bot) and claims
    // auto-provisioned bots, which is how the FIRST bot arrives. Refusing to
    // run on an empty config was a chicken-and-egg that killed onboarding:
    // the web can only provision onto a machine whose supervisor is up.
    if c.daemons.is_empty() && crate::session::load().is_none() {
        println!("Nothing to do yet — this machine has no login and no daemons. Either:");
        println!("  mafold login          # connect it (bots you create on the web then auto-install)");
        println!("  mafold add <bot>      # then, from its project folder, run one you already have");
        return Ok(());
    }
    let what = managing(c.daemons.len());
    if autostart_current(base, no_auto_update) {
        // Registered and pointing at this binary + these flags — but
        // registered isn't running. A supervisor killed by hand, crashing past
        // what the service manager retries, or (Windows, no KeepAlive) waiting
        // for the next sign-in stays down, and `up` is exactly the command a
        // user runs to fix an offline bot: start it, and say it's running only
        // once it is.
        let pid = match sup_running() {
            Some(pid) => pid,
            None => {
                start_registered(base, no_auto_update)?;
                let pid = await_supervisor()?;
                println!("↑ started the supervisor — it was registered but not running");
                pid
            }
        };
        println!("✓ supervisor running (pid {pid}) — {what}; picks up changes within ~10s");
        return Ok(());
    }
    // An older supervisor (detached, or registered with other flags) goes
    // first, and is gone before the new one starts — two would fight over the
    // daemons.
    if let Some(old) = sup_running() {
        if !platform::stop_and_wait(old, STOP_WAIT) {
            anyhow::bail!(
                "the supervisor already running (pid {old}) is still running {}s after being asked to stop — not starting a second one",
                STOP_WAIT.as_secs()
            );
        }
        crate::lifecycle::note_exit(old, crate::lifecycle::Why::Stopped, Some("mafold up"));
    }
    let _ = fs::remove_file(sup_pid_path());
    match ensure_autostart(base, no_auto_update) {
        Ok(()) => {
            let pid = await_supervisor().context("registered the supervisor to start on login")?;
            println!("✓ supervisor running (pid {pid}) — {what}; {}", autostart_promise());
            println!("  logs:   mafold logs <name>  ·  the supervisor's: mafold logs");
            println!("  status: mafold status   ·   stop: mafold down");
        }
        Err(e) => {
            // No service manager → at least run it now (just not across reboots).
            eprintln!("note: couldn't enable autostart ({e}); running detached (won't survive reboot)");
            let pid = match sup_running() {
                Some(pid) => pid,
                None => {
                    start_supervisor(base, no_auto_update)?;
                    await_supervisor()?
                }
            };
            println!("✓ supervisor running (pid {pid}) — {what}");
        }
    }
    Ok(())
}

/// The "what it's doing" clause of `up`'s confirmations — with zero daemons the
/// supervisor isn't managing anything yet, it's WAITING for the first one.
fn managing(n: usize) -> String {
    if n == 0 {
        "waiting to auto-install the first bot you create on the web".into()
    } else {
        format!("managing {n} daemon(s)")
    }
}

/// Spawn the detached `mafold supervise` process.
fn start_supervisor(base: &str, no_auto_update: bool) -> Result<u32> {
    fs::create_dir_all(daemons_dir())?;
    let exe = std::env::current_exe()?;
    // Already its own log, begun with the mark — so whatever it prints before
    // it takes the file over (an error it dies of) is read like the rest.
    let out = crate::logfile::open_for_child(&supervisor_log())?;
    let err = out.try_clone()?;
    let mut cmd = platform::std_command(exe);
    cmd.arg("--base").arg(base).arg("supervise");
    // Carry the caller's update setting into the detached process — this is the
    // same intent the service/task command line encodes on the managed paths.
    if no_auto_update {
        cmd.arg("--no-auto-update");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    platform::configure_detached(&mut cmd);
    let child = cmd.spawn().context("failed to spawn supervisor")?;
    // No pid file from here: `supervise` writes its own first thing, so
    // `up`'s read-back proves the new process got that far — a file written
    // on its behalf would read as running while it crashed at startup.
    Ok(child.id())
}

/// The long-lived supervisor loop: keep every configured daemon running, and own
/// updates — check every 10 minutes (or when a daemon relays a release), replace
/// the shared binary safely (locked + verified),
/// then stop all daemons and re-exec self so the new supervisor respawns them on
/// the new version.
pub async fn supervise(base: String, auto_update: bool) {
    // Its own log file, whoever started it — launchd, systemd, the Windows
    // logon task, `up` detached, a person in a terminal (who is told where).
    let log = supervisor_log();
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("logging to {} — `mafold logs -f` to watch", log.display());
    }
    if let Err(e) = crate::logfile::keep(&log) {
        eprintln!("⚠️  couldn't write {} ({e}) — output stays where it was", log.display());
    }
    let _ = fs::create_dir_all(daemons_dir());
    if let Ok(mut f) = fs::File::create(sup_pid_path()) {
        let _ = writeln!(f, "{}", std::process::id());
    }
    // Since when and on what this one runs, and why the one before it
    // stopped — for `mafold status` (activity center C5).
    let (_, last_exit) = crate::lifecycle::begin(std::process::id(), auto_update);
    note_own_exits();
    println!("supervisor up (pid {}) · base {base}", std::process::id());
    if let Some(x) = &last_exit {
        println!("  the one before: {}", exit_line(x, crate::accounts::now()));
    }
    // Answer granted connection calls for EACH account logged in here while
    // the daemons run — the piece that makes "@chatgpt on my Codex
    // subscription" work whenever this machine is up. One listener per
    // account, spawned from the loop below so an account that logs in later
    // gets served without restarting the supervisor. Quiet: each only serves
    // when that account's vault key is already cached here (see
    // `connection::supervise_listener`).
    let mut listening: std::collections::HashSet<String> = std::collections::HashSet::new();
    let http = reqwest::Client::new();
    // A busy mark left by the process this one replaced (an update re-execs
    // in place, mid-«waiting for turns») is not true of this one.
    let _ = fs::remove_file(doing_path());
    // What commands ask of it (`control`: `mafold restart`, and `update` /
    // `rollback` having put another binary on disk), on a thread of its own —
    // a bot restarts now, not once the loop below is done waiting on an
    // install or on turns. A request the one before took and never answered
    // is answered first.
    crate::control::recover(&crate::control::dir(), crate::accounts::now());
    {
        let base = base.clone();
        let served = std::thread::Builder::new().name("control".into()).spawn(move || loop {
            if SERVING.load(std::sync::atomic::Ordering::SeqCst) {
                let mut moving = None;
                // A request that panics this thread would end it for good,
                // with the run record still saying requests are taken.
                let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::control::serve_once(&crate::control::dir(), crate::accounts::now(), &mut |a| carry_out(&base, a, &mut moving));
                }));
                if pass.is_err() {
                    eprintln!("✗ carrying out a request failed — still taking the next ones");
                }
                // Its own restart goes to the loop only now, answered: a
                // re-exec can't answer once it has happened.
                if let Some(m) = moving {
                    *MOVING.lock().unwrap_or_else(|e| e.into_inner()) = Some(m);
                    wake().notify_one();
                }
            }
            std::thread::sleep(crate::control::POLL);
        });
        if let Err(e) = served {
            eprintln!("⚠️  couldn't start taking requests ({e}) — `mafold restart` won't be answered");
        }
    }
    // Keep this machine's harness report fresh (every 30s) so New-Bot and the
    // computers card see it online — on its own task, so the loop below can
    // wait on turns, an update or an install without the machine going dark
    // (the api's online window is 90s). EVERY account logged in here, not
    // just the current one: the server keeps a row per `(account,
    // device_id)`, so reporting only the current account would make a second
    // person's machine read as offline the moment somebody ran `mafold
    // account use`.
    tokio::spawn(heartbeat(http.clone(), base.clone(), crate::session::all, Ledger::here()));
    // Wall clock, like the failure cooldown in `update.rs`: counting 10-second
    // ticks stalls while a Mac sleeps, so a laptop that wakes up an hour past
    // a release still waited up to 10 more minutes before it looked.
    let mut last_update_check = std::time::SystemTime::now();
    // The update setting as last said in the log — a change is said once.
    let mut said_setting: Option<bool> = None;
    loop {
        reap(); // clear zombies so the liveness check below is accurate
        // A daemon whose bot was deleted server-side tombstones itself and
        // exits — drop it from the config BEFORE the respawn pass below.
        sweep_deprovisioned();
        let cfg = load();
        // `mafold update` / `rollback` put another binary on disk and asked to
        // move onto it (`control`) — here, between two passes.
        let moving = MOVING.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((why, version)) = moving {
            let what = match why {
                crate::control::Moved::Update => "updated",
                crate::control::Moved::Rollback => "rolled back",
            };
            println!("↻ the binary on disk was {what}{} — restarting onto it…", version.as_deref().map(|v| format!(" to v{v}")).unwrap_or_default());
            move_on(&cfg.daemons, version.as_deref(), last_exit.as_ref()).await;
        }
        // Keep every configured daemon running (start any that died / aren't up).
        for d in &cfg.daemons {
            let had = read_pid(&d.name);
            if let Some(pid) = had.filter(|p| alive(*p)) {
                seen().insert(d.name.clone(), pid);
                continue;
            }
            // Not while a restart is between stopping it and starting it.
            let _one = spawning();
            match start_one(&base, d) {
                Ok(Some(pid)) => {
                    // `status` reads a few of those in a row as a bot that
                    // won't stay up.
                    let mut seen = seen();
                    if died_by_itself(had, seen.get(&d.name).copied(), &stopping_path(&d.name)) {
                        crate::lifecycle::note_death(&deaths_path(&d.name), crate::accounts::now());
                    }
                    seen.insert(d.name.clone(), pid);
                    println!("↑ started {} (pid {pid})", d.name)
                }
                Ok(None) => {}
                Err(e) => eprintln!("✗ {}: {e}", d.name),
            }
        }
        // Update check every 10 min of wall-clock time, OR immediately when an
        // agent child nudged us (it relayed an events.cliUpdate) — so a new
        // release lands in seconds via the webhook path, not only on the 10-min
        // poll. Gated by `--no-auto-update` (a locally-patched install must not
        // be clobbered by an official release; the nudge is still consumed so
        // the file can't pile up). A version that just failed to apply (e.g. the
        // release download is unreachable from this network) is under cooldown —
        // retried later, not on every tick, and the skip is logged.
        let nudged = crate::update::take_nudge();
        let due = crate::update::wall_elapsed(last_update_check) >= std::time::Duration::from_secs(600);
        // The machine setting, read on every pass: `mafold update
        // --auto-update off` holds this one back from its next check on, with
        // no restart and nothing re-registered (activity center C8).
        let setting = crate::update::auto_update_on();
        if auto_update && said_setting != Some(setting) {
            if said_setting.is_some() || !setting {
                println!("{}", setting_note(setting));
            }
            said_setting = Some(setting);
        }
        if auto_update && !setting && nudged {
            println!("↻ update nudge from a daemon — automatic updates are off on this computer, so not checking");
        }
        if auto_update && setting && (nudged || due) {
            last_update_check = std::time::SystemTime::now();
            if nudged { println!("↻ update nudge from a daemon — checking now"); }
            let checked = crate::update::check(&http, &base, crate::update::Channel::current()).await;
            if let Ok(offer) = &checked {
                crate::update::note_checked(offer.as_ref().map(|r| r.version.as_str()));
            }
            match checked {
                Ok(Some(r)) if !crate::update::should_attempt(&r.version) => {}
                Ok(Some(r)) => {
                    println!("{} — applying + restarting daemons…", r.action_line());
                    let _busy = Busy::start(&doing_path(), "updating", Some(&r.version));
                    match crate::update::apply(&http, &r.url, &r.version, r.sha256.as_deref()).await {
                        Ok(()) => {
                            crate::update::note_installed();
                            move_on(&cfg.daemons, Some(&r.version), last_exit.as_ref()).await;
                        }
                        Err(e) => {
                            let wait = crate::update::mark_failed(&r.version, &e);
                            crate::update::note_failed(&r.version, &e);
                            eprintln!(
                                "update to v{} failed ({e:#}) — keeping v{}, cooling down for {}",
                                r.version,
                                crate::update::current_version(),
                                crate::update::human(wait),
                            );
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("update check failed: {e}"),
            }
        }
        // Control plane (after `mafold login`): claim any auto-provisioned bots
        // (→ add + start their daemons, no `mafold add` paste) for EVERY
        // account logged in here, like the heartbeat above.
        for sess in crate::session::all() {
            if let Err(e) = poll_provisions(&http, &base, &sess).await {
                eprintln!("provision poll failed (@{}): {e}", sess.username);
            }
            // Accounts are re-read every tick, so a login added while the
            // supervisor is already up gets its vault listener right here.
            if listening.insert(sess.username.to_lowercase()) {
                tokio::spawn(crate::connection::supervise_listener(
                    base.clone(),
                    sess.username.clone(),
                ));
            }
        }
        // Or sooner, when a command asks it to move onto a new binary.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            _ = wake().notified() => {}
        }
    }
}

/// Move onto the binary on disk — `onto`, its version when known: let the
/// replies in progress finish (at most 5 minutes, so a stuck one can't hold
/// it forever), stop every daemon, re-exec; the new supervisor starts them
/// again on the new binary. How an update the supervisor installed itself
/// lands, and how `mafold update` / `rollback` ask for theirs (`control`).
///
/// Returns only when the re-exec failed — said loudly — and this, the old
/// supervisor, carries on: its loop starts the daemons again, on the binary
/// on disk.
async fn move_on(daemons: &[DaemonCfg], onto: Option<&str>, last_exit: Option<&crate::lifecycle::Exit>) {
    let _busy = Busy::start(&doing_path(), "waiting_for_turns", onto);
    drain_daemons(daemons, std::time::Duration::from_secs(300)).await;
    // No restart a command asked for starts one in between.
    let _one = spawning();
    for d in daemons {
        let _ = stop_one(&d.name);
    }
    crate::lifecycle::note_exit(std::process::id(), crate::lifecycle::Why::Update, onto);
    SERVING.store(false, std::sync::atomic::Ordering::SeqCst);
    crate::update::reexec_or_warn(onto);
    // Still here: this one didn't stop after all — the stop before it is
    // still the last, and the requests are still its to take.
    SERVING.store(true, std::sync::atomic::Ordering::SeqCst);
    crate::lifecycle::restore_exit(last_exit);
}

/// A restart of the supervisor itself that a command asked for (`control`),
/// left for the loop, which re-execs between two of its passes.
static MOVING: std::sync::Mutex<Option<(crate::control::Moved, Option<String>)>> = std::sync::Mutex::new(None);

/// Wakes the loop from its wait between two passes.
fn wake() -> &'static tokio::sync::Notify {
    static W: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    W.get_or_init(tokio::sync::Notify::new)
}

/// What the supervisor does with a request a command left (`control`). Its
/// own restart is put in `moving`, for the loop once the answer is written.
fn carry_out(
    base: &str,
    a: &crate::control::Action,
    moving: &mut Option<(crate::control::Moved, Option<String>)>,
) -> (crate::control::Code, crate::control::Reply) {
    use crate::control::{Action, Code, Reply};
    match a {
        Action::RestartBot { name } => {
            let (code, reply) = restart_bot(base, name);
            match code {
                Code::Restarted => println!(
                    "↻ restarted {name} as asked ({} → pid {})",
                    reply.old_pid.map_or("wasn't running".into(), |p| format!("pid {p}")),
                    reply.new_pid.map_or("?".into(), |p| p.to_string()),
                ),
                _ => eprintln!("✗ restarting {name} as asked: {code:?}{}", reply.detail.as_deref().map(|d| format!(" — {d}")).unwrap_or_default()),
            }
            (code, reply)
        }
        Action::RestartSupervisor { why, version } => {
            *moving = Some((*why, version.clone()));
            (Code::Accepted, Reply::default())
        }
    }
}

/// Restart one bot's daemon: stop it — until it is gone — and start it
/// again, with nothing else starting daemons meanwhile.
fn restart_bot(base: &str, name: &str) -> (crate::control::Code, crate::control::Reply) {
    use crate::control::Code;
    let cfg = load();
    let Some(d) = cfg.daemons.iter().find(|d| d.name == name) else {
        return (Code::UnknownBot, crate::control::Reply::default());
    };
    let _one = spawning();
    // A daemon that died since the loop last reaped is still a zombie, which
    // `start_one` would take for running and start nothing.
    reap();
    let had = read_pid(name);
    let (code, reply) = restart_with(name, &pid_path(name), STOP_WAIT, || start_one(base, d));
    if let Some(pid) = reply.new_pid.filter(|_| code == Code::Restarted) {
        let mut seen = seen();
        // Down before anyone asked, the loop not having noticed yet: a death
        // all the same, for `status` to count.
        if reply.old_pid.is_none() && died_by_itself(had, seen.get(name).copied(), &stopping_path(name)) {
            crate::lifecycle::note_death(&deaths_path(name), crate::accounts::now());
        }
        // The loop's to keep from here — and a death of this one is news.
        seen.insert(name.to_string(), pid);
    }
    (code, reply)
}

/// [`restart_bot`] for the daemon whose pid file is `pid_file`, started
/// again by `start`.
fn restart_with(
    name: &str,
    pid_file: &std::path::Path,
    wait: std::time::Duration,
    start: impl FnOnce() -> Result<Option<u32>>,
) -> (crate::control::Code, crate::control::Reply) {
    use crate::control::{Code, Reply};
    let running = || fs::read_to_string(pid_file).ok()?.trim().parse::<u32>().ok().filter(|p| platform::pid_running(*p));
    let old = running();
    // The replies it is cut off in the middle of; the new one picks them up.
    let interrupted = old
        .and_then(|_| fs::read_to_string(pid_file.with_file_name("busy")).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if let Some(pid) = old {
        if let Some(Err(e)) = stop_all_confirmed_at(&[(name, pid_file.to_path_buf())], wait).pop() {
            return (Code::StillRunning, Reply { detail: Some(format!("{e:#}")), old_pid: old, ..Reply::default() });
        }
        // Stopped on purpose: not a death of the daemon's own.
        crate::lifecycle::take_stop(&pid_file.with_file_name("stopping"), pid);
    }
    match start() {
        Ok(Some(pid)) => (Code::Restarted, Reply { old_pid: old, new_pid: Some(pid), interrupted, detail: None }),
        // Up already — something started it the moment it stopped.
        Ok(None) => match running() {
            Some(pid) if Some(pid) != old => (Code::Restarted, Reply { old_pid: old, new_pid: Some(pid), interrupted, detail: None }),
            _ => (Code::Failed, Reply { detail: Some("it didn't start again".into()), old_pid: old, ..Reply::default() }),
        },
        Err(e) => (Code::Failed, Reply { detail: Some(format!("{e:#}")), old_pid: old, ..Reply::default() }),
    }
}

/// Claim auto-provision requests for this device and wire each bot into the local
/// config (the next loop tick starts its daemon). The bot's mb_ token arrives over
/// the owner's authenticated session — no copy-paste.
async fn poll_provisions(http: &reqwest::Client, base: &str, sess: &crate::session::Session) -> Result<()> {
    let resp: serde_json::Value = http
        .post(format!("{base}/api/claimProvisions"))
        .bearer_auth(&sess.token)
        .json(&serde_json::json!({ "device_id": sess.device_id }))
        .send()
        .await?
        .json()
        .await?;
    let items = resp.pointer("/result/items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for it in items {
        let name = it["bot_username"].as_str().unwrap_or_default().to_string();
        let token = it["token"].as_str().unwrap_or_default().to_string();
        if name.is_empty() || token.is_empty() {
            continue;
        }
        let harness = it["harness"].as_str().map(str::to_string);
        let workdir = provision_workdir(&name);
        // wire(), NOT add(): add()'s `up()` would stop the running supervisor
        // — i.e. this very process, mid-claim. The next tick starts the daemon.
        // A provisioned daemon runs on the machine's own login — pinning one
        // to another account is a local decision (`mafold add --env`).
        match wire(name.clone(), token, workdir, harness, Default::default()) {
            Ok(()) => println!("⬇ provisioned @{name} — its daemon starts on the next tick"),
            Err(e) => eprintln!("provision @{name} failed: {e}"),
        }
    }

    // One-click runtime installs queued from New-Bot (`requestRuntimeInstall`).
    // Run each official installer headlessly, then re-report immediately so the
    // web's availability poll flips without waiting for the 30s cadence.
    // Tokens the owner just changed. The old one is already dead — the server
    // hangs its sockets up within a heartbeat — so the daemon is restarted on
    // the new one right away rather than drained: it can't finish a turn on a
    // credential that no longer works.
    let rotations = resp.pointer("/result/rotations").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for it in rotations {
        let name = it["bot_username"].as_str().unwrap_or_default().to_string();
        let token = it["token"].as_str().unwrap_or_default().to_string();
        let mut c = load();
        if !take_rotation(&mut c, &name, &token) {
            continue; // not a bot this machine runs
        }
        match store(&c) {
            Ok(()) => {
                let _ = stop_one(&name);
                println!("↻ @{name}: its token was changed — new one in place, restarting it");
            }
            Err(e) => eprintln!("✗ @{name}: couldn't save its new token: {e}"),
        }
    }

    let installs = resp.pointer("/result/installs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if !installs.is_empty() {
        for rt in installs.iter().filter_map(|v| v.as_str()) {
            let rt = rt.to_string();
            println!("⬇ installing runtime {rt} (requested from New-Bot)…");
            let _busy = Busy::start(&doing_path(), "installing_runtime", Some(&rt));
            let res = tokio::task::spawn_blocking(move || crate::install::run_within(&rt, true, Some(INSTALL_LIMIT))).await;
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("runtime install failed: {e}"),
                Err(e) => eprintln!("runtime install task failed: {e}"),
            }
        }
        // Installers default to ~/.local/bin; make sure this process (and the
        // daemons it spawns) resolve it even when the service PATH predates the
        // install — else probe() never flips the runtime available.
        if let Some(home) = std::env::var_os("HOME") {
            let local = std::path::Path::new(&home).join(".local/bin");
            let path = std::env::var_os("PATH").unwrap_or_default();
            if local.is_dir() && !std::env::split_paths(&path).any(|p| p == local) {
                let mut parts = vec![local];
                parts.extend(std::env::split_paths(&path));
                if let Ok(joined) = std::env::join_paths(parts) {
                    std::env::set_var("PATH", joined);
                }
            }
        }
        crate::harness::invalidate_versions();
        let _ = report_local_harnesses(http, base, sess, &Ledger::here()).await;
    }
    Ok(())
}

/// Re-send this machine's harnesses (keeps the device "online") — the same
/// report `mafold report` and `mafold status` give: each runtime's version and
/// whether its logins are signed in and how full they are. Version and seat
/// probes are cached (10 and 5 minutes), so a heartbeat stays cheap.
///
/// How Mafold answered lands in `account_health` — a 401 means this login was
/// signed out, and the machine reads offline for that account until someone
/// signs it back in — and a change is said once in the supervisor log.
async fn report_local_harnesses(
    http: &reqwest::Client,
    base: &str,
    sess: &crate::session::Session,
    ledger: &Ledger,
) -> Result<()> {
    let body = crate::harness::report_body(sess).await;
    // The runtimes as this supervisor — on its PATH, the one the bots get —
    // sees them, for `mafold status` to read instead of probing with its own.
    crate::lifecycle::write_runtimes(&ledger.runtimes, &body["harnesses"], crate::accounts::now());
    send_report(http, base, sess, &body, ledger).await
}

/// Where a report's answer is kept, and how to tell whether the login it went
/// out on is still the one this machine holds.
#[derive(Clone)]
struct Ledger {
    path: PathBuf,
    held: fn(&crate::session::Session) -> bool,
    /// Where the report's runtimes are kept (`lifecycle::Runtimes`).
    runtimes: PathBuf,
}

impl Ledger {
    fn here() -> Ledger {
        Ledger { path: crate::account_health::path(), held: still_held, runtimes: crate::lifecycle::runtimes_path() }
    }
}

/// Is `sess` still this machine's login for its account? A report in flight
/// across `mafold login` (a new token) or `mafold account rm` (signed out on
/// purpose) answers for a login that is gone, and says nothing about the
/// account now — recorded, its 401 would read as the new login signed out.
fn still_held(sess: &crate::session::Session) -> bool {
    crate::session::load_named(&sess.username).is_some_and(|s| s.token == sess.token)
}

async fn send_report(
    http: &reqwest::Client,
    base: &str,
    sess: &crate::session::Session,
    body: &serde_json::Value,
    ledger: &Ledger,
) -> Result<()> {
    use crate::account_health::Outcome;
    let sent = http
        .post(format!("{base}/api/reportHarnesses"))
        .bearer_auth(&sess.token)
        .json(body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await;
    let outcome = match &sent {
        Ok(r) if (ledger.held)(sess) => Outcome::of_status(r.status().as_u16()),
        _ => Outcome::Failed,
    };
    let change = crate::account_health::record_at(&ledger.path, &sess.username, &sess.token, outcome, crate::accounts::now());
    match health_news(&sess.username, outcome, change) {
        Some(crate::account_health::Change::SignedOut) => eprintln!(
            "✗ @{u}: Mafold refused this machine's sign-in (401) — it was signed out (expired, or revoked \
             in Settings ▸ Active Sessions). This machine reads offline for @{u} until `mafold login` as \
             @{u} signs it back in.",
            u = sess.username
        ),
        Some(_) => println!("✓ @{}: signed in again — Mafold takes this machine's heartbeat", sess.username),
        None => {}
    }
    sent?.error_for_status()?;
    Ok(())
}

/// What the supervisor log should say about this report, if anything: a
/// sign-out once, and «signed in again» once it is over — also when
/// `mafold login` (another process) cleared the record before this
/// supervisor's next heartbeat, which the file alone can't tell it.
fn health_news(
    username: &str,
    outcome: crate::account_health::Outcome,
    change: crate::account_health::Change,
) -> Option<crate::account_health::Change> {
    use crate::account_health::{Change, Outcome};
    static TOLD_OUT: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let mut told = TOLD_OUT.lock().unwrap_or_else(|e| e.into_inner());
    let who = username.to_lowercase();
    match outcome {
        Outcome::SignedOut if change == Change::SignedOut && told.insert(who.clone()) => Some(Change::SignedOut),
        Outcome::Accepted if told.remove(&who) || change == Change::SignedBackIn => Some(Change::SignedBackIn),
        _ => None,
    }
}

/// How often the heartbeat reports — a third of the api's 90-second online
/// window, so one lost report never reads as offline.
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The longest one account's report may take before the heartbeat moves on.
const HEARTBEAT_ONE: std::time::Duration = std::time::Duration::from_secs(25);

/// The longest a runtime install requested from the web may run.
const INSTALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(600);

/// The supervisor's heartbeat: report this machine for every account
/// `sessions` names, every [`HEARTBEAT_EVERY`], for as long as the process
/// lives. Its own task — nothing the main loop waits on can hold it up.
async fn heartbeat(http: reqwest::Client, base: String, sessions: fn() -> Vec<crate::session::Session>, ledger: Ledger) {
    beat_every(HEARTBEAT_EVERY, move || {
        let (http, base, ledger) = (http.clone(), base.clone(), ledger.clone());
        async move {
            for sess in sessions() {
                let _ = tokio::time::timeout(HEARTBEAT_ONE, report_local_harnesses(&http, &base, &sess, &ledger)).await;
            }
        }
    })
    .await
}

/// Run `beat` now and then every `every`; a beat that overruns pushes the
/// next one back rather than firing a burst to catch up.
async fn beat_every<F, Fut>(every: std::time::Duration, mut beat: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        beat().await;
    }
}

/// Note why this supervisor stops, where it can tell (`supervisor.exit.json`):
/// its loop panicking, or a stop signal — logout, shutdown, `kill`; `mafold
/// down` notes its own stop over that once this one is gone. Nothing can
/// catch a hard kill: `status` reads a supervisor gone without a note as
/// `unknown`.
fn note_own_exits() {
    use crate::lifecycle::{note_exit, Why};
    let me = std::process::id();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // The loop runs on the main thread. A panic in a spawned task (the
        // heartbeat, a vault listener) ends that task, not the supervisor.
        if std::thread::current().name() == Some("main") {
            note_exit(me, Why::Crashed, Some(&info.to_string()));
        }
        prev(info);
    }));
    // On a thread and runtime of its own: a supervisor whose workers are all
    // stuck is exactly the one `mafold down` is run against, and a signal
    // caught but never acted on would make it unstoppable. Each signal is
    // registered on its own, so one that can't be leaves the other working.
    #[cfg(unix)]
    let _ = std::thread::Builder::new().name("signals".into()).spawn(move || {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async move {
            let on = |kind: SignalKind, name: &'static str, num: libc::c_int| async move {
                let Ok(mut s) = signal(kind) else { return std::future::pending().await };
                s.recv().await;
                note_exit(me, Why::Signal, Some(name));
                // Then exactly what the signal would have done without this
                // handler — the service manager sees the same death as before.
                unsafe {
                    libc::signal(num, libc::SIG_DFL);
                    libc::raise(num);
                }
                std::process::exit(1)
            };
            tokio::join!(on(SignalKind::terminate(), "SIGTERM", libc::SIGTERM), on(SignalKind::interrupt(), "SIGINT", libc::SIGINT));
        });
    });
}

/// `updated to v0.9.150 · 2h ago`, for the log and `mafold status`.
fn exit_line(x: &crate::lifecycle::Exit, now: i64) -> String {
    use crate::lifecycle::Why;
    let what = match (x.reason, x.detail.as_deref()) {
        (Why::Update, Some(v)) => format!("updated to v{v}"),
        (Why::Update, None) => "updated".into(),
        (Why::Stopped, _) => "stopped (mafold down / up)".into(),
        (Why::Signal, Some(s)) => format!("stopped by {s}"),
        (Why::Signal, None) => "stopped by a signal".into(),
        (Why::Crashed, _) => "crashed".into(),
        (Why::Shutdown, _) => "went down with the machine (shutdown or restart)".into(),
        (Why::Unknown, _) => "gone without a word (killed, or crashed)".into(),
    };
    match x.at {
        Some(at) => format!("{what} · v{} · {}", x.version, EN.ago((now - at).max(0) as f64)),
        None => format!("{what} · v{}", x.version),
    }
}

/// What the supervisor is busy with, for `mafold status` (a separate
/// process) to say — `~/.mafold/supervisor.doing.json` while it lasts.
fn doing_path() -> PathBuf {
    home().join(".mafold/supervisor.doing.json")
}

/// One thing the supervisor is busy with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Doing {
    /// `installing_runtime` · `updating` (downloading a new mafold) ·
    /// `waiting_for_turns` (replies being finished before an update restart).
    pub what: String,
    /// The runtime being installed / the version being updated to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Epoch seconds it started.
    pub since: i64,
    /// The supervisor that wrote it; a mark from a process that is gone is
    /// not true any more.
    pub pid: u32,
}

/// Marks the supervisor busy until dropped. A newer mark replaces an older
/// one; dropping either clears the file.
struct Busy(PathBuf);

impl Busy {
    fn start(path: &std::path::Path, what: &str, detail: Option<&str>) -> Busy {
        let doing = Doing {
            what: what.into(),
            detail: detail.map(Into::into),
            since: crate::accounts::now(),
            pid: std::process::id(),
        };
        if let Ok(json) = serde_json::to_string(&doing) {
            let _ = fs::write(path, json);
        }
        Busy(path.to_path_buf())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `installing runtime codex · 3m so far`, for `mafold status`.
fn doing_line(d: &Doing, now: i64) -> String {
    let what = d.what.replace('_', " ");
    let what = match &d.detail {
        Some(x) => format!("{what} {x}"),
        None => what,
    };
    let took = EN.elapsed((now - d.since).max(0) as f64 * 1000.0);
    format!("{what} · {took} so far")
}

/// What the supervisor `running` (its pid) is busy with, if anything.
fn read_doing(path: &std::path::Path, running: Option<u32>) -> Option<Doing> {
    let doing: Doing = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    (Some(doing.pid) == running).then_some(doing)
}

/// Default workdir for an auto-provisioned bot: `~/.mafold/work/<label>` (created).
fn provision_workdir(bot_username: &str) -> String {
    let label = bot_username.rsplit(':').next().unwrap_or(bot_username);
    let dir = home().join(".mafold/work").join(label);
    let _ = fs::create_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

fn start_one(base: &str, d: &DaemonCfg) -> Result<Option<u32>> {
    if let Some(pid) = read_pid(&d.name) {
        if alive(pid) { return Ok(None); }
    }
    let dir = daemons_dir().join(safe(&d.name));
    fs::create_dir_all(&dir)?;
    let exe = platform::current_exe()?;
    // Where it writes until it points its output here itself, which it does
    // first thing (`agent::run` → `logfile::keep`) — and from then on rotates.
    // Begun with the mark and rotated here too: a daemon that dies before it
    // gets that far, again every 10 s, says why in a file `mafold diagnose`
    // reads, and that doesn't grow without end.
    let out = crate::logfile::open_for_child(&bot_log(&d.name))?;
    let err = out.try_clone()?;

    let mut cmd = platform::std_command(exe);
    cmd.arg("agent")
        // The supervisor owns updates — daemons never self-update.
        .arg("--no-auto-update")
        // The daemon's own extras first, so nothing in them can shadow the
        // MAFOLD_* identity below.
        .envs(&d.env)
        .env("MAFOLD_BASE", base)
        .env("MAFOLD_BOT_TOKEN", &d.token)
        .env("MAFOLD_WORKDIR", &d.workdir)
        .env("MAFOLD_HARNESS", d.harness.as_deref().unwrap_or("claude-code"))
        // So the daemon writes its busy marker at the path the supervisor's drain
        // check reads (keyed by THIS exact daemon name).
        .env("MAFOLD_DAEMON_NAME", &d.name)
        // The log it takes over and rotates itself (`agent::run` → `logfile::keep`).
        .env(crate::logfile::LOG_ENV, bot_log(&d.name))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // New session / no console → detached from the controlling terminal.
    platform::configure_detached(&mut cmd);
    let child = cmd.spawn().with_context(|| format!("failed to spawn daemon `{}`", d.name))?;
    let pid = child.id();
    writeln!(fs::File::create(pid_path(&d.name))?, "{pid}")?;
    Ok(Some(pid))
}

/// `mafold down [name]` — turn the supervisor off (remove autostart + stop all
/// daemons), or stop just one daemon. `down` is the explicit "off": after it, the
/// supervisor won't come back on reboot until `mafold up` / `add` again.
///
/// Every "✓ stopped" is read back: it's printed once the process is gone, and
/// anything still running when the wait runs out fails the command (activity
/// center C3 — it used to print right after the signal, whatever happened).
pub fn down(only: Option<&str>) -> Result<()> {
    let c = load();
    // A name that matches nothing is a typo or a wrong guess, never a no-op.
    // 2026-09-05: a bot restarted itself with `mafold down claude-code` while
    // registered as `fei_pota:claude-code`, read the "0 daemon(s) stopped" as
    // success, and the stuck turn it was trying to shake off lived on for
    // another seven minutes of "why is it still ignoring me".
    require_known(only, &c.daemons)?;
    // Stop the supervisor FIRST (when stopping everything), and be sure it's
    // gone, so it can't respawn the daemons we're about to stop: remove the
    // service so KeepAlive / Restart / the watchdog task can't relaunch it,
    // then stop the process.
    if only.is_none() {
        let sup = sup_running();
        remove_autostart();
        if service_registered() {
            anyhow::bail!(
                "couldn't remove the supervisor's login service — it's still registered, so it would start again; nothing was stopped"
            );
        }
        match sup {
            Some(pid) if !platform::stop_and_wait(pid, STOP_WAIT) => anyhow::bail!(
                "the supervisor (pid {pid}) is still running {}s after being asked to stop — left the daemons alone, it would restart them",
                STOP_WAIT.as_secs()
            ),
            Some(pid) => {
                crate::lifecycle::note_exit(pid, crate::lifecycle::Why::Stopped, None);
                println!("✓ stopped the supervisor (pid {pid}) · autostart removed")
            }
            None => println!("✓ autostart removed · the supervisor wasn't running"),
        }
        let _ = fs::remove_file(sup_pid_path());
    }
    // A bot's own turn can run this, and it dies with that bot's daemon — so
    // every OTHER daemon is signalled before any is waited on, and the one this
    // command runs under (MAFOLD_DAEMON_NAME, inherited by its turns) goes
    // last, after the rest is reported. One at a time, the daemons after it in
    // the config were never stopped.
    let own = std::env::var("MAFOLD_DAEMON_NAME").ok();
    let (mine, others) = down_order(&c.daemons, only, own.as_deref());
    let mut stopped = 0;
    let mut stuck = Vec::new();
    let mut report = |name: &str, res: Result<Option<u32>>| match res {
        Ok(Some(pid)) => {
            println!("✓ stopped {name} (pid {pid})");
            stopped += 1;
        }
        Ok(None) if only.is_some() => println!("· {name} wasn't running"),
        Ok(None) => {}
        Err(e) => {
            eprintln!("✗ {e:#}");
            stuck.push(name.to_string());
        }
    };
    let files: Vec<(&str, PathBuf)> = others.iter().map(|n| (*n, pid_path(n))).collect();
    for ((name, _), res) in files.iter().zip(stop_all_confirmed_at(&files, STOP_WAIT)) {
        report(name, res);
    }
    for name in mine {
        println!("… stopping {name} — the daemon this command runs under, so this may be its last line");
        let _ = std::io::stdout().flush();
        report(name, stop_confirmed(name));
    }
    println!("{stopped} daemon(s) stopped");
    if let Some(n) = only.filter(|_| stopped > 0) {
        if sup_running().is_some() {
            println!("{}", respawn_note(n));
        }
    }
    if !stuck.is_empty() {
        anyhow::bail!("{} daemon(s) still running: {}", stuck.len(), stuck.join(", "));
    }
    Ok(())
}

/// Which daemons `down` stops: `(the one this command runs under, the rest)`,
/// narrowed to `only` when it names one.
fn down_order<'a>(daemons: &'a [DaemonCfg], only: Option<&str>, own: Option<&str>) -> (Vec<&'a str>, Vec<&'a str>) {
    daemons
        .iter()
        .map(|d| d.name.as_str())
        .filter(|n| only.is_none_or(|o| o == *n))
        .partition(|n| own == Some(*n))
}

/// How long `restart` gives the supervisor to take its request — it looks
/// every 300 ms — before withdrawing it. Asked while another restart is under
/// way, it is taken once that one is done: up to its stop's wait (10 s) and a
/// start, so twice that.
const TAKE_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// …and then to answer: the stop's own wait, the start, and room to spare.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A daemon that starts and dies at once isn't a restart that worked: how
/// long after it started `restart` looks at it again.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// What `mafold restart` came to — all it prints with `--json`.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub(crate) struct Restart {
    pub ok: bool,
    /// `restarted`; or `unverified` — asked, and maybe done, but the new
    /// daemon couldn't be read back; or not done: `unknown_bot`,
    /// `no_supervisor`, `supervisor_too_old` (it takes no requests),
    /// `not_taken` (it didn't take this one: withdrawn), `still_running`
    /// (the bot didn't stop), `exited` (it started and died at once),
    /// `failed`.
    pub code: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_pid: Option<u32>,
    /// Replies it was cut off in the middle of; the new daemon picks them up.
    pub interrupted: usize,
}

/// `mafold restart <name>` — restart one bot. The supervisor does it (a
/// `control` request); it counts once the bot's pid file names a new process
/// that is still running a moment later. Exit status 0: restarted; 2: asked,
/// not confirmed; 1: not done.
pub fn restart(name: &str, json: bool) -> Result<()> {
    if !json && std::env::var("MAFOLD_DAEMON_NAME").ok().as_deref() == Some(name) {
        println!("… restarting {name} — the daemon this command runs under, so this may be its last line");
        let _ = std::io::stdout().flush();
    }
    let r = restarting(name);
    if json {
        println!("{}", serde_json::to_string(&r)?);
    } else if r.ok {
        match r.old_pid {
            Some(old) => println!("✓ restarted {name} (pid {old} → {})", r.new_pid.unwrap_or_default()),
            None => println!("✓ started {name} (pid {}) — it wasn't running", r.new_pid.unwrap_or_default()),
        }
        if r.interrupted > 0 {
            let n = r.interrupted;
            println!("  cut off {n} {} in progress — it picks {} back up now", if n == 1 { "reply" } else { "replies" }, if n == 1 { "it" } else { "them" });
        }
    } else {
        eprintln!("{}", restart_failure(&r));
    }
    let _ = std::io::stdout().flush();
    match r.code {
        "restarted" => Ok(()),
        "unverified" => std::process::exit(2),
        _ => std::process::exit(1),
    }
}

/// Ask the running supervisor to restart `name`, and read back what it did.
fn restarting(name: &str) -> Restart {
    use crate::control::{Action, Asked, Code};
    let mut r = Restart { ok: false, code: "failed", name: name.to_string(), detail: None, old_pid: None, new_pid: None, interrupted: 0 };
    if let Err(e) = require_known(Some(name), &load().daemons) {
        r.code = "unknown_bot";
        r.detail = Some(format!("{e:#}"));
        return r;
    }
    let Some(sup) = sup_running() else {
        r.code = "no_supervisor";
        return r;
    };
    let run = crate::lifecycle::read::<crate::lifecycle::Run>(&crate::lifecycle::run_path()).filter(|x| x.pid == sup);
    if run.as_ref().is_none_or(|x| x.control < 1) {
        r.code = "supervisor_too_old";
        r.detail = Some(too_old_note(run.as_ref(), name));
        return r;
    }
    r.old_pid = read_pid(name).filter(|p| platform::pid_running(*p));
    let a = match crate::control::ask(Action::RestartBot { name: name.to_string() }, TAKE_WAIT, ANSWER_WAIT) {
        Ok(Asked::Answered(a)) => a,
        Ok(Asked::Untaken) => {
            r.code = "not_taken";
            r.detail = Some(format!("the supervisor (pid {sup}) didn't take the request within {}s — nothing was restarted", TAKE_WAIT.as_secs()));
            return r;
        }
        Ok(Asked::Unanswered) => {
            r.code = "unverified";
            r.detail = Some(format!("the supervisor took the request but didn't say how it went within {}s", ANSWER_WAIT.as_secs()));
            return r;
        }
        Err(e) => {
            r.detail = Some(format!("couldn't ask the supervisor: {e:#}"));
            return r;
        }
    };
    r.detail = a.detail.clone();
    match a.code {
        Code::Restarted => {}
        Code::UnknownBot => {
            r.code = "unknown_bot";
            return r;
        }
        Code::StillRunning => {
            r.code = "still_running";
            return r;
        }
        Code::Failed => return r,
        // Answered, and not carried out.
        Code::Expired => {
            r.code = "not_taken";
            r.detail = Some("the supervisor got to the request too late and didn't carry it out — nothing was restarted".into());
            return r;
        }
        Code::Unknown => {
            r.detail.get_or_insert_with(|| "the supervisor couldn't read the request — nothing was restarted".into());
            return r;
        }
        // Maybe carried out, maybe not.
        Code::Accepted | Code::Interrupted => {
            r.code = "unverified";
            r.detail.get_or_insert_with(|| format!("the supervisor answered `{:?}`", a.code));
            return r;
        }
    }
    r.old_pid = a.old_pid.or(r.old_pid);
    r.interrupted = a.interrupted;
    // Not the supervisor's word for it: the pid file, read here, names the
    // process it says it started, which is not the one before and is running
    // — still, a moment later.
    let now = read_pid(name).filter(|p| platform::pid_running(*p));
    match a.new_pid {
        Some(new) if now == Some(new) && a.old_pid != Some(new) => {
            r.new_pid = Some(new);
            std::thread::sleep(SETTLE);
            if platform::pid_running(new) {
                r.ok = true;
                r.code = "restarted";
            } else {
                r.code = "exited";
                r.detail = Some(format!("it started (pid {new}) and exited right away — `mafold logs {name}` says why"));
            }
        }
        said => {
            r.code = "unverified";
            r.new_pid = now;
            r.detail = Some(format!(
                "the supervisor says it started pid {}, but the pid file names {}",
                said.map_or("?".into(), |p| p.to_string()),
                now.map_or("no running process".into(), |p| format!("pid {p}")),
            ));
        }
    }
    r
}

/// A supervisor from before `restart`: nothing was asked of it. When it
/// moves onto a newer one, and what does the job until then.
fn too_old_note(run: Option<&crate::lifecycle::Run>, name: &str) -> String {
    let which = run.map(|r| format!(" (v{})", r.version)).unwrap_or_default();
    let when = match run.map(|r| r.auto_update) {
        Some(false) => "It stays on its version until it is restarted",
        _ => "It moves onto a newer one by itself within ~10 minutes",
    };
    format!(
        "the supervisor running here{which} is from before `mafold restart` and takes no requests — nothing was restarted. \
         {when}; meanwhile `mafold down {name}` restarts the bot (the supervisor starts it again)"
    )
}

/// `restart` not done, or not confirmed, in a line for a person.
fn restart_failure(r: &Restart) -> String {
    let name = &r.name;
    let detail = r.detail.as_deref().map(|d| format!(" — {d}")).unwrap_or_default();
    match r.code {
        "no_supervisor" => {
            "✗ the supervisor isn't running, so nothing here keeps bots up — `mafold up` starts it, and every bot with it".into()
        }
        "supervisor_too_old" | "unknown_bot" | "not_taken" => format!("✗ {}", r.detail.as_deref().unwrap_or("not restarted")),
        "still_running" => format!("✗ {name} didn't stop, so it wasn't restarted{detail}"),
        "exited" => format!("✗ {}", r.detail.as_deref().unwrap_or("it exited right away")),
        "unverified" => format!("? asked the supervisor to restart {name}, but couldn't confirm it{detail} — `mafold status` shows where it is"),
        _ => format!("✗ couldn't restart {name}{detail}"),
    }
}

/// The binary on disk was just replaced (`mafold update`, `mafold rollback`):
/// ask the supervisor running here to move onto it, and say what came of
/// it — a line for the command to print, and whether it is moving. Read
/// back from the record the new supervisor writes first thing; one still
/// letting replies finish says so.
pub fn move_onto_disk(why: crate::control::Moved, version: Option<&str>) -> (String, bool) {
    use crate::control::{Action, Asked, Code};
    use crate::lifecycle::{read, run_path, Run};
    let Some(pid) = sup_running() else {
        return ("no supervisor runs here — `mafold up` starts one on it".into(), false);
    };
    // What happens without being asked: an update the supervisor picks up
    // itself; a rollback, never — it runs on the newer one it started with.
    let unasked = || match why {
        crate::control::Moved::Update => update_pickup_note().to_string(),
        crate::control::Moved::Rollback => "the supervisor here keeps running the version it started with until it is restarted".into(),
    };
    if read::<Run>(&run_path()).filter(|r| r.pid == pid).is_none_or(|r| r.control < 1) {
        return (format!("{} (it is from before restart requests)", unasked()), false);
    }
    let asked = crate::accounts::now();
    let action = Action::RestartSupervisor { why, version: version.map(str::to_string) };
    match crate::control::ask(action, TAKE_WAIT, std::time::Duration::from_secs(5)) {
        Ok(Asked::Answered(a)) if a.code == Code::Accepted => {
            let mut back = None;
            platform::wait_until(std::time::Duration::from_secs(20), || {
                back = read::<Run>(&run_path()).filter(|r| r.started_at >= asked && sup_running() == Some(r.pid));
                back.is_some()
            });
            let line = match back {
                Some(r) => format!("the supervisor restarted onto it (v{}, pid {})", r.version, r.pid),
                None => "the supervisor restarts onto it as soon as the replies in progress are written (at most 5 minutes) — `mafold status` shows when".into(),
            };
            (line, true)
        }
        Ok(Asked::Answered(a)) => (format!("the supervisor answered `{:?}` — {}", a.code, unasked()), false),
        Ok(Asked::Untaken) => (format!("the supervisor didn't take the request — {}", unasked()), false),
        Ok(Asked::Unanswered) => ("asked the supervisor to restart onto it — `mafold status` shows when it has".into(), true),
        Err(e) => (format!("couldn't ask the supervisor to restart ({e:#}) — {}", unasked()), false),
    }
}

/// `down <name>` while the supervisor runs is a restart, not an off: the
/// supervisor's next tick starts every configured daemon that isn't running.
/// Said, rather than leaving "✓ stopped" to be found out ten seconds later.
fn respawn_note(name: &str) -> String {
    format!(
        "  the supervisor is running, so it starts `{name}` again within ~10s — a restart.\n  \
         to keep it off: `mafold down` (everything) or `mafold rm {name}`"
    )
}

/// Stop `name`'s daemon and wait until its process is gone — what the
/// commands a person (or the desktop app) runs report on. The supervisor's
/// own restarts use [`stop_one`], which only signals. `Ok(None)`: it wasn't
/// running.
fn stop_confirmed(name: &str) -> Result<Option<u32>> {
    stop_confirmed_at(&pid_path(name), name, STOP_WAIT)
}

/// [`stop_confirmed`] for the daemon whose pid file is `pid_file`.
fn stop_confirmed_at(pid_file: &std::path::Path, name: &str, wait: std::time::Duration) -> Result<Option<u32>> {
    let mut one = stop_all_confirmed_at(&[(name, pid_file.to_path_buf())], wait);
    one.pop().expect("one result per daemon")
}

/// Stop several daemons — `(name, pid file)` — and wait for all of them:
/// every one is signalled before any is waited on, so stuck daemons cost one
/// wait between them, not one each. One result per daemon, in order;
/// `Ok(None)`: it wasn't running.
fn stop_all_confirmed_at(daemons: &[(&str, PathBuf)], wait: std::time::Duration) -> Vec<Result<Option<u32>>> {
    let read = |f: &std::path::Path| -> Option<u32> { fs::read_to_string(f).ok()?.trim().parse().ok() };
    let mut pids: Vec<Option<u32>> = daemons.iter().map(|(_, f)| read(f).filter(|p| platform::pid_running(*p))).collect();
    for ((_, f), pid) in daemons.iter().zip(pids.iter_mut()) {
        if let Some(p) = *pid {
            // Said before the signal: the supervisor must not count this
            // stop as the daemon dying — not even when the daemon is the
            // one running this command, and takes it down with it.
            crate::lifecycle::note_stop(&f.with_file_name("stopping"), p);
            if !end_daemon(p, f) {
                // Not the daemon: a process that took its pid since the pid
                // file was written. Nothing of it runs; the file goes.
                crate::lifecycle::take_stop(&f.with_file_name("stopping"), p);
                if read(f) == Some(p) {
                    let _ = fs::remove_file(f);
                }
                *pid = None;
            }
        }
    }
    platform::wait_until(wait, || pids.iter().flatten().all(|p| !platform::pid_running(*p)));
    daemons
        .iter()
        .zip(pids)
        .map(|((name, f), pid)| {
            let Some(pid) = pid else { return Ok(None) };
            if platform::pid_running(pid) {
                anyhow::bail!("`{name}` (pid {pid}) is still running {}s after being asked to stop", wait.as_secs());
            }
            // A running supervisor may already have written its replacement's pid.
            if read(f) == Some(pid) {
                let _ = fs::remove_file(f);
            }
            Ok(Some(pid))
        })
        .collect()
}

/// `down <name>` must name a configured daemon. Daemons are registered under the
/// bot's full handle (`owner:bot`), which is not how anyone says them out loud,
/// so besides listing what IS configured the error points at the handle whose
/// bot part matches. Resolving the short form silently is a separate decision:
/// two owners' bots on one machine can share it, so here it is only a hint.
fn require_known(only: Option<&str>, daemons: &[DaemonCfg]) -> Result<()> {
    let Some(n) = only else { return Ok(()) };
    if daemons.iter().any(|d| d.name == n) {
        return Ok(());
    }
    let known: Vec<&str> = daemons.iter().map(|d| d.name.as_str()).collect();
    if known.is_empty() {
        anyhow::bail!("no daemon named `{n}` — none configured (see `mafold status`)");
    }
    let meant: Vec<&str> = known
        .iter()
        .copied()
        .filter(|k| k.rsplit(':').next() == Some(n) || k.eq_ignore_ascii_case(n))
        .collect();
    let hint = match meant.as_slice() {
        [] => String::new(),
        [one] => format!(" — did you mean `{one}`?"),
        many => format!(" — did you mean one of: {}?", many.join(", ")),
    };
    anyhow::bail!("no daemon named `{n}`{hint}\nconfigured: {}", known.join(", "))
}

fn stop_one(name: &str) -> Result<()> {
    match read_pid(name) {
        Some(pid) => {
            end_daemon(pid, &pid_path(name));
            let _ = fs::remove_file(pid_path(name));
            Ok(())
        }
        None => anyhow::bail!("not running"),
    }
}

/// Stop the daemon `pid`, the way every stop of one goes.
///
/// Unix: plain SIGTERM. The daemon's own shutdown handler kills its in-flight
/// claude children precisely (harness::live_children). Never group-kill
/// here — the agent's legitimate background tasks share the daemon's pgroup
/// and must survive a restart (the 2026-07-19 bg-task regression).
///
/// Windows: nothing reaches the daemon to run that handler, and a process
/// outlives its parent — ending the daemon alone left its claude processes
/// running. It goes with everything it started, but for its background tasks'
/// workers and what runs under them (activity center C7) — and only if it is
/// the process `pid_file` was written about: one that started after it took
/// the pid since (a pid file left from before a reboot), and is left be.
/// `false`: so it was — not a daemon of ours, nothing was asked to stop.
pub(crate) fn end_daemon(pid: u32, pid_file: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        // A process started before the note about it was written; a few
        // seconds' slack for a file system that keeps coarse times (FAT: two).
        let by = |at: std::time::SystemTime| platform::ticks_of(at) + 30_000_000;
        let written = fs::metadata(pid_file).and_then(|m| m.modified()).map(by).unwrap_or(0);
        let mut workers = crate::bgtasks::registry_dir().map(|d| crate::bgtasks::running_in(&d)).unwrap_or_default();
        workers.extend(crate::bgtasks::running_in(&crate::computer::tasks_root()));
        let spare: Vec<platform::Spare> = workers.into_iter().map(|(pid, at)| platform::Spare { pid, by: by(at) }).collect();
        platform::end_tree(pid, written, &spare)
    }
    #[cfg(not(windows))]
    {
        let _ = pid_file;
        platform::terminate(pid);
        true
    }
}

/// `mafold status` — list configured daemons and whether each is running.
pub fn status() {
    let auto = if autostart_loaded() { "enabled (boot-persistent)" } else { "off" };
    let running = sup_running();
    match running {
        Some(p) => println!("supervisor: running (pid {p}) · autostart {auto} · owns updates"),
        None => println!("supervisor: not running · autostart {auto} (mafold up)"),
    }
    if !crate::update::auto_update_on() {
        println!("  automatic updates off on this computer (`mafold update --auto-update on`)");
    }
    let now = crate::accounts::now();
    let service = crate::lifecycle::service(running);
    if let Some(r) = &service.run {
        println!("  up {} · v{}", EN.ago((now - r.started_at).max(0) as f64), r.version);
    }
    if let Some(x) = &service.last_exit {
        let whose = if running.is_some() { "the one before" } else { "last stop" };
        println!("  {whose}: {}", exit_line(x, now));
    }
    if let Some(d) = read_doing(&doing_path(), running) {
        println!("  busy: {}", doing_line(&d, now));
    }
    let accounts = crate::session::all();
    if !accounts.is_empty() {
        println!("accounts:");
        let now = crate::accounts::now();
        for s in &accounts {
            let h = crate::account_health::of(&s.username, &s.token);
            println!("  • {:<26} {}", format!("@{}", s.username), crate::account_health::line(&s.username, &h, now));
        }
    }
    let c = load();
    if c.daemons.is_empty() {
        println!("No bot daemons configured (mafold add …).");
        return;
    }
    println!("bot daemons ({}):", c.daemons.len());
    for d in &c.daemons {
        let state = match read_pid(&d.name) {
            Some(p) if platform::pid_running(p) => match crate::lifecycle::read_turns(&turns_path(&d.name), p).and_then(|t| t.since) {
                Some(since) => format!("running (pid {p}) · replying {}", EN.elapsed((now - since).max(0) as f64 * 1000.0)),
                None => format!("running (pid {p})"),
            },
            _ => "stopped".into(),
        };
        let env = if d.env.is_empty() {
            String::new()
        } else {
            format!("  ·  env: {}", d.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "))
        };
        println!(
            "  • {:<26} {:<20} harness={}  ·  {}{env}",
            d.name,
            state,
            d.harness.as_deref().unwrap_or("claude-code"),
            d.workdir,
        );
    }
}

// ---------------- machine-readable status (`mafold status --json`) ----------------

/// What the boot-persistent service is actually registered to run — read back
/// from the plist / unit / scheduled task, not assumed.
///
/// A program that manages this machine (the desktop app) compares this with
/// the binary it would run BEFORE calling `up`: `up` re-registers the service
/// against the CALLER's binary whenever the registered flags differ
/// (`autostart_current`, which on macOS and Linux never looks at the binary
/// itself), so an `up` from a different install would quietly take the service
/// over — and with it every bot on the machine.
#[derive(Debug, PartialEq)]
pub(crate) struct Registration {
    pub exe: Option<String>,
    pub no_auto_update: bool,
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// The first `<string>` of `ProgramArguments` in a launchd plist.
pub(crate) fn parse_plist_registration(xml: &str) -> Registration {
    let exe = xml
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("<string>").nth(1))
        .and_then(|s| s.split("</string>").next())
        .map(xml_unescape);
    Registration { exe, no_auto_update: xml.contains("--no-auto-update") }
}

/// The program of `ExecStart=` in a systemd unit — everything before
/// ` --base `, since `ensure_autostart` writes the path unquoted.
pub(crate) fn parse_unit_registration(unit: &str) -> Registration {
    let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart="));
    let exe = exec.map(|e| e.split(" --base ").next().unwrap_or(e).trim().to_string());
    Registration { exe, no_auto_update: exec.is_some_and(|e| e.contains("--no-auto-update")) }
}

/// The quoted binary inside the scheduled task's `<Arguments>` (`task_arguments`
/// writes `--headless "<exe>" --base …`).
pub(crate) fn parse_task_registration(xml: &str) -> Registration {
    let args = xml
        .split("<Arguments>")
        .nth(1)
        .and_then(|s| s.split("</Arguments>").next())
        .map(xml_unescape)
        .unwrap_or_default();
    let exe = args.split('"').nth(1).map(str::to_string).filter(|s| !s.is_empty());
    Registration { exe, no_auto_update: args.contains("--no-auto-update") }
}

#[cfg(target_os = "macos")]
fn registration() -> Option<Registration> {
    fs::read_to_string(launchagent_path()).ok().map(|x| parse_plist_registration(&x))
}
#[cfg(target_os = "linux")]
fn registration() -> Option<Registration> {
    fs::read_to_string(systemd_unit_path()).ok().map(|x| parse_unit_registration(&x))
}
#[cfg(windows)]
fn registration() -> Option<Registration> {
    query_task_xml().map(|x| parse_task_registration(&x))
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn registration() -> Option<Registration> {
    None
}

/// A path as the filesystem resolves it (symlinks followed), else as given —
/// so `/opt/homebrew/bin/mafold` and `~/.mafold/mafold` compare equal.
fn canonical(p: &str) -> String {
    fs::canonicalize(p).map(|c| c.display().to_string()).unwrap_or_else(|_| p.to_string())
}

/// When `p` was last written, epoch seconds.
fn written_at(p: &std::path::Path) -> Option<i64> {
    let t = fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// Does the supervisor here update itself? When it was started to (no
/// `--no-auto-update`) — and, if it reads the machine setting, the setting
/// says on; one from before the setting updates whatever it says. With no
/// record, its registration — and the setting the next one will read. `None`
/// when nothing says.
fn updates_itself(run: Option<&crate::lifecycle::Run>, reg: Option<&Registration>, setting: bool) -> Option<bool> {
    match run {
        Some(r) => Some(r.auto_update && (setting || !r.update_setting)),
        None => reg.map(|r| !r.no_auto_update && setting),
    }
}

/// Everything a managing program needs to decide what it may do here, as one
/// JSON object. Paths are given both as registered and canonicalised.
///
/// Version 2 (`schema: 2`, activity center C5) only ADDS: every field version
/// 1 had keeps its name and shape. `issues` is THE list of what needs a person
/// here (`machine::issues`) — the computers card and the tray read it instead
/// of working it out again. Nothing here asks Mafold anything: it reads what
/// the supervisor, its daemons and the heartbeat left on disk, and probes the
/// runtimes itself only when no running supervisor has reported them lately.
pub async fn status_json() -> serde_json::Value {
    let now = crate::accounts::now();
    // By the path on disk now: after an update on Linux, current_exe() alone
    // is `<path> (deleted)`, which no managing program can find.
    let me = platform::current_exe().ok().map(|p| p.display().to_string());
    let reg = registration();
    let pid = sup_running();
    let service = crate::lifecycle::service(pid);
    let c = load();
    let mut bots = Vec::new();
    let daemons: Vec<serde_json::Value> = c
        .daemons
        .iter()
        .map(|d| {
            let harness = d.harness.as_deref().unwrap_or("claude-code");
            // Asked from outside the supervisor: a daemon that exited stays its
            // zombie until the next reap (≤10 s), and is not running.
            let running = read_pid(&d.name).filter(|p| platform::pid_running(*p));
            // A count left by a daemon that is gone isn't a reply being written.
            let busy = running.map(|_| busy_count(&d.name)).unwrap_or(0);
            let turns = running.and_then(|p| crate::lifecycle::read_turns(&turns_path(&d.name), p));
            // The runtime it runs: what Mafold told the daemon, as it says;
            // otherwise the local hint as a daemon would resolve it
            // (`claude` → claude-code, anything unimplemented → Claude Code).
            let runtime = turns
                .as_ref()
                .and_then(|t| t.runtime.clone())
                .unwrap_or_else(|| crate::harness::select(harness).id().to_string());
            bots.push(crate::machine::Bot {
                name: d.name.clone(),
                harness: runtime.clone(),
                running: running.is_some(),
                deaths: crate::lifecycle::read_deaths(&deaths_path(&d.name)),
            });
            serde_json::json!({
                "name": d.name,
                // As configured (version 1); `runtime` is what it runs.
                "harness": harness,
                "runtime": runtime,
                "running": running.is_some(),
                "busy": busy > 0,
                "pid": running,
                // The supervisor writes the pid file right after starting it.
                "started_at": running.and_then(|_| written_at(&pid_path(&d.name))),
                // Replies in flight, and since when the longest-running one
                // (a daemon from before C5 only has the count).
                "turns": turns.as_ref().map_or(busy, |t| t.active),
                "turn_since": turns.and_then(|t| t.since),
                "log": bot_log(&d.name).display().to_string(),
            })
        })
        .collect();
    let sessions = crate::session::all();
    let logins: Vec<crate::machine::Login> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| crate::machine::Login::of(&s.username, i == 0, &crate::account_health::of(&s.username, &s.token), now))
        .collect();
    // The runtimes as the running supervisor last reported them — its PATH
    // is the bots' — else (none running, or its report is stale) a probe
    // from here.
    let harnesses = match pid.and_then(|p| crate::lifecycle::read_runtimes(&crate::lifecycle::runtimes_path(), p, now, 120)) {
        Some(rows) => rows,
        None => crate::harness::report_rows().await,
    };
    // Read field by field: a seat health this build can't parse must not
    // turn an installed runtime into a missing one.
    let runtimes: Vec<mafold_core::mafold_types::computers::HarnessInfo> = harnesses
        .iter()
        .filter_map(|h| {
            Some(mafold_core::mafold_types::computers::HarnessInfo {
                id: h["id"].as_str()?.to_string(),
                available: h["available"].as_bool().unwrap_or(false),
                version: h["version"].as_str().map(str::to_string),
                health: serde_json::from_value(h["health"].clone()).ok(),
                accounts: Vec::new(),
            })
        })
        .collect();
    // The version and update setting of the supervisor RUNNING — what this
    // binary is (it may be another install's) says nothing about either.
    let running_version =
        service.run.as_ref().map_or_else(|| crate::update::current_version().to_string(), |r| r.version.clone());
    let setting = crate::update::auto_update_on();
    let auto_update = updates_itself(service.run.as_ref(), reg.as_ref(), setting);
    let seen = crate::update::seen();
    let failing = crate::update::failing(&seen, &running_version, auto_update == Some(true))
        .map(|f| crate::machine::Failing { version: f.version.clone(), since: f.since });
    let issues = crate::machine::issues(&crate::machine::Facts {
        now,
        running: pid,
        service: &service,
        logins: &logins,
        bots: &bots,
        runtimes: &runtimes,
        update: failing.as_ref(),
    });
    serde_json::json!({
        "schema": 2,
        // When this was read, epoch seconds — every `*_at` / `since` below is
        // on the same clock.
        "now": now,
        "version": env!("CARGO_PKG_VERSION"),
        "exe": me,
        "exe_canonical": me.as_deref().map(canonical),
        // This machine's id, as the api keys its rows (`HarnessHost.device_id`).
        "device_id": crate::session::known_device_id(),
        "supervisor": {
            "running": pid.is_some(),
            "pid": pid,
            "autostart": autostart_loaded(),
            "registered_exe": reg.as_ref().and_then(|r| r.exe.clone()),
            "registered_exe_canonical": reg.as_ref().and_then(|r| r.exe.as_deref()).map(canonical),
            "no_auto_update": reg.as_ref().map(|r| r.no_auto_update),
            // Busy installing a runtime or updating (null = not busy).
            "doing": read_doing(&doing_path(), pid),
            // From its own record — null for a supervisor from before C5.
            "started_at": service.run.as_ref().map(|r| r.started_at),
            "version": service.run.as_ref().map(|r| r.version.clone()),
            "auto_update": service.run.as_ref().map(|r| r.auto_update),
            // The requests it takes (`mafold restart`): 0 for one from before
            // them — or from before C5, which says nothing (null).
            "control": service.run.as_ref().map(|r| r.control),
            // Running: why the one before it stopped. Not running: why it did.
            "last_exit": service.last_exit,
            // Its starts after a crash in the last 10 minutes.
            "crashes": service.crashes,
            // Its own file on every OS since C6 (rotated to `<log>.1` past 10 MB).
            "log": supervisor_log().display().to_string(),
            // Was the systemd unit whose journal held it; kept for readers of
            // schema 2, always null now.
            "journal": serde_json::Value::Null,
        },
        "accounts": sessions.iter().map(|s| s.username.clone()).collect::<Vec<_>>(),
        // Each login's heartbeat, as Mafold last answered it.
        "logins": logins,
        "daemons": daemons,
        "harnesses": harnesses,
        "vault": crate::vault::local_state(),
        "update": {
            "channel": crate::update::Channel::current().as_str(),
            "auto_update": auto_update,
            // The machine setting alone (`mafold update --auto-update`), set
            // or not: `auto_update` is false while either keeps it off.
            "auto_update_setting": setting,
            "supported": crate::update::platform_supported(),
            "checked_at": seen.checked_at,
            // A release newer than the supervisor runs, as the last check saw.
            "available": crate::update::available(&seen, &running_version),
            "failed": crate::update::unresolved(&seen, &running_version),
        },
        "issues": issues,
    })
}

/// `mafold logs [<name>] [-n N] [-f] [--json]` — the last lines of a bot's log,
/// or with no name the supervisor's; `-f` keeps printing what is written,
/// across a rotation, until the reader goes away. `--json` is one object per
/// line: `{"type":"open","source","name","path"}` first, then
/// `{"type":"line","text"}` for each line and `{"type":"rotated"}` when the
/// log moved aside under `-f`.
pub fn logs(name: Option<&str>, lines: usize, follow: bool, json: bool) -> Result<()> {
    use std::io::Write as _;
    let path = match name {
        Some(n) => bot_log(n),
        None => supervisor_log(),
    };
    let tail = crate::logfile::tail(&path, lines, false).with_context(|| match name {
        Some(n) => format!("no log for `{n}` (looked at {})", path.display()),
        None => format!("no supervisor log yet (looked at {})", path.display()),
    })?;
    let stdout = std::io::stdout();
    // A reader that went away (the desktop closed the window, `| head`) ends
    // this quietly instead of a panic on the broken pipe.
    let say = |tick: crate::logfile::Tick| -> bool {
        let mut out = stdout.lock();
        let written = match (&tick, json) {
            (crate::logfile::Tick::Line(text), false) => writeln!(out, "{text}"),
            (crate::logfile::Tick::Rotated, false) => Ok(()),
            (crate::logfile::Tick::Line(text), true) => writeln!(out, "{}", serde_json::json!({ "type": "line", "text": text })),
            (crate::logfile::Tick::Rotated, true) => writeln!(out, "{}", serde_json::json!({ "type": "rotated" })),
        };
        written.and_then(|_| out.flush()).is_ok()
    };
    if json {
        let open = serde_json::json!({
            "type": "open",
            "source": if name.is_some() { "bot" } else { "supervisor" },
            "name": name,
            "path": path.display().to_string(),
        });
        if writeln!(stdout.lock(), "{open}").is_err() {
            return Ok(());
        }
    }
    let crate::logfile::Tail { lines: last, end, unfinished, file, .. } = tail;
    for line in last {
        if !say(crate::logfile::Tick::Line(line)) {
            return Ok(());
        }
    }
    if follow {
        // From the last line break (a line still being written comes whole),
        // in the very file those lines came from.
        let file = match file {
            Some(f) => f,
            None => fs::File::open(&path)?,
        };
        crate::logfile::follow_on(file, &path, end, std::time::Duration::from_millis(500), say)
            .with_context(|| format!("stopped reading {}", path.display()))?;
    } else if let Some(rest) = unfinished {
        say(crate::logfile::Tick::Line(rest));
    }
    Ok(())
}

/// The bots configured here as `mafold diagnose` shows them: only fields a
/// person helping would ask about. Never the token (a fingerprint tells two
/// apart) nor an `--env` value (its names only).
pub fn config_summary() -> serde_json::Value {
    let daemons: Vec<serde_json::Value> = load()
        .daemons
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "harness": d.harness,
                "workdir": d.workdir,
                "env": d.env.keys().collect::<Vec<_>>(),
                "token": crate::account_health::fingerprint(&d.token),
            })
        })
        .collect();
    serde_json::json!({ "daemons": daemons })
}

#[cfg(test)]
mod registration_tests {
    use super::*;

    // Each parser is fed exactly what this file's own `ensure_autostart` /
    // `task_arguments` write, so a change to one side breaks a test here.

    #[test]
    fn reads_the_binary_back_out_of_a_launchd_plist() {
        let xml = "<plist version=\"1.0\"><dict>\n  <key>Label</key><string>com.mafold.supervisor</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>/Users/a b/.mafold/mafold</string>\n    <string>--base</string><string>https://api.mafold.com</string>\n    <string>supervise</string>\n    <string>--no-auto-update</string>\n  </array>\n</dict></plist>";
        assert_eq!(
            parse_plist_registration(xml),
            Registration { exe: Some("/Users/a b/.mafold/mafold".into()), no_auto_update: true }
        );
        assert_eq!(parse_plist_registration("<plist/>"), Registration { exe: None, no_auto_update: false });
    }

    #[test]
    fn reads_the_binary_back_out_of_a_systemd_unit() {
        let unit = "[Service]\nEnvironment=PATH=/usr/bin\nExecStart=/home/a/.mafold/mafold --base https://api.mafold.com supervise\nRestart=always\n";
        assert_eq!(
            parse_unit_registration(unit),
            Registration { exe: Some("/home/a/.mafold/mafold".into()), no_auto_update: false }
        );
    }

    #[test]
    fn reads_the_binary_back_out_of_the_scheduled_task() {
        let args = task_arguments(r"C:\Users\John Smith\.mafold\mafold.exe", "https://api.mafold.com", true);
        let xml = format!("<Task><Actions><Exec><Command>conhost.exe</Command><Arguments>{}</Arguments></Exec></Actions></Task>", xml_escape(&args));
        assert_eq!(
            parse_task_registration(&xml),
            Registration { exe: Some(r"C:\Users\John Smith\.mafold\mafold.exe".into()), no_auto_update: true }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str) -> DaemonCfg {
        DaemonCfg { name: name.into(), token: "mb_x".into(), workdir: "/tmp".into(), harness: None, env: Default::default() }
    }

    /// 2026-09-28: five leaked tokens were rotated by hand — revoke, `mafold
    /// add` with the new one, kill the old process — once per bot. The server
    /// now leaves the new token for the owner's machines; the one running the
    /// bot takes it and keeps everything else about the daemon.
    #[test]
    fn a_rotation_for_a_bot_this_machine_runs_swaps_only_the_token() {
        let mut c = Config::default();
        let mut d = cfg("ops:claude333");
        d.workdir = "/Users/ops/.mafold/work/claude333".into();
        d.harness = Some("claude-code".into());
        d.env.insert("CLAUDE_SECURESTORAGE_CONFIG_DIR".into(), "/x/work".into());
        c.daemons.push(d);
        c.daemons.push(cfg("ops:codex"));
        assert!(take_rotation(&mut c, "Ops:Claude333", "mb_new"), "usernames compare case-insensitively");
        let d = &c.daemons[0];
        assert_eq!(d.token, "mb_new");
        assert_eq!(d.workdir, "/Users/ops/.mafold/work/claude333");
        assert_eq!(d.harness.as_deref(), Some("claude-code"));
        assert_eq!(d.env["CLAUDE_SECURESTORAGE_CONFIG_DIR"], "/x/work");
        assert_eq!(c.daemons[1].token, "mb_x", "the other bot is untouched");
    }

    /// Every machine the owner is signed into gets the notice. The ones that
    /// don't run the bot must not start one.
    #[test]
    fn a_rotation_for_a_bot_this_machine_does_not_run_changes_nothing() {
        let mut c = Config::default();
        c.daemons.push(cfg("ops:codex"));
        assert!(!take_rotation(&mut c, "ops:claude333", "mb_new"));
        assert_eq!(c.daemons.len(), 1);
        assert_eq!(c.daemons[0].token, "mb_x");
    }

    /// `--env` pairs become the daemon's extra environment; a leading `~/`
    /// is the home directory, and a pair without `=` is refused rather than
    /// silently dropped.
    #[test]
    fn env_pairs_parse_and_expand_home() {
        let m = parse_env(&["A=1".into(), "CLAUDE_SECURESTORAGE_CONFIG_DIR=~/.mafold/claude-accounts/work".into()]).unwrap();
        assert_eq!(m["A"], "1");
        assert!(m["CLAUDE_SECURESTORAGE_CONFIG_DIR"].ends_with("/.mafold/claude-accounts/work"));
        assert!(!m["CLAUDE_SECURESTORAGE_CONFIG_DIR"].starts_with('~'));
        assert!(parse_env(&["NOEQUALS".into()]).is_err());
        assert!(parse_env(&["=x".into()]).is_err());
        // Older daemons.json rows have no `env` — they still load.
        let d: DaemonCfg = serde_json::from_str(r#"{"name":"a:b","token":"mb_x","workdir":"/tmp"}"#).unwrap();
        assert!(d.env.is_empty());
    }

    /// `mafold down claude-code` against a daemon registered as
    /// `fei_pota:claude-code` printed "0 daemon(s) stopped" and exited 0 — a
    /// restart that never happened, read as one that did.
    #[test]
    fn an_unknown_name_is_an_error_that_names_the_real_one() {
        let d = [cfg("fei_pota:claude-code"), cfg("fei_pota:cuicui")];
        let e = require_known(Some("claude-code"), &d).unwrap_err().to_string();
        assert!(e.contains("no daemon named `claude-code`"), "{e}");
        assert!(e.contains("did you mean `fei_pota:claude-code`"), "{e}");
        assert!(e.contains("fei_pota:cuicui"), "{e}");
        // the exact handle, and "everything", stay fine
        assert!(require_known(Some("fei_pota:cuicui"), &d).is_ok());
        assert!(require_known(None, &d).is_ok());
        // nothing configured at all says so instead of listing nothing
        let e = require_known(Some("x"), &[]).unwrap_err().to_string();
        assert!(e.contains("none configured"), "{e}");
    }

    /// Two owners' bots can share the short form on one machine — the hint
    /// lists both rather than picking one, which is why matching it silently
    /// is a separate decision.
    #[test]
    fn a_shared_short_name_lists_every_candidate() {
        let d = [cfg("a:assistant"), cfg("b:assistant")];
        let e = require_known(Some("assistant"), &d).unwrap_err().to_string();
        assert!(e.contains("did you mean one of: a:assistant, b:assistant"), "{e}");
    }
}

/// Activity center C3: `down` / `rm` say "stopped" once the process is gone,
/// and fail when it isn't. Real processes, a pid file in a temp dir.
#[cfg(test)]
mod stop_tests {
    use super::*;
    use std::time::Duration;

    fn pid_file(tag: &str, pid: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mafold-stop-{}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pid");
        fs::write(&f, format!("{pid}\n")).unwrap();
        f
    }

    #[test]
    fn a_daemon_that_stops_is_reported_once_it_is_gone() {
        let mut child = platform::idle_child();
        let pid = child.id();
        let f = pid_file("stops", pid);
        assert_eq!(stop_confirmed_at(&f, "a:bot", Duration::from_secs(5)).unwrap(), Some(pid));
        assert!(!platform::pid_running(pid), "gone by the time it said so");
        assert!(!f.exists(), "its pid file is cleared");
        // …and the supervisor, should it look before the pid file is gone,
        // finds this stop was asked for: no death.
        let stopping = f.with_file_name("stopping");
        assert!(!died_by_itself(Some(pid), Some(pid), &stopping));
        child.wait().unwrap();
        let _ = fs::remove_dir_all(f.parent().unwrap());
    }

    /// Only a daemon this supervisor saw running, gone with no stop asked
    /// for, died by itself.
    #[test]
    fn a_death_is_a_daemon_this_supervisor_saw_gone_unasked() {
        let stopping = pid_file("deaths", 42).with_file_name("stopping");
        assert!(died_by_itself(Some(42), Some(42), &stopping), "seen running, now gone, nobody stopped it");
        // A pid file from before this supervisor started: a reboot, or the
        // supervisor before it took its daemons down with it.
        assert!(!died_by_itself(Some(42), None, &stopping));
        assert!(!died_by_itself(Some(42), Some(7), &stopping), "the one it saw was another process");
        assert!(!died_by_itself(None, Some(42), &stopping), "no pid file: stopped on purpose");
        crate::lifecycle::note_stop(&stopping, 42);
        assert!(!died_by_itself(Some(42), Some(42), &stopping), "`mafold down <bot>` asked");
        let _ = fs::remove_dir_all(stopping.parent().unwrap());
    }

    /// The fake success this replaces: a process still running after the
    /// wait used to read "✓ stopped" all the same.
    #[cfg(unix)]
    #[test]
    fn a_daemon_that_keeps_running_fails_the_command() {
        let child = platform::stubborn_child();
        let f = pid_file("keeps", child.0.id());
        let e = stop_confirmed_at(&f, "a:bot", Duration::from_secs(1)).unwrap_err().to_string();
        assert!(e.contains("`a:bot` (pid") && e.contains("still running"), "{e}");
        assert!(f.exists(), "nothing claims it's gone");
        let _ = fs::remove_dir_all(f.parent().unwrap());
    }

    /// `down` stops everything at once: one stuck daemon costs one wait, and
    /// the one after it in the config is stopped all the same — `down` run
    /// from a bot's own turn may not live to reach it one at a time.
    #[cfg(unix)]
    #[test]
    fn stopping_several_signals_them_all_before_waiting() {
        let stuck = platform::stubborn_child();
        let also_stuck = platform::stubborn_child();
        let mut a = platform::idle_child();
        let files = [
            ("a:stuck", pid_file("many-stuck", stuck.0.id())),
            ("a:also-stuck", pid_file("many-also", also_stuck.0.id())),
            ("a:one", pid_file("many-one", a.id())),
        ];
        let t = std::time::Instant::now();
        let res = stop_all_confirmed_at(&files, Duration::from_secs(2));
        let took = t.elapsed();
        assert!(took >= Duration::from_secs(2) && took < Duration::from_millis(3500), "one 2s wait for both stuck ones, not one each: {took:?}");
        assert!(res[0].as_ref().is_err_and(|e| e.to_string().contains("`a:stuck`")), "{:?}", res[0].as_ref().err());
        assert!(res[1].as_ref().is_err_and(|e| e.to_string().contains("`a:also-stuck`")), "{:?}", res[1].as_ref().err());
        assert_eq!(res[2].as_ref().ok(), Some(&Some(a.id())), "the one after them is stopped all the same");
        a.wait().unwrap();
        for (_, f) in &files {
            let _ = fs::remove_dir_all(f.parent().unwrap());
        }
    }

    /// The daemon `down` runs under goes last; `down <name>` narrows to one.
    #[test]
    fn down_stops_its_own_daemon_last() {
        let cfg = |name: &str| DaemonCfg { name: name.into(), token: "mb_x".into(), workdir: "/tmp".into(), harness: None, env: Default::default() };
        let d = [cfg("a:one"), cfg("a:me"), cfg("a:two")];
        assert_eq!(down_order(&d, None, Some("a:me")), (vec!["a:me"], vec!["a:one", "a:two"]));
        assert_eq!(down_order(&d, None, None), (vec![], vec!["a:one", "a:me", "a:two"]));
        assert_eq!(down_order(&d, Some("a:me"), Some("a:me")), (vec!["a:me"], vec![]), "a bot restarting itself");
        assert_eq!(down_order(&d, Some("a:two"), Some("a:me")), (vec![], vec!["a:two"]));
    }

    /// A daemon that already exited is "wasn't running" — and is not signalled:
    /// the pid in a stale file may be somebody else's process by now. Covers
    /// the zombie the supervisor hasn't reaped yet, too.
    #[test]
    fn a_daemon_that_already_exited_was_not_running() {
        let mut child = platform::idle_child();
        let pid = child.id();
        child.kill().unwrap();
        assert!(platform::wait_until(Duration::from_secs(5), || !platform::pid_running(pid)));
        let f = pid_file("exited", pid);
        assert_eq!(stop_confirmed_at(&f, "a:bot", Duration::from_secs(5)).unwrap(), None, "dead, unreaped");
        child.wait().unwrap();
        assert_eq!(stop_confirmed_at(&f, "a:bot", Duration::from_secs(5)).unwrap(), None, "dead, reaped");
        let _ = fs::remove_dir_all(f.parent().unwrap());
    }

    /// A restart asked for (`control`, activity center C7): the daemon is
    /// stopped — gone — before another is started, and the answer names both.
    #[test]
    fn a_restart_stops_the_daemon_and_starts_another() {
        use crate::control::Code;
        let mut old = platform::idle_child();
        let f = pid_file("restart", old.id());
        fs::write(f.with_file_name("busy"), "1").unwrap();
        let mut started = None;
        let (code, r) = restart_with("a:bot", &f, Duration::from_secs(5), || {
            assert!(!platform::pid_running(old.id()), "the old one is gone before the new one starts");
            let c = platform::idle_child();
            let pid = c.id();
            fs::write(&f, format!("{pid}\n"))?;
            started = Some(c);
            Ok(Some(pid))
        });
        let mut new = started.expect("started again");
        assert_eq!(code, Code::Restarted, "{r:?}");
        assert_eq!((r.old_pid, r.new_pid, r.interrupted), (Some(old.id()), Some(new.id()), 1), "the reply it was writing is said");
        assert!(platform::pid_running(new.id()));
        assert!(!f.with_file_name("stopping").exists(), "stopped on purpose, and no note of it left to mistake later");
        old.wait().unwrap();
        let _ = new.kill();
        let _ = new.wait();
        let _ = fs::remove_dir_all(f.parent().unwrap());
    }

    #[test]
    fn restarting_a_bot_that_isnt_running_starts_it() {
        use crate::control::Code;
        let mut gone = platform::idle_child();
        let f = pid_file("restart-down", gone.id());
        gone.kill().unwrap();
        gone.wait().unwrap();
        let (code, r) = restart_with("a:bot", &f, Duration::from_secs(5), || Ok(Some(4242)));
        assert_eq!((code, r.old_pid, r.new_pid, r.interrupted), (Code::Restarted, None, Some(4242), 0));
        let _ = fs::remove_dir_all(f.parent().unwrap());
    }

    /// A daemon that won't stop is left alone, and no second one of the same
    /// bot is started beside it.
    #[cfg(unix)]
    #[test]
    fn a_daemon_that_wont_stop_is_not_restarted() {
        use crate::control::Code;
        let stuck = platform::stubborn_child();
        let f = pid_file("restart-stuck", stuck.0.id());
        let mut ran = false;
        let (code, r) = restart_with("a:bot", &f, Duration::from_secs(1), || {
            ran = true;
            Ok(Some(1))
        });
        assert_eq!(code, Code::StillRunning);
        assert!(!ran, "never two of one bot");
        assert!(r.detail.as_deref().is_some_and(|d| d.contains("still running")), "{r:?}");
        let _ = fs::remove_dir_all(f.parent().unwrap());
    }

    /// The supervisor's own restart is left for its loop — which re-execs
    /// between two passes, never in the middle of one — and woken for it.
    #[test]
    fn a_restart_of_the_supervisor_is_left_for_its_loop() {
        use crate::control::{Action, Code, Moved};
        let mut moving = None;
        let (code, _) = carry_out("http://127.0.0.1:1", &Action::RestartSupervisor { why: Moved::Rollback, version: Some("0.9.146".into()) }, &mut moving);
        assert_eq!(code, Code::Accepted, "accepted now: a re-exec can't answer once it has happened");
        assert_eq!(moving, Some((Moved::Rollback, Some("0.9.146".to_string()))), "for the loop, once the answer is written");
        assert!(MOVING.lock().unwrap().is_none(), "not handed over before that");
    }

    /// What `restart` says when it didn't — or couldn't tell that it did.
    #[test]
    fn a_restart_not_done_says_so_and_what_to_do() {
        let r = |code: &'static str, detail: Option<&str>| Restart {
            ok: false, code, name: "ops:bot".into(), detail: detail.map(Into::into), old_pid: None, new_pid: None, interrupted: 0,
        };
        assert!(restart_failure(&r("no_supervisor", None)).contains("`mafold up` starts it"));
        assert!(restart_failure(&r("unverified", Some("x"))).starts_with("? asked the supervisor to restart ops:bot, but couldn't confirm it — x"));
        assert!(restart_failure(&r("still_running", None)).contains("didn't stop, so it wasn't restarted"));
        let run = |auto| crate::lifecycle::Run { pid: 1, started_at: 1, version: "0.9.148".into(), auto_update: auto, crashes: vec![], control: 0, update_setting: false };
        let old = too_old_note(Some(&run(true)), "ops:bot");
        assert!(old.contains("(v0.9.148)") && old.contains("within ~10 minutes") && old.contains("`mafold down ops:bot` restarts the bot"), "{old}");
        assert!(too_old_note(Some(&run(false)), "ops:bot").contains("until it is restarted"), "a supervisor that doesn't update itself won't move on its own");
        assert!(too_old_note(None, "ops:bot").starts_with("the supervisor running here is from before"));
    }

    /// It updates itself when started to and — if it reads the setting — the
    /// setting says on. One from before the setting updates whatever it says.
    #[test]
    fn whether_it_updates_itself_takes_the_setting_where_it_is_read() {
        let run = |auto, reads| crate::lifecycle::Run {
            pid: 1, started_at: 1, version: "0.9.150".into(), auto_update: auto, crashes: vec![], control: 1, update_setting: reads,
        };
        assert_eq!(updates_itself(Some(&run(true, true)), None, true), Some(true));
        assert_eq!(updates_itself(Some(&run(true, true)), None, false), Some(false), "off by the setting");
        assert_eq!(updates_itself(Some(&run(false, true)), None, true), Some(false), "off by --no-auto-update");
        assert_eq!(updates_itself(Some(&run(true, false)), None, false), Some(true), "from before the setting");
        let reg = |no| Registration { exe: None, no_auto_update: no };
        assert_eq!(updates_itself(None, Some(&reg(false)), false), Some(false));
        assert_eq!(updates_itself(None, Some(&reg(true)), true), Some(false));
        assert_eq!(updates_itself(None, None, true), None);
    }

    /// `down <name>` with the supervisor up is a restart, and says so.
    #[test]
    fn stopping_one_daemon_under_a_running_supervisor_says_it_comes_back() {
        let n = respawn_note("ops:claude333");
        assert!(n.contains("starts `ops:claude333` again within ~10s"), "{n}");
        assert!(n.contains("mafold rm ops:claude333"), "{n}");
    }
}

#[cfg(test)]
mod busy_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mafold-busy-{}-{name}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir.join("supervisor.doing.json")
    }

    /// The heartbeat is its own task: a main loop stuck on an install (a
    /// blocking call, like the real one) doesn't hold it up (activity center C2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_heartbeat_keeps_beating_while_the_loop_is_stuck_installing() {
        let beats = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = beats.clone();
        let task = tokio::spawn(beat_every(std::time::Duration::from_millis(100), move || {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
        tokio::task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_secs(2))).await.unwrap();
        task.abort();
        let n = beats.load(std::sync::atomic::Ordering::SeqCst);
        assert!(n >= 15, "about 20 beats during a 2-second install, got {n}");
    }

    #[test]
    fn busy_is_said_while_it_lasts_and_only_by_the_running_supervisor() {
        let path = tmp("mark");
        let me = Some(std::process::id());
        {
            let _busy = Busy::start(&path, "installing_runtime", Some("codex"));
            let d = read_doing(&path, me).expect("said while installing");
            assert_eq!((d.what.as_str(), d.detail.as_deref()), ("installing_runtime", Some("codex")));
            assert_eq!(read_doing(&path, None), None, "no supervisor running: not busy");
            assert_eq!(read_doing(&path, Some(std::process::id() + 1)), None, "another process's mark");
            {
                let _next = Busy::start(&path, "waiting_for_turns", Some("0.9.150"));
                assert_eq!(read_doing(&path, me).unwrap().what, "waiting_for_turns", "the newer mark wins");
            }
        }
        assert_eq!(read_doing(&path, me), None, "cleared when done");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A stand-in api answering one request per entry, in order, then closing.
    async fn api_answering(answers: Vec<(&'static str, &'static str)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            for (status, body) in answers {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let _ = s.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        base
    }

    /// Activity center C4: a heartbeat Mafold refuses (401 — the login was
    /// signed out) is kept, a 5xx says nothing about the login, and the next
    /// accepted heartbeat clears it.
    #[tokio::test]
    async fn a_refused_heartbeat_is_kept_until_one_is_taken_again() {
        const REFUSED: &str = r#"{"ok":false,"error_code":401,"description":"unauthenticated"}"#;
        let health = tmp("health").with_file_name("account-health.json");
        let sess = crate::session::Session {
            token: "s_c4".into(),
            username: "C4Test".into(),
            device_id: "dev-c4".into(),
            device_name: "c4".into(),
        };
        let base = api_answering(vec![
            ("200 OK", r#"{"ok":true,"result":null}"#),
            ("401 Unauthorized", REFUSED),
            ("503 Service Unavailable", r#"{"ok":false,"error_code":503,"description":"down"}"#),
            ("401 Unauthorized", REFUSED),
            ("200 OK", r#"{"ok":true,"result":null}"#),
        ])
        .await;
        let http = reqwest::Client::new();
        let body = serde_json::json!({ "device_id": "dev-c4", "harnesses": [] });
        let ledger = Ledger { path: health.clone(), held: |_| true, runtimes: health.with_file_name("runtimes.json") };
        let beat = || send_report(&http, &base, &sess, &body, &ledger);
        let of = || crate::account_health::of_at(&health, "c4test", "s_c4");

        beat().await.expect("taken");
        let taken_at = of().ok_at.expect("an accepted heartbeat is stamped");
        assert!(of().refused.is_none());

        assert!(beat().await.is_err(), "a refused heartbeat is not a success");
        let refused = of().refused.expect("signed out, on disk");
        assert_eq!(of().ok_at, Some(taken_at), "the last accepted one stays");

        assert!(beat().await.is_err());
        assert_eq!(of().refused, Some(refused.clone()), "a 503 neither clears nor restamps it");
        assert!(beat().await.is_err());
        assert_eq!(of().refused, Some(refused), "still the first refusal");

        beat().await.expect("taken again");
        assert!(of().refused.is_none(), "cleared by the next accepted heartbeat");
        let _ = fs::remove_dir_all(health.parent().unwrap());
    }

    /// A 401 for a login this machine no longer holds — `mafold login` swapped
    /// the token, or `mafold account rm` signed it out on purpose, while the
    /// report was in flight — is not recorded: it says nothing about the
    /// account now.
    #[tokio::test]
    async fn a_refusal_for_a_login_no_longer_held_here_is_not_recorded() {
        let health = tmp("gone").with_file_name("account-health.json");
        let sess = crate::session::Session {
            token: "s_old".into(),
            username: "c4gone".into(),
            device_id: "dev-c4".into(),
            device_name: "c4".into(),
        };
        let base = api_answering(vec![("401 Unauthorized", r#"{"ok":false,"error_code":401,"description":"x"}"#)]).await;
        let ledger = Ledger { path: health.clone(), held: |_| false, runtimes: health.with_file_name("runtimes.json") };
        let body = serde_json::json!({ "harnesses": [] });
        assert!(send_report(&reqwest::Client::new(), &base, &sess, &body, &ledger).await.is_err());
        assert_eq!(crate::account_health::of_at(&health, "c4gone", "s_old"), Default::default());
        assert!(!health.exists(), "nothing written");
        let _ = fs::remove_dir_all(health.parent().unwrap());
    }

    /// The supervisor log says a sign-out once, and «signed in again» once it
    /// is over — also when `mafold login` (another process) cleared the
    /// record first, so the file shows no change to this supervisor.
    #[test]
    fn the_log_says_a_sign_out_once_and_when_it_is_over() {
        use crate::account_health::{Change, Outcome};
        let who = "C4News";
        assert_eq!(health_news(who, Outcome::Accepted, Change::None), None);
        assert_eq!(health_news(who, Outcome::SignedOut, Change::SignedOut), Some(Change::SignedOut));
        assert_eq!(health_news(who, Outcome::SignedOut, Change::None), None, "every 30s after: quiet");
        assert_eq!(health_news(who, Outcome::SignedOut, Change::SignedOut), None, "already said");
        assert_eq!(health_news(who, Outcome::Failed, Change::None), None);
        // `mafold login` cleared the file; this supervisor's next report is
        // taken with no change on disk — still worth the line.
        assert_eq!(health_news("c4news", Outcome::Accepted, Change::None), Some(Change::SignedBackIn));
        assert_eq!(health_news(who, Outcome::Accepted, Change::None), None, "said once");
        // A supervisor that started after the sign-out learns of the end from the file.
        assert_eq!(health_news("c4later", Outcome::Accepted, Change::SignedBackIn), Some(Change::SignedBackIn));
    }

    #[test]
    fn busy_reads_as_a_line() {
        let d = Doing { what: "installing_runtime".into(), detail: Some("codex".into()), since: 1000, pid: 1 };
        assert_eq!(doing_line(&d, 1030), "installing runtime codex · 30s so far");
        assert_eq!(doing_line(&d, 1000 + 185), "installing runtime codex · 3m 5s so far");
        assert_eq!(doing_line(&d, 1000 + 4500), "installing runtime codex · 1h 15m so far", "an hour reads as one, not 75m");
        let u = Doing { what: "updating".into(), detail: None, since: 1000, pid: 1 };
        assert_eq!(doing_line(&u, 999), "updating · 0s so far");
    }
}

#[cfg(test)]
mod heartbeat_e2e {
    /// A throwaway `account-health.json`, so a run against the dev environment
    /// leaves nothing in this machine's real one.
    fn scratch_ledger(tag: &str) -> super::Ledger {
        let dir = std::env::temp_dir().join(format!("mafold-e2e-ledger-{}-{tag}", std::process::id()));
        super::Ledger { path: dir.join("account-health.json"), held: |_| true, runtimes: dir.join("runtimes.json") }
    }

    /// The e2e run's own account on the dev environment, as this machine.
    fn e2e_session() -> Vec<crate::session::Session> {
        let user = std::env::var("MAFOLD_TEST_USER").expect("the e2e run's test account");
        vec![crate::session::Session {
            token: format!("dev:{user}"),
            username: user,
            device_id: "e2e-c2-heartbeat".into(),
            device_name: "E2E C2 heartbeat".into(),
        }]
    }

    /// Activity center C2 on the shared dev environment: the supervisor's
    /// heartbeat task runs while the loop is stuck in an install for longer
    /// than the api's 90-second online window (MAFOLD_TEST_INSTALL_SECS,
    /// default 120), and the machine stays online the whole time.
    /// `MAFOLD_TEST_API_ORIGIN=$E2E_API MAFOLD_TEST_USER=$U cargo test -- --ignored --exact
    /// supervisor::heartbeat_e2e::live_stays_online_through_a_long_install --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn live_stays_online_through_a_long_install() {
        let base = std::env::var("MAFOLD_TEST_API_ORIGIN").expect("dev API origin");
        assert!(base.starts_with("http://127.0.0.1:"), "dev only: {base}");
        let secs: u64 = std::env::var("MAFOLD_TEST_INSTALL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
        let sess = e2e_session().remove(0);
        let client = crate::client::Client::new(base.clone(), sess.token.clone());
        let task = tokio::spawn(super::heartbeat(reqwest::Client::new(), base.clone(), e2e_session, scratch_ledger("c2")));
        let start = std::time::Instant::now();
        // The loop, stuck in a runtime install the way the real one blocks.
        let install = tokio::task::spawn_blocking(move || std::thread::sleep(std::time::Duration::from_secs(secs)));
        let mut seen = Vec::new();
        while !install.is_finished() {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let r = client.call("listHarnessHosts", serde_json::json!({})).await.expect("listHarnessHosts");
            let r = r.get("result").cloned().unwrap_or(r);
            let host = r["items"].as_array().into_iter().flatten().find(|h| h["device_id"] == "e2e-c2-heartbeat").cloned();
            let (online, reported_at) = host.map_or((false, 0), |h| (h["online"] == true, h["reported_at"].as_i64().unwrap_or(0)));
            println!("{:>4}s into the install: online={online} reported_at={reported_at}", start.elapsed().as_secs());
            seen.push((online, reported_at));
        }
        task.abort();
        assert!(start.elapsed().as_secs() > 90, "the install outlasted the online window");
        assert!(seen.iter().all(|(online, _)| *online), "online the whole time: {seen:?}");
        let reports: std::collections::BTreeSet<i64> = seen.iter().map(|(_, at)| *at).collect();
        assert!(reports.len() as u64 >= secs / 30, "a fresh report every 30s: {reports:?}");
    }

    /// Activity center C1 on the shared dev environment: two heartbeats in a
    /// row from one process carry the runtimes' login state, the server keeps
    /// it, and the second didn't ask Anthropic again (same `checked_at`).
    /// Reads THIS machine's runtimes and logins, as `mafold status` does.
    /// `MAFOLD_TEST_API_ORIGIN=$E2E_API MAFOLD_TEST_USER=$U cargo test -- --ignored --exact
    /// supervisor::heartbeat_e2e::live_two_heartbeats_keep_login_state --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_two_heartbeats_keep_login_state() {
        let base = std::env::var("MAFOLD_TEST_API_ORIGIN").expect("dev API origin");
        assert!(base.starts_with("http://127.0.0.1:"), "dev only: {base}");
        let user = std::env::var("MAFOLD_TEST_USER").expect("the e2e run's test account");
        let sess = crate::session::Session {
            token: format!("dev:{user}"),
            username: user,
            device_id: "e2e-c1-heartbeat".into(),
            device_name: "E2E C1 heartbeat".into(),
        };
        let http = reqwest::Client::new();
        let client = crate::client::Client::new(base.clone(), sess.token.clone());
        let claude = || async {
            let r = client.call("listHarnessHosts", serde_json::json!({})).await.expect("listHarnessHosts");
            let r = r.get("result").cloned().unwrap_or(r);
            let host = r["items"].as_array().into_iter().flatten().find(|h| h["device_id"] == "e2e-c1-heartbeat").cloned().expect("this device is listed");
            host["harnesses"].as_array().into_iter().flatten().find(|h| h["id"] == "claude-code").cloned().expect("claude-code row")
        };
        let mut seen = Vec::new();
        for beat in 1..=2 {
            let t = std::time::Instant::now();
            super::report_local_harnesses(&http, &base, &sess, &scratch_ledger("c1")).await.expect("heartbeat");
            let row = claude().await;
            println!(
                "heartbeat {beat} ({} ms): available={} version={} health={} checked_at={} logins={}",
                t.elapsed().as_millis(),
                row["available"],
                row["version"],
                row["health"]["state"],
                row["health"]["checked_at"],
                row["accounts"].as_array().map_or(0, Vec::len),
            );
            seen.push(row);
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        assert_eq!(seen[0]["available"], true, "this machine has Claude Code");
        assert!(seen[0]["health"].is_object(), "a heartbeat now says whether Claude Code is signed in");
        assert!(!seen[0]["accounts"].as_array().unwrap().is_empty(), "and lists its logins");
        assert_eq!(seen[1]["health"], seen[0]["health"], "still there after the second heartbeat, not re-asked (same checked_at)");
        assert_eq!(seen[1]["accounts"], seen[0]["accounts"]);
    }

    /// A real session on the dev environment (password sign-in).
    async fn sign_in(base: &str, user: &str, password: &str) -> String {
        let v: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/api/auth/login"))
            .json(&serde_json::json!({ "username": user, "password": password }))
            .send()
            .await
            .expect("auth/login")
            .json()
            .await
            .expect("auth/login json");
        v.pointer("/result/token").and_then(|t| t.as_str()).unwrap_or_else(|| panic!("a session token: {v}")).to_string()
    }

    /// Activity center C4 on the shared dev environment: a login signed out
    /// server-side (`auth/logout`, as Settings ▸ Active Sessions does) makes
    /// the next heartbeat land as «signed out» on disk; signing in again reads
    /// as signed in at once and the next heartbeat clears it.
    /// `MAFOLD_TEST_API_ORIGIN=$E2E_API MAFOLD_TEST_USER=$U MAFOLD_TEST_PASSWORD=… cargo test -- --ignored --exact
    /// supervisor::heartbeat_e2e::live_a_signed_out_login_is_seen_and_cleared --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_a_signed_out_login_is_seen_and_cleared() {
        let base = std::env::var("MAFOLD_TEST_API_ORIGIN").expect("dev API origin");
        assert!(base.starts_with("http://127.0.0.1:"), "dev only: {base}");
        let user = std::env::var("MAFOLD_TEST_USER").expect("the e2e run's test account");
        let password = std::env::var("MAFOLD_TEST_PASSWORD").expect("its password");
        let ledger = scratch_ledger("c4");
        let health = ledger.path.clone();
        let http = reqwest::Client::new();
        let body = serde_json::json!({ "device_id": "e2e-c4-heartbeat", "device_name": "E2E C4", "harnesses": [] });
        let as_ = |token: &str| crate::session::Session {
            token: token.into(),
            username: user.clone(),
            device_id: "e2e-c4-heartbeat".into(),
            device_name: "E2E C4".into(),
        };

        let first = sign_in(&base, &user, &password).await;
        super::send_report(&http, &base, &as_(&first), &body, &ledger).await.expect("taken");
        let h = crate::account_health::of_at(&health, &user, &first);
        println!("signed in:        {h:?}");
        assert!(h.ok_at.is_some() && h.refused.is_none());

        crate::client::Client::new(base.clone(), first.clone()).call("auth/logout", serde_json::json!({})).await.expect("logout");
        let refused = super::send_report(&http, &base, &as_(&first), &body, &ledger).await;
        let h = crate::account_health::of_at(&health, &user, &first);
        println!("after logout:     {refused:?} → {h:?}");
        assert!(refused.is_err(), "the api refuses a signed-out login");
        assert!(h.refused.is_some(), "and that lands on disk");
        println!("status line:      {}", crate::account_health::line(&user, &h, crate::accounts::now()));

        let second = sign_in(&base, &user, &password).await;
        let h = crate::account_health::of_at(&health, &user, &second);
        println!("signed in again:  {h:?}");
        assert!(h.refused.is_none(), "a new login reads as signed in before any heartbeat");
        super::send_report(&http, &base, &as_(&second), &body, &ledger).await.expect("taken again");
        let raw = std::fs::read_to_string(&health).unwrap();
        println!("on disk:          {raw}");
        assert!(!raw.contains("refused"), "cleared by the next accepted heartbeat");
        assert!(!raw.contains(&first) && !raw.contains(&second), "no token on disk");
        let _ = std::fs::remove_dir_all(health.parent().unwrap());
    }
}
