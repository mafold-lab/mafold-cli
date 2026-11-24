//! Mafold Harness on its owner's ChatGPT Codex plan (ops 11-23: 「codex as
//! fuel也补充一下(connection 和 本地复用)」, flag harnessOnCodex).
//!
//! The house kernel keeps its loop — its prompt, its tools, its sessions — and
//! each model step goes to the Codex Responses API itself
//! (`mafold_harness::CodexResponsesRuntime`), not through the real `codex`:
//! its shell and patch tools can't be reliably switched off, and the harness
//! brings its own.
//!
//! Which login, in this order ([`open`]):
//! - **Connections first** — the owner's newest Codex connection, opened from
//!   their vault on this computer and renewed by mafold-core
//!   (`codex::Opened`), the renewed grant written back for every device.
//! - **Else this computer's own Codex login** — `$CODEX_HOME/auth.json`
//!   (`~/.codex`), renewed here when it runs out and written back in Codex's
//!   own format, so `codex` itself carries on with the new one.
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

/// How long before its expiry a local access token is renewed instead of sent.
const RENEW_MARGIN_SECS: i64 = 5 * 60;

// ── where the login comes from ──────────────────────────────────────────────

/// Where a turn's Codex login lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The owner's Codex connection `name`, listed on `base` (the api origin).
    Connection { base: String, owner: String, name: String },
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
            Source::Connection { base, owner, name } => env.extend([
                (FROM_ENV.to_string(), "connection".to_string()),
                (CONNECTION_ENV.to_string(), name.clone()),
                (OWNER_ENV.to_string(), owner.clone()),
                (BASE_ENV.to_string(), base.clone()),
            ]),
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
            "connection" => Source::Connection { base: get(BASE_ENV)?, owner: get(OWNER_ENV)?, name: get(CONNECTION_ENV)? },
            "local" => Source::Local { auth: PathBuf::from(get(AUTH_ENV)?) },
            _ => return None,
        };
        Some((source, auth))
    }
}

/// The turn's Codex login, opened: the owner's Codex connection when they
/// have one this computer can open, else this computer's own Codex login.
/// `Err` is the turn's error, in the words the reply ends with.
pub async fn open(base: &str, owner: &str) -> Result<(Source, CodexAuth), String> {
    let issue = match open_connection(base, owner).await {
        Ok(opened) => return Ok(opened),
        Err(issue) => issue,
    };
    let auth = auth_path();
    match read_local(&auth) {
        Ok(Some(_)) => {
            let login = local_current(&auth, codex::TOKEN_ENDPOINT).await?;
            Ok((Source::Local { auth }, login))
        }
        Ok(None) => Err(no_login(owner, issue)),
        Err(why) => Err(format!("Couldn't read the Codex login on this computer ({}): {why}", auth.display())),
    }
}

async fn open_connection(base: &str, owner: &str) -> Result<(Source, CodexAuth), OwnerKeyIssue> {
    let (rt, name) = crate::connection::open_owner_login(base, owner, PROVIDER).await?;
    let opened = mafold_core::codex::Opened::open(&rt, &name).await.map_err(OwnerKeyIssue::Failed)?;
    let auth = connection_auth(&opened).map_err(OwnerKeyIssue::Failed)?;
    Ok((Source::Connection { base: base.to_string(), owner: owner.to_string(), name }, auth))
}

fn connection_auth(opened: &mafold_core::codex::Opened) -> Result<CodexAuth, String> {
    let (access, account) = (opened.access_token(), opened.account_id());
    if access.is_empty() || account.is_empty() {
        return Err("your Codex connection holds no usable login — reconnect it in Settings ▸ Connections".into());
    }
    Ok(CodexAuth { access_token: access.to_string(), account_id: account.to_string() })
}

/// What a reply ends with when there was no Codex login to run on: why the
/// connection didn't open, and that this computer has none of its own either.
fn no_login(owner: &str, issue: OwnerKeyIssue) -> String {
    use OwnerKeyIssue as I;
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
    source: Source,
    current: Mutex<CodexAuth>,
    masked: Mutex<Vec<crate::conceal::LiveSecret>>,
    token_endpoint: String,
}

impl CodexFuel {
    /// The login the daemon handed this turn, or `None` without one.
    pub fn from_env(env: &[(String, String)]) -> Option<Self> {
        let (source, auth) = Source::from_env(env)?;
        Some(Self { source, current: Mutex::new(auth), masked: Mutex::new(Vec::new()), token_endpoint: codex::TOKEN_ENDPOINT.to_string() })
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
        if let Source::Local { auth } = &self.source {
            if expiring(&held.access_token) {
                return local_current(auth, &self.token_endpoint).await.map(|a| self.take(a));
            }
        }
        Ok(held)
    }

    async fn renew(&self, refused: &CodexAuth) -> Result<CodexAuth, String> {
        let fresh = match &self.source {
            Source::Connection { base, owner, name } => {
                let (rt, _) = crate::connection::open_owner_login(base, owner, PROVIDER)
                    .await
                    .map_err(|issue| no_login(owner, issue))?;
                let mut opened = mafold_core::codex::Opened::open(&rt, name).await?;
                // Another device may have renewed it already: that one, as it is.
                if opened.access_token() == refused.access_token {
                    opened.renew(&rt).await?;
                }
                connection_auth(&opened)?
            }
            Source::Local { auth } => local_renew(auth, &refused.access_token, &self.token_endpoint).await?,
        };
        Ok(self.take(fresh))
    }
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

/// The models a Codex login runs, as the codex-oauth provider lists them.
pub fn models() -> Vec<Value> {
    codex::SUBSCRIPTION_MODELS.iter().map(|m| json!({ "id": m.id, "name": m.label })).collect()
}

/// The model a turn runs: the one it was given (with or without a `codex/`
/// prefix), else Codex's default.
pub fn codex_model(wanted: Option<&str>) -> String {
    wanted
        .map(|m| m.trim().trim_start_matches("codex/").to_string())
        .filter(|m| !m.is_empty() && !m.contains('/'))
        .unwrap_or_else(|| codex::DEFAULT_MODEL.to_string())
}

/// [`models`] as a caps report: the Customize sheet's model menu for an agent
/// on Codex, with each model's efforts so the sheet can offer the dial (the
/// runtime already sends the one picked as `reasoning.effort`).
pub fn probe() -> Option<HarnessProbe> {
    let models: Vec<ModelCap> = codex::SUBSCRIPTION_MODELS
        .iter()
        .map(|m| ModelCap {
            id: m.id.to_string(),
            aliases: vec![format!("codex/{}", m.id)],
            display: m.label.to_string(),
            resolved: None,
            description: None,
            efforts: m.efforts.iter().map(|e| e.to_string()).collect(),
        })
        .collect();
    (!models.is_empty()).then(|| HarnessProbe { source: CapsSource::Handshake, models, ..Default::default() })
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

    /// The hand-off survives the trip through the turn's env, both ways.
    #[test]
    fn the_login_rides_in_the_turns_env_and_back() {
        let auth = CodexAuth { access_token: "at-fake".into(), account_id: "acct-1".into() };
        for source in [
            Source::Connection { base: "https://api.example".into(), owner: "opsdu".into(), name: "codex".into() },
            Source::Local { auth: PathBuf::from("/home/x/.codex/auth.json") },
        ] {
            let env = source.env(&auth);
            assert!(env.iter().all(|(k, _)| k.starts_with("MAFOLD_CODEX_")), "the shell never sees any of it");
            let fuel = CodexFuel::from_env(&env).expect("read back");
            assert_eq!(fuel.source, source);
            assert_eq!(*fuel.current.lock().unwrap(), auth);
        }
        assert!(CodexFuel::from_env(&[(TOKEN_ENV.into(), "at".into())]).is_none(), "half a hand-off is none");
    }

    #[test]
    fn the_model_menu_is_the_codex_providers() {
        let ids: Vec<String> = models().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
        assert!(ids.contains(&codex::DEFAULT_MODEL.to_string()));
        assert_eq!(codex_model(None), codex::DEFAULT_MODEL);
        assert_eq!(codex_model(Some("codex/gpt-5.6-luna")), "gpt-5.6-luna");
        assert_eq!(probe().unwrap().models.len(), ids.len());
    }

    /// 11-24 ops: 「mafold harness codex的effort无法切换」 — the report carried
    /// no efforts, so the sheet had no dial to offer.
    #[test]
    fn the_report_carries_each_models_efforts() {
        let caps = probe().unwrap();
        let sol = caps.models.iter().find(|m| m.id == "gpt-5.6-sol").unwrap();
        assert_eq!(sol.efforts, ["low", "medium", "high", "xhigh", "max", "ultra"]);
        let luna = caps.models.iter().find(|m| m.id == "gpt-5.6-luna").unwrap();
        assert!(luna.efforts.contains(&"xhigh".to_string()) && !luna.efforts.contains(&"ultra".to_string()));
    }
}
