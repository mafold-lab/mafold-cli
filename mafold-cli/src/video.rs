//! `mafold video …` — the agent's four verbs for third-party video models.
//!
//! Two keys a clip can run on, and its job id says which:
//!
//! * **The owner's own** — the default since 2026-10-10 (the cofounder:「我都连
//!   接了我自己的key，肯定是默认走我的key啊」). A BytePlus ModelArk key the bot's
//!   owner linked as a Mafold connection is used whenever this turn may use it:
//!   the server checks the job first on a dry run (same model table, refs
//!   resolved to links), then the owner's vault on THIS machine opens the key
//!   and mafold-core's `ark-video` driver talks to ModelArk directly. ModelArk
//!   bills the owner's own account; the Mafold wallet is not touched — no hold,
//!   no debit (`.docs/connection-backed-models-v1.md`: "wallet 完全不参与"). The
//!   finished clip still lands through the server's `ingestUrl`. Job ids read
//!   `own:<connection>:<ModelArk task>`.
//! * **The house key** — for owners without one, or `--via house`. Thin over
//!   the server's `videoModels / videoSubmit / videoStatus / videoCancel`; the
//!   server validates against the model table, sends the job through
//!   mafold-router (where the house key lives), holds the estimate on the
//!   wallet that pays — the bot's owner's — and bills the vendor's real count
//!   when the clip lands in the registry. Job ids read `{seat}~{task}`.
//!
//! A bot its owner hasn't allowed onto their key is REFUSED on a real submit,
//! with a consent card posted — never moved onto the wallet behind the owner's
//! back. Only a real submit asks: a `--dry-run` prices the clip and says
//! `billing.allowed: false`, and `status` / `cancel` say what's missing — none
//! of them posts a card (2026-10-10: the skill prices a plan with several dry
//! runs, and a card per run was spam that an Allow could later act on). The
//! server holds the same line (`own_key_linked`): a house submit for an owner
//! with a linked key needs `house: true`, which only `--via house` sends.
//!
//! The known limit: the owner's vault opens only on a machine the owner is
//! signed in to. A bot running anywhere else can't reach their key — nothing
//! relays its video call to the owner's machines yet — so it lands on the
//! house path and the server refuses it (`own_key_linked`); the error says
//! exactly that, and that the wallet (`--via house`) is the owner's call.
//!
//! Output is JSON on stdout so an agent can read it without scraping prose.

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use crate::connection::{KeyHolder, OwnKey, OwnerVault};
use mafold_core::mafold_types::video as cost;

#[derive(Subcommand)]
pub enum VideoCmd {
    /// List the models the server can drive, with their enums and known-bad combos.
    Models,
    /// Submit one job. On the house key prints `{job_id, estimate, wallet}`:
    /// the estimate is held on the paying wallet, the bill comes with `status`
    /// once the job finishes. On the owner's own ModelArk key prints `{job_id,
    /// estimate, billing}`: ModelArk bills them, nothing is held. `--dry-run`
    /// prints the same estimate without submitting.
    Submit {
        /// model id (see `models`)
        #[arg(long)]
        model: String,
        /// what happens in the shot
        prompt: String,
        /// 480p | 720p | 1080p — the user's choice on the plan card
        #[arg(long, default_value = "720p")]
        resolution: String,
        /// adaptive | 16:9 | 9:16 | … — any with reference_* refs; Seedance 2.5
        /// with a first_frame / last_frame takes adaptive only (keeps the frame's shape)
        #[arg(long, default_value = "adaptive")]
        ratio: String,
        /// seconds (4–30, or -1 = model picks)
        #[arg(long, default_value_t = 5)]
        duration: i64,
        /// `role=<path|url|mf:file-id>`, repeatable: first_frame=./cup.jpg,
        /// reference_video=https://… — a local path (e.g. what the user sent) is uploaded for you
        #[arg(long = "ref")]
        refs: Vec<String>,
        /// a silent clip. Every clip carries its own sound by default — same
        /// price either way on Seedance 2.5 / 2.0; never on a shot with lines
        #[arg(long)]
        no_audio: bool,
        /// sound on — the default already; kept so older calls still run
        #[arg(long, hide = true, conflicts_with = "no_audio")]
        audio: bool,
        #[arg(long)]
        watermark: bool,
        #[arg(long)]
        draft: bool,
        /// check the job and print its estimate WITHOUT submitting: nothing
        /// reaches the vendor, nothing is held or spent, nobody is asked —
        /// `billing.allowed: false` says the owner hasn't let this bot onto
        /// their key yet (prices a plan card)
        #[arg(long)]
        dry_run: bool,
        /// whose key: `own` (the owner's linked ModelArk key — ModelArk bills
        /// them, the Mafold wallet is untouched), `own:<connection>`, or `house`
        /// (Mafold's key, billed to the owner's Mafold wallet). Default: their
        /// own key when one is linked and this turn may use it, else house
        #[arg(long)]
        via: Option<String>,
    },
    /// Read a job. With `--wait`, poll every 10 s until it ends — a read that
    /// fails on the network (or on the vendor being unreachable) is retried
    /// with backoff, and only several failures in a row give up. With
    /// `--attach`, hang the landed clip on the reply you are streaming
    /// (MAFOLD_DRAFT) or on `--message <id>`. A job on the owner's own key
    /// (`own:…`) is read from ModelArk on this machine, and its clip landed
    /// through the server the same way.
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
        VideoCmd::Submit { model, prompt, resolution, ratio, duration, refs, no_audio, audio: _, watermark, draft, dry_run, via } => {
            // Whose key, settled before a byte is uploaded: a run refused here
            // (not allowed yet, no key on this machine) has nothing worth
            // sending, and a typo in `--via` isn't worth a round trip.
            let via = Via::parse(via.as_deref())?;
            let wants = via.wants();
            let own = match via {
                // A decision already made, out loud: nothing to look up.
                Via::House => None,
                // A blink while looking is not "no key" (that would send the
                // clip to the house key, which the server refuses for an owner
                // with one): tried again, then said as what it is.
                _ => Some(
                    patiently(OPEN_TRIES, move || crate::connection::own_ark_key(client, wants), tokio::time::sleep)
                        .await
                        .map_err(|e| {
                            e.context(
                                "couldn't find out whether the owner's own ModelArk key is there — that is the \
                                 network, not an answer. Nothing was submitted and nothing was spent; run it again",
                            )
                        })?,
                ),
            };
            let plan = match (route(&via, own.as_ref().map_or(Found::Nothing, found), dry_run), own) {
                (Route::Own, Some(OwnKey::Use(key))) => Plan::Own(key),
                (Route::Quote, Some(OwnKey::Denied { name, owner, bot, .. })) => Plan::Quote { name, owner, bot },
                (Route::AskOwner, Some(OwnKey::Denied { name, owner, bot, asker })) => {
                    return Err(ask_owner(&asker, &owner, &bot, &name).await);
                }
                (Route::Refuse, own) => bail!(no_own_key(&via, own.as_ref())),
                (Route::House { explicit }, Some(OwnKey::Unseen { why })) => {
                    Plan::House { explicit, why_not_own: Some(why) }
                }
                (Route::House { explicit }, _) => Plan::House { explicit, why_not_own: None },
                (Route::Own | Route::AskOwner | Route::Quote, _) => {
                    unreachable!("route() answers Own only for a usable key, AskOwner and Quote only for a denied one")
                }
            };
            let mut rs = Vec::new();
            // ModelArk bills a reference video's own frames: its length, read
            // off the file when it is one on this machine (an animatic the
            // director stage rendered, a clip the user sent). `None` once any
            // reference video can't be measured — the estimate then says it
            // assumed one as long as the clip.
            let mut ref_video_secs = Some(0.0);
            let mut ref_videos = 0;
            for r in &refs {
                let (role, val) = r
                    .split_once('=')
                    .with_context(|| format!("--ref wants role=<path|url|mf:id>, got {r}"))?;
                let (role, val) = (role.trim(), val.trim());
                let measured = if val.starts_with("http://") || val.starts_with("https://") {
                    rs.push(json!({ "role": role, "url": val }));
                    None
                } else if let Some(id) = val.strip_prefix("mf:") {
                    rs.push(json!({ "role": role, "file": id }));
                    None
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
                    if role == "reference_video" { mp4_secs(p) } else { None }
                };
                if role == "reference_video" {
                    ref_videos += 1;
                    ref_video_secs = ref_video_secs.zip(measured).map(|(a, b)| a + b);
                }
            }
            let ref_video_secs = ref_video_secs.filter(|_| ref_videos > 0);
            let mut body = json!({
                "model": model, "prompt": prompt, "resolution": resolution, "ratio": ratio,
                "duration": duration, "refs": rs, "generate_audio": !no_audio, "watermark": watermark,
            });
            if let Some(secs) = ref_video_secs {
                body["ref_video_secs"] = json!(secs);
            }
            if draft {
                body["draft"] = json!(true);
            }
            if dry_run {
                body["dry_run"] = json!(true);
            }
            let v = match plan {
                Plan::Own(key) => own_submit(client, key, body, dry_run).await?,
                // Only ever a dry run ([`route`]): the server prices it, the
                // answer says whose key it would run on and that the owner
                // hasn't said yes — and nobody is asked.
                Plan::Quote { name, owner, bot } => {
                    body["dry_run"] = json!(true);
                    let checked = client.call("videoSubmit", body.clone()).await?;
                    own_priced(checked, &body, billing(&name, &owner, Some(&bot)))
                }
                Plan::House { explicit, why_not_own } => {
                    if explicit {
                        body["house"] = json!(true);
                    }
                    let mut v = client
                        .call("videoSubmit", body)
                        .await
                        .map_err(|e| house_refused(e, why_not_own.as_deref()))?;
                    // A dry run for an owner who has their own key, which this
                    // machine couldn't use: the server says the first half
                    // (`own_key_linked` — a real submit here is refused), this
                    // says the second, so the plan can tell the owner why.
                    if let Some(why) = why_not_own {
                        if v["own_key_linked"] == true && v.is_object() {
                            v["own_key_unusable_here"] = json!(why);
                        }
                    }
                    v
                }
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        VideoCmd::Status { job, wait, attach, message } => {
            let v = match parse_job(&job)? {
                Job::House => {
                    let read = || client.call("videoStatus", json!({ "job_id": job }));
                    if wait {
                        wait_until_done(&job, read, tokio::time::sleep).await?
                    } else {
                        read().await?
                    }
                }
                Job::Own { connection, task } => {
                    // Opened once, before the first read: a wait reads every
                    // ten seconds, and the vault key doesn't change in between.
                    // With the reads' patience (2026-10-10): finding the key
                    // and opening the vault are network round trips too, and
                    // one 502 there used to end the wait — and the agent told
                    // the person their clip had failed while it was running.
                    let tries = if wait { WAIT_MISSES } else { OPEN_TRIES };
                    let vault = patiently(tries, || own_vault(client, &job, connection), tokio::time::sleep)
                        .await
                        .map_err(|e| {
                            out_of_reach(
                                e,
                                &format!(
                                    "the job is still ModelArk's and may well finish (and bill); don't resubmit — \
                                     `mafold video status {job}` later"
                                ),
                            )
                        })?;
                    let read = || own_status(client, &vault, &job, task);
                    if wait {
                        wait_until_done(&job, read, tokio::time::sleep).await?
                    } else {
                        read().await?
                    }
                }
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
                let msg = crate::agent::current_draft(msg).await?;
                let att = json!({ "kind": "video", "id": file_id, "file": file_id });
                client.attach(&msg, json!([att])).await?;
                eprintln!("✓ attached {file_id} to {msg}");
            }
        }
        VideoCmd::Cancel { job } => {
            let v: Value = match parse_job(&job)? {
                Job::House => client.call("videoCancel", json!({ "job_id": job })).await?,
                Job::Own { connection, task } => {
                    let vault = patiently(OPEN_TRIES, || own_vault(client, &job, connection), tokio::time::sleep)
                        .await
                        .map_err(|e| {
                            out_of_reach(e, &format!("nothing was cancelled — `mafold video cancel {job}` again later"))
                        })?;
                    let r = vault
                        .call("video.cancel", &json!({ "job_id": task }))
                        .await
                        .map_err(|e| own_failure(e, "video.cancel"))?;
                    json!({
                        "job_id": job,
                        "cancelled": r["cancelled"].as_bool().unwrap_or(true),
                        "billing": billing(&vault.name, &vault.owner, None),
                    })
                }
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
    }
    Ok(())
}

// ── whose key ──────────────────────────────────────────────────────────────

/// `--via`: whose key a submit runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Via {
    /// Nothing said: the owner's own key when one is linked and this turn may
    /// use it, else the house key.
    Auto,
    /// The owner's own key, or a refusal that says why not — never the wallet.
    /// A name picks one of several.
    Own(Option<String>),
    /// Mafold's key and the owner's Mafold wallet, asked for by name (the
    /// submit carries `house: true`, which is what gets past `own_key_linked`).
    House,
}

impl Via {
    fn parse(raw: Option<&str>) -> Result<Via> {
        let v = raw.map(str::trim).unwrap_or("");
        match v {
            "" | "auto" => Ok(Via::Auto),
            "house" => Ok(Via::House),
            "own" => Ok(Via::Own(None)),
            _ => match v.strip_prefix("own:").map(str::trim) {
                Some(name) if !name.is_empty() => Ok(Via::Own(Some(name.to_string()))),
                _ => bail!("--via takes `own`, `own:<connection>` or `house` — not `{v}`"),
            },
        }
    }

    /// The connection asked for by name, if any.
    fn wants(&self) -> Option<&str> {
        match self {
            Via::Own(Some(name)) => Some(name.as_str()),
            _ => None,
        }
    }
}

/// What [`crate::connection::own_ark_key`] found, as far as the rule cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    Nothing,
    Unseen,
    Usable,
    Denied,
}

fn found(own: &OwnKey) -> Found {
    match own {
        OwnKey::None { .. } => Found::Nothing,
        OwnKey::Unseen { .. } => Found::Unseen,
        OwnKey::Use(_) => Found::Usable,
        OwnKey::Denied { .. } => Found::Denied,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// The owner's own key, from this machine.
    Own,
    /// The house key; `explicit` = asked for by name (`house: true`).
    House { explicit: bool },
    /// The key is there and this bot may not use it: ask the owner, go no further.
    AskOwner,
    /// The same, on a dry run: price it and say `allowed: false` — ask nobody.
    Quote,
    /// `--via own` with no key to use: say why, go no further.
    Refuse,
}

/// The rule, in one place. The owner's own key whenever it is there and this
/// turn may use it. A key that is there but not allowed to this bot is a
/// question for the owner — NEVER a quiet move onto their wallet, which is the
/// path they chose not to be on — and only a real submit asks it: a dry run
/// prices the clip on that key and says it isn't allowed yet (2026-10-10; the
/// skill prices a plan with several, and a card each was spam an Allow could
/// later act on). The house key when there is no key to speak of, or when it
/// was asked for by name. `Unseen` (this machine can't tell) goes to the house
/// key too, because the server knows what this machine doesn't: it refuses a
/// house submit for an owner with a linked key (`own_key_linked`), so not
/// knowing can't end up spending the wallet.
fn route(via: &Via, found: Found, dry_run: bool) -> Route {
    match (via, found) {
        (Via::House, _) => Route::House { explicit: true },
        (_, Found::Usable) => Route::Own,
        (_, Found::Denied) if dry_run => Route::Quote,
        (_, Found::Denied) => Route::AskOwner,
        (Via::Auto, Found::Nothing | Found::Unseen) => Route::House { explicit: false },
        (Via::Own(_), Found::Nothing | Found::Unseen) => Route::Refuse,
    }
}

/// A submit that is going ahead, and on which key.
enum Plan {
    Own(KeyHolder),
    /// A dry run on a key `bot` may not use yet ([`Route::Quote`]).
    Quote { name: String, owner: String, bot: String },
    /// `why_not_own`: what this machine couldn't see about the owner's own key
    /// — said beside the server's `own_key_linked`, should it come back.
    House { explicit: bool, why_not_own: Option<String> },
}

/// Own-key job ids: `own:<connection>:<ModelArk task>`. A house id is
/// `{seat}~{task}` and the server refuses `:` in one, connection names are
/// `[a-z0-9_-]`, and so are ModelArk's task ids — no id reads as both.
const OWN_JOB: &str = "own:";

fn own_job_id(connection: &str, task: &str) -> String {
    format!("{OWN_JOB}{connection}:{task}")
}

#[derive(Debug, PartialEq, Eq)]
enum Job<'a> {
    House,
    Own { connection: &'a str, task: &'a str },
}

fn parse_job(job: &str) -> Result<Job<'_>> {
    let Some(rest) = job.trim().strip_prefix(OWN_JOB) else {
        return Ok(Job::House);
    };
    match rest.split_once(':') {
        Some((connection, task)) if !connection.is_empty() && !task.is_empty() => {
            Ok(Job::Own { connection, task })
        }
        _ => bail!("`{job}` isn't a job id — one on the owner's own key reads own:<connection>:<task>"),
    }
}

// ── the owner's own key ────────────────────────────────────────────────────

/// The estimate's label on the own-key path. The figure is the server's — the
/// wallet price, which the launch checklist sets to ModelArk's list price —
/// but the bill is ModelArk's, on the owner's own account.
const OWN_ESTIMATE_NOTE: &str =
    "估算：按标价 × 公式；实账是 ModelArk 记在 key 主人自己账上的 token 数，不经 Mafold 钱包";

/// What stands where `wallet` stands on the house key: who pays, that it isn't
/// Mafold, and whether this turn may run on it (`allowed`). That is always so
/// for a job already running there; `waiting_on` is the bot a dry run priced
/// the clip for although the owner hasn't let it onto their key yet — nobody
/// was asked, and a real submit asks before anything runs.
fn billing(connection: &str, owner: &str, waiting_on: Option<&str>) -> Value {
    let note = match waiting_on {
        None => "ModelArk bills this to the key's own account — the Mafold wallet is not touched".to_string(),
        Some(bot) => format!(
            "ModelArk would bill this to @{owner}'s own account, but @{owner} hasn't allowed @{bot} to use the \
             key yet. Nobody was asked: a real submit posts a consent card for @{owner} and goes no further \
             until they allow it — it does not fall back to the Mafold wallet"
        ),
    };
    json!({
        "via": "own",
        "connection": connection,
        "owner": owner,
        "allowed": waiting_on.is_none(),
        "note": note,
    })
}

/// The quote on the owner's own key: ModelArk's own count at ModelArk's list
/// price (`mafold_types::video`, the rule the api's estimate uses too), for
/// the job as the server checked it. Not the server's dollar figure: that is
/// the WALLET's rate, which bills nobody on this path — and is `null` where no
/// wallet price is set, so the 10-30 film was planned on quotes without a
/// dollar in them — and an api from before 10-31 left a reference video out of
/// the count (48,437 quoted, 100,858 billed).
fn own_estimate(checked: &Value, body: &Value) -> Value {
    let model = checked["model"].as_str().unwrap_or_default();
    let resolution = checked["resolution"].as_str().unwrap_or("720p");
    let duration = checked["duration"].as_i64().unwrap_or(5);
    let has_video = body["refs"].as_array().is_some_and(|rs| rs.iter().any(|r| r["role"] == "reference_video"));
    let (input, assumed) = match (has_video, body["ref_video_secs"].as_f64()) {
        (false, _) => (None, false),
        (true, Some(secs)) => (Some(secs), false),
        (true, None) => (Some(if duration <= 0 { 5.0 } else { duration as f64 }), true),
    };
    let tokens = cost::clip_tokens(model, resolution, duration, input);
    let per_m = cost::list_usd_per_m(model, resolution, input.is_some());
    let mut estimate = json!({
        "tokens": tokens,
        "usd": per_m.map(|p| (tokens as f64) * p / 1e6),
        "usd_per_m": per_m,
        "audio": if body["generate_audio"] == false { "off" } else { "on — the same price as a silent clip" },
        "note": OWN_ESTIMATE_NOTE,
    });
    if let Some(secs) = input {
        estimate["input_video_secs"] = json!(secs);
    }
    if assumed {
        estimate["input_video_assumed"] = json!(
            "the reference video's length couldn't be read here, so it is counted as long as the clip — \
             `status` puts the real count beside this"
        );
    }
    estimate
}

/// A dry run on the owner's own key, as the agent reads it: the server's
/// verdict and figures, the estimate labelled for ModelArk's bill, `billing`
/// where `wallet` would stand. House-key facts (is the router wired, has the
/// model a wallet price, would a house submit be refused) and plumbing go:
/// none of it bears on a clip ModelArk bills the owner for, and `walletable:
/// false` would read as "can't".
fn own_priced(mut checked: Value, body: &Value, billing: Value) -> Value {
    let estimate = own_estimate(&checked, body);
    if let Some(o) = checked.as_object_mut() {
        for k in ["configured", "walletable", "ref_urls", "own_key_linked"] {
            o.remove(k);
        }
        o.insert("estimate".to_string(), estimate);
        o.insert("billing".to_string(), billing);
    }
    checked
}

/// A submit on the owner's own key.
///
/// The server checks it first, on a dry run: the same model table, the same
/// known-bad combinations, refs resolved to links — what the house path
/// answers to, so the two paths never disagree about a model, and nothing is
/// held on anyone's wallet. Then the owner's vault on this machine opens the
/// key and the driver sends it. `--dry-run` stops after the check.
///
/// The send itself is never repeated: ModelArk takes no idempotency key, and
/// a reply lost on the way back may still have started a job — sending again
/// is a second bill (2026-10-30: `video.submit failed` on a blink, the task
/// already at ModelArk). So each submit goes out with a fresh `seed`, which the
/// task keeps, and a send that may have landed is looked for by it
/// ([`find_lost`]): found, it is this submit's job; not there, nothing started.
async fn own_submit(client: &Client, key: KeyHolder, mut body: Value, dry_run: bool) -> Result<Value> {
    body["dry_run"] = json!(true);
    let checked = client.call("videoSubmit", body.clone()).await?;
    if dry_run {
        return Ok(own_priced(checked, &body, billing(&key.name, &key.owner, None)));
    }
    let billing = billing(&key.name, &key.owner, None);
    let estimate = own_estimate(&checked, &body);
    let sent = body["refs"].as_array().cloned().unwrap_or_default();
    let refs = vendor_refs(&sent, &checked)?;
    let (name, owner) = (key.name.clone(), key.owner.clone());
    let vault = key.open().await.map_err(|e| {
        out_of_reach(no_vault_here(e, &owner, &name, true), "nothing was submitted and nothing was spent; run it again")
    })?;
    let mut args = json!({
        "model": checked["model"].clone(),
        "prompt": body["prompt"].clone(),
        "resolution": checked["resolution"].clone(),
        "ratio": checked["ratio"].clone(),
        "duration": checked["duration"].clone(),
        "refs": refs,
        "generate_audio": body["generate_audio"].clone(),
        "watermark": body["watermark"].clone(),
    });
    if body["draft"] == true {
        args["draft"] = json!(true);
    }
    let seed = fresh_seed();
    args["seed"] = json!(seed);
    let sent_at = unix_now();
    let (task, recovered) = match vault.call("video.submit", &args).await {
        Ok(task) => (task, None),
        Err(e) => {
            let e = own_failure(e, "video.submit");
            if !may_have_landed(&e) {
                return Err(e);
            }
            eprintln!("  the submit's reply was lost ({e:#}) — looking for it at ModelArk by its seed {seed}");
            let want = json!({ "seed": seed, "model": args["model"].clone(), "since": sent_at - LOST_CLOCK_SLACK });
            let (vault, want) = (&vault, &want);
            let look = move || async move {
                vault.call("video.list", want).await.map_err(|e| own_failure(e, "video.list"))
            };
            match find_lost(look, tokio::time::sleep).await {
                Lost::Found(task) => {
                    let note = format!(
                        "the reply to this submit was lost on the way back, but ModelArk had started it (found by \
                         its seed {seed}) — this is that job; nothing was sent twice"
                    );
                    (task, Some(note))
                }
                Lost::NotThere => {
                    return Err(e.context(format!(
                        "ModelArk has no job from this submit: its seed ({seed}) is on none of the key's newest \
                         tasks, looked for {} times over {}s. Nothing was started and nothing will be billed — it \
                         is safe to submit again",
                        LOST_PAUSES.len() + 1,
                        LOST_PAUSES.iter().map(|d| d.as_secs()).sum::<u64>()
                    )));
                }
                Lost::Unknown(why) => {
                    return Err(e.context(format!(
                        "couldn't tell whether ModelArk started it ({why:#}). Don't submit again yet — it may be \
                         running, and billing. Look first: `mafold connection call {name} video.list --params \
                         '{{\"seed\":{seed}}}'` — a task there is this submit (`mafold video status \
                         own:{name}:<its id>`); none there means nothing started"
                    )));
                }
            }
        }
    };
    let id = task["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .with_context(|| format!("ModelArk answered without a task id: {task}"))?;
    let mut v = json!({
        "job_id": own_job_id(&name, id),
        "model": checked["model"].clone(),
        "vendor": checked["vendor"].clone(),
        "resolution": checked["resolution"].clone(),
        "ratio": checked["ratio"].clone(),
        "duration": checked["duration"].clone(),
        "status": task["status"].clone(),
        "estimate": estimate,
        "billing": billing,
    });
    if let Some(note) = recovered {
        v["recovered"] = json!(note);
    }
    // Kept for `status`, which puts the bill beside it: no server holds an
    // own-key job, so the quote stays where the job was sent from.
    let job = own_job_id(&name, id);
    landed_put(&quotes_path(), &landed_key(&client.base, &job), &v["estimate"]);
    Ok(v)
}

/// A submit's `seed` on the owner's key: random, so it changes nothing about
/// the clip (ModelArk draws one at random when none is given), and the mark
/// [`find_lost`] knows the job by. 1 … 2³¹−1: inside every range ModelArk
/// takes, never `-1` ("pick one") or `0`.
fn fresh_seed() -> i64 {
    let n = u32::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..4].try_into().expect("4 bytes"));
    i64::from(n % (i32::MAX as u32)) + 1
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// How far ModelArk's `created_at` may sit before this machine's clock at the
/// send and still be this submit's — the two clocks are not the same clock.
const LOST_CLOCK_SLACK: i64 = 300;

/// The pauses between [`find_lost`]'s looks: the first right away, then after
/// 10 s and 30 s more — a send the vendor was still taking in when the reply
/// broke shows up by then.
const LOST_PAUSES: [std::time::Duration; 2] =
    [std::time::Duration::from_secs(10), std::time::Duration::from_secs(30)];

/// A submit that failed in a way that may still have reached ModelArk: the
/// request died after it was on its way (`transport`), or the vendor's side
/// broke (a 5xx, read as 502/503/504). Not a connect failure — nothing left
/// this machine — nor a 429 or any other answer: ModelArk refused it, and a
/// refused task is never created.
fn may_have_landed(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<mafold_core::RpcError>() {
        Some(mafold_core::RpcError::Transport(_)) => true,
        Some(mafold_core::RpcError::Api(env)) => serde_json::from_str::<Value>(env)
            .ok()
            .and_then(|v| v["error_code"].as_u64())
            .is_some_and(|c| matches!(c, 502 | 503 | 504)),
        _ => false,
    }
}

/// What [`find_lost`] learned about a submit whose reply was lost.
#[derive(Debug)]
enum Lost {
    /// ModelArk has it: the task, as `video.list` answered it.
    Found(Value),
    /// Every look got an answer and none had it: nothing started.
    NotThere,
    /// A look never got an answer — or the key is driven by a core without
    /// `video.list`, whose `video.submit` didn't send the seed either. Can't
    /// tell, so nobody may say "nothing started".
    Unknown(anyhow::Error),
}

/// Look for a lost submit by its seed (`look` = `video.list` narrowed to it):
/// right away, then after each of [`LOST_PAUSES`]. Each look rides blinks with
/// [`patiently`]; one that still doesn't get through, or a refusal, ends it as
/// [`Lost::Unknown`]. "Not there" only when every look was answered.
async fn find_lost<L, LF, S, SF>(mut look: L, mut sleep: S) -> Lost
where
    L: FnMut() -> LF,
    LF: std::future::Future<Output = Result<Value>>,
    S: FnMut(std::time::Duration) -> SF,
    SF: std::future::Future<Output = ()>,
{
    let mut pauses = LOST_PAUSES.iter();
    loop {
        match patiently(OPEN_TRIES, &mut look, &mut sleep).await {
            Ok(v) => {
                if let Some(task) = v["items"].as_array().and_then(|a| a.first()) {
                    return Lost::Found(task.clone());
                }
                if !v["items"].is_array() {
                    return Lost::Unknown(anyhow!("video.list answered without `items`: {v}"));
                }
            }
            Err(e) => return Lost::Unknown(e),
        }
        match pauses.next() {
            Some(p) => sleep(*p).await,
            None => return Lost::NotThere,
        }
    }
}

/// What ModelArk is handed for each `--ref` on the own-key path: where the
/// server resolved it on the dry run (`ref_urls`) — a file id means nothing on
/// the owner's device. That is a link, or on a server without public storage
/// an inline `data:image/…` (the bytes the house path sends too); a ref with
/// neither (`url: null` — a video or audio file there) is refused here, before
/// anything is spent. An api from before `ref_urls` resolved the refs and kept
/// the answer: a ref given as a link passes through as it was, one given as a
/// file can't, and says so.
fn vendor_refs(sent: &[Value], checked: &Value) -> Result<Vec<Value>> {
    let link = |r: &Value| -> Option<String> {
        r["url"]
            .as_str()
            .filter(|u| u.starts_with("https://") || u.starts_with("http://") || u.starts_with("data:image/"))
            .map(str::to_string)
    };
    let (refs, older) = match checked.get("ref_urls").and_then(Value::as_array) {
        Some(resolved) => (resolved.as_slice(), false),
        None => (sent, true),
    };
    refs.iter()
        .map(|r| {
            let role = r["role"].as_str().unwrap_or("?");
            match link(r) {
                Some(url) => Ok(json!({ "role": role, "url": url })),
                None if older => Err(anyhow!(
                    "--ref {role}: this server doesn't say where that file can be fetched (it predates \
                     own-key video) — pass a public https URL instead, or `--via house`"
                )),
                None => Err(anyhow!(
                    "--ref {role}: this server has no public link for that file, and only an image \
                     can travel inline — pass a public https URL instead, or `--via house`"
                )),
            }
        })
        .collect()
}

/// The owner's vault for a job already running on their key — the questions
/// a submit asks (is the key there, may this turn use it, can this machine
/// open it), answered for the connection the job id names. It asks nobody
/// anything: a bot no longer allowed on the key is told so and no consent card
/// goes up (2026-10-10 — a card per `status` is spam, and an Allow on one
/// would bring the bot back to re-run whatever its turn was doing). A blink
/// comes back as the typed error it is, for [`patiently`] to try again.
async fn own_vault(client: &Client, job: &str, connection: &str) -> Result<OwnerVault> {
    match crate::connection::own_ark_key(client, Some(connection)).await? {
        OwnKey::Use(key) => {
            let (owner, name) = (key.owner.clone(), key.name.clone());
            key.open().await.map_err(|e| no_vault_here(e, &owner, &name, false))
        }
        OwnKey::Denied { name, owner, bot, .. } => Err(anyhow!(
            "`{job}` runs on @{owner}'s own ModelArk key (`{name}`), and @{owner} hasn't allowed @{bot} to use \
             it — so it can't be read or cancelled from here. Nothing was asked and nothing changed; the job \
             itself is untouched at ModelArk. @{owner} can allow @{bot} under Settings ▸ Connections."
        )),
        OwnKey::None { owner } => Err(anyhow!(
            "`{job}` runs on @{owner}'s ModelArk connection `{connection}`, which isn't there any more — \
             the job is still ModelArk's, but nothing here can read it"
        )),
        OwnKey::Unseen { why } => Err(anyhow!(
            "`{job}` runs on the owner's own ModelArk key, and this machine can't reach it: {why}"
        )),
    }
}

/// One read of a job on the owner's key, answered in the house path's shape.
/// A failure keeps the type `retryable` reads ([`own_failure`]), so `--wait`
/// treats a blink the same whichever key the job runs on.
async fn own_status(client: &Client, vault: &OwnerVault, job: &str, task: &str) -> Result<Value> {
    let v = vault
        .call("video.status", &json!({ "job_id": task }))
        .await
        .map_err(|e| own_failure(e, "video.status"))?;
    let quote = quote_get(&quotes_path(), &landed_key(&client.base, job));
    let mut out = own_status_view(job, &vault.name, &vault.owner, &v, quote.as_ref());
    if v["status"] == "succeeded" {
        land(client, job, task, &v, &mut out).await;
    }
    Ok(out)
}

/// A ModelArk task in the shape `videoStatus` answers on the house key — what
/// the skill, and an agent that has read one, expects. The status word as the
/// vendor said it (mafold-router's `from_upstream` passes it through the same
/// way, `unknown` when missing), the vendor's count with its unit, the dollar
/// figure at list price, the vendor's error on a job that ended badly.
/// `billing` stands where `wallet` stands: nothing was held, nothing is
/// charged here. The 24 h `video_url` is never handed out — it is landed.
///
/// The dollar figure is ModelArk's list price for the model and tier, at the
/// lower rate when the input included a video — which the `quote` this
/// machine kept at submit says, or, for a job sent from elsewhere, a count
/// well over the clip's own frames can only mean. The quote stands beside the
/// bill with a `check` (`mafold_types::video::check_bill`), so a gap shows on
/// the job it happens on (ops, 10-30).
fn own_status_view(job: &str, connection: &str, owner: &str, v: &Value, quote: Option<&Value>) -> Value {
    let status = v["status"].as_str().unwrap_or("unknown");
    let output = v["usage"]["completion_tokens"].as_u64();
    let usage = match output {
        Some(n) => json!({
            "unit": "token",
            "output": n,
            "total": v["usage"]["total_tokens"].as_u64().unwrap_or(n),
        }),
        None => Value::Null,
    };
    let model = v["model"].as_str().unwrap_or_default();
    let resolution = v["resolution"].as_str().unwrap_or("720p");
    let plain = cost::clip_tokens(model, resolution, v["duration"].as_i64().unwrap_or(5), None);
    let video_input = match quote {
        Some(q) => q["input_video_secs"].is_number(),
        None => output.is_some_and(|t| t as f64 > plain as f64 * 1.05),
    };
    let price = output.and(cost::list_usd_per_m(model, resolution, video_input));
    let check = match (quote.and_then(|q| q["tokens"].as_u64()), output) {
        (Some(quoted), Some(billed)) => json!(cost::check_bill(quoted, billed)),
        (None, Some(_)) => json!("no quote kept on this machine for this job — nothing to compare the bill with"),
        _ => Value::Null,
    };
    let mut out = json!({
        "job_id": job,
        "model": v["model"].clone(),
        "status": status,
        "usage": usage,
        "cost_usd": output.zip(price).map(|(t, p)| (t as f64) * p / 1e6),
        "price_usd_per_m": price,
        "estimate": quote.cloned().unwrap_or(Value::Null),
        "check": check,
        "billing": billing(connection, owner, None),
        "seed": v["seed"].clone(),
        "created_at": v["created_at"].clone(),
        "updated_at": v["updated_at"].clone(),
    });
    if matches!(status, "failed" | "cancelled" | "expired") {
        out["error"] = if v["error"].is_null() {
            Value::Null
        } else {
            json!({ "code": v["error"]["code"].clone(), "message": v["error"]["message"].clone() })
        };
    }
    out
}

/// Land a finished clip through the server's `ingestUrl` — the one door every
/// byte enters the registry through (围栏二: 成功先落地) — and remember it, so
/// reading the job again answers with the same file instead of pulling
/// ModelArk's bytes a second time. A clip that can't be landed is still a
/// succeeded job: said beside it, as the house path says it.
async fn land(client: &Client, job: &str, task: &str, v: &Value, out: &mut Value) {
    let (cache, key) = (landed_path(), landed_key(&client.base, job));
    let landed = match landed_get(&cache, &key) {
        Some(file) => Ok(file),
        None => match v["content"]["video_url"].as_str() {
            None => Err("ModelArk says succeeded but gave no video_url".to_string()),
            Some(url) => client
                .call(
                    "ingestUrl",
                    json!({ "url": url, "mime": "video/mp4", "filename": format!("{task}.mp4") }),
                )
                .await
                .map(|file| {
                    landed_put(&cache, &key, &file);
                    file
                })
                .map_err(|e| format!("{e:#}")),
        },
    };
    match landed {
        Ok(file) => {
            out["file"] = file;
            out["vendor_url_ttl"] = json!("24h — not returned; the file above is the durable copy");
        }
        Err(msg) => {
            out["file"] = Value::Null;
            out["landing_error"] = json!(msg);
            out["hint"] = json!(
                "ModelArk keeps a finished clip for 24h: within that, reading the job again retries the \
                 landing; past it, re-run the shot"
            );
        }
    }
}

/// Own-key clips already landed. The house path keeps this in the api process
/// (`landed()` in video_api.rs); here no server holds the job, so it lives
/// with the rest of the cli's state under `~/.mafold` — one file that every
/// cli on this HOME shares: a dev and a prod one, two bots, two `status`
/// runs at once. Hence the key ([`landed_key`]), the lock and the whole-file
/// rename ([`landed_put`]), and a cache this can't read left alone
/// ([`landed_with`]) — all 2026-10-10.
fn landed_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
        .join(".mafold")
        .join("video-landed.json")
}

/// How many landed clips are remembered. A job is read for minutes, maybe once
/// more a day later; the oldest fall off.
const LANDED_KEEP: usize = 200;

/// A cache entry's key: the api the clip was landed through, and the job. A
/// job id alone is ModelArk's task under a connection NAME — `own:ark:…` from
/// the dev api and from prod name different files, and a dev cli handing out
/// a prod file id (or the other way round) hangs a file on a message that
/// server has never seen.
fn landed_key(base: &str, job: &str) -> String {
    format!("{}|{job}", base.trim_end_matches('/'))
}

/// The landed file for `key` in the cache's text. Text this can't read holds
/// nothing.
fn landed_find(text: &str, key: &str) -> Option<Value> {
    entry(text, key).filter(|f| f["id"].is_string())
}

/// What a cache's text holds under `key` — the same layout for both caches.
fn entry(text: &str, key: &str) -> Option<Value> {
    serde_json::from_str::<Vec<Value>>(text).ok()?.into_iter().find(|e| e["key"] == key).map(|e| e["file"].clone())
}

/// The quotes own-key jobs were submitted on, for `status` to set the bill
/// beside — kept the way landed clips are ([`landed_put`]: same key, lock and
/// whole-file rename, the oldest past [`LANDED_KEEP`] dropped), in a file of
/// their own.
fn quotes_path() -> std::path::PathBuf {
    landed_path().with_file_name("video-quotes.json")
}

fn quote_get(path: &std::path::Path, key: &str) -> Option<Value> {
    entry(&std::fs::read_to_string(path).ok()?, key).filter(|q| q["tokens"].is_u64())
}

/// How long an mp4 / mov runs, from its `moov`/`mvhd` box — enough to price a
/// reference video without ffprobe. `None` for anything this can't read.
fn mp4_secs(path: &std::path::Path) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    // Walk `[start, end)`'s boxes for `want`: (payload start, payload end).
    fn find(f: &mut std::fs::File, mut at: u64, end: u64, want: &[u8; 4]) -> Option<(u64, u64)> {
        while at + 8 <= end {
            f.seek(SeekFrom::Start(at)).ok()?;
            let mut h = [0u8; 8];
            f.read_exact(&mut h).ok()?;
            let mut size = u64::from(u32::from_be_bytes(h[..4].try_into().ok()?));
            let mut head = 8;
            if size == 1 {
                let mut big = [0u8; 8];
                f.read_exact(&mut big).ok()?;
                size = u64::from_be_bytes(big);
                head = 16;
            } else if size == 0 {
                size = end - at;
            }
            if size < head || at + size > end {
                return None;
            }
            if &h[4..] == want {
                return Some((at + head, at + size));
            }
            at += size;
        }
        None
    }
    let (moov, moov_end) = find(&mut f, 0, len, b"moov")?;
    let (mvhd, _) = find(&mut f, moov, moov_end, b"mvhd")?;
    f.seek(SeekFrom::Start(mvhd)).ok()?;
    let mut b = [0u8; 32];
    f.read_exact(&mut b).ok()?;
    let (scale, dur) = if b[0] == 1 {
        (u32::from_be_bytes(b[20..24].try_into().ok()?), u64::from_be_bytes(b[24..32].try_into().ok()?))
    } else {
        (u32::from_be_bytes(b[12..16].try_into().ok()?), u64::from(u32::from_be_bytes(b[16..20].try_into().ok()?)))
    };
    (scale > 0 && dur > 0).then(|| dur as f64 / f64::from(scale))
}

/// The cache's next text: `text` (`None` = no cache yet) with `key → file` in
/// it and the oldest past [`LANDED_KEEP`] dropped. `None` when `text` isn't a
/// cache this can read — someone else's file, a newer layout: it is left as
/// it is rather than replaced by a cache of one, which would drop every other
/// clip in it (a skipped entry costs one clip a second pull, no more).
fn landed_with(text: Option<&str>, key: &str, file: &Value) -> Option<String> {
    let mut all: Vec<Value> = match text {
        None => Vec::new(),
        Some(text) => serde_json::from_str(text).ok()?,
    };
    all.retain(|e| e["key"] != key);
    all.push(json!({ "key": key, "file": file }));
    let keep = &all[all.len().saturating_sub(LANDED_KEEP)..];
    serde_json::to_string(keep).ok()
}

fn landed_get(path: &std::path::Path, key: &str) -> Option<Value> {
    landed_find(&std::fs::read_to_string(path).ok()?, key)
}

/// Best effort: a cache that can't be written costs a second pull, not a clip.
///
/// Read, add, write under [`LandedLock`], so two `status` runs landing at once
/// both keep their entry; written whole to a file of its own and renamed over
/// the cache, so a reader never meets half of one (`std::fs::rename` replaces
/// an existing target — on Windows too: `MoveFileExW`, replace-existing).
fn landed_put(path: &std::path::Path, key: &str, file: &Value) {
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let Some(_lock) = LandedLock::take(path) else { return };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return,
    };
    let Some(next) = landed_with(text.as_deref(), key, file) else { return };
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, next).is_err() || !replace_file(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// `std::fs::rename`, a few times: on Windows a scanner or indexer holding the
/// target open for a moment fails a rename that works 20 ms later.
fn replace_file(from: &std::path::Path, to: &std::path::Path) -> bool {
    for _ in 0..5 {
        if std::fs::rename(from, to).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// A lock left behind longer than this was left by a process that died
/// holding it: one read-modify-write takes milliseconds.
const LANDED_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(10);

/// The cache's lock: a file beside it that only one process can create, gone
/// when this is dropped. Not got within about a second — no caching this time.
struct LandedLock(std::path::PathBuf);

impl LandedLock {
    fn take(cache: &std::path::Path) -> Option<LandedLock> {
        let lock = cache.with_extension("json.lock");
        for _ in 0..50 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Some(LandedLock(lock)),
                // Held — or, on Windows, a lock file still being let go of.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_some_and(|age| age > LANDED_LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for LandedLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A driver failure, typed the way [`retryable`] reads the house path's. The
/// driver answers in sentences — `connect failed: …` / `transport failed: …`
/// (the core's own words for a request that died on the way) and `ark <status>:
/// …` (ModelArk's) — so the sentence is read back into the [`RpcError`] the
/// same blink would have been on the house key: a 429 stays a 429, a vendor
/// 5xx is the vendor out of reach (503). Everything else — a 401 on a dead
/// key, a 400 on a parameter — is the vendor's verdict and stays one. The
/// words are kept either way: a billing system is quoted, never paraphrased.
///
/// [`RpcError`]: mafold_core::RpcError
fn own_failure(e: anyhow::Error, method: &str) -> anyhow::Error {
    use mafold_core::RpcError;
    // The relay's errors (a connection answered by another machine) are typed already.
    if e.downcast_ref::<RpcError>().is_some() {
        return e.context(format!("{method} failed"));
    }
    let msg = e.to_string();
    let typed = if let Some(rest) = msg.strip_prefix("connect failed: ") {
        Some(RpcError::Connect(rest.to_string()))
    } else if let Some(rest) = msg.strip_prefix("transport failed: ") {
        Some(RpcError::Transport(rest.to_string()))
    } else {
        vendor_status(&msg).filter(|s| *s == 429 || *s >= 500).map(|s| {
            let code = if matches!(s, 429 | 502 | 503 | 504) { s } else { 503 };
            RpcError::Api(json!({ "ok": false, "error_code": code, "description": msg }).to_string())
        })
    };
    match typed {
        Some(t) => anyhow::Error::new(t).context(format!("{method} failed")),
        None => e.context(format!("{method} failed")),
    }
}

/// The HTTP status in one of the ark driver's `ark <status>: …` sentences.
fn vendor_status(msg: &str) -> Option<u16> {
    msg.strip_prefix("ark ")?.get(..3)?.parse().ok()
}

/// This bot may not use its owner's key yet, on a REAL submit — the one place
/// a card goes up (a dry run quotes, `status` / `cancel` explain; 2026-10-10).
/// Nothing goes ahead — and nothing moves onto the wallet instead: that is the
/// owner's money, on the path they chose not to be on. The consent card goes
/// where the turn is (`MAFOLD_CONV`, in its forum channel when there is one);
/// Allow brings the bot back (`resume_after_consent`). Answers the error to
/// end the command with.
async fn ask_owner(asker: &Client, owner: &str, bot: &str, name: &str) -> anyhow::Error {
    let here = |var: &str| std::env::var(var).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let asked = match here("MAFOLD_CONV") {
        None => format!(
            "This isn't a conversation turn, so no consent card was posted — @{owner} can allow it under \
             Settings ▸ Connections."
        ),
        Some(conv) => {
            let mut body = json!({ "user": owner, "conversation_id": conv, "connection": name });
            if let Some(channel) = here("MAFOLD_FORUM_CHANNEL") {
                body["channel_id"] = json!(channel);
            }
            match asker.call("requestConnectionAccess", body).await {
                Ok(v) if v["granted"] == true => "The server says it is allowed now — run this again.".to_string(),
                Ok(v) => format!(
                    "A consent card for @{owner} is in the chat (message {}); when they tap Allow you are \
                     brought back — run this again then.",
                    v["message_id"].as_str().unwrap_or("?")
                ),
                Err(e) => format!(
                    "The consent card couldn't be posted ({e:#}) — @{owner} can allow it under Settings ▸ \
                     Connections."
                ),
            }
        }
    };
    anyhow!(
        "@{owner} has linked their own ModelArk key (`{name}`), and video runs on it by default — but \
         @{owner} hasn't allowed @{bot} to use it. Nothing was submitted and nothing was spent — and it \
         was not moved onto the Mafold wallet: `--via house` does that, and only with @{owner}'s yes. {asked}"
    )
}

/// The owner's key is there and this turn may use it, but this machine can't
/// open their vault. Said in the clip's terms, not the vault's.
fn no_vault_here(e: anyhow::Error, owner: &str, name: &str, submitting: bool) -> anyhow::Error {
    if e.downcast_ref::<crate::connection::KeyNotHandedOver>().is_none() {
        return e.context(format!("couldn't open @{owner}'s vault on this machine for `{name}`"));
    }
    let then = if submitting {
        format!(
            "Nothing was submitted and nothing was spent; `--via house` bills @{owner}'s Mafold wallet \
             instead, with their yes."
        )
    } else {
        "The job itself is untouched; read it again once the key is here.".to_string()
    };
    anyhow!(
        "@{owner}'s own ModelArk key (`{name}`) is what this runs on, but this machine doesn't hold \
         @{owner}'s vault key yet — opening Mafold on a device that has it (web, mac or iOS) hands it \
         over. {then}"
    )
}

/// `--via own` with no key this machine can use.
fn no_own_key(via: &Via, own: Option<&OwnKey>) -> String {
    let named = via.wants().map(|n| format!(" named `{n}`")).unwrap_or_default();
    match own {
        Some(OwnKey::None { owner }) => format!(
            "--via own: @{owner} has no ModelArk connection{named} — link one under Settings ▸ \
             Connections; leave --via off and Mafold's key is used. Nothing was submitted."
        ),
        Some(OwnKey::Unseen { why }) => format!(
            "--via own: the owner's own ModelArk key can't be used from here — {why}. Nothing was submitted."
        ),
        _ => "--via own: no own ModelArk key to use. Nothing was submitted.".to_string(),
    }
}

/// The server turned a house submit down because the owner has their own key
/// (`own_key_linked`). It knows THAT; only this machine knows why the key
/// wasn't used here — most often the known limit (2026-10-10): the owner isn't
/// signed in on the machine this bot runs on, and nothing relays the call to
/// one where they are. Say both, and that the wallet is the owner's call, not
/// the bot's.
fn house_refused(e: anyhow::Error, why_not_own: Option<&str>) -> anyhow::Error {
    match why_not_own {
        Some(why) if format!("{e:#}").contains("own_key_linked") => anyhow!(
            "{e:#}\n  — this machine couldn't run it on that key: {why}. Don't switch to `--via house` on \
             your own: that bills the owner's Mafold wallet, and needs the owner's yes."
        ),
        _ => e,
    }
}

/// A step before the job is touched — finding the owner's key, opening their
/// vault — that ran out of [`patiently`]'s tries on the network: said as the
/// network, with `then` (what that means for the job), never as an answer
/// about the key. Anything else is already in words and passes through.
fn out_of_reach(e: anyhow::Error, then: &str) -> anyhow::Error {
    if !retryable(&e) {
        return e;
    }
    e.context(format!("couldn't reach the owner's own ModelArk key on the network — {then}"))
}

// ── waiting ────────────────────────────────────────────────────────────────

/// `--wait`: how long one tick is, how long a job may take, and how many
/// failed reads IN A ROW mean "give up" rather than "the network blinked".
const WAIT_TICK: std::time::Duration = std::time::Duration::from_secs(10);
const WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const WAIT_MISSES: u32 = 8;
const WAIT_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(60);
/// [`patiently`]'s tries for a command that isn't waiting anyway — a submit, a
/// `cancel`, one `status` read: a blink or two (10 s, 20 s), then it says so.
const OPEN_TRIES: u32 = 3;

/// The pause after the `misses`-th failure in a row: 10 s, 20 s, 40 s, then
/// every 60 s.
fn backoff(misses: u32) -> std::time::Duration {
    (WAIT_TICK * 2u32.pow(misses.saturating_sub(1).min(16))).min(WAIT_BACKOFF_CAP)
}

/// A step before the reads — finding the owner's key, opening their vault —
/// with the reads' patience (2026-10-10). A blink ([`retryable`]) is tried
/// again on [`backoff`], up to `tries` in a row; an answer, good or bad, comes
/// back at once. Folded into "no key here", one 502 on the way to the owner's
/// vault ended `status --wait` on a job that went on running, and the agent
/// reported the clip failed.
async fn patiently<T, O, OF, S, SF>(tries: u32, mut op: O, mut sleep: S) -> Result<T>
where
    O: FnMut() -> OF,
    OF: std::future::Future<Output = Result<T>>,
    S: FnMut(std::time::Duration) -> SF,
    SF: std::future::Future<Output = ()>,
{
    let mut misses = 0u32;
    loop {
        match op().await {
            Err(e) if retryable(&e) => {
                misses += 1;
                if misses >= tries {
                    return Err(e.context(format!("{misses} tries in a row didn't get through")));
                }
                let pause = backoff(misses);
                eprintln!("  couldn't reach the owner's key ({e:#}) — retry {misses}/{tries} in {}s", pause.as_secs());
                sleep(pause).await;
            }
            done => return done,
        }
    }
}

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
                    eprintln!("  {status} … {}", mafold_core::mafold_types::units::EN.seconds(waited.as_secs() as f64));
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
                let pause = backoff(misses);
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
/// Anything else the server said is an answer that will not change. On the
/// owner's own key the driver's failures are read into the same types first
/// ([`own_failure`]), and finding the key / opening the vault keep theirs
/// (`connection::own_ark_key`, `connection::no_key_yet`), which read them by
/// this same rule.
pub(crate) fn retryable(e: &anyhow::Error) -> bool {
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
/// (`~/.mafold/plugins/mafold`): the agent sees it as `mafold:mafold-video`,
/// the text lives in this binary,
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
    std::fs::create_dir_all(dir.join("director"))?;
    std::fs::write(dir.join("SKILL.md"), SKILL_MD)?;
    for (name, body) in DIRECTOR {
        std::fs::write(dir.join("director").join(name), body)?;
    }
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

/// The director stage the skill sends the agent to (`director/` next to
/// SKILL.md): a blocking file → a grey-box animatic of jointed mannequins, which
/// goes to the model as `reference_video`. Rendered by node + headless Chrome on
/// the agent's machine; nothing here runs it.
pub const DIRECTOR: [(&str, &str); 4] = [
    ("README.md", include_str!("video_director/README.md")),
    ("render.mjs", include_str!("video_director/render.mjs")),
    ("stage.html", include_str!("video_director/stage.html")),
    ("example.json", include_str!("video_director/example.json")),
];

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

    /// Runs `find_lost` over a scripted sequence of `video.list` answers;
    /// returns the outcome, every pause, and how many answers were left over.
    async fn find_over(looks: Vec<Result<Value>>) -> (Lost, Vec<u64>, usize) {
        let left = RefCell::new(VecDeque::from(looks));
        let slept = RefCell::new(Vec::new());
        let out = find_lost(
            || std::future::ready(left.borrow_mut().pop_front().expect("looked more often than scripted")),
            |d: Duration| {
                slept.borrow_mut().push(d.as_secs());
                std::future::ready(())
            },
        )
        .await;
        let unread = left.borrow().len();
        (out, slept.into_inner(), unread)
    }
    fn items(ids: &[&str]) -> Result<Value> {
        Ok(json!({ "items": ids.iter().map(|id| json!({ "id": id, "status": "queued", "seed": 42 })).collect::<Vec<_>>() }))
    }

    /// The 2026-10-30 run: `video.submit failed` on a blink, the task already
    /// at ModelArk. Found by its seed — that job is the answer, not an error.
    #[tokio::test]
    async fn a_lost_submit_is_found_by_its_seed() {
        let (out, slept, _) = find_over(vec![items(&["cgt-1"])]).await;
        assert!(matches!(out, Lost::Found(ref t) if t["id"] == "cgt-1"), "{out:?}");
        assert!(slept.is_empty());
        // Still being taken in when the reply broke: there on a later look.
        let (out, slept, _) = find_over(vec![items(&[]), items(&[]), items(&["cgt-2"])]).await;
        assert!(matches!(out, Lost::Found(ref t) if t["id"] == "cgt-2"), "{out:?}");
        assert_eq!(slept, vec![10, 30]);
        // A blink while looking is ridden out, not taken for "not there".
        let (out, slept, _) =
            find_over(vec![Err(failed(RpcError::Transport("reset".into()))), items(&["cgt-3"])]).await;
        assert!(matches!(out, Lost::Found(ref t) if t["id"] == "cgt-3"), "{out:?}");
        assert_eq!(slept, vec![10]);
    }

    #[tokio::test]
    async fn not_there_only_when_every_look_was_answered() {
        let (out, slept, unread) = find_over(vec![items(&[]), items(&[]), items(&[])]).await;
        assert!(matches!(out, Lost::NotThere), "{out:?}");
        assert_eq!(slept, vec![10, 30]);
        assert_eq!(unread, 0);
        // The network never came back: can't tell — never "nothing started".
        let (out, _, _) = find_over((0..OPEN_TRIES).map(|_| Err(api(503))).collect()).await;
        assert!(matches!(out, Lost::Unknown(_)), "{out:?}");
        let (out, _, _) =
            find_over(vec![items(&[]), Err(failed(RpcError::Connect("down".into()))), Err(api(502)), Err(api(502))])
                .await;
        assert!(matches!(out, Lost::Unknown(_)), "a later look that never got through: {out:?}");
        // A core without `video.list` (a key driven elsewhere, older) sent no
        // seed either — its refusal is "can't tell", at once.
        let (out, _, unread) = find_over(vec![
            Err(anyhow::anyhow!("BytePlus ModelArk offers `video.submit`, `video.status` and `video.cancel` — `video.list` is not one of them")),
            items(&[]),
        ])
        .await;
        assert!(matches!(out, Lost::Unknown(_)), "{out:?}");
        assert_eq!(unread, 1, "no look after the refusal");
        let (out, _, _) = find_over(vec![Ok(json!({ "error": "odd" }))]).await;
        assert!(matches!(out, Lost::Unknown(_)), "an answer without items says nothing: {out:?}");
    }

    #[test]
    fn what_may_have_reached_modelark() {
        let submit = |e: anyhow::Error| own_failure(e, "video.submit");
        assert!(may_have_landed(&submit(anyhow::anyhow!("transport failed: connection reset"))));
        assert!(may_have_landed(&submit(anyhow::anyhow!("ark 500: InternalServiceError oops"))));
        assert!(may_have_landed(&submit(anyhow::anyhow!("ark 502: bad gateway"))));
        assert!(!may_have_landed(&submit(anyhow::anyhow!("connect failed: refused"))), "nothing left this machine");
        assert!(!may_have_landed(&submit(anyhow::anyhow!("ark 429: RateLimitExceeded slow down"))), "refused");
        assert!(!may_have_landed(&submit(anyhow::anyhow!("ark 400: InvalidParameter bad"))), "refused");
        assert!(!may_have_landed(&submit(anyhow::anyhow!("ark 401: AuthenticationError"))), "refused");
    }

    #[test]
    fn a_fresh_seed_is_a_real_one() {
        let seeds: std::collections::HashSet<i64> = (0..200).map(|_| fresh_seed()).collect();
        assert!(seeds.iter().all(|s| (1..=i64::from(i32::MAX)).contains(s)));
        assert!(seeds.len() > 190, "random, not a constant");
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

    /// The rule, as a table (2026-10-10, 「我都连接了我自己的key，肯定是默认走我的
    /// key啊」): their own key first; a key this bot may not use is a question
    /// for the owner, never a quiet move onto the wallet; the wallet only for
    /// an owner without a key, or by name.
    #[test]
    fn the_owners_own_key_comes_first_and_the_wallet_only_by_name() {
        use Found::*;
        assert_eq!(
            route(&Via::Auto, Denied, false),
            Route::AskOwner,
            "not allowed yet is not a reason to bill the wallet"
        );
        for dry in [false, true] {
            assert_eq!(route(&Via::Auto, Usable, dry), Route::Own);
            assert_eq!(route(&Via::Auto, Nothing, dry), Route::House { explicit: false });
            // This machine can't tell: the server can, and refuses a house
            // submit for an owner with a linked key (`own_key_linked`).
            assert_eq!(route(&Via::Auto, Unseen, dry), Route::House { explicit: false });
            for want in [None, Some("ark".to_string())] {
                let via = Via::Own(want);
                assert_eq!(route(&via, Usable, dry), Route::Own);
                assert_eq!(route(&via, Nothing, dry), Route::Refuse, "--via own never falls back");
                assert_eq!(route(&via, Unseen, dry), Route::Refuse, "--via own never falls back");
            }
            for f in [Nothing, Unseen, Usable, Denied] {
                assert_eq!(route(&Via::House, f, dry), Route::House { explicit: true }, "the wallet, by name: {f:?}");
            }
        }
    }

    /// 2026-10-10: the skill prices a plan with dry runs, several at a time. A
    /// bot its owner hasn't let onto their key gets the price — the server's,
    /// labelled for ModelArk's bill — and `allowed: false`, and NOBODY is asked:
    /// a consent card per dry run was spam, and an Allow on a stale one brought
    /// the bot back to run the request it had only priced. Only a real submit
    /// asks.
    #[test]
    fn a_dry_run_prices_the_owners_key_without_asking_them() {
        for via in [Via::Auto, Via::Own(None), Via::Own(Some("ark".to_string()))] {
            assert_eq!(route(&via, Found::Denied, true), Route::Quote, "{via:?}: priced, nobody asked");
            assert_eq!(route(&via, Found::Denied, false), Route::AskOwner, "{via:?}: a real submit asks");
        }
        let checked = json!({
            "dry_run": true, "submitted": false, "configured": true, "walletable": false,
            "model": "dreamina-seedance-2-5-260628", "resolution": "480p", "duration": 4, "refs": 1,
            "ref_urls": [{ "role": "first_frame", "url": "https://cdn.example/f_1" }],
            "own_key_linked": true,
            "estimate": { "tokens": 39749, "usd": 0.47, "note": "估算：按钱包价 × 公式" },
        });
        let body = json!({ "refs": [{ "role": "first_frame", "file": "f_1" }], "generate_audio": true });
        let quoted = own_priced(checked.clone(), &body, billing("ark", "ops", Some("ops:cam")));
        assert_eq!(quoted["billing"]["via"], "own");
        assert_eq!(quoted["billing"]["connection"], "ark");
        assert_eq!(quoted["billing"]["owner"], "ops");
        assert_eq!(quoted["billing"]["allowed"], false);
        let note = quoted["billing"]["note"].as_str().unwrap();
        assert!(note.contains("@ops:cam") && note.contains("consent card"), "says what a real submit does: {note}");
        assert_eq!(quoted["dry_run"], true);
        // ModelArk's count and list price, not the server's wallet figure.
        assert_eq!(quoted["estimate"]["tokens"], 38830, "the vendor's count for 2.5 · 480p · 4 s");
        assert_eq!(quoted["estimate"]["usd_per_m"], 10.7, "list price at 480p");
        assert_eq!(quoted["estimate"]["note"], OWN_ESTIMATE_NOTE, "labelled for ModelArk's bill");
        assert!(quoted["estimate"]["audio"].as_str().unwrap().starts_with("on"));
        assert!(quoted["estimate"].get("input_video_secs").is_none(), "a first frame adds nothing");
        for house_fact in ["configured", "walletable", "ref_urls", "own_key_linked", "wallet"] {
            assert!(quoted.get(house_fact).is_none(), "{house_fact} is the house key's");
        }
        let allowed = own_priced(checked, &body, billing("ark", "ops", None));
        assert_eq!(allowed["billing"]["allowed"], true);
        assert!(!allowed["billing"]["note"].as_str().unwrap().contains("consent card"));
    }

    /// The known limit, said as it is: the owner isn't signed in where this
    /// bot runs, so the server refused the house key for an owner who has
    /// their own — and the wallet is the owner's call, not the bot's.
    #[test]
    fn a_house_refusal_says_why_the_owners_key_wasnt_used_here() {
        let refused = || anyhow!("own_key_linked: @ops has linked their own ModelArk key … Nothing was submitted");
        let why = "@ops isn't signed in to Mafold on this machine, and a bot can use its owner's own key only on \
                   a machine where the owner is";
        let said = format!("{:#}", house_refused(refused(), Some(why)));
        assert!(said.contains("own_key_linked") && said.contains(why), "both halves: {said}");
        assert!(said.contains("needs the owner's yes"), "{said}");
        assert_eq!(format!("{:#}", house_refused(refused(), None)), format!("{:#}", refused()));
        let other = house_refused(anyhow!("@ops's wallet can't cover this clip"), Some(why));
        assert!(!format!("{other:#}").contains(why), "only the own-key refusal gets the explanation");
    }

    /// Finding the owner's key and opening their vault get the reads' backoff:
    /// a blink is tried again, an answer comes back at once, and running out
    /// of tries keeps the cause — typed, so it still reads as the network.
    #[tokio::test]
    async fn the_way_to_the_owners_key_is_tried_again_after_a_blink() {
        async fn over(results: Vec<Result<&'static str>>, tries: u32) -> (Result<&'static str>, Vec<u64>, usize) {
            let left = RefCell::new(VecDeque::from(results));
            let slept = RefCell::new(Vec::new());
            let out = patiently(
                tries,
                || std::future::ready(left.borrow_mut().pop_front().unwrap_or(Ok("opened"))),
                |d: Duration| {
                    slept.borrow_mut().push(d.as_secs());
                    std::future::ready(())
                },
            )
            .await;
            let unread = left.borrow().len();
            (out, slept.into_inner(), unread)
        }

        let (out, slept, _) = over(vec![Err(api(502)), Err(failed(RpcError::Transport("reset".into())))], OPEN_TRIES).await;
        assert_eq!(out.unwrap(), "opened");
        assert_eq!(slept, vec![10, 20]);

        let (out, slept, unread) = over(vec![Err(anyhow!("@ops hasn't allowed @ops:cam")), Ok("never")], OPEN_TRIES).await;
        assert!(format!("{:#}", out.unwrap_err()).contains("hasn't allowed"), "an answer ends it at once");
        assert!(slept.is_empty());
        assert_eq!(unread, 1);

        let (out, slept, _) = over((0..WAIT_MISSES + 2).map(|_| Err(api(503))).collect(), WAIT_MISSES).await;
        let e = out.unwrap_err();
        assert!(retryable(&e), "still the network: {e:#}");
        assert!(format!("{e:#}").contains(&format!("{WAIT_MISSES} tries in a row")), "{e:#}");
        assert_eq!(slept, vec![10, 20, 40, 60, 60, 60, 60], "the reads' backoff");
        let said = format!("{:#}", out_of_reach(e, "the job is still ModelArk's"));
        assert!(said.contains("on the network") && said.contains("the job is still ModelArk's"), "{said}");
        let answer = anyhow!("`own:ark:cgt-1` runs on @ops's own ModelArk key … hasn't allowed");
        assert!(!format!("{:#}", out_of_reach(answer, "x")).contains("on the network"), "an answer stays one");
    }

    #[test]
    fn via_reads_own_house_or_a_connection() {
        assert_eq!(Via::parse(None).unwrap(), Via::Auto);
        assert_eq!(Via::parse(Some("house")).unwrap(), Via::House);
        assert_eq!(Via::parse(Some(" own ")).unwrap(), Via::Own(None));
        assert_eq!(Via::parse(Some("own:ark-2")).unwrap(), Via::Own(Some("ark-2".into())));
        assert_eq!(Via::parse(Some("own:ark-2")).unwrap().wants(), Some("ark-2"));
        assert_eq!(Via::parse(Some("own")).unwrap().wants(), None);
        for bad in ["wallet", "own:", "mine", "own:  "] {
            assert!(Via::parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_job_id_says_which_key_it_runs_on() {
        assert_eq!(parse_job("ark1~cgt-20261010").unwrap(), Job::House);
        let id = own_job_id("ark", "cgt-20261010-abc");
        assert_eq!(id, "own:ark:cgt-20261010-abc");
        assert_eq!(parse_job(&id).unwrap(), Job::Own { connection: "ark", task: "cgt-20261010-abc" });
        for bad in ["own:ark", "own::cgt-1", "own:ark:"] {
            assert!(parse_job(bad).is_err(), "{bad}");
        }
    }

    /// The same shape as a house `videoStatus`, minus the wallet: an agent
    /// that has read one reads the other.
    #[test]
    fn an_own_key_status_reads_like_a_house_one_minus_the_wallet() {
        let task = json!({
            "id": "cgt-1", "model": "dreamina-seedance-2-5-260628", "status": "succeeded",
            "resolution": "480p", "duration": 4,
            "content": { "video_url": "https://ark-cdn.example/v.mp4" },
            "usage": { "completion_tokens": 39749, "total_tokens": 39749 },
            "seed": 7, "created_at": 1, "updated_at": 2,
        });
        let quote = json!({ "tokens": 38830, "usd": 0.415, "usd_per_m": 10.7 });
        let v = own_status_view("own:ark:cgt-1", "ark", "ops", &task, Some(&quote));
        assert_eq!(v["job_id"], "own:ark:cgt-1");
        assert_eq!(v["status"], "succeeded");
        assert_eq!(v["usage"], json!({ "unit": "token", "output": 39749, "total": 39749 }));
        assert!((v["cost_usd"].as_f64().unwrap() - 39749.0 * 10.7 / 1e6).abs() < 1e-9, "list price at 480p");
        assert_eq!(v["estimate"], quote, "the quote beside the bill");
        assert!(v["check"].as_str().unwrap().starts_with("close"), "{}", v["check"]);
        assert_eq!(v["billing"]["via"], "own");
        assert_eq!(v["billing"]["connection"], "ark");
        assert!(v.get("wallet").is_none(), "nothing was held, nothing is charged here");
        assert!(v.get("video_url").is_none() && v.get("content").is_none(), "the 24h link is landed, not handed out");

        let queued = own_status_view("own:ark:cgt-2", "ark", "ops", &json!({ "status": "queued" }), None);
        assert_eq!(queued["status"], "queued");
        assert!(queued["usage"].is_null() && queued["cost_usd"].is_null() && queued["check"].is_null());
        assert!(queued.get("error").is_none());

        let failed = own_status_view(
            "own:ark:cgt-3",
            "ark",
            "ops",
            &json!({ "status": "failed", "error": { "code": "InternalServiceError", "message": "boom" } }),
            Some(&quote),
        );
        assert_eq!(failed["error"], json!({ "code": "InternalServiceError", "message": "boom" }));
        assert!(failed["cost_usd"].is_null(), "no count, no figure");
        assert_eq!(own_status_view("j", "ark", "ops", &json!({}), None)["status"], "unknown");
    }

    /// 2026-10-30, director stage C: Seedance 2.0 · 480p · 5 s with a 5 s
    /// animatic. The old quote said 48,437 and ModelArk billed 100,858 at its
    /// with-video rate — the gap now shows on the job, and a quote that counts
    /// the animatic matches.
    #[test]
    fn a_reference_video_is_in_the_quote_and_the_bill_is_checked_against_it() {
        let checked = json!({ "model": "dreamina-seedance-2-0-260128", "resolution": "480p", "duration": 5 });
        let refs = json!([{ "role": "reference_video", "file": "f_animatic" }, { "role": "reference_image", "file": "f_girl" }]);
        let measured = json!({ "refs": refs.clone(), "ref_video_secs": 5.0, "generate_audio": false });
        let q = own_estimate(&checked, &measured);
        assert_eq!(q["tokens"], 100_858);
        assert_eq!(q["usd_per_m"], 4.3, "the with-video rate");
        assert_eq!(q["input_video_secs"], 5.0);
        assert!(q.get("input_video_assumed").is_none());
        assert_eq!(q["audio"], "off");
        // Not measured (a link, a file id): counted as long as the clip, and said.
        let unmeasured = own_estimate(&checked, &json!({ "refs": refs }));
        assert_eq!(unmeasured["tokens"], 100_858);
        assert!(unmeasured["input_video_assumed"].is_string());

        let task = json!({
            "model": "dreamina-seedance-2-0-260128", "status": "succeeded", "resolution": "480p", "duration": 5,
            "usage": { "completion_tokens": 100858, "total_tokens": 100858 },
        });
        let v = own_status_view("own:ark:cgt-c", "ark", "ops", &task, Some(&q));
        assert_eq!(v["check"], "match");
        assert!((v["cost_usd"].as_f64().unwrap() - 100858.0 * 4.3 / 1e6).abs() < 1e-9);
        let old = json!({ "tokens": 48437 });
        let v = own_status_view("own:ark:cgt-c", "ark", "ops", &task, Some(&old));
        assert!(v["check"].as_str().unwrap().starts_with("OFF"), "{}", v["check"]);
        // No quote kept here: a count that far over the clip's own frames is a
        // reference video's, priced at the with-video rate.
        let v = own_status_view("own:ark:cgt-c", "ark", "ops", &task, None);
        assert_eq!(v["price_usd_per_m"], 4.3);
        assert!(v["check"].as_str().unwrap().starts_with("no quote"));
    }

    /// `--no-audio` is the only way to a silent clip; `--audio` still parses.
    #[test]
    fn sound_is_on_unless_asked_off() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            cmd: VideoCmd,
        }
        let audio_of = |extra: &[&str]| -> bool {
            let mut args = vec!["x", "submit", "--model", "m", "p"];
            args.extend_from_slice(extra);
            match Cli::try_parse_from(args).expect("parses").cmd {
                VideoCmd::Submit { no_audio, .. } => !no_audio,
                _ => unreachable!(),
            }
        };
        assert!(audio_of(&[]), "on by default");
        assert!(audio_of(&["--audio"]), "the old flag still runs");
        assert!(!audio_of(&["--no-audio"]));
        assert!(Cli::try_parse_from(["x", "submit", "--model", "m", "p", "--audio", "--no-audio"]).is_err());
    }

    /// A reference video's length, off the file's own `mvhd`.
    #[test]
    fn an_mp4s_length_is_read_off_its_header() {
        fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(payload);
            b
        }
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes()); // timescale
        mvhd[16..20].copy_from_slice(&5040u32.to_be_bytes()); // duration
        let mut file = bx(b"ftyp", b"isom0000");
        file.extend(bx(b"moov", &[bx(b"trak", &[0u8; 16]), bx(b"mvhd", &mvhd)].concat()));
        file.extend(bx(b"mdat", &[0u8; 64]));
        let dir = std::env::temp_dir().join(format!("mvhd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.mp4");
        std::fs::write(&p, &file).unwrap();
        assert_eq!(mp4_secs(&p), Some(5.04));
        std::fs::write(&p, b"not a video").unwrap();
        assert_eq!(mp4_secs(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The driver's blinks get #826's backoff exactly as the house path's do;
    /// its verdicts (a dead key, a bad parameter) end the wait at once — and
    /// its words survive either way.
    #[test]
    fn the_drivers_blinks_are_read_again_and_its_verdicts_are_not() {
        for blink in [
            "connect failed: tcp connect error",
            "transport failed: operation timed out",
            "ark 429: RateLimitExceeded slow down",
            "ark 500: InternalServiceError boom",
            "ark 503: ServiceUnavailable",
        ] {
            let e = own_failure(anyhow::anyhow!("{blink}"), "video.status");
            assert!(retryable(&e), "{blink}");
            let said = format!("{e:#}");
            assert!(said.contains("video.status failed") && said.contains(blink), "keeps the driver's words: {said}");
        }
        for verdict in [
            "ark 401: AuthenticationError the API key is invalid",
            "ark 400: InvalidParameter nope",
            "job_id is required",
        ] {
            let e = own_failure(anyhow::anyhow!("{verdict}"), "video.status");
            assert!(!retryable(&e), "{verdict}");
            assert!(format!("{e:#}").contains(verdict));
        }
        // A relayed call's failure is typed already, and stays as it is.
        let relayed = failed(RpcError::Api(r#"{"ok":false,"error_code":503,"description":"x"}"#.into()));
        assert!(retryable(&own_failure(relayed, "video.status")));
    }

    #[test]
    fn modelark_gets_links_only() {
        let sent = vec![
            json!({ "role": "first_frame", "file": "f_1" }),
            json!({ "role": "reference_video", "url": "https://x.example/a.mp4" }),
        ];
        let checked = json!({ "ref_urls": [
            { "role": "first_frame", "url": "https://cdn.mafold.com/f_1" },
            { "role": "reference_video", "url": "https://x.example/a.mp4" },
        ]});
        assert_eq!(
            vendor_refs(&sent, &checked).unwrap(),
            vec![
                json!({ "role": "first_frame", "url": "https://cdn.mafold.com/f_1" }),
                json!({ "role": "reference_video", "url": "https://x.example/a.mp4" }),
            ]
        );
        // A server without public storage inlined the image: those bytes go
        // to ModelArk as they would on the house path.
        let png = "data:image/png;base64,iVBORw0KGgo=";
        let inline = json!({ "ref_urls": [{ "role": "first_frame", "url": png }] });
        assert_eq!(vendor_refs(&sent[..1], &inline).unwrap(), vec![json!({ "role": "first_frame", "url": png })]);
        // Nothing ModelArk can read (a video file there): refused before
        // anything is spent.
        let none = json!({ "ref_urls": [{ "role": "first_frame", "url": null }] });
        let e = vendor_refs(&sent[..1], &none).unwrap_err().to_string();
        assert!(e.contains("first_frame") && e.contains("--via house"), "{e}");
        // An api from before `ref_urls`: a link passes through, a file can't.
        assert_eq!(vendor_refs(&sent[1..], &json!({})).unwrap(), vec![sent[1].clone()]);
        assert!(vendor_refs(&sent[..1], &json!({})).unwrap_err().to_string().contains("predates"));
        assert!(vendor_refs(&[], &json!({})).unwrap().is_empty());
    }

    #[test]
    fn a_landed_clip_is_remembered_and_the_oldest_fall_off() {
        let dir = std::env::temp_dir().join(format!("mafold-video-landed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("video-landed.json");
        let k = |job: &str| landed_key("https://api.mafold.com", job);
        assert!(landed_get(&path, &k("own:ark:cgt-1")).is_none(), "no cache yet is an empty one");
        landed_put(&path, &k("own:ark:cgt-1"), &json!({ "id": "f_1", "mime": "video/mp4" }));
        assert_eq!(landed_get(&path, &k("own:ark:cgt-1")).unwrap()["id"], "f_1");
        landed_put(&path, &k("own:ark:cgt-1"), &json!({ "id": "f_1b" }));
        assert_eq!(landed_get(&path, &k("own:ark:cgt-1")).unwrap()["id"], "f_1b", "one entry per job");
        for i in 0..LANDED_KEEP {
            landed_put(&path, &k(&format!("own:ark:cgt-x{i}")), &json!({ "id": format!("f_x{i}") }));
        }
        assert!(landed_get(&path, &k("own:ark:cgt-1")).is_none(), "the oldest fell off");
        assert!(landed_get(&path, &k(&format!("own:ark:cgt-x{}", LANDED_KEEP - 1))).is_some());
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("video-landed.json")], "no temp file, no lock left behind");

        // A cache this can't read reads as empty — and is left as it is, not
        // replaced by a cache of one.
        std::fs::write(&path, "not json").unwrap();
        assert!(landed_get(&path, &k("own:ark:cgt-x1")).is_none(), "a broken cache is an empty one");
        landed_put(&path, &k("own:ark:cgt-new"), &json!({ "id": "f_new" }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json", "not clobbered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One file under `~/.mafold` that every cli on this HOME shares
    /// (2026-10-10): keyed by api as well as job, so a dev and a prod cli — or
    /// two bots — never hand out each other's files; a cache it can't read is
    /// never replaced by a map of one; and two `status` runs landing at once
    /// both keep their entry.
    #[test]
    fn the_landed_cache_is_per_api_and_survives_its_neighbours() {
        let prod = landed_key("https://api.mafold.com/", "own:ark:cgt-1");
        let dev = landed_key("http://127.0.0.1:4000", "own:ark:cgt-1");
        assert_eq!(prod, "https://api.mafold.com|own:ark:cgt-1");
        assert_ne!(prod, dev, "the same job id from another api is another job");
        let text = landed_with(None, &prod, &json!({ "id": "f_prod" })).unwrap();
        assert_eq!(landed_find(&text, &prod).unwrap()["id"], "f_prod");
        assert!(landed_find(&text, &dev).is_none(), "prod's file is not dev's");
        let text = landed_with(Some(text.as_str()), &dev, &json!({ "id": "f_dev" })).unwrap();
        assert_eq!(landed_find(&text, &prod).unwrap()["id"], "f_prod", "kept");
        assert_eq!(landed_find(&text, &dev).unwrap()["id"], "f_dev");
        let half = &text[..text.len() / 2];
        for unreadable in ["", half, "not json", "{}"] {
            assert!(landed_with(Some(unreadable), &dev, &json!({ "id": "f" })).is_none(), "{unreadable:?} left alone");
            assert!(landed_find(unreadable, &prod).is_none());
        }

        let dir = std::env::temp_dir().join(format!("mafold-video-landed-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("video-landed.json");
        std::thread::scope(|s| {
            for t in 0..2 {
                let path = &path;
                s.spawn(move || {
                    for i in 0..30 {
                        landed_put(path, &format!("api|own:ark:t{t}-{i}"), &json!({ "id": format!("f{t}-{i}") }));
                    }
                });
            }
        });
        for t in 0..2 {
            for i in 0..30 {
                let got = landed_get(&path, &format!("api|own:ark:t{t}-{i}"));
                assert_eq!(got.map(|f| f["id"].clone()), Some(json!(format!("f{t}-{i}"))), "t{t}-{i} kept");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
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
        for (name, body) in DIRECTOR {
            let got = std::fs::read_to_string(plugin.join("skills/mafold-video/director").join(name)).unwrap();
            assert_eq!(got, body, "director/{name} is installed beside the skill");
        }
        for name in ["README.md", "example.json", "render.mjs"] {
            assert!(SKILL_MD.contains(&format!("director/{name}")), "the skill sends the agent to director/{name}");
        }
        assert_eq!(crate::drive::plugin_name(&plugin).as_deref(), Some("mafold"));
        assert!(!old.exists(), "our own copy is gone");

        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), "the owner's own edit").unwrap();
        install_into(&plugin, Some(&home)).unwrap();
        assert_eq!(std::fs::read_to_string(old.join("SKILL.md")).unwrap(), "the owner's own edit", "not ours: left alone");
        let _ = std::fs::remove_dir_all(&root);
    }
}
