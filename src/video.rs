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
    /// Read a job. With `--wait`, poll every 10 s until it ends. With
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
            let mut v = client.call("videoStatus", json!({ "job_id": job })).await?;
            if wait {
                let mut ticks = 0u32;
                while !matches!(v["status"].as_str(), Some("succeeded" | "failed" | "cancelled")) {
                    ticks += 1;
                    if ticks > 90 {
                        bail!("still {} after 15 min — leaving it; `mafold video status {job}` later", v["status"]);
                    }
                    eprintln!("  {} … {}s", v["status"].as_str().unwrap_or("?"), ticks * 10);
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    v = client.call("videoStatus", json!({ "job_id": job })).await?;
                }
            }
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

/// Install the `mafold-video` skill where Claude Code discovers skills, the same
/// way `room::install_skill` ships `mafold-room`: the text lives in this binary,
/// so the commands it names are the ones this version has. Loaded on demand by
/// the harness (description-matched), so it costs nothing on unrelated turns.
pub fn install_skill() -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .context("no HOME")?;
    let dir = home.join(".claude").join("skills").join("mafold-video");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), SKILL_MD)?;
    Ok(())
}

pub const SKILL_MD: &str = include_str!("video_skill.md");
