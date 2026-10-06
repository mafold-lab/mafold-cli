//! Drive a REAL `mafold pair` machine through a real api and count the calls
//! it never answered.
//!
//! The relay between `callConnection` and a paired machine is three hops and
//! two processes, and the way it loses calls only shows up under the shape an
//! agent actually gives it: calls back to back, a background poller talking
//! over a foreground job, commands that keep the machine busy. A unit test
//! stubs one of those away. This drives all of them:
//!
//! ```text
//!   PORT=4055 cargo run                       # mafold-api, no DATABASE_URL
//!   HOME=$(mktemp -d) mafold pair --base http://127.0.0.1:4055
//!   cargo run --example pair_stress -- --pack pack.json --calls 200
//! ```
//!
//! It approves the waiting pairing the way the web does (`seal_payload_for`
//! to the machine's own key), holds a socket for the owner the way their
//! other devices do — so a call the machine misses times out exactly as in
//! production instead of failing fast with "none of your devices are online" —
//! and prints one JSON line per call, then a summary.
//!
//! A call is LOST when it comes back "no device answered": the machine was
//! listening and the call never reached it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:4055")]
    base: String,
    /// The account the machine is paired to (a dev token is used for it).
    #[arg(long, default_value = "ops")]
    owner: String,
    /// A signed provider pack to publish first: a fresh in-memory api has
    /// none, and a machine without one declines every computer call.
    #[arg(long)]
    pack: Option<PathBuf>,
    /// Skip pairing and call this existing connection.
    #[arg(long)]
    connection: Option<String>,
    /// Calls to make in total, foreground and background together.
    #[arg(long, default_value_t = 200)]
    calls: usize,
    /// A background poller's pause between its calls; 0 = no poller.
    #[arg(long, default_value_t = 1500)]
    poll_ms: u64,
    /// Every Nth foreground call keeps the machine busy; 0 = never.
    #[arg(long, default_value_t = 4)]
    busy_every: usize,
    /// How long a busy call runs on the machine.
    #[arg(long, default_value_t = 2000)]
    busy_ms: u64,
}

struct Api {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Api {
    /// `{ok, result}` → result; anything else → its description as the error.
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{}/api/{method}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{method}: request failed"))?;
        let v: Value = resp.json().await.with_context(|| format!("{method}: non-JSON"))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        let why = ["description", "error", "message"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string());
        Err(anyhow!("{why}"))
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[derive(Default)]
struct Tally {
    ok: usize,
    lost: usize,
    other: usize,
    ms: Vec<u128>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let api = Arc::new(Api {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()?,
        base: args.base.trim_end_matches('/').to_string(),
        token: format!("dev:{}", args.owner),
    });

    if let Some(path) = &args.pack {
        let pack: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        let publisher = Api {
            http: api.http.clone(),
            base: api.base.clone(),
            token: "dev:mafoldcards".into(),
        };
        match publisher.call("publishConnectionProviders", pack).await {
            Ok(r) => eprintln!("pack published: {r}"),
            // Already there from an earlier run against the same api.
            Err(e) if e.to_string().contains("move forward") => eprintln!("pack already published"),
            Err(e) => return Err(e.context("publishConnectionProviders")),
        }
    }

    let connection = match &args.connection {
        Some(c) => c.clone(),
        None => approve_waiting_machine(&api).await?,
    };
    eprintln!("calling `{connection}`");

    // The owner's OTHER device: online, listening, and unable to serve this
    // row — what every one of their browsers and laptops is to a paired box.
    let ws = hold_owner_socket(&api).await?;

    // One call that must land before the clock starts: the machine has to
    // finish its own approval poll and park once.
    let warm = Instant::now();
    loop {
        match exec(&api, &connection, "echo ready", 5_000).await {
            Ok(_) => break,
            Err(e) if warm.elapsed() < Duration::from_secs(120) => {
                eprintln!("warm-up: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => bail!("the machine never answered a warm-up call: {e:#}"),
        }
    }
    eprintln!("machine answering after {:?}; starting {} calls", warm.elapsed(), args.calls);

    let tickets = Arc::new(AtomicUsize::new(0));
    let tally = Arc::new(Mutex::new(Tally::default()));
    let started = Instant::now();

    let fg = {
        let (api, conn, tickets, tally) = (api.clone(), connection.clone(), tickets.clone(), tally.clone());
        let (every, busy_ms) = (args.busy_every, args.busy_ms);
        let total = args.calls;
        tokio::spawn(async move {
            let mut i = 0usize;
            while tickets.fetch_add(1, Ordering::SeqCst) < total {
                i += 1;
                let busy = every > 0 && i % every == 0;
                let (cmd, timeout) = if busy {
                    (format!("sleep {:.1} && echo fg{i}", busy_ms as f64 / 1000.0), busy_ms + 3_000)
                } else {
                    (format!("echo fg{i}"), 5_000)
                };
                one(&api, &conn, "fg", i, &cmd, timeout, &tally).await;
            }
        })
    };
    let bg = (args.poll_ms > 0).then(|| {
        let (api, conn, tickets, tally) = (api.clone(), connection.clone(), tickets.clone(), tally.clone());
        let (poll, total) = (args.poll_ms, args.calls);
        tokio::spawn(async move {
            let mut j = 0usize;
            loop {
                tokio::time::sleep(Duration::from_millis(poll)).await;
                if tickets.fetch_add(1, Ordering::SeqCst) >= total {
                    break;
                }
                j += 1;
                one(&api, &conn, "bg", j, &format!("echo bg{j}"), 5_000, &tally).await;
            }
        })
    });
    fg.await?;
    if let Some(bg) = bg {
        bg.await?;
    }
    ws.abort();

    let t = tally.lock().await;
    let mut ms = t.ms.clone();
    ms.sort_unstable();
    let pct = |p: f64| ms.get(((ms.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0);
    println!(
        "{}",
        json!({
            "summary": true,
            "calls": t.ok + t.lost + t.other,
            "ok": t.ok,
            "lost": t.lost,
            "other_errors": t.other,
            "wall_s": started.elapsed().as_secs_f64(),
            "answered_ms_p50": pct(0.5),
            "answered_ms_p95": pct(0.95),
            "answered_ms_max": ms.last().copied().unwrap_or(0),
        })
    );
    Ok(())
}

async fn one(api: &Api, conn: &str, who: &str, i: usize, cmd: &str, timeout_ms: u64, tally: &Mutex<Tally>) {
    let at = now_ms();
    let t0 = Instant::now();
    let r = exec(api, conn, cmd, timeout_ms).await;
    let ms = t0.elapsed().as_millis();
    let (outcome, detail) = match &r {
        Ok(v) => {
            let want = cmd.rsplit(' ').next().unwrap_or("");
            let out = v.get("stdout").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if out == want { ("ok", out) } else { ("other", format!("unexpected output: {v}")) }
        }
        Err(e) if format!("{e:#}").contains("no device answered") => ("lost", format!("{e:#}")),
        Err(e) => ("other", format!("{e:#}")),
    };
    println!(
        "{}",
        json!({ "who": who, "i": i, "at_ms": at, "ms": ms, "outcome": outcome, "detail": detail })
    );
    let mut t = tally.lock().await;
    match outcome {
        "ok" => {
            t.ok += 1;
            t.ms.push(ms);
        }
        "lost" => t.lost += 1,
        _ => t.other += 1,
    }
}

/// The command's own result (`{exit_code, stdout, …}`), out of `{result}`.
async fn exec(api: &Api, conn: &str, cmd: &str, timeout_ms: u64) -> Result<Value> {
    let r = api
        .call(
            "callConnection",
            json!({
                "connection": conn,
                "method": "shell.exec",
                "params": { "cmd": cmd, "timeout_ms": timeout_ms },
            }),
        )
        .await?;
    Ok(r.get("result").cloned().unwrap_or(r))
}

/// Approve the newest machine waiting to be paired — what the web's
/// Settings ▸ Connections ▸ Pair a computer does, byte for byte: the row's
/// payload names the machine's public key as its device, and the row's DEK
/// is wrapped to that same key.
async fn approve_waiting_machine(api: &Api) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let pending = loop {
        let items = api.call("listMachinePairings", json!({})).await?;
        if let Some(p) = items
            .get("items")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().max_by_key(|p| p.get("created_at").and_then(Value::as_i64).unwrap_or(0)))
        {
            break p.clone();
        }
        if Instant::now() > deadline {
            bail!("no machine is waiting — start `mafold pair --base {}` first", api.base);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let public_key = pending["public_key"].as_str().unwrap_or_default().to_string();
    let machine = pending["machine"].as_str().unwrap_or("stress box").to_string();
    let umk = mafold_core::vault::Key::random();
    let sealed = mafold_core::vault::seal_payload_for(
        &umk,
        &public_key,
        &json!({ "device_id": public_key, "machine": machine }).to_string(),
    )
    .map_err(|e| anyhow!("seal: {e}"))?;
    let r = api
        .call(
            "approveMachinePairing",
            json!({
                "user_code": pending["user_code"],
                "name": format!("stress-{}", now_ms() % 100_000),
                "provider": "computer",
                "label": machine,
                "blob": sealed.blob,
                "wrapped_dek": sealed.wrapped_dek,
                "sealed_dek": sealed.sealed_dek,
                "key_id": "stress",
            }),
        )
        .await?;
    r.get("connection")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("approveMachinePairing answered without a connection: {r}"))
}

async fn hold_owner_socket(api: &Api) -> Result<tokio::task::JoinHandle<()>> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = format!(
        "{}/api/ws",
        api.base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1)
    );
    let mut req = url.into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {}", api.token).parse()?);
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.context("owner socket")?;
    // Reading is what answers the server's pings; nothing here is acted on.
    Ok(tokio::spawn(async move { while let Some(Ok(_)) = ws.next().await {} }))
}
