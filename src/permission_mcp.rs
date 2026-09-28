//! `mafold permission-mcp` — the stdio MCP server Claude Code consults before it
//! runs a tool call that a permission RULE says a human has to approve.
//!
//! ## Why this exists
//!
//! The harness runs `claude -p … --dangerously-skip-permissions`, which reads as
//! "nothing will ever be blocked". It isn't: an `ask` rule in the user's own
//! settings outranks the permission MODE, outranks an `allow` rule, and outranks
//! a PreToolUse hook's `allow`. Its meaning is "a person must say yes", and in
//! headless `-p` there is no person — so `--permission-prompts` (default `host`)
//! finds nobody to ask and denies. A user whose settings say
//! `ask: ["Bash(rm *)"]` therefore got a hard `Claude requested permissions to
//! use Bash, but you haven't granted it yet.` inside Mafold, where they'd have
//! been asked in the TUI. Verified on claude 2.1.260, all four combinations.
//!
//! There IS a person here — they're just in a chat window. So Mafold becomes the
//! host: `--permission-prompt-tool` routes every would-be prompt to this server,
//! which puts the question in the reply as the `{% mafold/ask %}` card the
//! interactive ask already uses, and blocks on the tap.
//!
//! ## The loop
//!
//! 1. claude calls our one tool with `{tool_name, input, tool_use_id}`.
//! 2. We append the request to `$MAFOLD_PERM_FILE`. The harness watches that file
//!    and turns each line into an `AskUserQuestion` tool-call event on the very
//!    same sink the model's own events go through — so the card paints, and the
//!    daemon flips the turn into "awaiting answer" through the code path that
//!    already existed. Nothing downstream learned a new concept.
//! 3. We block on `$MAFOLD_ASK_FILE` — the same file, the same wait, the same
//!    reply-to-the-draft routing as [`crate::ask_hook`].
//! 4. The tap comes back as `Allow` / `Deny` and we answer claude with
//!    `{"behavior":"allow","updatedInput":…}` or `{"behavior":"deny","message":…}`.
//! 5. No tap in time: deny, and append an expiry line so the watcher stamps the
//!    card [`EXPIRED`] — closed, instead of offering Allow for a refused call.
//!
//! The ask mailbox is per TURN, so step 2 empties it before appending: an
//! answer already sitting there was a late tap on an earlier, expired card.
//!
//! **Fail closed.** No ask file, no answer in ten minutes, or any answer that
//! isn't exactly [`ALLOW`] ⇒ deny. A permission prompt that defaults to yes is
//! not a permission prompt, and the one thing worse than being denied a `rm` is
//! having it run because a timer expired.

use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// The tool's fully-qualified name, as `--permission-prompt-tool` wants it:
/// `mcp__<server>__<tool>`. The harness passes this string and the server below
/// answers to it, so the two can never drift apart.
pub const TOOL_REF: &str = "mcp__mafold__permission";
/// The server key inside `--mcp-config`. Half of [`TOOL_REF`].
pub const SERVER: &str = "mafold";
/// The tool name this server advertises. The other half of [`TOOL_REF`].
pub const TOOL: &str = "permission";

/// The verdict CODES — what a tap sends and what [`decide`] compares against.
/// Codes, not copy: the card draws its own buttons in the reader's language
/// («允许这一次» / «Allow once») and sends one of these regardless, so no
/// translation, rewording or display label can ever change what counts as a
/// yes. `cards/_agent/permission.ts` holds the same three strings.
///
/// Compared case-insensitively, which keeps the cards published before these
/// were codes — they sent the old `Allow` / `Deny` labels — answering correctly.
pub const ALLOW: &str = "allow";
pub const DENY: &str = "deny";
/// The stamp a prompt gets when nobody answered it in time. Not an option — no
/// one can send it — but the same vocabulary: the card reads it off `answered=`
/// and says "nobody approved it, so it didn't run" instead of still offering
/// two buttons for a question the agent has stopped waiting on.
pub const EXPIRED: &str = "expired";

/// The card action a tap on this prompt sends. The server prefix-dispatches it
/// to a RELAY (`events.permissionAnswer` → this daemon), where the default
/// `ask:answer` would have posted a chat message instead. Kept next to the
/// labels because the three of them are one contract: the card offers ALLOW or
/// DENY, sends them under this action, and [`decide`] reads them back.
pub const ACTION: &str = "perm:answer";

/// How long a pending permission question stays open. Matches `ask_hook`: the
/// person being asked is reading a chat, not watching a terminal.
const WAIT: std::time::Duration = std::time::Duration::from_secs(600);

pub fn run() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(line) else { continue };
        // A notification (no `id`) is fire-and-forget — `notifications/initialized`
        // is the only one claude sends. Answering it would be a protocol error.
        let Some(id) = req.get("id").filter(|v| !v.is_null()).cloned() else { continue };
        let method = req["method"].as_str().unwrap_or("");
        let reply = match method {
            "initialize" => ok(id, json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mafold-permission", "version": env!("CARGO_PKG_VERSION") },
            })),
            "tools/list" => ok(id, json!({ "tools": [tool_descriptor()] })),
            "tools/call" => {
                let mut args = req["params"]["arguments"].clone();
                // Which rule asked — found here, where claude's cwd and config
                // dir are ours, and carried to the card with the question.
                let tool = args["tool_name"].as_str().unwrap_or("").to_string();
                if let Some((rule, source)) = matched_ask_rule(&tool, &args["input"], &settings_sources()) {
                    args["rule"] = json!(rule);
                    args["rule_source"] = json!(source);
                }
                let payload = decide(&args, &Mailbox::from_env(), WAIT);
                // The permission verdict travels as a JSON *string* in a text
                // content block — that is the shape claude parses, not a
                // structured result.
                ok(id, json!({ "content": [{ "type": "text", "text": payload.to_string() }] }))
            }
            // We advertise tools and nothing else, so a well-behaved client never
            // gets here. Say so properly rather than returning a bare `{}` that
            // looks like an empty capability.
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            }),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn tool_descriptor() -> Value {
    json!({
        "name": TOOL,
        "description": "Ask the human in the Mafold chat whether a tool call may run.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool_name": { "type": "string" },
                "input": { "type": "object" },
                "tool_use_id": { "type": "string" },
            },
            "required": ["tool_name", "input"],
        },
    })
}

/// Where the question goes out and the answer comes back — the pair of per-turn
/// files the harness hands us. Both or neither: a question nobody can see and an
/// answer nobody can send are the same situation.
struct Mailbox {
    /// Questions out — the harness watches this and draws the card.
    perm: Option<String>,
    /// Answers in — the daemon writes the tap here (shared with `ask_hook`).
    ask: Option<String>,
}

impl Mailbox {
    fn from_env() -> Self {
        // Via `turnenv`, not the raw env: this server is a child of the
        // CONNECTION, which serves many turns, so its own environment names
        // whichever turn spawned the process. Answering into that turn's files
        // means the card never paints for THIS one and the tool call hangs.
        Self { perm: crate::turnenv::perm_file(), ask: crate::turnenv::ask_file() }
    }
}

/// Put the question in the chat, wait for the tap, and turn it into claude's
/// allow/deny payload.
///
/// Takes the mailbox and the deadline rather than reading the environment, so
/// the tests below can exercise every branch — including the timeout — without
/// mutating process-global state that the other tests in this binary are
/// reading at the same time.
fn decide(args: &Value, mailbox: &Mailbox, within: std::time::Duration) -> Value {
    let tool_name = args["tool_name"].as_str().unwrap_or("this tool");
    let input = args["input"].clone();

    let (Some(perm_file), Some(ask_file)) = (&mailbox.perm, &mailbox.ask) else {
        return deny(format!(
            "This run has no chat to ask in, and your settings require a person to \
             approve `{tool_name}` calls like this one. Nothing was run."
        ));
    };

    // Empty the mailbox BEFORE publishing. It belongs to the turn, not to this
    // question, and nobody can have answered this question yet — its card is
    // drawn from the line appended below. So anything already in there answers
    // an EARLIER prompt: a tap on a card that had timed out. Read as ours, that
    // tap approved a command its card never showed.
    let _ = std::fs::remove_file(ask_file);

    // Publish the question. Appended, one JSON object per line: a turn can hit
    // several gated calls, and each is a separate card.
    let tool_use_id = args["tool_use_id"].as_str().unwrap_or("");
    let mut record = json!({
        "tool_name": tool_name,
        "input": input,
        "tool_use_id": tool_use_id,
    });
    for key in ["rule", "rule_source"] {
        if let Some(v) = args[key].as_str().filter(|v| !v.is_empty()) {
            record[key] = json!(v);
        }
    }
    if let Err(e) = append_line(perm_file, &record.to_string()) {
        return deny(format!(
            "Couldn't reach the chat to ask for approval ({e}), and your settings \
             require a person to approve `{tool_name}` calls like this one. Nothing was run."
        ));
    }

    let answer = wait_for_answer(ask_file, within);
    if answer.is_none() {
        // Close the card too: it would otherwise keep offering Allow for a call
        // that was just refused. Best-effort — the verdict below stands either way.
        let _ = append_line(perm_file, &json!({ "tool_use_id": tool_use_id, "expired": true }).to_string());
    }
    match answer {
        Some(ans) if ans.trim().eq_ignore_ascii_case(ALLOW) => json!({
            "behavior": "allow",
            // Echoed unchanged: we are a yes/no gate, not a rewriter. The one
            // place input is edited is `bash_hook`, and it does it there.
            "updatedInput": input,
        }),
        Some(ans) if ans.trim().eq_ignore_ascii_case(DENY) => {
            deny(format!("The user was asked to approve this `{tool_name}` call and declined."))
        }
        // They answered with words instead of tapping. Still a no — but their
        // words are the most useful thing the model could read right now, so
        // they go through verbatim instead of being flattened into "denied".
        Some(ans) if !ans.trim().is_empty() => deny(format!(
            "The user was asked to approve this `{tool_name}` call and answered: {}",
            ans.trim()
        )),
        _ => deny(format!(
            "Nobody answered the approval request for this `{tool_name}` call in time, \
             so it was not run. Ask in plain text before trying again."
        )),
    }
}

fn deny(message: String) -> Value {
    json!({ "behavior": "deny", "message": message })
}

fn append_line(path: &str, line: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

/// Poll `path` until the daemon writes the user's answer, then consume it (remove
/// the file so the next question in the same turn starts clean). Shared shape
/// with `ask_hook::wait_for_answer` because it is the same mailbox: the daemon
/// routes a reply-to-the-draft into exactly one pending question, whichever kind
/// it is.
fn wait_for_answer(path: &str, within: std::time::Duration) -> Option<String> {
    let deadline = std::time::Instant::now() + within;
    // Check BEFORE the first sleep, not after: an answer that is already sitting
    // there is the whole point, and it also makes the deadline honest — a zero
    // wait looks once and gives up, rather than never looking at all.
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            if !s.trim().is_empty() {
                let _ = std::fs::remove_file(path);
                return Some(s);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// One published request → the `AskUserQuestion`-shaped input the transcript
/// renderer already knows how to draw as a `{% mafold/ask %}` card.
///
/// Built here, next to the labels the answer is matched against, and called by
/// the harness watcher — so "what the card offers" and "what an answer means"
/// are one decision in one file.
///
/// FACTS ONLY — the tool, exactly what it wants to run, and the two verdict
/// words. Every sentence the reader sees is the card's (`cards/_agent`, which
/// has it in their language). This used to append an English explanation to the
/// question, which the transcript caps at 200 characters: a long command pushed
/// the explanation off the end, and a Chinese reader got a truncated command,
/// «Allow / run it, just this once» and no idea why they were being asked.
pub fn ask_card_input(record: &Value) -> Value {
    let tool = record["tool_name"].as_str().unwrap_or("");
    let detail = tool_detail(tool, &record["input"]);
    json!({
        // NOT the default `ask:answer`: that one is defined to become a real
        // user message, which is right for a question the model asked and wrong
        // here — it put a stray "Allow" bubble in the room on every guarded
        // command. `perm:answer` is relayed by the server straight to this
        // daemon (`events.permissionAnswer`) and posts nothing.
        "action": ACTION,
        "questions": [{
            "header": tool,
            "multiSelect": false,
            // The flattened one-liner, for a renderer that only knows questions.
            "question": if detail.is_empty() { tool.to_string() } else { detail.clone() },
            // What the card actually shows: carried verbatim (`d|` rows), because
            // the command you approve has to be the command that runs.
            "detail": detail,
            // WHY anyone is being asked: the rule and the file it is written in
            // (empty when no rule could be named — see `matched_ask_rule`).
            "rule": record["rule"].as_str().unwrap_or(""),
            "ruleSource": record["rule_source"].as_str().unwrap_or(""),
            // WHAT it is for, in the agent's own words (a Bash call's
            // `description`) — the line a reader can judge without reading shell.
            "summary": record["input"]["description"].as_str().unwrap_or("").trim(),
            // Codes, not copy — the card labels them.
            "options": [
                { "label": ALLOW, "description": "" },
                { "label": DENY, "description": "" },
            ],
        }],
    })
}

/// One settings file Claude Code reads permission rules from, and the name the
/// card gives it.
#[derive(Clone, Debug)]
struct Source {
    path: std::path::PathBuf,
    shown: String,
}

/// The files the `ask` rule can live in, in the order Claude Code lets them
/// override one another (managed, then local, project, user), so the first one
/// that matches is the one that is in force. Read from THIS process: it is a
/// child of claude, so its working directory and `CLAUDE_CONFIG_DIR` are
/// claude's own.
fn settings_sources() -> Vec<Source> {
    #[cfg(target_os = "macos")]
    let managed = "/Library/Application Support/ClaudeCode/managed-settings.json";
    #[cfg(windows)]
    let managed = r"C:\ProgramData\ClaudeCode\managed-settings.json";
    #[cfg(all(unix, not(target_os = "macos")))]
    let managed = "/etc/claude-code/managed-settings.json";
    let mut out = vec![Source { path: managed.into(), shown: "managed-settings.json".into() }];
    if let Ok(cwd) = std::env::current_dir() {
        for f in [".claude/settings.local.json", ".claude/settings.json"] {
            out.push(Source { path: cwd.join(f), shown: f.into() });
        }
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from);
    let user = match (std::env::var_os("CLAUDE_CONFIG_DIR"), &home) {
        (Some(dir), _) => Some(std::path::PathBuf::from(dir).join("settings.json")),
        (None, Some(h)) => Some(h.join(".claude").join("settings.json")),
        (None, None) => None,
    };
    if let Some(path) = user {
        let shown = match home.as_ref().and_then(|h| path.strip_prefix(h).ok()) {
            Some(rel) => format!("~/{}", rel.to_string_lossy().replace('\\', "/")),
            None => path.to_string_lossy().into_owned(),
        };
        out.push(Source { path, shown });
    }
    out
}

/// The `permissions.ask` rule that stopped this call, and the file it is in.
///
/// Claude Code does not hand the permission tool that fact — the MCP call is
/// just `{tool_name, input, tool_use_id}` — so it is read back out of the same
/// settings files. Only rules this side can evaluate the way Claude Code does
/// are claimed: a bare tool name, an MCP server, and `Bash(…)` command patterns
/// (`prefix:*`, `*` wildcards, exact), each checked against every subcommand of
/// a compound line. Anything else (a gitignore-style path pattern, a domain
/// rule) answers None and the card says a setting asked without naming one —
/// a wrong rule on the card is worse than none.
fn matched_ask_rule(tool: &str, input: &Value, sources: &[Source]) -> Option<(String, String)> {
    for src in sources {
        let Ok(text) = std::fs::read_to_string(&src.path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(asks) = v["permissions"]["ask"].as_array() else { continue };
        for rule in asks.iter().filter_map(Value::as_str).map(str::trim) {
            if rule_matches(rule, tool, input) {
                return Some((rule.to_string(), src.shown.clone()));
            }
        }
    }
    None
}

fn rule_matches(rule: &str, tool: &str, input: &Value) -> bool {
    let (name, content) = match rule.find('(') {
        Some(i) if rule.ends_with(')') => (&rule[..i], Some(rule[i + 1..rule.len() - 1].trim())),
        _ => (rule, None),
    };
    let named = name == tool || (name.starts_with("mcp__") && tool.starts_with(&format!("{name}__")));
    if !named {
        return false;
    }
    match content {
        None | Some("") | Some("*") => true,
        Some(pat) if tool == "Bash" => input["command"]
            .as_str()
            .is_some_and(|cmd| subcommands(cmd).iter().any(|c| command_matches(pat, c))),
        Some(_) => false,
    }
}

/// One `Bash(…)` pattern against one simple command.
fn command_matches(pat: &str, cmd: &str) -> bool {
    if let Some(prefix) = pat.strip_suffix(":*") {
        return cmd == prefix || cmd.starts_with(&format!("{prefix} "));
    }
    if !pat.contains('*') {
        return cmd == pat;
    }
    // `ls *` covers a bare `ls` too.
    if pat.strip_suffix(" *") == Some(cmd) {
        return true;
    }
    let parts: Vec<&str> = pat.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !cmd.starts_with(first) || cmd.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &cmd[first.len()..];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(j) => rest = &rest[j + mid.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// A shell line split into its simple commands at `;` `&&` `||` `|` `&` and
/// newlines — outside quotes only — each with any leading `NAME=value`
/// assignments dropped, since `LANG=C rm x` is an `rm`.
fn subcommands(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut flush = |cur: &mut String| {
        let mut rest = cur.trim();
        while let Some((word, tail)) = rest.split_once(char::is_whitespace) {
            let is_assign = word.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !k.starts_with(|c: char| c.is_ascii_digit())
            });
            if !is_assign {
                break;
            }
            rest = tail.trim_start();
        }
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
        cur.clear();
    };
    for c in line.chars() {
        if escaped {
            escaped = false;
            cur.push(c);
            continue;
        }
        match c {
            '\\' if !single => {
                escaped = true;
                cur.push(c);
            }
            '\'' if !double => {
                single = !single;
                cur.push(c);
            }
            '"' if !single => {
                double = !double;
                cur.push(c);
            }
            ';' | '|' | '&' | '\n' if !single && !double => flush(&mut cur),
            _ => cur.push(c),
        }
    }
    flush(&mut cur);
    out
}

/// A line in the permission file that CLOSES a prompt rather than opening one:
/// `decide` writes it when nobody answered in time, and the harness watcher turns
/// it into the card's [`EXPIRED`] stamp.
pub fn is_expiry(record: &Value) -> bool {
    record["expired"].as_bool() == Some(true)
}

/// The one line that says WHAT is being approved. Deliberately the same fields
/// the transcript's tool cards summarise on, so the question in the card reads
/// like the card that would have appeared had it run.
fn tool_detail(name: &str, input: &Value) -> String {
    let raw = match name.to_lowercase().as_str() {
        "bash" => input["command"].as_str(),
        "edit" | "write" | "multiedit" | "read" | "notebookedit" | "apply_patch" => {
            input["file_path"].as_str()
        }
        "glob" | "grep" => input["pattern"].as_str(),
        "webfetch" => input["url"].as_str(),
        "task" => input["description"].as_str(),
        _ => input["command"]
            .as_str()
            .or_else(|| input["file_path"].as_str())
            .or_else(|| input["path"].as_str())
            .or_else(|| input["url"].as_str())
            .or_else(|| input["query"].as_str()),
    };
    raw.unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag the harness passes and the tool this server answers to are one
    /// string split two ways — a rename that touches only one of them would make
    /// every gated call time out instead of failing loudly.
    #[test]
    fn tool_ref_matches_the_server_and_tool_names() {
        assert_eq!(TOOL_REF, format!("mcp__{SERVER}__{TOOL}"));
    }

    /// Facts, not copy: the card writes every sentence in the reader's language,
    /// so nothing here may carry an English one. The command rides whole in
    /// `detail`, pipes and newlines included.
    #[test]
    fn a_bash_request_sends_the_command_and_no_copy() {
        let cmd = "ls out | grep png\nrm -rf build";
        let rec = json!({ "tool_name": "Bash", "input": { "command": cmd } });
        let card = ask_card_input(&rec);
        let q = &card["questions"][0];
        assert_eq!(q["header"], "Bash");
        assert_eq!(q["question"], cmd);
        assert_eq!(q["detail"], cmd);
        assert_eq!(q["options"][0]["label"], ALLOW);
        assert_eq!(q["options"][1]["label"], DENY);
        for o in q["options"].as_array().unwrap() {
            assert_eq!(o["description"], "", "option copy is the card's to write: {o}");
        }
        assert!(!card.to_string().contains("settings say"), "{card}");
    }

    /// A tool we have no field mapping for still names itself rather than
    /// producing a card that asks about nothing.
    #[test]
    fn an_unmapped_tool_still_names_itself() {
        let rec = json!({ "tool_name": "SomeFutureTool", "input": { "wat": 1 } });
        let q = &ask_card_input(&rec)["questions"][0];
        assert_eq!(q["question"], "SomeFutureTool");
        assert_eq!(q["detail"], "");
    }

    /// Fail closed: with no chat to ask in, the answer is no — never a silent yes.
    #[test]
    fn no_mailbox_means_denied() {
        let nowhere = Mailbox { perm: None, ask: None };
        let v = decide(&rm_x(), &nowhere, INSTANT);
        assert_eq!(v["behavior"], "deny");
        assert!(v["message"].as_str().unwrap().contains("Nothing was run"), "{v}");
    }

    /// `tools/call` answers with the verdict as a JSON string inside a text
    /// block — the shape claude parses. A structured result is silently ignored.
    #[test]
    fn a_verdict_is_a_json_string_in_a_text_block() {
        let payload = decide(&rm_x(), &Mailbox { perm: None, ask: None }, INSTANT);
        let wire = ok(json!(2), json!({
            "content": [{ "type": "text", "text": payload.to_string() }]
        }));
        let text = wire["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["behavior"], "deny");
    }

    /// An answered mailbox: `Allow` runs it and hands the input back untouched.
    #[test]
    fn allow_echoes_the_input_back() {
        let m = mailbox("allow");
        let v = answered(&rm_x(), &m, 1, ALLOW);
        assert_eq!(v["behavior"], "allow");
        assert_eq!(v["updatedInput"]["command"], "rm x");
        // The question was published for the harness to draw…
        let published = std::fs::read_to_string(m.perm.as_ref().unwrap()).unwrap();
        assert!(published.contains("\"command\":\"rm x\""), "{published}");
        // …and the answer was consumed, so the next question starts clean.
        assert!(!std::path::Path::new(m.ask.as_ref().unwrap()).exists());
    }

    #[test]
    fn deny_says_the_user_declined() {
        let v = answered(&rm_x(), &mailbox("deny"), 1, DENY);
        assert_eq!(v["behavior"], "deny");
        assert!(v["message"].as_str().unwrap().contains("declined"), "{v}");
    }

    /// Words instead of a tap are still a no — and reach the model verbatim, so
    /// "no, use trash instead" can redirect the agent in one move.
    #[test]
    fn free_text_denies_but_carries_the_words() {
        let v = answered(&rm_x(), &mailbox("words"), 1, "no — use trash instead");
        assert_eq!(v["behavior"], "deny");
        assert!(v["message"].as_str().unwrap().contains("use trash instead"), "{v}");
    }

    /// Nobody was around. The `rm` does NOT run — a permission prompt that
    /// defaults to yes when it times out is not a permission prompt.
    #[test]
    fn silence_denies() {
        let v = decide(&rm_x(), &mailbox("silence"), INSTANT);
        assert_eq!(v["behavior"], "deny");
        assert!(v["message"].as_str().unwrap().contains("in time"), "{v}");
    }

    /// …and the card is told so. Without this line it kept offering Allow for a
    /// call the agent had already been refused and moved on from.
    #[test]
    fn silence_closes_the_card() {
        let m = mailbox("expire");
        let with_id = json!({ "tool_name": "Bash", "input": { "command": "rm x" }, "tool_use_id": "toolu_1" });
        decide(&with_id, &m, INSTANT);
        let published = std::fs::read_to_string(m.perm.as_ref().unwrap()).unwrap();
        let lines: Vec<Value> = published.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "the question, then its expiry: {published}");
        assert!(!is_expiry(&lines[0]));
        assert!(is_expiry(&lines[1]));
        assert_eq!(lines[1]["tool_use_id"], "toolu_1", "the expiry names the prompt it closes");
    }

    /// An answered prompt is NOT closed as expired — its stamp is the answer.
    #[test]
    fn an_answer_does_not_publish_an_expiry() {
        let m = mailbox("answered-no-expiry");
        answered(&rm_x(), &m, 1, DENY);
        let published = std::fs::read_to_string(m.perm.as_ref().unwrap()).unwrap();
        assert_eq!(published.lines().count(), 1, "{published}");
    }

    /// Two gated calls in one turn each publish their own question, so the
    /// harness draws two cards rather than redrawing the first.
    #[test]
    fn a_second_question_appends_rather_than_overwrites() {
        let m = mailbox("twice");
        answered(&rm_x(), &m, 1, ALLOW);
        answered(&json!({ "tool_name": "Bash", "input": { "command": "rm y" } }), &m, 2, ALLOW);
        let published = std::fs::read_to_string(m.perm.as_ref().unwrap()).unwrap();
        assert_eq!(published.lines().count(), 2, "{published}");
        assert!(published.lines().nth(1).unwrap().contains("rm y"), "{published}");
    }

    /// The mailbox is per TURN, not per question. A card that timed out stays on
    /// screen, and a tap on it afterwards still lands in that one file — where
    /// the NEXT gated call used to find it and read it as its own answer. The
    /// user approved `rm x` late and got `rm -rf y` run, a command no card had
    /// shown them yet.
    #[test]
    fn a_late_tap_on_an_expired_prompt_cannot_approve_the_next_one() {
        let m = mailbox("late");
        assert_eq!(decide(&rm_x(), &m, INSTANT)["behavior"], "deny", "nobody answered the first one");
        // …and then somebody taps Allow on that stale card.
        std::fs::write(m.ask.as_ref().unwrap(), ALLOW).unwrap();
        let next = json!({ "tool_name": "Bash", "input": { "command": "rm -rf y" } });
        let v = decide(&next, &m, INSTANT);
        assert_eq!(v["behavior"], "deny", "an Allow meant for `rm x` ran `rm -rf y`: {v}");
    }

    /// The verdict is a fixed CODE, never the words on a button. The card writes
    /// «允许这一次» / «Allow once» in the reader's language and sends `allow`;
    /// reader-language words arriving here are a reply, and a reply is a no.
    #[test]
    fn the_verdict_is_a_code_not_a_label() {
        assert_eq!((ALLOW, DENY, EXPIRED), ("allow", "deny", "expired"));
        let card = ask_card_input(&json!({ "tool_name": "Bash", "input": { "command": "rm x" } }));
        let labels: Vec<&str> = card["questions"][0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, [ALLOW, DENY], "the body offers the codes, not copy");
        assert_eq!(answered(&rm_x(), &mailbox("label"), 1, "允许这一次")["behavior"], "deny");
        // A card published before this one sends the old `Allow` — still a yes.
        assert_eq!(answered(&rm_x(), &mailbox("legacy"), 1, "Allow")["behavior"], "allow");
    }

    /// WHICH rule stopped the call, and the file it lives in — the first thing
    /// the card says. The command is the one from the report: a compound line,
    /// and it is its `rm` that `Bash(rm *)` catches.
    #[test]
    fn the_rule_that_stopped_it_is_named() {
        let s = settings("rules", r#"{"permissions":{"allow":["Bash(*)"],"ask":["Bash(git push:*)","Bash(rm *)"]}}"#);
        let cmd = r#"H=dist/a.html; O=shots; rm -f $O/*.png; PRE="document.querySelectorAll('img')""#;
        assert_eq!(
            matched_ask_rule("Bash", &json!({ "command": cmd }), &s),
            Some(("Bash(rm *)".to_string(), "~/.claude/settings.json".to_string()))
        );
        let push = matched_ask_rule("Bash", &json!({ "command": "cd x && git push origin main" }), &s);
        assert_eq!(push.unwrap().0, "Bash(git push:*)");
        assert_eq!(matched_ask_rule("Bash", &json!({ "command": "ls shots | grep png" }), &s), None);
        // Inside quotes it is an argument, not a command.
        assert_eq!(matched_ask_rule("Bash", &json!({ "command": "echo 'x; rm -rf y'" }), &s), None);
        // An env prefix doesn't hide the command it prefixes.
        assert!(matched_ask_rule("Bash", &json!({ "command": "LANG=C rm y" }), &s).is_some());
    }

    /// Only a rule this side can evaluate the way Claude Code does gets NAMED —
    /// naming the wrong one is worse than naming none, and then the card says a
    /// setting asked without saying which. Path patterns are gitignore-style
    /// there, so they aren't claimed; a bare tool rule is exact.
    #[test]
    fn only_a_rule_we_can_match_is_named() {
        let s = settings("rules-2", r#"{"permissions":{"ask":["Edit(src/**)","WebFetch","mcp__notion"]}}"#);
        assert_eq!(matched_ask_rule("Edit", &json!({ "file_path": "src/a.ts" }), &s), None);
        assert_eq!(matched_ask_rule("WebFetch", &json!({ "url": "https://x" }), &s).unwrap().0, "WebFetch");
        assert_eq!(matched_ask_rule("mcp__notion__search", &json!({}), &s).unwrap().0, "mcp__notion");
        assert_eq!(matched_ask_rule("mcp__notional__x", &json!({}), &s), None);
    }

    /// What the card leads with rides the question: the rule, its file, and the
    /// agent's own one-line account of what the call is for.
    #[test]
    fn the_card_gets_the_rule_and_the_purpose() {
        let rec = json!({
            "tool_name": "Bash",
            "input": { "command": "rm x", "description": "清掉旧截图" },
            "rule": "Bash(rm *)", "rule_source": "~/.claude/settings.json",
        });
        let q = &ask_card_input(&rec)["questions"][0];
        assert_eq!(q["rule"], "Bash(rm *)");
        assert_eq!(q["ruleSource"], "~/.claude/settings.json");
        assert_eq!(q["summary"], "清掉旧截图");
        // …and they reach the harness: `decide` publishes what `run` found.
        let m = mailbox("rule-published");
        let mut args = rm_x();
        args["rule"] = json!("Bash(rm *)");
        args["rule_source"] = json!("~/.claude/settings.json");
        decide(&args, &m, INSTANT);
        let first = std::fs::read_to_string(m.perm.as_ref().unwrap()).unwrap();
        let rec: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
        assert_eq!((rec["rule"].as_str(), rec["rule_source"].as_str()), (Some("Bash(rm *)"), Some("~/.claude/settings.json")));
    }

    /// One user-level settings file, shown the way the card names it.
    fn settings(tag: &str, body: &str) -> Vec<Source> {
        let dir = std::env::temp_dir().join(format!("mafold-permrules-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, body).unwrap();
        vec![Source { path, shown: "~/.claude/settings.json".into() }]
    }

    /// A zero deadline still checks the mailbox once, then falls straight
    /// through to the timeout.
    const INSTANT: std::time::Duration = std::time::Duration::from_millis(0);

    fn rm_x() -> Value {
        json!({ "tool_name": "Bash", "input": { "command": "rm x" } })
    }

    /// `decide` with the user answering the way they really do: AFTER the
    /// question is published (its card can't be drawn before), from elsewhere.
    /// `nth` is how many lines the permission file holds once this question is
    /// out. Pre-seeding the mailbox instead is exactly the stale-tap case — it
    /// is discarded now, by design.
    fn answered(args: &Value, m: &Mailbox, nth: usize, answer: &str) -> Value {
        let (perm, ask, answer) = (m.perm.clone().unwrap(), m.ask.clone().unwrap(), answer.to_string());
        let tapper = std::thread::spawn(move || {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < until {
                let out = std::fs::read_to_string(&perm).map(|s| s.lines().count()).unwrap_or(0);
                if out >= nth {
                    std::fs::write(&ask, &answer).unwrap();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let v = decide(args, m, std::time::Duration::from_secs(10));
        tapper.join().unwrap();
        v
    }

    /// A private pair of files per test — no shared process state, so these all
    /// run in parallel like every other test in this binary.
    fn mailbox(tag: &str) -> Mailbox {
        let dir = std::env::temp_dir().join(format!(
            "mafold-permtest-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Mailbox {
            perm: Some(dir.join("perm.jsonl").to_string_lossy().into_owned()),
            ask: Some(dir.join("ask.txt").to_string_lossy().into_owned()),
        }
    }
}
