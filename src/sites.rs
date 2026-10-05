//! `mafold sites` — mafold.app sites from a terminal: deploy a folder of
//! static files, list, remove; and a site's own Worker — its backend, with
//! your D1 databases bound in (`.docs/site-workers-v1.md`).
//!
//! A bot deploys under itself; the databases it binds are its owner's.

use anyhow::{bail, Context, Result};
use base64::Engine;
use clap::Subcommand;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::client::Client;

#[derive(Subcommand)]
pub enum SitesCmd {
    /// Deploy a folder of static files (index.html at its root) to
    /// <site>.mafold.app. Without --site a new site is created.
    Deploy {
        dir: String,
        /// The site to update (yours).
        #[arg(long)]
        site: Option<String>,
        /// Naming hint for a new site.
        #[arg(long)]
        name: Option<String>,
    },
    /// Your sites.
    List,
    /// Remove a site: its files, and its Worker if it has one.
    Rm {
        site: String,
        #[arg(long, short)]
        yes: bool,
    },
    /// A site's own Worker — the paths it claims (default /api/*) run your code.
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Call a webview app's backend as yourself: mints your launch token for
    /// the app (`getAppLaunch`) and sends it as the Bearer. How an agent writes
    /// into a site, e.g. `mafold sites call opsdu/garden-beta POST /api/inbox
    /// --json '{"title":"…"}'`.
    Call {
        /// The app, `owner/slug`.
        app: String,
        /// GET, POST, PATCH, PUT or DELETE.
        method: String,
        /// Path on the app's site, e.g. /api/home.
        path: String,
        /// JSON request body.
        #[arg(long)]
        json: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum WorkerCmd {
    /// Upload the site's Worker (ES modules) and route its paths to it.
    Deploy {
        site: String,
        /// The main module (exports `default { fetch }`).
        main: String,
        /// Upload every .js/.mjs/.wasm under this folder too; module names
        /// (and MAIN) are relative to it.
        #[arg(long)]
        dir: Option<String>,
        /// A path the Worker answers (repeat). Default /api/*.
        #[arg(long = "route")]
        routes: Vec<String>,
        /// Bind one of your D1 databases: --d1 DB=my-database (repeat).
        #[arg(long = "d1")]
        d1: Vec<String>,
        /// A plain-text variable: --var NAME=value (repeat). Not for secrets.
        #[arg(long = "var")]
        vars: Vec<String>,
        #[arg(long)]
        compatibility_date: Option<String>,
        /// The webview app (owner/slug) hosted on this site whose launch
        /// tokens the Worker verifies (binds MAFOLD_APP_ID + MAFOLD_APP_SECRET).
        #[arg(long)]
        app: Option<String>,
    },
    /// What the site's Worker is: routes, bindings, size.
    Info {
        site: String,
        #[arg(long)]
        json: bool,
    },
    /// Take the site's Worker down (its static files stay).
    Rm {
        site: String,
        #[arg(long, short)]
        yes: bool,
    },
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Every file under `root`, skipping hidden names, as ("a/b.txt", path).
fn walk(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                go(root, &path, out)?;
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, path));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn pairs(flag: &str, items: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for it in items {
        let Some((k, v)) = it.split_once('=') else {
            bail!("--{flag} takes NAME=value (got {it:?})");
        };
        out.insert(k.trim().to_string(), v.to_string());
    }
    Ok(out)
}

fn print_worker(w: &Value) {
    println!("  routes: {}", w["routes"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default());
    for b in w["d1"].as_array().cloned().unwrap_or_default() {
        println!("  env.{} → database {} ({})", b["binding"].as_str().unwrap_or("?"), b["database"].as_str().unwrap_or("?"), b["database_id"].as_str().unwrap_or("?"));
    }
    if let Some(vars) = w["vars"].as_object().filter(|v| !v.is_empty()) {
        println!("  vars: {}", vars.keys().cloned().collect::<Vec<_>>().join(", "));
    }
    println!(
        "  {} · {} KB · by {}",
        w["main_module"].as_str().unwrap_or("?"),
        w["bytes"].as_i64().unwrap_or(0) / 1024,
        w["deployed_by"].as_str().unwrap_or("?")
    );
}

pub async fn run(cmd: SitesCmd, client: &Client) -> Result<()> {
    match cmd {
        SitesCmd::Deploy { dir, site, name } => {
            let root = PathBuf::from(&dir);
            let files = walk(&root)?;
            if files.is_empty() {
                bail!("{dir} has no files");
            }
            if !files.iter().any(|(rel, _)| rel == "index.html") {
                eprintln!("note: {dir} has no index.html at its root — the site's / will be a 404");
            }
            let mut manifest = Map::new();
            for (rel, path) in &files {
                let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                manifest.insert(rel.clone(), json!({ "b64": b64(&bytes) }));
            }
            let out = client
                .call("deploySite", json!({ "site": site, "name": name, "files": Value::Object(manifest) }))
                .await?;
            println!("✓ {} files → {}", out["files"].as_u64().unwrap_or(0), out["url"].as_str().unwrap_or("?"));
            println!("  site: {}", out["site"].as_str().unwrap_or("?"));
        }
        SitesCmd::List => {
            let out = client.call("listSites", json!({})).await?;
            let items = out["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                println!("(no sites yet — `mafold sites deploy <folder>`)");
            }
            for s in &items {
                println!("{}  {}", s["site"].as_str().unwrap_or("?"), s["url"].as_str().unwrap_or("?"));
            }
        }
        SitesCmd::Rm { site, yes } => {
            if !yes {
                bail!("this deletes {site}'s files and its Worker — re-run with --yes");
            }
            client.call("removeSite", json!({ "site": site })).await?;
            println!("✓ removed {site}");
        }
        SitesCmd::Worker { cmd } => match cmd {
            WorkerCmd::Deploy { site, main, dir, routes, d1, vars, compatibility_date, app } => {
                let mut modules = Map::new();
                let main_module = match &dir {
                    Some(dir) => {
                        let root = PathBuf::from(dir);
                        for (rel, path) in walk(&root)? {
                            if [".js", ".mjs", ".wasm"].iter().any(|e| rel.ends_with(e)) {
                                let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                                modules.insert(rel, Value::String(b64(&bytes)));
                            }
                        }
                        main.trim_start_matches("./").to_string()
                    }
                    None => {
                        let path = PathBuf::from(&main);
                        let bytes = std::fs::read(&path).with_context(|| format!("reading {main}"))?;
                        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| main.clone());
                        modules.insert(name.clone(), Value::String(b64(&bytes)));
                        name
                    }
                };
                if !modules.contains_key(&main_module) {
                    bail!("{main_module} isn't among the modules under {}", dir.as_deref().unwrap_or("."));
                }
                let body = json!({
                    "site": site,
                    "main_module": main_module,
                    "modules": Value::Object(modules),
                    "routes": if routes.is_empty() { Value::Null } else { json!(routes) },
                    "d1": pairs("d1", &d1)?,
                    "vars": pairs("var", &vars)?,
                    "compatibility_date": compatibility_date,
                    "app": app,
                });
                let w = client.call("deploySiteWorker", body).await?;
                println!("✓ {site}'s Worker is live");
                print_worker(&w);
            }
            WorkerCmd::Info { site, json } => {
                let out = client.call("getSiteWorker", json!({ "site": site })).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&out)?);
                    return Ok(());
                }
                println!("{}", out["url"].as_str().unwrap_or(&site));
                if out["worker"].is_null() {
                    println!("  (static only — no Worker)");
                } else {
                    print_worker(&out["worker"]);
                }
            }
            WorkerCmd::Rm { site, yes } => {
                if !yes {
                    bail!("this takes {site}'s Worker down (its files stay) — re-run with --yes");
                }
                let out = client.call("removeSiteWorker", json!({ "site": site })).await?;
                if out["removed"].as_bool() == Some(true) {
                    println!("✓ {site} is static again");
                } else {
                    println!("{site} had no Worker");
                }
            }
        },
        SitesCmd::Call { app, method, path, json: body } => {
            let method = reqwest::Method::from_bytes(method.trim().to_uppercase().as_bytes())
                .context("method is GET, POST, PATCH, PUT or DELETE")?;
            if !path.starts_with('/') {
                bail!("path starts with / (e.g. /api/home)");
            }
            let launch = client.call("getAppLaunch", json!({ "app_id": app })).await?;
            let base = launch["url"].as_str().context("the app has no url")?.trim_end_matches('/').to_string();
            let token = launch["init_data"].as_str().context("no launch token")?;
            let mut req = reqwest::Client::new().request(method, format!("{base}{path}")).bearer_auth(token);
            if let Some(b) = body {
                let v: Value = serde_json::from_str(&b).context("--json must be valid JSON")?;
                req = req.json(&v);
            }
            let res = req.send().await.with_context(|| format!("calling {base}{path}"))?;
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
                Err(_) => println!("{text}"),
            }
            if !status.is_success() {
                bail!("{status}");
            }
        }
    }
    Ok(())
}
