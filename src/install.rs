//! `mafold install <runtime>` — install a coding-agent runtime the Mafold agent
//! drives (claude-code / codex / kimi-code / opencode).
//!
//! Each runtime: detect an existing install, otherwise run its OFFICIAL
//! installer (with confirmation; `--yes` for headless), re-verify, and point at
//! the next onboarding step. `mafold install` with no argument lists the
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
    /// The binary whose presence on PATH means "installed".
    bin: &'static str,
    /// The official installer, run via `bash -c`.
    install_cmd: &'static str,
    /// Manual fallback shown when the installer fails.
    fallback: &'static str,
    /// Post-install auth step (every runtime authenticates on this machine).
    auth_hint: &'static str,
}

const TOOLS: &[Tool] = &[
    Tool {
        id: "claude-code",
        aliases: &["claude-code", "claude", "claudecode"],
        name: "Claude Code",
        bin: "claude",
        install_cmd: "curl -fsSL https://claude.ai/install.sh | bash",
        fallback: "try `npm install -g @anthropic-ai/claude-code` or see https://claude.com/claude-code",
        auth_hint: "run `claude` once in a project folder to sign in to Anthropic",
    },
    Tool {
        id: "codex",
        aliases: &["codex", "codex-cli"],
        name: "Codex",
        bin: "codex",
        install_cmd: "npm install -g @openai/codex",
        fallback: "needs Node/npm on PATH — `brew install node` first, or see https://developers.openai.com/codex/cli",
        auth_hint: "run `codex` once to sign in to OpenAI",
    },
    Tool {
        id: "kimi-code",
        aliases: &["kimi-code", "kimi", "kimi-cli", "kimicode"],
        name: "Kimi Code",
        bin: "kimi",
        install_cmd: "curl -LsSf https://code.kimi.com/install.sh | bash",
        fallback: "see https://moonshotai.github.io/kimi-cli/",
        auth_hint: "run `kimi login` to sign in to Moonshot",
    },
    Tool {
        id: "opencode",
        aliases: &["opencode", "open-code"],
        name: "OpenCode",
        bin: "opencode",
        install_cmd: "curl -fsSL https://opencode.ai/install | bash",
        fallback: "try `npm install -g opencode-ai` or see https://opencode.ai",
        auth_hint: "run `opencode auth login` to connect a provider",
    },
];

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
        let state = if crate::harness::on_path(t.bin) {
            format!(
                "✓ installed{}",
                version_of(t.bin)
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default()
            )
        } else {
            format!("· not installed — `mafold install {}`", t.id)
        };
        println!("  {:<12} {}", t.id, state);
    }
    Ok(())
}

fn install(t: &Tool, yes: bool, limit: Option<std::time::Duration>) -> Result<()> {
    if cfg!(windows) {
        bail!(
            "automatic install isn't supported on Windows yet — {}",
            t.fallback
        );
    }
    if crate::harness::on_path(t.bin) {
        println!(
            "✓ {} already installed ({})",
            t.name,
            version_of(t.bin).unwrap_or_else(|| "version unknown".into())
        );
        next_steps(t);
        return Ok(());
    }

    println!("{} (`{}`) not found on PATH.", t.name, t.bin);
    println!("About to run the official installer:");
    println!("  {}", t.install_cmd);
    if !yes {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal — re-run with --yes to install non-interactively");
        }
        let answer = crate::prompt("Proceed? [y/N] ");
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
            println!("aborted — nothing installed.");
            return Ok(());
        }
    }

    // `bash` is the installer's shell everywhere it exists; where it doesn't (a
    // stock Windows box) the manual route in `fallback` IS the answer. Bailing
    // with a bare "failed to run bash" left the user with no next step on the one
    // platform that had no other way in.
    let status = match run_installer(t.install_cmd, limit) {
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
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let local = std::path::Path::new(&home).join(".local/bin").join(t.bin);
        if local.is_file() {
            println!("✓ installed to {} — open a new terminal (or add ~/.local/bin to PATH) so `{}` resolves", local.display(), t.bin);
        } else {
            bail!("installer finished but `{}` isn't on PATH — open a new terminal and re-check, or: {}", t.bin, t.fallback);
        }
    }
    next_steps(t);
    Ok(())
}

/// Run `cmd` under `bash -c`. With a `limit` it runs in its own process
/// group, and when the limit passes the whole group is killed — an installer
/// is a pipeline (`curl … | bash`), so killing `bash` alone would leave the
/// download running. `Ok(None)` = it was stopped. Without a limit it stays in
/// the caller's group: `mafold install` in a terminal may ask its questions.
fn run_installer(
    cmd: &str,
    limit: Option<std::time::Duration>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let mut c = crate::platform::console_std_command("bash");
    c.arg("-c").arg(cmd);
    let Some(limit) = limit else {
        return c.status().map(Some);
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let mut child = c.spawn()?;
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
    use super::run_installer;
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
        let ran = run_installer(&cmd, Some(Duration::from_secs(1))).unwrap();
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
        let ok = run_installer("exit 0", Some(Duration::from_secs(30))).unwrap().unwrap();
        assert!(ok.success());
        let failed = run_installer("exit 3", Some(Duration::from_secs(30))).unwrap().unwrap();
        assert_eq!(failed.code(), Some(3));
        assert!(run_installer("exit 0", None).unwrap().unwrap().success(), "no limit: as before");
    }
}
