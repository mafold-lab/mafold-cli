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
    serve(base, &dev.public, paired, dek).await
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

/// Answer calls for the one connection this machine was paired for, forever.
///
/// The loop is thin on purpose: everything that decides whether to claim, what
/// to run and how to report it lives in `mafold_core::connections::handle_event`
/// — the same function a signed-in daemon and the browser run. A paired machine
/// is not a second implementation of answering a call; it is the same one
/// holding less.
async fn serve(base: &str, public_key: &str, paired: Paired, dek: Key) -> Result<()> {
    let mut rt = Runtime::for_row(
        &format!("{base}/api"),
        &paired.token,
        &paired.connection,
        dek,
    );
    // The id in the row's sealed payload is this machine's PUBLIC KEY: the very
    // thing its owner approved. `can_serve` compares the two before claiming,
    // so a call addressed to a different machine is declined here rather than
    // claimed and fumbled.
    rt.attach_computer(public_key, crate::computer::executor());

    let anon = Client::new(base.to_string(), paired.token.clone());
    println!("  Listening. Ctrl-C to stop; this machine answers nothing else.");
    let mut quiet_failures = 0u32;
    loop {
        match anon.call("waitConnectionCall", json!({})).await {
            Ok(v) => {
                quiet_failures = 0;
                let Some(event) = v.get("event").filter(|e| !e.is_null()) else {
                    // A quiet window. Park again immediately — the gap between
                    // two parks is the only moment a call can miss this
                    // machine, so it is kept as short as the round trip.
                    continue;
                };
                handle_event(&mut rt, &event.to_string()).await;
            }
            Err(e) => {
                // The api being unreachable is temporary; the token being
                // refused is not, but this machine cannot fix either, so it
                // says what happened and keeps trying with a backoff rather
                // than exiting and leaving the connection dead.
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
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}
