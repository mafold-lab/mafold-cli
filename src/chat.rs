//! `mafold read` and `mafold access` — the CLI's first READ door.
//!
//! Until this shipped, `Client::get_chat_history` existed but nothing outside
//! the daemon ever called it: an agent could `send` to any conversation it
//! could name and could not read a single one, including the room it was
//! standing in. History only ever arrived by being folded into the prompt
//! before the turn started, at limits (50 / 200 / 24 000 chars) the model had
//! no hand on. `read` is that hand.
//!
//! The surface follows the trust story rather than the CRUD, the same way
//! `connection.rs` does:
//!
//!   read                      the transcript
//!   access request/status     asking for one
//!   access list/revoke        who I've lent my rooms to
//!
//! **The denial IS the request.** `read` on a room with no ticket does not
//! fail with "you lack permission" — an LLM reads that as a dead end and gives
//! up. It posts the consent card and says so, exiting 3. That one behaviour is
//! most of why this is usable by an agent at all; everything else here is
//! plumbing around it.
//!
//! WHICH IDENTITY: a bot token goes through the ticket; a human session is just
//! a participant and needs none. That is the OPPOSITE of `mafold connection`,
//! which refuses bot tokens on purpose because credentials belong to a person.
//! A room does not — it belongs to the people in it, and a bot can be one of
//! them. Do not "fix" this into a `require_human`.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;

/// Exit codes, because an agent has to tell "wait" from "give up" and grepping
/// our own prose is a probe that breaks the next time someone edits a string.
pub const EXIT_PENDING: i32 = 3;
pub const EXIT_REFUSED: i32 = 4;
pub const EXIT_EXPIRED: i32 = 5;

#[derive(Subcommand)]
pub enum AccessCmd {
    /// Ask a room's people to let you read it. `read` does this for you on the
    /// first miss; run it directly to ask for a room before you need it, to
    /// set a different TTL, or to ask on behalf of another agent.
    Request {
        /// Conversation id, or a name from `mafold chats`. Leave it out when
        /// using --of/--with.
        chat: Option<String>,
        /// Name a DM you are NOT in by its two people: `--of @ops --with
        /// @linsky`. The only form that can reach such a room — its uuid is
        /// learnable only from inside it.
        #[arg(long)]
        of: Option<String>,
        /// The other end of `--of`.
        #[arg(long)]
        with: Option<String>,
        /// Who to address the ask to. Defaults to a participant you already
        /// share a conversation with; required when that is ambiguous.
        /// Implied by --of.
        #[arg(long)]
        from: Option<String>,
        /// How long to ask for (1–90).
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// Ask on behalf of ANOTHER account — the a2a hand-off, for when you
        /// are in the room and the agent that needs it isn't. You may open the
        /// ask for someone else; only a participant can answer it.
        #[arg(long = "for")]
        for_account: Option<String>,
    },
    /// Lend one of YOUR rooms to an account, without waiting to be asked.
    ///
    /// The mirror of `request`. It exists because the ask alone could not
    /// start: to post the card the asker must name the room by uuid, and a
    /// uuid is only learnable from inside — so the commonest case, "let my own
    /// agent read my own chat", had nothing to press.
    Grant {
        /// Conversation id, or a name from `mafold chats`. You must be in it.
        chat: String,
        /// Who to lend it to.
        grantee: String,
        /// How long (1–90).
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
    /// What you may read, and what you are still waiting on.
    Status,
    /// Who you have let read your rooms.
    List,
    /// Take one account off one room.
    Revoke { chat: String, grantee: String },
}

// ─────────────────────────── mafold read ───────────────────────────

pub struct ReadArgs {
    pub chat: Option<String>,
    pub limit: usize,
    pub channel: Option<String>,
    /// The main timeline (`#all`) even inside a channel turn.
    pub main: bool,
    pub json: bool,
    pub media: bool,
    /// Only the unread (the chat's badge, capped by `limit`), then mark read.
    pub unread: bool,
    /// Prefix every row with its message id.
    pub ids: bool,
}

/// How many unread messages the caller has in this timeline — the same badge
/// the chat list shows. A channel keeps its own count (`listChannels`); the
/// main timeline's is on the chat row. None = the room isn't in my list.
async fn unread_badge(client: &Client, chat_id: &str, channel: Option<&str>) -> Result<Option<usize>> {
    let rows = match channel {
        Some(_) => client.list_channels(chat_id).await?,
        None => client.chats().await?,
    };
    let want = channel.unwrap_or(chat_id);
    let list = rows["items"].as_array().or_else(|| rows.as_array()).cloned().unwrap_or_default();
    Ok(list
        .iter()
        .find(|r| r["id"].as_str() == Some(want))
        .map(|r| r["unread_count"].as_u64().unwrap_or(0) as usize))
}

/// The newest message on a page — the read marker to move to. By timestamp,
/// not position, so it holds whichever order the page came in.
pub(crate) fn newest_id(page: &Value) -> Option<String> {
    page["items"]
        .as_array()?
        .iter()
        .filter(|m| m["id"].as_str().is_some())
        .max_by(|a, b| a["created_at"].as_str().unwrap_or("").cmp(b["created_at"].as_str().unwrap_or("")))
        .and_then(|m| m["id"].as_str().map(str::to_string))
}

/// `mafold read [chat]` — the transcript, and an ask if there isn't one yet.
pub async fn read(client: &Client, a: ReadArgs) -> Result<()> {
    // No argument = the room this turn is happening in. The daemon presets
    // MAFOLD_CONV, so the commonest read needs no ticket, no id and no
    // thinking — which is what stops an agent treating `read` as the scary
    // permission command and never trying it.
    let raw = match a.chat.clone() {
        Some(c) => c,
        None => std::env::var("MAFOLD_CONV").ok().filter(|s| !s.is_empty()).context(
            "no conversation — pass one (`mafold read <chat>`) or run inside a turn",
        )?,
    };
    let chat = resolve_room(client, &raw).await?;
    // Whether this turn may open it as this account at all (`own_room`) —
    // asked before anything else about the room is: its channels, its unread
    // count. One of this account's rooms that isn't this turn's people's is
    // refused outright, by id, not name: what it is called is already more
    // than they may know about it. No ticket is the way round that.
    let reads = ReadsFor::from_env();
    let own = own_room(client, &chat.id, &reads).await;
    if matches!(own, Own::Closed) {
        not_theirs(&reads, &chat.id);
    }
    // Inside a channel turn, this room means THIS channel unless told
    // otherwise — the same default `send` has, so an agent reads where it is
    // about to write. `--main` asks for `#all`.
    let from_turn = if a.channel.is_none() && !a.main { turn_channel(&chat.id) } else { None };
    let channel = match &a.channel {
        Some(c) => Some(resolve_channel(client, &chat.id, c).await?),
        None => from_turn.clone(),
    };

    // `--unread`: only as many as the badge says, and none at all is an answer.
    let mut limit = a.limit;
    if a.unread {
        match unread_badge(client, &chat.id, channel.as_deref()).await? {
            Some(0) => {
                println!("# {} · {} · 没有未读", chat.label, chat.shape);
                return Ok(());
            }
            Some(n) => limit = n.min(a.limit),
            None => anyhow::bail!("--unread: {} isn't one of your chats", chat.label),
        }
    }

    // Try as myself first. A participant never touches the grant table, so the
    // ordinary case costs exactly one request.
    let mine = match own {
        Own::Open => client.chat_history(&chat.id, limit, channel.as_deref(), None).await,
        Own::Closed => not_theirs(&reads, &chat.id),
        // Not among its rooms, as far as the list says: only a ticket opens
        // it — and the read that would have answered «you are in it» anyway
        // (an old room, past the list) is not made.
        Own::NotIn => Err(anyhow::anyhow!("permission denied: not a participant")),
    };
    let (page, lender) = match mine {
        Ok(p) => (p, None),
        Err(e) if !is_permission_error(&e) => return Err(e),
        Err(_) => {
            let grants = client.list_chat_grants().await.unwrap_or_else(|_| json!({}));
            match lender_for(&grants, &chat.id, &reads) {
                Some(g) => {
                    let p = client
                        .chat_history(&chat.id, a.limit, channel.as_deref(), Some(&g))
                        .await
                        .with_context(|| format!("reading as @{g}"))?;
                    (p, Some(g))
                }
                None => return no_ticket(client, &grants, &chat, a.limit, &reads).await,
            }
        }
    };

    if a.json {
        println!("{}", serde_json::to_string_pretty(&page)?);
        return Ok(());
    }
    // Bytes first, so the transcript can print real paths beside the rows they
    // belong to rather than a trailing list the reader has to match up.
    let files = if a.media { fetch_media(client, &page).await } else { Default::default() };
    if let Some(ch) = &from_turn {
        // Say which timeline this is: the reader didn't name it.
        let name = channel_name(client, &chat.id, ch).await;
        println!("# #{name} —— 本回合所在的频道(读 #all 加 --main)");
    }
    print_transcript_ex(&chat, lender.as_deref(), &page, &files, a.ids);
    // Reading the unread is what opening the chat does: move MY marker to the
    // newest message shown (never past it — anything that arrived since stays
    // unread). Not when reading on someone else's ticket: it isn't my room.
    let dry = std::env::var("MAFOLD_SEND_DRY").is_ok_and(|v| v.trim() == "1");
    if a.unread && lender.is_none() && !dry {
        if let Some(newest) = newest_id(&page) {
            match channel.as_deref() {
                Some(ch) => client.mark_channel_read(&chat.id, ch, &newest).await?,
                None => client.mark_read(&chat.id, Some(&newest)).await?,
            }
        }
    }
    if channel.is_none() {
        forum_hint(client, &chat).await;
    }
    Ok(())
}

/// A forum's `#all` is usually the quietest timeline it has — say so.
///
/// Reading a forum without `--channel` gives you the main timeline, which in a
/// busy forum can be hours or weeks behind the rooms people actually talk in.
/// Silence about that is the whole problem: the transcript LOOKS complete, so
/// the reader concludes messages have stopped arriving rather than that they
/// opened the wrong one. (Burned on the Mafold DEV group: `#all` ended 1.5h
/// back while the live conversation sat in a channel, and it read exactly like
/// a sync bug.)
///
/// Printed after the transcript and only when no channel was named; a room
/// that isn't a forum says nothing at all.
async fn forum_hint(client: &Client, chat: &Room) {
    let Ok(v) = client.list_channels(&chat.id).await else { return };
    let items: Vec<&Value> = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["archived"].as_bool() != Some(true))
        .collect();
    if items.is_empty() {
        return;
    }
    println!(
        "\n※「{}」是论坛,上面读到的是 #all 主时间线 —— 另有 {} 个频道。",
        chat.label,
        items.len()
    );
    // Name the few with unread, since that is where a reader looking for
    // "recent" almost certainly wants to be.
    let mut hot: Vec<(&str, u64)> = items
        .iter()
        .filter_map(|c| Some((c["name"].as_str()?, c["unread_count"].as_u64().unwrap_or(0))))
        .filter(|(_, n)| *n > 0)
        .collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, n) in hot.iter().take(5) {
        println!("   #{name}  ({n} 条未读)");
    }
    println!("   mafold read {} --channel '<频道名>'", chat.id);
}

/// Download every attachment on the page; `id → local path`.
///
/// Opt-in, and it has to be: a 200-message page of a photo-heavy room would
/// otherwise pull tens of megabytes to answer a question about the text.
///
/// No new permission is involved. A file id IS the capability — `/media/<id>`
/// is unauthenticated by design (`main.rs::media_get` looks the row up and
/// serves or 302s, and never asks who is calling), so anything that can read
/// the transcript can already read its bytes. The earlier note claiming the
/// ticket had to be taught to the file-id auth path was simply wrong: there is
/// no such path to teach.
///
/// Failures are per-file and non-fatal. One dead attachment must not cost the
/// reader the other forty, and the row still prints its name.
async fn fetch_media(client: &Client, page: &Value) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for m in page["items"].as_array().into_iter().flatten() {
        for a in m["attachments"].as_array().into_iter().flatten() {
            let Some(f) = a.get("file") else { continue };
            let Some(id) = f["id"].as_str() else { continue };
            if out.contains_key(id) {
                continue;
            }
            // The id leads, the sender's name follows: unique on disk, and the
            // agent still reads `…-budget-q3.csv` rather than a bare token —
            // which is also what restores the TYPE for whatever opens it.
            let name = match f["filename"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                Some(n) => format!("{id}-{}", crate::agent::sanitize_attachment_name(n)),
                None => crate::agent::sanitize_attachment_name(id),
            };
            match save_attachment(client, &format!("/media/{id}"), &name).await {
                Ok(path) => {
                    out.insert(id.to_string(), path.to_string_lossy().into_owned());
                }
                Err(e) => eprintln!("· 取不到附件 {name}: {e}"),
            }
        }
    }
    out
}

/// One attachment into `~/.mafold/attachments/<name>`, and where it landed.
/// `name` is the caller's, already unique and already through
/// `sanitize_attachment_name` — it is joined as it is. A file already there is
/// not fetched again. `src` goes through `Client::download`: a Mafold path or a
/// Mafold media origin, never anywhere else. Shared by `mafold read --media`
/// and `mafold mail read --media`.
pub(crate) async fn save_attachment(client: &Client, src: &str, name: &str) -> Result<std::path::PathBuf> {
    let dir = crate::agent::attachments_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(name);
    if path.is_file() {
        return Ok(path);
    }
    let bytes = client.download(src).await?;
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// No live ticket this turn may use: say what is already in flight, or ask —
/// and never just fail. An ask from a turn outside the owner's circle says
/// whom it is for (`ReadsFor::askers`), so the ticket it brings is theirs.
async fn no_ticket(client: &Client, grants: &Value, chat: &Room, limit: usize, reads: &ReadsFor) -> Result<()> {
    if let Some(p) = pending_for(grants, &chat.id, reads) {
        let asked = p["asked"].as_str().unwrap_or("");
        let at = p["asked_at"].as_str().unwrap_or("");
        eprintln!("⏳ 已经问过了,还没人回应 —— 卡还挂在「{}」里", chat.label);
        eprintln!("   问的是 @{asked} · {at}");
        eprintln!("   别再问一次。等对方点了「允许」,重跑这条命令。");
        std::process::exit(EXIT_PENDING);
    }
    match ask(client, &chat.id, None, 7, None, reads.askers()).await {
        Ok(r) if r["granted"].as_bool() == Some(true) => {
            // Raced a grant between the refusal and the ask. Outside the circle
            // that is only so if the ticket is one this turn may use — an api
            // that predates `askers` says «granted» for anybody's — and «you are
            // in that room» is the room `own_room` already refused.
            let g = if reads.is_all() {
                r["grantor"].as_str().map(str::to_string)
            } else {
                if r["reason"].as_str() == Some("participant") {
                    not_theirs(reads, &chat.label);
                }
                let fresh = client.list_chat_grants().await.unwrap_or_else(|_| json!({}));
                match lender_for(&fresh, &chat.id, reads) {
                    Some(g) => Some(g),
                    None => not_theirs(reads, &chat.label),
                }
            };
            let page = client.chat_history(&chat.id, limit, None, g.as_deref()).await?;
            print_transcript(chat, g.as_deref(), &page, &Default::default());
            Ok(())
        }
        Ok(r) if someone_elses_ask(&r) => {
            eprintln!("⏳ 「{}」已经有一张卡在等回应 —— 不是替这一轮的人问的", chat.label);
            eprintln!("   一次只挂一张卡。等那张有了结果,再跑一次这条命令,会替这一轮的人另问。");
            std::process::exit(EXIT_PENDING);
        }
        Ok(r) => {
            eprintln!("✗ 没有这个房间的读取票 —— 已经替你申请了");
            eprintln!("   同意卡已发到「{}」·这个房间里任何一个人点「允许」都算数", chat.label);
            eprintln!("   7 天 · 批准后重跑本条命令,或 `mafold access status` 看进度");
            if let Some(id) = r["message_id"].as_str() {
                eprintln!("   卡: {id}");
            }
            std::process::exit(EXIT_PENDING);
        }
        Err(e) => {
            eprintln!("✗ 读不了,也问不了:{e}");
            eprintln!("   要读一个你不在的房间,得先跟里面的某个人在同一个会话里;");
            eprintln!("   用 `mafold access request <房间> --from @谁` 指定问谁。");
            std::process::exit(EXIT_REFUSED);
        }
    }
}

/// The api's «a card is already up for this room — and it isn't asking for
/// you» (`yours: false`): one card at a time, and nobody rides in on a tap
/// that never saw their name. An api from before says nothing, which reads as
/// the old «still waiting».
fn someone_elses_ask(r: &Value) -> bool {
    r["pending"].as_bool() == Some(true) && r["yours"].as_bool() == Some(false)
}

/// Ask for a ticket, working out WHO to address when nobody said.
///
/// The default is the people in the room this turn is happening in. That is
/// almost always right and it is right for a reason rather than by luck: an
/// agent learns about a room it cannot see because somebody in front of it
/// mentioned one, and that somebody is the person to ask. Requiring `--from`
/// every time would put a flag between the agent and the one thing it needs.
///
/// Candidates are tried in order because the server answers "not found" for
/// "that person isn't in that room" — deliberately, so an ask cannot be used
/// to probe who is where. Which means the only way to find the right addressee
/// is to try, and the only cost of a wrong guess is one refused request.
async fn ask(
    client: &Client,
    conv: &str,
    from: Option<&str>,
    days: i64,
    for_account: Option<&str>,
    askers: &[String],
) -> Result<Value> {
    if let Some(u) = from {
        return client.request_chat_access(conv, u, days, for_account, askers).await;
    }
    let candidates = neighbours(client).await;
    if candidates.is_empty() {
        bail!("不知道该问谁 —— 用 --from @某人 指定一个那个房间里的人");
    }
    let mut last = None;
    for u in &candidates {
        match client.request_chat_access(conv, u, days, for_account, askers).await {
            Ok(r) => return Ok(r),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no one to ask")))
}

/// Everyone sharing the CURRENT conversation with me, humans first.
///
/// Humans first because a human can tap a consent card and a bot, in general,
/// will not — addressing the ask to one would leave it hanging in the room
/// forever while the outbox refused to let a second one be sent.
async fn neighbours(client: &Client) -> Vec<String> {
    let Ok(here) = std::env::var("MAFOLD_CONV") else { return vec![] };
    let Ok(list) = client.chats().await else { return vec![] };
    let me = client
        .me()
        .await
        .ok()
        .and_then(|m| m["username"].as_str().map(str::to_lowercase))
        .unwrap_or_default();
    let Some(conv) = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["id"].as_str() == Some(here.as_str()))
    else {
        return vec![];
    };
    let mut out: Vec<(bool, String)> = conv["participants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let u = p["username"].as_str()?;
            (u.to_lowercase() != me).then(|| (u.contains(':'), u.to_string()))
        })
        .collect();
    out.sort_by_key(|(is_bot, _)| *is_bot);
    out.into_iter().map(|(_, u)| u).collect()
}

/// The transcript, in the SAME shape the daemon folds into a prompt.
///
/// Not JSON by default and not a table: an agent reading this has already been
/// trained on the history block it gets every turn, and giving it a second
/// format to learn buys nothing. `--json` is there for code.
fn print_transcript(
    chat: &Room,
    lender: Option<&str>,
    page: &Value,
    files: &std::collections::HashMap<String, String>,
) {
    print_transcript_ex(chat, lender, page, files, false)
}

fn print_transcript_ex(
    chat: &Room,
    lender: Option<&str>,
    page: &Value,
    files: &std::collections::HashMap<String, String>,
    ids: bool,
) {
    let items = page["items"].as_array().cloned().unwrap_or_default();
    let head = match lender {
        Some(g) => format!("# {} · {} · 以 @{} 的视角", chat.label, chat.shape, g),
        None => format!("# {} · {}", chat.label, chat.shape),
    };
    println!("{head}");
    if items.is_empty() {
        println!("(还没有消息)");
        return;
    }
    let mut unfetched = 0usize;
    let now = crate::inbox::now_ms();
    for m in &items {
        let who = m["sender"]["username"].as_str().unwrap_or("?");
        let ts = m["created_at"].as_str().unwrap_or("");
        let when = ts.get(5..16).unwrap_or(ts).replace('T', " ");
        // A reply still being written reads as what it is — someone on it
        // since …, gone quiet, or waiting on a card — not as a half-finished
        // answer with a stray "[卡片:mafold/generating]" in it.
        let mid = m["id"].as_str().unwrap_or("");
        let body = if crate::inbox::in_progress(m) {
            crate::inbox::working_status(m, now)
        } else {
            readable_body(m["content"].as_str().unwrap_or(""), mid)
        };
        // Every row carries its id — what `mafold msg`, `--reply` and a card
        // action name a message by. `--ids` predates that and is kept as a
        // no-op so scripts that pass it keep working.
        let _ = ids;
        let id = if mid.is_empty() { String::new() } else { format!("#{mid} ") };
        println!("{id}[{when}] {who}: {}", if body.is_empty() { "—".into() } else { body });
        for a in m["attachments"].as_array().into_iter().flatten() {
            let name = attachment_name(a);
            match a["file"]["id"].as_str().and_then(|id| files.get(id)) {
                Some(p) => println!("              └─ {name} → {p}"),
                None => {
                    unfetched += 1;
                    println!("              └─ 附:{name}");
                }
            }
        }
    }
    let more = page["next_cursor"].as_str().is_some();
    let mut tail = String::new();
    if more {
        tail.push_str(" · 还有更早的,--limit 提高上限");
    }
    if unfetched > 0 {
        // The name alone is not usable. Say the one flag that makes it so,
        // rather than leaving a reader to guess that the bytes are reachable.
        tail.push_str(&format!(" · {unfetched} 个附件没取,--media 下载到本地"));
    }
    println!("─ {} 条{tail} ─", items.len());
}

pub(crate) fn attachment_name(a: &Value) -> String {
    match a["kind"].as_str().unwrap_or("") {
        "photo" => "图片".to_string(),
        "video" => "视频".to_string(),
        "chat_record" => "聊天记录".to_string(),
        _ => a["file"]["filename"].as_str().unwrap_or("附件").to_string(),
    }
}

/// `mafold msg <id>`: one message exactly as stored — what every fold note
/// points at. A header line (id, time, sender) and the attachments' names,
/// then the content verbatim: nothing folded, nothing rewritten.
pub async fn print_message(client: &Client, id: &str, json: bool) -> Result<()> {
    let id = id.trim().trim_start_matches('#');
    let m = client.get_message(id).await?;
    // A message id opens whichever of this account's rooms it was said in —
    // so it answers to the same rule `read` does (`own_room`).
    let reads = ReadsFor::from_env();
    if !reads.is_all() {
        let conv = m["conversation_id"].as_str().unwrap_or_default();
        if !matches!(own_room(client, conv, &reads).await, Own::Open) {
            not_theirs(&reads, "这条消息所在的房间");
        }
    }
    let mut m = m;
    let me = my_username(client).await;
    let sender = m["sender"].clone();
    if let Some(items) = m.get_mut("sidecar").and_then(|s| s.get_mut("state")).and_then(Value::as_object_mut) {
        named_for(items, &sender, &me);
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&m)?);
        return Ok(());
    }
    print!("{}", message_text(&m));
    // Its controlled state, as far as this account may read it — fetched when
    // there was too much to come with the message.
    let mut state = if m["sidecar"]["state_more"].as_bool() == Some(true) {
        client.get_card_state(id).await.map(|r| r["state"].clone()).unwrap_or(Value::Null)
    } else {
        m["sidecar"]["state"].clone()
    };
    if let Some(items) = state.as_object_mut() {
        named_for(items, &m["sender"], &me);
    }
    if let Some(items) = state.as_object().filter(|s| !s.is_empty()) {
        print!("\n── 卡片状态(`mafold state` 读写)──\n{}", state_lines(items));
    }
    Ok(())
}

/// This account's username, lowercase ("" when the server would not say —
/// then [`named_for`] keeps only what every reader may read).
async fn my_username(client: &Client) -> String {
    client.me().await.ok().and_then(|m| m["username"].as_str().map(str::to_lowercase)).unwrap_or_default()
}

/// What an agent is SHOWN of a message's state: the items whose read list
/// names `me` — not every item it may read as the message's author. The
/// author reads all of its own message's items (card-state §3.3), an owner's
/// summary, a failure's detail for whoever paid and a note for one person
/// included; printed into a turn, the model can repeat them to whoever asked.
/// The api's `card_state::named_for`, read off each item's own list.
fn named_for(items: &mut serde_json::Map<String, Value>, sender: &Value, me: &str) {
    let name = sender["username"].as_str().unwrap_or_default();
    let owner = sender["parent_username"]
        .as_str()
        .filter(|o| !o.is_empty())
        .or_else(|| name.split_once(':').map(|(o, _)| o))
        .unwrap_or(name);
    let is = |n: &str| !me.is_empty() && me.eq_ignore_ascii_case(n.trim_start_matches('@'));
    items.retain(|_, it| {
        it["read"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|w| {
            if w.eq_ignore_ascii_case("all") {
                true
            } else if w.eq_ignore_ascii_case("owner") {
                is(owner)
            } else if w.eq_ignore_ascii_case("sender") {
                is(name)
            } else if w.eq_ignore_ascii_case("server") {
                false
            } else {
                is(w)
            }
        })
    });
}

/// A message's state items, one per line: key = value · who · when · rev.
fn state_lines(items: &serde_json::Map<String, Value>) -> String {
    let mut out = String::new();
    for (key, it) in items {
        let v = serde_json::to_string(&it["v"]).unwrap_or_default();
        let v = if v.chars().count() > 400 { format!("{}…", v.chars().take(400).collect::<String>()) } else { v };
        let when = it["at"]
            .as_i64()
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| t.format("%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "{key} = {v} · {} · {when} · rev {}\n",
            it["by"].as_str().unwrap_or("?"),
            it["rev"].as_u64().unwrap_or(0)
        ));
    }
    out
}

/// `mafold state` — a message's controlled state (`.docs/card-state-v1.md`).
#[derive(Subcommand)]
pub enum StateCmd {
    /// What you may read of a message's state: every item, or the keys named.
    Get {
        /// The message id.
        message: String,
        /// Only these keys (`mafold/html/0.items`, `mafold/html/0.vote@linsky`).
        keys: Vec<String>,
        /// As the API returns it.
        #[arg(long)]
        json: bool,
    },
    /// Write one item as yourself. The first write of a key fixes its lists
    /// (default read all, write sender); only a message's sender creates a key,
    /// except one ending `@<your name>`, which is yours alone.
    Set {
        /// The message id.
        message: String,
        /// `<card tag>/<sid or ordinal>.<field>` — `mafold/html/0.items`.
        key: String,
        /// The value as JSON: `'"text"'`, `3`, `'["a","b"]'`, `null`.
        value: String,
        /// Who may read it (first write only): all, sender, owner, @name — comma separated.
        #[arg(long, value_delimiter = ',')]
        read: Option<Vec<String>>,
        /// Who may write it (first write only), same words.
        #[arg(long, value_delimiter = ',')]
        write: Option<Vec<String>>,
        /// Only if the item is still at this revision (0: only if there is none
        /// yet) — read, change, write back without losing someone else's edit.
        #[arg(long)]
        if_rev: Option<u64>,
    },
}

pub async fn run_state(cmd: StateCmd, client: &Client) -> Result<()> {
    let room_of = |m: &Value| m["conversation_id"].as_str().unwrap_or_default().to_string();
    let reads = ReadsFor::from_env();
    match cmd {
        StateCmd::Get { message, keys, json } => {
            let id = message.trim().trim_start_matches('#');
            let m = client.get_message(id).await?;
            if !reads.is_all() && !matches!(own_room(client, &room_of(&m), &reads).await, Own::Open) {
                not_theirs(&reads, "这条消息所在的房间");
            }
            let r = client.get_card_state(id).await?;
            let mut state = r["state"].as_object().cloned().unwrap_or_default();
            named_for(&mut state, &m["sender"], &my_username(client).await);
            if !keys.is_empty() {
                state.retain(|k, _| keys.iter().any(|w| w == k));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&Value::Object(state))?);
            } else if state.is_empty() {
                println!("(这条消息没有你能读的卡片状态)");
            } else {
                print!("{}", state_lines(&state));
            }
        }
        StateCmd::Set { message, key, value, read, write, if_rev } => {
            let id = message.trim().trim_start_matches('#');
            if !reads.is_all() {
                let m = client.get_message(id).await?;
                if !matches!(own_room(client, &room_of(&m), &reads).await, Own::Open) {
                    not_theirs(&reads, "这条消息所在的房间");
                }
            }
            let v: Value = serde_json::from_str(&value)
                .with_context(|| format!("the value must be JSON — text is written '\"{value}\"'"))?;
            let r = client
                .set_card_state_with(id, &key, &v, read.as_deref(), write.as_deref(), if_rev)
                .await?;
            let item = &r["item"];
            if r["applied"].as_bool() == Some(false) {
                println!(
                    "✗ not applied — {key} is at rev {} now: {}",
                    item["rev"].as_u64().unwrap_or(0),
                    serde_json::to_string(&item["v"]).unwrap_or_default()
                );
                std::process::exit(EXIT_CONFLICT);
            }
            println!("✓ {key} = {} · rev {}", serde_json::to_string(&item["v"]).unwrap_or_default(), item["rev"].as_u64().unwrap_or(0));
        }
    }
    Ok(())
}

/// `mafold state set --if-rev` found the item moved on.
pub const EXIT_CONFLICT: i32 = 6;

fn message_text(m: &Value) -> String {
    let ts = m["created_at"].as_str().unwrap_or("");
    let when = ts.get(0..16).unwrap_or(ts).replace('T', " ");
    let mut out = format!(
        "#{} [{when}] @{}\n",
        m["id"].as_str().unwrap_or("?"),
        m["sender"]["username"].as_str().unwrap_or("?")
    );
    for a in m["attachments"].as_array().into_iter().flatten() {
        out.push_str(&format!("└─ 附:{}\n", attachment_name(a)));
    }
    out.push('\n');
    out.push_str(m["content"].as_str().unwrap_or(""));
    out.push('\n');
    out
}

/// A message body on one line, as `mafold read` and the inbox print it: the
/// same folded view an agent's history uses ([`crate::agent::model_view`]) —
/// every card its tag and attributes, the line a person sees on the collapsed
/// card, with its body folded into a note naming `mafold msg <id>` — then
/// whitespace collapsed.
///
/// One view for everything that reads a chat. This used to be a third one:
/// cards became bare `[卡片:name]` markers, the agent machinery was cut with
/// its bodies (an agent's turn printed as «—»), a forwarded record was
/// expanded inline. Folding says what is there and how to open it, and the
/// opening (`mafold msg`) prints the message exactly as stored.
pub(crate) fn readable_body(text: &str, id: &str) -> String {
    crate::agent::model_view(text, id).split_whitespace().collect::<Vec<_>>().join(" ")
}

// ─────────────────────────── mafold access ───────────────────────────

pub async fn run(cmd: AccessCmd, client: &Client) -> Result<()> {
    // Outside the owner's circle a turn may ask for a room of its own — the
    // ticket is then theirs — and see only its own. Which rooms this account
    // lends, and to whom, stays its owner's business.
    let reads = ReadsFor::from_env();
    if !reads.is_all() && matches!(cmd, AccessCmd::Grant { .. } | AccessCmd::Revoke { .. } | AccessCmd::List) {
        eprintln!("✗ 这一轮不是这个 agent 主人圈子的人发起的 —— 它把哪些房间借给谁、收不收回,是主人的事");
        std::process::exit(EXIT_REFUSED);
    }
    match cmd {
        AccessCmd::Request { chat: None, of: Some(of), with: Some(with), days, for_account, .. } => {
            let who = for_account.as_deref().unwrap_or("你");
            let r = client
                .request_chat_access_pair(&of, &with, days, for_account.as_deref(), reads.askers())
                .await?;
            let pair = format!("@{} 和 @{} 的私聊", of.trim_start_matches('@'), with.trim_start_matches('@'));
            // Outside the circle «granted» can't be checked here — the pair
            // form never names the room — and saying it would tell this turn
            // that a DM exists and the agent may read it: the very answer this
            // form keeps uniform. It hears what «no such DM» hears.
            let restricted = !reads.is_all() && for_account.is_none();
            if r["granted"].as_bool() == Some(true) && !restricted {
                println!("✓ {who} 已经能读「{pair}」了 —— 不用再问");
            } else if r["pending"].as_bool() == Some(true) && !restricted {
                println!("⏳ 已经问过了,卡还挂着,没人回应");
                std::process::exit(EXIT_PENDING);
            } else {
                // Deliberately does not claim the card landed: for this form
                // the server answers the same whether that DM exists or not,
                // so saying "已发到 X" would be inventing a fact we were
                // never told. See `request_chat_access_pair`.
                println!("已向 @{} 提出申请:读「{pair}」,受权人写的是 {who}。", of.trim_start_matches('@'));
                println!("对方点「允许」才算数 —— 我不能替他点头。用 `mafold access status` 看有没有下文。");
            }
            Ok(())
        }
        AccessCmd::Request { chat, of, with, from, days, for_account } => {
            if of.is_some() || with.is_some() {
                bail!("--of 和 --with 要成对出现,而且不能再带 <chat>");
            }
            let chat = chat.context("要指名一个会话:<chat>,或者 --of @某人 --with @某人")?;
            let room = resolve_room(client, &chat).await?;
            let r = ask(client, &room.id, from.as_deref(), days, for_account.as_deref(), reads.askers()).await?;
            let who = for_account.as_deref().unwrap_or("你");
            // «Granted» outside the circle only if this turn may use it: an api
            // from before `askers` says so for anybody's ticket, and «you are in
            // it» is a room `read` will refuse this turn anyway.
            if r["granted"].as_bool() == Some(true) && !reads.is_all() && for_account.is_none() {
                let theirs = r["reason"].as_str() != Some("participant")
                    && lender_for(&client.list_chat_grants().await.unwrap_or_else(|_| json!({})), &room.id, &reads).is_some();
                if !theirs {
                    not_theirs(&reads, &room.label);
                }
            }
            if r["granted"].as_bool() == Some(true) {
                println!("✓ {who} 已经能读「{}」了 —— 不用再问", room.label);
            } else if someone_elses_ask(&r) {
                println!("⏳ 「{}」已经有一张卡在等回应 —— 不是替这一轮的人问的;等它有了结果再申请", room.label);
                std::process::exit(EXIT_PENDING);
            } else if r["pending"].as_bool() == Some(true) {
                println!("⏳ 已经问过了,卡还挂在「{}」里,没人回应", room.label);
                std::process::exit(EXIT_PENDING);
            } else {
                println!("同意卡已发到「{}」,受权人写的是 {who}", room.label);
                println!("这个房间里任何一个人点「允许」都算数 —— 我不能替它点头。");
            }
            Ok(())
        }
        AccessCmd::Grant { chat, grantee, days } => {
            let room = resolve_room(client, &chat).await?;
            let r = client.grant_chat_access(&room.id, &grantee, days).await?;
            let who = grantee.trim_start_matches('@');
            if r["reason"].as_str() == Some("participant") {
                println!("@{who} 本来就在「{}」里 —— 不需要票", room.label);
            } else {
                println!("✓ 已把「{}」借给 @{who},{days} 天后到期", room.label, days = days);
                println!("随时可以收回:mafold access revoke {} @{who}", room.id);
            }
            Ok(())
        }
        AccessCmd::Status => {
            let g = client.list_chat_grants().await?;
            let held: Vec<Value> = g["held"].as_array().into_iter().flatten().filter(|r| reads.admits_ticket(r)).cloned().collect();
            let pending: Vec<Value> = g["pending"].as_array().into_iter().flatten().filter(|r| reads.admits_ask(r)).cloned().collect();
            if held.is_empty() && pending.is_empty() {
                println!("(没有任何房间的读取票,也没有在等的申请)");
                return Ok(());
            }
            if !held.is_empty() {
                println!("持有 ({})", held.len());
                for r in &held {
                    println!(
                        "  {:<28} 由 @{} 授权   {}",
                        r["title"].as_str().unwrap_or("?"),
                        r["account"].as_str().unwrap_or("?"),
                        expiry(r["expires_at"].as_i64().unwrap_or(0)),
                    );
                    println!("  {:<28} mafold read {}", "", r["conversation_id"].as_str().unwrap_or(""));
                }
            }
            if !pending.is_empty() {
                println!("等待批准 ({})", pending.len());
                for r in &pending {
                    println!(
                        "  {}  问的是 @{} · {}",
                        r["conversation_id"].as_str().unwrap_or("?"),
                        r["asked"].as_str().unwrap_or("?"),
                        r["asked_at"].as_str().unwrap_or(""),
                    );
                }
                println!("  卡还挂在那些房间里 —— 别重复申请。");
            }
            Ok(())
        }
        AccessCmd::List => {
            let g = client.list_chat_grants().await?;
            let granted = g["granted"].as_array().cloned().unwrap_or_default();
            if granted.is_empty() {
                println!("(没有人能读你的房间)");
                return Ok(());
            }
            println!("# 谁能读我的房间");
            for r in &granted {
                println!(
                    "  {:<28} @{:<24} {}",
                    r["title"].as_str().unwrap_or("?"),
                    r["account"].as_str().unwrap_or("?"),
                    expiry(r["expires_at"].as_i64().unwrap_or(0)),
                );
            }
            println!("撤销:mafold access revoke <房间> <@谁>  (网页里也点得到:设置 ▸ 隐私)");
            Ok(())
        }
        AccessCmd::Revoke { chat, grantee } => {
            let room = resolve_room(client, &chat).await?;
            let r = client.revoke_chat_grant(&room.id, &grantee).await?;
            if r["removed"].as_bool() == Some(true) {
                println!("✓ 已撤销 —— @{grantee} 下一次调用就读不到「{}」了", room.label);
            } else {
                println!("(@{grantee} 本来就没有「{}」的票)", room.label);
            }
            Ok(())
        }
    }
}

fn expiry(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    match chrono::DateTime::from_timestamp(unix, 0) {
        Some(t) => format!("{} 到期", t.format("%m/%d")),
        None => String::new(),
    }
}

/// The rooms this bot may read, as a prompt block — a POINTER list, not the
/// history itself.
///
/// A capability nobody tells the agent about does not exist: the daemon has
/// been through this once already with the (since retired) `mafold room`, which
/// sat unused until the installed apps were named in the prompt. So `chat.read`
/// gets the same treatment, in the same place, in the same shape.
///
/// Deliberately NOT the transcripts. Folding five rooms' history into every
/// system prompt would burn the context window on turn one and be wasted on
/// the nine turns in ten that never look at another room. A name and the one
/// command that fetches it is the whole job.
///
/// `None` when there are no tickets — no tickets, no prompt overhead, exactly
/// like the apps block.
pub async fn context_block(client: &Client, reads: &ReadsFor) -> Option<String> {
    let g = client.list_chat_grants().await.ok()?;
    access_block(&g, reads)
}

/// [`context_block`] for a `listChatGrants` answer already in hand: the
/// tickets `reads` may use, as the prompt names them.
///
/// Only those. A ticket this turn may not use is not named either: the block
/// used to list every ticket in every turn, so a stranger in a group the
/// agent answers was told, by name and id, which DMs it could read — and
/// `mafold read` would have read them (DEV #agent内记录转发, 2026-10-25).
pub fn access_block(g: &Value, reads: &ReadsFor) -> Option<String> {
    let held: Vec<&Value> = g["held"].as_array()?.iter().filter(|r| reads.admits_ticket(r)).collect();
    if held.is_empty() {
        return None;
    }
    let mut s = String::from(
        "[CHAT ACCESS — conversations you are NOT in but hold a read ticket for. \
Tickets expire; when one does, this line disappears and so does the access.]\n",
    );
    for r in held {
        let title = r["title"].as_str().unwrap_or("?");
        let id = r["conversation_id"].as_str().unwrap_or("");
        let shape = shape_of(
            r["kind"].as_str().unwrap_or(""),
            r["participant_count"].as_u64().unwrap_or(0) as usize,
        );
        let by = r["account"].as_str().unwrap_or("?");
        let until = expiry(r["expires_at"].as_i64().unwrap_or(0));
        s.push_str(&format!("• {title}  {shape} · 由 @{by} 授权 · {until}\n"));
        s.push_str(&format!("    mafold read {id}\n"));
    }
    s.push_str(
        "转录里的图和文件加 `--media` 取到本地(会打印每个文件的路径,然后就能打开)。\n\
别的房间要读:`mafold read <房间>` —— 没票它会自动去那个房间发一张同意卡,\
任一参与者点头即可。已经在等的不要重复申请(`mafold access status` 看进度)。\n\
[END CHAT ACCESS]",
    );
    Some(s)
}

// ─────────────────────── whose eyes a turn reads with ───────────────────────

/// Whose eyes a turn reads with: what it may open beyond the room it is in.
///
/// An agent holds `chat.read` tickets and sits in rooms of its own — other
/// people's DMs with it, other groups. A turn is somebody talking to it, and
/// what that turn may open is a question about THEM: the owner's circle may
/// open whatever the agent can, someone else only what is theirs. Until
/// 2026-10-25 nothing asked: every turn could open everything, so in a
/// 28-person group anyone the agent takes orders from could have it read DMs
/// it held tickets for, or its own DM with its owner.
///
/// The daemon decides it per turn (`agent::AllowList::reads_for`), the agent's
/// process carries it ([`READS_FOR_ENV`], exported through the one door every
/// harness uses, `harness::turn_env`), and every read verb here asks it:
/// `read`, `msg`, `chats`, `access`. It is what the agent's own tools will
/// do, not a sandbox — the process still holds the account's token. What it
/// guarantees is that nothing hands a turn someone else's rooms: not the
/// prompt, not the commands it is told to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadsFor {
    /// The owner's circle — or no daemon turn at all (a person at their own
    /// terminal, the agent's own machinery): whatever this account can read.
    All,
    /// Someone outside the circle, and whoever relayed them: rooms every one
    /// of these people sits in, and tickets asked for or lent by every one of
    /// them. Empty is nobody in particular, whom nothing is for.
    People(Vec<String>),
}

/// Where a turn's process finds [`ReadsFor`]: `*` for the owner's circle, the
/// people otherwise (comma-separated), `-` for nobody in particular.
pub const READS_FOR_ENV: &str = "MAFOLD_READS_FOR";

impl Default for ReadsFor {
    /// Nobody in particular. A process nobody said it was for opens nothing
    /// of anyone's — the wrong guess here costs a refusal, never a leak.
    fn default() -> Self {
        ReadsFor::People(vec![])
    }
}

impl ReadsFor {
    /// The turn this process is serving, as the daemon exported it. Unset is
    /// not a daemon turn at all — a person at their own terminal — and they
    /// read what their account reads.
    pub fn from_env() -> Self {
        match std::env::var(READS_FOR_ENV) {
            Ok(v) => Self::parse(&v),
            Err(_) => ReadsFor::All,
        }
    }

    pub fn parse(v: &str) -> Self {
        if v.trim() == "*" {
            return ReadsFor::All;
        }
        ReadsFor::People(
            v.split(',')
                .map(|p| p.trim().trim_start_matches('@').to_lowercase())
                .filter(|p| !p.is_empty() && p != "-")
                .collect(),
        )
    }

    /// [`Self::parse`]'s inverse — what the daemon exports.
    pub fn env_value(&self) -> String {
        match self {
            ReadsFor::All => "*".into(),
            ReadsFor::People(p) if p.is_empty() => "-".into(),
            ReadsFor::People(p) => p.join(","),
        }
    }

    /// Whom an ask made in this turn is for — recorded beside the ticket, so
    /// the people who asked can use it and nobody else outside the circle can.
    /// Empty for the circle, who may use every ticket anyway.
    pub fn askers(&self) -> &[String] {
        match self {
            ReadsFor::All => &[],
            ReadsFor::People(p) => p,
        }
    }

    pub fn is_all(&self) -> bool {
        matches!(self, ReadsFor::All)
    }

    /// Is every one of this turn's people among `names`? Nobody in particular
    /// is among no one.
    fn all_among<'a>(&self, names: impl Iterator<Item = &'a str> + Clone) -> bool {
        match self {
            ReadsFor::All => true,
            ReadsFor::People(p) => {
                !p.is_empty() && p.iter().all(|x| names.clone().any(|n| n.eq_ignore_ascii_case(x)))
            }
        }
    }

    /// May this turn use a `held` ticket (a `listChatGrants` row)? It is for
    /// the people it was asked for (`for`) and for its lender (`account`),
    /// who can read that room themselves.
    pub fn admits_ticket(&self, row: &Value) -> bool {
        let on = row["for"].as_array().into_iter().flatten().filter_map(Value::as_str);
        self.all_among(on.chain(row["account"].as_str()))
    }

    /// May this turn see an ask still waiting (a `pending` row)?
    pub fn admits_ask(&self, row: &Value) -> bool {
        self.all_among(row["for"].as_array().into_iter().flatten().filter_map(Value::as_str))
    }
}

/// May a turn reading for `reads` open `conv`, a room this account sits in?
/// `row` is the room's line in `getChats` (None: not among them), `here` the
/// room the turn is happening in — which everyone in the turn can read.
pub(crate) fn may_read_room(row: Option<&Value>, conv: &str, here: Option<&str>, reads: &ReadsFor) -> bool {
    if reads.is_all() || here.is_some_and(|h| h.trim().eq_ignore_ascii_case(conv.trim())) {
        return true;
    }
    let Some(row) = row else { return false };
    reads.all_among(row["participants"].as_array().into_iter().flatten().filter_map(|p| p["username"].as_str()))
}

/// The room this turn is happening in (`MAFOLD_CONV`), if it is a turn.
fn turn_conv() -> Option<String> {
    std::env::var("MAFOLD_CONV").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// What [`own_room`] found.
enum Own {
    /// This turn may open it as this account.
    Open,
    /// One of this account's rooms, and not this turn's people's.
    Closed,
    /// Not among this account's rooms, as far as its chat list says.
    NotIn,
}

/// [`may_read_room`] against this account's chat list. The circle's turns and
/// the turn's own room never ask. A room missing from the list (or a list that
/// can't be fetched) is one nobody can vouch for: a restricted turn opens it
/// only on a ticket of its own.
async fn own_room(client: &Client, conv: &str, reads: &ReadsFor) -> Own {
    let here = turn_conv();
    if reads.is_all() || here.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(conv.trim())) {
        return Own::Open;
    }
    let Ok(list) = client.chats().await else { return Own::NotIn };
    match list["items"].as_array().into_iter().flatten().find(|c| c["id"].as_str() == Some(conv)) {
        Some(row) if may_read_room(Some(row), conv, here.as_deref(), reads) => Own::Open,
        Some(_) => Own::Closed,
        None => Own::NotIn,
    }
}

/// The rule `read` keeps, for every other verb that opens one of this
/// account's rooms by id (`channels`, `tap`, `app call`): in a turn outside
/// the owner's circle, a room its people could not open themselves is refused
/// (`own_room`). The circle's turns, and the turn's own room, pass without a
/// lookup.
pub(crate) async fn ensure_may_open(client: &Client, conv: &str) {
    let reads = ReadsFor::from_env();
    if !reads.is_all() && !matches!(own_room(client, conv, &reads).await, Own::Open) {
        not_theirs(&reads, conv);
    }
}

/// What a restricted turn hears when the room is not its people's to open.
fn not_theirs(reads: &ReadsFor, label: &str) -> ! {
    let who = match reads {
        ReadsFor::People(p) if !p.is_empty() => p.iter().map(|u| format!("@{u}")).collect::<Vec<_>>().join("、"),
        _ => "不具名的访客".into(),
    };
    eprintln!("✗ 这一轮是替 {who} 跑的,「{label}」不是这一轮能打开的");
    eprintln!("   这个 agent 待的别的房间、它持有的读取票,只在它主人圈子的那几轮、");
    eprintln!("   或者房间里本来就有这个人 / 票是替这个人申请的时候才能用。别换个办法再试。");
    std::process::exit(EXIT_REFUSED);
}

// ─────────────────────────── naming rooms ───────────────────────────

/// A conversation, named the way every surface here should name one.
pub struct Room {
    pub id: String,
    pub label: String,
    /// «5 人群» / «私聊» — the half `mafold chats` used to drop, which is how
    /// an untitled group ended up wearing one member's name.
    pub shape: String,
}

/// Resolve `<chat>`: a uuid, an @username (their DM), or a name from the list.
///
/// The name arm is the point. `resolve_chat` only ever knew uuids and
/// usernames, while `mafold chats` printed neither — so the list and the verbs
/// could not be used together at all. An agent that just read a room name off
/// the screen can now pass it straight back.
pub async fn resolve_room(client: &Client, arg: &str) -> Result<Room> {
    let a = arg.trim();
    // A turn outside the owner's circle resolves among the rooms its people
    // could open (`may_read_room`): naming one — or listing them when a name
    // matches none or several — is the first thing about the others it has no
    // business with. (`mafold chats` lists by the same rule.)
    let reads = ReadsFor::from_env();
    let here = turn_conv();
    let theirs = |c: &Value| may_read_room(Some(c), c["id"].as_str().unwrap_or_default(), here.as_deref(), &reads);
    if is_uuid(a) {
        // A room I may not be in: name it from the grant list if it's there,
        // and otherwise say the uuid rather than inventing a title.
        if let Ok(list) = client.chats().await {
            if let Some(c) = list["items"].as_array().into_iter().flatten().find(|c| c["id"].as_str() == Some(a)) {
                if theirs(c) {
                    return Ok(room_from(client, c).await);
                }
                return Ok(Room { id: a.into(), label: a.into(), shape: String::new() });
            }
        }
        if let Ok(g) = client.list_chat_grants().await {
            // Only from a ticket this turn may use.
            let keys: &[&str] = if reads.is_all() { &["held", "granted"] } else { &["held"] };
            for key in keys {
                if let Some(r) = g[*key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|r| r["conversation_id"].as_str() == Some(a) && (*key != "held" || reads.admits_ticket(r)))
                {
                    return Ok(Room {
                        id: a.into(),
                        label: r["title"].as_str().unwrap_or(a).to_string(),
                        shape: shape_of(r["kind"].as_str().unwrap_or(""), r["participant_count"].as_u64().unwrap_or(0) as usize),
                    });
                }
            }
        }
        return Ok(Room { id: a.into(), label: a.into(), shape: String::new() });
    }

    let list = client.chats().await?;
    let items: Vec<Value> = list["items"].as_array().into_iter().flatten().filter(|c| theirs(*c)).cloned().collect();
    let me = client.me().await.ok();
    let my = me
        .as_ref()
        .and_then(|m| m["username"].as_str())
        .unwrap_or_default()
        .to_lowercase();

    let needle = a.trim_start_matches('@').to_lowercase();
    // An exact @handle wins over a fuzzy name: `@ops` must never resolve to a
    // group that happens to be called "ops something".
    if a.starts_with('@') || needle.contains(':') {
        if let Some(c) = items.iter().find(|c| {
            c["kind"].as_str() == Some("direct")
                && c["participants"].as_array().into_iter().flatten().any(|p| {
                    p["username"].as_str().map(str::to_lowercase).as_deref() == Some(&needle)
                })
        }) {
            return Ok(room_from(client, c).await);
        }
        // Not in my list — and this is deliberately where it STOPS.
        //
        // It used to fall through to `resolve_chat`, i.e. `startChat`, i.e.
        // `conversations_create`: a lookup that quietly opened a brand-new DM
        // between me and them and handed back its id. So `mafold read @linsky`
        // meant "create my own chat with linsky, then read it" — and because it
        // SUCCEEDED, the caller got an almost-empty room and exit 0 instead of
        // an error. A resolver that invents its answer is worse than one that
        // fails; someone else's DM is not a room `startChat` can ever return.
        bail!(
            "我不在和 @{needle} 的私聊里,而「解析」不会去建一个。\n\
             · 要读某人和 @{needle} 的私聊:mafold access request --of @那个人 --with @{needle}\n\
             · 要给 @{needle} 发消息(这一步才会建私聊):mafold send @{needle} ……"
        );
    }

    let hits: Vec<&Value> = items
        .iter()
        .filter(|c| label_of(c, &my).to_lowercase().contains(&needle))
        .collect();
    match hits.len() {
        1 => Ok(room_from(client, hits[0]).await),
        0 => {
            // Same rule as the @handle arm: no match is an ANSWER, not a cue
            // to create. This one was worse — `a` here is a room NAME, so the
            // old fallback handed a group's title to `startChat` as if it were
            // a username. Report what does exist instead; a caller that can
            // see the list can correct itself in one step.
            let mut msg = format!("没有叫「{a}」的会话。现在能看到的:\n");
            for c in items.iter().take(20) {
                msg.push_str(&format!(
                    "  {}  {} · {}\n",
                    c["id"].as_str().unwrap_or("?"),
                    label_of(c, &my),
                    shape_of(
                        c["kind"].as_str().unwrap_or(""),
                        c["participants"].as_array().map_or(0, |p| p.len())
                    ),
                ));
            }
            if items.len() > 20 {
                msg.push_str(&format!("  …… 还有 {} 个,`mafold chats` 看全部\n", items.len() - 20));
            }
            bail!(msg)
        }
        _ => {
            // Ambiguity is REPORTED, never guessed. Two rooms wearing the same
            // name is the exact defect this shipped to fix; silently taking
            // the first would have kept it, one layer down.
            let mut msg = format!("「{a}」对上了 {} 个会话,用 id 指定:\n", hits.len());
            for c in hits {
                msg.push_str(&format!(
                    "  {}  {} · {}\n",
                    c["id"].as_str().unwrap_or("?"),
                    label_of(c, &my),
                    shape_of(
                        c["kind"].as_str().unwrap_or(""),
                        c["participants"].as_array().map_or(0, |p| p.len())
                    ),
                ));
            }
            bail!(msg)
        }
    }
}

async fn room_from(_client: &Client, c: &Value) -> Room {
    Room {
        id: c["id"].as_str().unwrap_or_default().to_string(),
        label: label_of(c, ""),
        shape: shape_of(
            c["kind"].as_str().unwrap_or(""),
            c["participants"].as_array().map_or(0, |p| p.len()),
        ),
    }
}

/// What to call a conversation in a list.
///
/// THE FIX. The old rule was "title, else the first participant who isn't me",
/// applied WITHOUT looking at `kind` — so an untitled group took one member's
/// display name and sat in the list looking exactly like a DM with that
/// person. On @opsdu's account that produced two rows reading «ops» (a DM with
/// `opsdu:claude-code`, whose display name is also "ops", and a 3-person group)
/// and two reading «fei_pota» (a DM and a 4-person group). Nothing was wrong
/// with the ids — every conversation id is a fresh v4 uuid — the printer was
/// throwing away the two fields that tell them apart.
pub fn label_of(c: &Value, me_lc: &str) -> String {
    if let Some(t) = c["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
        return t.to_string();
    }
    let names: Vec<String> = c["participants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| {
            me_lc.is_empty()
                || p["username"].as_str().map(str::to_lowercase).as_deref() != Some(me_lc)
        })
        .map(|p| {
            let d = p["display_name"].as_str().unwrap_or("").trim();
            if d.is_empty() { p["username"].as_str().unwrap_or("?") } else { d }.to_string()
        })
        .collect();
    match names.len() {
        0 => "Chat".into(),
        1..=3 => names.join(", "),
        n => format!("{}, +{}", names[..2].join(", "), n - 2),
    }
}

/// «私聊» / «5 人群» — never omitted, because it is half of the answer to
/// "which of these two rows do I want".
pub fn shape_of(kind: &str, participants: usize) -> String {
    match kind {
        "direct" => "私聊".into(),
        "group" if participants > 0 => format!("{participants} 人群"),
        "group" => "群".into(),
        "garden" => "园地".into(),
        _ => String::new(),
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.matches('-').count() == 4
}

/// Did the server say no, as opposed to falling over? Only a refusal is worth
/// re-trying with a ticket; a 500 must surface as itself.
///
/// Matched on the envelope's `description`, which is `ApiError`'s Display —
/// «permission denied: not a participant». A string match is the interface
/// here because `Client::post` flattens the wire's `error_code` into an
/// `anyhow` chain before this ever sees it.
///
/// `{:#}` AND NOT `to_string()`. `post` wraps every failure in
/// `.context("<method> failed")`, and an anyhow error's Display is the
/// OUTERMOST context alone — so `to_string()` here was matching the literal
/// string "getChatHistory failed" and finding nothing in it. The whole
/// denial-is-the-request behaviour silently degraded to a raw error and an
/// exit 1: the feature looked finished and did nothing. Found by running it,
/// not by reading it.
fn is_permission_error(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("permission denied") || s.contains("not a participant") || s.contains("chat.read")
}

/// Whose ticket `mafold read` reads `conv` on, for a turn reading for `reads`:
/// the longest-lived of the tickets this turn may use ([`ReadsFor::admits_ticket`]).
/// A room lent several times keeps reading until the last of them lapses.
pub(crate) fn lender_for(grants: &Value, conv: &str, reads: &ReadsFor) -> Option<String> {
    grants["held"]
        .as_array()?
        .iter()
        .filter(|r| r["conversation_id"].as_str() == Some(conv) && reads.admits_ticket(r))
        .max_by_key(|r| r["expires_at"].as_i64().unwrap_or(0))?["account"]
        .as_str()
        .map(str::to_string)
}

/// An ask for `conv` still waiting, if it is this turn's to wait on.
fn pending_for(grants: &Value, conv: &str, reads: &ReadsFor) -> Option<Value> {
    grants["pending"]
        .as_array()?
        .iter()
        .find(|r| r["conversation_id"].as_str() == Some(conv) && reads.admits_ask(r))
        .cloned()
}

/// The forum channel this turn was asked in, when `chat_id` IS the turn's own
/// conversation — where `send` and `read` go when no `--channel`/`--main` is
/// given. The daemon exports both halves (`harness::turn_env`). None outside a
/// turn, on `#all`, and for any other chat: a DM or another room is never
/// steered into a channel of this one.
pub fn turn_channel(chat_id: &str) -> Option<String> {
    turn_channel_from(
        chat_id,
        std::env::var("MAFOLD_CONV").ok().as_deref(),
        std::env::var("MAFOLD_FORUM_CHANNEL").ok().as_deref(),
    )
}

fn turn_channel_from(chat_id: &str, conv: Option<&str>, channel: Option<&str>) -> Option<String> {
    let conv = conv.map(str::trim).filter(|c| !c.is_empty())?;
    let channel = channel.map(str::trim).filter(|c| !c.is_empty())?;
    conv.eq_ignore_ascii_case(chat_id.trim()).then(|| channel.to_string())
}

/// A channel's name for a receipt or a heading; the id when it can't be found.
pub async fn channel_name(client: &Client, chat: &str, channel_id: &str) -> String {
    client
        .list_channels(chat)
        .await
        .ok()
        .and_then(|v| {
            let list = v.get("items").cloned().unwrap_or(v);
            list.as_array()?
                .iter()
                .find(|c| c["id"].as_str() == Some(channel_id))
                .and_then(|c| c["name"].as_str().map(str::to_string))
        })
        .unwrap_or_else(|| channel_id.to_string())
}

async fn resolve_channel(client: &Client, chat: &str, name: &str) -> Result<String> {
    let n = name.trim_start_matches('#');
    if is_uuid(n) {
        return Ok(n.to_string());
    }
    let chs = client.list_channels(chat).await?;
    chs.as_array()
        .into_iter()
        .flatten()
        // An untitled channel's name is a placeholder it shares with every
        // other untitled one: those go by id.
        .filter(|c| c["untitled"].as_bool() != Some(true))
        .find(|c| c["name"].as_str().map(str::to_lowercase).as_deref() == Some(&n.to_lowercase()))
        .and_then(|c| c["id"].as_str().map(str::to_string))
        .with_context(|| format!("no channel called #{n}"))
}

/// The block a turn in an UNTITLED forum channel carries. A channel created
/// without a name is named by the agent answering in it, the way an AI chat
/// titles itself — this is the self-hosted half of the hosted `name_channel`
/// tool (mafold-api `brains/platform.rs`). Both ids are spelled out, so the
/// command reads the same from every harness with no env expansion involved.
///
/// `None` on `#all`, in a named channel, or when the list can't be fetched: a
/// hint that fails is silence, never a failed turn. Not cached — a channel is
/// untitled for one exchange, and a stale "untitled" would send the agent to
/// rename a channel that already has a name (the server refuses that anyway).
pub async fn untitled_channel_block(client: &Client, chat: &str, channel_id: Option<&str>) -> Option<String> {
    let channel_id = channel_id?;
    let list = client.list_channels(chat).await.ok()?;
    untitled_block_from(&list, chat, channel_id)
}

fn untitled_block_from(list: &Value, chat: &str, channel_id: &str) -> Option<String> {
    let items = list.get("items").unwrap_or(list);
    let ch = items.as_array()?.iter().find(|c| c["id"].as_str() == Some(channel_id))?;
    if ch["untitled"].as_bool() != Some(true) {
        return None;
    }
    // In the FIRST step, never "once you have replied" — the reply is the last
    // thing a turn says, and a rename after it either buried the answer under
    // the folded trail or, hosted, ended a turn on the rename with no answer at
    // all (mafold-api `brains/platform.rs`, same wording).
    Some(format!(
        "[UNTITLED CHANNEL — the channel you are answering in has no name yet. Name it in your \
FIRST step, alongside whatever else you do there: `mafold channels rename {chat} {channel_id} <a \
few words for what it is about, in the conversation's language, no #>`. Naming is housekeeping, \
not a reply — the person never sees it as an answer, so your answer to them is still the last \
thing you write. Name it once; if it answers that the channel already has a name, leave it.]"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `send`/`read` default to the turn's channel — for the turn's OWN
    /// conversation only, and only when there is one.
    #[test]
    fn the_turn_channel_is_the_default_only_for_this_turns_own_chat() {
        let conv = "72355ef4-c43f-44ba-a0d5-b2c061026cd6";
        let ch = "6037c16d-af20-4df5-82c5-175acd50c7c1";
        assert_eq!(turn_channel_from(conv, Some(conv), Some(ch)).as_deref(), Some(ch));
        assert_eq!(turn_channel_from(&conv.to_uppercase(), Some(conv), Some(ch)).as_deref(), Some(ch), "uuid case");
        assert_eq!(turn_channel_from("df093413-8311-4ed4-a5e9-f243158d3a6b", Some(conv), Some(ch)), None, "another chat");
        assert_eq!(turn_channel_from(conv, Some(conv), Some("")), None, "#all turn");
        assert_eq!(turn_channel_from(conv, Some(conv), None), None, "an older daemon that exports no channel");
        assert_eq!(turn_channel_from(conv, None, Some(ch)), None, "not in a turn");
    }

    fn conv(kind: &str, title: Option<&str>, people: &[(&str, &str)]) -> Value {
        json!({
            "id": "39e5723e-cb57-475c-8475-2838f1ccfbeb",
            "kind": kind,
            "title": title,
            "participants": people.iter().map(|(u, d)| json!({"username": u, "display_name": d})).collect::<Vec<_>>(),
        })
    }

    /// The regression this fix exists for: an untitled group must not come out
    /// wearing one member's name, no matter how many members share it.
    #[test]
    fn an_untitled_group_is_not_labelled_as_one_person() {
        let group = conv(
            "group",
            None,
            &[("opsdu", "ops"), ("opsdu:claude-code", "ops"), ("fishssssgsbgsv", "fishssssgsbgsv")],
        );
        let dm = conv("direct", None, &[("opsdu", "ops"), ("opsdu:claude-code", "ops")]);

        assert_eq!(label_of(&dm, "opsdu"), "ops");
        assert_ne!(
            label_of(&group, "opsdu"),
            "ops",
            "a 3-person group is still being printed as a single name"
        );
        // And the shape is what separates them even when the labels collide.
        assert_eq!(shape_of("direct", 2), "私聊");
        assert_eq!(shape_of("group", 3), "3 人群");
    }

    /// A big group gets a name that fits, and still says how many are in it.
    #[test]
    fn a_crowded_room_truncates_rather_than_picking_a_favourite() {
        let big = conv(
            "group",
            None,
            &[("me", "me"), ("a", "Ann"), ("b", "Bo"), ("c", "Cy"), ("d", "Dee")],
        );
        assert_eq!(label_of(&big, "me"), "Ann, Bo, +2");
    }

    /// A titled room uses its title, full stop — the participant fallback is a
    /// fallback, not a preference.
    #[test]
    fn a_title_wins() {
        let t = conv("group", Some("Redq 财务"), &[("me", "me"), ("a", "Ann")]);
        assert_eq!(label_of(&t, "me"), "Redq 财务");
    }

    /// The refusal has to be RECOGNISED, or `read` never asks for anything.
    ///
    /// Pinned against the shape `Client::post` actually produces — a context
    /// wrapper over the server's description — because the first version
    /// matched `to_string()`, saw only "getChatHistory failed", and turned the
    /// whole feature into an exit-1 with no card sent.
    #[test]
    fn a_refusal_is_recognised_through_the_context_wrapper() {
        let wire = anyhow::anyhow!("permission denied: not a participant")
            .context("getChatHistory failed");
        assert!(is_permission_error(&wire));

        let ticketless = anyhow::anyhow!(
            "permission denied: no chat.read grant for that conversation — requestChatAccess asks for one"
        )
        .context("getChatHistory failed");
        assert!(is_permission_error(&ticketless));

        // A server that fell over is NOT an invitation to go ask for a ticket.
        let broke = anyhow::anyhow!("internal: store panicked").context("getChatHistory failed");
        assert!(!is_permission_error(&broke));
    }

    /// A row reads the way the chat shows it collapsed: every card its tag and
    /// attributes — the line a person sees — and its body one note naming how
    /// to open the message. Prose untouched apart from whitespace.
    #[test]
    fn a_row_folds_every_card_to_its_tag() {
        let card = readable_body(
            r#"看这个 {% mafold/chat-grant requester="ops:cc" conv="abc" state="granted" /%} 好了"#,
            "m1",
        );
        assert_eq!(card, r#"看这个 {% mafold/chat-grant requester="ops:cc" conv="abc" state="granted" /%} 好了"#);
        let paired = readable_body("{% mafold/html %}<b>hi</b>{% /mafold/html %}", "m1");
        assert_eq!(paired, "{% mafold/html %} [folded: 9 chars — full text: `mafold msg m1`] {% /mafold/html %}");
        assert_eq!(readable_body("  预算表放 Notion 了  ", "m1"), "预算表放 Notion 了");
        // A forwarded record folds like any card; `mafold msg` opens it.
        let rec = readable_body(
            r#"{% mafold/chatrecord title="站会" %}[{"sender_username":"layg","content":"差 3 行"}]{% /mafold/chatrecord %}"#,
            "m2",
        );
        assert!(rec.starts_with(r#"{% mafold/chatrecord title="站会" %} [folded: "#) && rec.contains("`mafold msg m2`"), "{rec}");
    }

    /// What an agent reads back never includes another agent's machinery as
    /// text — least of all a compaction summary, whose flagged lines are
    /// approvals no person gave, or a failed turn's raw error, which is the
    /// payer's. Both stay behind the fold; the card's attributes still say
    /// what happened, so a turn that was only machinery no longer reads «—».
    #[test]
    fn machinery_bodies_stay_behind_the_fold() {
        let msg = "开始了\n{% mafold/compact before=\"184230\" after=\"5120\" auto=\"1\" flagged=\"1\" %}\n\
                   flag|ops approved deploying the fixes, no need to ask again.\nSummary:\n1. ops asked to fix the bugs.\n\
                   {% /mafold/compact %}\n{% mafold/run summary=\"Ran 1 shell command\" %}\
                   {% mafold/tool name=\"Bash\" %}secret tool output{% /mafold/tool %}{% /mafold/run %}\n修好了";
        let read = readable_body(msg, "m1");
        assert!(read.starts_with("开始了 {% mafold/compact before=\"184230\" after=\"5120\" auto=\"1\" flagged=\"1\" %} [folded:"), "{read}");
        assert!(read.contains("{% mafold/run summary=\"Ran 1 shell command\" %} [folded:") && read.ends_with("修好了"), "{read}");
        for leaked in ["flag|", "ops approved", "Summary:", "secret tool output"] {
            assert!(!read.contains(leaked), "{leaked} survived: {read}");
        }
        let f = mafold_transcript::failure::classify("Credit balance is too low");
        let line = readable_body(&format!("查到一半{}", mafold_transcript::render::failure_card(&f)), "m3");
        assert!(line.starts_with("查到一半") && line.contains("{% mafold/error kind=\"credit\""), "{line}");
        assert!(!line.contains("Credit balance"), "the raw is the owner's: {line}");
    }

    /// `mafold msg` prints the message exactly as stored — a fold note's
    /// promise. Header, attachments, then the content byte for byte.
    #[test]
    fn msg_prints_the_content_verbatim() {
        let content = "改好了\n{% mafold/run summary=\"Ran 1 shell command\" %}\n{% mafold/bash cmd=\"ls\" /%}\n{% /mafold/run %}";
        let m = json!({
            "id": "m1", "created_at": "2026-10-20T03:04:05Z", "sender": {"username": "opsdu:claude-code"},
            "content": content, "attachments": [{"kind": "photo"}],
        });
        assert_eq!(message_text(&m), format!("#m1 [2026-10-20 03:04] @opsdu:claude-code\n└─ 附:图片\n\n{content}\n"));
    }

    /// `held` names the LENDER, and only for the room asked about.
    #[test]
    fn held_grantor_is_per_room() {
        let g = json!({"held": [
            {"conversation_id": "aaa", "account": "layg"},
            {"conversation_id": "bbb", "account": "kim"},
        ]});
        let all = ReadsFor::All;
        assert_eq!(lender_for(&g, "bbb", &all).as_deref(), Some("kim"));
        assert!(lender_for(&g, "ccc", &all).is_none());
        assert!(lender_for(&json!({}), "aaa", &all).is_none());
    }

    fn people(p: &[&str]) -> ReadsFor {
        ReadsFor::People(p.iter().map(|s| s.to_string()).collect())
    }

    /// A ticket asked for in one person's turn is THAT person's (and its
    /// lender's — they can read the room anyway). The next stranger to talk to
    /// the agent does not inherit it, and a relay is held to everyone in it.
    #[test]
    fn a_ticket_is_for_whoever_asked_for_it() {
        let g = json!({"held": [{"conversation_id": "room", "account": "kim", "for": ["alice"]}]});
        assert_eq!(lender_for(&g, "room", &people(&["alice"])).as_deref(), Some("kim"));
        assert_eq!(lender_for(&g, "room", &people(&["kim"])).as_deref(), Some("kim"), "the lender's own turn");
        assert!(lender_for(&g, "room", &people(&["bob"])).is_none(), "bob never asked");
        assert!(lender_for(&g, "room", &people(&["alice", "bob"])).is_none(), "bob relaying alice");
        assert!(lender_for(&g, "room", &people(&[])).is_none(), "nobody in particular");
        assert_eq!(lender_for(&g, "room", &ReadsFor::All).as_deref(), Some("kim"), "the owner's circle");
        // Among several lenders, the one whose ticket is this turn's.
        let two = json!({"held": [
            {"conversation_id": "room", "account": "kim", "for": ["alice"], "expires_at": 9},
            {"conversation_id": "room", "account": "lee", "for": ["bob"], "expires_at": 5},
        ]});
        assert_eq!(lender_for(&two, "room", &people(&["bob"])).as_deref(), Some("lee"));
        assert_eq!(lender_for(&two, "room", &ReadsFor::All).as_deref(), Some("kim"), "the longest-lived");
    }

    /// The prompt names exactly the tickets the turn may use, and an ask still
    /// waiting is only the business of whom it is for.
    #[test]
    fn a_turn_is_told_only_of_its_own_tickets_and_asks() {
        let g = json!({
            "held": [
                {"conversation_id": "r-alice", "title": "Alice's", "kind": "direct", "participant_count": 2, "account": "kim", "for": ["alice"], "expires_at": 0},
                {"conversation_id": "r-owner", "title": "Owner's", "kind": "direct", "participant_count": 2, "account": "ops", "expires_at": 0},
            ],
            "pending": [{"conversation_id": "r-wait", "asked": "kim", "for": ["alice"]}, {"conversation_id": "r-old", "asked": "kim"}],
        });
        let alice = access_block(&g, &people(&["alice"])).expect("alice's ticket");
        assert!(alice.contains("r-alice") && !alice.contains("r-owner"), "{alice}");
        assert!(access_block(&g, &people(&["bob"])).is_none(), "no block at all for a turn with none");
        let all = access_block(&g, &ReadsFor::All).unwrap();
        assert!(all.contains("r-alice") && all.contains("r-owner"));
        assert!(pending_for(&g, "r-wait", &people(&["alice"])).is_some());
        assert!(pending_for(&g, "r-wait", &people(&["bob"])).is_none());
        assert!(pending_for(&g, "r-old", &people(&["alice"])).is_none(), "an ask from before `for` is the circle's");
        assert!(pending_for(&g, "r-old", &ReadsFor::All).is_some());
    }

    /// The same hole without any ticket: the agent's OWN other rooms — the
    /// owner's DM with it, someone else's. In a stranger's turn it opens the
    /// room the turn is in and rooms the stranger sits in themselves.
    #[test]
    fn a_strangers_turn_cannot_open_the_agents_other_rooms() {
        let row = |id: &str, who: &[&str]| json!({"id": id, "participants": who.iter().map(|u| json!({"username": u})).collect::<Vec<_>>()});
        let owners_dm = row("dm", &["ops", "ops:claude"]);
        let shared = row("g2", &["Stranger", "ops:claude", "ann"]);
        let s = people(&["stranger"]);
        assert!(!may_read_room(Some(&owners_dm), "dm", Some("dev"), &s), "the owner's DM, from a stranger's turn");
        assert!(may_read_room(Some(&shared), "g2", Some("dev"), &s), "a room the stranger is in");
        assert!(may_read_room(None, "dev", Some("dev"), &s), "the room the turn is in");
        assert!(!may_read_room(None, "elsewhere", Some("dev"), &s), "a room nobody can vouch for");
        assert!(!may_read_room(Some(&shared), "g2", Some("dev"), &people(&["stranger", "bob"])), "bob isn't in it");
        assert!(!may_read_room(Some(&shared), "g2", Some("dev"), &people(&[])));
        assert!(may_read_room(Some(&owners_dm), "dm", Some("dev"), &ReadsFor::All));
    }

    /// Only a turn in an untitled channel is told to name it, with both ids
    /// in the command; a named channel and an unknown one cost nothing.
    #[test]
    fn only_an_untitled_channel_asks_to_be_named() {
        let list = json!([
            {"id": "c-new", "name": "未命名", "untitled": true},
            {"id": "c-old", "name": "发版"},
        ]);
        let block = untitled_block_from(&list, "conv-1", "c-new").expect("untitled");
        assert!(block.contains("mafold channels rename conv-1 c-new "), "{block}");
        // Named first, never after the reply: the answer is the turn's last word.
        assert!(block.contains("FIRST step") && !block.contains("Once you have replied"), "{block}");
        assert!(untitled_block_from(&list, "conv-1", "c-old").is_none());
        assert!(untitled_block_from(&list, "conv-1", "gone").is_none());
        // The list arrives either bare or wrapped in `items`.
        let wrapped = json!({"items": list});
        assert!(untitled_block_from(&wrapped, "conv-1", "c-new").is_some());
    }

    /// `mafold msg` / `mafold state get` on the bot's own reply: the server
    /// hands the author every item, the agent is shown only those whose read
    /// lists name it — an owner's summary or a note for one person printed
    /// into a turn would go to whoever asked.
    #[test]
    fn an_agent_is_shown_the_items_its_lists_name_it_for() {
        let item = |read: Value| json!({ "v": 1, "read": read, "write": ["sender"], "by": "ops:claude", "rev": 1 });
        let mut items = json!({
            "a.note": item(json!(["@linsky"])),
            "a.summary": item(json!(["owner"])),
            "a.tally": item(json!(["sender"])),
            "a.open": item(json!(["all"])),
            "a.mine": item(json!(["@Ops:Claude"])),
            "a.receipt": item(json!(["server"])),
        });
        let items = items.as_object_mut().unwrap();
        named_for(items, &json!({ "username": "ops:claude" }), "ops:claude");
        let mut kept: Vec<&str> = items.keys().map(String::as_str).collect();
        kept.sort();
        assert_eq!(kept, vec!["a.mine", "a.open", "a.tally"]);

        // The owner reading their bot's reply is named by `owner`.
        let mut theirs = json!({ "a.summary": item(json!(["owner"])), "a.tally": item(json!(["sender"])) });
        named_for(theirs.as_object_mut().unwrap(), &json!({ "username": "ops:claude" }), "ops");
        assert_eq!(theirs.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["a.summary"]);
        // Nobody known: only what everyone reads.
        let mut nobody = json!({ "a.open": item(json!(["all"])), "a.tally": item(json!(["sender"])) });
        named_for(nobody.as_object_mut().unwrap(), &json!({ "username": "ops:claude" }), "");
        assert_eq!(nobody.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["a.open"]);
    }
}
