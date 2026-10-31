//! `mafold install <runtime>` — install a coding-agent runtime the Mafold agent
//! drives (claude-code / codex / kimi-code / opencode).
//!
//! Each runtime: detect an existing install, otherwise run its OFFICIAL
//! installer (with confirmation; `--yes` for headless), re-verify, and point at
//! the next onboarding step. OpenCode is the exception: Mafold runs its own
//! pinned build of it on the wallet (`harness::opencode_pin`), so that is what
//! gets installed, checked against its pinned hash, under `~/.mafold/runtimes`
//! — whatever `opencode` the user may have on PATH is left alone and never
//! counts. `mafold install` with no argument lists the
//! runtimes + their install state. The web New-Bot modal's one-click install
//! rides this too: the supervisor claims a queued install (`claimProvisions`
//! → `installs`) and calls [`run`] with `yes = true`. @mafold teaches this
//! command in-app, so keep behaviour aligned with the product guide
//! (mafold-api brains/prompts.rs) and the api's `INSTALLABLE_RUNTIMES`.

use anyhow::{bail, Result};

/// One installable runtime. Order = the product's canonical display order
/// (claude code / codex / kimi / opencode) — the web install page mirrors it.
struct Tool {
    /// Stable harness id (matches `harness::KNOWN` / the api).
    id: &'static str,
    /// Accepted aliases for the CLI argument.
    aliases: &'static [&'static str],
    /// Human name for prints.
    name: &'static str,
    /// The binary whose presence on PATH means "installed" (an official
    /// install; a pinned one is its own path, [`How::Pinned`]).
    bin: &'static str,
    /// How it gets onto this machine.
    how: How,
    /// Manual fallback shown when the installer fails.
    fallback: &'static str,
    /// Post-install auth step (every runtime authenticates on this machine).
    auth_hint: &'static str,
}

/// Where a runtime's install comes from.
#[derive(Clone, Copy)]
enum How {
    /// The vendor's own installer: `unix` run via `bash -c` (macOS, Linux),
    /// and on Windows the vendor's own one, or why there is none to run.
    Official { unix: &'static str, windows: Win },
    /// A build Mafold pins: `install` fetches and checks it, `path` is where
    /// it lives. The PATH is never consulted for it.
    Pinned {
        version: &'static str,
        path: fn() -> std::path::PathBuf,
        install: fn(Option<std::time::Duration>) -> Result<std::path::PathBuf>,
    },
}

const TOOLS: &[Tool] = &[
    Tool {
        id: "claude-code",
        aliases: &["claude-code", "claude", "claudecode"],
        name: "Claude Code",
        bin: "claude",
        how: How::Official {
            unix: "curl -fsSL https://claude.ai/install.sh | bash",
            // Anthropic's PowerShell installer (claude.com/claude-code): the
            // native build into %USERPROFILE%\.local\bin.
            windows: Win::PowerShell("irm https://claude.ai/install.ps1 | iex"),
        },
        fallback: "try `npm install -g @anthropic-ai/claude-code` or see https://claude.com/claude-code",
        auth_hint: "run `claude` once in a project folder to sign in to Anthropic",
    },
    Tool {
        id: "codex",
        aliases: &["codex", "codex-cli"],
        name: "Codex",
        bin: "codex",
        how: How::Official {
            unix: "npm install -g @openai/codex",
            // OpenAI ships Codex for Windows as the same npm package.
            windows: Win::PowerShell("npm install -g @openai/codex"),
        },
        fallback: "needs Node/npm on PATH — `brew install node` first, or see https://developers.openai.com/codex/cli",
        auth_hint: "run `codex` once to sign in to OpenAI",
    },
    Tool {
        id: "kimi-code",
        aliases: &["kimi-code", "kimi", "kimi-cli", "kimicode"],
        name: "Kimi Code",
        bin: "kimi",
        how: How::Official {
            unix: "curl -LsSf https://code.kimi.com/install.sh | bash",
            // Its PowerShell installer (code.kimi.com/install.ps1) now installs
            // the deprecated Python kimi-cli only when told to, and otherwise
            // hands over to the next-generation Kimi Code — not this runtime.
            windows: Win::Unsupported(
                "Kimi's Windows installer is moving from kimi-cli to its next-generation Kimi Code, so Mafold doesn't install it here",
            ),
        },
        fallback: "see https://moonshotai.github.io/kimi-cli/",
        auth_hint: "run `kimi login` to sign in to Moonshot",
    },
    Tool {
        id: "opencode",
        aliases: &["opencode", "open-code"],
        name: "OpenCode",
        bin: "opencode",
        // Mafold Harness: the one build its event mapping was measured
        // against, never `curl | bash` of whatever is newest.
        how: How::Pinned {
            version: crate::harness::opencode_pin::VERSION,
            path: crate::harness::opencode_pin::binary,
            install: crate::harness::opencode_pin::ensure_blocking,
        },
        fallback: "check that this computer can reach registry.npmjs.org or registry.npmmirror.com, then run `mafold install opencode` again",
        auth_hint: "nothing to sign in to — Mafold runs it on your Mafold wallet",
    },
];

/// A runtime's Windows install.
#[derive(Clone, Copy)]
enum Win {
    /// Run by Windows PowerShell (`irm … | iex`, `npm …`).
    PowerShell(&'static str),
    /// Not installed automatically — why, said as such instead of trying.
    Unsupported(&'static str),
}

/// What the installer runs: a `bash -c` line, or a PowerShell one.
#[derive(Debug, PartialEq)]
enum Installer {
    Bash(&'static str),
    PowerShell(&'static str),
}

/// `t`'s installer on this platform; `Err` = why there's none.
fn installer(t: &Tool, windows: bool) -> std::result::Result<Installer, &'static str> {
    let How::Official { unix, windows: win } = t.how else {
        return Err("Mafold installs its own pinned build of it");
    };
    match (windows, win) {
        (false, _) => Ok(Installer::Bash(unix)),
        (true, Win::PowerShell(cmd)) => Ok(Installer::PowerShell(cmd)),
        (true, Win::Unsupported(why)) => Err(why),
    }
}

/// What to look for to call `t` installed: its name on PATH, or the path of
/// Mafold's pinned build.
fn program_of(t: &Tool) -> String {
    match t.how {
        How::Official { .. } => t.bin.to_string(),
        How::Pinned { path, .. } => path().to_string_lossy().into_owned(),
    }
}

/// An installed `t`'s version: what its CLI says, or the pin's — which is
/// what a pinned build is by construction, and asking it would make
/// OpenCode's folders in the user's home (`harness::opencode_pin`).
fn installed_version(t: &Tool) -> Option<String> {
    match t.how {
        How::Official { .. } => version_of(t.bin),
        How::Pinned { version, .. } => Some(version.to_string()),
    }
}

/// The PowerShell line that runs `cmd` so a failure is an exit code: an error
/// stops it (`irm` that can't download, a script that throws), and a native
/// command's own exit code (`npm`) is the process's. PowerShell otherwise
/// exits 0 after either.
fn powershell_script(cmd: &str) -> String {
    format!("$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; $global:LASTEXITCODE = 0; {cmd}; exit $LASTEXITCODE")
}

/// The arguments Windows PowerShell gets: no profile (a user's profile is not
/// the installer's business), nothing interactive, the script policy of this
/// one process only.
fn powershell_args(cmd: &str) -> Vec<String> {
    ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"]
        .into_iter()
        .map(str::to_string)
        .chain(std::iter::once(powershell_script(cmd)))
        .collect()
}

/// Where an installer that installs into the user's home puts `bin`, as a
/// launchable file name: `~/.local/bin/<bin>` (Claude Code's native installer,
/// on every OS).
fn home_install(home: &std::path::Path, bin: &str, windows: bool) -> Vec<std::path::PathBuf> {
    let dir = home.join(".local").join("bin");
    if windows {
        [".exe", ".cmd"].iter().map(|e| dir.join(format!("{bin}{e}"))).collect()
    } else {
        vec![dir.join(bin)]
    }
}

fn find(tool: &str) -> Option<&'static Tool> {
    let q = tool.trim().to_lowercase();
    TOOLS.iter().find(|t| t.aliases.contains(&q.as_str()))
}

pub fn run(tool: &str, yes: bool) -> Result<()> {
    run_within(tool, yes, None)
}

/// [`run`], with the official installer stopped (it and everything it
/// started) once it has run for `limit`. The supervisor installs with a limit:
/// an installer stuck on a download used to keep the machine busy for good.
pub fn run_within(tool: &str, yes: bool, limit: Option<std::time::Duration>) -> Result<()> {
    if tool.trim().is_empty() || tool == "list" {
        return list();
    }
    let Some(t) = find(tool) else {
        bail!(
            "unknown runtime `{tool}` — available: {}",
            TOOLS.iter().map(|t| t.id).collect::<Vec<_>>().join(", ")
        );
    };
    install(t, yes, limit)
}

/// `mafold install` with no argument: the four runtimes + their install state.
fn list() -> Result<()> {
    println!("runtimes:");
    for t in TOOLS {
        let state = if crate::harness::on_path(&program_of(t)) {
            format!(
                "✓ installed{}{}",
                installed_version(t)
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default(),
                if matches!(t.how, How::Pinned { .. }) { ", pinned by Mafold" } else { "" }
            )
        } else {
            format!("· not installed — `mafold install {}`", t.id)
        };
        println!("  {:<12} {}", t.id, state);
    }
    Ok(())
}

fn install(t: &Tool, yes: bool, limit: Option<std::time::Duration>) -> Result<()> {
    if crate::harness::on_path(&program_of(t)) {
        println!(
            "✓ {} already installed ({})",
            t.name,
            installed_version(t).unwrap_or_else(|| "version unknown".into())
        );
        next_steps(t);
        return Ok(());
    }
    if let How::Pinned { version, path, install } = t.how {
        println!("Mafold runs its own pinned build of {} ({version}).", t.name);
        println!("About to download it from npm (whichever of registry.npmjs.org and registry.npmmirror.com delivers first), check it");
        println!("against its pinned SHA-512, and install it to {}.", path().display());
        if !yes && !confirmed()? {
            return Ok(());
        }
        let bin = install(limit).map_err(|e| anyhow::anyhow!("{e:#} — {}", t.fallback))?;
        println!("✓ {} {version} installed ({})", t.name, bin.display());
        next_steps(t);
        return Ok(());
    }

    // Said before anything runs, and without "try again": there is nothing
    // here to try.
    let how = match installer(t, cfg!(windows)) {
        Ok(how) => how,
        Err(why) => bail!("automatic install of {} isn't supported on Windows — {why}; {}", t.name, t.fallback),
    };
    println!("{} (`{}`) not found on PATH.", t.name, t.bin);
    println!("About to run the official installer:");
    println!("  {}", match &how {
        Installer::Bash(c) | Installer::PowerShell(c) => c,
    });
    if !yes && !confirmed()? {
        return Ok(());
    }

    // A shell that won't start leaves the manual route in `fallback` as the
    // next step — never a bare "failed to run bash".
    let status = match run_installer(&how, limit) {
        Ok(Some(s)) => s,
        Ok(None) => bail!(
            "the installer was still running after {} minutes — stopped it; {}",
            limit.unwrap_or_default().as_secs() / 60,
            t.fallback
        ),
        Err(e) => bail!("couldn't run the installer ({e}) — {}", t.fallback),
    };
    if !status.success() {
        bail!("installer exited with {status} — {}", t.fallback);
    }

    if crate::harness::on_path(t.bin) {
        println!(
            "✓ {} installed ({})",
            t.name,
            version_of(t.bin).unwrap_or_else(|| "version unknown".into())
        );
    } else {
        // Installers default to ~/.local/bin (or npm's prefix), which may not be
        // on PATH in this shell yet — installed-but-unresolvable is the common
        // case, not an error.
        let home = crate::platform::home_dir().unwrap_or_else(|| ".".into());
        if let Some(local) = home_install(&home, t.bin, cfg!(windows)).into_iter().find(|p| p.is_file()) {
            println!("✓ installed to {} — open a new terminal (or add ~/.local/bin to PATH) so `{}` resolves", local.display(), t.bin);
        } else {
            bail!("installer finished but `{}` isn't on PATH — open a new terminal and re-check, or: {}", t.bin, t.fallback);
        }
    }
    next_steps(t);
    Ok(())
}

/// Asked at a terminal before anything is installed; `false` = they said no
/// (and were told nothing was installed).
fn confirmed() -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal — re-run with --yes to install non-interactively");
    }
    let answer = crate::prompt("Proceed? [y/N] ");
    if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
        println!("aborted — nothing installed.");
        return Ok(false);
    }
    Ok(true)
}

/// Run the installer — `bash -c`, or Windows PowerShell by its full path (a
/// service's PATH is no promise). With a `limit` it runs as a tree of its own,
/// and when the limit passes the whole tree is killed — an installer is a
/// pipeline (`curl … | bash`, `irm … | iex` downloading), so killing the shell
/// alone would leave the download running. `Ok(None)` = it was stopped.
/// Without a limit it stays in the caller's group: `mafold install` in a
/// terminal may ask its questions.
fn run_installer(
    how: &Installer,
    limit: Option<std::time::Duration>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut c = match how {
        Installer::Bash(cmd) => {
            let mut c = crate::platform::console_std_command("bash");
            c.arg("-c").arg(cmd);
            c
        }
        Installer::PowerShell(cmd) => {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            let mut c = crate::platform::console_std_command(format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe"));
            c.args(powershell_args(cmd));
            // Started from PowerShell 7 (a pwsh terminal, or a supervisor
            // that one started), PSModulePath lists 7's own modules first,
            // and Windows PowerShell autoloads `Get-FileHash` from 7's
            // Microsoft.PowerShell.Utility, which it can't load: Claude Code's
            // installer dies at its checksum. Without the variable, Windows
            // PowerShell builds its own module path.
            c.env_remove("PSModulePath");
            c
        }
    };
    let Some(limit) = limit else {
        return c.status().map(Some);
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let mut child = c.spawn()?;
    // Into the job at once, before the shell has started anything.
    #[cfg(windows)]
    let tree = {
        use std::os::windows::io::AsRawHandle;
        crate::platform::JobTree::of(child.as_raw_handle())
    };
    let end = std::time::Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if std::time::Instant::now() >= end {
            #[cfg(unix)]
            // SAFETY: plain kill(2) on the group this child leads.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(windows)]
            if let Some(tree) = &tree {
                tree.kill();
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn version_of(bin: &str) -> Option<String> {
    let out = crate::platform::std_command(crate::harness::program(bin))
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn next_steps(t: &Tool) {
    println!();
    println!("next steps:");
    println!("  1. {}", t.auth_hint);
    println!(
        "  2. in Mafold: pencil icon → New Bot → Runtime: {}",
        t.name
    );
}

#[cfg(all(test, unix))]
mod tests {
    use super::{run_installer, Installer};
    use std::time::{Duration, Instant};

    /// A stuck installer is stopped at its limit — and so is what it started
    /// (`curl … | bash` is a pipeline; the download is a grandchild).
    #[test]
    fn a_stuck_installer_is_stopped_with_everything_it_started() {
        let dir = std::env::temp_dir().join(format!("mafold-install-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("grandchild.pid");
        let cmd = format!("sleep 60 & echo $! > '{}'; wait", pidfile.display());
        let t = Instant::now();
        let ran = run_installer(&Installer::Bash(Box::leak(cmd.into_boxed_str())), Some(Duration::from_secs(1))).unwrap();
        assert!(ran.is_none(), "reported as stopped, not as an exit");
        assert!(t.elapsed() < Duration::from_secs(10), "stopped at the limit: {:?}", t.elapsed());
        let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        // SAFETY: signal 0 only probes.
        let alive = unsafe { libc::kill(grandchild, 0) } == 0;
        assert!(!alive, "the grandchild went with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_installer_that_finishes_in_time_reports_its_exit() {
        let ok = run_installer(&Installer::Bash("exit 0"), Some(Duration::from_secs(30))).unwrap().unwrap();
        assert!(ok.success());
        let failed = run_installer(&Installer::Bash("exit 3"), Some(Duration::from_secs(30))).unwrap().unwrap();
        assert_eq!(failed.code(), Some(3));
        assert!(run_installer(&Installer::Bash("exit 0"), None).unwrap().unwrap().success(), "no limit: as before");
    }
}

#[cfg(test)]
mod windows_installer_tests {
    use super::*;

    fn tool(id: &str) -> &'static Tool {
        find(id).expect(id)
    }

    /// Every runtime with an official installer still installs with its bash
    /// line on macOS and Linux.
    #[test]
    fn unix_runs_each_official_installer_under_bash() {
        for t in TOOLS {
            if let How::Official { unix, .. } = t.how {
                assert_eq!(installer(t, false), Ok(Installer::Bash(unix)), "{}", t.id);
            }
        }
    }

    /// OpenCode is Mafold's pinned build on every OS: no vendor installer,
    /// no npm -g, nothing found on PATH standing in for it.
    #[test]
    fn opencode_installs_the_pinned_build_on_every_os() {
        let t = tool("open-code");
        let How::Pinned { version, path, .. } = t.how else { panic!("opencode is pinned") };
        assert_eq!(version, crate::harness::opencode_pin::VERSION);
        assert_eq!(path(), crate::harness::opencode_pin::binary());
        assert_eq!(program_of(t), crate::harness::opencode_pin::binary().to_string_lossy());
        assert!(installer(t, false).is_err() && installer(t, true).is_err(), "no official installer is run for it");
    }

    /// Windows runs the vendor's own installer — never bash, which a stock
    /// Windows doesn't have — or says why there's none, before trying.
    #[test]
    fn windows_runs_each_vendors_own_installer_or_says_why_not() {
        assert_eq!(installer(tool("claude-code"), true), Ok(Installer::PowerShell("irm https://claude.ai/install.ps1 | iex")));
        assert_eq!(installer(tool("codex"), true), Ok(Installer::PowerShell("npm install -g @openai/codex")));
        let why = installer(tool("kimi"), true).expect_err("no Windows install for Kimi");
        assert!(why.contains("Kimi Code"), "{why}");
        for t in TOOLS {
            if let Ok(Installer::PowerShell(cmd)) = installer(t, true) {
                assert!(!cmd.contains("bash") && !cmd.contains("curl"), "{}: {cmd}", t.id);
            }
        }
    }

    /// The same npm package on both: Windows doesn't install something else.
    #[test]
    fn the_npm_routes_install_the_package_the_unix_side_names() {
        let How::Official { unix, windows } = tool("codex").how else { panic!("codex is official") };
        assert_eq!(unix, "npm install -g @openai/codex");
        assert!(matches!(windows, Win::PowerShell(cmd) if cmd == unix));
    }

    /// A failure is an exit code: an error stops the script, a native
    /// command's code is the process's, and no profile or prompt gets a say.
    #[test]
    fn powershell_gets_a_script_whose_failure_is_its_exit_code() {
        let args = powershell_args("irm https://claude.ai/install.ps1 | iex");
        assert_eq!(&args[..5], ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"]);
        assert_eq!(
            args[5],
            "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; $global:LASTEXITCODE = 0; irm https://claude.ai/install.ps1 | iex; exit $LASTEXITCODE"
        );
        assert_eq!(args.len(), 6);
    }

    /// Installed but not on this shell's PATH yet: found where the native
    /// installer puts it, as Windows names it.
    #[test]
    fn a_home_install_is_looked_for_under_its_windows_names() {
        let home = std::path::Path::new("/h");
        assert_eq!(
            home_install(home, "claude", true),
            vec![home.join(".local/bin/claude.exe"), home.join(".local/bin/claude.cmd")]
        );
        assert_eq!(home_install(home, "claude", false), vec![home.join(".local/bin/claude")]);
    }
}
