//! `mafold d1` — your D1 databases from a terminal, shaped like `wrangler d1`.
//!
//! A database belongs to a person; a bot works in its owner's account (the way
//! a Cloudflare API token acts for its account), so the same commands serve a
//! person signed in with `--account` and an agent running on its bot token.
//! `<database>` is a name or an id. See `.docs/d1-v1.md`.
//!
//! `execute` is the console path — it rides Cloudflare's REST API and is
//! rate-limited. An app's data goes through its own Worker's binding instead.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use mafold_core::mafold_types::d1::validate_name;
use serde_json::{json, Value};

use crate::client::Client;

#[derive(Subcommand)]
pub enum D1Cmd {
    /// List your databases, the recycle bin, and how much of your allowance is used.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Create a database (the name is yours to pick: a-z 0-9 - _).
    Create {
        name: String,
        /// Where its primary lives: wnam, enam, weur, eeur, apac (default), oc.
        #[arg(long)]
        location: Option<String>,
    },
    /// One database: size and restore points.
    Info {
        database: String,
        #[arg(long)]
        json: bool,
    },
    /// Rename a database. Its id — and everything bound to it — stays.
    Rename { database: String, name: String },
    /// Run SQL against a database (console path; rate-limited).
    Execute {
        database: String,
        /// The SQL to run.
        #[arg(long, short)]
        command: Option<String>,
        /// Read the SQL from a file instead.
        #[arg(long, conflicts_with = "command")]
        file: Option<String>,
        /// A bound parameter for `?` (repeat for each, in order).
        #[arg(long = "param")]
        params: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remember "now" as a restore point.
    Bookmark {
        database: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Time Travel a database back to a bookmark or to a moment (last 30 days).
    Restore {
        database: String,
        #[arg(long, conflicts_with = "at")]
        bookmark: Option<String>,
        /// ISO 8601, e.g. 2026-10-04T08:00:00Z
        #[arg(long)]
        at: Option<String>,
        /// Confirm — the database's current contents are replaced.
        #[arg(long, short)]
        yes: bool,
    },
    /// Move a database to the recycle bin (restorable for 30 days).
    /// With --permanent, delete one that is already in the bin, for good.
    Delete {
        database: String,
        #[arg(long)]
        permanent: bool,
        /// Confirm.
        #[arg(long, short)]
        yes: bool,
    },
    /// Bring a database back from the recycle bin.
    Undelete { database: String },
}

fn mb(bytes: i64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn when(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn line(db: &Value) -> String {
    format!(
        "{}  {}  {}  {}\n  id: {}",
        db["name"].as_str().unwrap_or("?"),
        db["location"].as_str().unwrap_or("?"),
        mb(db["size_bytes"].as_i64().unwrap_or(0)),
        db["created_by"].as_str().unwrap_or("?"),
        db["id"].as_str().unwrap_or("?"),
    )
}

/// One cell of a result row, the way a terminal should show it.
fn cell(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn print_results(out: &Value) {
    let Some(statements) = out.as_array() else {
        println!("{out}");
        return;
    };
    for (i, s) in statements.iter().enumerate() {
        if statements.len() > 1 {
            println!("── statement {}", i + 1);
        }
        let rows = s["results"].as_array().cloned().unwrap_or_default();
        if let Some(first) = rows.first().and_then(Value::as_object) {
            let cols: Vec<&String> = first.keys().collect();
            println!("{}", cols.iter().map(|c| c.as_str()).collect::<Vec<_>>().join("\t"));
            for r in &rows {
                let cells: Vec<String> = cols.iter().map(|c| cell(&r[c.as_str()])).collect();
                println!("{}", cells.join("\t"));
            }
        }
        let m = &s["meta"];
        println!(
            "({} row{} · read {} · written {} · {} ms)",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
            m["rows_read"].as_i64().unwrap_or(0),
            m["rows_written"].as_i64().unwrap_or(0),
            m["duration"].as_f64().map(|d| format!("{d:.1}")).unwrap_or_else(|| "?".into()),
        );
    }
}

pub async fn run(cmd: D1Cmd, client: &Client) -> Result<()> {
    match cmd {
        D1Cmd::List { json } => {
            let l = client.call("listDatabases", json!({})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&l)?);
                return Ok(());
            }
            let dbs = l["databases"].as_array().cloned().unwrap_or_default();
            if dbs.is_empty() {
                println!("(no databases yet — `mafold d1 create <name>`)");
            }
            for db in &dbs {
                println!("{}", line(db));
            }
            let deleted = l["deleted"].as_array().cloned().unwrap_or_default();
            if !deleted.is_empty() {
                println!("\nrecycle bin:");
                for db in &deleted {
                    println!(
                        "{}\n  restorable until {}",
                        line(db),
                        when(db["purge_after"].as_i64().unwrap_or(0))
                    );
                }
            }
            let q = &l["quota"];
            println!(
                "\n{} of {} databases · {} of {}",
                q["databases"].as_i64().unwrap_or(0),
                q["max_databases"].as_i64().unwrap_or(0),
                mb(q["bytes"].as_i64().unwrap_or(0)),
                mb(q["max_bytes"].as_i64().unwrap_or(0)),
            );
        }
        D1Cmd::Create { name, location } => {
            if let Err(e) = validate_name(&name) {
                bail!("{e}");
            }
            let db = client.call("createDatabase", json!({ "name": name, "location": location })).await?;
            println!("✓ created {}\n  id: {}  ({})", db["name"].as_str().unwrap_or(&name), db["id"].as_str().unwrap_or("?"), db["location"].as_str().unwrap_or("?"));
        }
        D1Cmd::Info { database, json } => {
            let d = client.call("getDatabase", json!({ "database": database })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
                return Ok(());
            }
            println!("{}", line(&d["database"]));
            println!("  created {}", when(d["database"]["created_at"].as_i64().unwrap_or(0)));
            let points = d["restore_points"].as_array().cloned().unwrap_or_default();
            if !points.is_empty() {
                println!("restore points:");
                for p in &points {
                    println!(
                        "  {}  {}  ({}, {})",
                        when(p["created_at"].as_i64().unwrap_or(0)),
                        p["reason"].as_str().unwrap_or(""),
                        p["created_by"].as_str().unwrap_or("?"),
                        p["bookmark"].as_str().unwrap_or("?"),
                    );
                }
            }
        }
        D1Cmd::Rename { database, name } => {
            if let Err(e) = validate_name(&name) {
                bail!("{e}");
            }
            let db = client.call("renameDatabase", json!({ "database": database, "name": name })).await?;
            println!("✓ renamed to {} (id {})", db["name"].as_str().unwrap_or(&name), db["id"].as_str().unwrap_or("?"));
        }
        D1Cmd::Execute { database, command, file, params, json } => {
            let sql = match (command, file) {
                (Some(c), None) => c,
                (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("reading {f}"))?,
                _ => bail!("give the SQL with --command \"…\" or --file <path>"),
            };
            let out = client.call("queryDatabase", json!({ "database": database, "sql": sql, "params": params })).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                print_results(&out);
            }
        }
        D1Cmd::Bookmark { database, reason } => {
            let p = client.call("bookmarkDatabase", json!({ "database": database, "reason": reason })).await?;
            println!("✓ restore point {}", p["bookmark"].as_str().unwrap_or("?"));
        }
        D1Cmd::Restore { database, bookmark, at, yes } => {
            if bookmark.is_none() == at.is_none() {
                bail!("give exactly one of --bookmark <b> or --at <time>");
            }
            if !yes {
                bail!("this replaces the database's current contents — re-run with --yes (it can be undone: the old state becomes a restore point)");
            }
            let r = client
                .call("restoreDatabase", json!({ "database": database, "bookmark": bookmark, "at": at }))
                .await?;
            println!(
                "✓ restored. To undo: mafold d1 restore {database} --bookmark {} --yes",
                r["previous_bookmark"].as_str().unwrap_or("?")
            );
        }
        D1Cmd::Delete { database, permanent, yes } => {
            if !yes {
                if permanent {
                    bail!("this deletes the database for good — re-run with --yes");
                }
                bail!("this moves the database to the recycle bin (restorable for 30 days) — re-run with --yes");
            }
            let db = client.call("deleteDatabase", json!({ "database": database, "permanent": permanent })).await?;
            if permanent {
                println!("✓ deleted {} for good", db["name"].as_str().unwrap_or(&database));
            } else {
                println!(
                    "✓ {} is in the recycle bin until {} — `mafold d1 undelete {}` brings it back",
                    db["name"].as_str().unwrap_or(&database),
                    when(db["purge_after"].as_i64().unwrap_or(0)),
                    db["id"].as_str().unwrap_or(&database),
                );
            }
        }
        D1Cmd::Undelete { database } => {
            let db = client.call("undeleteDatabase", json!({ "database": database })).await?;
            println!("✓ {} is back", db["name"].as_str().unwrap_or(&database));
        }
    }
    Ok(())
}
