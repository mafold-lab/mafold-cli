//! The wallet fuel step (PR 4c, 10-31): an agent on its owner's computer
//! whose fuel is the owner's Mafold wallet (`getBot`'s `fuel` = `wallet`).
//!
//! Such an agent has no login of its own on the machine. Each reply gets ONE
//! key, minted by the api for that reply's draft (`mintReplyKey`): scoped to
//! the models the wallet pays for, dead when the reply is finalized. The key
//! reaches the harness through the turn's env ([`env`]) and nowhere else —
//! not argv, not a log line, not the transcript (`conceal::hide_while_live`).
//! When the harness is done, what the reply charged is read back from the
//! ledger ([`settle`]) and goes on the result card as `RunStats::wallet`.
//!
//! The owner's wallet answers the owner alone ([`gate`]). The api refuses the
//! key for anyone else's message as well; the daemon asks first so a guest's
//! turn never starts a process.
//!
//! Everything here is what the daemon's turn calls (`agent::handle`,
//! `agent::render_loop`), in that order: gate → draft → [`step`] → run →
//! [`before_done`] / [`without_done`]. Kept out of the turn so the billing
//! table drives the very same functions (`mafold dev-wallet-turn`, debug
//! builds: `crate::dev_wallet_turn`).

use std::future::Future;
use std::time::Duration;

use mafold_transcript::{failure, AgentEvent, RunStats};

use crate::client::{Client, ReplyCost, ReplyKey};

/// `getBot`'s `fuel` for an agent the owner's wallet pays for.
pub const WALLET: &str = "wallet";

/// The hand-off to the harness (`Turn.env`), named once: the OpenCode harness
/// reads exactly these and refuses a turn without them.
pub const KEY_ENV: &str = "MAFOLD_WALLET_KEY";
pub const BASE_ENV: &str = "MAFOLD_WALLET_BASE";
pub const MODELS_ENV: &str = "MAFOLD_WALLET_MODELS";

/// How often, and for how long, the reply's total is read back after the
/// harness is done. The router settles a call a few seconds after its stream
/// ends at most; twenty seconds of a finished reply still saying «generating»
/// is the ceiling on waiting for that.
pub const POLL_EVERY: Duration = Duration::from_secs(1);
pub const POLL_FOR: Duration = Duration::from_secs(20);

/// Is `fuel` the wallet? Unset is not: an agent from before fuel was stored
/// runs on its computer's own login, as it always did.
pub fn is_wallet(fuel: Option<&str>) -> bool {
    fuel.is_some_and(|f| f.trim().eq_ignore_ascii_case(WALLET))
}

/// May this turn start a reply on the owner's wallet? Only a message the
/// owner sent: not a guest's, not someone else's relayed by a bot (the owner's
/// own bots included — whose words they carry is theirs to say, and the api
/// checks the trigger's sender, not the chain), and not a turn nobody's
/// message started (an introduction, a background-task update, words queued
/// behind a running reply whose message wasn't recorded — [`Queued`]) — the
/// api mints a key only for a draft whose trigger the owner sent. `Err` is
/// the refusal, worded for the error card.
pub fn gate(owner: Option<&str>, sender: &str, relayed_for: Option<&str>, triggered: bool) -> Result<(), String> {
    let norm = crate::agent::norm_user;
    let Some(owner) = owner.map(norm).filter(|o| !o.is_empty()) else {
        return Err("wallet_owner_only: This agent runs on its owner's wallet, and this computer can't tell who \
                    its owner is, so it starts no replies."
            .into());
    };
    let relayed_other = relayed_for.is_some_and(|p| norm(p) != owner);
    if norm(sender) != owner || relayed_other {
        return Err(failure::wallet_owner_only(&owner));
    }
    if !triggered {
        // The owner's own turn, but no message of theirs to answer. Their
        // card, not a guest's: in words that say what to do about it.
        return Err(format!(
            "This agent runs on @{owner}'s wallet, which pays only for a reply to a message @{owner} sent it. \
             This one had no message of its own (one sent while it was busy, or a background update) — send it \
             as a new message."
        ));
    }
    Ok(())
}

/// The turn's env for a wallet reply: the key, the router base and the models
/// it may call, comma-separated.
pub fn env(key: &ReplyKey) -> Vec<(String, String)> {
    vec![
        (KEY_ENV.to_string(), key.key.clone()),
        (BASE_ENV.to_string(), key.base_url.trim_end_matches('/').to_string()),
        (MODELS_ENV.to_string(), key.models.join(",")),
    ]
}

/// A minted reply: its env, and the guard that keeps the key masked out of
/// everything the turn writes for as long as the turn holds it.
pub struct Fuel {
    /// The draft the key is for — the one the reply's total is read for,
    /// wherever a steer moves the reply afterwards.
    pub draft: String,
    pub env: Vec<(String, String)>,
    _masked: crate::conceal::LiveSecret,
}

/// Mint `draft`'s key. `Err` is the turn's error, in the server's words when
/// it refused (grant / cap / owner-only / flag) — the error card reads them.
/// A reply picked up after a restart mints again: the api answers the same
/// draft with the same live key.
pub async fn mint(client: &Client, draft: &str) -> Result<Fuel, String> {
    let key = client.mint_reply_key(draft).await.map_err(|e| format!("{e:#}"))?;
    let masked = crate::conceal::hide_while_live(&key.key);
    println!(
        "🔑 wallet: reply {draft} has its key ({} model(s), until {})",
        key.models.len(),
        key.expires_at
    );
    Ok(Fuel { draft: draft.to_string(), env: env(&key), _masked: masked })
}

/// The turn's fuel step, as `agent::handle` takes it once the draft exists.
/// Off the wallet nothing happens: no call, `env` as it was. On it, `draft`'s
/// key is minted and its three variables join `env` — the env every attempt
/// of the turn is built from — or the refusal comes back as the turn's error.
pub async fn step(client: &Client, wallet: bool, draft: &str, env: &mut Vec<(String, String)>) -> Result<Option<Fuel>, String> {
    if !wallet {
        return Ok(None);
    }
    let fuel = mint(client, draft).await?;
    env.extend(fuel.env.iter().cloned());
    Ok(Some(fuel))
}

/// Read the reply's total until nothing of it is in flight, at most `limit`:
/// a call's settle can land after its stream ended. What comes back is the
/// stats patch for the result card — the last answer's totals, marked partial
/// when something was still unsettled (or no answer came at all).
pub async fn settle<F, Fut>(mut fetch: F, every: Duration, limit: Duration) -> RunStats
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<ReplyCost>>,
{
    let started = tokio::time::Instant::now();
    let mut last: Option<ReplyCost> = None;
    let mut failed: Option<String> = None;
    loop {
        match fetch().await {
            Ok(cost) if cost.in_flight == 0 => {
                return RunStats { wallet: Some(cost.totals), ..Default::default() };
            }
            Ok(cost) => last = Some(cost),
            Err(e) => failed = Some(format!("{e:#}")),
        }
        if started.elapsed() + every > limit {
            break;
        }
        tokio::time::sleep(every).await;
    }
    match (&last, &failed) {
        (Some(c), _) => println!("⏳ wallet: {} call(s) still settling after {limit:?} — the card says so", c.in_flight),
        (None, Some(e)) => eprintln!("wallet: the reply's total couldn't be read ({e}) — the card says it's settling"),
        (None, None) => {}
    }
    RunStats {
        wallet: Some(last.map(|c| c.totals).unwrap_or_default()),
        wallet_partial: Some(true),
        ..Default::default()
    }
}

/// [`settle`] against the api, for `draft`.
pub async fn settled(client: &Client, draft: &str) -> RunStats {
    settle(|| client.reply_cost(draft), POLL_EVERY, POLL_FOR).await
}

/// Did the reply spend anything, or might it have? A turn that ended without
/// its own result card (stopped, failed) gets one only then — the owner's
/// money moved, and this is the only place the reply says how much.
pub fn spent(patch: &RunStats) -> bool {
    patch.wallet_partial == Some(true) || patch.wallet.as_ref().is_some_and(|w| w.iter().any(|c| c.charged != 0))
}

/// What a wallet reply's renderer pushes just before the harness's `Done`,
/// which stamps the result card: the ledger's total for `draft`, read once
/// the harness is done and until nothing of the reply is in flight — so a
/// call that settles after its stream ended is in the figure.
pub async fn before_done(client: &Client, draft: &str) -> AgentEvent {
    AgentEvent::Stats(settled(client, draft).await)
}

/// What a wallet reply's renderer pushes when the harness's stream closed
/// without a `Done` (stopped, failed): nothing when the reply spent nothing,
/// else the ledger's total and a `Done` — the result card that says how much
/// of the owner's money moved, which such a turn would otherwise not have.
pub async fn without_done(client: &Client, draft: &str) -> Vec<AgentEvent> {
    let patch = settled(client, draft).await;
    if !spent(&patch) {
        return Vec::new();
    }
    vec![AgentEvent::Stats(patch), AgentEvent::Done { duration_ms: None, cost_usd: None, tokens: None }]
}

/// The owner's messages queued behind a running wallet reply, by surface —
/// what its follow-up round opens its draft on.
///
/// OpenCode can't be steered, so the owner's words sent mid-reply wait in its
/// mailbox and come back as the follow-up round. That round needs a trigger
/// of its own: the api mints a key only for a draft whose trigger the owner
/// sent ([`gate`]), and opening it on the FIRST round's trigger was worse —
/// `botCreateDraft` hands back the open draft for the same (bot, trigger), so
/// while round 1's finalize was still in the outbox the follow-up wrote into
/// round 1's bubble, got round 1's key (which that finalize then revokes
/// mid-reply) and counted round 1's charges on its own card too.
///
/// The latest message per surface wins (the follow-up answers everything
/// queued, and opens on the newest), and each is taken once. A surface is a
/// thread, else a channel's timeline (`steer::on_surface`). With nothing
/// recorded the follow-up has no trigger, and [`gate`] refuses it in words.
#[derive(Default)]
pub struct Queued(std::sync::Mutex<std::collections::HashMap<Surface, String>>);

/// (conversation, channel, thread): where a reply runs.
type Surface = (String, Option<String>, Option<String>);

impl Queued {
    fn key(chat: &str, channel: Option<&str>, thread: Option<&str>) -> Surface {
        // A thread is its own surface: its channel adds nothing.
        let channel = if thread.is_some() { None } else { channel.map(str::to_string) };
        (chat.to_string(), channel, thread.map(str::to_string))
    }

    /// `message` was queued behind the running reply on this surface.
    pub fn remember(&self, chat: &str, channel: Option<&str>, thread: Option<&str>, message: &str) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(Self::key(chat, channel, thread), message.to_string());
    }

    /// The message this surface's follow-up round opens on, if any — once.
    pub fn take(&self, chat: &str, channel: Option<&str>, thread: Option<&str>) -> Option<String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(&Self::key(chat, channel, thread))
    }
}

/// This daemon's [`Queued`].
pub fn queued() -> &'static Queued {
    static ONE: std::sync::OnceLock<Queued> = std::sync::OnceLock::new();
    ONE.get_or_init(Queued::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mafold_transcript::WalletCharge;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A wallet reply's follow-up round opens on the owner's own message that
    /// was queued behind it — its newest one there, since the follow-up
    /// answers everything queued — and on nothing else: not a message queued
    /// on another surface, not one an earlier follow-up already opened on.
    #[test]
    fn a_follow_up_opens_on_the_latest_message_queued_on_its_surface_once() {
        let q = Queued::default();
        q.remember("c1", None, None, "m1");
        q.remember("c1", None, None, "m2");
        q.remember("c1", Some("ch-1"), None, "m3");
        q.remember("c1", Some("ch-1"), Some("root-1"), "m4");
        q.remember("c2", None, None, "m5");
        assert_eq!(q.take("c1", None, None).as_deref(), Some("m2"), "the latest wins");
        assert_eq!(q.take("c1", None, None), None, "taken once");
        assert_eq!(q.take("c1", Some("ch-1"), Some("root-1")).as_deref(), Some("m4"));
        assert_eq!(q.take("c1", Some("ch-1"), None).as_deref(), Some("m3"), "a channel and a thread in it are two surfaces");
        assert_eq!(q.take("c2", Some("ch-1"), None), None, "nothing queued there");
        assert_eq!(q.take("c2", None, None).as_deref(), Some("m5"));
        // A thread is its own surface whatever channel names it.
        q.remember("c1", Some("ch-1"), Some("root-2"), "m6");
        assert_eq!(q.take("c1", None, Some("root-2")).as_deref(), Some("m6"));
    }

    #[test]
    fn only_wallet_is_the_wallet() {
        assert!(is_wallet(Some("wallet")) && is_wallet(Some(" Wallet ")));
        for other in [None, Some(""), Some("claude"), Some("opencode-go")] {
            assert!(!is_wallet(other), "{other:?}");
        }
    }

    /// Who starts a reply on the owner's wallet: the owner, with a message of
    /// their own — and nobody else, whichever road they came by.
    #[test]
    fn only_the_owners_own_message_starts_a_wallet_reply() {
        let owner = Some("ops");
        assert_eq!(gate(owner, "ops", None, true), Ok(()));
        assert_eq!(gate(owner, "@OPS ", Some("ops"), true), Ok(()), "relayed for the owner, by the owner");
        let refused = failure::wallet_owner_only("ops");
        assert_eq!(
            refused,
            "wallet_owner_only: This agent runs on @ops's wallet. Only @ops can start its replies."
        );
        // A guest, whitelisted or not, and the owner's own bot.
        for sender in ["alice", "ops:helper", ""] {
            assert_eq!(gate(owner, sender, None, true), Err(refused.clone()), "{sender:?}");
        }
        // A bot relaying someone else, or nobody in particular, to the owner.
        assert_eq!(gate(owner, "ops", Some("alice"), true), Err(refused.clone()));
        assert_eq!(gate(owner, "ops", Some(""), true), Err(refused.clone()));
        // No trigger: a guest's still reads as a guest's…
        assert_eq!(gate(owner, "alice", None, false), Err(refused.clone()));
        // …the owner's says what happened, and is no guest's card.
        let none = gate(owner, "ops", None, false).unwrap_err();
        assert!(none.contains("@ops's wallet") && none.contains("send it as a new message"), "{none}");
        assert_eq!(failure::classify(&none).kind, failure::FailureKind::Unknown, "the owner reads it whole");
        assert_eq!(failure::classify(&refused).kind, failure::FailureKind::OwnerOnly);
        // An ownerless bot can't be on anyone's wallet.
        assert!(gate(None, "ops", None, true).is_err());
    }

    #[test]
    fn the_hand_off_is_three_variables_and_the_key_is_never_debug_printed() {
        let key = ReplyKey {
            key: "mk_secret_value_000000000001".into(),
            expires_at: 1_793_400_000,
            models: vec!["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
            base_url: "https://api.mafold.ai/v1/".into(),
        };
        assert_eq!(
            env(&key),
            vec![
                (KEY_ENV.into(), "mk_secret_value_000000000001".into()),
                (BASE_ENV.into(), "https://api.mafold.ai/v1".into()),
                (MODELS_ENV.into(), "deepseek-v4-flash,deepseek-v4-pro".into()),
            ]
        );
        let shown = format!("{key:?}");
        assert!(!shown.contains("mk_secret") && shown.contains("<redacted>"), "{shown}");
    }

    fn cost(charged: i64, in_flight: u64) -> ReplyCost {
        ReplyCost {
            totals: vec![WalletCharge { currency: "deepseek-v4-flash".into(), charged }],
            calls: 2,
            in_flight,
        }
    }

    /// The total is read until nothing is in flight, and that answer is the
    /// card's — not partial.
    #[tokio::test(start_paused = true)]
    async fn the_total_is_read_until_nothing_is_in_flight() {
        let calls = AtomicUsize::new(0);
        let answers = [cost(700, 1), cost(1234, 1), cost(1500, 0)];
        let got = settle(
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let a = answers[n.min(2)].clone();
                async move { Ok(a) }
            },
            POLL_EVERY,
            POLL_FOR,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(got.wallet, Some(cost(1500, 0).totals));
        assert_eq!(got.wallet_partial, None);
        assert!(spent(&got));
    }

    /// Still settling when the time is up: what had settled, marked partial.
    /// No answer at all: nothing known, still marked — never a silent zero.
    #[tokio::test(start_paused = true)]
    async fn a_total_still_settling_at_the_limit_is_marked_partial() {
        let calls = AtomicUsize::new(0);
        let t0 = tokio::time::Instant::now();
        let got = settle(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(cost(800, 1)) }
            },
            POLL_EVERY,
            POLL_FOR,
        )
        .await;
        assert_eq!(got.wallet, Some(cost(800, 1).totals));
        assert_eq!(got.wallet_partial, Some(true));
        assert!(t0.elapsed() <= POLL_FOR, "{:?}", t0.elapsed());
        assert!(calls.load(Ordering::SeqCst) >= 20, "polled every second: {}", calls.load(Ordering::SeqCst));

        let got = settle(|| async { Err(anyhow::anyhow!("getReplyCost failed: connect failed")) }, POLL_EVERY, POLL_FOR).await;
        assert_eq!(got.wallet, Some(vec![]));
        assert_eq!(got.wallet_partial, Some(true));
        assert!(spent(&got), "unknown is not «nothing spent»");
    }

    /// A stand-in api: answers every request with `(status, body)` and keeps
    /// what it was asked (request line + body).
    async fn api(status: &'static str, body: &'static str) -> (Client, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 16384];
                let n = s.read(&mut buf).await.unwrap_or(0);
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        (Client::new(base, "mb_bot_token_for_tests_0001".into()), seen)
    }

    /// Off the wallet, the step is nothing at all: no call, the turn's env
    /// as the seat left it.
    #[tokio::test]
    async fn off_the_wallet_the_turn_env_is_untouched_and_nothing_is_minted() {
        let (client, seen) = api("200 OK", r#"{"ok":true,"result":{}}"#).await;
        let mut env = vec![("CLAUDE_SECURESTORAGE_CONFIG_DIR".to_string(), "/x".to_string())];
        let got = step(&client, false, "d-1", &mut env).await;
        assert!(matches!(got, Ok(None)));
        assert_eq!(env, vec![("CLAUDE_SECURESTORAGE_CONFIG_DIR".to_string(), "/x".to_string())]);
        assert!(seen.lock().unwrap().is_empty(), "no call");
    }

    /// On it, the draft's key is asked for with the bot token, and the turn's
    /// env carries it — masked out of the turn's writes while it's held.
    #[tokio::test]
    async fn on_the_wallet_the_drafts_key_joins_the_turn_env() {
        let (client, seen) = api(
            "200 OK",
            r#"{"ok":true,"result":{"key":"mk_reply_key_value_0000000042","expires_at":1793400000,"models":["deepseek-v4-flash"],"base_url":"https://api.mafold.ai/v1"}}"#,
        )
        .await;
        let mut env = Vec::new();
        let fuel = step(&client, true, "d-42", &mut env).await.unwrap().expect("minted");
        assert_eq!(fuel.draft, "d-42");
        assert_eq!(
            env,
            vec![
                (KEY_ENV.to_string(), "mk_reply_key_value_0000000042".to_string()),
                (BASE_ENV.to_string(), "https://api.mafold.ai/v1".to_string()),
                (MODELS_ENV.to_string(), "deepseek-v4-flash".to_string()),
            ]
        );
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("POST /api/mintReplyKey"), "{req}");
        assert!(req.to_lowercase().contains("authorization: bearer mb_bot_token_for_tests_0001"), "{req}");
        assert!(req.contains(r#""message_id":"d-42""#), "{req}");
        let mut body = serde_json::json!({"content": "MAFOLD_WALLET_KEY=mk_reply_key_value_0000000042"});
        assert_eq!(crate::conceal::scrub_outgoing("botEditDraft", &mut body, "mb_x"), 1);
        drop(fuel);
    }

    /// A refusal is the turn's error in the server's own words — what the
    /// error card reads — not wrapped in the transport's.
    #[tokio::test]
    async fn a_refused_mint_is_the_turns_error_in_the_servers_words() {
        let (client, _) = api(
            "403 Forbidden",
            r#"{"ok":false,"error_code":403,"description":"wallet_grant_required: @ops no longer lets @ops:wallet spend from their wallet"}"#,
        )
        .await;
        let mut env = Vec::new();
        let why = step(&client, true, "d-7", &mut env).await.err().expect("refused");
        assert_eq!(why, "wallet_grant_required: @ops no longer lets @ops:wallet spend from their wallet");
        assert!(env.is_empty());
        assert_eq!(failure::classify(&why).kind, failure::FailureKind::Grant);
    }

    /// A reply that charged nothing (a refusal, a failure before the first
    /// call) adds no card to a turn that has none.
    #[test]
    fn nothing_settled_and_nothing_charged_is_not_spent() {
        assert!(!spent(&RunStats { wallet: Some(vec![]), ..Default::default() }));
        assert!(!spent(&RunStats { wallet: Some(cost(0, 0).totals), ..Default::default() }));
        assert!(!spent(&RunStats::default()));
    }
}
