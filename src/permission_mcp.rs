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

/// The card's two option labels. They are also the ANSWER vocabulary — the tap
/// posts the label back as the user's next message — so the card builder and the
/// answer reader have to share them, which is why they live here rather than in
/// the harness that draws the card.
pub const ALLOW: &str = "Allow";
pub const DENY: &str = "Deny";

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
                let args = req["params"]["arguments"].clone();
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
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        Self { perm: var("MAFOLD_PERM_FILE"), ask: var("MAFOLD_ASK_FILE") }
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

    // Publish the question. Appended, one JSON object per line: a turn can hit
    // several gated calls, and each is a separate card.
    let record = json!({
        "tool_name": tool_name,
        "input": input,
        "tool_use_id": args["tool_use_id"].as_str().unwrap_or(""),
    });
    if let Err(e) = append_line(perm_file, &record.to_string()) {
        return deny(format!(
            "Couldn't reach the chat to ask for approval ({e}), and your settings \
             require a person to approve `{tool_name}` calls like this one. Nothing was run."
        ));
    }

    match wait_for_answer(ask_file, within) {
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
pub fn ask_card_input(record: &Value) -> Value {
    let tool = record["tool_name"].as_str().unwrap_or("A tool");
    let detail = tool_detail(tool, &record["input"]);
    let question = if detail.is_empty() {
        format!("{tool} — your Claude Code settings say this one needs your OK.")
    } else {
        format!("{detail} — your Claude Code settings say this one needs your OK.")
    };
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
            "question": question,
            "options": [
                { "label": ALLOW, "description": "run it, just this once" },
                { "label": DENY, "description": "skip it — the agent is told you said no" },
            ],
        }],
    })
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

    #[test]
    fn a_bash_request_asks_about_the_command() {
        let rec = json!({ "tool_name": "Bash", "input": { "command": "rm -rf build" } });
        let card = ask_card_input(&rec);
        let q = &card["questions"][0];
        assert_eq!(q["header"], "Bash");
        assert!(q["question"].as_str().unwrap().starts_with("rm -rf build —"), "{q}");
        assert_eq!(q["options"][0]["label"], ALLOW);
        assert_eq!(q["options"][1]["label"], DENY);
    }

    /// A tool we have no field mapping for still produces a usable question
    /// rather than a card that asks about nothing.
    #[test]
    fn an_unmapped_tool_still_names_itself() {
        let rec = json!({ "tool_name": "SomeFutureTool", "input": { "wat": 1 } });
        let q = ask_card_input(&rec)["questions"][0]["question"].as_str().unwrap().to_string();
        assert!(q.starts_with("SomeFutureTool —"), "{q}");
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
        let m = mailbox("allow", Some(ALLOW));
        let v = decide(&rm_x(), &m, INSTANT);
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
        let v = decide(&rm_x(), &mailbox("deny", Some(DENY)), INSTANT);
        assert_eq!(v["behavior"], "deny");
        assert!(v["message"].as_str().unwrap().contains("declined"), "{v}");
    }

    /// Words instead of a tap are still a no — and reach the model verbatim, so
    /// "no, use trash instead" can redirect the agent in one move.
    #[test]
    fn free_text_denies_but_carries_the_words() {
        let v = decide(&rm_x(), &mailbox("words", Some("no — use trash instead")), INSTANT);
        assert_eq!(v["behavior"], "deny");
        assert!(v["message"].as_str().unwrap().contains("use trash instead"), "{v}");
    }

    /// Nobody was around. The `rm` does NOT run — a permission prompt that
    /// defaults to yes when it times out is not a permission prompt.
    #[test]
    fn silence_denies() {
        let v = decide(&rm_x(), &mailbox("silence", None), INSTANT);
        assert_eq!(v["behavior"], "deny");
        assert!(v["message"].as_str().unwrap().contains("in time"), "{v}");
    }

    /// Two gated calls in one turn each publish their own question, so the
    /// harness draws two cards rather than redrawing the first.
    #[test]
    fn a_second_question_appends_rather_than_overwrites() {
        let m = mailbox("twice", Some(ALLOW));
        decide(&rm_x(), &m, INSTANT);
        std::fs::write(m.ask.as_ref().unwrap(), ALLOW).unwrap();
        decide(
            &json!({ "tool_name": "Bash", "input": { "command": "rm y" } }),
            &m,
            INSTANT,
        );
        let published = std::fs::read_to_string(m.perm.as_ref().unwrap()).unwrap();
        assert_eq!(published.lines().count(), 2, "{published}");
        assert!(published.lines().nth(1).unwrap().contains("rm y"), "{published}");
    }

    /// A zero deadline still checks the mailbox once — an answer already sitting
    /// there is read, and an empty one falls straight through to the timeout.
    const INSTANT: std::time::Duration = std::time::Duration::from_millis(0);

    fn rm_x() -> Value {
        json!({ "tool_name": "Bash", "input": { "command": "rm x" } })
    }

    /// A private pair of files per test — no shared process state, so these all
    /// run in parallel like every other test in this binary.
    fn mailbox(tag: &str, answer: Option<&str>) -> Mailbox {
        let dir = std::env::temp_dir().join(format!(
            "mafold-permtest-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ask = dir.join("ask.txt");
        if let Some(a) = answer {
            std::fs::write(&ask, a).unwrap();
        }
        Mailbox {
            perm: Some(dir.join("perm.jsonl").to_string_lossy().into_owned()),
            ask: Some(ask.to_string_lossy().into_owned()),
        }
    }
}
