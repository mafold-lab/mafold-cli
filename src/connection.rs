//! `mafold connection` — your credentials at third parties, carried between
//! your own machines without ever being readable by Mafold.
//!
//! Link a provider once and every daemon you run can use it. The plaintext is
//! assembled here and nowhere else: the server sees a provider slug, a masked
//! label, and a blob. See `.docs/connections-v1.md`; the ciphers are in
//! `vault.rs`.
//!
//! The command surface follows the trust story rather than the CRUD:
//!
//!   list / add / show / rm      the credentials
//!   devices / approve / revoke  who is allowed to open them
//!   unlock / recovery           getting a key onto a machine
//!
//! Auth is the HUMAN session (`mafold login`), never a bot token — a daemon
//! running on your laptop must not be able to enumerate your credentials just
//! because it shares the filesystem.

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use crate::session;
use crate::vault::{self, DeviceKey, Key};
use mafold_core::mafold_types::connections::{provider_infos, ProviderInfo, ProviderKind};

/// The registry, from the cloud — there is no compiled-in copy to fall back on.
///
/// The cli reads the same signed pack every other surface does, which is what
/// makes "add a provider" a push rather than five releases. It also means the
/// cli can be OLDER than the registry and still link and call a provider it has
/// never heard of, as long as that provider needs no native driver.
async fn registry(client: &Client) -> Result<Vec<ProviderInfo>> {
    // `/api` is appended here for the same reason `Runtime::new` does it:
    // `Client::base` is the ORIGIN (`https://api.mafold.com`), while the core's
    // `net::rpc` takes a base that already includes the prefix. Passing the
    // origin straight through posts to `/getConnectionProviders` and gets a 404
    // that reads like "your server is too old" — which is what shipped in
    // cli@0.9.97 and is exactly the wrong thing to tell a user.
    mafold_core::providers::ensure(&format!("{}/api", client.base), &client.token, now_ms())
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(mafold_core::providers::all())
}

async fn descriptor(client: &Client, id: &str) -> Result<ProviderInfo> {
    registry(client)
        .await?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| anyhow!("no provider called `{id}` — see `mafold connection providers`"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Subcommand)]
pub enum ConnectionCmd {
    /// List your connections.
    List,
    /// Link a credential: `mafold connection add anthropic --provider anthropic-api`.
    Add {
        /// A short name you'll refer to it by (`anthropic`, `claude-max`).
        name: String,
        /// Provider id — see `mafold connection providers`.
        #[arg(long)]
        provider: String,
        /// Import from the local file the provider's own CLI wrote
        /// (Claude Code / Codex OAuth). Beats pasting a refresh token by hand.
        #[arg(long)]
        import: bool,
        /// Read the value from the provider's conventional environment variable.
        #[arg(long)]
        from_env: bool,
        /// Log in through the provider's own OAuth consent screen, right here:
        /// the browser opens, the redirect lands on this machine, and the fresh
        /// grant goes straight into the vault. Only for providers whose CLI
        /// client is a published public client (Codex).
        #[arg(long)]
        oauth: bool,
        /// The MCP server to link, for `--provider mcp` — any server this
        /// registry doesn't name yet (`https://mcp.stripe.com/`). It is probed
        /// first: a server with OAuth opens its consent screen here, one that
        /// needs no credential is linked as-is, and only one that wants a
        /// token asks for it.
        #[arg(long)]
        url: Option<String>,
        /// The header a pasted token rides in, when the server wants something
        /// other than `Authorization: Bearer …` (`X-Api-Key`). Only meaningful
        /// with `--url`, and only when the server turns out to want a token.
        #[arg(long)]
        auth_header: Option<String>,
        /// A human tag for the linked identity. Stored in CLEARTEXT so the list
        /// is readable; defaults to a masked tail of the secret.
        #[arg(long)]
        label: Option<String>,
    },
    /// Show one connection. Metadata only unless you ask for the secret.
    Show {
        name: String,
        /// Print the decrypted secret to stdout.
        #[arg(long)]
        reveal: bool,
    },
    /// Print `export VAR=…` lines for a connection, to feed a local tool.
    /// Refused in a bot's turn — there, what is printed is posted to the chat.
    Env { name: String },
    /// Run a command with connections' secrets in its environment and their
    /// values masked out of everything it prints:
    /// `mafold connection run notion -- sh -c 'curl -H "Authorization: Bearer $NOTION_TOKEN" …'`.
    Run {
        /// Connections to inject (each as its provider's variable, e.g. `NOTION_TOKEN`).
        #[arg(required = true)]
        names: Vec<String>,
        /// The command, after `--`. Quote it for `sh -c` when the command line
        /// itself needs the value — your own shell would expand `$VAR` first.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// What a connection can do — its methods, straight from the provider.
    Methods {
        name: String,
        /// Print each method's full JSON Schema rather than a one-line summary.
        #[arg(long)]
        schema: bool,
    },
    /// Call one method: `mafold connection call notion search --params '{"query":"roadmap"}'`.
    Call {
        name: String,
        /// Method name from `mafold connection methods <name>`.
        method: String,
        /// Arguments as a JSON object. Omit for methods that take none.
        #[arg(long, default_value = "{}")]
        params: String,
    },
    /// Forget a connection (Mafold's copy only — never the provider's).
    Rm { name: String },
    /// The providers this build knows how to hold a credential for.
    Providers,
    /// Devices allowed to open your connections.
    Devices,
    /// Approve a pending device, wrapping the master key for it.
    Approve {
        /// Device id from `mafold connection devices`.
        device_id: String,
        /// Skip the fingerprint confirmation. Only for scripted enrollment of a
        /// machine you already control.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a device, and re-key so it genuinely loses access.
    Revoke {
        device_id: String,
        /// Skip the re-key. The removed machine keeps whatever it already
        /// holds — only use this if it was never approved.
        #[arg(long)]
        no_rotate: bool,
    },
    /// Fetch this machine's wrapped master key once another device approves it.
    Unlock {
        /// One JSON line for a program (the desktop app):
        /// `{"state":"unlocked",…}`, `{"state":"waiting"}` (no device has handed
        /// this one the key yet — retry later), or `{"state":"error",…}`.
        #[arg(long)]
        json: bool,
    },
    /// Stay online and answer connection calls addressed to your devices.
    ///
    /// While this runs, bots you've granted a connection to get their calls
    /// executed HERE — the credential is opened on this machine and only the
    /// result leaves it. Close it and another of your devices (an open web
    /// client, your phone) takes over; none online means calls fail with a
    /// message saying so.
    Listen,
    /// Set the offline recovery passphrase (wraps the master key under it).
    SetRecovery,
    /// Recover the master key on a machine with no approved device.
    Recover,
}

// ── plumbing ───────────────────────────────────────────────────────────────

/// A connection command always speaks as the person, so it builds its own
/// client from the saved human session instead of the ambient bot token.
fn human_client(base: &str) -> Result<(Client, session::Session)> {
    let sess = session::load()
        .context("not logged in — run `mafold login` first (connections belong to your account)")?;
    Ok((
        Client::new(base.to_string(), sess.token.clone()),
        sess.clone(),
    ))
}

fn as_array(v: &Value, key: &str) -> Vec<Value> {
    v.get(key).and_then(|x| x.as_array()).cloned().unwrap_or_default()
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// `mafold connection call`/`env`/`show --reveal` all run on the HUMAN session
/// (`human_client`), because the vault only opens for its owner. But a
/// self-hosted bot's turn shells out as that very human — so without this,
/// every bot the owner runs (and therefore anyone who can drive that bot) could
/// use any connection and read raw tokens, with none of the `connection.use:`
/// grant the relayed `callConnection` path checks server-side. `MAFOLD_BOT_TOKEN`
/// in the environment is how a turn says "I am acting as a bot": when it is set,
/// the same grant is required here, and the denial reads exactly like the
/// relay's so the two paths can't be told apart by probing.
///
/// Pure, so the decision is testable without a network: `bot` is `None` for a
/// human at their own terminal (no gate), `Some(handle)` for a bot turn.
/// `grants` is the owner's `(grantee, connection, can_use)` rows. Returns the
/// denial message when the bot may not use `name`, else `None`.
fn connection_use_denied(
    bot: Option<&str>,
    owner: &str,
    name: &str,
    grants: &[(String, String, bool)],
) -> Option<String> {
    let bot = bot?; // no bot token → the person's own terminal, their own vault
    let granted = grants.iter().any(|(grantee, connection, can_use)| {
        *can_use && grantee.eq_ignore_ascii_case(bot) && connection.eq_ignore_ascii_case(name)
    });
    if granted {
        None
    } else {
        Some(format!(
            "@{owner} hasn't allowed @{bot} to use their `{name}` — \
             ask with requestConnectionAccess and let them approve the card"
        ))
    }
}

/// Enforce [`connection_use_denied`] against the live account, but only when a
/// bot token is present in the environment. Fails CLOSED: a bot token we can't
/// resolve, or grants we can't fetch, denies rather than falling through to the
/// vault — the whole point is that a bot turn is not implicitly the owner.
async fn require_connection_use(
    base: &str,
    human: &Client,
    owner: &str,
    name: &str,
) -> Result<()> {
    let Some(token) = std::env::var("MAFOLD_BOT_TOKEN").ok().filter(|t| !t.trim().is_empty())
    else {
        return Ok(()); // a human at a real terminal — the vault is theirs to open
    };
    let bot = match Client::new(base.to_string(), token).call("getMe", json!({})).await {
        Ok(v) => s(&v, "username"),
        Err(e) => bail!(
            "running as a bot but couldn't confirm which one ({e}); \
             refusing to touch @{owner}'s `{name}` without checking the grant"
        ),
    };
    if bot.is_empty() {
        bail!("running as a bot with no resolvable identity; refusing to touch @{owner}'s `{name}`");
    }
    let rows: Vec<(String, String, bool)> = human
        .call("listConnectionGrants", json!({}))
        .await
        .context("couldn't check connection grants; refusing rather than assuming access")?
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|g| {
            (
                s(g, "grantee"),
                s(g, "connection"),
                g.get("can_use").and_then(|b| b.as_bool()).unwrap_or(false),
            )
        })
        .collect();
    if let Some(msg) = connection_use_denied(Some(&bot), owner, name, &rows) {
        bail!(msg);
    }
    Ok(())
}

/// Whether this process is a bot's turn (the daemon sets `MAFOLD_BOT_TOKEN`
/// for every turn it runs) — the same test [`require_connection_use`] makes.
fn bot_turn() -> bool {
    std::env::var("MAFOLD_BOT_TOKEN").is_ok_and(|t| !t.trim().is_empty())
}

/// `env` and `show --reveal` print a connection's raw secret. At a person's
/// own terminal that is the point; in a bot's turn everything printed becomes
/// the trace card, posted to the conversation for everyone in it — which is
/// how `export NOTION_TOKEN=…` ended up in a group of two dozen (2026-10-05).
/// The grant check doesn't help: a bot that IS allowed to use the connection
/// still must not publish it. Refused before the vault is touched; `run` is
/// the way to hand the value to a command.
fn plaintext_refused(bot_turn: bool, command: &str, name: &str) -> Option<String> {
    bot_turn.then(|| {
        format!(
            "`mafold connection {command}` would print `{name}`'s secret, and in a bot's turn \
             everything printed is posted to the conversation. Use \
             `mafold connection run {name} -- <command>` instead: the secret goes into that \
             command's environment, and anything it prints comes back with the secret masked."
        )
    })
}

/// The `(VAR, value)` a connection is fed to a local tool as.
fn env_pair(spec: &ProviderInfo, fields: &serde_json::Map<String, Value>, name: &str) -> Result<(String, String)> {
    // A provider that names no variable but holds exactly one value — a secret
    // someone typed into a card — is fed as the CONNECTION's name: the bot
    // asked for `db_password`, it gets `$DB_PASSWORD`. Read off the row's
    // shape, not the provider's id (`.docs/secure-input-v1.md` §8).
    let var = match (&spec.env_var, spec.fields.len()) {
        (Some(v), _) => v.clone(),
        (None, 1) => name.to_uppercase().replace('-', "_"),
        (None, _) => bail!("{} is an OAuth bag, not a single env var — use `mafold connection call {name} …`", spec.id),
    };
    let primary = spec.fields.first().map(|f| f.key.as_str()).unwrap_or("api_key");
    let val = fields
        .get(primary)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("connection has no `{primary}`"))?;
    Ok((var, val.to_string()))
}

/// Mask everything but the last 4 — the same shape the api uses for bot
/// secrets, so a masked value looks the same wherever it appears.
fn mask_tail(v: &str) -> String {
    let n = v.chars().count();
    if n <= 4 {
        return "•".repeat(n);
    }
    let tail: String = v.chars().skip(n - 4).collect();
    format!("{}{}", "•".repeat((n - 4).min(8)), tail)
}

/// Register this machine's public key and report where it stands.
///
/// Called by every path that needs a key, because a device that was revoked or
/// re-installed must re-enrol rather than fail with a decrypt error.
async fn register(client: &Client, sess: &session::Session, dev: &DeviceKey) -> Result<Value> {
    let reg = client
        .call(
            "registerVaultDevice",
            json!({
                "device_id": sess.device_id,
                "device_name": sess.device_name,
                "public_key": dev.public,
                "fingerprint": vault::fingerprint(&dev.public),
            }),
        )
        .await
        .context("registerVaultDevice failed")?;

    // Every command comes through here, so this is where "any machine of mine
    // that holds the key hands it to the others" actually becomes true. Hooking
    // it to the unlock path alone was too narrow: `list` and `devices` never
    // unlock, so a laptop could sit next to a waiting phone all day and do
    // nothing about it — which is exactly the stuck feeling the ceremony was
    // removed to fix.
    if let Some((key, key_id)) = vault::cached_umk(dev) {
        if !key_id.is_empty() && s(&reg["device"], "key_id") == key_id {
            auto_approve_pending(client, &key, &key_id).await;
        }
    }
    Ok(reg)
}

/// The master key for this machine, or a clear explanation of what to do next.
///
/// Order matters: the local cache first (a daemon must not do a round trip per
/// read), then the wrap left by an approving device. There is deliberately no
/// third fallback that mints a fresh key — that would silently orphan every
/// existing connection instead of saying "this device isn't approved yet".
/// Hand the key to every other machine of yours that is waiting for it.
///
/// **Owner ruling, 2026-08-12: signing in to your own client IS the
/// authorization.** No prompt, no fingerprint comparison, no command to run.
/// Whichever of your devices holds the key gives it to the others the moment it
/// notices, and the whole enrolment ceremony disappears.
///
/// The property that survives is the one worth having: the server still only
/// ever relays a wrap it has no key for, so it cannot read a credential, and
/// neither can anyone who steals its database or a session token. What is given
/// up is the defence against the SERVER itself substituting a device public key
/// — an active, targeted attack by us, traded away because the ceremony that
/// prevented it charged every honest user a step they frequently could not
/// perform at all.
async fn auto_approve_pending(client: &Client, umk: &Key, key_id: &str) {
    let Ok(v) = client.call("listVaultDevices", json!({})).await else {
        return;
    };
    for d in as_array(&v, "items") {
        let has_key = d["has_key"].as_bool().unwrap_or(false);
        let approved = d["approved"].as_bool().unwrap_or(false);
        if approved && has_key {
            continue;
        }
        let (id, public) = (s(&d, "device_id"), s(&d, "public_key"));
        if id.is_empty() || public.is_empty() {
            continue;
        }
        let Ok(wrapped) = vault::wrap_key_for(&public, umk) else {
            continue;
        };
        // Best effort per device: one that fails is not a reason to abandon the
        // rest, and nothing here is worth interrupting the command the user
        // actually ran.
        let _ = client
            .call(
                "approveVaultDevice",
                json!({
                    "device_id": id,
                    "sealed_umk": wrapped,
                    "key_id": key_id,
                    "public_key": public,
                }),
            )
            .await;
    }
}

pub(crate) async fn unlock(client: &Client, sess: &session::Session) -> Result<(Key, String, DeviceKey)> {
    let dev = vault::device_key()?;
    let reg = register(client, sess, &dev).await?;

    if let Some((key, key_id)) = vault::cached_umk(&dev) {
        // Trust the cache only when the server still records THIS generation
        // wrapped for THIS device. An empty id is not a permissive "server
        // doesn't track that" — it means no wrap is on record, i.e. the device
        // was revoked. Treating that as trustworthy would let a revoked machine
        // keep working off its cache until something happened to re-key.
        // `register` above has already passed the key on to anything waiting.
        if !key_id.is_empty() && s(&reg["device"], "key_id") == key_id {
            return Ok((key, key_id, dev));
        }
        vault::forget_cached_umk();
    }

    if reg["first"].as_bool() == Some(true) {
        // Nobody can approve us because nobody holds a key yet: this account's
        // vault starts here.
        let umk = Key::random();
        let key_id = vault::new_key_id();
        let wrapped = vault::wrap_key_for(&dev.public, &umk)?;
        client
            .call(
                "approveVaultDevice",
                json!({
                    "device_id": sess.device_id,
                    "sealed_umk": wrapped,
                    "key_id": key_id,
                    "public_key": dev.public,
                }),
            )
            .await
            .context("approveVaultDevice (self) failed")?;
        vault::cache_umk(&umk, &dev, &key_id)?;
        println!("✓ vault created on {} ({})", sess.device_name, vault::fingerprint(&dev.public));
        println!("  set an offline recovery passphrase now:  mafold connection set-recovery");
        return Ok((umk, key_id, dev));
    }

    match client
        .call("getVaultKey", json!({ "device_id": sess.device_id }))
        .await
    {
        Ok(v) => {
            let wrapped = s(&v, "sealed_umk");
            let key_id = s(&v, "key_id");
            let umk = vault::unwrap_key(&dev.secret, &wrapped).map_err(|e| anyhow!("{e}"))?;
            vault::cache_umk(&umk, &dev, &key_id)?;
            auto_approve_pending(client, &umk, &key_id).await;
            Ok((umk, key_id, dev))
        }
        // Not "you forgot to approve it" — there is nothing to approve. Any
        // device of yours that is signed in hands this one the key by itself;
        // this message only appears when none of them has been online since
        // this machine registered, so the only accurate instruction is to open
        // Mafold somewhere and come back.
        Err(_) => Err(anyhow::Error::new(KeyNotHandedOver)),
    }
}

/// `unlock` found no key wrapped for this machine yet: a state to wait out,
/// not a failure — typed, so `connection unlock --json` can say "waiting".
#[derive(Debug)]
pub(crate) struct KeyNotHandedOver;

impl std::fmt::Display for KeyNotHandedOver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this machine doesn't have your vault key yet.\n\n  \
             open Mafold on a device that already has it (web, mac or iOS) and it \
             will hand the key over — then run this again.\n  \
             no other device? recover with your passphrase:  mafold connection recover",
        )
    }
}

impl std::error::Error for KeyNotHandedOver {}

/// Open a connection's payload.
///
/// `key_id` is checked first so that the common failure — this device holds a
/// retired master key — reports itself as such. Letting it fall through to the
/// AEAD would surface "wrong key or corrupt data", which reads like the
/// credential was damaged rather than that the caller needs to re-unlock.
fn open_payload(umk: &Key, key_id: &str, conn: &Value) -> Result<serde_json::Map<String, Value>> {
    // Secure input (`.docs/secure-input-v1.md` §7): sealed by someone holding
    // no master key, to each of the owner's devices. Only this device's own
    // wrap opens with its secret, so trying them all needs no device id.
    if s(conn, "wrapped_dek").is_empty() {
        let wraps = conn.get("device_wraps").and_then(|w| w.as_object());
        let dev = vault::device_key()?;
        let plain = wraps
            .into_iter()
            .flatten()
            .filter_map(|(_, w)| w.as_str())
            .find_map(|w| mafold_core::vault::open_for_device(&dev.secret, &s(conn, "blob"), w).ok())
            .ok_or_else(|| {
                anyhow!(
                    "`{}` was handed over to this account's devices before this one joined the vault — \
                     ask for it again (it's a card in the chat), this machine can't open the copy that exists",
                    s(conn, "name")
                )
            })?;
        return serde_json::from_str(&plain).context("connection payload is not JSON");
    }
    let want = s(conn, "key_id");
    if !want.is_empty() && !key_id.is_empty() && want != key_id {
        bail!(
            "`{}` was re-keyed (it needs master key {want}, this device holds {key_id}).\n  \
             run `mafold connection unlock` — or, if this device was revoked, re-approve it \
             from one that wasn't.",
            s(conn, "name")
        );
    }
    // The DEK dance lives in the shared core, so the cli and the browser seal
    // and open the same way by construction rather than by review.
    let plain = vault::open_payload(umk, &s(conn, "blob"), &s(conn, "wrapped_dek"))
        .map_err(|e| anyhow!("{e}"))?;
    serde_json::from_str(&plain).context("connection payload is not JSON")
}

/// Seal a payload under a fresh DEK wrapped by the master key.
pub(crate) fn seal_payload(umk: &Key, fields: &serde_json::Map<String, Value>) -> Result<(String, String)> {
    let sealed = vault::seal_payload(umk, &serde_json::to_string(fields)?);
    Ok((sealed.blob, sealed.wrapped_dek))
}

async fn fetch(client: &Client, name: &str) -> Result<Value> {
    let v = client.call("listConnections", json!({})).await?;
    as_array(&v, "items")
        .into_iter()
        .find(|c| s(c, "name") == name)
        .ok_or_else(|| anyhow!("no connection named `{name}` — see `mafold connection list`"))
}

// ── commands ───────────────────────────────────────────────────────────────

pub async fn run(base: &str, cmd: ConnectionCmd) -> Result<()> {
    let (client, sess) = human_client(base)?;
    match cmd {
        ConnectionCmd::Providers => providers(&client).await,
        ConnectionCmd::List => list(&client).await,
        ConnectionCmd::Add { name, provider, import, from_env, oauth, url, auth_header, label } => {
            add(&client, &sess, &name, &provider, import, from_env, oauth, url, auth_header, label)
                .await
        }
        ConnectionCmd::Show { name, reveal } => show(base, &client, &sess, &name, reveal).await,
        ConnectionCmd::Env { name } => env(base, &client, &sess, &name).await,
        ConnectionCmd::Run { names, command } => {
            run_with(base, &client, &sess, &names, &command).await
        }
        ConnectionCmd::Methods { name, schema } => {
            methods(base, &client, &sess, &name, schema).await
        }
        ConnectionCmd::Call { name, method, params } => {
            call(base, &client, &sess, &name, &method, &params).await
        }
        ConnectionCmd::Rm { name } => rm(&client, &name).await,
        ConnectionCmd::Devices => devices(&client, &sess).await,
        ConnectionCmd::Approve { device_id, yes } => approve(&client, &sess, &device_id, yes).await,
        ConnectionCmd::Revoke { device_id, no_rotate } => {
            revoke(&client, &sess, &device_id, no_rotate).await
        }
        ConnectionCmd::Listen => listen(base, &client, &sess).await,
        ConnectionCmd::Unlock { json: false } => {
            let (_, key_id, dev) = unlock(&client, &sess).await?;
            println!(
                "✓ unlocked on {} — key {} · fingerprint {}",
                sess.device_name,
                key_id,
                vault::fingerprint(&dev.public)
            );
            Ok(())
        }
        ConnectionCmd::Unlock { json: true } => {
            let out = match unlock(&client, &sess).await {
                Ok((_, key_id, dev)) => json!({ "state": "unlocked", "key_id": key_id, "fingerprint": vault::fingerprint(&dev.public) }),
                Err(e) if e.downcast_ref::<KeyNotHandedOver>().is_some() => json!({ "state": "waiting" }),
                Err(e) => json!({ "state": "error", "message": format!("{e:#}") }),
            };
            println!("{out}");
            if out["state"] == "error" {
                anyhow::bail!("unlock failed");
            }
            Ok(())
        }
        ConnectionCmd::SetRecovery => set_recovery(&client, &sess).await,
        ConnectionCmd::Recover => recover(&client, &sess).await,
    }
}

async fn providers(client: &Client) -> Result<()> {
    let rows = registry(client).await?;
    println!("{:<20} {:<26} {:<8} {}", "ID", "PROVIDER", "AUTH", "LINK VIA");
    for p in &rows {
        let how = match (p.import_path.as_deref(), p.env_var.as_deref()) {
            // Nothing to collect: `add` writes this machine's own binding.
            _ if is_device_binding(p) => "run it on that machine".to_string(),
            (Some(path), _) => format!("--import  (~/{path})"),
            (None, Some(v)) => format!("--from-env  (${v})"),
            // A consent screen the browser runs is the modern default, and it
            // is not "paste" — saying so sent people looking for a token page
            // that no longer exists for that provider.
            (None, None) if p.oauth => "sign in (browser)".to_string(),
            // The server is named by the user; what it wants is found out by
            // asking it, so no single word here would be true of all of them.
            (None, None) if p.delegates_endpoint() => "--url <server>  (probed)".to_string(),
            (None, None) => "paste".to_string(),
        };
        // Printed from what the row IS, not from `kind`: a machine binding
        // carries `ApiKey` for wire-compatibility reasons that have nothing to
        // do with the human reading this table (see the `computer` row).
        let kind = if is_device_binding(p) {
            "device"
        } else {
            match p.kind {
                ProviderKind::OAuth => "oauth",
                ProviderKind::ApiKey => "key",
            }
        };
        println!("{:<20} {:<26} {:<8} {}", p.id, p.display, kind, how);
    }
    Ok(())
}

async fn list(client: &Client) -> Result<()> {
    let v = client.call("listConnections", json!({})).await?;
    let items = as_array(&v, "items");
    if items.is_empty() {
        println!("(no connections)\n\n  link one:  mafold connection add <name> --provider <id>");
        println!("  providers: mafold connection providers");
        return Ok(());
    }
    println!("{:<16} {:<20} {:<10} {}", "NAME", "PROVIDER", "STATUS", "LABEL");
    let mut dead = Vec::new();
    for c in &items {
        // The provider is printed VERBATIM, because that is exactly what the
        // server asserted by returning it. The status used to say `unknown`
        // whenever this binary's compiled-in table had no such id — which
        // described the CLI's build, not the connection. It says something
        // other than `linked` only when a device has LEARNED otherwise: a
        // renewal the provider refused (`relink_at`).
        let expired = c.get("relink_at").and_then(Value::as_i64).is_some();
        if expired {
            dead.push((s(c, "name"), s(c, "relink_reason")));
        }
        // A value someone handed over through a card says who and until when —
        // and how a bot spends it, since its variable is the row's own name.
        let handed = s(c, "provided_by");
        let note = if handed.is_empty() {
            String::new()
        } else {
            let until = c
                .get("expires_at")
                .and_then(Value::as_i64)
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| format!(" until {}", t.format("%Y-%m-%d %H:%M UTC")))
                .unwrap_or_default();
            let name = s(c, "name");
            format!(
                "  from @{handed}{until} · use: mafold connection run {name} -- <cmd> (as ${})",
                name.to_uppercase().replace('-', "_")
            )
        };
        println!(
            "{:<16} {:<20} {:<10} {}{}",
            s(c, "name"),
            s(c, "provider"),
            if expired { "expired" } else { "linked" },
            s(c, "label"),
            note
        );
    }
    for (name, why) in dead {
        let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
        println!("\n  {name}: the provider no longer accepts this sign-in{why} — reconnect it in Settings ▸ Connections");
    }
    Ok(())
}

/// Collect a provider's fields, by import, environment, or prompt.
fn collect(spec: &ProviderInfo, import: bool, from_env: bool)
    -> Result<serde_json::Map<String, Value>>
{
    let mut out = serde_json::Map::new();

    if import {
        let path = spec
            .import_path
            .as_deref()
            .ok_or_else(|| anyhow!("{} has no local credential file to import", spec.id))?;
        let full = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(path);
        let raw = std::fs::read_to_string(&full)
            .with_context(|| format!("read {} — log in with that tool first", full.display()))?;
        let parsed: Value = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not JSON", full.display()))?;
        // Vendors nest their bag differently; search rather than hard-code a
        // path per vendor, so a layout change costs nothing here.
        for key in &spec.payload_keys {
            if let Some(v) = find_key(&parsed, key) {
                out.insert(key.to_string(), v);
            }
        }
        if out.is_empty() {
            bail!("found no recognizable fields in {}", full.display());
        }
    } else if from_env {
        let var = spec
            .env_var
            .as_deref()
            .ok_or_else(|| anyhow!("{} has no conventional environment variable", spec.id))?;
        let val = std::env::var(var)
            .with_context(|| format!("${var} is not set"))?;
        let first = spec.fields.first().ok_or_else(|| anyhow!("provider has no fields"))?;
        out.insert(first.key.to_string(), Value::String(val));
    } else {
        for f in &spec.fields {
            let label = if f.required {
                format!("{}: ", f.label)
            } else {
                format!("{} (optional, blank to skip): ", f.label)
            };
            let val = crate::prompt_password(&label);
            if !val.is_empty() {
                out.insert(f.key.to_string(), Value::String(val));
            }
        }
    }

    for f in &spec.fields {
        if f.required && !out.contains_key(&f.key) {
            bail!("{} is required for {}", f.label, spec.id);
        }
    }
    Ok(out)
}

/// Depth-first search for a key anywhere in a vendor's JSON.
fn find_key(v: &Value, key: &str) -> Option<Value> {
    match v {
        Value::Object(m) => {
            let camel = key.split('_').enumerate().map(|(i, part)| {
                if i == 0 { part.to_string() } else {
                    let mut chars = part.chars();
                    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
                }
            }).collect::<String>();
            if let Some(found) = m.get(key).or_else(|| m.get(&camel)) {
                if !found.is_null() && !found.is_object() && !found.is_array() {
                    return Some(found.clone());
                }
            }
            m.values().find_map(|x| find_key(x, key))
        }
        Value::Array(a) => a.iter().find_map(|x| find_key(x, key)),
        _ => None,
    }
}

// ── the vendor-client OAuth dance (`add --oauth`) ──────────────────────────
//
// For providers whose OAuth client is a PUBLISHED PUBLIC client of the
// vendor's own CLI (`ProviderInfo::oauth_fixed`), we can mint a fresh grant
// instead of importing a file: PKCE, a localhost listener on the vendor's
// registered redirect, and a form-encoded code exchange. The whole dance runs
// on this machine — the registered redirect URI makes any server-side variant
// impossible, which is not a limitation but the property that keeps the vault
// honest: the token is born on a device the user controls and sealed there.
//
// A fresh grant also races NOTHING: `--import` shares a refresh token with the
// vendor's own CLI, and two holders spending one rotating token is the classic
// way an imported credential goes stale under it.

fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Query-string percent-encoding (RFC 3986 unreserved survive).
fn q_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The claims segment of a JWT, unverified.
///
/// Unverified is CORRECT here, not lazy: these tokens arrive over TLS from the
/// vendor's own token endpoint (or its CLI's credential file), and the values
/// lifted out of them — account id, expiry, an email for the label — are hints
/// for our own bookkeeping, not authorization inputs. The party that must
/// trust the signature is the vendor's API, and it verifies for itself.
fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Fill the ISSUED fields a link flow can't collect from a human: the
/// registry's fixed OAuth client (what lets a *different* device refresh this
/// grant), the account id buried in the id_token, and an expiry read from the
/// access token itself when nothing recorded one. Import files vary; this makes
/// an OAuth payload renewal-complete regardless of which flow produced it.
///
/// `token_endpoint` is copied from the registry rather than from the vendor's
/// metadata on purpose: for a brokered provider it is mafold-api, not the
/// vendor, and a daemon renewing months later must post where the secret is —
/// which only the registry knows.
fn enrich_oauth_payload(
    spec: &ProviderInfo,
    fields: &mut serde_json::Map<String, Value>,
) {
    let missing = |m: &serde_json::Map<String, Value>, k: &str| {
        m.get(k).and_then(|v| v.as_str()).map(str::trim).unwrap_or("").is_empty()
    };
    if let Some(oc) = &spec.oauth_fixed {
        if missing(fields, "client_id") {
            fields.insert("client_id".into(), Value::String(oc.client_id.clone()));
        }
        if missing(fields, "token_endpoint") {
            fields.insert("token_endpoint".into(), Value::String(oc.token_endpoint.clone()));
        }
    }
    if missing(fields, "account_id") {
        if let Some(claims) = fields.get("id_token").and_then(Value::as_str).and_then(jwt_claims) {
            if let Some(acc) = claims
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                fields.insert("account_id".into(), Value::String(acc.into()));
            }
        }
    }
    if missing(fields, "expires_at") {
        if let Some(exp) = fields
            .get("access_token")
            .and_then(Value::as_str)
            .and_then(jwt_claims)
            .and_then(|c| c.get("exp").and_then(Value::as_i64))
        {
            fields.insert("expires_at".into(), Value::String((exp * 1000).to_string()));
        }
    }
}

/// Serve the redirect: accept connections until the callback with our `state`
/// arrives, answer it with a small "done" page, and hand back the code.
async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    redirect_path: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut sock, _) = listener.accept().await.context("accept on the redirect port")?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
        let (route, query) = match path.split_once('?') {
            Some((r, q)) => (r, q),
            None => (path, ""),
        };
        // Browsers also ask for favicons and the like; only the registered
        // callback path ends the wait.
        if route != redirect_path {
            let _ = sock
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
            continue;
        }
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = v.replace('+', " ");
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                "error" | "error_description" if error.is_none() => error = Some(v),
                _ => {}
            }
        }
        let ok = error.is_none() && code.is_some() && state.as_deref() == Some(expected_state);
        // Neutral about WHERE the flow was started: the same listener serves
        // `connection add --oauth` (a terminal) and a Connect button in the
        // web pane, and telling a person who clicked a button to "return to
        // the terminal" is the exact seam this feature exists to remove.
        let page = if ok {
            "<html><body style=\"font-family:system-ui;padding:2rem\"><h2>Linked ✓</h2>\
             <p>You can close this tab — Mafold has the rest.</p></body></html>"
        } else {
            "<html><body style=\"font-family:system-ui;padding:2rem\"><h2>That didn't work</h2>\
             <p>You can close this tab — Mafold will say what went wrong.</p></body></html>"
        };
        let _ = sock
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                )
                .as_bytes(),
            )
            .await;
        if let Some(e) = error {
            bail!("the provider refused the login: {e}");
        }
        if state.as_deref() != Some(expected_state) {
            bail!("state mismatch on the OAuth callback — refusing a code this flow didn't ask for");
        }
        return Ok(code.expect("checked above"));
    }
}

/// The first leg of the dance, once the port is ours.
///
/// Split from the second leg because the two halves have different audiences:
/// the terminal opens `authorize_url` itself, while a link started from another
/// surface (`events.connectionLink`) hands it back so the browser the person is
/// actually looking at opens it. Everything that must survive between them —
/// the bound listener above all — travels in here rather than in a global, so
/// two flows can never share a port by accident.
pub(crate) struct OauthLeg {
    listener: tokio::net::TcpListener,
    verifier: String,
    state: String,
    redirect: url_parts::Parts,
    pub(crate) authorize_url: String,
    pub(crate) port: u16,
}

/// The public half of the OAuth client a dance runs as.
///
/// Two ways one comes to exist, and the dance must not care which: the
/// registry's FIXED client (Codex — a vendor CLI's published constants) or a
/// client this machine REGISTERED a moment ago (an MCP server the user named,
/// RFC 7591). Everything after "we have a client id" is the same PKCE, the
/// same loopback listener, the same exchange — so it is one struct and one
/// pair of legs, not two flows that agree today and drift tomorrow.
pub(crate) struct OauthClient {
    pub client_id: String,
    pub authorize_url: String,
    pub token_endpoint: String,
    pub redirect_uri: String,
    pub scopes: String,
    pub extra_params: Vec<(String, String)>,
    /// RFC 8707: the server the token is FOR. Sent on both legs when set, so
    /// an authorization server that guards several resources scopes the token
    /// to this one rather than to its default. Dynamic clients always set it;
    /// a fixed vendor client never does (its scope is implied by the client).
    pub resource: Option<String>,
}

impl From<&mafold_core::mafold_types::connections::FixedClientInfo> for OauthClient {
    fn from(oc: &mafold_core::mafold_types::connections::FixedClientInfo) -> Self {
        Self {
            client_id: oc.client_id.clone(),
            authorize_url: oc.authorize_url.clone(),
            token_endpoint: oc.token_endpoint.clone(),
            redirect_uri: oc.redirect_uri.clone(),
            scopes: oc.scopes.clone(),
            extra_params: oc.extra_params.clone(),
            resource: None,
        }
    }
}

/// Bind the vendor's registered redirect and build its consent URL.
async fn oauth_begin(spec: &ProviderInfo) -> Result<OauthLeg> {
    let oc: OauthClient = spec
        .oauth_fixed
        .as_ref()
        .ok_or_else(|| {
            anyhow!(
                "{} has no OAuth client this cli can drive — link it with --import or --from-env",
                spec.id
            )
        })?
        .into();

    // Bind BEFORE the browser opens: if the port is taken (the vendor's own
    // CLI mid-login, an earlier attempt wedged), fail now with a sentence —
    // not after the user has clicked through a consent screen whose redirect
    // will land on the wrong listener.
    let redirect: url_parts::Parts = url_parts::split(&oc.redirect_uri)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", redirect.port))
        .await
        .with_context(|| {
            format!(
                "can't listen on 127.0.0.1:{} — is another {} login already running?",
                redirect.port, spec.display
            )
        })?;
    oauth_leg(&oc, listener)
}

/// The first leg for an already-bound listener: PKCE, state, the consent URL.
///
/// Takes the listener rather than binding one so a caller that had to know
/// its port BEFORE it had a client (dynamic registration puts the redirect
/// URI in the registration request) can bind first and register second.
pub(crate) fn oauth_leg(oc: &OauthClient, listener: tokio::net::TcpListener) -> Result<OauthLeg> {
    use sha2::{Digest, Sha256};
    // PKCE. The verifier is HEX (the shape the vendor's own CLI sends), the
    // challenge standard base64url-nopad S256.
    let verifier = format!("{}{}", hex_bytes(&Key::random().0), hex_bytes(&Key::random().0));
    let challenge = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    };
    let state = hex_bytes(&Key::random().0);
    let redirect: url_parts::Parts = url_parts::split(&oc.redirect_uri)?;

    let mut auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        oc.authorize_url,
        q_encode(&oc.client_id),
        q_encode(&oc.redirect_uri),
        state,
        challenge,
    );
    // An empty scope is NOT sent as `scope=`: a dynamically registered client
    // asks for the server's default, which is what the browser flow does too,
    // and an explicit empty scope is refused by some authorization servers.
    if !oc.scopes.is_empty() {
        auth_url.push_str(&format!("&scope={}", q_encode(&oc.scopes)));
    }
    if let Some(r) = &oc.resource {
        auth_url.push_str(&format!("&resource={}", q_encode(r)));
    }
    for (k, v) in &oc.extra_params {
        auth_url.push('&');
        auth_url.push_str(&format!("{}={}", q_encode(k), q_encode(v)));
    }

    let port = redirect.port;
    Ok(OauthLeg {
        listener,
        verifier,
        state,
        redirect,
        authorize_url: auth_url,
        port,
    })
}

/// The second leg: wait for the vendor to come back to our port, then trade the
/// code for tokens. Returns the token bag plus a suggested cleartext label
/// (email · plan) read from the id_token.
async fn oauth_finish(
    spec: &ProviderInfo,
    leg: OauthLeg,
) -> Result<(serde_json::Map<String, Value>, Option<String>)> {
    let oc: OauthClient = spec
        .oauth_fixed
        .as_ref()
        .ok_or_else(|| anyhow!("{} has no OAuth client this cli can drive", spec.id))?
        .into();
    oauth_exchange(&oc, leg).await
}

/// The second leg for any client: wait, then exchange. Carries `client_id`
/// and `token_endpoint` into the bag so a DIFFERENT device can renew the
/// grant later — for a dynamically registered client those two values exist
/// nowhere else in the world.
pub(crate) async fn oauth_exchange(
    oc: &OauthClient,
    leg: OauthLeg,
) -> Result<(serde_json::Map<String, Value>, Option<String>)> {
    let OauthLeg { listener, verifier, state, redirect, .. } = leg;

    let code = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        wait_for_callback(listener, &state, &redirect.path),
    )
    .await
    .map_err(|_| anyhow!("no sign-in came back within 5 minutes — start it again to retry"))??;

    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("client_id", oc.client_id.clone()),
        ("code", code),
        ("redirect_uri", oc.redirect_uri.clone()),
        ("code_verifier", verifier),
    ];
    if let Some(r) = &oc.resource {
        form.push(("resource", r.clone()));
    }
    let resp = reqwest::Client::new()
        .post(&oc.token_endpoint)
        .form(&form)
        .send()
        .await
        .context("token exchange failed to send")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token exchange answered HTTP {status}: {}", body.chars().take(300).collect::<String>());
    }
    let grant: Value = serde_json::from_str(&body).context("token endpoint returned non-JSON")?;
    let take = |k: &str| grant.get(k).and_then(Value::as_str).unwrap_or("").to_string();

    let mut fields = serde_json::Map::new();
    let access = take("access_token");
    if access.is_empty() {
        bail!("token endpoint returned no access token");
    }
    fields.insert("access_token".into(), Value::String(access));
    for k in ["refresh_token", "id_token"] {
        let v = take(k);
        if !v.is_empty() {
            fields.insert(k.to_string(), Value::String(v));
        }
    }
    if let Some(secs) = grant.get("expires_in").and_then(Value::as_i64) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        fields.insert("expires_at".into(), Value::String((now_ms + secs * 1000).to_string()));
    }
    // What a renewal needs, from the client that just obtained the grant.
    // `enrich_oauth_payload` fills these for a FIXED client from the registry;
    // a dynamic client is not in any registry, so they are recorded here.
    fields.insert("client_id".into(), Value::String(oc.client_id.clone()));
    fields.insert("token_endpoint".into(), Value::String(oc.token_endpoint.clone()));

    // Label: the human identity of the grant, from the id_token.
    let label = fields
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims)
        .map(|c| {
            let email = c.get("email").and_then(Value::as_str).unwrap_or("").to_string();
            let plan = c
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match (email.is_empty(), plan.is_empty()) {
                (false, false) => format!("{email} · {plan}"),
                (false, true) => email,
                _ => String::new(),
            }
        })
        .filter(|s| !s.is_empty());

    Ok((fields, label))
}

/// Both legs, driven from a terminal: bind, open the browser here, wait.
async fn oauth_dance(
    spec: &ProviderInfo,
) -> Result<(serde_json::Map<String, Value>, Option<String>)> {
    let leg = oauth_begin(&spec).await?;
    println!("Opening {}'s consent screen…", spec.display);
    if !crate::platform::open_browser(&leg.authorize_url) {
        println!(
            "  couldn't open a browser — visit this URL yourself:\n\n  {}\n",
            leg.authorize_url
        );
    }
    println!("  waiting for the login to come back to 127.0.0.1:{}…", leg.port);
    oauth_finish(spec, leg).await
}

/// The two pieces of a redirect URI this flow needs. A module rather than a
/// dependency: pulling a URL crate into the cli for one host:port/path split
/// would be the heavier tool.
mod url_parts {
    use anyhow::{anyhow, Result};

    pub struct Parts {
        pub port: u16,
        /// Owned. It used to borrow a `&'static str`, which worked only while
        /// the redirect came from a compiled-in const — the registry is served
        /// now, so the string outlives nothing on its own.
        pub path: String,
    }

    pub fn split(uri: &str) -> Result<Parts> {
        let rest = uri
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("redirect URI must be http://localhost-style: {uri}"))?;
        let (host_port, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let port = host_port
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| anyhow!("redirect URI has no port: {uri}"))?;
        Ok(Parts { port, path: path.to_string() })
    }
}

#[allow(clippy::too_many_arguments)]
async fn add(
    client: &Client,
    sess: &session::Session,
    name: &str,
    provider: &str,
    import: bool,
    from_env: bool,
    oauth: bool,
    url: Option<String>,
    auth_header: Option<String>,
    label: Option<String>,
) -> Result<()> {
    let spec = &descriptor(client, provider).await?;
    if is_device_binding(spec) {
        return bind_machine(client, sess, name, spec, label).await;
    }
    // A server the USER names: the address goes into the sealed payload, and
    // what to collect is decided by asking the server, not by this table.
    if spec.delegates_endpoint() {
        let endpoint = url.ok_or_else(|| {
            anyhow!(
                "{} links a server you name — pass it:  mafold connection add {name} \
                 --provider {} --url https://…",
                spec.display, spec.id
            )
        })?;
        return crate::mcp_link::add_server(client, sess, name, spec, &endpoint, auth_header, label)
            .await;
    }
    if url.is_some() || auth_header.is_some() {
        bail!(
            "--url / --auth-header are for `--provider mcp` — {} knows its own server",
            spec.display
        );
    }
    let (mut fields, suggested_label) = if oauth {
        oauth_dance(spec).await?
    } else {
        (collect(spec, import, from_env)?, None)
    };
    // Issued fields the flows above can't know by themselves: the registry's
    // fixed OAuth client (so ANY device can refresh later) and an expiry read
    // out of the token itself when the vendor's file didn't record one.
    enrich_oauth_payload(&spec, &mut fields);
    let fields = fields;
    let label = label.or(suggested_label);
    let (umk, key_id, _) = unlock(client, sess).await?;
    let (blob, wrapped_dek) = seal_payload(&umk, &fields)?;

    // The label is the one thing we hand over in the clear, so derive it from
    // the least sensitive part available and say what it is.
    let label = label.unwrap_or_else(|| {
        fields
            .get("account")
            .or_else(|| fields.get("account_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let primary = spec.fields.first().map(|f| f.key.as_str()).unwrap_or("api_key");
                fields
                    .get(primary)
                    .and_then(|v| v.as_str())
                    .map(mask_tail)
                    .unwrap_or_default()
            })
    });

    client
        .call(
            "putConnection",
            json!({
                "name": name,
                "provider": spec.id,
                "label": label,
                "blob": blob,
                "wrapped_dek": wrapped_dek,
                "key_id": key_id,
            }),
        )
        .await
        .context("putConnection failed")?;
    println!("✓ linked {name} → {} ({label})", spec.display);
    println!("  the server stored ciphertext it cannot open; only your enrolled devices can.");
    if spec.mcp_url.is_some() {
        println!("  its methods:  mafold connection methods {name}");
    }
    Ok(())
}

/// Hand the call to the server, which fans it out to the caller's own devices
/// so the one that can run it does.
///
/// The same route a granted bot uses (`callConnection`) — a person calling
/// their own laptop from their desktop is not a different feature, and giving
/// it a second path is how the two would drift on exactly the details (claim,
/// timeout, error wording) that make it work.
async fn relay(client: &Client, name: &str, method: &str, params: &Value) -> Result<Value> {
    let v = client
        .call(
            "callConnection",
            json!({ "connection": name, "method": method, "params": params }),
        )
        .await
        .with_context(|| format!("`{name}` did not answer"))?;
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Is this provider a MACHINE rather than an account somewhere else?
///
/// Read off the served registry, not off the provider's name: a row with
/// nothing for a human to type (`fields` empty — every field is issued) that
/// still needs a device to finish (`device_link`) can only be a binding. The
/// day a second such provider ships, this keeps working; a `== "computer"`
/// here would not, and would be the §9 shape the whole registry exists to
/// avoid.
fn is_device_binding(spec: &ProviderInfo) -> bool {
    spec.fields.is_empty() && spec.device_link && spec.oauth_fixed.is_none()
}

/// Point a connection at THIS machine.
///
/// There is no credential to collect, no consent screen, and nothing to paste
/// — the whole act is writing down which device answers, and sealing it so the
/// server cannot read (or choose) that. Which is why it can only be run on the
/// machine itself: a laptop cannot bind a desktop, because it does not hold the
/// desktop's shell.
async fn bind_machine(
    client: &Client,
    sess: &session::Session,
    name: &str,
    spec: &ProviderInfo,
    label: Option<String>,
) -> Result<()> {
    let (umk, key_id, _) = unlock(client, sess).await?;
    let fields = machine_binding(sess);
    let (blob, wrapped_dek) = seal_payload(&umk, &fields)?;
    let label = label.unwrap_or_else(|| sess.device_name.clone());
    client
        .call(
            "putConnection",
            json!({
                "name": name,
                "provider": spec.id,
                "label": label,
                "blob": blob,
                "wrapped_dek": wrapped_dek,
                "key_id": key_id,
            }),
        )
        .await
        .context("putConnection failed")?;
    println!("✓ {name} → {} ({label})", spec.display);
    println!("  calls to it run HERE, and only here — the binding is sealed, so nothing");
    println!("  server-side chooses which of your machines answers.");
    println!("  keep it answering:  mafold up   (or `mafold connection listen`)");
    println!("  what it can do:     mafold connection methods {name}");
    Ok(())
}

/// The sealed payload of a `computer` row. Keys match the registry's
/// `COMPUTER_BINDING`, or `filter_payload` would drop them the first time
/// anything rewrote the row.
fn machine_binding(sess: &session::Session) -> serde_json::Map<String, Value> {
    let mut fields = serde_json::Map::new();
    fields.insert("device_id".into(), json!(sess.device_id));
    fields.insert("machine".into(), json!(sess.device_name));
    fields.insert("os".into(), json!(std::env::consts::OS));
    fields.insert("bound_at".into(), json!(now_ms().to_string()));
    fields
}

async fn show(base: &str, client: &Client, sess: &session::Session, name: &str, reveal: bool) -> Result<()> {
    let conn = fetch(client, name).await?;
    println!("name      {}", s(&conn, "name"));
    println!("provider  {}", s(&conn, "provider"));
    println!("label     {}", s(&conn, "label"));
    println!("key       {}", s(&conn, "key_id"));
    if !reveal {
        println!("\n(secret withheld — `mafold connection show {name} --reveal` to decrypt here)");
        return Ok(());
    }
    // `--reveal` decrypts the raw secret on this machine — same gate as `env`.
    if let Some(msg) = plaintext_refused(bot_turn(), "show --reveal", name) {
        bail!(msg);
    }
    require_connection_use(base, client, &sess.username, name).await?;
    let spec = descriptor(client, &s(&conn, "provider")).await?;
    refuse_if_spend_only(name, &spec)?;
    let (umk, key_id, _) = unlock(client, sess).await?;
    let fields = open_payload(&umk, &key_id, &conn)?;
    println!();
    for (k, v) in fields {
        // Vendors store expiries as numbers and flags as bools; printing only
        // strings would render those blank and read as a missing field.
        let shown = match &v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        println!("{k:<16}{shown}");
    }
    Ok(())
}

/// A spend-only row (see `credential_stays_inside`) is used through its
/// methods and never printed — checked before the vault is even opened.
fn refuse_if_spend_only(name: &str, spec: &ProviderInfo) -> Result<()> {
    if mafold_core::connections::credential_stays_inside(spec) {
        return Err(anyhow!(
            "`{name}` ({}) is never revealed or exported: its token is only used inside \
             `mafold connection call {name} …`, so nothing that runs here ever holds it. \
             See what it can do: `mafold connection methods {name}`",
            spec.display
        ));
    }
    Ok(())
}

/// Shell exports for a connection, so a local tool can consume it without a
/// bespoke integration. Deliberately not written to any file: piping into
/// `eval` keeps the plaintext in a process, not on disk.
async fn env(base: &str, client: &Client, sess: &session::Session, name: &str) -> Result<()> {
    if let Some(msg) = plaintext_refused(bot_turn(), "env", name) {
        bail!(msg);
    }
    require_connection_use(base, client, &sess.username, name).await?;
    let conn = fetch(client, name).await?;
    let provider = s(&conn, "provider");
    let spec = descriptor(client, &provider).await?;
    refuse_if_spend_only(name, &spec)?;
    let (umk, key_id, _) = unlock(client, sess).await?;
    let fields = open_payload(&umk, &key_id, &conn)?;
    let (var, val) = env_pair(&spec, &fields, name)?;
    println!("export {var}={val}");
    Ok(())
}

/// `mafold connection run <name>… -- <command>`: the secrets go into the
/// command's environment, never into this process's output — see
/// [`crate::conceal`]. Same grant as `call` in a bot's turn; exits with the
/// command's own status so `&&` chains behave.
async fn run_with(
    base: &str,
    client: &Client,
    sess: &session::Session,
    names: &[String],
    command: &[String],
) -> Result<()> {
    let mut vars: Vec<(String, String)> = Vec::new();
    let mut opened: Option<(Key, String)> = None;
    for name in names {
        require_connection_use(base, client, &sess.username, name).await?;
        let conn = fetch(client, name).await?;
        let spec = descriptor(client, &s(&conn, "provider")).await?;
        refuse_if_spend_only(name, &spec)?;
        if opened.is_none() {
            let (umk, key_id, _) = unlock(client, sess).await?;
            opened = Some((umk, key_id));
        }
        let (umk, key_id) = opened.as_ref().expect("unlocked above");
        let fields = open_payload(umk, key_id, &conn)?;
        vars.push(env_pair(&spec, &fields, name)?);
    }
    let command = command.to_vec();
    let code = tokio::task::spawn_blocking(move || crate::conceal::run(&command, &vars))
        .await
        .context("the command's runner panicked")??;
    std::process::exit(code);
}

/// A device-side runtime, unlocked.
///
/// The master key is passed in rather than reachable from the runtime, so the
/// core never learns how this machine stores its device key — that stays here,
/// where the 0600 file and the platform's rules live.
async fn runtime(
    base: &str,
    client: &Client,
    sess: &session::Session,
) -> Result<mafold_core::connections::Runtime> {
    let (umk, _, _) = unlock(client, sess).await?;
    Ok(core_runtime(base, sess, umk))
}

/// Every runtime this cli builds, with the one capability that makes this
/// process a MACHINE the user can call.
///
/// One constructor, because the failure mode of forgetting it is invisible:
/// a daemon without an executor attached receives shell calls addressed to
/// this very machine and silently declines them (`Runtime::can_serve`), so the
/// caller times out and no log anywhere says why. There are three places that
/// build a runtime — `mafold connection call`, `listen`, and `mafold up`'s
/// resident listener — and all three are the same machine.
fn core_runtime(
    base: &str,
    sess: &session::Session,
    umk: Key,
) -> mafold_core::connections::Runtime {
    // `base` here is the ORIGIN — `Client` appends `/api` itself, and the
    // core's rpc does not. Passing it through unchanged makes every core call
    // a 404 that reads as "this server is older than this client", which is a
    // very convincing wrong answer.
    let mut rt =
        mafold_core::connections::Runtime::new(&format!("{base}/api"), &sess.token, umk);
    rt.attach_computer(&sess.device_id, crate::computer::executor());
    rt
}

// ── linking on behalf of another surface (`events.connectionLink`) ─────────
//
// The web pane has a Connect button for Codex and no command to copy, and this
// is what stands behind it. A provider whose OAuth client redirects to a
// loopback port can only be linked ON a machine — but the machine does not have
// to be the INTERFACE. A client asks (`startConnectionLink`), the event fans out
// to every device the person has online, one claims it, binds the port, and
// answers with the URL for the asking surface to open. The credential is still
// born here and sealed here; only the button moved.

/// A connection name that isn't taken yet — `codex`, then `codex-2`.
///
/// Same rule as the web's `uniqueName`, for the same reason: a second Codex
/// account must make a second row rather than overwrite the first. Naming is
/// this device's job because the asking surface never sees the grant.
async fn free_name(client: &Client, provider_id: &str) -> String {
    let base = provider_id
        .trim_end_matches("-api")
        .trim_end_matches("-oauth")
        .to_string();
    free_name_from(client, &base).await
}

/// `base`, or `base-2`, `base-3`… — the first name the account isn't using.
pub(crate) async fn free_name_from(client: &Client, base: &str) -> String {
    let base = base.to_string();
    let taken: Vec<String> = client
        .call("listConnections", json!({}))
        .await
        .ok()
        .map(|v| as_array(&v, "items").iter().map(|c| s(c, "name")).collect())
        .unwrap_or_default();
    if !taken.iter().any(|n| n == &base) {
        return base;
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|c| !taken.iter().any(|n| n == c))
        .unwrap_or(base)
}

/// Answer an `events.connectionLink` frame. `true` when this device took it.
///
/// Claim FIRST, like every other relayed event: the frame reaches every socket
/// the account has, and two machines binding the vendor's port for one request
/// is two consent screens for one click. The claim also decides who reports the
/// outcome, so the asking surface hears exactly one ending.
pub async fn handle_link_event(
    client: &Client,
    sess: &session::Session,
    umk: &Key,
    key_id: &str,
    envelope: &str,
) -> bool {
    let env: Value = match serde_json::from_str(envelope) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if env.get("method").and_then(Value::as_str) != Some("events.connectionLink") {
        return false;
    }
    let p = env.get("params").cloned().unwrap_or(Value::Null);
    let link_id = s(&p, "link_id");
    let provider = s(&p, "provider");
    let reconnect = p["reconnect"].as_str().map(str::to_owned);
    let reconnect_label = reconnect.as_ref().and_then(|_| p["label"].as_str()).map(str::to_owned);
    if link_id.is_empty() {
        return false;
    }

    let claimed = client
        .call("claimConnectionCall", json!({ "call_id": link_id }))
        .await
        .ok()
        .and_then(|v| v.get("claimed").and_then(Value::as_bool))
        .unwrap_or(false);
    if !claimed {
        return false;
    }

    // Answer once, whatever happens: the caller is parked on this and a silent
    // device turns "your Mac is busy" into "no machine took it".
    let answer = |result: Value, error: Option<String>| {
        let mut body = json!({ "call_id": link_id, "result": result });
        if let Some(e) = error {
            body["error"] = Value::String(e);
        }
        client.call("answerConnectionCall", body)
    };

    let spec = match descriptor(client, &provider).await.ok() {
        // A server the person NAMED. This machine talks to it and answers with
        // whichever of three endings it turns out to have — a consent screen to
        // open, a connection already stored (no credential needed), or "it
        // wants a token typed" — and the asking surface never has to reach the
        // server itself, which is the case this branch exists for.
        Some(sp) if sp.delegates_endpoint() => {
            let endpoint = s(&p, "endpoint");
            let typed = |k: &str| Some(s(&p, k)).filter(|v| !v.is_empty());
            crate::mcp_link::serve_link(
                client,
                sess,
                umk,
                key_id,
                &sp,
                &endpoint,
                &link_id,
                reconnect.clone().or_else(|| typed("name")),
                typed("label"),
                reconnect.is_some(),
            )
            .await;
            return true;
        }
        // A machine binding finishes right here: no port to bind, no consent
        // screen, nothing for the asking surface to open. It answers with the
        // connection instead of a URL, and `startConnectionLink` reads that as
        // "already linked".
        Some(sp) if is_device_binding(&sp) => {
            let outcome = bind_for_link(client, sess, umk, key_id, &sp, reconnect.as_deref(), reconnect_label.as_deref()).await;
            match &outcome {
                Ok(name) => {
                    let _ = answer(
                        json!({
                            "authorize_url": "",
                            "device": sess.device_name,
                            "connection": name,
                        }),
                        None,
                    )
                    .await;
                    println!("· connections: bound {name} → this machine ({})", sess.device_name);
                }
                Err(e) => {
                    let _ = answer(Value::Null, Some(format!("{e:#}"))).await;
                    println!("· connections: could not bind this machine — {e:#}");
                }
            }
            // Report as well, so a caller that polls rather than reading the
            // start response lands on the same ending.
            let body = match &outcome {
                Ok(name) => json!({ "link_id": link_id, "connection": name }),
                Err(e) => json!({ "link_id": link_id, "error": format!("{e:#}") }),
            };
            let _ = client.call("reportConnectionLink", body).await;
            return true;
        }
        Some(sp) if sp.import_path.is_some() && sp.oauth_fixed.is_none() => {
            let outcome = import_for_link(client, umk, key_id, &sp, reconnect.as_deref(), reconnect_label.as_deref()).await;
            match &outcome {
                Ok(name) => { let _ = answer(json!({ "authorize_url": "", "device": sess.device_name, "connection": name }), None).await; }
                Err(e) => { let _ = answer(Value::Null, Some(format!("{e:#}"))).await; }
            }
            let body = match outcome {
                Ok(name) => json!({ "link_id": link_id, "connection": name }),
                Err(e) => json!({ "link_id": link_id, "error": format!("{e:#}") }),
            };
            let _ = client.call("reportConnectionLink", body).await;
            return true;
        }
        Some(sp) if sp.oauth_fixed.is_some() => sp,
        Some(sp) => {
            let _ = answer(
                Value::Null,
                Some(format!(
                    "{} isn't linked by a consent screen — it's pasted or read from a file",
                    sp.display
                )),
            )
            .await;
            return true;
        }
        None => {
            let _ = answer(
                Value::Null,
                Some(format!(
                    "this machine's Mafold doesn't know a provider called `{provider}` — update it"
                )),
            )
            .await;
            return true;
        }
    };

    let leg = match oauth_begin(&spec).await {
        Ok(leg) => leg,
        Err(e) => {
            let _ = answer(Value::Null, Some(format!("{e:#}"))).await;
            return true;
        }
    };
    let authorize_url = leg.authorize_url.clone();
    let _ = answer(
        json!({ "authorize_url": authorize_url, "device": sess.device_name }),
        None,
    )
    .await;

    // The human half — a consent screen, on a person's clock — must not hold
    // the socket loop. Everything it needs is owned here so the task outlives
    // this frame.
    let client = client.clone();
    let umk = umk.clone();
    let key_id = key_id.to_string();
    tokio::spawn(async move {
        let outcome = finish_linking(&client, &spec, leg, &umk, &key_id, reconnect.as_deref(), reconnect_label.as_deref()).await;
        let body = match &outcome {
            Ok(name) => json!({ "link_id": link_id, "connection": name }),
            Err(e) => json!({ "link_id": link_id, "error": format!("{e:#}") }),
        };
        let _ = client.call("reportConnectionLink", body).await;
        match outcome {
            Ok(name) => println!("· connections: linked {name} → {}", spec.display),
            Err(e) => println!("· connections: {} link failed — {e:#}", spec.display),
        }
    });
    true
}

/// Bind this machine for a link someone started elsewhere (the web's Connect
/// button). Same sealing as `bind_machine`, minus the terminal.
///
/// The name is chosen here, by `free_name`, for the same reason the OAuth path
/// chooses it here: the asking surface never sees the payload, and two laptops
/// bound from the same browser must become two rows rather than one machine
/// overwriting the other.
async fn bind_for_link(
    client: &Client,
    sess: &session::Session,
    umk: &Key,
    key_id: &str,
    spec: &ProviderInfo,
    reconnect: Option<&str>,
    reconnect_label: Option<&str>,
) -> Result<String> {
    let fields = machine_binding(sess);
    let (blob, wrapped_dek) = seal_payload(umk, &fields)?;
    let name = match reconnect { Some(name) => name.to_owned(), None => free_name(client, &spec.id).await };
    client
        .call(
            "putConnection",
            json!({
                "name": name,
                "provider": spec.id,
                "label": reconnect_label.unwrap_or(&sess.device_name),
                "blob": blob,
                "wrapped_dek": wrapped_dek,
                "key_id": key_id, "create_only": reconnect.is_none(),
            }),
        )
        .await
        .context("putConnection failed")?;
    Ok(name)
}

/// Import the provider's declared local login into one new sealed connection.
/// The same collection path as `connection add --import`, callable from a card.
async fn import_for_link(client: &Client, umk: &Key, key_id: &str, spec: &ProviderInfo, reconnect: Option<&str>, reconnect_label: Option<&str>) -> Result<String> {
    let mut fields = collect(spec, true, false)?;
    enrich_oauth_payload(spec, &mut fields);
    let (blob, wrapped_dek) = seal_payload(umk, &fields)?;
    let name = match reconnect { Some(name) => name.to_owned(), None => free_name(client, &spec.id).await };
    client.call("putConnection", json!({
        "name": name, "provider": spec.id, "label": reconnect_label.unwrap_or(&spec.display),
        "blob": blob, "wrapped_dek": wrapped_dek, "key_id": key_id, "create_only": reconnect.is_none(),
    })).await.context("putConnection failed")?;
    Ok(name)
}

/// Wait out the consent screen, seal what comes back, store it. The connection
/// name it chose, so the report can name it.
async fn finish_linking(
    client: &Client,
    spec: &ProviderInfo,
    leg: OauthLeg,
    umk: &Key,
    key_id: &str,
    reconnect: Option<&str>,
    reconnect_label: Option<&str>,
) -> Result<String> {
    let (mut fields, suggested) = oauth_finish(spec, leg).await?;
    enrich_oauth_payload(&spec, &mut fields);
    let (blob, wrapped_dek) = seal_payload(umk, &fields)?;
    let name = match reconnect { Some(name) => name.to_owned(), None => free_name(client, &spec.id).await };
    let label = suggested.unwrap_or_else(|| {
        fields
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    });
    client
        .call(
            "putConnection",
            json!({
                "name": name,
                "provider": spec.id,
                "label": reconnect_label.unwrap_or(&label),
                "blob": blob,
                "wrapped_dek": wrapped_dek,
                "key_id": key_id, "create_only": reconnect.is_none(),
            }),
        )
        .await
        .context("putConnection failed")?;
    Ok(name)
}

/// Hold a human WS open and let the CORE answer connection calls on it.
///
/// This is the terminal's version of what the web client does passively: the
/// frame arrives, `connections::handle_event` decides whether it's ours, claims
/// it, opens the vault, calls the provider, answers. Nothing here inspects the
/// event beyond handing it over — the whole point is that every device answers
/// with the same Rust.
/// The server pings every 25s. A link that has said NOTHING for this long is
/// dead whatever the socket says: a laptop that slept, a proxy that dropped the
/// flow, an api restart whose RST never arrived. Both listeners used to wait on
/// such a link forever — process alive, calls unanswered, "listening" to no
/// one (@fei_pota's Mac, 2026-09-27: a Connect sent to it went 20s unanswered;
/// this Mac on 2026-09-23: seven hours of silence in supervisor.log).
const LISTEN_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// The next frame of a listener's link, or `None` once the link is over:
/// closed, errored, or silent for `idle`. The caller reconnects on `None`.
async fn next_live<S>(
    ws: &mut S,
    idle: std::time::Duration,
) -> Option<tokio_tungstenite::tungstenite::Message>
where
    S: futures_util::Stream<
            Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
{
    use futures_util::StreamExt;
    match tokio::time::timeout(idle, ws.next()).await {
        Ok(Some(Ok(frame))) => Some(frame),
        _ => None,
    }
}

async fn listen(base: &str, client: &Client, sess: &session::Session) -> Result<()> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    // Unlocked once, used twice: the core answers calls with the key, and a
    // link started from another surface seals its new grant with the same one.
    let (umk, key_id, _) = unlock(client, sess).await?;
    let mut rt = core_runtime(base, sess, umk.clone());
    println!(
        "✓ listening as @{} on {} — connection calls granted to your bots run here.\n  ctrl-c to stop.",
        sess.username, sess.device_name
    );
    loop {
        let mut ws = match client.ws_connect().await {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("ws connect failed ({e}) — retrying in 3s");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };
        while let Some(frame) = next_live(&mut ws, LISTEN_IDLE).await {
            match frame {
                WsMsg::Text(t) => {
                    if mafold_core::connections::handle_event(&mut rt, &t).await {
                        println!("· answered a connection call");
                    } else if handle_link_event(client, sess, &umk, &key_id, &t).await {
                        println!("· took a link request — finish the sign-in in your browser");
                    }
                }
                // The server pings every 25s and treats silence as death; an
                // unanswered ping here would look like "listen is on but calls
                // time out", which is the worst version of off.
                WsMsg::Ping(p) => {
                    let _ = ws.send(WsMsg::Pong(p)).await;
                }
                WsMsg::Close(_) => break,
                _ => {}
            }
        }
        eprintln!("ws dropped or went silent — reconnecting in 2s");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// The `mafold up` resident listener: the reason "start your daemons" is
/// enough for granted bots (@chatgpt on your Codex connection) to work, with
/// no second command to know about.
///
/// QUIET by construction: it only serves when this machine already holds a
/// still-current vault key, fetching an approved wrap when the cache is empty.
/// It never creates a vault or prompts. Waiting machines retry automatically.
const VAULT_RECHECK: std::time::Duration = std::time::Duration::from_secs(120);

/// Keep the socket alive even before a key is available. Requests are retained
/// in this loop, so the request that wakes a locked device is also answered.
fn key_refresh_event(envelope: &str, device_id: &str, locked: bool) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(envelope) else { return false; };
    match v.get("method").and_then(Value::as_str) {
        Some("events.connectionCall" | "events.connectionLink") => locked,
        Some("events.vaultReset") => true,
        Some("events.vaultDeviceApproved") => v["params"]["device_id"].as_str() == Some(device_id),
        _ => false,
    }
}

async fn listener_frame<S>(ws: &mut S, retry: &mut tokio::time::Interval)
    -> Result<Option<tokio_tungstenite::tungstenite::Message>, ()>
where S: futures_util::Stream<Item = Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>> + Unpin {
    tokio::select! {
        frame = next_live(ws, LISTEN_IDLE) => Ok(frame),
        _ = retry.tick() => Err(()),
    }
}

pub async fn supervise_listener(base: String, username: String) {
    loop {
        let Some(sess) = session::load_named(&username) else {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            continue;
        };
        let client = Client::new(base.clone(), sess.token.clone());
        if let Ok(dev) = vault::device_key() {
            let _ = listener_session(&base, &client, &sess, &dev, true).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn listener_session(base: &str, client: &Client, sess: &session::Session, dev: &DeviceKey, persist_cache: bool) -> Result<()> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message as WsMsg;
    let mut said_locked = false;
        // Subscribe BEFORE enrollment, so an approval during the first fetch
        // is queued rather than lost until the two-minute fallback.
        let (mut ws, _) = client.ws_connect().await?;
        let mut runtime = quiet_runtime_for_device(base, client, sess, dev, persist_cache).await;
        let mut retry = tokio::time::interval_at(tokio::time::Instant::now() + VAULT_RECHECK, VAULT_RECHECK);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if runtime.is_none() && !said_locked {
                println!("· connections: waiting for this computer's vault key — open Settings → Connections on a device that already holds it; this computer will pick it up automatically");
                said_locked = true;
            } else if runtime.is_some() && said_locked {
                println!("· connections: answering granted calls as @{}", sess.username);
                said_locked = false;
            }
            let frame = match listener_frame(&mut ws, &mut retry).await {
                Err(()) => {
                    if runtime.is_none() { runtime = quiet_runtime_for_device(base, client, sess, dev, persist_cache).await; }
                    continue;
                }
                Ok(Some(frame)) => frame,
                Ok(None) => break,
            };
            match frame {
                WsMsg::Text(t) => {
                    let reset = serde_json::from_str::<Value>(&t).ok()
                        .is_some_and(|v| v["method"] == "events.vaultReset");
                    if reset { runtime = None; }
                    if key_refresh_event(&t, &sess.device_id, runtime.is_none()) {
                        // Preserve the runtime and MCP tool cache on ordinary calls.
                        // Reconnect, reset and local approval revalidate the key.
                        runtime = quiet_runtime_for_device(base, client, sess, dev, persist_cache).await;
                        retry.reset();
                    }
                    if let Some((rt, umk, key_id)) = runtime.as_mut() {
                        if mafold_core::connections::handle_event(rt, &t).await {
                            println!("· connections: answered a call");
                        } else {
                            handle_link_event(&client, &sess, umk, key_id, &t).await;
                        }
                    }
                }
                WsMsg::Ping(p) => { let _ = ws.send(WsMsg::Pong(p)).await; }
                WsMsg::Close(_) => break,
                _ => {}
            }
        }
    Ok(())
}

/// The unlocked runtime IF this machine can produce one silently: registered
/// device + a cached or newly fetched UMK in the server's recorded generation.
/// Missing approval stays pending; this path never mints a replacement key.
///
/// The key comes back alongside the runtime because answering a call and
/// SEALING a new grant (a link this device runs for another surface) are the
/// same permission — a machine that can do one can do the other, and handing
/// out both from one place is what keeps that true.
async fn quiet_runtime_for_device(
    base: &str, client: &Client, sess: &session::Session, dev: &DeviceKey, persist_cache: bool,
) -> Option<(mafold_core::connections::Runtime, Key, String)> {
    let reg = register(client, sess, dev).await.ok()?;
    let cached = if persist_cache { vault::cached_umk(dev) } else { None };
    let cached_id = cached.as_ref().map(|(_, id)| id.clone());
    let (umk, key_id) = quiet_key(client, sess, dev, &reg, cached).await?;
    if cached_id.as_deref() != Some(key_id.as_str()) {
        if persist_cache { vault::cache_umk(&umk, dev, &key_id).ok()?; }
        auto_approve_pending(client, &umk, &key_id).await;
    }
    let rt = core_runtime(base, sess, umk.clone());
    Some((rt, umk, key_id))
}

/// Kept separate from the disk cache so pending → approved can be exercised
/// without touching the host machine's real vault.
async fn quiet_key(
    client: &Client,
    sess: &session::Session,
    dev: &DeviceKey,
    reg: &Value,
    cached: Option<(Key, String)>,
) -> Option<(Key, String)> {
    let registered_id = s(&reg["device"], "key_id");
    if reg["device"]["approved"].as_bool() != Some(true) || registered_id.is_empty() {
        return None;
    }
    if let Some((key, id)) = cached {
        if id == registered_id {
            return Some((key, id));
        }
    }
    let v = client.call("getVaultKey", json!({ "device_id": sess.device_id })).await.ok()?;
    let key_id = s(&v, "key_id");
    // Approval may have changed during the request. Retry on the next tick.
    if key_id != registered_id {
        return None;
    }
    let key = vault::unwrap_key(&dev.secret, &s(&v, "sealed_umk")).ok()?;
    Some((key, key_id))
}

/// What a connection can do, asked of the provider itself.
///
/// Nothing here is a Mafold-maintained list: the catalog comes from the
/// provider's MCP server, so a tool it ships tomorrow shows up without a
/// release. That is the reason this layer speaks MCP at all.
async fn methods(
    base: &str,
    client: &Client,
    sess: &session::Session,
    name: &str,
    schema: bool,
) -> Result<()> {
    let mut rt = runtime(base, client, sess).await?;
    let methods = rt.methods(name).await.map_err(|e| anyhow!("{e}"))?;
    if methods.is_empty() {
        println!("{name} offers no methods.");
        return Ok(());
    }
    println!("{} method{}", methods.len(), if methods.len() == 1 { "" } else { "s" });
    for m in &methods {
        let mark = if m.read_only { " " } else { "!" };
        println!("\n{mark} {}", m.name);
        if !m.description.is_empty() {
            // One line: a catalog is for choosing, and some descriptions run
            // to paragraphs.
            let first = m.description.lines().next().unwrap_or("");
            println!("    {}", first.chars().take(140).collect::<String>());
        }
        if schema {
            println!(
                "    {}",
                serde_json::to_string_pretty(&m.input_schema)
                    .unwrap_or_default()
                    .replace('\n', "\n    ")
            );
        } else if let Some(props) = m.input_schema.get("properties").and_then(|p| p.as_object()) {
            let required: Vec<&str> = m
                .input_schema
                .get("required")
                .and_then(|r| r.as_array())
                .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let args: Vec<String> = props
                .keys()
                .map(|k| {
                    if required.contains(&k.as_str()) {
                        k.clone()
                    } else {
                        format!("[{k}]")
                    }
                })
                .collect();
            if !args.is_empty() {
                println!("    params: {}", args.join(" "));
            }
        }
    }
    println!("\n! = may write. --schema for full parameter types.");
    Ok(())
}

/// Run one method and print what came back.
async fn call(
    base: &str,
    client: &Client,
    sess: &session::Session,
    name: &str,
    method: &str,
    params: &str,
) -> Result<()> {
    // Parse before unlocking: a typo in `--params` should not cost a vault
    // round trip, and the error should point at the JSON rather than arriving
    // after something that looks like real work.
    let args: Value = serde_json::from_str(params)
        .with_context(|| format!("--params is not JSON: {params}"))?;
    if !args.is_object() {
        bail!("--params must be a JSON object, e.g. --params '{{\"query\":\"roadmap\"}}'");
    }
    // A bot turn must hold the grant before the local vault runs the call — the
    // relayed path checks it server-side, and the local fast-path must not be
    // the way around that.
    require_connection_use(base, client, &sess.username, name).await?;
    let mut rt = runtime(base, client, sess).await?;
    // Two places this can run, and the runtime already knows which: a
    // credential opens HERE (that is the whole vault), but a machine of yours
    // answers on ITSELF. Asking the relay for something this process could have
    // done would be a round trip to reach your own shell; running locally
    // something bound to another laptop would be worse than a round trip.
    let out = if rt.can_serve(name, method).await {
        rt.call_any("cli", name, method, &args, false)
            .await
            .map_err(|e| anyhow!("{e}"))?
    } else {
        eprintln!("· {name} lives on another machine of yours — relaying");
        relay(client, name, method, &args).await?
    };

    // MCP wraps results in `content: [{type, text}]`. Unwrap the common
    // all-text case so a shell pipeline gets the payload rather than the
    // envelope; anything richer prints whole.
    if let Some(items) = out.get("content").and_then(|c| c.as_array()) {
        let all_text = !items.is_empty()
            && items
                .iter()
                .all(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"));
        if all_text {
            for i in items {
                println!("{}", i.get("text").and_then(|t| t.as_str()).unwrap_or(""));
            }
            // A tool that reports failure in-band still exits non-zero, or a
            // script would treat "I couldn't do that" as success.
            if out.get("isError").and_then(|e| e.as_bool()) == Some(true) {
                bail!("{name}.{method} reported an error");
            }
            return Ok(());
        }
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn rm(client: &Client, name: &str) -> Result<()> {
    client
        .call("deleteConnection", json!({ "name": name }))
        .await
        .context("deleteConnection failed")?;
    println!("✓ removed {name} from Mafold.");
    println!("  the credential still exists at the provider — revoke it there if you meant to.");
    Ok(())
}

async fn devices(client: &Client, sess: &session::Session) -> Result<()> {
    let dev = vault::device_key()?;
    register(client, sess, &dev).await?;
    let v = client.call("listVaultDevices", json!({})).await?;
    let items = as_array(&v, "items");
    let items_pending = items.iter().any(|d| {
        d["approved"].as_bool() != Some(true) || d["has_key"].as_bool() != Some(true)
    });
    println!("{:<20} {:<22} {:<12} {}", "DEVICE", "NAME", "STATUS", "FINGERPRINT");
    for d in items {
        let id = s(&d, "device_id");
        let approved = d["approved"].as_bool() == Some(true);
        let has_key = d["has_key"].as_bool() == Some(true);
        let mine = id == sess.device_id;
        let status = match (approved && has_key, mine) {
            (true, true) => "this device",
            (true, false) => "enrolled",
            (false, true) => "pending (you)",
            (false, false) => "pending",
        };
        println!("{:<20} {:<22} {:<12} {}", id, s(&d, "device_name"), status, s(&d, "fingerprint"));
    }
    // No "approve a pending device" footer. `register` above already handed the
    // key to everything waiting, so anything still listed as pending is waiting
    // on a machine that hasn't been online — not on the user. Advertising a
    // command here would suggest otherwise.
    if items_pending {
        println!("\npending devices get the key automatically from any machine of yours that has it.");
    }
    Ok(())
}

async fn approve(
    client: &Client,
    sess: &session::Session,
    device_id: &str,
    yes: bool,
) -> Result<()> {
    let (umk, key_id, _) = unlock(client, sess).await?;
    let v = client.call("listVaultDevices", json!({})).await?;
    let target = as_array(&v, "items")
        .into_iter()
        .find(|d| s(d, "device_id") == device_id)
        .ok_or_else(|| anyhow!("no device `{device_id}` — see `mafold connection devices`"))?;

    let public_key = s(&target, "public_key");
    let fp = vault::fingerprint(&public_key);
    println!("device      {}", s(&target, "device_id"));
    println!("name        {}", s(&target, "device_name"));
    println!("fingerprint {fp}");

    // The server relays the public key, so the server could substitute one. The
    // human comparing this string to the one printed on the other machine is
    // the actual authorization — everything else here is bookkeeping.
    if !yes {
        let ans = crate::prompt("\nDoes that fingerprint match the other machine? [y/N] ");
        if !ans.eq_ignore_ascii_case("y") {
            println!("aborted — nothing was shared.");
            return Ok(());
        }
    }

    let wrapped = vault::wrap_key_for(&public_key, &umk)?;
    client
        .call(
            "approveVaultDevice",
            json!({
                "device_id": device_id,
                "sealed_umk": wrapped,
                "key_id": key_id,
                "public_key": public_key,
            }),
        )
        .await
        .context("approveVaultDevice failed")?;
    println!("✓ approved — run `mafold connection unlock` on that machine.");
    Ok(())
}

async fn revoke(
    client: &Client,
    sess: &session::Session,
    device_id: &str,
    no_rotate: bool,
) -> Result<()> {
    if device_id == sess.device_id {
        bail!("that's this machine — revoke it from another device, or you'll lock yourself out");
    }
    client
        .call("revokeVaultDevice", json!({ "device_id": device_id }))
        .await
        .context("revokeVaultDevice failed")?;
    println!("✓ {device_id} removed from the vault.");

    if no_rotate {
        println!("  NOT re-keyed: if that machine was ever approved, it still holds the master key");
        println!("  and can still open every connection. Re-key with `mafold connection revoke … `");
        return Ok(());
    }
    rotate(client, sess).await
}

/// Mint a new master key, re-seal every connection under it, and re-wrap it for
/// the devices that remain.
///
/// This is what makes revocation real. Deleting a row cannot reach into a
/// machine that already copied the key, so the only honest revocation is to
/// stop using the key it has.
async fn rotate(client: &Client, sess: &session::Session) -> Result<()> {
    let (old_umk, old_key_id, dev) = unlock(client, sess).await?;
    let conns = as_array(
        &client.call("listConnections", json!({})).await?,
        "items",
    );
    let devices = as_array(
        &client.call("listVaultDevices", json!({})).await?,
        "items",
    );

    let new_umk = Key::random();
    let key_id = vault::new_key_id();

    // Re-seal first. If this fails halfway, the old key still opens everything
    // and the vault is merely un-rotated — whereas re-wrapping keys first would
    // leave devices holding a key that opens nothing.
    for c in &conns {
        let name = s(c, "name");
        // A value handed over through a card is wrapped per device, and the
        // device being revoked may hold one of those wraps. Re-sealing can't
        // take a wrap back, and these rows live for days — so they go, and
        // whoever needs one asks again.
        if s(c, "wrapped_dek").is_empty() {
            client
                .call("deleteConnection", json!({ "name": name }))
                .await
                .with_context(|| format!("drop handed-over `{name}`"))?;
            eprintln!("  dropped `{name}` (handed over through a card; ask for it again)");
            continue;
        }
        let fields = open_payload(&old_umk, &old_key_id, c)
            .with_context(|| format!("re-key {name}: could not open it with the current key"))?;
        let (blob, wrapped_dek) = seal_payload(&new_umk, &fields)?;
        client
            .call(
                "putConnection",
                json!({
                    "name": name,
                    "provider": s(c, "provider"),
                    "label": s(c, "label"),
                    "blob": blob,
                    "wrapped_dek": wrapped_dek,
                    "key_id": key_id,
                }),
            )
            .await
            .with_context(|| format!("re-key {name}: putConnection failed"))?;
    }

    for d in &devices {
        if d["approved"].as_bool() != Some(true) {
            continue;
        }
        let public_key = s(d, "public_key");
        let wrapped = vault::wrap_key_for(&public_key, &new_umk)?;
        client
            .call(
                "approveVaultDevice",
                json!({
                    "device_id": s(d, "device_id"),
                    "sealed_umk": wrapped,
                    "key_id": key_id,
                    "public_key": public_key,
                }),
            )
            .await
            .with_context(|| format!("re-wrap for {}", s(d, "device_name")))?;
    }

    vault::cache_umk(&new_umk, &dev, &key_id)?;
    println!(
        "✓ re-keyed {} connection(s) under a new master key; {} device(s) re-wrapped.",
        conns.len(),
        devices.iter().filter(|d| d["approved"].as_bool() == Some(true)).count()
    );
    println!("  set the recovery passphrase again — the old one wraps the retired key:");
    println!("    mafold connection set-recovery");
    Ok(())
}

async fn set_recovery(client: &Client, sess: &session::Session) -> Result<()> {
    let (umk, key_id, _) = unlock(client, sess).await?;
    let pass = crate::prompt_password("Recovery passphrase: ");
    if pass.chars().count() < 12 {
        bail!("use at least 12 characters — this is the one thing an attacker can grind offline");
    }
    let again = crate::prompt_password("Again: ");
    if pass != again {
        bail!("they don't match");
    }
    let blob = vault::wrap_umk_with_passphrase(&umk, &pass)?;
    client
        .call(
            "putVaultRecovery",
            json!({
                "salt": blob.salt,
                "mem_kib": blob.mem_kib,
                "time_cost": blob.time_cost,
                "lanes": blob.lanes,
                "sealed_umk": blob.sealed_umk,
                "key_id": key_id,
            }),
        )
        .await
        .context("putVaultRecovery failed")?;
    println!("✓ recovery set. Write the passphrase down somewhere physical —");
    println!("  we cannot reset it, which is the same reason we cannot read your connections.");
    Ok(())
}

async fn recover(client: &Client, sess: &session::Session) -> Result<()> {
    let dev = vault::device_key()?;
    register(client, sess, &dev).await?;
    let v = client
        .call("getVaultRecovery", json!({}))
        .await
        .context("no recovery blob is set for this account")?;
    let r = &v["recovery"];
    let blob = vault::RecoveryBlob {
        salt: s(r, "salt"),
        mem_kib: r["mem_kib"].as_u64().unwrap_or(0) as u32,
        time_cost: r["time_cost"].as_u64().unwrap_or(0) as u32,
        lanes: r["lanes"].as_u64().unwrap_or(0) as u32,
        sealed_umk: s(r, "sealed_umk"),
    };
    let pass = crate::prompt_password("Recovery passphrase: ");
    let umk = vault::unwrap_umk_with_passphrase(&blob, &pass)?;
    let key_id = s(r, "key_id");

    // Recovering proves possession of the passphrase, not of an approved
    // device — so enrol this machine properly rather than leaving it working
    // off a cache that `devices` would never list.
    let wrapped = vault::wrap_key_for(&dev.public, &umk)?;
    client
        .call(
            "approveVaultDevice",
            json!({
                "device_id": sess.device_id,
                "sealed_umk": wrapped,
                "key_id": key_id,
                "public_key": dev.public,
            }),
        )
        .await
        .context("enrolling this device after recovery failed")?;
    vault::cache_umk(&umk, &dev, &key_id)?;
    println!("✓ recovered and enrolled {} ({})", sess.device_name, vault::fingerprint(&dev.public));
    println!("  review your devices and revoke anything you don't recognize:");
    println!("    mafold connection devices");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real CLI event loop for browser → API → native-device E2E. No mock
    /// transport and no host account/key files. The browser enrolls/approves
    /// this generated device through the dev api, then clicks Connect.
    /// Driven by `mafold-e2e`: the account is the run's own test account
    /// (`MAFOLD_TEST_USER`, token `dev:<user>`), never a shared or seeded one.
    #[tokio::test]
    #[ignore = "requires the dev api (mafold-e2e) and a browser E2E driver"]
    async fn live_resident_listener_for_browser_e2e() {
        let base = std::env::var("MAFOLD_TEST_API_ORIGIN").expect("dev API origin");
        assert!(base.starts_with("http://127.0.0.1:"));
        let user = std::env::var("MAFOLD_TEST_USER").expect("the e2e run's test account");
        let generated = mafold_core::vault::generate_device();
        let dev = DeviceKey { secret: generated.secret, public: generated.public };
        let sess = session::Session { token: format!("dev:{user}"), username: user, device_id: "e2e-native-device".into(), device_name: "E2E Mac CLI".into() };
        let client = Client::new(base.clone(), sess.token.clone());
        let run = async { loop {
            let _ = listener_session(&base, &client, &sess, &dev, false).await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }};
        let _ = tokio::time::timeout(std::time::Duration::from_secs(900), run).await;
    }

    #[test]
    fn unlocked_listener_keeps_runtime_for_calls_and_links() {
        for method in ["events.connectionCall", "events.connectionLink"] {
            let event = json!({"method": method}).to_string();
            assert!(!key_refresh_event(&event, "local", false));
            assert!(key_refresh_event(&event, "local", true));
        }
        for method in ["events.vaultReset", "events.vaultDeviceApproved"] {
            let event = json!({"method": method, "params": {"device_id": "local"}}).to_string();
            assert!(key_refresh_event(&event, "local", false));
        }
        let other = json!({"method": "events.vaultDeviceApproved", "params": {"device_id": "other"}}).to_string();
        assert!(!key_refresh_event(&other, "local", false));
    }

    #[tokio::test(start_paused = true)]
    async fn listener_waits_two_minutes_but_requests_and_approval_wake_immediately() {
        use tokio_tungstenite::tungstenite::Message as WsMsg;
        let start = tokio::time::Instant::now();
        let mut retry = tokio::time::interval_at(start + VAULT_RECHECK, VAULT_RECHECK);
        // Real sockets receive server pings every 25s. Those frames keep the
        // idle watchdog alive without causing register/getVaultKey calls.
        for seconds in [5, 25, 50, 75, 100, 119] {
            tokio::time::advance(std::time::Duration::from_secs(seconds) - start.elapsed()).await;
            let mut ping = futures_util::stream::iter([Ok(WsMsg::Ping(vec![]))]);
            assert!(matches!(listener_frame(&mut ping, &mut retry).await, Ok(Some(WsMsg::Ping(_)))));
            assert_eq!(start.elapsed().as_secs(), seconds);
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let mut idle = futures_util::stream::pending();
        assert!(listener_frame(&mut idle, &mut retry).await.is_err());
        assert_eq!(start.elapsed(), VAULT_RECHECK);
        for method in ["events.connectionCall", "events.connectionLink", "events.vaultDeviceApproved"] {
            let request = json!({"method":method,"params":{"device_id":"this-device","call_id":"same-request"}}).to_string();
            let mut incoming = futures_util::stream::iter([Ok(WsMsg::Text(request.clone()))]);
            let Ok(Some(WsMsg::Text(received))) = listener_frame(&mut incoming, &mut retry).await else { panic!("request was lost") };
            assert_eq!(received, request, "handle the waking request, not a second click");
            assert!(key_refresh_event(&received, "this-device", true));
            assert_eq!(start.elapsed(), VAULT_RECHECK, "events do not wait for a timer");
        }
        assert!(!key_refresh_event(r#"{"method":"events.vaultDeviceApproved","params":{"device_id":"other"}}"#, "this-device", true));
        assert!(!key_refresh_event(r#"{"method":"events.vaultDevicePending"}"#, "this-device", true));
    }

    #[tokio::test]
    async fn quiet_key_picks_up_approval_without_unlock_or_a_local_cache() {
        let generated = mafold_core::vault::generate_device();
        let dev = DeviceKey { secret: generated.secret, public: generated.public };
        let umk = Key::random();
        let api = spawn_api(vec![("getVaultKey", json!({
            "key_id": "generation-2",
            "sealed_umk": vault::wrap_key_for(&dev.public, &umk).unwrap(),
        }))]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let sess = a_session();
        let pending = json!({"device": {"approved": false, "key_id": ""}});
        assert!(quiet_key(&client, &sess, &dev, &pending, None).await.is_none());
        assert!(api.calls("getVaultKey").is_empty());

        let approved = json!({"device": {"approved": true, "key_id": "generation-2"}});
        let (key, id) = quiet_key(&client, &sess, &dev, &approved, None).await.unwrap();
        assert_eq!(id, "generation-2");
        let sealed = vault::wrap_key_for(&dev.public, &key).unwrap();
        assert!(vault::unwrap_key(&dev.secret, &sealed).is_ok());
        assert_eq!(api.calls("getVaultKey")[0]["device_id"], sess.device_id);
        assert!(quiet_key(&client, &sess, &dev, &approved, Some((key, id))).await.is_some());
        assert_eq!(api.calls("getVaultKey").len(), 1, "current cache needs no fetch");
        assert!(quiet_key(&client, &sess, &dev, &approved,
            Some((Key::random(), "retired-generation".into()))).await.is_some());
        assert_eq!(api.calls("getVaultKey").len(), 2, "stale cache fetches replacement");
        let rotated = json!({"device": {"approved": true, "key_id": "generation-3"}});
        assert!(quiet_key(&client, &sess, &dev, &rotated, None).await.is_none());
        assert!(api.calls("approveVaultDevice").is_empty(), "never creates a vault");
    }

    /// 2026-10-05: `mafold connection env notion` in a bot's turn put
    /// `export NOTION_TOKEN=…` into the trace card of a 24-person group. In a
    /// bot turn the two commands that print a raw secret refuse — even for a
    /// bot that holds the grant — and name the way that doesn't print it.
    #[test]
    fn a_bot_turn_never_prints_a_connections_secret() {
        for cmd in ["env", "show --reveal"] {
            let msg = plaintext_refused(true, cmd, "notion").expect("refused in a bot turn");
            assert!(msg.contains("posted to the conversation"), "{msg}");
            assert!(msg.contains("mafold connection run notion -- "), "points at run: {msg}");
            assert_eq!(plaintext_refused(false, cmd, "notion"), None, "a person's own terminal");
        }
    }

    #[test]
    fn run_takes_names_then_a_command_after_the_double_dash() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(subcommand)]
            cmd: ConnectionCmd,
        }
        let cli = Cli::try_parse_from([
            "x", "run", "notion", "figma", "--", "sh", "-c", "curl -H \"Authorization: Bearer $NOTION_TOKEN\"",
        ])
        .unwrap();
        match cli.cmd {
            ConnectionCmd::Run { names, command } => {
                assert_eq!(names, ["notion", "figma"]);
                assert_eq!(command[..2], ["sh", "-c"]);
            }
            _ => panic!("not run"),
        }
        assert!(Cli::try_parse_from(["x", "run", "notion"]).is_err(), "a command is required");
    }

    #[test]
    fn env_pair_names_the_providers_variable() {
        let spec = provider_infos()
            .into_iter()
            .find(|p| p.env_var.is_some() && !p.fields.is_empty())
            .expect("a single-variable provider");
        let mut fields = serde_json::Map::new();
        fields.insert(spec.fields[0].key.clone(), json!("v4lue-1234"));
        let (var, val) = env_pair(&spec, &fields, "x").unwrap();
        assert_eq!(Some(var), spec.env_var);
        assert_eq!(val, "v4lue-1234");

        // One value and no variable of its own: the connection's name is the
        // variable — what a bot asked for in a secure-input card.
        let mut secret = spec.clone();
        secret.env_var = None;
        secret.fields.truncate(1);
        let (var, _) = env_pair(&secret, &fields, "db-password").unwrap();
        assert_eq!(var, "DB_PASSWORD");
    }

    /// A row a card handed over is sealed to the owner's DEVICES, not the
    /// master key; this machine opens its own wrap and no other.
    #[test]
    fn a_handed_over_row_opens_with_this_devices_own_wrap() {
        let me = mafold_core::vault::generate_device();
        let other = mafold_core::vault::generate_device();
        let (blob, wraps) = mafold_core::vault::seal_for_devices(
            &[("other".into(), other.public.clone()), ("me".into(), me.public.clone())],
            r#"{"value":"hunter2-42"}"#,
        )
        .unwrap();
        let mine = wraps.values().find_map(|w| mafold_core::vault::open_for_device(&me.secret, &blob, w).ok());
        assert_eq!(mine.as_deref(), Some(r#"{"value":"hunter2-42"}"#));
        let stranger = mafold_core::vault::generate_device();
        assert!(wraps.values().all(|w| mafold_core::vault::open_for_device(&stranger.secret, &blob, w).is_err()));
    }

    /// A link that goes silent ends, so the listener reconnects — instead of
    /// waiting forever on a socket nobody is at the other end of (@fei_pota's
    /// Mac, 2026-09-27). A link that keeps talking is left alone.
    #[tokio::test]
    async fn a_silent_link_ends_and_a_talking_one_does_not() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::{protocol::Role, Message as WsMsg};
        use tokio_tungstenite::WebSocketStream;
        let idle = std::time::Duration::from_millis(150);

        let (a, b) = tokio::io::duplex(4096);
        let mut listener = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let _server = WebSocketStream::from_raw_socket(b, Role::Server, None).await; // says nothing
        let over = tokio::time::timeout(std::time::Duration::from_secs(2), next_live(&mut listener, idle))
            .await
            .expect("a silent link must end, not hang");
        assert!(over.is_none());

        let (a, b) = tokio::io::duplex(4096);
        let mut listener = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
        server.send(WsMsg::Ping(vec![1].into())).await.unwrap();
        let frame = next_live(&mut listener, idle).await;
        assert!(matches!(frame, Some(WsMsg::Ping(_))), "{frame:?}");
    }

    /// The registry is served now, so a test that links must have one in the
    /// process — the mock api serves connection calls, not packs.
    fn seat_registry() {
        mafold_core::providers::install_unverified_for_tests(1, provider_infos(), now_ms());
    }

    #[test]
    fn masking_keeps_only_the_tail() {
        assert_eq!(mask_tail("sk-ant-api03-abcd3f9a"), "••••••••3f9a");
        assert_eq!(mask_tail("ab"), "••");
    }

    /// Vendors nest their token bags differently and change the nesting between
    /// versions; import searches instead of hard-coding a path per vendor.
    #[test]
    fn import_finds_fields_at_any_depth() {
        let v: Value = serde_json::from_str(
            r#"{"claudeAiOauth":{"accessToken":"x","access_token":"tok","expires_at":123}}"#,
        )
        .unwrap();
        assert_eq!(find_key(&v, "access_token"), Some(Value::String("tok".into())));
        assert_eq!(find_key(&v, "expires_at"), Some(Value::Number(123.into())));
        assert_eq!(find_key(&v, "refresh_token"), None);
    }

    #[test]
    fn import_reads_camel_case_token_bags_without_provider_specific_paths() {
        let value = json!({ "login": { "accessToken": "access", "refreshToken": "refresh", "expiresAt": 123 } });
        assert_eq!(find_key(&value, "access_token"), Some(json!("access")));
        assert_eq!(find_key(&value, "refresh_token"), Some(json!("refresh")));
        assert_eq!(find_key(&value, "expires_at"), Some(json!(123)));
    }

    /// A container must never be mistaken for a value — that would store `{…}`
    /// as if it were a token and fail much later, at the third party.
    #[test]
    fn import_ignores_containers_with_the_right_name() {
        let v: Value = serde_json::from_str(r#"{"token":{"inner":"x"},"a":{"token":"real"}}"#).unwrap();
        assert_eq!(find_key(&v, "token"), Some(Value::String("real".into())));
    }

    #[test]
    fn every_registry_provider_is_addable() {
        for p in provider_infos() {
            assert!(provider_infos().iter().any(|q| q.id == p.id));
        }
    }

    // ── the --oauth machinery ──

    fn fake_jwt(claims: Value) -> String {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"RS256"}"#),
            b64(claims.to_string().as_bytes()),
            b64(b"sig")
        )
    }

    #[test]
    fn jwt_claims_reads_the_middle_segment_unverified() {
        let t = fake_jwt(serde_json::json!({ "exp": 1234, "email": "a@b.c" }));
        let c = jwt_claims(&t).unwrap();
        assert_eq!(c["exp"], 1234);
        assert!(jwt_claims("not-a-jwt").is_none());
    }

    /// The import file carries neither the OAuth client nor an expiry; the
    /// enrichment is what makes an imported codex payload renewal-complete on
    /// any device.
    #[test]
    fn enrich_fills_client_account_and_expiry_without_clobbering() {
        use mafold_core::mafold_types::connections::codex;
        let spec = provider_infos().into_iter().find(|p| p.id == "codex-oauth").unwrap();
        let id_token = fake_jwt(serde_json::json!({
            "email": "ops@example.com",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc-42", "chatgpt_plan_type": "pro" },
        }));
        let access = fake_jwt(serde_json::json!({ "exp": 1_900_000_000i64 }));
        let mut fields = serde_json::Map::new();
        fields.insert("access_token".into(), Value::String(access));
        fields.insert("id_token".into(), Value::String(id_token));

        enrich_oauth_payload(&spec, &mut fields);
        assert_eq!(fields["client_id"], codex::CLIENT_ID);
        assert_eq!(fields["token_endpoint"], codex::TOKEN_ENDPOINT);
        assert_eq!(fields["account_id"], "acc-42");
        assert_eq!(fields["expires_at"], (1_900_000_000i64 * 1000).to_string());

        // A payload that already knows better keeps its own values.
        fields.insert("account_id".into(), Value::String("acc-original".into()));
        fields.insert("expires_at".into(), Value::String("777".into()));
        enrich_oauth_payload(&spec, &mut fields);
        assert_eq!(fields["account_id"], "acc-original");
        assert_eq!(fields["expires_at"], "777");
    }

    /// The security property behind B: a bot turn (`MAFOLD_BOT_TOKEN` set →
    /// `bot = Some`) may only touch a connection it was granted `connection.use`
    /// for; a human at their own terminal (`bot = None`) is never gated. The
    /// gate exists because the local vault would otherwise run the call as the
    /// owner regardless of any grant.
    #[test]
    fn bot_needs_connection_use_grant_human_never_does() {
        let grants = vec![
            ("opsdu:8964".into(), "notion".into(), true),   // 8964 may use notion
            ("opsdu:codex".into(), "figma".into(), false),  // present, but use=false
        ];

        // Human at a terminal: no bot token, no gate, ever.
        assert_eq!(connection_use_denied(None, "opsdu", "notion", &grants), None);
        assert_eq!(connection_use_denied(None, "opsdu", "stripe", &grants), None);

        // Granted bot → allowed. Handle/name compare case-insensitively.
        assert_eq!(connection_use_denied(Some("opsdu:8964"), "opsdu", "notion", &grants), None);
        assert_eq!(connection_use_denied(Some("OPSDU:8964"), "opsdu", "NOTION", &grants), None);

        // Bot with NO grant for that connection → denied, with the relay's words.
        let d = connection_use_denied(Some("opsdu:8964"), "opsdu", "stripe", &grants)
            .expect("a bot without the grant must be denied");
        assert!(d.contains("@opsdu hasn't allowed @opsdu:8964 to use their `stripe`"), "{d}");
        assert!(d.contains("requestConnectionAccess"), "{d}");

        // A grant that exists but is `can_use=false` does NOT authorize.
        assert!(connection_use_denied(Some("opsdu:codex"), "opsdu", "figma", &grants).is_some());

        // A grant to a DIFFERENT bot doesn't carry over.
        assert!(connection_use_denied(Some("linsky:opus48"), "opsdu", "notion", &grants).is_some());
    }

    /// Providers without a fixed OAuth client must pass through untouched —
    /// enrichment is additive, never a codex branch inside `add`.
    #[test]
    fn enrich_is_a_noop_for_providers_without_an_oauth_client() {
        let spec = provider_infos().into_iter().find(|p| p.id == "notion").unwrap();
        let mut fields = serde_json::Map::new();
        fields.insert("access_token".into(), Value::String("ntn_x".into()));
        enrich_oauth_payload(&spec, &mut fields);
        assert!(!fields.contains_key("client_id"));
        assert!(!fields.contains_key("token_endpoint"));
    }

    #[test]
    fn query_encoding_survives_token_alphabets() {
        assert_eq!(q_encode("openid profile email"), "openid%20profile%20email");
        assert_eq!(q_encode("http://localhost:1455/auth/callback"), "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback");
    }

    #[test]
    fn redirect_uri_splits_into_port_and_path() {
        let p = url_parts::split("http://localhost:1455/auth/callback").unwrap();
        assert_eq!(p.port, 1455);
        assert_eq!(p.path, "/auth/callback");
        assert!(url_parts::split("https://example.com/cb").is_err(), "https redirect would mean a public callback — refuse");
    }

    /// The callback server ends only on OUR state, answers noise with 404, and
    /// refuses a code minted for someone else's flow.
    #[tokio::test]
    async fn callback_waits_past_noise_and_checks_state() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wait = tokio::spawn(async move {
            wait_for_callback(listener, "st-1", "/auth/callback").await
        });

        // Favicon noise first — must be 404'd and NOT end the wait.
        let mut s1 = tokio::net::TcpStream::connect(addr).await.unwrap();
        s1.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = String::new();
        let _ = s1.read_to_string(&mut buf).await;
        assert!(buf.starts_with("HTTP/1.1 404"));

        // The real callback.
        let mut s2 = tokio::net::TcpStream::connect(addr).await.unwrap();
        s2.write_all(b"GET /auth/callback?code=c-9&state=st-1 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = String::new();
        let _ = s2.read_to_string(&mut buf).await;
        assert!(buf.contains("Linked"), "{buf}");
        assert_eq!(wait.await.unwrap().unwrap(), "c-9");

        // And a wrong-state flow dies rather than returning the code.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wait = tokio::spawn(async move {
            wait_for_callback(listener, "st-2", "/auth/callback").await
        });
        let mut s3 = tokio::net::TcpStream::connect(addr).await.unwrap();
        s3.write_all(b"GET /auth/callback?code=c-9&state=EVIL HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = String::new();
        let _ = s3.read_to_string(&mut buf).await;
        let err = wait.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("state mismatch"), "{err}");
    }

    // ── the device side of a link started somewhere else ──────────────────

    /// A one-shot API stand-in: records every `(path, body)` and answers each
    /// call from `replies` by METHOD NAME, defaulting to `{ok:true}`. Local
    /// rather than shared because the cli has no test harness crate and one
    /// screen of tokio is cheaper than inventing one.
    struct MockApi {
        base: String,
        seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    }

    impl MockApi {
        fn calls(&self, method: &str) -> Vec<Value> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p == &format!("/api/{method}"))
                .map(|(_, b)| b.clone())
                .collect()
        }
        /// Block until `method` has been called (or the test's patience runs
        /// out). Polling beats a sleep: the device answers in microseconds and
        /// a fixed wait would either be flaky or slow.
        async fn wait_for(&self, method: &str) -> Value {
            for _ in 0..200 {
                if let Some(b) = self.calls(method).into_iter().next() {
                    return b;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("{method} was never called: {:?}", self.seen.lock().unwrap());
        }
    }

    fn spawn_api(replies: Vec<(&'static str, Value)>) -> MockApi {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let base = format!("http://{}", std_listener.local_addr().unwrap());
        let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio");
        let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>> = Default::default();
        let sink = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let sink = sink.clone();
                let replies = replies.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let body: Value = req
                        .split_once("\r\n\r\n")
                        .and_then(|(_, b)| serde_json::from_str(b).ok())
                        .unwrap_or(Value::Null);
                    sink.lock().unwrap().push((path.clone(), body));
                    let result = replies
                        .iter()
                        .find(|(m, _)| path == format!("/api/{m}"))
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| json!({}));
                    let payload = json!({ "ok": true, "result": result }).to_string();
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                                payload.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        MockApi { base, seen }
    }

    fn a_session() -> session::Session {
        session::Session {
            token: "s_test".into(),
            username: "ops".into(),
            device_id: "d-1".into(),
            device_name: "ops-mbp".into(),
        }
    }

    fn link_frame(provider: &str) -> String {
        json!({
            "method": "events.connectionLink",
            "params": { "link_id": "11111111-1111-4111-8111-111111111111", "provider": provider },
        })
        .to_string()
    }

    /// The frames this handler must NOT touch. A device that claims events it
    /// can't finish is worse than one that ignores them: the claim is what
    /// stops another machine from doing the work.
    #[tokio::test]
    async fn an_unrelated_frame_is_ignored_without_a_claim() {
        let api = spawn_api(vec![]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        for frame in [
            r#"{"method":"events.connectionCall","params":{"call_id":"c-1"}}"#.to_string(),
            r#"{"method":"events.message","params":{}}"#.to_string(),
            "not json at all".to_string(),
        ] {
            assert!(!handle_link_event(&client, &a_session(), &umk, "k1", &frame).await);
        }
        assert!(api.calls("claimConnectionCall").is_empty());
    }

    /// Losing the claim ends it. Two laptops online must not mean two consent
    /// screens for one click.
    #[tokio::test]
    async fn losing_the_claim_stops_before_binding_anything() {
        let api = spawn_api(vec![("claimConnectionCall", json!({ "claimed": false }))]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(!handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("codex-oauth")).await);
        assert_eq!(api.calls("claimConnectionCall").len(), 1);
        assert!(api.calls("answerConnectionCall").is_empty());
    }

    /// A provider this build has never heard of still gets an ANSWER. The
    /// caller is parked on the rendezvous; a silent device turns "your Mafold
    /// is out of date" into "no machine took it", which sends the user looking
    /// at the wrong thing entirely.
    #[tokio::test]
    async fn an_unknown_provider_answers_with_words() {
        let api = spawn_api(vec![("claimConnectionCall", json!({ "claimed": true }))]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("nope-oauth")).await);
        let answer = api.wait_for("answerConnectionCall").await;
        assert!(answer["error"].as_str().unwrap().contains("nope-oauth"), "{answer}");
        assert!(answer["result"].is_null());
    }

    /// A provider linked by paste or import is not a bug either — it is a
    /// sentence about the provider, not about the machine.
    #[tokio::test]
    async fn a_pasted_provider_says_so_rather_than_binding() {
        seat_registry();
        let api = spawn_api(vec![("claimConnectionCall", json!({ "claimed": true }))]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("notion")).await);
        let answer = api.wait_for("answerConnectionCall").await;
        assert!(
            answer["error"].as_str().unwrap().contains("isn't linked by a consent screen"),
            "{answer}"
        );
    }

    /// The whole point: a codex link event comes back with a real consent URL
    /// and the machine's name, so the asking surface can send the person there
    /// and say where the sign-in is happening.
    ///
    /// The vendor's redirect port is a fixed constant (1455), so this test
    /// tolerates a machine where a real `codex login` already owns it — the
    /// handler must then answer with THAT sentence rather than go quiet.
    #[tokio::test]
    async fn a_codex_link_answers_with_a_consent_url_and_the_device_name() {
        seat_registry();
        let api = spawn_api(vec![
            ("claimConnectionCall", json!({ "claimed": true })),
            ("listConnections", json!({ "items": [] })),
        ]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let umk = Key::random();
        assert!(handle_link_event(&client, &a_session(), &umk, "k1", &link_frame("codex-oauth")).await);
        let answer = api.wait_for("answerConnectionCall").await;
        if let Some(err) = answer["error"].as_str() {
            assert!(err.contains("127.0.0.1:1455"), "unexpected failure: {err}");
            return;
        }
        let url = answer["result"]["authorize_url"].as_str().expect("a url");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"), "{url}");
        assert!(url.contains("codex_cli_simplified_flow=true"), "{url}");
        assert_eq!(answer["result"]["device"], "ops-mbp");
    }

    #[tokio::test]
    async fn reconnecting_a_machine_replaces_the_original_connection_name() {
        let api = spawn_api(vec![]);
        let client = Client::new(api.base.clone(), "s_test".into());
        let spec = provider_infos().into_iter().find(|p| p.id == "computer").unwrap();
        let name = bind_for_link(&client, &a_session(), &Key::random(), "new-key", &spec, Some("original-computer"), Some("My office computer")).await.unwrap();
        assert_eq!(name, "original-computer");
        let writes = api.calls("putConnection");
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["name"], "original-computer");
        assert_eq!(writes[0]["key_id"], "new-key");
        assert_eq!(writes[0]["label"], "My office computer");
        assert_eq!(writes[0]["create_only"], false);
        assert!(api.calls("listConnections").is_empty(), "reconnect must not choose a free name");
    }

    /// Names don't collide: a second Codex account makes a second row.
    #[tokio::test]
    async fn a_free_name_steps_around_what_is_already_linked() {
        let api = spawn_api(vec![(
            "listConnections",
            json!({ "items": [ { "name": "codex" }, { "name": "codex-2" } ] }),
        )]);
        let client = Client::new(api.base.clone(), "s_test".into());
        assert_eq!(free_name(&client, "codex-oauth").await, "codex-3");
    }
}
