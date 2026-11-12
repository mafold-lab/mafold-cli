//! `mafold sandbox …` — Python in the reply's own sandbox, for an agent on a
//! computer. The same `sandboxRun` the hosted kernel's `python` tool goes
//! through (`mafold-api/src/sandbox_api.rs` decides who may run what and who
//! pays), and the same reading of its answer (`mafold_types::sandbox`), so a
//! run says the same thing wherever the agent runs.
//!
//! The reply is the turn's own, as for `mafold attach`. What the code writes to
//! /mnt/data/out comes back on that reply. A file on this machine is uploaded
//! first. A file someone sent is saved here already, and the server knows its
//! bytes, so the upload hands back the conversation's own file; anything else
//! the agent uploads is its own (the api takes either).

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use mafold_core::mafold_types::sandbox::run_text;

#[derive(Subcommand)]
pub enum SandboxCmd {
    /// Run Python 3.13 in this reply's sandbox: 1 vCPU, 3 GiB, no network,
    /// nothing can be installed. Files persist between runs in the same reply;
    /// variables don't. Every file the code writes to /mnt/data/out is attached
    /// to the reply. Billed to the agent's owner, in the `sandbox` currency.
    Run {
        /// The script: a .py file on this machine, or `-` to read it from stdin.
        #[arg(required_unless_present = "code", conflicts_with = "code")]
        script: Option<String>,
        /// The code itself, instead of a script.
        #[arg(short = 'c', long)]
        code: Option<String>,
        /// A file to put in /mnt/data/in under its own name: a path on this
        /// machine (uploaded first) or a file id from this conversation.
        /// Repeat it for more (at most 20).
        #[arg(long = "file")]
        files: Vec<String>,
        /// Seconds this run may take: 1–300. The server's default is 120.
        #[arg(long)]
        timeout: Option<u64>,
        /// The checks and the price as JSON. Nothing is started or held.
        #[arg(long)]
        dry_run: bool,
        /// Print the server's whole answer as JSON instead of the run's text.
        #[arg(long)]
        json: bool,
        /// The reply to run for. Defaults to the turn's own (`$MAFOLD_DRAFT`).
        #[arg(long)]
        message: Option<String>,
    },
    /// End this reply's sandbox now and bill it. Optional: it ends with the
    /// reply anyway.
    End {
        /// The reply whose sandbox to end. Defaults to the turn's own.
        #[arg(long)]
        message: Option<String>,
    },
}

pub async fn run(cmd: SandboxCmd, client: &Client) -> Result<()> {
    match cmd {
        SandboxCmd::Run { script, code, files, timeout, dry_run, json, message } => {
            let code = match (code, script.as_deref()) {
                (Some(c), _) => c,
                (None, Some("-")) => std::io::read_to_string(std::io::stdin()).context("reading the code from stdin")?,
                (None, Some(p)) => std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?,
                (None, None) => bail!("no code — give a script, `-` for stdin, or -c '<code>'"),
            };
            let draft = reply(message.as_deref()).await?;
            let mut inputs = Vec::with_capacity(files.len());
            for f in &files {
                inputs.push(input_id(client, f).await?);
            }
            let v = client.call("sandboxRun", run_body(&draft, &code, inputs, timeout, dry_run)).await?;
            if json || dry_run {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{}", run_text(&v));
            }
        }
        SandboxCmd::End { message } => {
            let draft = reply(message.as_deref()).await?;
            let v = client.call("sandboxEnd", json!({ "draft": draft })).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

/// The reply a run is for: `--message`, else the turn's own as it stands now
/// (`agent::current_draft` follows a steer to the fresh draft, and opens an
/// update turn's held bubble — a run attaches its files to it).
async fn reply(message: Option<&str>) -> Result<String> {
    if let Some(m) = message {
        return Ok(m.to_string());
    }
    let env_id = crate::turnenv::draft().context(
        "no reply to run for — run this inside an agent turn (the daemon sets MAFOLD_DRAFT), or pass --message <id>",
    )?;
    crate::agent::current_draft(env_id).await
}

/// `sandboxRun`'s body (`RunBody`, mafold-api/src/sandbox_api.rs).
fn run_body(draft: &str, code: &str, inputs: Vec<String>, timeout: Option<u64>, dry_run: bool) -> Value {
    let mut body = json!({ "draft": draft, "code": code, "inputs": inputs });
    if let Some(t) = timeout {
        body["timeout_s"] = json!(t);
    }
    if dry_run {
        body["dry_run"] = json!(true);
    }
    body
}

/// The id `sandboxRun` takes for `--file f`: a file on this machine is uploaded
/// and goes by the id the upload answers with; anything else is an id already.
async fn input_id(client: &Client, f: &str) -> Result<String> {
    let path = std::path::Path::new(f);
    if path.is_file() {
        let up = client.upload_path(path).await.with_context(|| format!("uploading {f}"))?;
        return up["id"].as_str().map(str::to_string).context("the upload answered without a file id");
    }
    named_id(f)
}

/// [`input_id`] for something that isn't a file here: a bare id, `mf-file:<id>`,
/// or `/media/<id>` (how a file too large to download is named to the agent).
/// Anything else with a slash in it was meant as a path.
fn named_id(f: &str) -> Result<String> {
    let id = f.trim();
    let id = id.strip_prefix("/media/").unwrap_or(id);
    if id.is_empty() || id.contains('/') || id.contains('\\') {
        bail!("{f}: no such file on this machine");
    }
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        cmd: SandboxCmd,
    }

    #[test]
    fn code_comes_from_a_script_stdin_or_c_and_only_one_of_them() {
        let run = |args: &[&str]| Cli::try_parse_from([&["mafold"], args].concat()).map(|c| c.cmd);
        match run(&["run", "plot.py", "--file", "a.csv", "--file", "mf-file:x1", "--timeout", "60"]).unwrap() {
            SandboxCmd::Run { script, code, files, timeout, .. } => {
                assert_eq!((script.as_deref(), code), (Some("plot.py"), None));
                assert_eq!(files, vec!["a.csv", "mf-file:x1"]);
                assert_eq!(timeout, Some(60));
            }
            SandboxCmd::End { .. } => panic!("parsed as end"),
        }
        assert!(matches!(run(&["run", "-c", "print(1)"]).unwrap(), SandboxCmd::Run { code: Some(_), script: None, .. }));
        assert!(matches!(run(&["run", "-"]).unwrap(), SandboxCmd::Run { script: Some(s), .. } if s == "-"));
        assert!(run(&["run"]).is_err(), "no code at all");
        assert!(run(&["run", "plot.py", "-c", "print(1)"]).is_err(), "two kinds of code");
        assert!(matches!(run(&["end"]).unwrap(), SandboxCmd::End { message: None }));
    }

    #[test]
    fn the_body_is_the_apis_run_body() {
        let b = run_body("d1", "print(1)", vec!["f1".into()], Some(30), false);
        assert_eq!(b, json!({ "draft": "d1", "code": "print(1)", "inputs": ["f1"], "timeout_s": 30 }));
        let dry = run_body("d1", "x", vec![], None, true);
        assert_eq!(dry, json!({ "draft": "d1", "code": "x", "inputs": [], "dry_run": true }));
    }

    #[test]
    fn a_name_that_is_not_a_file_here_is_an_id_or_a_mistake() {
        assert_eq!(named_id("abc_DEF-123").unwrap(), "abc_DEF-123");
        assert_eq!(named_id("mf-file:abc").unwrap(), "mf-file:abc", "the api reads the prefix");
        assert_eq!(named_id("/media/abc.png").unwrap(), "abc.png");
        assert!(named_id("/tmp/missing.csv").is_err(), "a path that isn't there");
        assert!(named_id("data/missing.csv").is_err());
        assert!(named_id("  ").is_err());
    }
}
