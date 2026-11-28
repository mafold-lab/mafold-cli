//! `mafold install claude-code --yes` on Windows, for real — Anthropic's own
//! PowerShell installer, run the way the supervisor runs a remote install
//! (activity center C12). Before it, every install on Windows was refused
//! outright (the installers were all `bash -c`).
//!
//! It downloads and installs Claude Code, so it's ignored by default and run
//! on a throwaway CI runner (`scripts/ci/public-cli/cli-pr-tests.yml`, with
//! `--ignored`) — into a throwaway USERPROFILE, never the machine's own.
#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mafold-install-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("make temp home");
    dir
}

#[test]
#[ignore = "installs Claude Code — run on a throwaway CI runner"]
fn claude_code_installs_with_anthropics_own_windows_installer() {
    let home = temp_home("claude");
    // The runner's PATH, minus anything that already has a `claude`: the
    // install has to be what puts it there.
    let path: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|d| !d.join("claude.exe").is_file() && !d.join("claude.cmd").is_file())
        .collect();
    // Run from a PowerShell 7 terminal: 7's modules first on PSModulePath,
    // which once cost the installer its `Get-FileHash`.
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    assert!(PathBuf::from(format!(r"{pf}\PowerShell\7\Modules")).is_dir(), "this runner has PowerShell 7");
    let ps7_modules = format!(r"{pf}\PowerShell\7\Modules;{}", std::env::var("PSModulePath").unwrap_or_default());
    let out = Command::new(env!("CARGO_BIN_EXE_mafold"))
        .args(["install", "claude-code", "--yes"])
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("PSModulePath", ps7_modules)
        .env_remove("MAFOLD_BOT_TOKEN")
        .env_remove("MAFOLD_DAEMON_NAME")
        .stdin(Stdio::null())
        .output()
        .expect("run mafold install");
    let said = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{said}");
    assert!(said.contains("irm https://claude.ai/install.ps1 | iex"), "the installer it ran is named: {said}");
    let claude = home.join(r".local\bin\claude.exe");
    assert!(claude.is_file(), "installed where the native installer puts it: {said}");
    // Not on this process's PATH yet — said as such, not as a failure.
    assert!(said.contains("✓ installed to"), "{said}");
    let v = Command::new(&claude).arg("--version").env("USERPROFILE", &home).output().expect("run the installed claude");
    assert!(String::from_utf8_lossy(&v.stdout).contains("Claude Code"), "{}", String::from_utf8_lossy(&v.stdout));
}

#[test]
fn kimi_is_refused_on_windows_before_anything_runs() {
    let home = temp_home("kimi");
    let path: Vec<PathBuf> = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|d| !d.join("kimi.exe").is_file() && !d.join("kimi.cmd").is_file())
        .collect();
    let out = Command::new(env!("CARGO_BIN_EXE_mafold"))
        .args(["install", "kimi", "--yes"])
        .env("USERPROFILE", &home)
        .env("PATH", std::env::join_paths(path).unwrap())
        .stdin(Stdio::null())
        .output()
        .expect("run mafold install");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(said.contains("isn't supported on Windows") && said.contains("Kimi Code"), "{said}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("About to run"), "nothing was run");
    let _ = std::fs::remove_dir_all(&home);
}
