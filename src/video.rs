//! `mafold video …` — the agent's four verbs for third-party video models.
//! Thin over the server's `videoModels / videoSubmit / videoStatus /
//! videoCancel`; the server validates against the model table, sends the job
//! through mafold-router (where the house key lives), holds the estimate on
//! the wallet that pays — the bot's owner's — and bills the vendor's real
//! count when the clip lands in the registry. Output is JSON on stdout so an
//! agent can read it without scraping prose.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;

#[derive(Subcommand)]
pub enum VideoCmd {
    /// List the models the server can drive, with their enums and known-bad combos.
    Models,
    /// Submit one job. Prints `{job_id, estimate, wallet}`: the estimate is
    /// held on the paying wallet, the bill comes with `status` once the job
    /// finishes. `--dry-run` prints the same estimate without submitting.
    Submit {
        /// model id (see `models`)
        #[arg(long)]
        model: String,
        /// what happens in the shot
        prompt: String,
        /// 480p | 720p | 1080p — the user's choice on the plan card
        #[arg(long, default_value = "720p")]
        resolution: String,
        /// adaptive | 16:9 | 9:16 | … (forced adaptive when a ref is given)
        #[arg(long, default_value = "adaptive")]
        ratio: String,
        /// seconds (4–30, or -1 = model picks)
        #[arg(long, default_value_t = 5)]
        duration: i64,
        /// `role=<path|url|mf:file-id>`, repeatable: first_frame=./cup.jpg,
        /// reference_video=https://… — a local path (e.g. what the user sent) is uploaded for you
        #[arg(long = "ref")]
        refs: Vec<String>,
        #[arg(long)]
        audio: bool,
        #[arg(long)]
        watermark: bool,
        #[arg(long)]
        draft: bool,
        /// check the job and print its estimate WITHOUT submitting: nothing
        /// reaches the vendor, nothing is held or spent (prices a plan card)
        #[arg(long)]
        dry_run: bool,
    },
    /// Read a job. With `--wait`, poll every 10 s until it ends — a read that
    /// fails on the network (or on the vendor being unreachable) is retried
    /// with backoff, and only several failures in a row give up. With
    /// `--attach`, hang the landed clip on the reply you are streaming
    /// (MAFOLD_DRAFT) or on `--message <id>`.
    Status {
        job: String,
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        attach: bool,
        #[arg(long)]
        message: Option<String>,
    },
    /// Cancel a queued job (the vendor refuses anything already running).
    Cancel { job: String },
}

pub async fn run(cmd: VideoCmd, client: &Client) -> Result<()> {
    match cmd {
        VideoCmd::Models => {
            let v = client.call("videoModels", json!({})).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        VideoCmd::Submit { model, prompt, resolution, ratio, duration, refs, audio, watermark, draft, dry_run } => {
            let mut rs = Vec::new();
            for r in &refs {
                let (role, val) = r
                    .split_once('=')
                    .with_context(|| format!("--ref wants role=<path|url|mf:id>, got {r}"))?;
                let (role, val) = (role.trim(), val.trim());
                if val.starts_with("http://") || val.starts_with("https://") {
                    rs.push(json!({ "role": role, "url": val }));
                } else if let Some(id) = val.strip_prefix("mf:") {
                    rs.push(json!({ "role": role, "file": id }));
                } else {
                    // A path on this machine — typically an attachment the user
                    // sent, which the daemon saved locally. Upload it so the
                    // server can hand the vendor something it can fetch.
                    let p = std::path::Path::new(val);
                    if !p.is_file() {
                        bail!("--ref {role}={val}: not a URL, not mf:<file id>, and no such file");
                    }
                    let up = client.upload_path(p).await.with_context(|| format!("uploading {val}"))?;
                    let id = up["id"].as_str().context("upload returned no file id")?;
                    rs.push(json!({ "role": role, "file": id }));
                }
            }
            let mut body = json!({
                "model": model, "prompt": prompt, "resolution": resolution, "ratio": ratio,
                "duration": duration, "refs": rs, "generate_audio": audio, "watermark": watermark,
            });
            if draft {
                body["draft"] = json!(true);
            }
            if dry_run {
                body["dry_run"] = json!(true);
            }
            let v = client.call("videoSubmit", body).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        VideoCmd::Status { job, wait, attach, message } => {
            let read = || client.call("videoStatus", json!({ "job_id": job }));
            let v = if wait {
                wait_until_done(&job, read, tokio::time::sleep).await?
            } else {
                read().await?
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
            if attach && v["status"] == "succeeded" {
                let file_id = v["file"]["id"].as_str().context("succeeded but no file id")?;
                let msg = match message {
                    Some(m) => m,
                    None => std::env::var("MAFOLD_DRAFT").ok().filter(|s| !s.is_empty()).context(
                        "no message to attach to — run inside an agent turn or pass --message <id>",
                    )?,
                };
                let msg = std::fs::read_to_string(crate::agent::draft_ptr_path(&msg))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(msg);
                let att = json!({ "kind": "video", "id": file_id, "file": file_id });
                client.attach(&msg, json!([att])).await?;
                eprintln!("✓ attached {file_id} to {msg}");
            }
        }
        VideoCmd::Cancel { job } => {
            let v: Value = client.call("videoCancel", json!({ "job_id": job })).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

/// `--wait`: how long one tick is, how long a job may take, and how many
/// failed reads IN A ROW mean "give up" rather than "the network blinked".
const WAIT_TICK: std::time::Duration = std::time::Duration::from_secs(10);
const WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const WAIT_MISSES: u32 = 8;
const WAIT_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(60);

/// Read the job until it ends. A read the network or the vendor's side broke
/// is retried with backoff (10 s, 20 s, 40 s, then every 60 s), and only
/// [`WAIT_MISSES`] failures in a row give up — one blink on the way to the
/// vendor used to end the whole wait, and an agent that sees its wait end in
/// an error tells the person the clip failed while the job is still running
/// and still billing (2026-10-07, video-e2e: a 502 from router→ModelArk 50 s
/// into a job that went on to succeed). Reading is safe to repeat: the bill
/// settles once, server-side, however often the job is read. A refusal from
/// the server (bad job id, not yours) is a verdict and ends the wait at once.
async fn wait_until_done<R, RF, S, SF>(job: &str, mut read: R, mut sleep: S) -> Result<Value>
where
    R: FnMut() -> RF,
    RF: std::future::Future<Output = Result<Value>>,
    S: FnMut(std::time::Duration) -> SF,
    SF: std::future::Future<Output = ()>,
{
    let mut waited = std::time::Duration::ZERO;
    let mut misses = 0u32;
    let mut status = String::from("?");
    loop {
        let pause = match read().await {
            Ok(v) => {
                misses = 0;
                status = v["status"].as_str().unwrap_or("?").to_string();
                if matches!(status.as_str(), "succeeded" | "failed" | "cancelled" | "expired") {
                    return Ok(v);
                }
                if waited > std::time::Duration::ZERO {
                    eprintln!("  {status} … {}s", waited.as_secs());
                }
                WAIT_TICK
            }
            Err(e) if retryable(&e) => {
                misses += 1;
                if misses >= WAIT_MISSES {
                    return Err(e.context(format!(
                        "{misses} reads of {job} failed in a row (last seen: {status}) — the job is still the \
                         vendor's and may well finish; `mafold video status {job}` later"
                    )));
                }
                let pause = (WAIT_TICK * 2u32.pow(misses - 1)).min(WAIT_BACKOFF_CAP);
                eprintln!("  couldn't read {job} ({e:#}) — retry {misses}/{WAIT_MISSES} in {}s", pause.as_secs());
                pause
            }
            Err(e) => return Err(e),
        };
        if waited >= WAIT_BUDGET {
            bail!("still {status} after {} min — leaving it; `mafold video status {job}` later", WAIT_BUDGET.as_secs() / 60);
        }
        sleep(pause).await;
        waited += pause;
    }
}

/// Worth reading again: the request died on the way (connect / transport), or
/// the server answered that something behind it is out of reach — 502 / 503 /
/// 504 (`unavailable`: the router, the vendor, the clip's download) or 429.
/// Anything else the server said is an answer that will not change.
fn retryable(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<mafold_core::RpcError>() {
        Some(mafold_core::RpcError::Connect(_) | mafold_core::RpcError::Transport(_)) => true,
        Some(mafold_core::RpcError::Api(env)) => serde_json::from_str::<Value>(env)
            .ok()
            .and_then(|v| v["error_code"].as_u64())
            .is_some_and(|c| matches!(c, 429 | 502 | 503 | 504)),
        _ => false,
    }
}

/// Install the `mafold-video` skill into the daemon's own plugin
/// (`~/.mafold/plugins/mafold`), beside `mafold-room` (`room::install_skill`):
/// the agent sees it as `mafold:mafold-video`, the text lives in this binary,
/// so the commands it names are the ones this version has. Loaded on demand by
/// the harness (description-matched), so it costs nothing on unrelated turns.
///
/// It used to go into `~/.claude/skills` — the owner's own skills, which a
/// turn someone else starts may not use (`crate::drive::skill_gate`), and
/// which every other Claude Code session on the machine picks up too. The copy
/// an older daemon left there is removed, but only if it is byte-for-byte ours.
pub fn install_skill() -> Result<()> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    install_into(&crate::drive::mafold_plugin_dir(), home.as_deref())
}

fn install_into(plugin: &std::path::Path, home: Option<&std::path::Path>) -> Result<()> {
    let dir = plugin.join("skills").join("mafold-video");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), SKILL_MD)?;
    let manifest = plugin.join(".claude-plugin");
    std::fs::create_dir_all(&manifest)?;
    std::fs::write(manifest.join("plugin.json"), r#"{"name":"mafold"}"#)?;
    if let Some(home) = home {
        let old = home.join(".claude").join("skills").join("mafold-video");
        if std::fs::read_to_string(old.join("SKILL.md")).is_ok_and(|s| s == SKILL_MD) {
            let _ = std::fs::remove_file(old.join("SKILL.md"));
            let _ = std::fs::remove_dir(&old);
        }
    }
    Ok(())
}

pub const SKILL_MD: &str = include_str!("video_skill.md");

#[cfg(test)]
mod tests {
    use super::*;
    use mafold_core::RpcError;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// What `Client::post` hands back: the core's error under a `"<method> failed"` context.
    fn failed(e: RpcError) -> anyhow::Error {
        anyhow::Error::new(e).context("videoStatus failed")
    }
    fn api(code: u16) -> anyhow::Error {
        failed(RpcError::Api(format!(r#"{{"ok":false,"error_code":{code},"description":"x"}}"#)))
    }
    fn st(s: &str) -> Result<Value> {
        Ok(json!({ "status": s }))
    }

    /// Runs `wait_until_done` over a scripted sequence of reads; returns the
    /// outcome and every pause it asked for (nothing really sleeps).
    async fn wait_over(reads: Vec<Result<Value>>) -> (Result<Value>, Vec<u64>, usize) {
        let left = RefCell::new(VecDeque::from(reads));
        let slept = RefCell::new(Vec::new());
        let out = wait_until_done(
            "job-1",
            || std::future::ready(left.borrow_mut().pop_front().unwrap_or_else(|| st("running"))),
            |d: Duration| {
                slept.borrow_mut().push(d.as_secs());
                std::future::ready(())
            },
        )
        .await;
        let unread = left.borrow().len();
        (out, slept.into_inner(), unread)
    }

    #[test]
    fn what_is_worth_reading_again() {
        assert!(retryable(&failed(RpcError::Connect("refused".into()))));
        assert!(retryable(&failed(RpcError::Transport("timed out".into()))));
        for code in [429, 502, 503, 504] {
            assert!(retryable(&api(code)), "{code}");
        }
        for code in [400, 401, 403, 404, 409, 500] {
            assert!(!retryable(&api(code)), "{code} is the server's answer");
        }
        assert!(!retryable(&anyhow::anyhow!("not an rpc error")));
    }

    /// The 2026-10-07 run: a 503 (router → ModelArk 502) 50 s into a job that
    /// went on to succeed. One blink, then the clip.
    #[tokio::test]
    async fn a_blink_on_the_way_does_not_end_the_wait() {
        let (out, slept, _) = wait_over(vec![
            st("queued"),
            st("running"),
            Err(failed(RpcError::Transport("connection reset".into()))),
            Err(api(503)),
            st("running"),
            st("succeeded"),
        ])
        .await;
        assert_eq!(out.unwrap()["status"], "succeeded");
        assert_eq!(slept, vec![10, 10, 10, 20, 10], "tick, tick, backoff 10 → 20, back to the tick");
    }

    #[tokio::test]
    async fn only_failures_in_a_row_give_up() {
        let mut reads: Vec<Result<Value>> = (0..WAIT_MISSES - 1).map(|_| Err(api(502))).collect();
        reads.push(st("running")); // a good read resets the count
        reads.extend((0..WAIT_MISSES - 1).map(|_| Err(api(502))));
        reads.push(st("succeeded"));
        let (out, _, _) = wait_over(reads).await;
        assert_eq!(out.unwrap()["status"], "succeeded");

        let (out, slept, _) =
            wait_over((0..WAIT_MISSES + 3).map(|_| Err(failed(RpcError::Connect("down".into())))).collect()).await;
        let msg = format!("{:#}", out.unwrap_err());
        assert!(msg.contains(&format!("{WAIT_MISSES} reads of job-1 failed in a row")), "{msg}");
        assert!(msg.contains("mafold video status job-1"), "says how to look later: {msg}");
        assert!(msg.contains("down"), "keeps the cause: {msg}");
        assert_eq!(slept, vec![10, 20, 40, 60, 60, 60, 60], "backoff, capped at 60 s");
    }

    #[tokio::test]
    async fn a_refusal_ends_the_wait_at_once() {
        let (out, slept, unread) = wait_over(vec![st("running"), Err(api(404)), st("succeeded")]).await;
        assert!(format!("{:#}", out.unwrap_err()).contains("videoStatus failed"));
        assert_eq!(slept, vec![10]);
        assert_eq!(unread, 1, "no read after the refusal");
    }

    #[tokio::test]
    async fn every_ending_ends_it_and_the_budget_still_holds() {
        for end in ["succeeded", "failed", "cancelled", "expired"] {
            let (out, _, _) = wait_over(vec![st("running"), st(end)]).await;
            assert_eq!(out.unwrap()["status"], end);
        }
        let (out, slept, _) = wait_over(vec![]).await; // running forever
        assert!(format!("{:#}", out.unwrap_err()).contains("still running after 15 min"));
        assert_eq!(slept.iter().sum::<u64>(), WAIT_BUDGET.as_secs());
    }

    /// Into Mafold's own plugin; the copy an older daemon left in the owner's
    /// `~/.claude/skills` goes only when it is exactly ours.
    #[test]
    fn the_skill_lives_in_mafolds_plugin_and_an_old_copy_goes_only_if_ours() {
        let root = std::env::temp_dir().join(format!("mafold-video-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (plugin, home) = (root.join("plugin"), root.join("home"));
        let old = home.join(".claude").join("skills").join("mafold-video");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), SKILL_MD).unwrap();
        install_into(&plugin, Some(&home)).unwrap();
        assert_eq!(std::fs::read_to_string(plugin.join("skills/mafold-video/SKILL.md")).unwrap(), SKILL_MD);
        assert_eq!(crate::drive::plugin_name(&plugin).as_deref(), Some("mafold"));
        assert!(!old.exists(), "our own copy is gone");

        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), "the owner's own edit").unwrap();
        install_into(&plugin, Some(&home)).unwrap();
        assert_eq!(std::fs::read_to_string(old.join("SKILL.md")).unwrap(), "the owner's own edit", "not ours: left alone");
        let _ = std::fs::remove_dir_all(&root);
    }
}
