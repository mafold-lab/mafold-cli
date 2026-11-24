//! Mafold Harness on its owner's ChatGPT Codex plan (ops 11-23: 「codex as
//! fuel也补充一下(connection 和 本地复用)」, flag harnessOnCodex).
//!
//! The house kernel keeps its loop — its prompt, its tools, its sessions — and
//! each model step goes to the Codex Responses API itself
//! (`mafold_harness::CodexResponsesRuntime`), not through the real `codex`:
//! its shell and patch tools can't be reliably switched off, and the harness
//! brings its own.
//!
//! Which login: the one the owner picked in Sign in with ([`Pick`], ops 11-24
//! 「我选的是local login的 … 没有任何connection应该涉足的可能」), and no other:
//! - **This computer's own Codex login** — `$CODEX_HOME/auth.json`
//!   (`~/.codex`), renewed here when it runs out and written back in Codex's
//!   own format, so `codex` itself carries on with the new one.
//! - **Their Codex connection** — the owner's newest Codex connection, opened
//!   from their vault on this computer and renewed by mafold-core
//!   (`codex::Opened`), the renewed grant written back for every device.
//!
//! An agent picked before the pick was kept runs the connection, else this
//! computer's own login, as it did then ([`Pick::Either`]).
//!
//! The daemon's fuel step (`fuel::codex_step`) opens it and hands it over in
//! the turn's env alone ([`Source::env`]), masked out of everything the turn
//! writes; the turn's runtime reads it back ([`CodexFuel::from_env`]) and
//! renews it through the same source when Codex refuses it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use base64::Engine as _;
use mafold_core::mafold_types::connections::codex;
use mafold_harness::codex::CodexModel;
use mafold_harness::{CodexAuth, CodexCredentials};
use serde_json::{json, Value};

use super::{CapsSource, HarnessProbe, ModelCap};
use crate::connection::OwnerKeyIssue;

/// The Connections provider of a Codex (ChatGPT) login.
pub const PROVIDER: &str = "codex-oauth";

/// The hand-off to the turn (`Turn.env`), every name under `MAFOLD_CODEX_`,
/// which the shell never sees (`computer::SCRUB`).
pub const TOKEN_ENV: &str = "MAFOLD_CODEX_TOKEN";
pub const ACCOUNT_ENV: &str = "MAFOLD_CODEX_ACCOUNT";
/// `connection` or `local` — where the login came from, so a refusal renews it
/// there — and what that needs: the connection's name, its owner and the api
/// it is listed on; or the `auth.json` it was read from.
const FROM_ENV: &str = "MAFOLD_CODEX_FROM";
const CONNECTION_ENV: &str = "MAFOLD_CODEX_CONNECTION";
const OWNER_ENV: &str = "MAFOLD_CODEX_OWNER";
const BASE_ENV: &str = "MAFOLD_CODEX_BASE";
const AUTH_ENV: &str = "MAFOLD_CODEX_AUTH";
/// Set on a connection that may give way to this computer's own login
/// ([`Pick::Either`]); a connection picked as such never does.
const GIVE_WAY_ENV: &str = "MAFOLD_CODEX_GIVE_WAY";

/// How long before its expiry a local access token is renewed instead of sent.
const RENEW_MARGIN_SECS: i64 = 5 * 60;

// ── where the login comes from ──────────────────────────────────────────────

/// Which Codex login the owner picked (`getBot`'s `fuel_from`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// This computer's own Codex login, and never a connection.
    Local,
    /// Their Codex connection, and never this computer's login.
    Connection,
    /// Picked before the pick was kept: the connection, else this computer's.
    Either,
}

impl Pick {
    pub fn of(from: Option<&str>) -> Self {
        use mafold_core::mafold_types::agent_setup::{FROM_CONNECTION, FROM_LOCAL};
        match from.map(str::trim) {
            Some(FROM_LOCAL) => Pick::Local,
            Some(FROM_CONNECTION) => Pick::Connection,
            _ => Pick::Either,
        }
    }
}

/// Where a turn's Codex login lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The owner's Codex connection `name`, listed on `base` (the api origin).
    /// `give_way`: when it can't be renewed, this computer's own login runs
    /// instead — only for an agent that didn't pick ([`Pick::Either`]).
    Connection { base: String, owner: String, name: String, give_way: bool },
    /// This computer's own Codex login, at `auth`.
    Local { auth: PathBuf },
}

impl Source {
    /// The turn's env for `auth` from here.
    pub fn env(&self, auth: &CodexAuth) -> Vec<(String, String)> {
        let mut env = vec![
            (TOKEN_ENV.to_string(), auth.access_token.clone()),
            (ACCOUNT_ENV.to_string(), auth.account_id.clone()),
        ];
        match self {
            Source::Connection { base, owner, name, give_way } => {
                env.extend([
                    (FROM_ENV.to_string(), "connection".to_string()),
                    (CONNECTION_ENV.to_string(), name.clone()),
                    (OWNER_ENV.to_string(), owner.clone()),
                    (BASE_ENV.to_string(), base.clone()),
                ]);
                if *give_way {
                    env.push((GIVE_WAY_ENV.to_string(), "1".to_string()));
                }
            }
            Source::Local { auth } => env.extend([
                (FROM_ENV.to_string(), "local".to_string()),
                (AUTH_ENV.to_string(), auth.display().to_string()),
            ]),
        }
        env
    }

    /// The login the daemon's fuel step put in `env`, and where it lives.
    fn from_env(env: &[(String, String)]) -> Option<(Self, CodexAuth)> {
        let get = |k: &str| env.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
        let auth = CodexAuth { access_token: get(TOKEN_ENV)?, account_id: get(ACCOUNT_ENV)? };
        let source = match get(FROM_ENV)?.as_str() {
            "connection" => Source::Connection {
                base: get(BASE_ENV)?,
                owner: get(OWNER_ENV)?,
                name: get(CONNECTION_ENV)?,
                give_way: get(GIVE_WAY_ENV).as_deref() == Some("1"),
            },
            "local" => Source::Local { auth: PathBuf::from(get(AUTH_ENV)?) },
            _ => return None,
        };
        Some((source, auth))
    }
}

/// The turn's Codex login, opened: the one `pick` names and no other — this
/// computer's own Codex login, or the owner's Codex connection. An agent that
/// didn't pick ([`Pick::Either`]) runs the connection when this computer can
/// open it and it isn't known dead, else this computer's own login. `Err` is
/// the turn's error, in the words the reply ends with.
pub async fn open(base: &str, owner: &str, pick: Pick) -> Result<(Source, CodexAuth), String> {
    if pick == Pick::Local {
        return open_local().await?.ok_or_else(|| {
            "This agent runs on this computer's own Codex login, and Codex isn't signed in here. Sign in to Codex on \
             this computer, then send this again."
                .to_string()
        });
    }
    let issue = match open_connection(base, owner, pick == Pick::Either).await {
        Ok(opened) => return Ok(opened),
        Err(issue) => issue,
    };
    if pick == Pick::Connection {
        return Err(no_login(owner, issue, false));
    }
    open_local().await?.ok_or_else(|| no_login(owner, issue, true))
}

/// This computer's own Codex login, renewed if it is about to run out;
/// `None` when Codex isn't signed in here.
async fn open_local() -> Result<Option<(Source, CodexAuth)>, String> {
    let auth = auth_path();
    match read_local(&auth) {
        Ok(Some(_)) => {
            let login = local_current(&auth, codex::TOKEN_ENDPOINT).await?;
            Ok(Some((Source::Local { auth }, login)))
        }
        Ok(None) => Ok(None),
        Err(why) => Err(format!("Couldn't read the Codex login on this computer ({}): {why}", auth.display())),
    }
}

async fn open_connection(base: &str, owner: &str, give_way: bool) -> Result<(Source, CodexAuth), OwnerKeyIssue> {
    let (rt, name, relink) = crate::connection::open_owner_login(base, owner, PROVIDER).await?;
    // Its grant is known dead (a renewal was refused and the row marked):
    // every reply would be refused and fail the same way, while the Codex
    // login this computer keeps alive runs (ops 11-24: tester4, signed in
    // with the Mac's own login, failed on a spent connection every time).
    if relink {
        return Err(OwnerKeyIssue::Failed("it needs reconnecting in Settings ▸ Connections".into()));
    }
    let opened = mafold_core::codex::Opened::open(&rt, &name).await.map_err(OwnerKeyIssue::Failed)?;
    let auth = connection_auth(&opened).map_err(OwnerKeyIssue::Failed)?;
    Ok((Source::Connection { base: base.to_string(), owner: owner.to_string(), name, give_way }, auth))
}

fn connection_auth(opened: &mafold_core::codex::Opened) -> Result<CodexAuth, String> {
    let (access, account) = (opened.access_token(), opened.account_id());
    if access.is_empty() || account.is_empty() {
        return Err("your Codex connection holds no usable login — reconnect it in Settings ▸ Connections".into());
    }
    Ok(CodexAuth { access_token: access.to_string(), account_id: account.to_string() })
}

/// What a reply ends with when there was no Codex login to run on: why the
/// connection didn't open, and — `here_too`, for an agent that would have run
/// this computer's own login instead — that this computer has none either.
fn no_login(owner: &str, issue: OwnerKeyIssue, here_too: bool) -> String {
    use OwnerKeyIssue as I;
    if !here_too {
        return match issue {
            I::None => "This agent runs on your Codex connection, and there's none yet: connect Codex in Settings ▸ \
                        Connections, then send this again."
                .into(),
            I::NotHere => format!(
                "This agent runs on @{owner}'s Codex connection, which opens only on a computer where @{owner} is \
                 signed in to Mafold, and this isn't one."
            ),
            I::NoVaultKey => "Your Codex connection is in your Connections, but this computer hasn't been handed the \
                              key that opens them yet. Open Settings ▸ Connections on a device that already has it \
                              (the web, or your phone) and it passes the key to this computer by itself, then send \
                              this again."
                .into(),
            I::Failed(why) => format!("Couldn't open your Codex connection on this computer ({why})."),
        };
    }
    let neither = "and Codex isn't signed in on this computer either";
    match issue {
        I::None => "No Codex login yet: connect Codex in Settings ▸ Connections (or sign in to Codex on this computer), \
                    then send this again."
            .into(),
        I::NotHere => format!(
            "This agent runs on @{owner}'s Codex login. Their Codex connection opens only on a computer where \
             @{owner} is signed in to Mafold, which isn't this one, {neither}."
        ),
        I::NoVaultKey => format!(
            "Your Codex connection is in your Connections, but this computer hasn't been handed the key that opens \
             them yet, {neither}. Open Settings ▸ Connections on a device that already has it (the web, or your \
             phone) and it passes the key to this computer by itself, then send this again."
        ),
        I::Failed(why) => format!("Couldn't open your Codex connection on this computer ({why}), {neither}."),
    }
}

// ── the turn's side ─────────────────────────────────────────────────────────

/// The login a turn's model steps send, renewed where it lives when Codex
/// refuses it. Every token it holds stays masked until the turn ends.
pub struct CodexFuel {
    source: Mutex<Source>,
    current: Mutex<CodexAuth>,
    masked: Mutex<Vec<crate::conceal::LiveSecret>>,
    token_endpoint: String,
}

impl CodexFuel {
    /// The login the daemon handed this turn, or `None` without one.
    pub fn from_env(env: &[(String, String)]) -> Option<Self> {
        let (source, auth) = Source::from_env(env)?;
        Some(Self { source: Mutex::new(source), current: Mutex::new(auth), masked: Mutex::new(Vec::new()), token_endpoint: codex::TOKEN_ENDPOINT.to_string() })
    }

    fn take(&self, auth: CodexAuth) -> CodexAuth {
        self.masked.lock().unwrap().push(crate::conceal::hide_while_live(&auth.access_token));
        *self.current.lock().unwrap() = auth.clone();
        auth
    }
}

#[async_trait]
impl CodexCredentials for CodexFuel {
    async fn current(&self) -> Result<CodexAuth, String> {
        let held = self.current.lock().unwrap().clone();
        // A local token can run out mid-conversation: renewed before a step
        // is sent on it, as `codex` itself would.
        let source = self.source.lock().unwrap().clone();
        if let Source::Local { auth } = source {
            if expiring(&held.access_token) {
                return local_current(&auth, &self.token_endpoint).await.map(|a| self.take(a));
            }
        }
        Ok(held)
    }

    async fn renew(&self, refused: &CodexAuth) -> Result<CodexAuth, String> {
        let source = self.source.lock().unwrap().clone();
        let fresh = match source {
            Source::Connection { base, owner, name, give_way } => match renew_connection(&base, &owner, &name, refused).await {
                Ok(fresh) => fresh,
                // Picked as the connection: it fails as itself, never on
                // another login.
                Err(why) if !give_way => return Err(why),
                // The connection's grant is dead, on an agent that didn't
                // pick: this computer's own Codex login, when it has one,
                // rather than a failed reply — the same order `open` takes.
                // Said where the daemon logs it.
                Err(why) => {
                    let auth = auth_path();
                    if !matches!(read_local(&auth), Ok(Some(_))) {
                        return Err(why);
                    }
                    let login = local_current(&auth, &self.token_endpoint).await.map_err(|_| why.clone())?;
                    println!("🔑 Codex: @{owner}'s connection couldn't be renewed ({why}); this computer's Codex login runs this reply");
                    *self.source.lock().unwrap() = Source::Local { auth };
                    login
                }
            },
            Source::Local { auth } => local_renew(&auth, &refused.access_token, &self.token_endpoint).await?,
        };
        Ok(self.take(fresh))
    }
}

/// The owner's connection renewed where it lives — or, when another device
/// renewed it already, that login as it is.
async fn renew_connection(base: &str, owner: &str, name: &str, refused: &CodexAuth) -> Result<CodexAuth, String> {
    let (rt, _, _) = crate::connection::open_owner_login(base, owner, PROVIDER).await.map_err(|issue| no_login(owner, issue, false))?;
    let mut opened = mafold_core::codex::Opened::open(&rt, name).await?;
    if opened.access_token() == refused.access_token {
        opened.renew(&rt).await?;
    }
    connection_auth(&opened)
}

// ── this computer's own Codex login ─────────────────────────────────────────

/// Where Codex keeps its login here: `$CODEX_HOME/auth.json`, else
/// `~/.codex/auth.json` — the same resolution `codex` itself does.
pub fn auth_path() -> PathBuf {
    let home = std::env::var("CODEX_HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::platform::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".codex"));
    home.join("auth.json")
}

/// Codex is signed in here with a ChatGPT login (an API key doesn't run on
/// the plan) — what the computer's report says.
pub fn signed_in_here() -> bool {
    matches!(read_local(&auth_path()), Ok(Some(_)))
}

/// `auth.json` and the login in it: `Ok(None)` when there is no file or no
/// ChatGPT login in it.
fn read_local(path: &Path) -> Result<Option<(Value, CodexAuth)>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let file: Value = serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))?;
    let tokens = &file["tokens"];
    let Some(access) = tokens["access_token"].as_str().filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    // Older files keep the account only inside the id token.
    let account = tokens["account_id"]
        .as_str()
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .or_else(|| account_in(tokens["id_token"].as_str().unwrap_or("")))
        .unwrap_or_default();
    if account.is_empty() {
        return Err("the Codex login here names no ChatGPT account — sign in to Codex again".into());
    }
    let auth = CodexAuth { access_token: access.to_string(), account_id: account };
    Ok(Some((file, auth)))
}

/// The local login to send now: as it is, or renewed first when it is about
/// to run out.
async fn local_current(path: &Path, endpoint: &str) -> Result<CodexAuth, String> {
    let Some((_, auth)) = read_local(path)? else {
        return Err("Codex isn't signed in on this computer any more — sign in again, or connect Codex in Settings ▸ Connections.".into());
    };
    if !expiring(&auth.access_token) {
        return Ok(auth);
    }
    local_renew(path, &auth.access_token, endpoint).await
}

/// Codex refused `refused`: the file's login when `codex` already renewed it,
/// else a renewal spent here and written back.
async fn local_renew(path: &Path, refused: &str, endpoint: &str) -> Result<CodexAuth, String> {
    // One renewal at a time on this computer: the daemon (the catalog read)
    // and each turn renew the same file, and a refresh token spends once —
    // the second to spend it would be refused. Whoever waited reads the
    // login the first one wrote back, below.
    let _turn = renew_lock().await;
    let Some((mut file, auth)) = read_local(path)? else {
        return Err("Codex isn't signed in on this computer any more — sign in again, or connect Codex in Settings ▸ Connections.".into());
    };
    if auth.access_token != refused && !expiring(&auth.access_token) {
        return Ok(auth);
    }
    let refresh = file["tokens"]["refresh_token"].as_str().filter(|t| !t.is_empty()).map(str::to_string).ok_or_else(|| {
        "The Codex login on this computer has run out and holds nothing to renew it with — sign in to Codex again.".to_string()
    })?;
    let grant = refresh_grant(endpoint, &refresh).await?;
    let tokens = file["tokens"].as_object_mut().ok_or("the Codex login here has no tokens")?;
    for k in ["access_token", "refresh_token", "id_token"] {
        // Rotation is optional: what didn't come back stays as it was.
        if let Some(v) = grant[k].as_str().filter(|v| !v.is_empty()) {
            tokens.insert(k.to_string(), json!(v));
        }
    }
    file["last_refresh"] = json!(now_rfc3339());
    write_back(path, &file).map_err(|e| format!("couldn't save the renewed Codex login ({}): {e}", path.display()))?;
    read_local(path)?.map(|(_, a)| a).ok_or_else(|| "the renewed Codex login didn't keep".into())
}

/// Held while this computer renews its Codex login: an OS lock on a file of
/// Mafold's own (never one in Codex's folder), so it holds across the daemon
/// and its turns. `None` where it can't be taken: the renewal runs as it did
/// before the lock.
async fn renew_lock() -> Option<std::fs::File> {
    let lock = catalog_path().with_file_name("codex-renew.lock");
    tokio::task::spawn_blocking(move || {
        if let Some(dir) = lock.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock).ok()?;
        file.lock().ok()?;
        Some(file)
    })
    .await
    .ok()
    .flatten()
}

/// Spend `refresh` at Codex's token endpoint, the way `codex` does. A refused
/// grant is a sign-in to do again; anything else is said as it failed.
async fn refresh_grant(endpoint: &str, refresh: &str) -> Result<Value, String> {
    let body = json!({
        "client_id": codex::CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "scope": "openid profile email",
    });
    let resp = reqwest::Client::new()
        .post(endpoint)
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("couldn't reach Codex to renew its login: {}", e.without_url()))?;
    let status = resp.status();
    let grant: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() && grant["access_token"].as_str().is_some_and(|t| !t.is_empty()) {
        return Ok(grant);
    }
    if status.as_u16() == 400 || status.as_u16() == 401 {
        return Err("The Codex login on this computer was signed out or used elsewhere — sign in to Codex again, \
                    or connect Codex in Settings ▸ Connections."
            .into());
    }
    Err(format!("Codex didn't renew its login here (HTTP {status})"))
}

/// Replace `path` with `file` at once — a temp file beside it, then a rename
/// — so `codex` never reads half a login. Owner-only, like Codex writes it.
fn write_back(path: &Path, file: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(file)?;
    let tmp = path.with_file_name(format!(".auth.json.mafold-{}", std::process::id()));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    chrono::DateTime::from_timestamp(now.as_secs() as i64, now.subsec_nanos())
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
        .unwrap_or_default()
}

/// A JWT's claims, unverified — read only for what this login says about
/// itself (when it runs out, which account it is for).
fn claims(jwt: &str) -> Option<Value> {
    let part = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The ChatGPT account an id token is for.
fn account_in(id_token: &str) -> Option<String> {
    claims(id_token)?["https://api.openai.com/auth"]["chatgpt_account_id"].as_str().map(str::to_string)
}

/// Is this access token at or near its expiry? One that says nothing about it
/// is not: Codex answers for itself, and a refusal renews it then.
fn expiring(access: &str) -> bool {
    let Some(exp) = claims(access).and_then(|c| c["exp"].as_i64()) else {
        return false;
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    now + RENEW_MARGIN_SECS >= exp
}

// ── models ──────────────────────────────────────────────────────────────────

/// The models a Codex login runs, for New Agent's picker and the turn's own
/// line about its model: the catalog as last read here ([`probe`]), else the
/// roster built in.
pub fn models() -> Vec<Value> {
    known().iter().map(|m| json!({ "id": m.id, "name": m.label })).collect()
}

/// The roster built in, for a computer that hasn't read the catalog yet.
fn roster() -> Vec<CodexModel> {
    codex::SUBSCRIPTION_MODELS
        .iter()
        .map(|m| CodexModel {
            id: m.id.to_string(),
            label: m.label.to_string(),
            description: None,
            efforts: m.efforts.iter().map(|e| e.to_string()).collect(),
        })
        .collect()
}

/// Where the last catalog read is kept on this computer: one file, since the
/// catalog is the login's and every agent here on Codex shares it.
fn catalog_path() -> PathBuf {
    crate::platform::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mafold")
        .join("runtimes")
        .join("mafold-harness")
        .join("codex-models.json")
}

/// The catalog as last read here, else the roster. Codex picks the models
/// by the client version that asks, so a read made as another version isn't
/// this one's: one kept as a bare list predates the version being kept
/// (ops 11-24: cli 0.9.173 asked as Codex 0.146 and got only GPT-5.6).
fn known() -> Vec<CodexModel> {
    std::fs::read(catalog_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<KeptCatalog>(&b).ok())
        .filter(|k| k.client_version == codex::CLIENT_VERSION && !k.models.is_empty())
        .map(|k| k.models)
        .unwrap_or_else(roster)
}

/// The catalog as kept here, with the Codex version it was asked as.
#[derive(serde::Serialize, serde::Deserialize)]
struct KeptCatalog {
    client_version: String,
    models: Vec<CodexModel>,
}

fn keep(models: &[CodexModel]) {
    let path = catalog_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let kept = KeptCatalog { client_version: codex::CLIENT_VERSION.to_string(), models: models.to_vec() };
    if let Ok(bytes) = serde_json::to_vec(&kept) {
        let _ = std::fs::write(&path, bytes);
    }
}

/// How long a catalog read here stands before New Agent's report reads it
/// again ([`refresh_if_stale`]).
const CATALOG_STALE_SECS: u64 = 3600;

/// Read the catalog again in the background with this computer's own Codex
/// login, when the last read here is missing or old: New Agent's picker
/// ([`models`]) comes from this computer's report before any agent here runs
/// on Codex to read it. One read at a time.
pub fn refresh_if_stale() {
    static READING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let fresh = std::fs::metadata(catalog_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < CATALOG_STALE_SECS);
    if fresh || !signed_in_here() || READING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async {
        if let Some(auth) = catalog_login("", None).await {
            if let Ok(m) = mafold_harness::codex::list_models(&auth).await {
                keep(&m);
            }
        }
        READING.store(false, std::sync::atomic::Ordering::SeqCst);
    });
}

/// The login the catalog is read with: the one a turn would run on — the
/// owner's Codex connection, else this computer's own login (with no owner
/// known yet, this computer's own).
///
/// The connection as it is stored, never renewed: a renewal spends its
/// refresh token, and a reply on another device renewing at that moment
/// would then be refused (ops 11-24: tester4's first reply failed so); at or
/// near expiry it is `None`, and the next reply renews it. This computer's
/// own login is renewed as a turn renews it, one renewal at a time here
/// ([`renew_lock`]): left as it was, a login nothing here had used for a
/// while kept the menu on its last read for good (ops 11-24, tester5: only
/// GPT-5.6, from a read made as an older Codex).
async fn catalog_login(base: &str, owner: Option<&str>) -> Option<CodexAuth> {
    // No connection this computer can open: a turn runs on the computer's
    // own login then, and so does the read.
    if let Some(owner) = owner.filter(|o| !o.is_empty()) {
        if let Ok((rt, name, false)) = crate::connection::open_owner_login(base, owner, PROVIDER).await {
            let opened = mafold_core::codex::Opened::peek(&rt, &name).await.ok()??;
            return connection_auth(&opened).ok().filter(|a| !expiring(&a.access_token));
        }
    }
    match local_current(&auth_path(), codex::TOKEN_ENDPOINT).await {
        Ok(auth) => Some(auth),
        Err(why) => {
            println!("⚠️ Codex's model list couldn't be read with this computer's Codex login ({why}); the menu keeps the last one known here");
            None
        }
    }
}

/// The model a turn runs: the one it was given (with or without a `codex/`
/// prefix), else Codex's default.
pub fn codex_model(wanted: Option<&str>) -> String {
    wanted
        .map(|m| m.trim().trim_start_matches("codex/").to_string())
        .filter(|m| !m.is_empty() && !m.contains('/'))
        .unwrap_or_else(|| codex::DEFAULT_MODEL.to_string())
}

/// The Customize sheet's model menu for an agent on Codex: Codex's own
/// catalog for the login a turn would use, each model with its efforts (the
/// runtime sends the one picked as `reasoning.effort`) — kept here for the
/// next time it can't be read. Unread, the last read here or the roster
/// built in, said as `Cached`: a menu, never the claim that this is all the
/// login runs.
pub async fn probe(base: &str, owner: Option<&str>) -> Option<HarnessProbe> {
    let read = match catalog_login(base, owner).await {
        Some(auth) => match mafold_harness::codex::list_models(&auth).await {
            Ok(m) => Some(m),
            Err(why) => {
                println!("⚠️ Codex's model list couldn't be read ({why}); the menu keeps the last one known here");
                None
            }
        },
        None => None,
    };
    match read {
        Some(m) => {
            keep(&m);
            report(m, CapsSource::Handshake)
        }
        None => report(known(), CapsSource::Cached),
    }
}

/// `models` as a caps report, each answering to its `codex/` spelling too.
fn report(models: Vec<CodexModel>, source: CapsSource) -> Option<HarnessProbe> {
    let models: Vec<ModelCap> = models
        .into_iter()
        .map(|m| ModelCap {
            aliases: vec![format!("codex/{}", m.id)],
            id: m.id,
            display: m.label,
            resolved: None,
            description: m.description,
            efforts: m.efforts,
        })
        .collect();
    (!models.is_empty()).then(|| HarnessProbe { source, models, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: Value) -> String {
        let enc = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!("{}.{}.sig", enc(br#"{"alg":"none"}"#), enc(claims.to_string().as_bytes()))
    }

    fn now() -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    }

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mafold-codex-fuel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("auth.json")
    }

    /// A stand-in token endpoint: answers once with `status` and `body`, and
    /// keeps the request body it got.
    async fn token_endpoint(status: &str, body: &str) -> (String, std::sync::Arc<Mutex<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(Mutex::new(String::new()));
        let kept = seen.clone();
        let reply = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut req = Vec::new();
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    if rest.len() >= len {
                        *kept.lock().unwrap() = rest.to_string();
                        break;
                    }
                }
            }
            let _ = sock.write_all(reply.as_bytes()).await;
        });
        (url, seen)
    }

    #[test]
    fn the_login_and_its_account_are_read_from_codexs_own_file() {
        let path = home("read");
        assert!(matches!(read_local(&path), Ok(None)), "no file: not signed in");
        std::fs::write(&path, r#"{"OPENAI_API_KEY":"sk-fake","tokens":null}"#).unwrap();
        assert!(matches!(read_local(&path), Ok(None)), "an API key doesn't run on the plan");
        std::fs::write(&path, r#"{"tokens":{"access_token":"at-fake","refresh_token":"rt-fake","account_id":"acct-1"}}"#).unwrap();
        let (_, auth) = read_local(&path).unwrap().unwrap();
        assert_eq!(auth, CodexAuth { access_token: "at-fake".into(), account_id: "acct-1".into() });
        // An older file keeps the account inside the id token only.
        let id = jwt(json!({ "https://api.openai.com/auth": { "chatgpt_account_id": "acct-2" } }));
        std::fs::write(&path, json!({ "tokens": { "access_token": "at-fake", "id_token": id } }).to_string()).unwrap();
        assert_eq!(read_local(&path).unwrap().unwrap().1.account_id, "acct-2");
    }

    #[test]
    fn a_token_near_its_expiry_is_renewed_and_one_without_one_is_not() {
        assert!(expiring(&jwt(json!({ "exp": now() + 60 }))));
        assert!(!expiring(&jwt(json!({ "exp": now() + 3600 }))));
        assert!(!expiring("opaque-token"), "says nothing: Codex answers for itself");
    }

    /// A renewal is written back in Codex's own format: the new tokens in
    /// place, everything else in the file — fields this build has never
    /// heard of included — kept as it was.
    #[tokio::test]
    async fn a_refused_local_login_is_renewed_and_written_back_whole() {
        let path = home("renew");
        let old = jwt(json!({ "exp": now() + 3600 }));
        let file = json!({
            "OPENAI_API_KEY": null,
            "auth_mode": "chatgpt",
            "tokens": { "access_token": old, "refresh_token": "rt-fake-old", "id_token": "id-old", "account_id": "acct-1" },
            "last_refresh": "2026-11-01T00:00:00Z",
            "something_new": { "kept": true },
        });
        std::fs::write(&path, file.to_string()).unwrap();
        let (url, seen) = token_endpoint("200 OK", r#"{"access_token":"at-fake-new","refresh_token":"rt-fake-new","id_token":"id-new"}"#).await;
        let auth = local_renew(&path, &old, &url).await.unwrap();
        assert_eq!(auth.access_token, "at-fake-new");
        let sent: Value = serde_json::from_str(&seen.lock().unwrap()).unwrap();
        assert_eq!((sent["grant_type"].as_str(), sent["refresh_token"].as_str()), (Some("refresh_token"), Some("rt-fake-old")));
        assert_eq!(sent["client_id"], codex::CLIENT_ID);
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["tokens"]["access_token"], "at-fake-new");
        assert_eq!(back["tokens"]["refresh_token"], "rt-fake-new");
        assert_eq!(back["tokens"]["account_id"], "acct-1");
        assert_eq!(back["auth_mode"], "chatgpt");
        assert_eq!(back["something_new"]["kept"], true);
        assert_ne!(back["last_refresh"], "2026-11-01T00:00:00Z");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten().filter(|e| e.file_name() != "auth.json").collect();
        assert!(leftovers.is_empty(), "no temp file left behind");
    }

    /// Two renewals at once here (the catalog read and a turn): one spends
    /// the refresh token, the other takes what it wrote back. The stand-in
    /// endpoint answers once; a second spend would find nobody there.
    #[tokio::test]
    async fn two_renewals_at_once_spend_the_refresh_token_once() {
        let path = home("twice");
        let old = jwt(json!({ "exp": now() + 60 }));
        std::fs::write(&path, json!({ "tokens": { "access_token": old, "refresh_token": "rt-fake", "account_id": "a" } }).to_string()).unwrap();
        let (url, _) = token_endpoint("200 OK", r#"{"access_token":"at-fake-new","refresh_token":"rt-fake-new"}"#).await;
        let (a, b) = tokio::join!(local_renew(&path, &old, &url), local_renew(&path, &old, &url));
        assert_eq!((a.unwrap().access_token, b.unwrap().access_token), ("at-fake-new".to_string(), "at-fake-new".to_string()));
    }

    /// `codex` renewed it already: that one, and nothing spent.
    #[tokio::test]
    async fn a_login_codex_already_renewed_is_taken_as_it_is() {
        let path = home("taken");
        let fresh = jwt(json!({ "exp": now() + 3600 }));
        std::fs::write(&path, json!({ "tokens": { "access_token": fresh, "refresh_token": "rt-fake", "account_id": "a" } }).to_string()).unwrap();
        let auth = local_renew(&path, "at-fake-refused", "http://127.0.0.1:9/never").await.unwrap();
        assert_eq!(auth.access_token, fresh);
    }

    #[tokio::test]
    async fn a_signed_out_login_says_sign_in_again() {
        let path = home("gone");
        std::fs::write(&path, r#"{"tokens":{"access_token":"at-fake","refresh_token":"rt-fake","account_id":"a"}}"#).unwrap();
        let (url, _) = token_endpoint("400 Bad Request", r#"{"error":"invalid_grant"}"#).await;
        let why = local_renew(&path, "at-fake", &url).await.unwrap_err();
        assert!(why.contains("sign in to Codex again"), "{why}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("at-fake"), "the file is left as it was");
    }

    /// ops 11-24 「我选的是local login的」: what was picked is what runs; only
    /// an agent from before the pick was kept may use either.
    #[test]
    fn the_pick_is_read_as_picked() {
        assert_eq!(Pick::of(Some("local")), Pick::Local);
        assert_eq!(Pick::of(Some("connection")), Pick::Connection);
        assert_eq!(Pick::of(None), Pick::Either);
        assert_eq!(Pick::of(Some("vault")), Pick::Either);
    }

    /// The hand-off survives the trip through the turn's env, both ways.
    #[test]
    fn the_login_rides_in_the_turns_env_and_back() {
        let auth = CodexAuth { access_token: "at-fake".into(), account_id: "acct-1".into() };
        for source in [
            Source::Connection { base: "https://api.example".into(), owner: "opsdu".into(), name: "codex".into(), give_way: false },
            Source::Connection { base: "https://api.example".into(), owner: "opsdu".into(), name: "codex".into(), give_way: true },
            Source::Local { auth: PathBuf::from("/home/x/.codex/auth.json") },
        ] {
            let env = source.env(&auth);
            assert!(env.iter().all(|(k, _)| k.starts_with("MAFOLD_CODEX_")), "the shell never sees any of it");
            let fuel = CodexFuel::from_env(&env).expect("read back");
            assert_eq!(*fuel.source.lock().unwrap(), source);
            assert_eq!(*fuel.current.lock().unwrap(), auth);
        }
        assert!(CodexFuel::from_env(&[(TOKEN_ENV.into(), "at".into())]).is_none(), "half a hand-off is none");
    }

    #[test]
    fn the_roster_holds_the_default_model() {
        let ids: Vec<String> = roster().into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&codex::DEFAULT_MODEL.to_string()));
        assert_eq!(codex_model(None), codex::DEFAULT_MODEL);
        assert_eq!(codex_model(Some("codex/gpt-5.6-luna")), "gpt-5.6-luna");
        assert_eq!(report(roster(), CapsSource::Cached).unwrap().models.len(), ids.len());
    }

    /// What the catalog said goes up as it said it — models the roster never
    /// had among them (ops 11-24: 「有反馈是只有5.6的」) — each answering to its
    /// `codex/` spelling.
    #[test]
    fn the_report_is_the_catalog_s() {
        let read = vec![CodexModel { id: "gpt-6.2-sol".into(), label: "GPT-6.2 Sol".into(), description: None, efforts: vec!["low".into(), "ultra".into()] }];
        let caps = report(read, CapsSource::Handshake).unwrap();
        assert_eq!(caps.source, CapsSource::Handshake);
        assert_eq!(caps.models[0].id, "gpt-6.2-sol");
        assert_eq!(caps.models[0].aliases, ["codex/gpt-6.2-sol"]);
        assert_eq!(caps.models[0].efforts, ["low", "ultra"]);
        assert!(report(Vec::new(), CapsSource::Cached).is_none(), "nothing read, nothing claimed");
    }

    /// 11-24 ops: 「mafold harness codex的effort无法切换」 — the report carried
    /// no efforts, so the sheet had no dial to offer.
    #[test]
    fn the_report_carries_each_models_efforts() {
        let caps = report(roster(), CapsSource::Cached).unwrap();
        let sol = caps.models.iter().find(|m| m.id == "gpt-5.6-sol").unwrap();
        assert_eq!(sol.efforts, ["low", "medium", "high", "xhigh", "max", "ultra"]);
        let luna = caps.models.iter().find(|m| m.id == "gpt-5.6-luna").unwrap();
        assert!(luna.efforts.contains(&"xhigh".to_string()) && !luna.efforts.contains(&"ultra".to_string()));
    }
}
