//! `mafold flags` — read and change the server's feature flags with this
//! identity's own token (a bot's `mb_`): how an owner's agents open a flag for
//! their owner instead of asking them to (the owner's call 2026-10-23).
//!
//! The server decides who may: `flags.read` / `flags.write` in its capability
//! table, held by the bot itself or by its owner's agents' namespace
//! (`opsdu:*`). Every change is one audit row naming the bot, its owner and
//! the door it came in by; the admin portal's Flags page shows it and undoes
//! it. Nothing here is a privileged path — a caller without the capability
//! gets the server's refusal, logged there.
//!
//! (`flags.rs` is something else: the flags as the daemon itself sees them.)

use anyhow::{bail, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use mafold_core::mafold_types::units::EN;

#[derive(Subcommand)]
pub enum FlagsCmd {
    /// Every flag: what it is set to and who changed it last (the default).
    List,
    /// One flag: its record, what it is for, and its latest changes.
    Show {
        key: String,
        /// How many changes to show.
        #[arg(long, default_value_t = 10)]
        limit: i64,
    },
    /// Put accounts on a flag's allowlist: `mafold flags add iap opsdu`. The
    /// rest of the list and the note stay as they are.
    Add {
        key: String,
        #[arg(required = true)]
        users: Vec<String>,
        /// Write a key the server's registry doesn't know.
        #[arg(long)]
        unregistered: bool,
    },
    /// Take accounts off a flag's allowlist.
    Remove {
        key: String,
        #[arg(required = true)]
        users: Vec<String>,
    },
    /// Replace a flag's rollout: `on`, `off`, `25%`, or an account list
    /// `opsdu,linsky`. The note stays unless `--note` gives one (`--note ""`
    /// clears it).
    Set {
        key: String,
        rollout: String,
        #[arg(long)]
        note: Option<String>,
        /// Write a key the server's registry doesn't know.
        #[arg(long)]
        unregistered: bool,
    },
    /// Delete a flag's record: everyone falls back to what the key means
    /// without one (`mafold flags show <key>` says what that is).
    Delete { key: String },
    /// What a flag is for one account right now, and why.
    Check { key: String, user: String },
}

pub async fn run(cmd: FlagsCmd, client: &Client) -> Result<()> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match cmd {
        FlagsCmd::List => {
            let page = client.call("adminFlags", json!({})).await?;
            let items = page["items"].as_array().cloned().unwrap_or_default();
            let width = items.iter().filter_map(|i| i["key"].as_str()).map(str::len).max().unwrap_or(0);
            for i in &items {
                let state = match i["record"].as_object() {
                    Some(_) => rollout_summary(&i["record"]["rollout"]),
                    None => without_record(&i["meta"]),
                };
                let last = if i["last"].is_object() { format!("  #{} {}", i["last"]["id"], who(&i["last"], now)) } else { String::new() };
                println!("{:width$}  {state}{last}", i["key"].as_str().unwrap_or("?"));
            }
            println!("\n{} flags · `mafold flags show <key>` for one, `mafold flags add <key> <user>` to open one", items.len());
        }
        FlagsCmd::Show { key, limit } => {
            let item = find(client, &key).await?;
            let Some(item) = item else { bail!("no flag `{key}` — `mafold flags` lists them") };
            let m = &item["meta"];
            match m["label"].as_str() {
                Some(label) => println!("{key} — {label}\n  {}", m["description"].as_str().unwrap_or("")),
                None => println!("{key} — not in the registry"),
            }
            match item["record"].as_object() {
                Some(_) => {
                    println!("  now:     {}", rollout_summary(&item["record"]["rollout"]));
                    if let Some(note) = item["record"]["note"].as_str() {
                        println!("  note:    {note}");
                    }
                    // What deleting the record would fall back to.
                    if let Some(fallback) = without_record(m).strip_prefix("no record · ") {
                        println!("  without the record: {fallback}");
                    }
                }
                None => println!("  now:     {}", without_record(m)),
            }
            let h = client.call("adminFlagHistory", json!({ "key": key, "limit": limit })).await?;
            let rows = h["items"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("  no recorded changes");
            } else {
                println!("  changes, newest first:");
                for r in &rows {
                    println!("    {}", history_line(r, now));
                }
            }
        }
        FlagsCmd::Add { key, users, unregistered } => {
            known(client, &key, unregistered).await?;
            let r = client.call("setFlag", json!({ "key": key, "addAccounts": users })).await?;
            println!("{}", written(&key, &r));
        }
        FlagsCmd::Remove { key, users } => {
            let r = client.call("setFlag", json!({ "key": key, "removeAccounts": users })).await?;
            println!("{}", written(&key, &r));
        }
        FlagsCmd::Set { key, rollout, note, unregistered } => {
            let rollout = parse_rollout(&rollout)?;
            let item = known(client, &key, unregistered).await?;
            let record = item.as_ref().map(|i| &i["record"]).filter(|r| r.is_object());
            // Keep the note unless told otherwise, and only over the record
            // just read: a change made in between is refused, not overwritten.
            let note = note.or_else(|| record.and_then(|r| r["note"].as_str()).map(str::to_string));
            let seen = record.and_then(|r| r["updated_at"].as_str()).unwrap_or("");
            let r = client.call("setFlag", json!({ "key": key, "rollout": rollout, "note": note, "expect": seen })).await?;
            println!("{}", written(&key, &r));
        }
        FlagsCmd::Delete { key } => {
            // Only a record that is there, as it was just read: a typo writes
            // nothing, and a change made in between is refused, not lost.
            let item = find(client, &key).await?;
            let Some(seen) = item.as_ref().and_then(|i| i["record"]["updated_at"].as_str()) else {
                bail!("`{key}` has no record to delete — `mafold flags` lists them");
            };
            let r = client.call("setFlag", json!({ "key": key, "expect": seen })).await?;
            println!("{}", written(&key, &r));
        }
        FlagsCmd::Check { key, user } => {
            let c = client.call("adminFlagCheck", json!({ "key": key, "user": user })).await?;
            let on = |v: &Value| match v.as_bool() {
                Some(true) => "on",
                Some(false) => "off",
                None => "not read",
            };
            println!(
                "{key} for @{}: {} — {}  (clients: {} · api: {}){}",
                c["user"].as_str().unwrap_or(&user),
                on(&c["record"]).replace("not read", "no record"),
                c["why"].as_str().unwrap_or(""),
                on(&c["clients"]),
                on(&c["server"]),
                if c["exists"] == json!(false) { "  — no such account" } else { "" },
            );
        }
    }
    Ok(())
}

/// The Flags page's entry for `key`, `None` when the server has neither a
/// registration nor a record for it.
async fn find(client: &Client, key: &str) -> Result<Option<Value>> {
    let page = client.call("adminFlags", json!({})).await?;
    Ok(page["items"].as_array().and_then(|items| items.iter().find(|i| i["key"] == key).cloned()))
}

/// [`find`], refusing a key the server's registry doesn't know unless asked:
/// a typo would otherwise write a record nothing reads.
async fn known(client: &Client, key: &str, unregistered: bool) -> Result<Option<Value>> {
    let item = find(client, key).await?;
    if !unregistered && !item.as_ref().is_some_and(|i| i["meta"].is_object()) {
        bail!("`{key}` isn't a flag the server's registry knows — `mafold flags` lists them (--unregistered writes it anyway)");
    }
    Ok(item)
}

/// `on` / `off` / `25%` / `opsdu,@linsky` → the wire `FlagRollout`.
fn parse_rollout(s: &str) -> Result<Value> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "on" => return Ok(json!({ "kind": "on" })),
        "off" => return Ok(json!({ "kind": "off" })),
        _ => {}
    }
    if let Some(p) = s.strip_suffix('%') {
        return match p.trim().parse::<u8>() {
            Ok(p) if p <= 100 => Ok(json!({ "kind": "percent", "p": p })),
            _ => bail!("`{s}`: a percent is 0%–100%"),
        };
    }
    let list: Vec<String> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|a| a.trim().trim_start_matches('@').to_lowercase())
        .filter(|a| !a.is_empty())
        .collect();
    if list.is_empty() || list.iter().any(|a| !a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))) {
        bail!("`{s}`: say on, off, a percent like 25%, or accounts like opsdu,linsky");
    }
    Ok(json!({ "kind": "accounts", "list": list }))
}

/// A rollout in a few words: `on`, `off`, `25%`, `only @a @b`.
fn rollout_summary(r: &Value) -> String {
    match r["kind"].as_str() {
        Some("on") => "on".into(),
        Some("off") => "off".into(),
        Some("percent") => format!("{}%", r["p"]),
        Some("accounts") => {
            let list: Vec<String> = r["list"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|a| format!("@{a}")).collect();
            match list.len() {
                0 => "only nobody".into(),
                n if n <= 8 => format!("only {}", list.join(" ")),
                n => format!("only {} +{} more", list[..8].join(" "), n - 8),
            }
        }
        _ => r.to_string(),
    }
}

/// What a registered key means with no record: the api's direction (if it
/// reads the key) and the clients' default (if any client does).
fn without_record(meta: &Value) -> String {
    let mut parts = vec!["no record".to_string()];
    match meta["server"].as_str() {
        Some("open_without_record") => parts.push("api: open".into()),
        Some("closed_without_record") => parts.push("api: shut".into()),
        _ => {}
    }
    if meta["clients"].as_array().is_some_and(|c| !c.is_empty()) {
        parts.push(format!("clients: {}", if meta["default"] == json!(true) { "on" } else { "off" }));
    }
    parts.join(" · ")
}

/// Who made a change and how long ago: `@opsdu:claude-code · owner @opsdu ·
/// own token · 2m ago`.
fn who(row: &Value, now: i64) -> String {
    let mut parts = vec![format!("@{}", row["actor"].as_str().unwrap_or("?"))];
    if let Some(owner) = row["owner"].as_str() {
        parts.push(format!("owner @{owner}"));
    }
    parts.push(match row["via"].as_str() {
        Some("token") => "own token".into(),
        Some(v) => v.into(),
        None => "?".into(),
    });
    if let Some(at) = row["at"].as_i64() {
        parts.push(EN.ago((now - at).max(0) as f64));
    }
    parts.join(" · ")
}

/// A snapshot an audit row keeps (`{"record": …}`), in a few words.
fn snapshot_summary(v: &Value) -> String {
    match v.get("record") {
        Some(Value::Null) => "no record".into(),
        Some(r) => rollout_summary(&r["rollout"]),
        None => "?".into(),
    }
}

/// One audited change: `#41  setFlag  no record → only @opsdu  (@bot · …)`.
fn history_line(r: &Value, now: i64) -> String {
    let what = match (r["action"].as_str(), r["status"].as_str()) {
        (_, Some(s)) if s != "ok" => format!("{s}{}", r["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default()),
        (Some("adminFlagRevert"), _) => format!("undid #{}: {} → {}", r["after"]["reverts"], snapshot_summary(&r["before"]), snapshot_summary(&r["after"])),
        (action, _) => format!("{}: {} → {}", action.unwrap_or("?"), snapshot_summary(&r["before"]), snapshot_summary(&r["after"])),
    };
    format!("#{}  {what}  ({})", r["id"], who(r, now))
}

/// What a write answers with, as one line.
fn written(key: &str, r: &Value) -> String {
    let now = if r["record"].is_null() { "no record".to_string() } else { rollout_summary(&r["record"]["rollout"]) };
    format!("✓ {key}: {now}  (audit #{} — the admin portal's Flags page shows it and can undo it)", r["audit_id"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollouts_parse_from_a_word_a_percent_or_a_list() {
        assert_eq!(parse_rollout(" ON ").unwrap(), json!({ "kind": "on" }));
        assert_eq!(parse_rollout("off").unwrap(), json!({ "kind": "off" }));
        assert_eq!(parse_rollout("25%").unwrap(), json!({ "kind": "percent", "p": 25 }));
        assert_eq!(parse_rollout("@OpsDu, linsky  opsdu:claude-code").unwrap(), json!({ "kind": "accounts", "list": ["opsdu", "linsky", "opsdu:claude-code"] }));
        assert!(parse_rollout("101%").is_err());
        assert!(parse_rollout("-1%").is_err());
        assert!(parse_rollout(" , ").is_err());
        assert!(parse_rollout("not/a name").is_err());
    }

    #[test]
    fn a_rollout_reads_in_a_few_words() {
        assert_eq!(rollout_summary(&json!({ "kind": "on" })), "on");
        assert_eq!(rollout_summary(&json!({ "kind": "percent", "p": 5 })), "5%");
        assert_eq!(rollout_summary(&json!({ "kind": "accounts", "list": ["opsdu", "linsky"] })), "only @opsdu @linsky");
        let ten: Vec<String> = (0..10).map(|i| format!("u{i}")).collect();
        assert!(rollout_summary(&json!({ "kind": "accounts", "list": ten })).ends_with("@u7 +2 more"));
        assert_eq!(without_record(&json!({ "server": "closed_without_record", "clients": [], "default": false })), "no record · api: shut");
        assert_eq!(without_record(&json!({ "server": "not_read", "clients": ["web"], "default": true })), "no record · clients: on");
    }

    /// A bot's change names the bot, its owner and its own token — what the
    /// owner reads to know an agent did it.
    #[test]
    fn a_change_says_who_made_it_and_what_it_did() {
        let row = json!({
            "id": 41, "at": 1_000, "actor": "opsdu:claude-code", "owner": "opsdu", "via": "token",
            "action": "setFlag", "status": "ok",
            "before": { "record": null },
            "after": { "record": { "key": "iap", "rollout": { "kind": "accounts", "list": ["opsdu"] }, "note": null, "updated_at": "" } },
        });
        assert_eq!(history_line(&row, 1_120), "#41  setFlag: no record → only @opsdu  (@opsdu:claude-code · owner @opsdu · own token · 2m ago)");
        let undo = json!({ "id": 42, "actor": "opsdu", "via": "portal", "action": "adminFlagRevert", "status": "ok",
            "before": row["after"], "after": { "record": null, "reverts": 41 } });
        assert!(history_line(&undo, 0).starts_with("#42  undid #41: only @opsdu → no record  (@opsdu · portal"));
        let denied = json!({ "id": 43, "actor": "linsky:muse", "owner": "linsky", "via": "token", "action": "setFlag", "status": "denied", "error": "needs the `flags.write` capability" });
        assert!(history_line(&denied, 0).starts_with("#43  denied: needs the `flags.write` capability"));
        assert_eq!(
            written("iap", &json!({ "audit_id": 41, "record": row["after"]["record"] })),
            "✓ iap: only @opsdu  (audit #41 — the admin portal's Flags page shows it and can undo it)"
        );
    }
}
