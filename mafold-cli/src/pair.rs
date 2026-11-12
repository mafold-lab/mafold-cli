//! `mafold pair` — lend this machine to a Mafold account without signing in.
//!
//! The other way for a box to answer a `computer` connection is `mafold login`
//! plus vault enrolment, which leaves it holding a session (it can speak as
//! you) and the user master key (it can open every credential you own). For
//! your own laptop that is right. For a rented GPU box, a colleague's desktop
//! or a CI runner it is absurd — you wanted to lend a shell, not an identity.
//!
//! So this command asks for the narrow thing:
//!
//! ```text
//!   mafold pair
//!     ├ generate/reuse this machine's keypair (private half never leaves)
//!     ├ startMachinePairing         → a code + a fingerprint, printed here
//!     ├ (its owner types the code in Settings ▸ Connections and taps approve;
//!     │  their browser holds the vault, seals ONE row's DEK to our public key)
//!     ├ pollMachinePairing          → { token, sealed_dek }
//!     ├ unwrap the DEK with our private key, write ~/.mafold/paired.json
//!     └ serve: waitConnectionCall → the SAME core that answers on a laptop
//! ```
//!
//! What this machine ends up holding, in full: one connection's DEK, and a
//! token scoped `connection.answer:<that connection>`. No session, no UMK, no
//! socket (a socket would carry the account's messages here), and no way to
//! ask for another connection — `listConnections` on this token returns
//! exactly one row.
//!
//! Run it again to serve an existing pairing; it only pairs when there is
//! nothing on disk. Losing the file costs one re-pair, which is the point:
//! nothing here is worth stealing beyond the one row it names.
//!
//! One process serves a pairing at a time: the one started last. Running it
//! again — in an admin console, say — takes the pairing over, and the older
//! process finishes what it is running and stops. Every command runs with
//! exactly the rights of the process that took it, and every answer says
//! which process that was and whether it was elevated.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

use mafold_core::connections::{handle_event, Runtime};
use mafold_core::vault::{unwrap_key, Key};

use crate::client::Client;

/// How often to ask whether the human has approved yet. Two seconds is what
/// `mafold login` polls at, and pairing is the same wait for the same reason.
const POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// What this machine keeps after a pairing. One row's key and a token that
/// can answer for it — the whole of what was granted, written down.
#[derive(Serialize, Deserialize)]
struct Paired {
    /// The connection this machine answers for.
    connection: String,
    /// `connection.answer:<connection>` scoped token.
    token: String,
    /// The row's DEK, base64. Not the master key: it opens this row and
    /// nothing else in the account.
    dek: String,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn paired_path() -> PathBuf {
    home().join(".mafold/paired.json")
}

fn load() -> Option<Paired> {
    serde_json::from_str(&std::fs::read_to_string(paired_path()).ok()?).ok()
}

fn save(p: &Paired) -> Result<()> {
    let path = paired_path();
    std::fs::create_dir_all(home().join(".mafold")).ok();
    std::fs::write(&path, serde_json::to_string_pretty(p)?)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// `mafold pair` — pair if this machine isn't paired yet, then serve.
pub async fn run(base: &str, name: Option<String>, forget: bool) -> Result<()> {
    if forget {
        let path = paired_path();
        let had = std::fs::remove_file(&path).is_ok();
        println!(
            "{}",
            if had {
                format!("Forgot the pairing in {}.\n\
                         The token it held still exists on the account until its owner deletes \n\
                         that connection (or the key in Settings ▸ Tokens) — this only forgets \n\
                         OUR copy.", path.display())
            } else {
                "This machine isn't paired.".to_string()
            }
        );
        return Ok(());
    }

    // The machine's keypair — the same file `mafold connection` would use if
    // this box were ever enrolled properly. One keypair per machine, whichever
    // way it is trusted.
    let dev = crate::vault::device_key()?;

    let paired = match load() {
        Some(p) => {
            println!("Paired for `{}` — serving.", p.connection);
            p
        }
        None => pair(base, &dev, name).await?,
    };

    let dek = Key::from_b64(&paired.dek)
        .map_err(|e| anyhow!("{} holds an unreadable key ({e}) — `mafold pair --forget` and pair again", paired_path().display()))?;
    serve(base, &dev.public, paired, dek, &Instance::this_process(), &lock_path()).await
}

// ───────────────── one process per pairing ─────────────────

/// This process, as it introduces itself on every park — the machine's half
/// of the api's lease (`connections_call::Lease`): the pairing belongs to the
/// process started last, and an older one is told to stop.
///
/// Before this, nothing stopped a second `mafold pair` on the same machine.
/// Each held the same token and key, each was handed every call, and whichever
/// claimed first ran it — on linsky's ThinkBook (2026-10-08) five at once, so
/// the admin window the lender had just opened kept losing to older,
/// non-elevated ones, and the commands kept coming back "Access is denied".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Instance {
    id: String,
    started_ms: i64,
    pid: u32,
    elevated: bool,
}

impl Instance {
    fn this_process() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            started_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
            pid: std::process::id(),
            elevated: crate::platform::elevated(),
        }
    }

    /// The api's ordering (`connections_call::Instance::newer_than`): the
    /// later start, ties broken by id.
    fn newer_than(&self, other: &Instance) -> bool {
        (self.started_ms, self.id.as_str()) > (other.started_ms, other.id.as_str())
    }

    fn rights(&self) -> &'static str {
        if self.elevated {
            "with admin rights"
        } else {
            "without admin rights"
        }
    }
}

/// Next to `paired.json`: which process holds this machine's pairing. The
/// api's lease is what actually decides; this lets the process that loses
/// find out even while the api is out of reach, and lets the one that wins
/// say whom it replaced. A write that fails costs only that: the losing
/// process stops on the api's word instead.
fn lock_path() -> PathBuf {
    home().join(".mafold/paired.lock")
}

fn holder(lock: &std::path::Path) -> Option<Instance> {
    serde_json::from_str(&std::fs::read_to_string(lock).ok()?).ok()
}

/// Take the machine's pairing for `me`. Returns the process that held it, if
/// it is still running — it will see this and stop.
fn take_over(lock: &std::path::Path, me: &Instance) -> Option<Instance> {
    let before = holder(lock).filter(|h| h.id != me.id && crate::platform::pid_alive(h.pid));
    // Best effort: the api's lease holds without it.
    if let Some(dir) = lock.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let staged = lock.with_extension("lock.tmp");
    if let Ok(body) = serde_json::to_string(me) {
        if std::fs::write(&staged, body).is_ok() {
            std::fs::rename(&staged, lock).ok();
        }
    }
    before
}

/// The api refused this machine's token outright: the pairing was revoked
/// (its connection deleted, or the key removed in Settings ▸ Tokens).
fn revoked(e: &anyhow::Error) -> bool {
    e.downcast_ref::<mafold_core::RpcError>().is_some_and(|re| match re {
        mafold_core::RpcError::Api(env) => serde_json::from_str::<Value>(env)
            .ok()
            .and_then(|v| v.get("error_code").and_then(Value::as_u64))
            == Some(401),
        _ => false,
    })
}

/// The pairing ceremony, up to the moment this machine holds a key.
async fn pair(base: &str, dev: &crate::vault::DeviceKey, name: Option<String>) -> Result<Paired> {
    // No token: this box has no credential of the account's, which is the
    // entire premise. `startMachinePairing` and `pollMachinePairing` are the
    // only two routes that answer without one.
    let anon = Client::new(base.to_string(), String::new());
    let machine = name.unwrap_or_else(crate::session::device_name);

    let started = anon
        .call(
            "startMachinePairing",
            json!({ "machine": machine, "public_key": dev.public }),
        )
        .await
        .context("this server doesn't offer machine pairing yet (update the api)")?;
    let pair_id = s(&started, "pair_id");
    let code = s(&started, "user_code");
    // Digested HERE, from the key this machine generated. Not read out of the
    // server's answer: a server that substituted the public key it relays
    // would substitute its digest with it, and the comparison would agree
    // about the wrong key. Both ends deriving their own is the whole point of
    // printing it.
    let fingerprint = crate::vault::fingerprint(&dev.public);
    let minutes = started
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(600)
        / 60;

    println!();
    println!("  This machine wants to be lent to a Mafold account.");
    println!();
    println!("      code   {code}");
    println!("      key    {fingerprint}");
    println!();
    println!("  In Mafold ▸ Settings ▸ Connections ▸ Pair a computer, type that code.");
    println!("  Check the key matches what this screen shows before approving —");
    println!("  it is the only thing that proves the row's key reaches THIS machine.");
    println!();
    println!("  Waiting (the code expires in {minutes} minutes)…");

    loop {
        tokio::time::sleep(POLL_EVERY).await;
        let r = match anon.call("pollMachinePairing", json!({ "pair_id": pair_id })).await {
            Ok(r) => r,
            // A blip in the network is not an answer. Keep asking until the
            // code expires, and let the server be the one to say no.
            Err(_) => continue,
        };
        match s(&r, "status").as_str() {
            "pending" => continue,
            "approved" => {
                let sealed = s(&r, "sealed_dek");
                let connection = s(&r, "connection");
                let token = s(&r, "token");
                // Opened HERE, with a private key that never left. A server
                // that substituted the public key it relayed would produce a
                // wrap this cannot open — which is why the fingerprint above
                // is printed rather than merely computed.
                let dek = unwrap_key(&dev.secret, &sealed).map_err(|e| {
                    anyhow!(
                        "the key that came back isn't for this machine ({e}) — \
                         check that the fingerprint shown at approval was {}",
                        crate::vault::fingerprint(&dev.public)
                    )
                })?;
                let p = Paired { connection, token, dek: dek.to_b64() };
                save(&p)?;
                println!();
                println!("  Paired for `{}`.", p.connection);
                println!("  What this machine now holds: that one connection's key, and a token");
                println!("  that can answer for it. Not a session, and not the vault.");
                println!();
                return Ok(p);
            }
            _ => {
                return Err(anyhow!(
                    "that pairing is over — nobody approved the code before it expired, \
                     or it was refused. Run `mafold pair` again to get a new one."
                ))
            }
        }
    }
}

/// How many calls this machine runs at once. Enough that a background poll is
/// never stuck behind a foreground job; few enough that a runaway caller can't
/// fork-bomb a machine somebody lent you. At the cap the machine stops parking,
/// and new calls wait on the api's board until a slot frees.
const IN_FLIGHT: usize = 8;

/// Answer calls for the one connection this machine was paired for, forever.
///
/// The loop is thin on purpose: everything that decides whether to claim, what
/// to run and how to report it lives in `mafold_core::connections::handle_event`
/// — the same function a signed-in daemon and the browser run. A paired machine
/// is not a second implementation of answering a call; it is the same one
/// holding less.
///
/// Each call runs on its own task and the loop goes straight back to park. It
/// used to run the call inline, and a machine is only listening while it is
/// parked: every call that arrived while it ran the previous one — a whole
/// command, plus four round trips to fetch the row, claim and answer — went to
/// nobody, and its caller waited out 30s for "no device answered". The api now
/// keeps such calls on a board for the next park (`after` is this machine's
/// place in it), but a call that waits behind a long job still runs late; on
/// its own task it doesn't wait.
///
/// It serves until a process started later takes the pairing over (the api
/// says so on a park, or `lock` names someone else), or the api refuses its
/// token for good. Either way it finishes the calls it is running first.
async fn serve(
    base: &str,
    public_key: &str,
    paired: Paired,
    dek: Key,
    me: &Instance,
    lock: &std::path::Path,
) -> Result<()> {
    // One runtime per call, from the same four things: a `Runtime` is used
    // `&mut`, so calls running side by side can't share one. Building it costs
    // nothing — no network — and the provider registry it reads is
    // process-wide.
    let runtime = {
        let api = format!("{base}/api");
        let (token, connection) = (paired.token.clone(), paired.connection.clone());
        let (public_key, executor) = (public_key.to_string(), crate::computer::executor());
        move || {
            let mut rt = Runtime::for_row(&api, &token, &connection, dek.clone());
            // The id in the row's sealed payload is this machine's PUBLIC KEY:
            // the very thing its owner approved. `can_serve` compares the two
            // before claiming, so a call addressed to a different machine is
            // declined here rather than claimed and fumbled.
            rt.attach_computer(&public_key, executor.clone());
            rt
        }
    };

    let anon = Client::new(base.to_string(), paired.token.clone());
    if let Some(old) = take_over(lock, me) {
        println!(
            "  Took this machine's pairing over from PID {} ({}); it finishes what it is running and stops.",
            old.pid,
            old.rights()
        );
    }
    println!(
        "  Listening as PID {}, {} — every command runs with exactly that. Ctrl-C to stop; this machine answers nothing else.",
        me.pid,
        me.rights()
    );
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(IN_FLIGHT));
    // The newest call the api has handed this machine — so it is never handed
    // that one, or anything older, again.
    let mut after = 0u64;
    let mut quiet_failures = 0u32;
    let mut refusals = 0u32;
    let outcome = loop {
        // A free slot BEFORE parking: a call handed to a machine that can't
        // start it yet would sit here instead of on the board, where a slot
        // freeing up finds it just the same.
        let Ok(slot) = slots.clone().acquire_owned().await else {
            return Ok(());
        };
        // A process started later, by the api's own ordering, and still
        // running. A lock naming an older or dead process is left for the
        // api's lease to settle, so the two can never both tell a process to
        // stop and leave the pairing with nobody.
        if let Some(h) = holder(lock).filter(|h| h.newer_than(me) && crate::platform::pid_alive(h.pid)) {
            break Stop::Replaced { pid: Some(h.pid), elevated: Some(h.elevated) };
        }
        match anon.call("waitConnectionCall", json!({ "after": after, "instance": me })).await {
            Ok(v) => {
                (quiet_failures, refusals) = (0, 0);
                if let Some(by) = v.get("superseded").filter(|s| s.is_object()) {
                    break Stop::Replaced {
                        pid: by.get("pid").and_then(Value::as_u64).map(|p| p as u32),
                        elevated: by.get("elevated").and_then(Value::as_bool),
                    };
                }
                if let Some(seq) = v.get("seq").and_then(Value::as_u64) {
                    after = after.max(seq);
                }
                let Some(event) = v.get("event").filter(|e| !e.is_null()).cloned() else {
                    // A quiet window. Park again.
                    continue;
                };
                let mut rt = runtime();
                tokio::spawn(async move {
                    let _slot = slot;
                    if handle_event(&mut rt, &event.to_string()).await {
                        println!("  · answered a call");
                    }
                });
            }
            // Refused three times running: not a blip during a deploy but a
            // revoked pairing, and nothing on this machine can fix that.
            Err(e) if revoked(&e) && refusals >= 2 => break Stop::Revoked(e),
            Err(e) => {
                // The api being unreachable is temporary — keep trying with a
                // backoff rather than exiting and leaving the connection dead.
                if revoked(&e) {
                    refusals += 1;
                }
                quiet_failures = quiet_failures.saturating_add(1);
                if quiet_failures <= 3 || quiet_failures % 30 == 0 {
                    eprintln!("  waiting on {base} failed ({e}) — retrying");
                }
                tokio::time::sleep(std::time::Duration::from_secs(
                    5u64.saturating_mul(quiet_failures.min(6) as u64),
                ))
                .await;
            }
        }
    };

    // Whatever ends it, the calls already running finish and are answered:
    // their callers are parked on them.
    let _ = slots.acquire_many(IN_FLIGHT as u32).await;
    match outcome {
        Stop::Replaced { pid, elevated } => {
            let who = pid.map_or_else(|| "a newer `mafold pair`".to_string(), |p| format!("PID {p}"));
            let rights = match elevated {
                Some(true) => " (with admin rights)",
                Some(false) => " (without admin rights)",
                None => "",
            };
            println!("  {who}{rights} serves this pairing now — this one stops.");
            Ok(())
        }
        Stop::Revoked(e) => Err(anyhow!(
            "the api refuses this machine's pairing ({e}) — it was revoked on the account. \
             `mafold pair --forget`, then `mafold pair` to be lent again"
        )),
    }
}

/// Why a serving process stops.
enum Stop {
    /// A process started later holds the pairing now.
    Replaced { pid: Option<u32>, elevated: Option<bool> },
    /// The api refuses the token for good.
    Revoked(anyhow::Error),
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Enough api for `serve` to run against: one row, a claim that always
    /// wins, and a `waitConnectionCall` that hands out `calls` in order (with
    /// a `seq` each), then `then` (a quiet window unless a test says
    /// otherwise). Records each answer's result as it lands, and every park
    /// body the machine sent.
    async fn stub(
        row: Value,
        calls: Vec<Value>,
        then: Value,
    ) -> (String, Arc<Mutex<Vec<Value>>>, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let answers = Arc::new(Mutex::new(Vec::new()));
        let parks = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(calls.into_iter().enumerate().collect::<Vec<_>>()));
        let (a, p) = (answers.clone(), parks.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (answers, parks, queue, row, then) = (a.clone(), p.clone(), queue.clone(), row.clone(), then.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (path, body) = loop {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        let Some(end) = text.find("\r\n\r\n") else { continue };
                        let want: usize = text[..end]
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if text.len() < end + 4 + want {
                            continue;
                        }
                        let path = text.lines().next().and_then(|l| l.split(' ').nth(1)).unwrap_or("");
                        break (path.to_string(), text[end + 4..].to_string());
                    };
                    let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let result = match path.as_str() {
                        "/api/listConnections" => json!({ "items": [row] }),
                        "/api/claimConnectionCall" => json!({ "claimed": true }),
                        "/api/answerConnectionCall" => {
                            answers.lock().unwrap().push(body["result"].clone());
                            Value::Null
                        }
                        "/api/waitConnectionCall" => {
                            parks.lock().unwrap().push(body.clone());
                            let next = {
                                let mut q = queue.lock().unwrap();
                                (!q.is_empty()).then(|| q.remove(0))
                            };
                            match next {
                                Some((i, params)) => json!({
                                    "event": { "method": "events.connectionCall", "params": params },
                                    "seq": i as u64 + 1,
                                }),
                                None => {
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    then
                                }
                            }
                        }
                        _ => Value::Null,
                    };
                    let reply = json!({ "ok": true, "result": result }).to_string();
                    let out = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), answers, parks)
    }

    /// A `computer` row bound to a fresh machine key, and that key.
    fn fixture() -> (mafold_core::vault::DeviceKeypair, Key, Value) {
        use mafold_core::mafold_types::connections::provider_infos;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        mafold_core::providers::install_unverified_for_tests(1, provider_infos(), now);

        let machine = mafold_core::vault::generate_device();
        let dek = Key::random();
        let row = json!({
            "name": "box",
            "provider": "computer",
            "label": "box",
            "blob": mafold_core::vault::seal(
                &dek,
                json!({ "device_id": machine.public, "machine": "box" }).to_string().as_bytes(),
            ),
            "wrapped_dek": "",
            "key_id": "k1",
        });
        (machine, dek, row)
    }

    fn call(id: &str, cmd: &str) -> Value {
        json!({
            "call_id": id,
            "connection": "box",
            "method": "shell.exec",
            "params": { "cmd": cmd, "cwd": "/tmp", "timeout_ms": 10_000 },
        })
    }

    fn stdout(result: &Value) -> String {
        result["stdout"].as_str().unwrap_or("").trim().to_string()
    }

    const QUIET: fn() -> Value = || json!({ "event": null });

    /// A lock file of the test's own — never the real `~/.mafold`.
    fn scratch_lock() -> PathBuf {
        std::env::temp_dir().join(format!("mafold-pair-test-{}/paired.lock", uuid::Uuid::new_v4()))
    }

    fn instance(id: &str, started_ms: i64) -> Instance {
        Instance { id: id.into(), started_ms, pid: std::process::id(), elevated: false }
    }

    /// A paired machine is listening only while it is parked. It used to run
    /// each call before parking again, so every call that arrived meanwhile
    /// went to nobody (2026-10-05, every second or third call on a busy
    /// borrowed laptop). Now a slow call runs on its own while the machine
    /// takes — and answers — the next one; and each park says where the
    /// machine is in the api's board.
    #[tokio::test]
    async fn a_slow_call_does_not_stop_the_machine_taking_the_next() {
        let (machine, dek, row) = fixture();
        let (base, answers, parks) = stub(
            row,
            vec![call("slow", "sleep 2 && echo slow"), call("fast", "echo fast")],
            QUIET(),
        )
        .await;

        let paired = Paired { connection: "box".into(), token: "tok".into(), dek: dek.to_b64() };
        let (me, lock) = (instance("a", 1), scratch_lock());
        let serving = tokio::spawn(async move { serve(&base, &machine.public, paired, dek, &me, &lock).await });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while answers.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        serving.abort();

        assert_eq!(
            answers.lock().unwrap().iter().map(stdout).collect::<Vec<_>>(),
            vec!["fast".to_string(), "slow".to_string()],
            "the quick call must not wait behind the slow one"
        );
        let parks = parks.lock().unwrap().clone();
        assert_eq!(parks[0]["after"], json!(0), "the first park starts at the board's beginning");
        assert_eq!(parks[1]["after"], json!(1), "and each later one says what it was last handed");
        assert_eq!(parks[2]["after"], json!(2));
    }

    /// The machine's half of the api's lease (`connections_call::Lease`):
    /// told that a `mafold pair` started later holds the pairing now, this
    /// process stops — after finishing what it was running — instead of
    /// parking beside it forever (linsky's ThinkBook, 2026-10-08: five of them
    /// at once, and the admin window kept losing the claim). And every answer
    /// it gave says which process ran it, at what privilege, so a caller
    /// seeing "Access is denied" can tell why.
    #[tokio::test]
    async fn a_replaced_machine_stops_and_its_answers_say_who_ran_them() {
        let (machine, dek, row) = fixture();
        let (base, answers, _parks) = stub(
            row,
            vec![call("one", "sleep 1 && echo one")],
            json!({ "event": null, "superseded": { "pid": 4242, "elevated": true, "started_ms": 1 } }),
        )
        .await;

        let paired = Paired { connection: "box".into(), token: "tok".into(), dek: dek.to_b64() };
        let (me, lock) = (instance("a", 1), scratch_lock());
        let serving = tokio::spawn(async move { serve(&base, &machine.public, paired, dek, &me, &lock).await });
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), serving).await;
        assert!(stopped.is_ok(), "a replaced machine must stop serving, not park forever");

        let answers = answers.lock().unwrap().clone();
        assert_eq!(answers.len(), 1, "the call it was running is finished and answered first: {answers:?}");
        assert_eq!(stdout(&answers[0]), "one");
        assert_eq!(
            answers[0]["served_by"]["pid"],
            json!(std::process::id()),
            "the answer names the process that ran it: {}",
            answers[0]
        );
        assert!(answers[0]["served_by"]["elevated"].is_boolean(), "and whether it ran elevated");
    }

    /// The same takeover on the machine itself, for when the api is out of
    /// reach or older than leases: a `mafold pair` started later writes the
    /// lock, and the one already serving sees it and stops. Every park says
    /// which process it is, which is what the api's lease reads.
    #[tokio::test]
    async fn a_later_pair_on_the_same_machine_takes_over_and_the_older_one_stops() {
        let (machine, dek, row) = fixture();
        let (base, _answers, parks) = stub(row, vec![], QUIET()).await;
        let lock = scratch_lock();
        let paired = || Paired { connection: "box".into(), token: "tok".into(), dek: dek.to_b64() };

        let older = tokio::spawn({
            let (base, key, paired, dek, lock) = (base.clone(), machine.public.clone(), paired(), dek.clone(), lock.clone());
            async move { serve(&base, &key, paired, dek, &instance("old-window", 1_000), &lock).await }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while parks.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(parks.lock().unwrap()[0]["instance"]["id"], "old-window", "a park says who is parking");

        let newer = tokio::spawn({
            let (base, key, paired, dek, lock) = (base.clone(), machine.public.clone(), paired(), dek.clone(), lock.clone());
            async move { serve(&base, &key, paired, dek, &instance("admin-window", 2_000), &lock).await }
        });
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(5), older).await;
        assert!(stopped.is_ok(), "the older process stops once a later one holds the machine's pairing");
        assert!(!newer.is_finished(), "and the later one keeps serving");
        assert_eq!(holder(&lock).map(|h| h.id), Some("admin-window".to_string()));
        newer.abort();
    }
}
