//! `mafold add <bot>` with no token: the machine's own login fetches it.
//!
//! A bot's token used to reach its owner's machine one way: pasted. The first
//! message a new self-hosted bot sent was an install command with the token
//! in it — stored in the chat, synced to every device, forwardable — and on
//! 2026-09-28 a sweep found 351 bot tokens sitting in message bodies. Messages
//! no longer carry one (the api masks any it sees). So the owner signs this
//! machine in once (`mafold login`, one click on the web to approve) and names
//! the bot; the token comes from `getBotToken`, which answers the bot's owner
//! and nobody else, over the owner's own session.

use crate::client::Client;
use crate::session::Session;
use anyhow::Result;

/// Which of this machine's logins speaks for `bot`: the one `--account`
/// names, else the bot's owner — a bot's username is `owner:label` — else the
/// current login, and the server says no if that isn't its owner either.
pub fn login_for<'a>(bot: &str, account: Option<&str>, logins: &'a [Session]) -> Option<&'a Session> {
    let named = |u: &str| logins.iter().find(|s| s.username.eq_ignore_ascii_case(u));
    if let Some(a) = account.map(str::trim).filter(|a| !a.is_empty()) {
        return named(a);
    }
    bot.split_once(':')
        .and_then(|(owner, _)| named(owner))
        // `session::all()` lists the current login first.
        .or_else(|| logins.first())
}

/// `bot`'s token, asked of the server as `login`.
pub async fn fetch(base: &str, bot: &str, login: &Session) -> Result<String> {
    let r = Client::new(base.to_string(), login.token.clone())
        .call("getBotToken", serde_json::json!({ "username": bot }))
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "couldn't get @{bot}'s token as @{} ({e:#}) — only the bot's owner can; \
                 `mafold login` as them, or pass --token",
                login.username
            )
        })?;
    r["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("@{bot} has no token yet — open it once in the Mafold app"))
}

/// The agent `mafold add <name> --fuel …` is about: `name` as typed — a label
/// (`helper`), or a whole username (`ops:helper`) — under `login`'s namespace,
/// the only one `createBot` makes agents in.
pub fn username_for(name: &str, login: &str) -> String {
    let name = name.trim().trim_start_matches('@').to_lowercase();
    if name.contains(':') {
        name
    } else {
        format!("{}:{name}", login.to_lowercase())
    }
}

/// `createBot`'s body for `mafold add <name> --harness <h> --fuel <f> [--model
/// <m>]`: what New Agent sends for a computer, minus the device — this
/// machine registers the daemon itself, right after (`supervisor::add`), so
/// nothing is queued for a supervisor to claim a second time.
pub fn create_body(name: &str, harness: Option<&str>, fuel: &str, model: Option<&str>) -> serde_json::Value {
    let name = name.trim().trim_start_matches('@');
    let label = name.split_once(':').map_or(name, |(_, l)| l).trim();
    let mut body = serde_json::json!({ "label": label, "sign_in": fuel.trim() });
    if let Some(h) = harness.map(str::trim).filter(|h| !h.is_empty()) {
        body["harness"] = serde_json::json!(h);
    }
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        body["config"] = serde_json::json!({ "model": m });
    }
    body
}

/// The api's numeric `error_code` for a refusal, if that's what `e` is.
fn api_code(e: &anyhow::Error) -> Option<u64> {
    match e.downcast_ref::<mafold_core::RpcError>()? {
        mafold_core::RpcError::Api(env) => serde_json::from_str::<serde_json::Value>(env).ok()?["error_code"].as_u64(),
        _ => None,
    }
}

/// `mafold add <name> --fuel <f>`: create the agent as `login` when there is
/// none by that name yet (the terminal's road to what New Agent does on the
/// web). Answers the agent's username and, when it was just made, its token.
/// An agent that exists keeps what it runs on: `--fuel` is ignored with a
/// note — changing it later is the Customize sheet's job, not this command's.
pub async fn create_if_missing(
    base: &str,
    name: &str,
    harness: Option<&str>,
    fuel: &str,
    model: Option<&str>,
    login: &Session,
) -> Result<(String, Option<String>)> {
    let username = username_for(name, &login.username);
    if !username.split_once(':').is_some_and(|(owner, _)| owner.eq_ignore_ascii_case(&login.username)) {
        anyhow::bail!(
            "`--fuel` creates agents under your own name (@{}:…), and @{username} isn't — leave out the owner, or `mafold login` as them",
            login.username
        );
    }
    let client = Client::new(base.to_string(), login.token.clone());
    match client.call("createBot", create_body(name, harness, fuel, model)).await {
        Ok(v) => {
            let made = v["bot"]["username"].as_str().filter(|u| !u.is_empty()).unwrap_or(&username).to_string();
            println!("✓ created @{made}, signed in with {}", fuel.trim());
            Ok((made, v["token"].as_str().filter(|t| !t.is_empty()).map(str::to_string)))
        }
        Err(e) if api_code(&e) == Some(409) => {
            println!("@{username} already exists — --fuel is ignored (what it runs on changes in its Customize sheet)");
            Ok((username, None))
        }
        Err(e) => Err(anyhow::anyhow!("couldn't create @{username} as @{} ({e:#})", login.username)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// `mafold add wallet --harness opencode --fuel wallet --model …` asks
    /// for exactly what New Agent would — and nothing it didn't say.
    #[test]
    fn the_arguments_are_the_create_bot_body() {
        assert_eq!(
            create_body("wallet", Some("opencode"), "wallet", Some("deepseek-v4-flash")),
            serde_json::json!({"label":"wallet","harness":"opencode","sign_in":"wallet","config":{"model":"deepseek-v4-flash"}})
        );
        assert_eq!(
            create_body("@ops:helper ", None, " wallet ", Some("  ")),
            serde_json::json!({"label":"helper","sign_in":"wallet"}),
            "the owner part is the login's, a blank model is no model"
        );
        assert_eq!(username_for("Helper", "OPS"), "ops:helper");
        assert_eq!(username_for("@ops:helper", "ops"), "ops:helper");
    }

    #[tokio::test]
    async fn a_new_agent_is_created_as_its_owner_and_its_token_kept() {
        let (base, seen) = one_shot(
            "200 OK",
            r#"{"ok":true,"result":{"bot":{"username":"ops:wallet"},"token":"mb_0000000000000000000000000000beef"}}"#.into(),
        )
        .await;
        let got = create_if_missing(&base, "wallet", Some("opencode"), "wallet", None, &login("ops")).await.unwrap();
        assert_eq!(got, ("ops:wallet".to_string(), Some("mb_0000000000000000000000000000beef".to_string())));
        let req = seen.await.unwrap();
        assert!(req.starts_with("POST /api/createBot"), "{req}");
        assert!(req.to_lowercase().contains("authorization: bearer s_ops"), "{req}");
        assert!(req.contains(r#""sign_in":"wallet""#) && req.contains(r#""harness":"opencode""#), "{req}");
    }

    /// One that exists keeps what it runs on: no error, no token — the
    /// caller fetches it as for any `mafold add`.
    #[tokio::test]
    async fn an_existing_agent_is_left_as_it_is() {
        let (base, _seen) = one_shot(
            "409 Conflict",
            r#"{"ok":false,"error_code":409,"description":"already exists: ops:wallet"}"#.into(),
        )
        .await;
        let got = create_if_missing(&base, "ops:wallet", None, "wallet", None, &login("ops")).await.unwrap();
        assert_eq!(got, ("ops:wallet".to_string(), None));
    }

    #[tokio::test]
    async fn someone_elses_namespace_is_not_created_in() {
        let e = create_if_missing("http://127.0.0.1:9", "zed:bot", None, "wallet", None, &login("ops")).await.unwrap_err();
        assert!(format!("{e:#}").contains("your own name"), "{e:#}");
    }

    fn login(u: &str) -> Session {
        Session { token: format!("s_{u}"), username: u.into(), device_id: "d1".into(), device_name: "mac".into() }
    }

    /// A machine can hold several logins; the bot's owner is the one whose
    /// word the server takes for its token.
    #[test]
    fn the_bots_owner_speaks_for_it_whichever_login_is_current() {
        let logins = [login("alice"), login("ops")];
        assert_eq!(login_for("ops:helper", None, &logins).unwrap().username, "ops");
        assert_eq!(login_for("OPS:Helper", None, &logins).unwrap().username, "ops", "case-insensitive");
        assert_eq!(login_for("ops:helper", Some("alice"), &logins).unwrap().username, "alice", "--account wins");
        // Nobody here owns it: the current login asks, and the server decides.
        assert_eq!(login_for("zed:bot", None, &logins).unwrap().username, "alice");
        assert!(login_for("ops:helper", None, &[]).is_none(), "no login at all");
        assert!(login_for("ops:helper", Some("carol"), &logins).is_none(), "--account names nobody here");
    }

    /// One HTTP exchange from a stand-in api: records the request line + auth
    /// header, answers `status` with `body`.
    async fn one_shot(status: &'static str, body: String) -> (String, tokio::task::JoinHandle<String>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).await.unwrap();
            req
        });
        (base, h)
    }

    #[tokio::test]
    async fn the_token_comes_from_get_bot_token_over_the_owners_session() {
        let (base, seen) = one_shot(
            "200 OK",
            r#"{"ok":true,"result":{"username":"ops:helper","token":"mb_0badc0de0badc0de0badc0de0badc0de"}}"#.into(),
        )
        .await;
        let tok = fetch(&base, "ops:helper", &login("ops")).await.expect("the owner gets its token");
        assert_eq!(tok, "mb_0badc0de0badc0de0badc0de0badc0de");
        let req = seen.await.unwrap();
        assert!(req.starts_with("POST /api/getBotToken"), "{req}");
        assert!(req.to_lowercase().contains("authorization: bearer s_ops"), "{req}");
        assert!(req.contains(r#""username":"ops:helper""#), "{req}");
    }

    /// Not the owner: say whose login asked, so the fix is obvious.
    #[tokio::test]
    async fn someone_elses_bot_is_refused_with_who_asked() {
        let (base, _seen) = one_shot(
            "403 Forbidden",
            r#"{"ok":false,"error_code":403,"description":"permission denied: not the owner of this bot"}"#.into(),
        )
        .await;
        let e = fetch(&base, "zed:bot", &login("alice")).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("@alice") && msg.contains("zed:bot"), "{msg}");
    }
}
