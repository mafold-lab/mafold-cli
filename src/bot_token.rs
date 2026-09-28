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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
