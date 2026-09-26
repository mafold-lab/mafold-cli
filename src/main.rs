//! mafold-cli — Mafold from your terminal.
//!
//!   mafold agent --token mb_… --workdir ~/repo   # run Claude Code as your bot
//!   mafold --token mb_… chats                     # list conversations
//!   mafold --token mb_… send @alice "hi there"    # send a message
//!
//! Auth is a bot token (`mb_…`) via --token or $MAFOLD_BOT_TOKEN.

mod accounts;
mod agent;
mod apps;
mod ask_hook;
mod bash_hook;
mod cards;
mod cardtags;
mod channels;
mod chat;
mod client;
mod commands;
mod computer;
mod connection;
mod daemon;
mod discover;
mod drafts;
mod harness;
mod inbox;
mod install;
mod langpack;
mod mcp_link;
mod pair;
mod permission_mcp;
mod platform;
mod room;
mod session;
mod steer_hook;
mod supervisor;
mod turnenv;
mod update;
mod vault;
mod wallet;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use client::{Client, Dest};

#[derive(Parser)]
#[command(
    name = "mafold",
    version,
    about = "Mafold from your terminal — CLI client + coding-agent daemon (Claude Code, Codex, …)"
)]
struct Cli {
    #[arg(
        long,
        env = "MAFOLD_BASE",
        default_value = "https://api.mafold.com",
        global = true
    )]
    base: String,
    /// A credential: never echo its value in `--help` (clap prints env values
    /// by default, which put a live session token into an agent's transcript).
    #[arg(long, env = "MAFOLD_BOT_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Act as this logged-in human account (default: the current one).
    /// Applies to every command that speaks as a person — `connection`,
    /// `report`, and the control plane.
    #[arg(long, env = "MAFOLD_ACCOUNT", global = true)]
    account: Option<String>,
    /// Disable the agent's hourly auto-update.
    #[arg(long, env = "MAFOLD_NO_AUTO_UPDATE", global = true)]
    no_auto_update: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run an agent harness as your bot (daemon): receive messages, reply with
    /// the local agent CLI in the working directory.
    Agent {
        /// Working directory the harness runs in. If omitted, the bot's
        /// owner-set server config (`cwd`/`workdir`) is used, else the current
        /// directory. An explicit flag always wins over the server config.
        #[arg(long, env = "MAFOLD_WORKDIR")]
        workdir: Option<String>,
        /// Which agent harness to drive: claude-code (default), opencode, codex,
        /// openclaw. (Others land as they're implemented.)
        #[arg(long, env = "MAFOLD_HARNESS", default_value = "claude-code")]
        harness: String,
        /// Run in the background (detached from the terminal) so it keeps
        /// running after you close the shell. Logs to ~/.mafold/agent.log.
        #[arg(long, short)]
        detach: bool,
        /// Run the INBOX loop instead of the bot loop: look at the account's
        /// chats the way a person does (events + heartbeat), think, and speak
        /// only through `mafold send` / `mafold react` — the turn's own text
        /// goes to a log, never into a chat. Works for any account, human or
        /// bot (`--account <name>` for a human). See `.docs/clone-ceo-v1.md`.
        #[arg(long)]
        inbox: bool,
        #[command(flatten)]
        inbox_opts: inbox::InboxOpts,
    },
    /// Stop a background agent started with `agent --detach`.
    Stop,
    /// Show whether a background agent is running.
    Status,
    /// Update mafold to the latest release.
    Update {
        /// Switch this machine's release channel, then update into it.
        ///
        /// `stable` (default) follows published releases. `dev` follows the
        /// prereleases built from `cli.dev@` tags — for trying a build on a
        /// real machine WITHOUT turning auto-update off. The choice is sticky
        /// (`~/.mafold/channel`): the supervisor and every agent here follow it
        /// until you switch back with `--channel stable`.
        #[arg(long, value_name = "stable|dev")]
        channel: Option<String>,
    },
    /// Install a coding-agent runtime (claude-code / codex / kimi-code /
    /// opencode). No argument lists the runtimes + their install state.
    Install {
        /// The runtime to install; omit to list all with their state.
        tool: Option<String>,
        /// Don't ask before running the official installer.
        #[arg(long, short)]
        yes: bool,
    },
    /// List your conversations.
    Chats,
    /// Read a conversation's recent messages. `mafold read` with no argument
    /// reads the room this turn is in; naming a room you are NOT in asks its
    /// people for a ticket instead of failing (exit 3 = waiting on their tap).
    ///
    /// The counterpart to `send`: until this existed the CLI could write to any
    /// conversation it could name and read none of them.
    Read {
        /// Conversation id, @username, or a name from `mafold chats`.
        chat: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Read a forum channel instead of the main timeline.
        #[arg(long)]
        channel: Option<String>,
        /// Download the transcript's photos and files to ~/.mafold/attachments
        /// and print each one's path, so the agent can actually open them.
        /// Off by default: a long page of a photo-heavy room is a lot of bytes
        /// to fetch for a question about the text.
        #[arg(long)]
        media: bool,
        /// Machine-readable page, for code rather than for a model.
        #[arg(long)]
        json: bool,
        /// Only what you haven't read yet (as many as the chat's unread badge,
        /// capped by --limit), then mark it read — what opening a chat does.
        #[arg(long)]
        unread: bool,
        /// Print each message's id, for `send --reply <id>` / `react <id>`.
        #[arg(long)]
        ids: bool,
    },
    /// Chat-history tickets: ask for one, see what you hold, revoke one you gave.
    Access {
        #[command(subcommand)]
        cmd: chat::AccessCmd,
    },
    /// Send a message. <chat> is a conversation id or a @username.
    Send {
        chat: String,
        /// Send into a forum channel (id or #name) instead of the main timeline.
        #[arg(long)]
        channel: Option<String>,
        /// Quote-reply to this message id (`mafold read --ids` shows them).
        #[arg(long)]
        reply: Option<String>,
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    /// React to a message with an emoji (`--remove` takes it back).
    React {
        /// The message id (`mafold read --ids` shows them).
        message: String,
        emoji: String,
        #[arg(long)]
        remove: bool,
    },
    /// Attach local files to the reply you are streaming right now — images,
    /// clips, documents. Run by an AGENT mid-turn (the daemon presets
    /// MAFOLD_DRAFT), so what it just made arrives in the same bubble as the
    /// text about it. The kind is read from the bytes: an image becomes a photo,
    /// a clip becomes a player, anything else becomes a file card.
    Attach {
        /// Files on this machine.
        #[arg(required = true)]
        files: Vec<String>,
        /// Message to attach to. Defaults to `$MAFOLD_DRAFT` — the in-flight
        /// reply — which is what an agent almost always wants.
        #[arg(long)]
        message: Option<String>,
    },
    /// Manage a forum's channels (list/create/rename/close/pin/delete).
    Channels {
        #[command(subcommand)]
        cmd: channels::ChannelsCmd,
    },
    /// Your credentials at third parties (Claude Code, Anthropic, OpenAI,
    /// Codex, Notion, Figma) — encrypted so only your own devices can read
    /// them. `mafold connection list` to see them.
    Connection {
        #[command(subcommand)]
        cmd: connection::ConnectionCmd,
    },
    /// Lend THIS machine to a Mafold account without signing in: it gets one
    /// connection's key and may answer for that, and holds no session, no
    /// master key and no socket. For a box you don't trust with your account.
    Pair {
        /// What to call this machine in the approval screen. Defaults to its
        /// hostname.
        #[arg(long)]
        name: Option<String>,
        /// Forget the pairing stored on this machine. Does NOT revoke it on
        /// the account — that is `deleteConnection`, or the key in Settings.
        #[arg(long)]
        forget: bool,
    },
    /// Token wallet: balances / transfer / convert / rates / history / grants.
    Wallet {
        #[command(subcommand)]
        cmd: wallet::WalletCmd,
    },
    /// Author, preview, and publish developer cards.
    Cards {
        #[command(subcommand)]
        cmd: cards::CardsCmd,
    },
    /// Author, preview, and publish developer mini-apps.
    Apps {
        #[command(subcommand)]
        cmd: apps::AppsCmd,
    },
    /// Read/write an app's shared CRDT room in a conversation (the AI's room
    /// peer — backs the `mafold-room` skill). Conversation via `--conv` /
    /// `MAFOLD_CONV`; auth via `--token` / `MAFOLD_BOT_TOKEN`.
    Room {
        #[command(subcommand)]
        cmd: room::RoomCmd,
    },
    /// Publish the cloud language packs (langpacks/*.json) — first-party only.
    Langpack {
        #[command(subcommand)]
        cmd: langpack::LangpackCmd,
    },

    // ── multi-daemon supervisor: one daemon per bot ──
    /// Add a bot daemon to the local config (token from --token).
    Add {
        /// The bot username (also the daemon's pid/log name).
        name: String,
        #[arg(long, env = "MAFOLD_WORKDIR")]
        workdir: String,
        /// Local harness hint; the server (getMe) is authoritative at runtime.
        #[arg(long)]
        harness: Option<String>,
        /// Extra environment for this daemon (repeatable, KEY=VALUE) — e.g.
        /// `--env CLAUDE_SECURESTORAGE_CONFIG_DIR=~/.mafold/claude-accounts/work`
        /// pins it to one Claude login (see `/login <name>` for adding logins)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
    },
    /// Remove a bot daemon from the local config (stops it too).
    Rm { name: String },
    /// Start the supervisor — keeps all configured daemons running + owns updates.
    Up,
    /// Stop the supervisor + all daemons (or one daemon by name).
    Down { name: Option<String> },
    /// Show the last lines of a bot daemon's log.
    Logs { name: String },

    // ── human control plane (New-Bot harness recommendation + provisioning) ──
    /// Log in your HUMAN account so the Mafold app can recommend a harness from
    /// this machine (and soon auto-provision bots). Reports installed harnesses.
    Login {
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
    },
    /// The human accounts logged in on this machine: list them, switch which
    /// one commands act as, or forget one. No argument lists them.
    Account {
        #[command(subcommand)]
        cmd: Option<AccountCmd>,
    },
    /// Re-report this machine's available harnesses (uses the saved login).
    Report,
    /// Roll back to the previous binary (after a bad update).
    Rollback,
    /// (internal) The long-lived supervisor loop — started by `up`.
    #[command(hide = true)]
    Supervise {
        /// Hide this process's console window (Windows: the logon task hands a
        /// console binary a visible console at sign-in). Keep this flag forever:
        /// registered tasks reference it, so removing it would make every
        /// existing task fail at logon with a clap error (exit code 2).
        #[arg(long, hide = true)]
        hidden: bool,
    },
    /// (internal) PreToolUse hook claude runs for AskUserQuestion — blocks until
    /// the user answers the chat card, then feeds the answer back. Not for humans.
    #[command(hide = true)]
    AskHook,
    /// (internal) PreToolUse hook claude runs for Bash — detaches
    /// run_in_background tasks into their own session so they survive the turn
    /// (claude kills its background shells at exit). Not for humans.
    #[command(hide = true)]
    BashHook,
    /// (internal) PostToolUse hook claude runs after every tool — delivers what
    /// the user said mid-turn, so a long run can be corrected instead of killed.
    /// Not for humans.
    #[command(hide = true)]
    SteerHook,
    /// (internal) The stdio MCP server claude asks when a permission RULE says a
    /// human has to approve a tool call — puts the question in the chat as an
    /// ask card and blocks on the tap. Not for humans.
    #[command(hide = true)]
    PermissionMcp,
}

#[derive(Subcommand)]
enum AccountCmd {
    /// List the accounts logged in on this machine.
    List,
    /// Make this the account commands act as from now on.
    Use { username: String },
    /// Sign an account out here: revoke its session server-side, then forget
    /// it locally. The account itself is untouched — `mafold login` brings it
    /// back.
    Rm {
        username: String,
        /// Only forget it here, leaving the session alive server-side. For a
        /// machine you can't reach the api from; the session then has to be
        /// killed by hand in Settings ▸ Active Sessions.
        #[arg(long)]
        local: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // The agent stores its pid/log/config under `~/.mafold`, keyed off `$HOME`.
    // Windows doesn't set HOME — fall back to USERPROFILE so the same paths work.
    if std::env::var_os("HOME").is_none() {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            std::env::set_var("HOME", profile);
        }
    }

    let cli = Cli::parse();

    // The AskUserQuestion hook is invoked by claude (a child of the daemon) and
    // needs no auth — it just bridges stdin/the answer file. Handle it first.
    if matches!(cli.cmd, Cmd::AskHook) {
        return ask_hook::run();
    }
    if matches!(cli.cmd, Cmd::BashHook) {
        return bash_hook::run();
    }
    if matches!(cli.cmd, Cmd::SteerHook) {
        return steer_hook::run();
    }
    if matches!(cli.cmd, Cmd::PermissionMcp) {
        return permission_mcp::run();
    }

    // `--account` decides whose session the person-shaped commands speak with,
    // so it is resolved ONCE here rather than re-derived per command. Naming an
    // account this machine hasn't logged in gets a list, not a silent fallback
    // to somebody else's credentials — running as the wrong person "works"
    // right up until it writes something. `login` and `account` are exempt:
    // there the name is the thing being created or inspected.
    if let Some(want) = cli.account.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let exempt = matches!(cli.cmd, Cmd::Login { .. } | Cmd::Account { .. });
        if !exempt && session::load_named(want).is_none() {
            let known = session::all();
            anyhow::bail!(
                "no account @{want} on this machine{}",
                if known.is_empty() {
                    " — run `mafold login` first".to_string()
                } else {
                    format!(
                        " — logged in here: {}",
                        known.iter().map(|s| format!("@{}", s.username)).collect::<Vec<_>>().join(", ")
                    )
                }
            );
        }
        session::select(want);
    }

    if let Cmd::Account { cmd } = &cli.cmd {
        return account_cmd(&cli.base, cmd.as_ref()).await;
    }

    // Daemon control + self-update need no auth.
    if matches!(cli.cmd, Cmd::Stop) {
        return daemon::stop();
    }
    if matches!(cli.cmd, Cmd::Status) {
        let _ = daemon::status(); // legacy single `agent --detach`
        supervisor::status(); // multi-daemon config
        return Ok(());
    }
    match &cli.cmd {
        Cmd::Up => return supervisor::up(&cli.base, cli.no_auto_update),
        Cmd::Down { name } => return supervisor::down(name.as_deref()),
        Cmd::Logs { name } => return supervisor::logs(name),
        Cmd::Rm { name } => return supervisor::rm(name),
        Cmd::Rollback => return update::rollback(),
        Cmd::Supervise { hidden } => {
            if *hidden {
                platform::hide_console();
            }
            supervisor::supervise(cli.base, !cli.no_auto_update).await;
            return Ok(());
        }
        _ => {}
    }
    if let Cmd::Update { channel } = &cli.cmd {
        // No release binary is built for this platform (e.g. linux-arm64) → don't
        // claim "up to date" (the check would always no-op). Be honest instead.
        if !update::platform_supported() {
            println!("no mafold release is built for your platform — self-update isn't available.\nSee https://github.com/mafold-lab/mafold-cli/releases");
            return Ok(());
        }
        // `--channel` is persisted BEFORE the update runs, so the update that
        // follows is already the new channel's — one command switches and
        // lands, instead of switching and leaving the machine on the old line
        // until the next tick.
        let channel = match channel {
            Some(name) => {
                let c = update::Channel::parse(name).with_context(|| {
                    format!("unknown channel {name:?} — expected `stable` or `dev`")
                })?;
                c.save()?;
                println!("✓ channel → {}", c.as_str());
                c
            }
            None => update::Channel::current(),
        };
        let http = reqwest::Client::new();
        match update::update_to_latest(&http, &cli.base, channel).await {
            Ok(Some(v)) => println!("✓ updated to v{v} — restart a running agent with: mafold stop && mafold agent --detach …"),
            Ok(None) => println!(
                "already up to date (v{}, {} channel)",
                update::current_version(),
                channel.as_str()
            ),
            Err(e) => { eprintln!("update failed: {e}"); std::process::exit(1); }
        }
        return Ok(());
    }
    // Cards: init/dev need no token; publish/list check for one themselves.
    if matches!(cli.cmd, Cmd::Cards { .. }) {
        let Cmd::Cards { cmd } = cli.cmd else {
            unreachable!()
        };
        return cards::run(cmd, cli.base, cli.token).await;
    }
    // Apps: init/dev need no token; publish/list/remove check for one themselves.
    if matches!(cli.cmd, Cmd::Apps { .. }) {
        let Cmd::Apps { cmd } = cli.cmd else {
            unreachable!()
        };
        return apps::run(cmd, cli.base, cli.token).await;
    }
    // Room: the AI's CRDT room peer. Auth via --token / MAFOLD_BOT_TOKEN.
    if matches!(cli.cmd, Cmd::Room { .. }) {
        let Cmd::Room { cmd } = cli.cmd else {
            unreachable!()
        };
        return room::run(cmd, cli.base, cli.token).await;
    }
    // Langpack: publish/list check for the first-party token themselves.
    if matches!(cli.cmd, Cmd::Langpack { .. }) {
        let Cmd::Langpack { cmd } = cli.cmd else {
            unreachable!()
        };
        return langpack::run(cmd, cli.base, cli.token).await;
    }
    // Human control plane: `login` mints the s_ session; `report` uses it. No bot token.
    if matches!(cli.cmd, Cmd::Login { .. }) {
        let Cmd::Login { username, password } = cli.cmd else {
            unreachable!()
        };
        return login(&cli.base, username, password, cli.no_auto_update).await;
    }
    if matches!(cli.cmd, Cmd::Report) {
        return report_harnesses(&cli.base).await;
    }
    // Connections are the PERSON's, so they run on the human session too — a bot
    // token must never be able to enumerate its owner's credentials.
    if let Cmd::Connection { cmd } = cli.cmd {
        return connection::run(&cli.base, cmd).await;
    }
    // Pairing runs on NO credential of the account's — that is what it is for.
    // It must sit above the token gate below, or the one command meant for a
    // machine that has nothing would demand a bot token first.
    if let Cmd::Pair { name, forget } = cli.cmd {
        return pair::run(&cli.base, name, forget).await;
    }
    // Machine setup — no account or token involved at all.
    if let Cmd::Install { tool, yes } = &cli.cmd {
        return install::run(tool.as_deref().unwrap_or(""), *yes);
    }

    // The bot loop and `add` drive a BOT and need its mb_ token. Everything that
    // just talks in chats speaks as whoever `speaking_token` resolves — which
    // may be a person (`--account`), since the API has one door for both.
    let bot_token = cli.token.clone().context(
        "set --token or $MAFOLD_BOT_TOKEN (your bot's mb_ token — create a bot in the Mafold app)",
    );
    let token = speaking_token(cli.token.clone(), cli.account.as_deref());

    match cli.cmd {
        Cmd::Agent { workdir, harness, inbox: true, inbox_opts, detach } => {
            if detach {
                anyhow::bail!(
                    "--inbox doesn't detach — run it under your service manager (systemd / launchd) or nohup"
                );
            }
            let workdir = workdir.map(|w| {
                std::fs::canonicalize(&w)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(w)
            });
            inbox::run(Client::new(cli.base, token?), workdir, harness, inbox_opts).await?;
        }
        Cmd::Agent {
            workdir,
            harness,
            detach,
            ..
        } => {
            let token = bot_token?;
            // An explicit --workdir wins; if omitted, the server owner-config (or
            // the current dir) decides at runtime. Resolve an explicit one to an
            // absolute path so the agent — and the detached child, which has a
            // different cwd — both operate on the same real directory.
            let workdir = workdir.map(|w| {
                std::fs::canonicalize(&w)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(w)
            });
            if detach {
                let pid = daemon::start_detached(
                    &cli.base,
                    &token,
                    workdir.as_deref(),
                    &harness,
                    cli.no_auto_update,
                )?;
                if let Some(w) = &workdir {
                    println!("  workdir: {w}");
                }
                println!("✓ agent running in background (pid {pid})");
                println!("  logs:   ~/.mafold/agent.log");
                println!("  status: mafold status");
                println!("  stop:   mafold stop");
            } else {
                agent::run(
                    Client::new(cli.base, token),
                    workdir,
                    harness,
                    !cli.no_auto_update,
                )
                .await?;
            }
        }
        Cmd::Add {
            name,
            workdir,
            harness,
            env,
        } => {
            let env = supervisor::parse_env(&env)?;
            supervisor::add(name, bot_token?, workdir, harness, env, &cli.base, cli.no_auto_update)?
        }
        Cmd::Chats => chats(&Client::new(cli.base, token?)).await?,
        Cmd::Read { chat: c, limit, channel, media, json, unread, ids } => {
            chat::read(
                &Client::new(cli.base, token?),
                chat::ReadArgs { chat: c, limit, channel, json, media, unread, ids },
            )
            .await?
        }
        Cmd::Access { cmd } => chat::run(cmd, &Client::new(cli.base, token?)).await?,
        Cmd::Send {
            chat,
            channel,
            reply,
            text,
        } => {
            send(
                &Client::new(cli.base, token?),
                &chat,
                channel.as_deref(),
                reply.as_deref(),
                &text.join(" "),
            )
            .await?
        }
        Cmd::React { message, emoji, remove } => {
            react(&Client::new(cli.base, token?), &message, &emoji, remove).await?
        }
        Cmd::Attach { files, message } => {
            attach(&Client::new(cli.base, token?), &files, message.as_deref()).await?
        }
        Cmd::Channels { cmd } => channels::run(cmd, &Client::new(cli.base, token?)).await?,
        Cmd::Wallet { cmd } => wallet::run(cmd, &Client::new(cli.base, token?)).await?,
        Cmd::Stop | Cmd::Status | Cmd::Update { .. } | Cmd::Install { .. } | Cmd::Cards { .. }
        | Cmd::Apps { .. } | Cmd::Room { .. } | Cmd::Connection { .. }
        | Cmd::Pair { .. } | Cmd::Langpack { .. } | Cmd::Login { .. }
        | Cmd::Account { .. } | Cmd::Report
        | Cmd::Up | Cmd::Down { .. } | Cmd::Logs { .. } | Cmd::Rm { .. }
        | Cmd::Rollback | Cmd::Supervise { .. } | Cmd::AskHook | Cmd::BashHook
        | Cmd::SteerHook | Cmd::PermissionMcp => unreachable!(),
    }
    Ok(())
}

pub(crate) fn prompt(label: &str) -> String {
    use std::io::Write;
    print!("{label}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

/// Read a secret without echoing (Unix: toggle the tty via `stty`).
pub(crate) fn prompt_password(label: &str) -> String {
    use std::io::Write;
    print!("{label}");
    let _ = std::io::stdout().flush();
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!();
    s.trim().to_string()
}

/// `mafold login` — mint a human `s_` session + report this machine's harnesses.
async fn login(base: &str, username: Option<String>, password: Option<String>, no_auto_update: bool) -> Result<()> {
    // Default (no args) → browser device flow, à la `gh auth login`. Passing
    // --username/--password keeps the direct password login (CI / scripted).
    if username.is_none() && password.is_none() {
        return login_device(base, no_auto_update).await;
    }
    let username = username.unwrap_or_else(|| prompt("Mafold username: "));
    let password = password.unwrap_or_else(|| prompt_password("Password: "));
    let http = reqwest::Client::new();
    let resp: serde_json::Value = http
        .post(format!("{base}/api/auth/login"))
        // Same device descriptor the browser device-flow sends — without it
        // this machine lands in Active Sessions as "Unknown device".
        .json(&serde_json::json!({ "username": username, "password": password, "device": session::device_name(), "platform": std::env::consts::OS }))
        .send()
        .await
        .context("login request failed")?
        .json()
        .await
        .context("login: non-JSON response")?;
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        anyhow::bail!(
            "login failed: {}",
            resp["description"]
                .as_str()
                .unwrap_or("check username/password")
        );
    }
    let result = &resp["result"];
    let token = result["token"]
        .as_str()
        .context("login: no token in response")?
        .to_string();
    let uname = result["user"]["username"]
        .as_str()
        .unwrap_or(&username)
        .to_string();
    // Scripted path (CI, harnesses): mint the session + report, nothing more —
    // registering a boot-persistent supervisor is an interactive-machine move.
    finish_login(base, token, uname, false, no_auto_update).await
}

/// gh-style device login: get a short code, the user approves it in the Mafold
/// web app, and we poll until the session token comes back. Works on headless /
/// remote machines (no browser needed on THIS box — approve from your phone).
async fn login_device(base: &str, no_auto_update: bool) -> Result<()> {
    let http = reqwest::Client::new();
    let start: serde_json::Value = http
        .post(format!("{base}/api/auth/device/start"))
        .json(&serde_json::json!({ "device": session::device_name(), "platform": std::env::consts::OS }))
        .send().await.context("device/start failed")?
        .json().await.context("device/start: non-JSON response")?;
    let r = &start["result"];
    let device_code = r["device_code"]
        .as_str()
        .context("device/start: no device_code")?
        .to_string();
    let user_code = r["user_code"].as_str().unwrap_or("");
    let verify_url = r["verify_url"]
        .as_str()
        .unwrap_or("https://mafold.com/login/device");
    let interval = r["interval"].as_u64().unwrap_or(3).max(1);

    // The url carries the code (server-side), so the page can approve in one
    // tap — but print both anyway: the person may be reading this over ssh and
    // opening the page on another device, where they'll type the code by hand.
    let opened = platform::open_browser(verify_url);
    if opened {
        println!("\n  Opened your browser to approve this device.");
        println!("  (URL: {verify_url} — code {user_code})\n");
    } else {
        println!("\n  Open this URL in your browser:  {verify_url}");
        println!("  and enter the code:             {user_code}\n");
    }
    println!("  Waiting for you to approve…  (Ctrl-C to cancel)");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let poll: serde_json::Value = http
            .post(format!("{base}/api/auth/device/poll"))
            .json(&serde_json::json!({ "device_code": device_code }))
            .send()
            .await
            .context("device/poll failed")?
            .json()
            .await
            .context("device/poll: non-JSON response")?;
        match poll["result"]["status"].as_str().unwrap_or("") {
            "approved" => {
                let token = poll["result"]["token"]
                    .as_str()
                    .context("approved but no token")?
                    .to_string();
                let uname = poll["result"]["username"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                return finish_login(base, token, uname, true, no_auto_update).await;
            }
            "expired" => anyhow::bail!("that code expired — run `mafold login` again"),
            _ => {} // pending — keep polling
        }
    }
}

/// Persist the session + report this machine's harnesses (shared by both paths).
///
/// `auto_up`: the interactive (device-flow) login also brings the supervisor up.
/// A one-shot harness report goes stale after 90s (the api's online window), and
/// bots created on the web arrive as provisions only a running supervisor can
/// claim — so "logged in" without it is a machine that flickers online once and
/// then can never receive its first bot. The scripted path passes `false`.
async fn finish_login(base: &str, token: String, uname: String, auto_up: bool, no_auto_update: bool) -> Result<()> {
    let prev = session::load();
    let sess = session::Session {
        token,
        username: uname.clone(),
        device_id: session::device_id(prev.as_ref().map(|p| p.device_id.as_str())),
        device_name: session::device_name(),
    };
    session::save(&sess)?;
    println!("✓ logged in as {uname} on {}", sess.device_name);
    // A second login used to overwrite the first in silence. Now it stacks —
    // so say so, or someone who expected a clobber won't know the old account
    // is still here answering connection calls.
    let others: Vec<String> = session::all()
        .into_iter()
        .filter(|s| !s.username.eq_ignore_ascii_case(&uname))
        .map(|s| format!("@{}", s.username))
        .collect();
    if !others.is_empty() {
        println!("  also on this machine: {}  (mafold account)", others.join(", "));
    }
    report_with(base, &sess).await?;
    if auto_up {
        // Best-effort: a failure here must not fail the login itself.
        if let Err(e) = supervisor::up(base, no_auto_update) {
            eprintln!("note: couldn't start the supervisor ({e:#}) — run `mafold up` to keep this machine available");
        }
    } else {
        println!("\n→ keep this machine available + auto-provision new bots:  mafold up");
    }
    Ok(())
}

/// `mafold account [list|use|rm]` — the human logins this machine holds.
/// `list` and `use` never touch the network: they only read and re-point
/// ~/.mafold/session.json, so they still answer when the api is down, which is
/// exactly when you want to know who you are. Only `rm` calls out, because
/// signing out has to reach the server to mean anything.
async fn account_cmd(base: &str, cmd: Option<&AccountCmd>) -> Result<()> {
    match cmd.unwrap_or(&AccountCmd::List) {
        AccountCmd::List => {
            let accounts = session::all();
            if accounts.is_empty() {
                println!("No account logged in on this machine.  →  mafold login");
                return Ok(());
            }
            let current = session::current_username().unwrap_or_default();
            println!("{:<3}{:<24}{}", "", "ACCOUNT", "MACHINE");
            for s in &accounts {
                let mark = if s.username.eq_ignore_ascii_case(&current) { "*" } else { " " };
                println!("{mark:<3}{:<24}{}", format!("@{}", s.username), s.device_name);
            }
            // The star is the default, not a lock — say how to move it, and how
            // to override it for one command without moving it at all.
            println!(
                "\n  * = current   ·   switch: mafold account use <name>   ·   one command: --account <name>"
            );
        }
        AccountCmd::Use { username } => {
            let s = session::use_account(username)?;
            println!("✓ now acting as @{}", s.username);
        }
        AccountCmd::Rm { username, local } => {
            let Some(sess) = session::load_named(username) else {
                anyhow::bail!("no account @{username} on this machine — `mafold account` lists them");
            };
            // Revoke BEFORE forgetting. The stored token is the only thing that
            // can kill its own session, so dropping it first would strand a
            // live session nobody on this machine can reach any more — the
            // exact opposite of what signing out is for. A failure therefore
            // stops here rather than half-succeeding.
            if !*local {
                Client::new(base.to_string(), sess.token.clone())
                    .call("auth/logout", serde_json::json!({}))
                    .await
                    .with_context(|| {
                        format!(
                            "couldn't revoke @{username}'s session (nothing was forgotten, so you can retry). \
                             Offline? `mafold account rm {username} --local` forgets it here and leaves the \
                             session alive — kill it in Settings ▸ Active Sessions"
                        )
                    })?;
            }
            session::remove(username)?;
            let fate = if *local {
                "forgot here · session still ALIVE server-side"
            } else {
                "signed out · session revoked"
            };
            match session::current_username() {
                Some(next) => println!("✓ @{username} {fate} · now acting as @{next}"),
                None => println!("✓ @{username} {fate} · no accounts left on this machine"),
            }
        }
    }
    Ok(())
}

/// `mafold report` — re-report this machine's available harnesses.
async fn report_harnesses(base: &str) -> Result<()> {
    let sess = session::load().context("not logged in — run `mafold login` first")?;
    report_with(base, &sess).await
}

async fn report_with(base: &str, sess: &session::Session) -> Result<()> {
    let harnesses = harness::report_rows().await;
    let avail: Vec<&str> = harnesses
        .iter()
        .filter(|h| h["available"].as_bool() == Some(true))
        .filter_map(|h| h["id"].as_str())
        .collect();
    Client::new(base.to_string(), sess.token.clone())
        .call(
            "reportHarnesses",
            serde_json::json!({
                "device_id": sess.device_id,
                "device_name": sess.device_name,
                "cli_version": env!("CARGO_PKG_VERSION"),
                "harnesses": harnesses,
            }),
        )
        .await
        .context("reportHarnesses failed")?;
    println!(
        "✓ reported harnesses on {} — available: {}",
        sess.device_name,
        if avail.is_empty() {
            "(none detected)".to_string()
        } else {
            avail.join(", ")
        }
    );
    Ok(())
}

async fn chats(client: &Client) -> Result<()> {
    let me = client.me().await?;
    let my = me["username"].as_str().unwrap_or_default().to_lowercase();
    let result = client.chats().await?;
    let items = result["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        println!("(no conversations)");
        return Ok(());
    }
    for c in items {
        // Three things this row used to drop, each of which is the difference
        // between two rows a reader can tell apart and two they cannot:
        //
        //   • the KIND. The old fallback ("title, else the first participant
        //     who isn't me") ran without looking at it, so an untitled GROUP
        //     took one member's display name and sat here looking exactly like
        //     a DM with that person. On @opsdu's list that printed «ops» twice
        //     (a DM with `opsdu:claude-code`, display-named "ops", and a
        //     3-person group) and «fei_pota» twice. The ids were never reused
        //     — every conversation id is a fresh v4 uuid — the printer was
        //     throwing away what told them apart.
        //   • the HANDLE. Two accounts may share a display name; usernames are
        //     the unique key, so a DM prints one.
        //   • the ID. It was read nowhere, which left the list unusable as
        //     input to anything: `mafold read`/`send` take a uuid or an
        //     @username and this printed neither.
        let title = chat::label_of(&c, &my);
        let shape = chat::shape_of(
            c["kind"].as_str().unwrap_or(""),
            c["participants"].as_array().map_or(0, |p| p.len()),
        );
        // The handle only earns its place on a DM: on a group it would be the
        // arbitrary first member's, which is the very confusion being fixed.
        let handle = (c["kind"].as_str() == Some("direct"))
            .then(|| {
                c["participants"]
                    .as_array()?
                    .iter()
                    .find(|p| p["username"].as_str().map(str::to_lowercase) != Some(my.clone()))?
                    ["username"]
                    .as_str()
                    .map(|u| format!("  @{u}"))
            })
            .flatten()
            .unwrap_or_default();
        let preview = c["last_message"]["content"].as_str().unwrap_or("—");
        let unread = c["unread_count"].as_u64().unwrap_or(0);
        let badge = if unread > 0 {
            format!("  ({unread})")
        } else {
            String::new()
        };
        let oneline = preview.replace('\n', " ");
        let oneline = if oneline.chars().count() > 60 {
            format!("{}…", oneline.chars().take(60).collect::<String>())
        } else {
            oneline
        };
        let id = c["id"].as_str().unwrap_or("");
        println!("• {title}{handle}{badge}\n  {shape} · {id}\n  {oneline}");
    }
    Ok(())
}

/// Hang local files on a message we authored — the general door for "the agent
/// made something, put it in the reply". Images become photo bubbles, clips
/// become players, everything else becomes a file card (the kind is decided from
/// the bytes in `Client::attach_media`). Codex's own generated images are swept
/// up without this (see `harness::codex::ImageSweep`); every other harness, and
/// anything an agent writes with a script, comes through here.
async fn attach(client: &Client, files: &[String], message: Option<&str>) -> Result<()> {
    let msg = match message {
        Some(m) => m.to_string(),
        None => {
            let env_id = turnenv::draft().context(
                "no message to attach to — run this inside an agent turn (the daemon sets \
                 MAFOLD_DRAFT), or pass --message <id>",
            )?;
            // The reply may have MOVED since this process was spawned: a turn
            // that gets steered re-opens its draft below the message that
            // steered it, and our env still names the discarded one. The daemon
            // leaves a forwarding address keyed by the original id
            // (`agent::draft_ptr_path`); without following it, a picture the
            // agent just made would be hung on a draft that no longer exists.
            std::fs::read_to_string(agent::draft_ptr_path(&env_id))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or(env_id)
        }
    };
    for f in files {
        let path = std::path::Path::new(f);
        client
            .attach_media(&msg, path)
            .await
            .with_context(|| format!("attaching {}", path.display()))?;
        println!("✓ attached {}", path.display());
    }
    Ok(())
}

/// Whose credential the chat-shaped commands (`chats`, `read`, `send`, `react`,
/// `attach`, `channels`, `wallet`, `agent --inbox`) speak with.
///
/// An explicit `--account` (or `$MAFOLD_ACCOUNT`) names a person logged in on
/// this machine and wins even over an inherited `$MAFOLD_BOT_TOKEN`: an agent
/// running inside a bot's turn that writes `--account opsdu` means to speak as
/// that person, and silently speaking as the bot instead would be the wrong
/// author on every message. Then the bot token; then whoever is logged in here.
/// The API takes both kinds at the same door (`mafold-api/src/auth.rs`).
fn speaking_token(bot_token: Option<String>, account: Option<&str>) -> Result<String> {
    if let Some(name) = account.map(str::trim).filter(|s| !s.is_empty()) {
        // Validated against the logins on this machine at the top of `main`.
        return session::load_named(name)
            .map(|s| s.token)
            .with_context(|| format!("no account @{name} on this machine — `mafold login`"));
    }
    if let Some(t) = bot_token.filter(|t| !t.trim().is_empty()) {
        return Ok(t);
    }
    session::load().map(|s| s.token).context(
        "no identity — pass --token / $MAFOLD_BOT_TOKEN (a bot), or `mafold login` and --account <you> (a person)",
    )
}

/// `mafold send`. Two environment switches, set by the inbox loop for the agent
/// it runs and meaningful to anyone scripting a person-shaped sender:
///
/// * `MAFOLD_SEND_PACE=1` — show "typing…" first and wait roughly as long as a
///   person takes to type the text, so a burst of short messages arrives the
///   way a person's does instead of all in the same second.
/// * `MAFOLD_SEND_JOURNAL=<file>` — append one JSON line per message sent, so
///   the loop knows afterwards who its agent actually spoke to.
/// * `MAFOLD_SEND_DRY=1` — say what WOULD be sent and send nothing (the inbox
///   loop's `--dry-run`). Checked before anything touches the network: even
///   resolving an `@username` can open a DM.
async fn send(client: &Client, chat: &str, channel: Option<&str>, reply: Option<&str>, text: &str) -> Result<()> {
    if env_flag("MAFOLD_SEND_DRY") {
        journal(&serde_json::json!({
            "kind": "send", "dry": true, "chat_id": chat, "channel_id": channel, "reply_to": reply, "text": text,
        }));
        let at = channel.map(|c| format!(" #{c}")).unwrap_or_default();
        let re = reply.map(|r| format!(" (回复 #{r})")).unwrap_or_default();
        println!("✓ (dry-run,没有真发) → {chat}{at}{re}: {text}");
        return Ok(());
    }
    let (chat_id, channel_id, label) = match channel {
        Some(ch) => {
            let (chat_id, ch) = channels::resolve(client, chat, ch).await?;
            let name = ch["name"].as_str().unwrap_or("?").to_string();
            (chat_id, ch["id"].as_str().map(str::to_string), format!("{chat} #{name}"))
        }
        None => (client.resolve_chat(chat).await?, None, chat.to_string()),
    };
    if env_flag("MAFOLD_SEND_PACE") {
        pace_typing(client, &chat_id, channel_id.as_deref(), text).await;
    }
    let mut dest = Dest::chat(&chat_id).channel(channel_id.as_deref());
    dest.reply_to_message_id = reply;
    let sent = client.send_to(dest, text).await?;
    journal(&serde_json::json!({
        "kind": "send",
        "chat_id": chat_id,
        "channel_id": channel_id,
        "message_id": sent["id"],
        "reply_to": reply,
        "text": text,
    }));
    println!("✓ sent to {label}");
    Ok(())
}

async fn react(client: &Client, message: &str, emoji: &str, remove: bool) -> Result<()> {
    if env_flag("MAFOLD_SEND_DRY") {
        journal(&serde_json::json!({ "kind": "react", "dry": true, "message_id": message, "emoji": emoji }));
        println!("✓ (dry-run,没有真点) {emoji} → #{message}");
        return Ok(());
    }
    client.set_reaction(message, emoji, remove).await?;
    journal(&serde_json::json!({
        "kind": if remove { "unreact" } else { "react" },
        "message_id": message,
        "emoji": emoji,
    }));
    println!("✓ {} {emoji}", if remove { "removed" } else { "reacted" });
    Ok(())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
}

/// How long a person takes to type `text`: a beat to start, then per
/// character, capped — nobody watches dots for half a minute.
fn typing_secs(text: &str) -> f64 {
    (1.2 + 0.08 * text.chars().count() as f64).min(8.0)
}

/// Show "typing…" for about as long as typing `text` takes. Clients drop the
/// indicator after a few seconds, so it is renewed every 4s. Best-effort: a
/// failed indicator must never cost the message.
async fn pace_typing(client: &Client, chat_id: &str, channel_id: Option<&str>, text: &str) {
    let mut left = typing_secs(text);
    while left > 0.0 {
        let _ = client.send_chat_action(chat_id, channel_id, "typing").await;
        let step = left.min(4.0);
        tokio::time::sleep(std::time::Duration::from_secs_f64(step)).await;
        left -= step;
    }
}

/// Append one line to `$MAFOLD_SEND_JOURNAL`, if it is set. Best-effort.
fn journal(entry: &serde_json::Value) {
    let Ok(path) = std::env::var("MAFOLD_SEND_JOURNAL") else { return };
    if path.trim().is_empty() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{entry}");
    }
}
