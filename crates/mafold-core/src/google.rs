//! Google — Gmail and Calendar, driven natively.
//!
//! Why a driver and not Google's own MCP servers (`gmailmcp.googleapis.com`,
//! `calendarmcp.googleapis.com`): they are Developer Preview only, the Gmail
//! one cannot send (drafts only) and demands the restricted `gmail.compose`,
//! and they are two endpoints — two links for one account. The OAuth half of
//! the row (a brokered fixed client) is exactly what an MCP row would need
//! too; the day those servers do the job, the row gains an `mcp_url`, drops
//! this driver, and linking does not change.
//!
//! Four tools, each the narrowest request that does its job. Nothing here can
//! change a calendar, and not because the list below happens to lack a tool
//! for it: the GRANT is `calendar.readonly`, so Google itself refuses a write.
//! The catalog is the second lock, not the first.
//!
//! The access token never leaves this module in anything it returns. What an
//! agent gets back is mail and events, shaped for reading — never the
//! credential that fetched them.

use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde_json::{json, Map, Value};

use crate::connections::{Result, Runtime};
use crate::mcp::MethodSpec;
use crate::net;
use mafold_types::connections::ProviderInfo;

/// The name the registry row uses in `native_api`.
pub const DRIVER: &str = "google-workspace";

const GMAIL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
const CALENDAR: &str = "https://www.googleapis.com/calendar/v3";

/// Long enough for any mail a person writes; short enough that a newsletter
/// does not eat an agent's whole context. Truncation is reported, never silent.
const MAX_BODY_CHARS: usize = 20_000;
const MAX_SEARCH: u64 = 25;
const MAX_EVENTS: u64 = 50;

/// One tool, as both the catalog and dispatch need it.
pub struct Method {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    /// A fact about code in this repo, not a third party's self-report:
    /// `gmail.send` is the only tool that changes anything.
    pub read_only: bool,
}

pub const METHODS: &[Method] = &[
    Method {
        name: "gmail.search",
        description: "Search the mailbox with Gmail's own query syntax (from:, to:, subject:, \
                      is:unread, newer_than:2d, has:attachment …) and list the matches newest \
                      first — sender, subject, date and a snippet, not bodies. An empty query \
                      lists the latest mail. Read a match with gmail.read.",
        schema: search_schema,
        read_only: true,
    },
    Method {
        name: "gmail.read",
        description: "Read one message by its id (from gmail.search): headers, the plain-text \
                      body (HTML mail is reduced to text), and the names of any attachments.",
        schema: read_schema,
        read_only: true,
    },
    Method {
        name: "gmail.send",
        description: "Send a plain-text email from the connected Gmail account. Pass reply_to \
                      (a message id) to send it as a reply in that thread. This really sends — \
                      confirm the recipients and wording first.",
        schema: send_schema,
        read_only: false,
    },
    Method {
        name: "calendar.events",
        description: "List events on the primary calendar between two times (RFC 3339; default \
                      now → 7 days ahead), soonest first. Read-only: this connection cannot \
                      create, change or answer events.",
        schema: events_schema,
        read_only: true,
    },
];

fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Gmail search query; empty = latest mail" },
            "max": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH, "default": 10 }
        }
    })
}

fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "id": { "type": "string", "description": "A message id from gmail.search" } },
        "required": ["id"]
    })
}

fn send_schema() -> Value {
    let addrs = json!({
        "oneOf": [
            { "type": "string" },
            { "type": "array", "items": { "type": "string" } }
        ],
        "description": "Plain addresses (name@example.com), one or several"
    });
    json!({
        "type": "object",
        "properties": {
            "to": addrs,
            "cc": addrs,
            "subject": { "type": "string" },
            "body": { "type": "string", "description": "Plain text" },
            "reply_to": { "type": "string", "description": "Message id to reply to (threads it)" }
        },
        "required": ["to", "body"]
    })
}

fn events_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "from": { "type": "string", "description": "RFC 3339 start, default now" },
            "to": { "type": "string", "description": "RFC 3339 end; default 7 days from now (open-ended when only `from` is given)" },
            "query": { "type": "string", "description": "Free-text filter" },
            "max": { "type": "integer", "minimum": 1, "maximum": MAX_EVENTS, "default": 20 }
        }
    })
}

pub fn method_specs() -> Vec<MethodSpec> {
    METHODS
        .iter()
        .map(|m| MethodSpec {
            name: m.name.to_string(),
            title: m.name.to_string(),
            description: m.description.to_string(),
            input_schema: (m.schema)(),
            read_only: m.read_only,
        })
        .collect()
}

/// The `tools/list` answer, in the shape every other catalog uses.
pub fn catalog() -> Value {
    json!({
        "tools": METHODS
            .iter()
            .map(|m| json!({
                "name": m.name,
                "title": m.name,
                "description": m.description,
                "inputSchema": (m.schema)(),
                "readOnly": m.read_only,
            }))
            .collect::<Vec<_>>()
    })
}

/// Where the two REST surfaces live. A value rather than constants so a test
/// can point both at a local server.
pub(crate) struct Api {
    pub gmail: String,
    pub calendar: String,
}

impl Api {
    fn google() -> Self {
        Api { gmail: GMAIL.into(), calendar: CALENDAR.into() }
    }
}

/// How a request can fail, split by the ONE distinction the caller acts on.
#[derive(Debug)]
pub(crate) enum Fail {
    /// 401: the credential was refused — renew once, then decide.
    Unauthorized(String),
    /// Anything else, already worded for the person reading it.
    Other(String),
}

/// Run one tool on a `google` connection.
pub(crate) async fn run(
    rt: &Runtime,
    name: &str,
    conn: &Value,
    spec: &ProviderInfo,
    method: &str,
    params: &Value,
) -> Result<Value> {
    run_at(rt, &Api::google(), name, conn, spec, method, params).await
}

pub(crate) async fn run_at(
    rt: &Runtime,
    api: &Api,
    name: &str,
    conn: &Value,
    spec: &ProviderInfo,
    method: &str,
    params: &Value,
) -> Result<Value> {
    let mut payload = rt.refreshed_payload(name, conn, spec).await?;
    // At most two attempts: the token we had, and — if Google refused it —
    // one renewed. `recover_refused` settles a revoked or expired grant (and
    // marks the row); a refusal that survives a renewal is reported as such.
    // Nothing is sent twice: a refused request did not happen.
    let mut renewed = false;
    loop {
        let token = payload.get("access_token").and_then(Value::as_str).unwrap_or("").to_string();
        match dispatch(api, &token, method, params).await {
            Ok(v) => return Ok(v),
            Err(Fail::Unauthorized(detail)) if !renewed => {
                payload = rt.recover_refused(name, conn, spec, &payload, &detail).await?;
                renewed = true;
            }
            Err(Fail::Unauthorized(detail)) => {
                return Err(crate::connections::still_refused_msg(name, spec, &detail))
            }
            Err(Fail::Other(m)) => return Err(m),
        }
    }
}

pub(crate) async fn dispatch(api: &Api, token: &str, method: &str, params: &Value) -> std::result::Result<Value, Fail> {
    match method {
        "gmail.search" => search(api, token, params).await,
        "gmail.read" => read(api, token, params).await,
        "gmail.send" => send(api, token, params).await,
        "calendar.events" => events(api, token, params).await,
        other => Err(Fail::Other(format!(
            "Google offers {} — `{other}` is not one of them",
            METHODS.iter().map(|m| m.name).collect::<Vec<_>>().join(", ")
        ))),
    }
}

// ── the four tools ─────────────────────────────────────────────────────────

async fn search(api: &Api, token: &str, p: &Value) -> std::result::Result<Value, Fail> {
    let q = p.get("query").and_then(Value::as_str).unwrap_or("").trim();
    let max = p.get("max").and_then(Value::as_u64).unwrap_or(10).clamp(1, MAX_SEARCH);
    let mut url = format!("{}/messages?maxResults={max}", api.gmail);
    if !q.is_empty() {
        url.push_str(&format!("&q={}", crate::connections::form_encode(q)));
    }
    let list = get(&url, token).await?;
    let ids: Vec<String> = list
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).map(str::to_string).collect())
        .unwrap_or_default();
    // One metadata read per hit, in order. Sequential on purpose: at most
    // `MAX_SEARCH` small reads, and a burst of parallel ones is exactly what
    // trips Gmail's per-user rate limit.
    let mut out = Vec::with_capacity(ids.len());
    for id in &ids {
        check_id(id)?;
        let m = get(
            &format!(
                "{}/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=To\
                 &metadataHeaders=Subject&metadataHeaders=Date",
                api.gmail
            ),
            token,
        )
        .await?;
        out.push(summary(&m));
    }
    Ok(json!({ "messages": out, "more": list.get("nextPageToken").is_some() }))
}

async fn read(api: &Api, token: &str, p: &Value) -> std::result::Result<Value, Fail> {
    let id = p.get("id").and_then(Value::as_str).unwrap_or("").trim();
    check_id(id)?;
    let m = get(&format!("{}/messages/{id}?format=full", api.gmail), token).await?;
    let (body, truncated, attachments) = message_text(m.get("payload").unwrap_or(&Value::Null));
    let mut out = summary(&m);
    if let Value::Object(o) = &mut out {
        o.remove("snippet");
        o.insert("cc".into(), json!(header(&m, "Cc")));
        o.insert("body".into(), json!(body));
        o.insert("truncated".into(), json!(truncated));
        o.insert("attachments".into(), json!(attachments));
    }
    Ok(out)
}

async fn send(api: &Api, token: &str, p: &Value) -> std::result::Result<Value, Fail> {
    let to = addresses(p.get("to"));
    let cc = addresses(p.get("cc"));
    let body = p.get("body").and_then(Value::as_str).unwrap_or("");
    let mut subject = p.get("subject").and_then(Value::as_str).unwrap_or("").to_string();
    let mut thread = None;
    let mut threading = None;
    if let Some(rid) = p.get("reply_to").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        check_id(rid)?;
        let orig = get(
            &format!(
                "{}/messages/{rid}?format=metadata&metadataHeaders=Message-ID\
                 &metadataHeaders=References&metadataHeaders=Subject",
                api.gmail
            ),
            token,
        )
        .await?;
        let msgid = header(&orig, "Message-ID");
        if !msgid.is_empty() {
            let refs = header(&orig, "References");
            let refs = if refs.is_empty() { msgid.clone() } else { format!("{refs} {msgid}") };
            threading = Some((msgid, refs));
        }
        if subject.trim().is_empty() {
            let s = header(&orig, "Subject");
            subject = if s.to_ascii_lowercase().starts_with("re:") { s } else { format!("Re: {s}") };
        }
        thread = orig.get("threadId").and_then(Value::as_str).map(str::to_string);
    }
    let raw = build_mime(&to, &cc, &subject, body, threading.as_ref().map(|(a, b)| (a.as_str(), b.as_str())))
        .map_err(Fail::Other)?;
    let mut req = json!({ "raw": URL_SAFE_NO_PAD.encode(raw.as_bytes()) });
    if let Some(t) = thread {
        req["threadId"] = json!(t);
    }
    let sent = post(&format!("{}/messages/send", api.gmail), token, &req).await?;
    Ok(json!({
        "id": sent.get("id"),
        "thread_id": sent.get("threadId"),
        "to": to,
        "cc": cc,
        "subject": subject,
    }))
}

async fn events(api: &Api, token: &str, p: &Value) -> std::result::Result<Value, Fail> {
    let arg = |k: &str| -> std::result::Result<Option<String>, Fail> {
        match p.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) if s.chars().any(char::is_control) => {
                Err(Fail::Other(format!("`{k}` can't contain control characters")))
            }
            other => Ok(other.map(str::to_string)),
        }
    };
    let now = crate::connections::now_ms();
    let from = arg("from")?;
    // A window by default: now → a week out. An explicit start with no end
    // is left open-ended rather than guessed at.
    let to = match (&from, arg("to")?) {
        (_, Some(t)) => Some(t),
        (None, None) => Some(rfc3339(now + 7 * 86_400_000)),
        (Some(_), None) => None,
    };
    let from = from.unwrap_or_else(|| rfc3339(now));
    let max = p.get("max").and_then(Value::as_u64).unwrap_or(20).clamp(1, MAX_EVENTS);
    let enc = crate::connections::form_encode;
    let mut url = format!(
        "{}/calendars/primary/events?singleEvents=true&orderBy=startTime&maxResults={max}&timeMin={}",
        api.calendar,
        enc(&from)
    );
    if let Some(t) = to {
        url.push_str(&format!("&timeMax={}", enc(&t)));
    }
    if let Some(q) = arg("query")? {
        url.push_str(&format!("&q={}", enc(&q)));
    }
    let list = get(&url, token).await?;
    let events: Vec<Value> = list
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|e| {
                    let at = |which: &str| {
                        e.pointer(&format!("/{which}/dateTime"))
                            .or_else(|| e.pointer(&format!("/{which}/date")))
                            .cloned()
                            .unwrap_or(Value::Null)
                    };
                    json!({
                        "id": e.get("id"),
                        "summary": e.get("summary"),
                        "start": at("start"),
                        "end": at("end"),
                        "all_day": e.pointer("/start/date").is_some(),
                        "location": e.get("location"),
                        "status": e.get("status"),
                        "organizer": e.pointer("/organizer/email"),
                        "attendees": e.get("attendees").and_then(Value::as_array).map_or(0, Vec::len),
                        "link": e.get("htmlLink"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "events": events,
        "time_zone": list.get("timeZone"),
        "more": list.get("nextPageToken").is_some(),
    }))
}

// ── HTTP ───────────────────────────────────────────────────────────────────

fn bearer(token: &str) -> Vec<(String, String)> {
    vec![("authorization".into(), format!("Bearer {token}"))]
}

async fn get(url: &str, token: &str) -> std::result::Result<Value, Fail> {
    let reply = net::http_get(url, &bearer(token))
        .await
        .map_err(|e| Fail::Other(format!("couldn't reach Google: {e}")))?;
    answer(reply)
}

async fn post(url: &str, token: &str, body: &Value) -> std::result::Result<Value, Fail> {
    let mut headers = bearer(token);
    headers.push(("content-type".into(), "application/json".into()));
    let reply = net::http_post(url, &headers, &body.to_string())
        .await
        .map_err(|e| Fail::Other(format!("couldn't reach Google: {e}")))?;
    answer(reply)
}

/// Google's answer, with its failures worded for whoever reads them next —
/// usually a model deciding what to tell a person.
fn answer(reply: net::HttpReply) -> std::result::Result<Value, Fail> {
    if (200..300).contains(&reply.status) {
        if reply.body.trim().is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&reply.body)
            .map_err(|_| Fail::Other("Google answered with something that isn't JSON".into()));
    }
    let msg = serde_json::from_str::<Value>(&reply.body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| reply.body.chars().take(200).collect());
    let scope_missing = msg.contains("insufficient authentication scopes")
        || reply.body.contains("ACCESS_TOKEN_SCOPE_INSUFFICIENT")
        || reply.body.contains("insufficientPermissions");
    match reply.status {
        401 => Err(Fail::Unauthorized(msg)),
        // Not an expired grant: Google's consent screen lets each permission be
        // unticked, and this one was. Reconnecting fixes it only if the box is
        // left ticked, so that is what this says.
        403 if scope_missing => Err(Fail::Other(
            "Google refused: this connection wasn't granted that permission (Google's consent \
             screen lets each box be unticked). Reconnect it in Settings ▸ Connections and leave \
             every box ticked."
                .into(),
        )),
        429 => Err(Fail::Other("Google is rate-limiting this account right now — try again in a minute.".into())),
        s => Err(Fail::Other(format!("Google answered HTTP {s}: {msg}"))),
    }
}

// ── shaping ────────────────────────────────────────────────────────────────

/// A message id goes into a URL path. Gmail's ids are short and
/// alphanumeric; anything else is refused before a request is made.
fn check_id(id: &str) -> std::result::Result<(), Fail> {
    if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Ok(())
    } else {
        Err(Fail::Other(format!("`{id}` isn't a Gmail message id — take one from gmail.search")))
    }
}

fn header(m: &Value, name: &str) -> String {
    m.pointer("/payload/headers")
        .and_then(Value::as_array)
        .and_then(|hs| {
            hs.iter().find(|h| h.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name)))
        })
        .and_then(|h| h.get("value").and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn summary(m: &Value) -> Value {
    let unread = m
        .get("labelIds")
        .and_then(Value::as_array)
        .is_some_and(|l| l.iter().any(|x| x.as_str() == Some("UNREAD")));
    json!({
        "id": m.get("id"),
        "thread_id": m.get("threadId"),
        "from": header(m, "From"),
        "to": header(m, "To"),
        "subject": header(m, "Subject"),
        "date": header(m, "Date"),
        // Gmail escapes its snippets as HTML.
        "snippet": decode_entities(m.get("snippet").and_then(Value::as_str).unwrap_or("")),
        "unread": unread,
    })
}

/// `"a@b.co, c@d.co"` or `["a@b.co", …]` → one address per entry.
fn addresses(v: Option<&Value>) -> Vec<String> {
    let parts: Vec<String> = match v {
        Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    parts.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// An RFC 2822 message, ready to be base64url'd into Gmail's `raw`.
fn build_mime(
    to: &[String],
    cc: &[String],
    subject: &str,
    body: &str,
    reply: Option<(&str, &str)>,
) -> Result<String> {
    if to.is_empty() {
        return Err("a message needs at least one recipient (`to`)".into());
    }
    for a in to.iter().chain(cc) {
        check_address(a)?;
    }
    // Every header below is built from text an agent supplied — and an agent
    // may be relaying a prompt-injected mail. A line break in any value would
    // let that text add headers of its own (a `Bcc:` to somewhere else).
    let breaks = |s: &str| s.contains(['\r', '\n']);
    if breaks(subject) {
        return Err("the subject can't contain a line break".into());
    }
    let mut h = format!("To: {}\r\n", to.join(", "));
    if !cc.is_empty() {
        h.push_str(&format!("Cc: {}\r\n", cc.join(", ")));
    }
    h.push_str(&format!("Subject: {}\r\n", encode_header(subject)));
    if let Some((in_reply_to, references)) = reply {
        if breaks(in_reply_to) || breaks(references) {
            return Err("the original message's headers are malformed".into());
        }
        h.push_str(&format!("In-Reply-To: {in_reply_to}\r\nReferences: {references}\r\n"));
    }
    h.push_str("MIME-Version: 1.0\r\n");
    h.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    h.push_str("Content-Transfer-Encoding: base64\r\n");
    // Base64 keeps every byte of every script intact and every line short;
    // CRLF is what mail means by a line break.
    let text = body.replace("\r\n", "\n").replace('\n', "\r\n");
    let b64 = STANDARD.encode(text.as_bytes());
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    Ok(format!("{h}\r\n{}\r\n", lines.join("\r\n")))
}

/// One plain address. Display names are left out on purpose: they are where
/// non-ASCII, quoting, and header tricks live, and a bot sending mail needs
/// none of them.
fn check_address(a: &str) -> Result<()> {
    let bad = || format!("`{a}` isn't an email address — pass plain addresses like name@example.com");
    if !a.is_ascii() || a.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b"<>,;\"()".contains(&b)) {
        return Err(bad());
    }
    match a.split_once('@') {
        Some((local, domain)) if !local.is_empty() && domain.contains('.') && !domain.contains('@') => Ok(()),
        _ => Err(bad()),
    }
}

/// RFC 2047: ASCII passes through; anything else becomes `=?UTF-8?B?…?=`
/// words of at most 45 bytes each (never splitting a character), folded.
fn encode_header(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for ch in s.chars() {
        if chunk.len() + ch.len_utf8() > 45 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(chunk);
    }
    words
        .iter()
        .map(|w| format!("=?UTF-8?B?{}?=", STANDARD.encode(w.as_bytes())))
        .collect::<Vec<_>>()
        .join("\r\n ")
}

/// The readable text of a `format=full` message payload, whether it was cut,
/// and its attachments' names.
fn message_text(payload: &Value) -> (String, bool, Vec<Value>) {
    let (mut plain, mut html, mut attachments) = (None, None, Vec::new());
    walk(payload, &mut plain, &mut html, &mut attachments);
    let text = plain
        .map(|t| t.replace("\r\n", "\n"))
        .or_else(|| html.map(|h| strip_html(&h)))
        .unwrap_or_default();
    let text = text.trim();
    if text.chars().count() > MAX_BODY_CHARS {
        (text.chars().take(MAX_BODY_CHARS).collect(), true, attachments)
    } else {
        (text.to_string(), false, attachments)
    }
}

/// Depth-first over MIME parts: the first text/plain and first text/html,
/// and every part with a filename as an attachment (named, never fetched).
fn walk(part: &Value, plain: &mut Option<String>, html: &mut Option<String>, attachments: &mut Vec<Value>) {
    let mime = part.get("mimeType").and_then(Value::as_str).unwrap_or("");
    let filename = part.get("filename").and_then(Value::as_str).unwrap_or("");
    if !filename.is_empty() {
        attachments.push(json!({
            "filename": filename,
            "mime_type": mime,
            "size": part.pointer("/body/size").and_then(Value::as_u64).unwrap_or(0),
        }));
        return;
    }
    if let Some(data) = part.pointer("/body/data").and_then(Value::as_str) {
        let decoded = || {
            URL_SAFE_NO_PAD
                .decode(data.trim_end_matches('='))
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        };
        match mime {
            "text/plain" if plain.is_none() => *plain = decoded(),
            "text/html" if html.is_none() => *html = decoded(),
            _ => {}
        }
    }
    if let Some(parts) = part.get("parts").and_then(Value::as_array) {
        for p in parts {
            walk(p, plain, html, attachments);
        }
    }
}

/// HTML mail reduced to text a model can read: scripts, styles and the head
/// dropped, block elements as line breaks, whitespace collapsed, entities
/// decoded. Not a renderer — a reader.
fn strip_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    let bytes = html.as_bytes();
    // Guarantee at least `n` line breaks at the end (never at the start).
    fn brk(out: &mut String, n: usize) {
        while out.ends_with(' ') {
            out.pop();
        }
        if out.is_empty() {
            return;
        }
        let have = out.chars().rev().take_while(|c| *c == '\n').count();
        for _ in have..n {
            out.push('\n');
        }
    }
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(end) = html[i..].find('>').map(|e| i + e) else { break };
            let tag = lower[i + 1..end].trim_start_matches('/');
            let name: String = tag.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            // Whole elements whose content is not text.
            if !lower[i + 1..end].starts_with('/') && matches!(name.as_str(), "script" | "style" | "head" | "title") {
                let close = format!("</{name}");
                i = lower[end..].find(&close).map(|c| end + c).and_then(|c| lower[c..].find('>').map(|e| c + e + 1)).unwrap_or(bytes.len());
                continue;
            }
            match name.as_str() {
                "br" => {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('\n');
                }
                "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "ul" | "ol" | "table" => brk(&mut out, 2),
                "div" | "tr" | "li" | "section" | "article" | "header" | "footer" => brk(&mut out, 1),
                _ => {}
            }
            i = end + 1;
            continue;
        }
        let next = html[i..].find('<').map_or(bytes.len(), |n| i + n);
        // Source whitespace (newlines included) collapses to one space; a
        // non-breaking space is a space the author meant, so it stays.
        for c in decode_entities(&html[i..next]).chars() {
            if c == '\u{a0}' {
                out.push(' ');
            } else if c.is_whitespace() {
                if !out.is_empty() && !out.ends_with([' ', '\n']) {
                    out.push(' ');
                }
            } else {
                out.push(c);
            }
        }
        i = next;
    }
    out.trim().to_string()
}

/// The handful of entities real mail uses, plus numeric ones.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Unix milliseconds as RFC 3339 UTC — what Calendar's `timeMin` wants.
/// Civil-from-days (Howard Hinnant's algorithm), so no date crate is pulled
/// into a core that also ships as wasm.
fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64url(s: &str) -> String {
        URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    fn unmime(raw: &str) -> (String, String) {
        let (head, body) = raw.split_once("\r\n\r\n").expect("headers, blank line, body");
        let body: String = body.split("\r\n").collect();
        let body = String::from_utf8(STANDARD.decode(body.trim()).unwrap()).unwrap();
        (head.to_string(), body)
    }

    // ── what the catalog promises ─────────────────────────────────────────

    #[test]
    fn exactly_four_tools_and_only_sending_changes_anything() {
        let names: Vec<&str> = METHODS.iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["gmail.search", "gmail.read", "gmail.send", "calendar.events"]);
        for m in METHODS {
            assert_eq!(m.read_only, m.name != "gmail.send", "{}", m.name);
        }
        assert!(
            !names.iter().any(|n| n.starts_with("calendar.") && *n != "calendar.events"),
            "the grant is calendar.readonly; no tool may pretend otherwise"
        );
        assert_eq!(catalog()["tools"].as_array().unwrap().len(), 4);
    }

    // ── sending ───────────────────────────────────────────────────────────

    #[test]
    fn a_plain_message_is_utf8_base64_with_its_headers() {
        let raw = build_mime(&["fei@example.com".into()], &[], "Hello", "第一行\nsecond line", None).unwrap();
        let (head, body) = unmime(&raw);
        assert!(head.contains("To: fei@example.com"), "{head}");
        assert!(head.contains("Subject: Hello"), "{head}");
        assert!(head.contains("Content-Type: text/plain; charset=\"UTF-8\""), "{head}");
        assert!(head.contains("Content-Transfer-Encoding: base64"), "{head}");
        assert_eq!(body, "第一行\r\nsecond line", "bare LF becomes CRLF, bytes survive");
        assert!(raw.split("\r\n").all(|l| l.len() <= 998), "RFC 5322 line limit");
    }

    #[test]
    fn a_non_ascii_subject_is_an_encoded_word() {
        let raw = build_mime(&["a@b.co".into()], &[], "测试 Mafold 发信", "x", None).unwrap();
        let (head, _) = unmime(&raw);
        let line = head.lines().find(|l| l.starts_with("Subject:")).unwrap();
        assert!(line.starts_with("Subject: =?UTF-8?B?"), "{line}");
        let decoded: String = head
            .split("=?UTF-8?B?")
            .skip(1)
            .map(|w| String::from_utf8(STANDARD.decode(w.split("?=").next().unwrap()).unwrap()).unwrap())
            .collect();
        assert_eq!(decoded, "测试 Mafold 发信");
    }

    /// A header built from agent-supplied text is an injection point: a CR/LF
    /// in a recipient or subject would let a prompt-injected mail smuggle in a
    /// `Bcc:` line and copy the reply somewhere else.
    #[test]
    fn a_line_break_in_any_header_value_is_refused() {
        assert!(build_mime(&["a@b.co\r\nBcc: evil@x.io".into()], &[], "s", "b", None).is_err());
        assert!(build_mime(&["a@b.co".into()], &["c@d.co\nBcc: e@x.io".into()], "s", "b", None).is_err());
        assert!(build_mime(&["a@b.co".into()], &[], "hi\r\nBcc: evil@x.io", "b", None).is_err());
        assert!(build_mime(&["not-an-address".into()], &[], "s", "b", None).is_err());
        assert!(build_mime(&[], &[], "s", "b", None).is_err(), "no recipient, no mail");
    }

    #[test]
    fn a_reply_carries_the_threading_headers() {
        let raw = build_mime(
            &["a@b.co".into()],
            &[],
            "Re: plan",
            "ok",
            Some(("<m1@mail.gmail.com>", "<m0@x> <m1@mail.gmail.com>")),
        )
        .unwrap();
        let (head, _) = unmime(&raw);
        assert!(head.contains("In-Reply-To: <m1@mail.gmail.com>"), "{head}");
        assert!(head.contains("References: <m0@x> <m1@mail.gmail.com>"), "{head}");
    }

    // ── reading ───────────────────────────────────────────────────────────

    #[test]
    fn a_multipart_message_reads_as_its_plain_part_with_attachments_named() {
        let payload = json!({
            "mimeType": "multipart/mixed",
            "parts": [
                { "mimeType": "multipart/alternative", "parts": [
                    { "mimeType": "text/plain", "body": { "data": b64url("明天 3 点开会。\n— fei") } },
                    { "mimeType": "text/html", "body": { "data": b64url("<p>明天 3 点开会。</p>") } }
                ]},
                { "mimeType": "application/pdf", "filename": "agenda.pdf",
                  "body": { "attachmentId": "att-1", "size": 51234 } }
            ]
        });
        let (text, cut, atts) = message_text(&payload);
        assert_eq!(text, "明天 3 点开会。\n— fei");
        assert!(!cut);
        assert_eq!(atts, vec![json!({ "filename": "agenda.pdf", "mime_type": "application/pdf", "size": 51234 })]);
    }

    #[test]
    fn an_html_only_message_is_reduced_to_text() {
        let payload = json!({ "mimeType": "text/html", "body": { "data": b64url(
            "<html><head><style>p{color:red}</style></head><body><p>Hi&nbsp;fei,</p>\
             <p>Tickets &amp; <b>seats</b> below.<br>Row 3</p><script>track()</script></body></html>"
        ) } });
        let (text, _, _) = message_text(&payload);
        assert_eq!(text, "Hi fei,\n\nTickets & seats below.\nRow 3");
    }

    #[test]
    fn a_long_body_is_cut_and_says_so() {
        let long = "字".repeat(MAX_BODY_CHARS + 50);
        let (text, cut, _) = message_text(&json!({ "mimeType": "text/plain", "body": { "data": b64url(&long) } }));
        assert!(cut);
        assert_eq!(text.chars().count(), MAX_BODY_CHARS);
    }

    #[test]
    fn strip_html_keeps_words_apart_and_decodes_entities() {
        assert_eq!(strip_html("<div>a</div><div>b</div>"), "a\nb");
        assert_eq!(strip_html("x &lt;y&gt; &quot;z&quot; &#39;w&#39;"), "x <y> \"z\" 'w'");
    }

    // ── calendar ──────────────────────────────────────────────────────────

    #[test]
    fn rfc3339_is_utc_and_exact() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z", "a leap day");
        assert_eq!(rfc3339(1_790_000_000_123), "2026-09-21T14:13:20Z");
    }

    // ── against a server ─────────────────────────────────────────────────

    #[cfg(not(target_arch = "wasm32"))]
    fn api_at(base: &str) -> Api {
        Api { gmail: format!("{base}/gmail"), calendar: format!("{base}/calendar") }
    }

    /// A search is a list plus one metadata read per hit — never a body.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_search_lists_headers_and_snippets_not_bodies() {
        use crate::testutil::spawn_mock;
        let meta = |id: &str, subject: &str, unread: bool| {
            json!({
                "id": id, "threadId": "t-1", "snippet": "…",
                "labelIds": if unread { json!(["INBOX", "UNREAD"]) } else { json!(["INBOX"]) },
                "payload": { "headers": [
                    { "name": "From", "value": "Fei <fei@example.com>" },
                    { "name": "Subject", "value": subject },
                    { "name": "Date", "value": "Mon, 29 Sep 2026 10:00:00 +0800" }
                ]}
            })
            .to_string()
        };
        let g = spawn_mock(vec![
            (200, json!({ "messages": [{ "id": "m1", "threadId": "t-1" }, { "id": "m2", "threadId": "t-1" }] }).to_string()),
            (200, meta("m1", "One", true)),
            (200, meta("m2", "Two", false)),
        ]);
        let out = dispatch(&api_at(&g.base), "tok", "gmail.search", &json!({ "query": "from:fei is:unread", "max": 2 }))
            .await
            .unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["subject"], "One");
        assert_eq!(msgs[0]["unread"], true);
        assert_eq!(msgs[1]["unread"], false);
        assert!(msgs[0].get("body").is_none(), "a search returns no bodies");

        let reqs = g.requests.lock().unwrap();
        assert!(reqs[0].path.starts_with("/gmail/messages?"), "{}", reqs[0].path);
        assert!(reqs[0].path.contains("q=from%3Afei%20is%3Aunread"), "{}", reqs[0].path);
        assert!(reqs[0].path.contains("maxResults=2"), "{}", reqs[0].path);
        assert!(reqs.iter().skip(1).all(|r| r.path.contains("format=metadata")), "{reqs:?}");
        assert!(reqs.iter().all(|r| r.auth.as_deref() == Some("Bearer tok")));
    }

    /// Sending posts one `raw` message, and nothing about the credential comes
    /// back in the answer.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn sending_posts_one_raw_message() {
        use crate::testutil::spawn_mock;
        let g = spawn_mock(vec![(200, json!({ "id": "s1", "threadId": "t9", "labelIds": ["SENT"] }).to_string())]);
        let out = dispatch(
            &api_at(&g.base),
            "tok",
            "gmail.send",
            &json!({ "to": "fei@example.com", "subject": "test", "body": "hello" }),
        )
        .await
        .unwrap();
        assert_eq!(out["id"], "s1");
        assert!(!out.to_string().contains("tok"), "the credential never comes back: {out}");
        let reqs = g.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].path, "/gmail/messages/send");
        let sent: Value = serde_json::from_str(&reqs[0].body).unwrap();
        let raw = String::from_utf8(URL_SAFE_NO_PAD.decode(sent["raw"].as_str().unwrap()).unwrap()).unwrap();
        assert!(raw.contains("To: fei@example.com"), "{raw}");
    }

    /// A message id goes into a URL path; anything but Gmail's id alphabet is
    /// refused before a request is made.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_message_id_cannot_walk_the_path() {
        use crate::testutil::spawn_mock;
        let g = spawn_mock(vec![(200, "{}".into())]);
        let err = dispatch(&api_at(&g.base), "tok", "gmail.read", &json!({ "id": "../../settings" })).await;
        assert!(matches!(err, Err(Fail::Other(_))), "{err:?}");
        assert!(g.requests.lock().unwrap().is_empty());
    }

    /// Google's "you didn't grant that" is not "reconnect because it expired":
    /// it is a box the person unticked on the consent screen.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_scope_the_person_unticked_is_said_plainly() {
        use crate::testutil::spawn_mock;
        let g = spawn_mock(vec![(403, json!({ "error": { "code": 403,
            "message": "Request had insufficient authentication scopes.", "status": "PERMISSION_DENIED" } }).to_string())]);
        let err = dispatch(&api_at(&g.base), "tok", "calendar.events", &json!({})).await;
        match err {
            Err(Fail::Other(m)) => assert!(m.contains("tick"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// The whole device path for a revoked grant: Google refuses the token,
    /// one renewal is tried through the broker, the broker relays Google's
    /// `invalid_grant`, the row is marked, and the answer says "reconnect".
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn a_revoked_google_grant_marks_the_row() {
        use crate::testutil::{ok, spawn_mock};
        use crate::vault::{self, Key};
        let g = spawn_mock(vec![(401, json!({ "error": { "code": 401,
            "message": "Request had invalid authentication credentials.", "status": "UNAUTHENTICATED" } }).to_string())]);
        let broker = spawn_mock(vec![(400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#.into())]);
        let mafold = spawn_mock(vec![ok(r#"{"marked":true}"#)]);

        let umk = Key::random();
        let sealed = vault::seal_payload(&umk, &json!({
            "access_token": "revoked",
            "refresh_token": "r",
            "client_id": "cid.apps.googleusercontent.com",
            "token_endpoint": format!("{}/api/exchangeConnectionToken", broker.base),
            "expires_at": (crate::connections::now_ms() + 3_000_000).to_string(),
        }).to_string());
        let conn = json!({ "name": "google", "provider": "google", "blob": sealed.blob,
                           "wrapped_dek": sealed.wrapped_dek, "updated_at": 7 });
        // Any brokered row, driven by this module — the registry row itself
        // lands separately (it goes live the moment it merges).
        let mut spec = mafold_types::connections::provider_infos().into_iter().find(|p| p.id == "github").unwrap();
        spec.native_api = Some(DRIVER.to_string());
        let rt = Runtime::new(&mafold.base, "s_token", umk);

        let err = run_at(&rt, &api_at(&g.base), "google", &conn, &spec, "gmail.search", &json!({}))
            .await
            .expect_err("revoked");
        assert!(err.contains("Settings ▸ Connections"), "{err}");
        assert_eq!(broker.requests.lock().unwrap().len(), 1, "one renewal");
        let mark = mafold.request(0);
        assert_eq!(mark.path, "/markConnectionRelink");
        assert!(mark.body.contains("\"updated_at\":7"), "{}", mark.body);
    }
}
