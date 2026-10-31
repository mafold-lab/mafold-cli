//! `mafold apps …` — author, preview, and publish developer mini-apps.
//!
//!   mafold apps init <owner/slug>   # scaffold an app project
//!   mafold apps dev                 # bundle + watch + serve locally (live preview)
//!   mafold apps publish             # bundle + upload to Mafold
//!   mafold apps list                # the apps you can manage
//!   mafold apps remove <owner/slug> # take down an app (all versions)
//!
//! Apps are the interactive counterpart to cards: full React Native screens,
//! launched from a chat, talking to the host through the `useApp()` bridge.
//! Ownership is by ACCOUNT and the app-id is `owner/slug` (e.g. `mafold/wallet`,
//! `ops:bot/notes`) — reverse-DNS is dead (see .docs/unified-runtime-v0.md).
//!
//! This mirrors the cards pipeline (`cards.rs`): the same bundled esbuild with
//! externals injected by the host runtime, the same multipart upload, the same
//! token handling. Apps additionally externalize `@mafold/app` /
//! `@mafold/runtime-core` (the interactive SDK profile) on top of the card set.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cards::{bundle_args_with, ensure_esbuild, title_case, write};
use crate::client::Client;
use mafold_core::mafold_types::units::EN;

/// Imports the apps pipeline externalizes ON TOP of the card externals — the
/// interactive SDK profile the host injects at load time.
const APP_EXTERNALS: &[&str] = &["@mafold/app", "@mafold/runtime-core"];

#[derive(Subcommand)]
pub enum AppsCmd {
    /// Scaffold a new mini-app. Default = WEBVIEW app (one HTML file, any web
    /// stack — the primary runtime); `--rn` = the legacy remote-ui RN app (beta).
    Init {
        /// App id, `owner/slug` (e.g. `mafold/wallet`, `ops:bot/notes`).
        id: String,
        /// Parent directory to create the project in (default: current dir).
        #[arg(long, default_value = ".")]
        dir: String,
        /// Scaffold the legacy React-Native remote-ui app instead (BETA).
        #[arg(long)]
        rn: bool,
    },
    /// (beta, remote-ui apps) Bundle + watch + serve the app locally.
    Dev {
        /// App project directory (must contain mafold.app.json).
        #[arg(long, default_value = ".")]
        dir: String,
        #[arg(long, default_value_t = 8788)]
        port: u16,
    },
    /// (beta, remote-ui apps) Bundle and publish the RN app to Mafold.
    /// Webview apps don't publish — they register a URL (`apps register`).
    Publish {
        #[arg(long, default_value = ".")]
        dir: String,
    },
    /// Register a WEBVIEW app (Telegram-mini-app model): just a URL you host.
    /// Prints the signing secret ONCE — your backend verifies launch JWTs with it.
    Register {
        /// App id, `owner/slug` (e.g. `ops/todo`).
        id: String,
        /// The externally hosted app URL (https).
        #[arg(long)]
        url: String,
        #[arg(long)]
        name: Option<String>,
        /// Lucide icon name or image URL.
        #[arg(long)]
        icon: Option<String>,
        /// Capabilities, comma-separated (e.g. `room,chat.send,storage`).
        #[arg(long, value_delimiter = ',')]
        capabilities: Vec<String>,
        /// One paragraph: what this app is. Shown on the store page and by the
        /// launch surfaces. Pass `""` to clear it.
        #[arg(long)]
        description: Option<String>,
        /// A preview image — repeat for more. Give a LOCAL IMAGE PATH (uploaded
        /// for you) or an id you already uploaded. The share card crops the
        /// first one to 5:4, so lead with the shot you want people to see.
        /// Passing none leaves the registered set alone; `--screenshot ""`
        /// clears it.
        #[arg(long = "screenshot")]
        screenshots: Vec<String>,
        /// What an agent may call on the app's own backend, as a JSON object
        /// (or `@file.json`): `{"issues.list": {"description": "…", "path":
        /// "/api/agent/issues.list", "effect": "read", "params": {JSON Schema}}}`.
        /// Each is a POST to `path` on the app's own origin with the caller's
        /// launch token. `'{}'` clears them.
        #[arg(long)]
        methods: Option<String>,
        /// Let THIS app's own origin put the Mafold login door in an iframe
        /// (`Mafold.mountSignIn`). Off until asked for: mafold.com refuses to
        /// be framed by anyone otherwise. `--auth-embed false` takes it back.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        auth_embed: Option<bool>,
        /// Mafold is this app's only way in, so the embedded door stops
        /// offering ways back out to mafold.com.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        auth_primary: Option<bool>,
        /// Let a brand-new visitor create their Mafold account inside the
        /// frame instead of being sent away to do it.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        auth_register: Option<bool>,
    },
    /// What an agent may call on an app's own backend — the methods it
    /// declares. `mafold app call` calls one.
    Methods {
        /// App id, `owner/slug`.
        app: String,
        /// Show each method's parameters (JSON Schema).
        #[arg(long)]
        schema: bool,
    },
    /// Call one of an app's methods on its own backend, as yourself: a POST to
    /// the app's origin carrying your launch token (who you are, and this
    /// conversation). The app decides what you may do; its answer is printed.
    Call {
        /// App id, `owner/slug`.
        app: String,
        /// The method (`mafold app methods <app>` lists them).
        method: String,
        /// Its parameters, a JSON object.
        #[arg(long)]
        params: Option<String>,
        /// The conversation to call from (default `$MAFOLD_CONV`, this turn's).
        #[arg(long)]
        conv: Option<String>,
    },
    /// List the apps you can manage (own the namespace of).
    List,
    /// Take down an app you own (all versions). `id` is `owner/slug`.
    Remove {
        /// App id, `owner/slug`.
        id: String,
    },
    /// Rotate a webview app's initData signing secret (old one stops working).
    RotateSecret {
        /// App id, `owner/slug`.
        id: String,
    },
    /// List your hosted sites (*.mafold.app).
    Sites,
    /// Delete a hosted site you own.
    RemoveSite {
        /// Site subdomain (the `<name>` in `<name>.mafold.app`).
        site: String,
    },
}

/// The on-disk app manifest (`mafold.app.json`). Sent verbatim as the publish
/// `meta`; the server reads `id` + `version` and stores the rest opaquely, so
/// this is a client contract. Mirrors `cards/sdk-app/index.d.ts::AppManifest`.
#[derive(Deserialize)]
struct AppManifest {
    /// GLOBAL id, `owner/slug`.
    id: String,
    version: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(rename = "kind", default)]
    kind: Option<String>,
    #[serde(default = "default_entry")]
    entry: String,
    /// Anything else in the manifest (panel/capabilities/tag/…) is preserved and
    /// re-uploaded verbatim so we don't drop fields the server stores opaquely.
    #[serde(flatten)]
    rest: serde_json::Map<String, Value>,
}
fn default_entry() -> String {
    "src/app.tsx".into()
}

pub async fn run(cmd: AppsCmd, base: String, token: Option<String>) -> Result<()> {
    match cmd {
        AppsCmd::Init { id, dir, rn } => cmd_init(&id, &dir, rn),
        AppsCmd::Dev { dir, port } => cmd_dev(&dir, port).await,
        AppsCmd::Publish { dir } => cmd_publish(&dir, base, token).await,
        AppsCmd::Register {
            id,
            url,
            name,
            icon,
            capabilities,
            description,
            screenshots,
            methods,
            auth_embed,
            auth_primary,
            auth_register,
        } => {
            cmd_register(
                &id,
                &url,
                name,
                icon,
                capabilities,
                description,
                screenshots,
                methods,
                auth_embed,
                auth_primary,
                auth_register,
                base,
                token,
            )
            .await
        }
        AppsCmd::Methods { app, schema } => cmd_methods(&app, schema, base, token).await,
        AppsCmd::Call { app, method, params, conv } => cmd_call(&app, &method, params, conv, base, token).await,
        AppsCmd::List => cmd_list(base, token).await,
        AppsCmd::Remove { id } => cmd_remove(&id, base, token).await,
        AppsCmd::RotateSecret { id } => cmd_rotate_secret(&id, base, token).await,
        AppsCmd::Sites => cmd_sites(base, token).await,
        AppsCmd::RemoveSite { site } => cmd_remove_site(&site, base, token).await,
    }
}

async fn cmd_rotate_secret(id: &str, base: String, token: Option<String>) -> Result<()> {
    let token = token.context("needs your token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    let r = client
        .call("rotateAppSecret", serde_json::json!({ "id": id }))
        .await?;
    println!(
        "✓ rotated. NEW signing secret (shown once):\n  {}",
        r["secret"].as_str().unwrap_or("?")
    );
    println!("\nold secret no longer verifies — update your backend now.");
    Ok(())
}

async fn cmd_sites(base: String, token: Option<String>) -> Result<()> {
    let token = token.context("needs your token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    let r = client.call("listSites", serde_json::json!({})).await?;
    let items = r["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no sites)");
        return Ok(());
    }
    for s in items {
        println!(
            "• {}  {}  (updated {})",
            s["site"].as_str().unwrap_or("?"),
            s["url"].as_str().unwrap_or(""),
            s["updated_at"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

async fn cmd_remove_site(site: &str, base: String, token: Option<String>) -> Result<()> {
    let token = token.context("needs your token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    client
        .call("removeSite", serde_json::json!({ "site": site }))
        .await?;
    println!("✓ removed {site}");
    Ok(())
}

// ───────────────────────────── init ─────────────────────────────

fn cmd_init(id: &str, dir: &str, rn: bool) -> Result<()> {
    let (owner, slug) = parse_app_id(id).context(
        "id must be `owner/slug` (e.g. `mafold/wallet`); reverse-DNS is no longer valid",
    )?;
    let root = Path::new(dir).join(&slug);
    if root.exists() {
        anyhow::bail!("{} already exists", root.display());
    }

    // LEGACY (beta): the remote-ui RN scaffold, kept behind --rn.
    if rn {
        std::fs::create_dir_all(root.join("src"))?;
        let title = title_case(&slug);
        write(&root.join("mafold.app.json"), &manifest_json(id, &title))?;
        write(&root.join("src/app.tsx"), &sample_app(&slug, &title))?;
        write(&root.join("package.json"), &package_json(&slug))?;
        write(&root.join("README.md"), &readme(id, &slug))?;
        write(&root.join(".gitignore"), "dist/\nnode_modules/\n")?;
        println!(
            "✓ created REMOTE-UI (beta) app `{owner}/{slug}` in {}",
            root.display()
        );
        println!("\nnext:");
        println!("  cd {}", root.display());
        println!("  mafold apps dev               # live preview at http://127.0.0.1:8788");
        println!(
            "  mafold apps publish           # ship it (needs your token; you must own `{owner}`)"
        );
        return Ok(());
    }

    // DEFAULT: a WEBVIEW app — one HTML file, any web stack, hosted anywhere
    // (or on Mafold via deploySite). .docs/webview-apps.md is the guide.
    std::fs::create_dir_all(&root)?;
    let title = title_case(&slug);
    write(&root.join("index.html"), &webview_sample(&title))?;
    write(&root.join("README.md"), &webview_readme(id, &slug))?;
    println!(
        "✓ created webview app `{owner}/{slug}` in {}",
        root.display()
    );
    println!("\nnext:");
    println!(
        "  1. open {}/index.html in a browser (SDK mocks off-Mafold gracefully)",
        root.display()
    );
    println!("  2. host it anywhere with https — or let Mafold host it:");
    println!("       POST /api/deploySite  (see https://mafold.com/docs/apps/publishing)");
    println!(
        "  3. mafold apps register {owner}/{slug} --url https://… --capabilities room,chat.send"
    );
    println!("  4. install it into a conversation — the launcher icon appears");
    println!("\n(the old React-Native remote-ui scaffold is still available as BETA: --rn)");
    Ok(())
}

fn webview_sample(title: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  body {{ font-family: -apple-system, system-ui, sans-serif; margin: 0; padding: 16px;
         background: var(--mafold-bg, #fff); color: var(--mafold-text, #111); }}
  button {{ padding: 10px 14px; border-radius: 10px; border: none; cursor: pointer;
            background: var(--mafold-accent, #06f); color: #fff; font-size: 14px; }}
</style>
</head>
<body>
<h2>{title}</h2>
<p id="who">…</p>
<p>Shared taps: <b id="count">0</b></p>
<button id="tap">Tap (everyone sees it)</button>
<script src="https://mafold.com/js/mafold-webapp.js"></script>
<script>
  var M = window.Mafold;
  var u = M.initDataUnsafe;
  document.getElementById("who").textContent = u ? "hi @" + u.user.username : "(not launched from Mafold)";
  M.room.onUpdate(function (doc) {{
    document.getElementById("count").textContent = (doc && doc.taps && doc.taps.value) || doc && doc.taps || 0;
  }});
  M.room.open();
  document.getElementById("tap").onclick = function () {{ M.room.increment("taps", 1); }};
  M.ready();
</script>
</body>
</html>
"#
    )
}

fn webview_readme(id: &str, slug: &str) -> String {
    format!(
        r#"# {slug} — a Mafold webview mini-app

One HTML file, any web stack. Full guide: https://mafold.com/docs/apps

## Ship it

1. Host `index.html` anywhere with https (Vercel, your server, …) — or let
   Mafold host it via `POST /api/deploySite` (→ `https://<site>.mafold.app`).
2. Register: `mafold apps register {id} --url https://… --capabilities room,chat.send,storage`
   Keep the printed signing secret if your app has its own backend (it verifies
   the `Mafold.initData` JWT). Pure client-side apps can ignore it.
3. Install into a conversation → the launcher icon appears.

## Iterate

Redeploy/redeploy your URL — no re-registration needed. `mafold apps register`
again only to change name/icon/capabilities (the secret is kept).
"#
    )
}

// ───────────────────────────── dev ─────────────────────────────

async fn cmd_dev(dir: &str, port: u16) -> Result<()> {
    let manifest = read_manifest(dir)?;
    let (_owner, slug) =
        parse_app_id(&manifest.id).context("mafold.app.json id must be `owner/slug`")?;
    let entry = Path::new(dir).join(&manifest.entry);
    if !entry.exists() {
        anyhow::bail!("entry not found: {}", entry.display());
    }
    let esbuild = ensure_esbuild().await?;
    let out = format!("dist/{slug}.js");

    println!(
        "→ serving {} on http://127.0.0.1:{port}/{slug}.js",
        manifest.id
    );
    println!("  watching {} (Ctrl-C to stop)\n", manifest.entry);

    let mut c = crate::platform::console_command(&esbuild);
    c.current_dir(dir)
        .arg(&manifest.entry)
        .args(bundle_args_with(&out, APP_EXTERNALS))
        .arg("--watch")
        .arg("--servedir=dist")
        .arg(format!("--serve=127.0.0.1:{port}"));
    let status = c.status().await.context("failed to run esbuild")?;
    if !status.success() {
        anyhow::bail!("esbuild exited with {status}");
    }
    Ok(())
}

// ───────────────────────────── publish ─────────────────────────────

async fn cmd_publish(dir: &str, base: String, token: Option<String>) -> Result<()> {
    let token =
        token.context("publish needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let mut manifest = read_manifest(dir)?;
    let (_owner, slug) = parse_app_id(&manifest.id)
        .context("mafold.app.json id must be `owner/slug` (reverse-DNS is no longer valid)")?;
    if manifest.version.trim().is_empty() {
        anyhow::bail!("mafold.app.json version must be a non-empty string");
    }
    let entry = Path::new(dir).join(&manifest.entry);
    if !entry.exists() {
        anyhow::bail!("entry not found: {}", entry.display());
    }

    let client = Client::new(base, token);

    // If `icon` points at a LOCAL image file (relative to the project dir),
    // upload it and rewrite `icon` to the served `/media/…` path so the published
    // manifest carries a real logo. A lucide glyph name or an existing URL is
    // left untouched. See apps/AppIcon.tsx (web renders either kind).
    if let Some(icon) = manifest.icon.clone() {
        if let Some(path) = local_logo_path(dir, &icon) {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading icon {}", path.display()))?;
            let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("logo");
            println!("→ uploading logo {} ({}) …", icon, EN.bytes(bytes.len() as f64));
            let r = client.upload_media(bytes, fname, mime_for(fname)).await?;
            let url = r["url"]
                .as_str()
                .context("uploadFile returned no url")?
                .to_string();
            println!("  logo:  {url}");
            manifest.icon = Some(url);
        }
    }

    let esbuild = ensure_esbuild().await?;
    let out_dir = Path::new(dir).join("dist");
    std::fs::create_dir_all(&out_dir)?;
    let out = out_dir.join(format!("{slug}.js"));

    println!("→ bundling {} …", manifest.entry);
    let status = crate::platform::console_command(&esbuild)
        .current_dir(dir)
        .arg(&manifest.entry)
        .args(bundle_args_with(&format!("dist/{slug}.js"), APP_EXTERNALS))
        .arg("--minify")
        .status()
        .await
        .context("failed to run esbuild")?;
    if !status.success() {
        anyhow::bail!("esbuild exited with {status}");
    }
    let bundle = std::fs::read(&out).with_context(|| format!("reading {}", out.display()))?;
    println!("  bundle: {} ({})", out.display(), EN.bytes(bundle.len() as f64));

    // The server stores the manifest verbatim, so send the FULL manifest JSON
    // (not a hand-picked subset) — reconstruct it from the parsed fields + the
    // flattened `rest` so panel/capabilities/tag survive.
    let meta = manifest.to_json();
    let r = client.publish_app(&meta, bundle).await?;
    let id = r["id"].as_str().unwrap_or(&manifest.id);
    let version = r["version"].as_str().unwrap_or(&manifest.version);
    let url = r["url"].as_str().unwrap_or("?");
    println!("\n✓ published {id}@{version}\n  url:   {url}");
    Ok(())
}

// ───────────────────────────── list ─────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_register(
    id: &str,
    url: &str,
    name: Option<String>,
    icon: Option<String>,
    capabilities: Vec<String>,
    description: Option<String>,
    screenshots: Vec<String>,
    methods: Option<String>,
    auth_embed: Option<bool>,
    auth_primary: Option<bool>,
    auth_register: Option<bool>,
    base: String,
    token: Option<String>,
) -> Result<()> {
    let token =
        token.context("register needs your token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    // `--methods` inline or `@file`; checked here for shape, by the server for
    // everything else (names, paths on the app's own origin).
    let methods = match methods {
        Some(s) => {
            let text = match s.strip_prefix('@') {
                Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
                None => s,
            };
            match serde_json::from_str::<Value>(&text).context("--methods must be JSON")? {
                Value::Object(m) => Some(m),
                _ => anyhow::bail!("--methods must be a JSON object of name → {{description, path, effect?, params?}}"),
            }
        }
        None => None,
    };
    // Only the flags actually typed go up; the server merges them over what is
    // registered. Sending all three would mean `--auth-embed` quietly turns the
    // other two off, which is the bug this shape exists to avoid.
    let auth = {
        let mut m = serde_json::Map::new();
        for (k, v) in [("embed", auth_embed), ("primary", auth_primary), ("register", auth_register)] {
            if let Some(v) = v {
                m.insert(k.into(), Value::Bool(v));
            }
        }
        (!m.is_empty()).then_some(m)
    };
    let client = Client::new(base, token);
    // Screenshots are given as local image PATHS or as ids already uploaded.
    // Uploading them here is the difference between "add a preview" and "go run
    // uploadFile yourself and paste the id back" — the second is the ergonomics
    // the product refuses everywhere else.
    let shots = upload_screenshots(&client, &screenshots).await?;
    let r = client
        .call(
            "registerWebApp",
            serde_json::json!({
                "id": id, "url": url, "name": name, "icon": icon,
                "capabilities": capabilities, "methods": methods,
                "description": description, "auth": auth,
                // Absent ⇒ the server keeps what's registered; `[]` clears.
                "screenshots": shots,
            }),
        )
        .await?;
    println!(
        "✓ registered {} → {}",
        r["id"].as_str().unwrap_or(id),
        r["url"].as_str().unwrap_or(url)
    );
    match r["secret"].as_str() {
        Some(s) => {
            println!("\nsigning secret (shown ONCE — store it in your app's backend):\n  {s}");
            println!("\nverify a launch: jwt.verify(Mafold.initData, secret, {{algorithms:[\"HS256\"]}})");
        }
        None => println!("(registration updated; existing signing secret unchanged)"),
    }
    Ok(())
}

/// `mafold app methods <app>`: what its backend lets an agent do.
async fn cmd_methods(app: &str, schema: bool, base: String, token: Option<String>) -> Result<()> {
    let token = token.context("methods needs your token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    let r = client.call("getAppMethods", json!({ "app_id": app })).await?;
    let methods = r["methods"].as_object().cloned().unwrap_or_default();
    if methods.is_empty() {
        println!("{app} declares no methods — there is no backend of its an agent can call");
        return Ok(());
    }
    for (name, m) in &methods {
        println!(
            "• {name} [{}] — {}",
            m["effect"].as_str().unwrap_or("write"),
            m["description"].as_str().unwrap_or("")
        );
        if schema {
            println!("    params: {}", serde_json::to_string(&m["params"]).unwrap_or_default());
        }
    }
    println!("\ncall one: mafold app call {app} <method> --params '<json>'");
    Ok(())
}

/// How long `mafold app call` waits for the app, and how much of its answer
/// it reads — the same bounds the server keeps for a hosted agent's call.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_ANSWER: usize = 256 * 1024;

/// `mafold app call <app> <method>`: POST to the method's path on the app's
/// own origin with a launch token for this account — the request the app's
/// page makes for a person, made for an agent. Straight from this machine;
/// Mafold only says who is asking.
async fn cmd_call(
    app: &str,
    method: &str,
    params: Option<String>,
    conv: Option<String>,
    base: String,
    token: Option<String>,
) -> Result<()> {
    let token = token.context("call needs your token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let params: Value = match params.as_deref() {
        None => json!({}),
        Some(s) => serde_json::from_str(s).context("--params must be a JSON object")?,
    };
    let conv = conv.or_else(|| std::env::var("MAFOLD_CONV").ok()).filter(|c| !c.trim().is_empty());
    let client = Client::new(base, token);
    // The room in the token is where the app thinks it is being asked from —
    // linear hands that room's board to whoever calls from it. A turn outside
    // the owner's circle calls only from a room its people could open
    // themselves (`chat::ensure_may_open`), the rule `mafold room --conv` kept
    // until room was retired: the owner's DM with this agent keeps its apps'
    // data to itself. Such a turn always has a room; it never calls without.
    match conv.as_deref() {
        Some(c) => crate::chat::ensure_may_open(&client, c).await,
        None if !crate::chat::ReadsFor::from_env().is_all() => {
            eprintln!("✗ this turn calls an app only from a room — pass --conv");
            std::process::exit(crate::chat::EXIT_REFUSED);
        }
        None => {}
    }
    let declared = client.call("getAppMethods", json!({ "app_id": app })).await?;
    let path = declared["methods"][method]["path"]
        .as_str()
        .with_context(|| format!("{app} has no method `{method}` — `mafold app methods {app}` lists them"))?
        .to_string();
    let launch = client
        .call("getAppLaunch", json!({ "app_id": app, "conversation_id": conv }))
        .await?;
    let init = launch["init_data"].as_str().context("no launch token came back")?;
    let origin = reqwest::Url::parse(launch["url"].as_str().unwrap_or_default()).context("the app's url does not parse")?;
    let target = origin.join(&path).context("the method's path does not parse")?;
    // The token is who you are: it goes to the app it was minted for, nowhere else.
    if target.origin() != origin.origin() {
        anyhow::bail!("{app}'s method `{method}` points off the app's own origin — refusing to send your token there");
    }
    let http = public_only_client(&target).await.with_context(|| format!("{app}'s method `{method}`"))?;
    let mut resp = http
        .post(target)
        .bearer_auth(init)
        .header(reqwest::header::USER_AGENT, "Mafold-App-Call/1")
        .json(&params)
        .send()
        .await
        .with_context(|| format!("{app} did not answer"))?;
    let status = resp.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.with_context(|| format!("{app} broke off"))? {
        if bytes.len() + chunk.len() > MAX_ANSWER {
            anyhow::bail!("{app}'s answer is over {MAX_ANSWER} bytes");
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    println!("{}", serde_json::to_string_pretty(&body)?);
    if !status.is_success() {
        eprintln!("✗ {app} answered {status}");
        std::process::exit(1);
    }
    Ok(())
}

/// A client for one request to `target` that can only reach the public
/// internet. This runs on its owner's machine, beside their router, their NAS
/// and whatever listens on localhost — and the address is the app's, i.e.
/// whoever registered it; an agent talked into calling it must not become a
/// way in. The same rules as the api's own fetches (`mafold_types::net`):
/// the host as written, then every address it resolves to (refused, not
/// filtered — a public and a private answer at once is a rebinding setup),
/// and the connection pinned to the addresses just checked, no redirects.
async fn public_only_client(target: &reqwest::Url) -> Result<reqwest::Client> {
    use mafold_core::mafold_types::net::{host_is_public, ip_is_public};
    if !matches!(target.scheme(), "http" | "https") {
        anyhow::bail!("only http(s) addresses are called");
    }
    let host = target.host_str().unwrap_or_default();
    if !host_is_public(host) {
        anyhow::bail!("{host} is not on the public internet — refusing to call it from this machine");
    }
    let mut builder = reqwest::Client::builder().timeout(CALL_TIMEOUT).redirect(reqwest::redirect::Policy::none());
    let literal = host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok();
    if !literal {
        let name = host;
        let port = target.port_or_known_default().unwrap_or(443);
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((name, port))
            .await
            .with_context(|| format!("{name} does not resolve"))?
            .collect();
        if let Some(inside) = addrs.iter().find(|a| !ip_is_public(a.ip())) {
            anyhow::bail!("{name} resolves to {}, which is not on the public internet — refusing to call it", inside.ip());
        }
        if addrs.is_empty() {
            anyhow::bail!("{name} has no addresses");
        }
        builder = builder.resolve_to_addrs(name, &addrs);
    }
    Ok(builder.build()?)
}

async fn cmd_list(base: String, token: Option<String>) -> Result<()> {
    let token =
        token.context("list needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    let client = Client::new(base, token);
    let r = client.list_apps().await?;
    let items = r["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no apps)");
        return Ok(());
    }
    for a in items {
        let id = a["id"].as_str().unwrap_or("?");
        let ver = a["version"].as_str().unwrap_or("?");
        let name = a["manifest"]["name"].as_str().unwrap_or("");
        if name.is_empty() {
            println!("• {id}  {ver}");
        } else {
            println!("• {id}  {ver}  ({name})");
        }
    }
    Ok(())
}

// ───────────────────────────── remove ─────────────────────────────

async fn cmd_remove(id: &str, base: String, token: Option<String>) -> Result<()> {
    let token =
        token.context("remove needs your bot token — pass --token or set $MAFOLD_BOT_TOKEN")?;
    // Validate the shape locally for a friendlier error; the server re-checks
    // both the shape and that you own the namespace.
    parse_app_id(id).context("id must be `owner/slug`")?;
    let client = Client::new(base, token);
    let r = client.remove_app(id).await?;
    if r["removed"].as_bool() == Some(true) {
        println!("✓ removed {id} (all versions)");
    } else {
        println!("• nothing to remove for {id} (no such app, or already gone)");
    }
    Ok(())
}

// ───────────────────────────── helpers ─────────────────────────────

impl AppManifest {
    /// Reassemble the manifest JSON exactly as authored: the named fields plus
    /// every flattened-through field. Sent verbatim as the publish `meta`.
    fn to_json(&self) -> Value {
        let mut m = self.rest.clone();
        m.insert("id".into(), json!(self.id));
        m.insert("version".into(), json!(self.version));
        if let Some(n) = &self.name {
            m.insert("name".into(), json!(n));
        }
        if let Some(i) = &self.icon {
            m.insert("icon".into(), json!(i));
        }
        if let Some(k) = &self.kind {
            m.insert("kind".into(), json!(k));
        }
        m.insert("entry".into(), json!(self.entry));
        Value::Object(m)
    }
}

/// If `icon` names a LOCAL image file (relative to the project dir) — not a
/// lucide glyph name and not a URL/data URI — return its path. Requires the file
/// to exist with a known image extension.
/// `--screenshot` args → the file ids the registry stores, uploading any that
/// are local image paths.
///
/// Returns `None` when nothing was passed, so the server keeps the registered
/// set; `Some([])` for an explicit `--screenshot ""`, which clears it.
async fn upload_screenshots(client: &Client, args: &[String]) -> Result<Option<Vec<String>>> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for arg in args {
        let arg = arg.trim();
        if arg.is_empty() {
            continue;
        }
        let path = Path::new(arg);
        if !path.is_file() {
            // Not a path on disk — take it as an id the caller already has.
            out.push(arg.to_string());
            continue;
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {arg}"))?;
        let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("screenshot.png");
        println!("→ uploading {} ({}) …", fname, EN.bytes(bytes.len() as f64));
        let r = client.upload_media(bytes, fname, mime_for(fname)).await?;
        let file_id = r["id"]
            .as_str()
            .context("uploadFile returned no id")?
            .to_string();
        println!("  screenshot: {file_id}");
        out.push(file_id);
    }
    Ok(Some(out))
}

fn local_logo_path(dir: &str, icon: &str) -> Option<std::path::PathBuf> {
    if icon.starts_with("http://") || icon.starts_with("https://") || icon.starts_with("data:") {
        return None;
    }
    let ext = Path::new(icon)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let is_image = matches!(
        ext.as_deref(),
        Some("png" | "jpg" | "jpeg" | "svg" | "webp" | "gif" | "avif")
    );
    if !is_image {
        return None;
    }
    let path = Path::new(dir).join(icon);
    path.is_file().then_some(path)
}

/// Best-effort content-type from a filename extension (the server also re-derives
/// + gates the stored extension).
fn mime_for(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("avif") => "image/avif",
        _ => "application/octet-stream",
    }
}

fn read_manifest(dir: &str) -> Result<AppManifest> {
    let path = Path::new(dir).join("mafold.app.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no mafold.app.json in {dir} — run `mafold apps init` first"))?;
    serde_json::from_str(&text).context("mafold.app.json is not valid JSON")
}

/// `owner/slug` → (owner, slug), validating both halves. Mirrors the server's
/// `apps_api.rs::parse_app_id` exactly so a publish that would be rejected fails
/// here with a clearer message (the server stays authoritative — it re-checks
/// the shape AND namespace ownership).
fn parse_app_id(id: &str) -> Option<(String, String)> {
    let (owner, slug) = id.split_once('/')?;
    if valid_owner(owner) && valid_slug(slug) {
        Some((owner.to_string(), slug.to_string()))
    } else {
        None
    }
}

/// Account-username shape, allowing one `:` for namespaced accounts (`mafold:ai`).
fn valid_owner(owner: &str) -> bool {
    if owner.is_empty() || owner.len() > 64 {
        return false;
    }
    let segs: Vec<&str> = owner.split(':').collect();
    if segs.len() > 2 {
        return false;
    }
    segs.iter().all(|s| {
        !s.is_empty()
            && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

/// `[a-z][a-z0-9-]*`, ≤64 — same shape as a card tag / the server's `valid_slug`.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn manifest_json(id: &str, title: &str) -> String {
    format!(
        "{{\n  \"id\": \"{id}\",\n  \"name\": \"{title}\",\n  \"icon\": \"app-window\",\n  \"version\": \"0.1.0\",\n  \"entry\": \"src/app.tsx\",\n  \"panel\": {{ \"size\": \"tall\" }},\n  \"capabilities\": [\"storage\"]\n}}\n"
    )
}

fn package_json(slug: &str) -> String {
    format!(
        "{{\n  \"name\": \"{slug}-app\",\n  \"private\": true,\n  \"version\": \"0.1.0\",\n  \"peerDependencies\": {{\n    \"react\": \"*\",\n    \"react-native\": \"*\"\n  }}\n}}\n"
    )
}

fn sample_app(slug: &str, title: &str) -> String {
    // Automatic JSX, so no `import React` for the runtime. react-native is
    // provided by the host (react-native-web on web, Hermes on iOS); @mafold/app
    // exposes the interactive SDK (defineApp / useApp).
    let comp = title.replace(' ', "");
    format!(
        r#"import {{ View, Text, Pressable, StyleSheet }} from "react-native";
import {{ defineApp, useApp }} from "@mafold/app";

function {comp}() {{
  const app = useApp();
  const t = app.theme.tokens;
  const {{ me }} = app.context;
  return (
    <View style={{[styles.screen, {{ backgroundColor: t.bg }}]}}>
      <Text style={{[styles.title, {{ color: t.text }}]}}>Hello, {{me.displayName}} 👋</Text>
      <Text style={{[styles.sub, {{ color: t.muted }}]}}>{title} mini-app</Text>
      <Pressable
        style={{[styles.btn, {{ backgroundColor: t.accent }}]}}
        onPress={{() => app.ui.close()}}
      >
        <Text style={{{{ color: t.onAccent, fontWeight: "600" }}}}>Close</Text>
      </Pressable>
    </View>
  );
}}

const styles = StyleSheet.create({{
  screen: {{ flex: 1, padding: 20, gap: 12, justifyContent: "center" }},
  title: {{ fontSize: 22, fontWeight: "700" }},
  sub: {{ fontSize: 14 }},
  btn: {{ marginTop: 8, paddingVertical: 11, borderRadius: 12, alignItems: "center" }},
}});

export default defineApp(
  {{
    id: "{slug}",
    name: "{title}",
    icon: "app-window",
    version: "0.1.0",
    entry: "src/app.tsx",
  }},
  {comp},
);
"#,
        comp = comp,
        slug = slug,
        title = title,
    )
}

fn readme(id: &str, slug: &str) -> String {
    format!(
        "# {slug} app\n\nA Mafold mini-app written in React Native (`{id}`).\n\n```\nmafold apps dev       # live preview\nmafold apps publish   # ship it (you must own the namespace)\nmafold apps list      # the apps you can manage\nmafold apps remove {id}\n```\n"
    )
}

#[cfg(test)]
mod call_tests {
    use super::*;

    /// `mafold app call` runs on its owner's machine: an app registered at an
    /// address inside it (localhost, the LAN, a cloud's metadata address) is
    /// refused before anything is sent — whoever registered the app chose the
    /// address, and an agent may be talked into calling it.
    #[tokio::test]
    async fn an_app_at_an_address_inside_this_machines_network_is_not_called() {
        for at in [
            "http://127.0.0.1:8080/admin/delete",
            "http://localhost/x",
            "http://[::1]:3000/x",
            "https://192.168.1.1/x",
            "http://169.254.169.254/latest/meta-data",
            "http://router.local/x",
            "http://[::ffff:10.0.0.1]/x",
        ] {
            let url = reqwest::Url::parse(at).unwrap();
            let err = public_only_client(&url).await.expect_err(at);
            assert!(format!("{err:#}").contains("public internet"), "{at}: {err:#}");
        }
        let ftp = reqwest::Url::parse("ftp://8.8.8.8/x").unwrap();
        assert!(public_only_client(&ftp).await.is_err(), "only http(s)");
        // A public literal is fine (no lookup needed to build the client).
        assert!(public_only_client(&reqwest::Url::parse("https://8.8.8.8/x").unwrap()).await.is_ok());
    }
}
