//! Signing a runtime in: Claude Code's `claude auth login`, driven without a
//! terminal — the sign-in link goes to whoever asked, the Authentication Code
//! comes back from them. ONE implementation behind two doors (activity center
//! C11): a bot's `/login` in chat (`agent::login_flow`), and
//! `mafold runtime login claude-code` — the desktop app's "sign in to Claude
//! Code" (it opens the link and hands the code back), or a person at a
//! terminal.
//!
//! The host's browser never opens (a no-op `open` first on PATH, `$BROWSER`
//! neutralised): the link is handed over instead, so it can be opened on any
//! device — and so `claude` waits for the code on stdin, the one way both doors
//! can answer it.

use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::commands::WhoProbe;

/// How long the link may take to appear, and the code to come back.
pub(crate) const LINK_WAIT: Duration = Duration::from_secs(60);
pub(crate) const CODE_WAIT: Duration = Duration::from_secs(300);

/// What to sign in.
pub(crate) struct Ask {
    /// A named login (`crate::accounts`) — a second subscription next to the
    /// machine's own. None (or `default`) = this machine's own login.
    pub name: Option<String>,
    /// The Anthropic Console door instead of claude.ai.
    pub console: bool,
}

/// What happens before the sign-in finishes, in order — each door says it its
/// own way (chat messages; one JSON object per line).
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event {
    /// Starting: which login (`default` = this machine's own), and whether the
    /// name is new on this machine.
    Started { account: String, adding: bool },
    /// Open this (on any device), approve, then hand back the Authentication
    /// Code.
    Link { url: String },
}

/// Why it didn't sign in. `code`: `account` (the name couldn't be recorded),
/// `spawn` (`claude` didn't start), `no_link` (no sign-in link within
/// [`LINK_WAIT`]), `no_code` (no code within [`CODE_WAIT`]), `cancelled` (the
/// code's source went away), `rejected` (`claude` refused the code).
#[derive(Serialize, Debug, Clone, PartialEq)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub detail: String,
}

impl Failure {
    fn new(code: &'static str, detail: impl Into<String>) -> Failure {
        Failure { code, detail: detail.into() }
    }
}

/// What a finished sign-in changed: the receipt's facts.
pub(crate) struct Outcome {
    /// The named login signed in; None = the machine's own.
    pub named: Option<String>,
    /// The name was new: this sign-in ADDED a login to the machine.
    pub added: bool,
    /// Who held the slot before, and who holds it now.
    pub before: WhoProbe,
    pub after: WhoProbe,
    /// Every login on the machine afterwards, registry order.
    pub names: Vec<String>,
    /// Another login holding the very same subscription.
    pub twin: Option<String>,
    /// `claude auth status` — asked only when `after` couldn't be read.
    pub status: String,
    /// Signed in, but the bookkeeping after it failed (said, not fatal).
    pub note: Option<String>,
}

/// Sign `ask` in. `code` is where the Authentication Code comes from — polled
/// only once the link is out; `None` = it went away. Progress goes to
/// `events`.
pub(crate) async fn sign_in(
    ask: &Ask,
    code: impl std::future::Future<Output = Option<String>>,
    events: &UnboundedSender<Event>,
) -> Result<Outcome, Failure> {
    let wanted = ask.name.as_deref().map(|n| n.trim().to_ascii_lowercase()).filter(|n| n != crate::accounts::DEFAULT);
    // Registered BEFORE the login runs: the directory has to exist for
    // `claude` to write the credential into it, and a half-finished sign-in
    // leaving a named-but-empty seat is harmless — it probes as "not logged
    // in" and a turn steps over it.
    let (account, added) = match &wanted {
        Some(n) => {
            let recorded = crate::accounts::update_registry(|reg| {
                let existed = reg.get(n).is_some();
                reg.add(n).map(|a| (a, !existed))
            });
            match recorded {
                Ok(Ok((a, is_new))) => (Some(a), is_new),
                Ok(Err(e)) => return Err(Failure::new("account", e)),
                Err(e) => return Err(Failure::new("account", format!("Couldn't record the account: {e}"))),
            }
        }
        None => (None, false),
    };
    let seat_env = account.as_ref().map(|a| a.env()).unwrap_or_default();
    // Who holds the slot BEFORE the sign-in overwrites it — the receipt has to
    // be able to say "replaced X", not just "signed in".
    let before = crate::commands::login_identity(&seat_env).await;
    let name = account.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| crate::accounts::DEFAULT.into());
    let _ = events.send(Event::Started { account: name.clone(), adding: added });

    let mode = if ask.console { "--console" } else { "--claudeai" };
    drive(crate::harness::program("claude"), mode, &seat_env, &noop_open_dir(), code, events, LINK_WAIT, CODE_WAIT).await?;

    // Whatever this seat's health was, it is stale now; a fresh sign-in lifts
    // a remembered refusal outright.
    crate::accounts::forget_seat(&name);
    let note = crate::accounts::update_registry(|reg| {
        reg.signed_out.remove(&name);
        reg.clear_exhausted(&name);
    })
    .err()
    .map(|e| format!("Signed in, but couldn't clear the old account status: {e}"));
    let after = crate::commands::login_identity(&seat_env).await;
    let status = match &after {
        WhoProbe::Known(_) => String::new(),
        _ => crate::commands::auth_status_line(&seat_env).await,
    };
    let reg = crate::accounts::load();
    let names: Vec<String> = reg.accounts.iter().map(|a| a.name.clone()).collect();
    // The other slots, asked who they hold: signing a second slot into the
    // SAME subscription (the browser quietly reused the first account) looks
    // like success and leaves nothing to switch to.
    let twin = match &after {
        WhoProbe::Known(me) => {
            let others: Vec<&crate::accounts::Account> = reg.accounts.iter().filter(|a| a.name != name).collect();
            let envs: Vec<Vec<(String, String)>> = others.iter().map(|a| a.env()).collect();
            let who = futures_util::future::join_all(envs.iter().map(|e| crate::commands::login_identity(e))).await;
            others
                .iter()
                .zip(who)
                .find(|(_, w)| matches!(w, WhoProbe::Known(id) if id.same_as(me)))
                .map(|(a, _)| a.name.clone())
        }
        _ => None,
    };
    if let (Some(a), WhoProbe::Known(id)) = (&account, &after) {
        // Remember the email so the account is identifiable everywhere it is
        // listed. From the seat's own profile: `claude auth status` under a
        // seat env reports the shared file's identity, i.e. the default login's.
        crate::accounts::set_email(&a.name, id.email.clone());
    }
    Ok(Outcome { named: account.map(|a| a.name), added, before, after, names, twin, status, note })
}

/// Run `program auth login <mode>` for the seat `seat_env` selects: hand the
/// sign-in link to `events`, then feed it the code `code` yields, and succeed
/// once `claude` exits 0.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive(
    program: OsString,
    mode: &str,
    seat_env: &[(String, String)],
    noop: &Path,
    code: impl std::future::Future<Output = Option<String>>,
    events: &UnboundedSender<Event>,
    link_wait: Duration,
    code_wait: Duration,
) -> Result<(), Failure> {
    // Suppress the host browser (macOS opens via `open <url>`): a no-op `open`
    // first on PATH, `$BROWSER` neutralised. COLUMNS keeps the URL unwrapped.
    let path = std::env::join_paths(
        std::iter::once(noop.to_path_buf()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or_else(|_| std::env::var("PATH").unwrap_or_default());
    // Resolved by the caller, not bare: the child's PATH is overridden here,
    // and on Windows only the resolved `claude.cmd` is spawnable at all.
    let mut cmd = crate::platform::command(program);
    cmd.args(["auth", "login", mode])
        .env("PATH", path)
        .env("COLUMNS", "4096")
        .env("NO_COLOR", "1")
        // WHICH login this signs in: empty for the machine's own, a
        // credential directory for a named second account.
        .envs(seat_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    cmd.env("BROWSER", "/usr/bin/true"); // a real no-op binary only exists on Unix
    let mut child = cmd.spawn().map_err(|e| Failure::new("spawn", format!("Couldn't start `claude auth login`: {e}")))?;
    let mut stdin = child.stdin.take();

    // stdout + stderr, one line channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(o) = child.stdout.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut l = BufReader::new(o).lines();
            while let Ok(Some(line)) = l.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    if let Some(e) = child.stderr.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut l = BufReader::new(e).lines();
            while let Ok(Some(line)) = l.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);

    // 1. The link.
    let link = async {
        while let Some(line) = rx.recv().await {
            if let Some(url) = extract_auth_url(&crate::commands::strip_ansi(&line)) {
                return Some(url);
            }
        }
        None
    };
    let Some(url) = tokio::time::timeout(link_wait, link).await.ok().flatten() else {
        let _ = child.start_kill();
        return Err(Failure::new(
            "no_link",
            "Couldn't get a sign-in link — this environment may need a TTY. Run `claude auth login` on the host.",
        ));
    };
    let _ = events.send(Event::Link { url });

    // 2. The code, into its stdin — closing stdin is what makes it go on.
    match tokio::time::timeout(code_wait, code).await {
        Ok(Some(code)) => {
            if let Some(mut si) = stdin.take() {
                let _ = si.write_all(format!("{}\n", code.trim()).as_bytes()).await;
                let _ = si.flush().await;
            }
        }
        Ok(None) => {
            let _ = child.start_kill();
            return Err(Failure::new("cancelled", "Sign-in cancelled — no code came back."));
        }
        Err(_) => {
            let _ = child.start_kill();
            return Err(Failure::new(
                "no_code",
                format!("Sign-in timed out — no code within {} min.", code_wait.as_secs() / 60),
            ));
        }
    }

    // 3. Its verdict.
    while rx.recv().await.is_some() {}
    match child.wait().await {
        Ok(st) if st.success() => Ok(()),
        _ => Err(Failure::new("rejected", "Sign-in didn't complete — the code may have been wrong or expired.")),
    }
}

/// The OAuth sign-in URL in a line of `claude auth login` output — only real
/// `https://` links (NOT url-encoded `https%3A…` param values), preferring
/// the actual authorize endpoint so the relayed link is clickable.
pub(crate) fn extract_auth_url(line: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut from = 0usize;
    while let Some(rel) = line[from..].find("https://") {
        let start = from + rel;
        let tok: String = line[start..].chars().take_while(|c| !c.is_whitespace()).collect();
        let tok = tok.trim_end_matches(|c: char| matches!(c, '.' | ',' | ')' | '"' | '\'' | '>')).to_string();
        from = start + "https://".len();
        if tok.contains("authorize") || tok.contains("oauth") {
            return Some(tok);
        }
        if best.as_ref().map(|b| b.len() < tok.len()).unwrap_or(true) {
            best = Some(tok);
        }
    }
    best.filter(|u| u.len() > 24)
}

/// A directory holding a no-op `open` shim — stops the host browser from
/// launching during a sign-in (the link is handed over instead).
pub(crate) fn noop_open_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".mafold/noopbin");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        let open = dir.join("open");
        if !open.exists() && std::fs::write(&open, "#!/bin/sh\nexit 0\n").is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755));
        }
    }
    #[cfg(windows)]
    {
        // Windows browser-launch doesn't shell out to an `open` binary, but
        // shadow one anyway as a harmless no-op for anything that does.
        let open = dir.join("open.cmd");
        if !open.exists() {
            let _ = std::fs::write(&open, "@echo off\r\nexit /b 0\r\n");
        }
    }
    dir
}

// ---------------- `mafold runtime login claude-code` ----------------

/// The door for a program (the desktop app) or a person: `--json` prints each
/// step as one JSON object per line — `{"event":"started",…}`,
/// `{"event":"link","url":…}`, then `{"event":"done",…}` or
/// `{"event":"failed","code":…}` — and reads the code from stdin (one line).
pub(crate) async fn command(ask: Ask, json: bool) -> anyhow::Result<()> {
    use std::io::Write as _;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let say = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if json {
                println!("{}", serde_json::to_string(&ev).unwrap_or_default());
            } else {
                match &ev {
                    Event::Started { account, adding: true } => println!("Signing in a new Claude Code login, `{account}`."),
                    Event::Started { account, .. } if account == crate::accounts::DEFAULT => {
                        println!("Signing in this computer's own Claude Code login.")
                    }
                    Event::Started { account, .. } => println!("Signing in Claude Code login `{account}` again."),
                    Event::Link { url } => {
                        println!("Open this to sign in (any device works):\n  {url}");
                        print!("Then paste the Authentication Code here: ");
                    }
                }
            }
            let _ = std::io::stdout().flush();
        }
    });
    // One non-empty line from stdin; end of input = nobody's answering.
    let code = async {
        tokio::task::spawn_blocking(|| loop {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) if line.trim().is_empty() => continue,
                Ok(_) => return Some(line.trim().to_string()),
            }
        })
        .await
        .ok()
        .flatten()
    };
    let res = sign_in(&ask, code, &tx).await;
    drop(tx);
    let _ = say.await;
    match res {
        Ok(o) => {
            if json {
                println!("{}", serde_json::to_string(&done_json(&o))?);
            } else {
                if let Some(n) = &o.note {
                    println!("{n}");
                }
                println!("{}", done_line(&o));
            }
            Ok(())
        }
        Err(f) => {
            if json {
                println!("{}", serde_json::json!({ "event": "failed", "code": f.code, "detail": f.detail }));
            } else {
                eprintln!("✗ {}", f.detail);
            }
            let _ = std::io::stdout().flush();
            std::process::exit(1)
        }
    }
}

fn who_json(w: &WhoProbe) -> serde_json::Value {
    match w {
        WhoProbe::Known(id) => serde_json::json!({ "state": "known", "email": id.email, "label": id.label() }),
        WhoProbe::SignedOut => serde_json::json!({ "state": "signed_out" }),
        WhoProbe::Unknown => serde_json::json!({ "state": "unknown" }),
    }
}

/// `{"event":"done",…}`: which login, who holds it now and held it before,
/// whether that replaced someone, the machine's logins, a twin.
pub(crate) fn done_json(o: &Outcome) -> serde_json::Value {
    let replaced = matches!((&o.before, &o.after), (WhoProbe::Known(b), WhoProbe::Known(a)) if !b.same_as(a));
    serde_json::json!({
        "event": "done",
        "account": o.named.as_deref().unwrap_or(crate::accounts::DEFAULT),
        "added": o.added,
        "now": who_json(&o.after),
        "before": who_json(&o.before),
        "replaced": replaced,
        "logins": o.names,
        "twin": o.twin,
        "status": (!o.status.is_empty()).then_some(&o.status),
        "note": o.note,
    })
}

/// The same, in one line for a person at a terminal.
fn done_line(o: &Outcome) -> String {
    let now = match &o.after {
        WhoProbe::Known(id) => id.label(),
        _ if !o.status.is_empty() => format!("a login I couldn't identify ({})", o.status),
        _ => "a login I couldn't identify".into(),
    };
    let which = o.named.as_deref().unwrap_or(crate::accounts::DEFAULT);
    let mut out = match o.added {
        true => format!("✓ added Claude Code login `{which}` — {now}"),
        false => format!("✓ signed in `{which}` — {now}"),
    };
    if let (WhoProbe::Known(b), WhoProbe::Known(a)) = (&o.before, &o.after) {
        if !b.same_as(a) {
            out.push_str(&format!(" (replaced {})", b.label()));
        }
    }
    if let Some(t) = &o.twin {
        out.push_str(&format!(
            "\n⚠️ that's the same subscription `{t}` holds — sign in again with the OTHER Anthropic account (a private window stops the browser reusing this one)"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in `claude`: prints a sign-in link (unless told not to), reads
    /// the code from stdin, exits 0 for `good-code`.
    #[cfg(unix)]
    fn fake_claude(dir: &Path, link: bool) -> OsString {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("claude");
        let say = if link { "echo 'Browser didn'\"'\"'t open? Use the url below: https://claude.ai/oauth/authorize?code=true&client_id=x'" } else { "echo 'no link here'" };
        std::fs::write(&p, format!("#!/bin/sh\n{say}\nread code\n[ \"$code\" = good-code ]\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.into_os_string()
    }

    #[cfg(unix)]
    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mafold-rtlogin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    async fn run(tag: &str, link: bool, code: Option<&'static str>, code_wait: Duration) -> (Result<(), Failure>, Vec<Event>) {
        let d = tmp(tag);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let code = async move {
            match code {
                Some(c) => Some(c.to_string()),
                None => std::future::pending().await,
            }
        };
        let r = drive(fake_claude(&d, link), "--claudeai", &[], &d, code, &tx, Duration::from_secs(10), code_wait).await;
        drop(tx);
        let mut seen = Vec::new();
        while let Some(e) = rx.recv().await {
            seen.push(e);
        }
        let _ = std::fs::remove_dir_all(&d);
        (r, seen)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn the_link_goes_out_and_the_right_code_signs_in() {
        let (r, seen) = run("ok", true, Some("good-code"), Duration::from_secs(10)).await;
        assert_eq!(r, Ok(()));
        assert_eq!(seen, vec![Event::Link { url: "https://claude.ai/oauth/authorize?code=true&client_id=x".into() }]);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_code_claude_refuses_is_rejected() {
        let (r, seen) = run("bad", true, Some("nope"), Duration::from_secs(10)).await;
        assert_eq!(r.map_err(|f| f.code), Err("rejected"));
        assert_eq!(seen.len(), 1, "the link went out first");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn no_link_in_time_is_said() {
        let t = std::time::Instant::now();
        let (r, seen) = run("nolink", false, Some("good-code"), Duration::from_secs(10)).await;
        assert_eq!(r.map_err(|f| f.code), Err("no_link"));
        assert!(seen.is_empty());
        // `claude` waiting for input with no link printed: the link wait runs
        // out (10 s here) rather than hanging on.
        assert!(t.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn no_code_in_time_is_said_and_claude_is_stopped() {
        let (r, _) = run("nocode", true, None, Duration::from_millis(300)).await;
        assert_eq!(r.map_err(|f| f.code), Err("no_code"));
    }

    #[test]
    fn the_authorize_link_is_found_and_encoded_ones_are_not() {
        assert_eq!(
            extract_auth_url("Open https://claude.ai/oauth/authorize?redirect_uri=https%3A%2F%2Fx.y%2Fcb&state=s."),
            Some("https://claude.ai/oauth/authorize?redirect_uri=https%3A%2F%2Fx.y%2Fcb&state=s".into())
        );
        assert_eq!(extract_auth_url("nothing here"), None);
    }
}
