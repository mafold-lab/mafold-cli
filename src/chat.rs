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
        /// Conversation id, @username (the DM), or a name from `mafold chats`.
        chat: String,
        /// Who to address the ask to. Defaults to a participant you already
        /// share a conversation with; required when that is ambiguous.
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
    pub json: bool,
    pub media: bool,
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
    let channel = match &a.channel {
        Some(c) => Some(resolve_channel(client, &chat.id, c).await?),
        None => None,
    };

    // Try as myself first. A participant never touches the grant table, so the
    // ordinary case costs exactly one request.
    let mine = client.chat_history(&chat.id, a.limit, channel.as_deref(), None).await;
    let (page, lender) = match mine {
        Ok(p) => (p, None),
        Err(e) if !is_permission_error(&e) => return Err(e),
        Err(_) => {
            let grants = client.list_chat_grants().await.unwrap_or_else(|_| json!({}));
            match held_grantor(&grants, &chat.id) {
                Some(g) => {
                    let p = client
                        .chat_history(&chat.id, a.limit, channel.as_deref(), Some(&g))
                        .await
                        .with_context(|| format!("reading as @{g}"))?;
                    (p, Some(g))
                }
                None => return no_ticket(client, &grants, &chat, a.limit).await,
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
    print_transcript(&chat, lender.as_deref(), &page, &files);
    Ok(())
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
    let dir = crate::agent::attachments_dir();
    let _ = std::fs::create_dir_all(&dir);
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
            let path = dir.join(&name);
            if path.is_file() {
                out.insert(id.to_string(), path.to_string_lossy().into_owned());
                continue;
            }
            match client.download(&format!("/media/{id}")).await {
                Ok(bytes) => {
                    if std::fs::write(&path, &bytes).is_ok() {
                        out.insert(id.to_string(), path.to_string_lossy().into_owned());
                    }
                }
                Err(e) => eprintln!("· 取不到附件 {name}: {e}"),
            }
        }
    }
    out
}

/// No live ticket: say what is already in flight, or ask — and never just fail.
async fn no_ticket(client: &Client, grants: &Value, chat: &Room, limit: usize) -> Result<()> {
    if let Some(p) = pending_for(grants, &chat.id) {
        let asked = p["asked"].as_str().unwrap_or("");
        let at = p["asked_at"].as_str().unwrap_or("");
        eprintln!("⏳ 已经问过了,还没人回应 —— 卡还挂在「{}」里", chat.label);
        eprintln!("   问的是 @{asked} · {at}");
        eprintln!("   别再问一次。等对方点了「允许」,重跑这条命令。");
        std::process::exit(EXIT_PENDING);
    }
    match ask(client, &chat.id, None, 7, None).await {
        Ok(r) if r["granted"].as_bool() == Some(true) => {
            // Raced a grant between the refusal and the ask.
            let g = r["grantor"].as_str().map(str::to_string);
            let page = client.chat_history(&chat.id, limit, None, g.as_deref()).await?;
            print_transcript(chat, g.as_deref(), &page, &Default::default());
            Ok(())
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
) -> Result<Value> {
    if let Some(u) = from {
        return client.request_chat_access(conv, u, days, for_account).await;
    }
    let candidates = neighbours(client).await;
    if candidates.is_empty() {
        bail!("不知道该问谁 —— 用 --from @某人 指定一个那个房间里的人");
    }
    let mut last = None;
    for u in &candidates {
        match client.request_chat_access(conv, u, days, for_account).await {
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
    for m in &items {
        let who = m["sender"]["username"].as_str().unwrap_or("?");
        let ts = m["created_at"].as_str().unwrap_or("");
        let when = ts.get(5..16).unwrap_or(ts).replace('T', " ");
        let body = readable_body(m["content"].as_str().unwrap_or(""));
        println!("[{when}] {who}: {}", if body.is_empty() { "—".into() } else { body });
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

fn attachment_name(a: &Value) -> String {
    match a["kind"].as_str().unwrap_or("") {
        "photo" => "图片".to_string(),
        "video" => "视频".to_string(),
        "chat_record" => "聊天记录".to_string(),
        _ => a["file"]["filename"].as_str().unwrap_or("附件").to_string(),
    }
}

/// A message body as a person reads it, not as Markdoc stores it.
///
/// A card in a bubble is a rendered thing everywhere else in Mafold; dumped
/// into a terminal it is a line of tag soup that buries whatever text sat
/// beside it. The live smoke test made this obvious — a consent card came back
/// as sixty characters of `requester="…" conv="…" state="granted"`, three rows
/// of the transcript spent saying nothing a reader could use.
///
/// A forwarded chat record is the exception and gets EXPANDED, not collapsed:
/// its body is the frozen conversation, which is exactly what someone reading
/// a transcript asked for. That reuses the daemon's own renderer, so a record
/// reads the same here as it does in a prompt.
fn readable_body(text: &str) -> String {
    let mut photos = vec![];
    let flattened = crate::agent::flatten_body_records(text, &mut photos);
    let mut out = String::new();
    let mut rest = flattened.as_str();
    while let Some(i) = rest.find("{%") {
        let Some(end) = rest[i..].find("%}").map(|k| i + k + 2) else { break };
        out.push_str(&rest[..i]);
        let inner = rest[i + 2..end - 2].trim().trim_start_matches('/').trim();
        let name = inner.split_whitespace().next().unwrap_or("card");
        // A closing tag (`{% /x %}`) would otherwise print a second marker for
        // one card; the opener already said everything.
        if !rest[i + 2..end - 2].trim_start().starts_with('/') {
            out.push_str(&format!("[卡片:{name}]"));
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ─────────────────────────── mafold access ───────────────────────────

pub async fn run(cmd: AccessCmd, client: &Client) -> Result<()> {
    match cmd {
        AccessCmd::Request { chat, from, days, for_account } => {
            let room = resolve_room(client, &chat).await?;
            let r = ask(client, &room.id, from.as_deref(), days, for_account.as_deref()).await?;
            let who = for_account.as_deref().unwrap_or("你");
            if r["granted"].as_bool() == Some(true) {
                println!("✓ {who} 已经能读「{}」了 —— 不用再问", room.label);
            } else if r["pending"].as_bool() == Some(true) {
                println!("⏳ 已经问过了,卡还挂在「{}」里,没人回应", room.label);
                std::process::exit(EXIT_PENDING);
            } else {
                println!("同意卡已发到「{}」,受权人写的是 {who}", room.label);
                println!("这个房间里任何一个人点「允许」都算数 —— 我不能替它点头。");
            }
            Ok(())
        }
        AccessCmd::Status => {
            let g = client.list_chat_grants().await?;
            let held = g["held"].as_array().cloned().unwrap_or_default();
            let pending = g["pending"].as_array().cloned().unwrap_or_default();
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
/// been through this once already with `mafold room`, which sat unused until
/// the installed apps were named in the prompt. So `chat.read` gets the same
/// treatment, in the same place, in the same shape.
///
/// Deliberately NOT the transcripts. Folding five rooms' history into every
/// system prompt would burn the context window on turn one and be wasted on
/// the nine turns in ten that never look at another room. A name and the one
/// command that fetches it is the whole job.
///
/// `None` when there are no tickets — no tickets, no prompt overhead, exactly
/// like the apps block.
pub async fn context_block(client: &Client) -> Option<String> {
    let g = client.list_chat_grants().await.ok()?;
    let held = g["held"].as_array().filter(|a| !a.is_empty())?;
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
    if is_uuid(a) {
        // A room I may not be in: name it from the grant list if it's there,
        // and otherwise say the uuid rather than inventing a title.
        if let Ok(list) = client.chats().await {
            if let Some(c) = list["items"].as_array().into_iter().flatten().find(|c| c["id"].as_str() == Some(a)) {
                return Ok(room_from(client, c).await);
            }
        }
        if let Ok(g) = client.list_chat_grants().await {
            for key in ["held", "granted"] {
                if let Some(r) = g[key].as_array().into_iter().flatten().find(|r| r["conversation_id"].as_str() == Some(a)) {
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
    let items = list["items"].as_array().cloned().unwrap_or_default();
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
        // Not in the list yet — `startChat` opens (or finds) the DM.
        let id = client.resolve_chat(a).await?;
        return Ok(Room { id, label: format!("@{needle}"), shape: "私聊".into() });
    }

    let hits: Vec<&Value> = items
        .iter()
        .filter(|c| label_of(c, &my).to_lowercase().contains(&needle))
        .collect();
    match hits.len() {
        1 => Ok(room_from(client, hits[0]).await),
        0 => {
            let id = client.resolve_chat(a).await?;
            Ok(Room { id, label: a.into(), shape: String::new() })
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

fn held_grantor(grants: &Value, conv: &str) -> Option<String> {
    grants["held"]
        .as_array()?
        .iter()
        .find(|r| r["conversation_id"].as_str() == Some(conv))?["account"]
        .as_str()
        .map(str::to_string)
}

fn pending_for(grants: &Value, conv: &str) -> Option<Value> {
    grants["pending"]
        .as_array()?
        .iter()
        .find(|r| r["conversation_id"].as_str() == Some(conv))
        .cloned()
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
        .find(|c| c["name"].as_str().map(str::to_lowercase).as_deref() == Some(&n.to_lowercase()))
        .and_then(|c| c["id"].as_str().map(str::to_string))
        .with_context(|| format!("no channel called #{n}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// A transcript row must read as something somebody SAID.
    ///
    /// The live smoke test printed a consent card as sixty characters of raw
    /// Markdoc — three rows of transcript saying nothing usable. A card is a
    /// rendered object everywhere else in Mafold; in a terminal the honest
    /// projection is its name.
    #[test]
    fn card_markup_collapses_but_a_forwarded_record_expands() {
        let card = readable_body(
            r#"看这个 {% mafold/chat-grant requester="ops:cc" conv="abc" state="granted" /%} 好了"#,
        );
        assert_eq!(card, "看这个 [卡片:mafold/chat-grant] 好了");
        assert!(!card.contains("requester"), "tag soup survived: {card}");

        // A paired open/close is ONE card, not two markers.
        let paired = readable_body("{% mafold/run summary=\"x\" %}body{% /mafold/run %}");
        assert_eq!(paired.matches("[卡片:").count(), 1, "got {paired}");

        // Plain prose is untouched apart from whitespace normalising.
        assert_eq!(readable_body("  预算表放 Notion 了  "), "预算表放 Notion 了");

        // A forwarded chat record is the payload, so it is expanded rather
        // than reduced to its name — the daemon's renderer, reused.
        let rec = readable_body(
            r#"{% mafold/chatrecord title="站会" %}[{"sender_username":"layg","content":"差 3 行"}]{% /mafold/chatrecord %}"#,
        );
        assert!(rec.contains("差 3 行"), "record was collapsed instead of expanded: {rec}");
    }

    /// `held` names the LENDER, and only for the room asked about.
    #[test]
    fn held_grantor_is_per_room() {
        let g = json!({"held": [
            {"conversation_id": "aaa", "account": "layg"},
            {"conversation_id": "bbb", "account": "kim"},
        ]});
        assert_eq!(held_grantor(&g, "bbb").as_deref(), Some("kim"));
        assert!(held_grantor(&g, "ccc").is_none());
        assert!(held_grantor(&json!({}), "aaa").is_none());
    }
}
