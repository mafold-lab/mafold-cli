//! `mafold diagnose` — one file a person can hand to whoever is helping them
//! when Mafold misbehaves on their computer (activity center C6; the desktop
//! app's «Export diagnostics» saves the same file through a save dialog).
//!
//! What goes in is a list, not a sweep of `~/.mafold`, which also holds every
//! token this machine signs in with: `mafold status --json` (already the
//! machine's state as the desktop reads it), the bots' configuration with
//! only the fields worth asking about, and the last [`LOG_LINES`] lines of the
//! supervisor's log and each bot's.
//!
//! Only log files a cli since C6 began ([`crate::logfile::KEPT`]): before
//! that, a message went into the log as it came, line breaks and all, and no
//! rule can tell its second line from any other. Such a file is named and
//! left out. In the rest, a line that quotes something — a message somebody
//! sent, a reply, what a runtime printed — keeps its own words and the
//! quote's length, never the quote ([`crate::logfile::QUOTE`],
//! [`crate::logfile::EXITED`]).
//!
//! Then everything that is still text is masked, in this order:
//! 1. every credential on THIS machine, by value, however it is spelled —
//!    the same sweep `mafold connection run` hides output with
//!    ([`crate::conceal`]);
//! 2. what looks like someone else's: known token shapes, JWTs, a value after
//!    `Bearer` or after a key named like a secret, the password in a URL, the
//!    values of a URL's query (signed links), a long run of letters and digits;
//! 3. email addresses;
//! 4. the home directory, which carries the account name, becomes `~` — and
//!    the account name in any other `/Users/…`, `/home/…`, `C:\Users\…` path;
//! 5. this computer's name, which is often its owner's (`Alices-MacBook-Pro`).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::logfile::{EXITED, QUOTE};

/// How much of each log goes in.
pub const LOG_LINES: usize = 500;

/// What a hidden value reads as — the one `mafold connection run` and the api
/// write, so a mask looks the same everywhere.
const MASK: &str = crate::conceal::MASK;

/// What an email address reads as.
const EMAIL_MASK: &str = "••••@••••";

/// What this computer's name reads as.
const COMPUTER_MASK: &str = "[this computer]";

/// `mafold diagnose [--out FILE] [--json]`.
pub async fn run(out: Option<PathBuf>, json_out: bool) -> Result<()> {
    let now = crate::accounts::now();
    let mut bundle = gather(now).await;
    let masked = redact(&mut bundle, &Known::here());
    bundle["masked"] = json!(masked);
    let path = out.unwrap_or_else(|| PathBuf::from(default_name(now)));
    let text = serde_json::to_string_pretty(&bundle)?;
    std::fs::write(&path, &text).with_context(|| format!("couldn't write {}", path.display()))?;
    let shown = std::fs::canonicalize(&path).unwrap_or(path);
    if json_out {
        println!("{}", json!({ "path": shown.display().to_string(), "bytes": text.len(), "masked": masked }));
    } else {
        println!("✓ diagnostics written to {} ({} KB, {masked} value(s) masked)", shown.display(), text.len().div_ceil(1024));
        println!("  It holds this computer's status, its bots' settings (no tokens) and the last {LOG_LINES} lines of each log.");
        println!("  Tokens, keys, email addresses and this computer's name are masked; what people sent and what a");
        println!("  runtime printed keep only their length. Have a look before you send it.");
    }
    Ok(())
}

/// `mafold-diagnose-20261022-091502.json`, in UTC.
fn default_name(now: i64) -> String {
    let at = chrono::DateTime::from_timestamp(now, 0).unwrap_or_default();
    format!("mafold-diagnose-{}.json", at.format("%Y%m%d-%H%M%S"))
}

/// Everything that goes in, before masking.
async fn gather(now: i64) -> Value {
    let config = crate::supervisor::config_summary();
    let mut bots = serde_json::Map::new();
    for name in config["daemons"].as_array().into_iter().flatten().filter_map(|d| d["name"].as_str()) {
        bots.insert(name.to_string(), log_section(&crate::supervisor::bot_log(name)));
    }
    let mut logs = json!({
        "supervisor": log_section(&crate::supervisor::supervisor_log()),
        "bots": bots,
    });
    // A bot run with `mafold agent --detach` rather than by the supervisor.
    if let Some(detached) = crate::daemon::log_file().filter(|p| p.exists()) {
        logs["detached"] = log_section(&detached);
    }
    json!({
        "diagnose": 1,
        "generated_at": now,
        "cli": {
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "os_release": os_release(),
        },
        "status": crate::supervisor::status_json().await,
        "config": config,
        "logs": logs,
    })
}

/// The last lines of one log, each quote reduced to its length.
fn log_section(log: &Path) -> Value {
    match crate::logfile::tail(log, LOG_LINES, true) {
        Ok(t) => json!({
            "path": log.display().to_string(),
            "lines": t.lines.iter().map(|l| without_quotes(l)).collect::<Vec<_>>(),
            "left_out": t.left_out.iter().map(|p| json!({
                "path": p.display().to_string(),
                "bytes": std::fs::metadata(p).map_or(0, |m| m.len()),
                "why": "written before mafold kept this log itself, when a message went in as it came",
            })).collect::<Vec<_>>(),
        }),
        Err(e) => json!({ "path": log.display().to_string(), "error": e.to_string() }),
    }
}

/// A line with the text it quotes replaced by its length: `words ⟫ [N chars]`
/// — from whichever of [`QUOTE`] and [`EXITED`] comes first, since what
/// follows either can hold the other.
fn without_quotes(line: &str) -> String {
    let first = [QUOTE, EXITED].into_iter().filter_map(|sep| Some((line.find(sep)?, sep))).min_by_key(|(at, _)| *at);
    match first {
        Some((at, sep)) => format!("{}{sep}[{} chars]", &line[..at], line[at + sep.len()..].chars().count()),
        None => line.to_string(),
    }
}

#[cfg(unix)]
fn os_release() -> Option<String> {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return None;
    }
    let field = |f: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned();
    Some(format!("{} {}", field(&u.sysname), field(&u.release)))
}

#[cfg(not(unix))]
fn os_release() -> Option<String> {
    None
}

/// What this machine knows that a bundle must not carry, found here.
pub struct Known {
    /// Every credential on this machine ([`crate::conceal::local_credentials`]).
    pub credentials: Vec<String>,
    /// The home directory.
    pub home: Option<String>,
    /// This computer's names: as each login registered it, and `hostname`.
    pub computer: Vec<String>,
}

impl Known {
    fn here() -> Self {
        let mut computer: Vec<String> = crate::session::all().into_iter().map(|s| s.device_name).collect();
        computer.push(crate::session::device_name());
        computer.extend(std::env::var("COMPUTERNAME").ok());
        Known {
            credentials: crate::conceal::local_credentials(),
            home: std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok()),
            computer,
        }
    }
}

/// Names too common to be anybody's — masking them would only eat words.
const NOT_A_NAME: &[&str] = &[
    "this machine", "localhost", "home", "users", "server", "desktop", "laptop", "computer", "windows", "linux",
    "ubuntu", "debian", "fedora", "raspberrypi", "macbook", "macbook-pro", "macbook-air", "imac", "mafold",
];

/// Mask every string in `v` (see the module docs for the order). Returns how
/// many values were masked.
pub fn redact(v: &mut Value, known: &Known) -> usize {
    let mut masked = crate::conceal::scrub_with(v, &known.credentials);
    let home = known.home.as_deref().filter(|h| h.trim_end_matches(['/', '\\']).len() > 1).map(home_pattern);
    let mut names: Vec<&str> = known
        .computer
        .iter()
        .map(|c| c.trim())
        .filter(|c| c.chars().count() >= 4 && !NOT_A_NAME.contains(&c.to_ascii_lowercase().as_str()))
        .collect();
    // Longest first: `Alices-MacBook-Pro.local` before `Alices-MacBook-Pro`.
    names.sort_by_key(|c| std::cmp::Reverse(c.len()));
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    let computer: Vec<Regex> = names
        .iter()
        .filter_map(|n| Regex::new(&format!(r"(?i)\b{}\b", regex::escape(n))).ok())
        .collect();
    fn walk(v: &mut Value, home: Option<&Regex>, computer: &[Regex], masked: &mut usize) {
        match v {
            Value::String(s) => {
                let (out, n) = redact_text(s, home, computer);
                *masked += n;
                *s = out;
            }
            Value::Array(items) => items.iter_mut().for_each(|i| walk(i, home, computer, masked)),
            Value::Object(map) => map.values_mut().for_each(|i| walk(i, home, computer, masked)),
            _ => {}
        }
    }
    walk(v, home.as_ref(), &computer, &mut masked);
    masked
}

/// The home directory, matched however a path spells it: either slash
/// (Windows writes both), any case (neither macOS's nor Windows' file system
/// cares), with or without a trailing separator.
fn home_pattern(home: &str) -> Regex {
    let mut p = String::from("(?i)");
    for (i, c) in home.trim_end_matches(['/', '\\']).chars().enumerate() {
        match c {
            // Doubled inside, the way `{:?}` prints a Windows path
            // (`C:\\Users\\alice`) — never at the start, which would take
            // the slashes of `file:///Users/alice` along with it.
            '/' | '\\' if i == 0 => p.push_str(r"[/\\]"),
            '/' | '\\' => p.push_str(r"[/\\]{1,2}"),
            c => p.push_str(&regex::escape(&c.to_string())),
        }
    }
    // Not the start of a longer name: `/Users/al` must not eat `/Users/alice`.
    p.push_str(r"(?:[/\\]|\b|$)");
    Regex::new(&p).expect("an escaped path is a valid pattern")
}

/// Tokens by shape: a vendor prefix and a run long enough to be one. Mafold's
/// own (`mb_` bots, `s_` sessions) first.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:mb_|s_|sk-|sk_|rk_|ghp_|gho_|ghu_|ghs_|ghr_|github_pat_|glpat-|xox[abprs]-|AKIA|ASIA|AIza|ntn_|whsec_|shpat_|hf_|npm_|pypi-)",
        r"[A-Za-z0-9_\-]{12,}"
    ))
    .unwrap()
});

/// A JSON Web Token: three base64url parts, the first a JSON header.
static JWT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\beyJ[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}\.[A-Za-z0-9_\-]{6,}").unwrap());

/// What follows an `Authorization:` scheme. Not `token`: «token validation
/// failed» is a sentence — GitHub's `token ghp_…` is caught by its prefix.
static SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(bearer|basic)(\s+)([A-Za-z0-9._~+/=\-]{8,})").unwrap());

/// A value after a key named like a secret — `api_key=…`, `"password": "…"`,
/// `MAFOLD_BOT_TOKEN=…`, the same in JSON printed inside a string
/// (`\"access_token\":\"…\"`) — the names [`crate::conceal`] treats as secret,
/// plus cookies and authorization headers. A quoted value whole, whatever its
/// length; an unquoted one from 6 characters on.
static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)\b([A-Za-z0-9_.\-]*(?:token|secret|password|passwd|api[_-]?key|private[_-]?key|access[_-]?key|credential|cookie|authorization)[A-Za-z0-9_.\-]*)"#,
        r#"(\\?["']?\s*[:=]\s*)"#,
        r#"(\\?"[^"\\]+\\?"|\\?'[^'\\]+\\?'|[^\s"'\\,;&}\]]{6,})"#
    ))
    .unwrap()
});

/// Values after a secret's name that say how it went, not what it is:
/// `password: required`, `refresh_token: invalid_grant`. A list, not a shape:
/// `correcthorsebatterystaple` is all lowercase too.
const STATUS_WORDS: &[&str] = &[
    "required", "missing", "invalid", "invalid_grant", "invalid_token", "invalid_request", "invalid_client",
    "expired", "revoked", "rejected", "refused", "denied", "unauthorized", "forbidden", "failed", "error",
    "none", "null", "nil", "empty", "set", "unset", "present", "absent", "unknown", "ok", "true", "false",
    "yes", "no", "redacted", "hidden", "masked", "not_found", "not_set", "mismatch", "too_short",
];

/// A number, as a count is written: `1234`, `150000/200000`.
static COUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9][0-9.,:/+\-]*$").unwrap());

/// Whether `value`, after the secret-like name `key`, says how it went rather
/// than what the secret is: a word from [`STATUS_WORDS`], or a number after
/// the name of a count (`token_count=1234`, `context_tokens=150000/200000`) —
/// never after a password's, where digits are the secret.
fn status_value(key: &str, value: &str) -> bool {
    let key = key.to_ascii_lowercase();
    STATUS_WORDS.iter().any(|w| w.eq_ignore_ascii_case(value))
        || (COUNT.is_match(value) && (key.contains("count") || key.contains("tokens")))
}

/// The `user:password@` of a URL (the user may be empty: `redis://:pw@…`).
static USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.\-]*://)[^\s/@:]*:[^\s/@]+@").unwrap());

/// A URL's query: a signed download link (an expiring signature, a key id) is
/// as good as a password until it expires, and nothing in a log needs the
/// values to be read.
static QUERY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(\b[a-z][a-z0-9+.\-]*://[^\s?#"'<>()]*\?)([^\s#"'<>()]+)"#).unwrap());

/// A long run of letters and digits — how most keys look when they have no
/// prefix to know them by (which of these runs is one: [`opaque`]).
static OPAQUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9_\-+/]{32,}={0,2}").unwrap());

/// A part of a path: a word, not noise.
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z]?[a-z]{2,}$").unwrap());

/// Whether a long run is a key. Letters AND digits (a long name made of words
/// isn't). With a `/` or `+` in it, standard base64 — both cases, and no part
/// between slashes that is a word, the way every path has one
/// (`work/claude/ac-c6/…`). Otherwise a stretch of 20 or more between dashes:
/// a uuid, or ids joined by dashes, never has one longer than 12.
fn opaque(run: &str) -> bool {
    let body = run.trim_end_matches('=');
    let has = |f: fn(&u8) -> bool| body.as_bytes().iter().any(f);
    if !has(u8::is_ascii_digit) || !has(u8::is_ascii_alphabetic) {
        return false;
    }
    if body.contains(['/', '+']) {
        return has(u8::is_ascii_uppercase) && has(u8::is_ascii_lowercase) && !body.split('/').any(|p| WORD.is_match(p));
    }
    body.split('-').any(|part| part.len() >= 20)
}

static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}\b").unwrap());

/// The account name in somebody's home directory, wherever it is — up to the
/// next separator or space. Separators may be doubled (`{:?}` printing).
static USER_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)((?:^|[^A-Za-z0-9_.~\-])(?:[a-z]:[/\\]{1,2}|/[a-z]/|[/\\])(?:users|home)[/\\]{1,2})([^/\\\s'"<>()\[\]]+)"#)
        .unwrap()
});

/// A Windows account name with spaces in it (`C:\Users\John Smith\…`, or
/// `/c/Users/John Smith/…` the way Git Bash writes it): whole, as far as the
/// separator that ends it. Without one after it, [`USER_DIR`] takes the first
/// word.
static WIN_USER_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)((?:^|[^A-Za-z0-9_.~\-])(?:[a-z]:[/\\]{1,2}|\\{1,2}|/[a-z]/)users[/\\]{1,2})([^/\\:\r\n'"<>()\[\]]*? [^/\\:\r\n'"<>()\[\]]*?)([/\\])"#,
    )
    .unwrap()
});

/// One string through rules 2–5. Returns it and how many values were masked.
fn redact_text(s: &str, home: Option<&Regex>, computer: &[Regex]) -> (String, usize) {
    let mut n = 0;
    let mut out = s.to_string();
    // `with` says what a match becomes, or None to leave it as it is.
    let mut swap = |re: &Regex, out: &mut String, with: &dyn Fn(&regex::Captures) -> Option<String>| {
        if re.is_match(out) {
            let next = re.replace_all(out, |c: &regex::Captures| match with(c) {
                Some(s) => {
                    n += 1;
                    s
                }
                None => c[0].to_string(),
            });
            *out = next.into_owned();
        }
    };
    swap(&USERINFO, &mut out, &|c: &regex::Captures| Some(format!("{}{MASK}@", &c[1])));
    swap(&QUERY, &mut out, &|c: &regex::Captures| {
        let query: Vec<String> = c[2]
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) if !v.is_empty() && !v.starts_with('•') => format!("{k}={MASK}"),
                _ => kv.to_string(),
            })
            .collect();
        let query = query.join("&");
        (query != c[2]).then(|| format!("{}{query}", &c[1]))
    });
    swap(&JWT, &mut out, &|_: &regex::Captures| Some(MASK.to_string()));
    swap(&SCHEME, &mut out, &|c: &regex::Captures| Some(format!("{}{}{MASK}", &c[1], &c[2])));
    swap(&NAMED, &mut out, &|c: &regex::Captures| {
        let value = &c[3];
        // A quoted value keeps its quotes (and the backslashes before them).
        let (open, inner, close) = match value.find(['"', '\'']) {
            Some(i) if i <= 1 => {
                let q = value[i..].chars().next().unwrap();
                let end = value.rfind(q).filter(|&e| e > i).unwrap_or(value.len());
                let end = if value[..end].ends_with('\\') { end - 1 } else { end };
                (&value[..=i], &value[i + 1..end], &value[end..])
            }
            _ => ("", value, ""),
        };
        // A public key is published on purpose; an already-masked value stays.
        let leave = c[1].to_ascii_lowercase().contains("public") || inner.starts_with('•') || status_value(&c[1], inner);
        (!leave).then(|| format!("{}{}{open}{MASK}{close}", &c[1], &c[2]))
    });
    swap(&TOKEN, &mut out, &|_: &regex::Captures| Some(MASK.to_string()));
    swap(&OPAQUE, &mut out, &|c: &regex::Captures| opaque(&c[0]).then(|| MASK.to_string()));
    swap(&EMAIL, &mut out, &|_: &regex::Captures| Some(EMAIL_MASK.to_string()));
    if let Some(home) = home {
        if home.is_match(&out) {
            out = home
                .replace_all(&out, |c: &regex::Captures| {
                    // Keep the separator that followed, if one did.
                    let tail = c[0].chars().last().filter(|ch| *ch == '/' || *ch == '\\');
                    format!("~{}", tail.map(String::from).unwrap_or_default())
                })
                .into_owned();
        }
    }
    swap(&WIN_USER_DIR, &mut out, &|c: &regex::Captures| {
        (!c[2].starts_with('•')).then(|| format!("{}{MASK}{}", &c[1], &c[3]))
    });
    swap(&USER_DIR, &mut out, &|c: &regex::Captures| (!c[2].starts_with('•')).then(|| format!("{}{MASK}", &c[1])));
    for name in computer {
        swap(name, &mut out, &|_: &regex::Captures| Some(COMPUTER_MASK.to_string()));
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computer(names: &[&str]) -> Vec<Regex> {
        names.iter().map(|n| Regex::new(&format!(r"(?i)\b{}\b", regex::escape(n))).unwrap()).collect()
    }

    fn r(s: &str) -> String {
        let home = home_pattern("/Users/alice");
        redact_text(s, Some(&home), &computer(&["Alices-MacBook-Pro"])).0
    }

    #[test]
    fn tokens_by_shape_are_masked() {
        for secret in [
            "mb_9fK2LmQ81xPz0aQ7Rt",
            "s_Ab3dEf6hIj9kLm2nOp",
            "sk-ant-api03-AbCdEfGhIjKlMnOpQr",
            "ghp_0123456789abcdefABCDEF0123",
            "xoxb-123456789012-abcdefABCDEF",
            "AKIAIOSFODNN7EXAMPLE",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2lnbmF0dXJlX2hlcmU",
            // No prefix to know them by: a node's 64-hex serve token, a
            // standard-base64 key with `/` and `+` in it.
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "q8Zr+2bX/Lm4kP0aQ7RtYw9dE3fH6jN1sV5uC8oI2gA=",
        ] {
            let out = r(&format!("connecting with {secret} now"));
            assert!(!out.contains(secret), "{secret} survived: {out}");
            assert!(out.contains(MASK), "{out}");
        }
    }

    #[test]
    fn a_value_after_a_secret_name_or_a_scheme_is_masked() {
        let cases = [
            ("MAFOLD_BOT_TOKEN=mbXq81LmP0aZ7tY", "mbXq81LmP0aZ7tY"),
            (r#"{"api_key": "abcd1234efgh5678"}"#, "abcd1234efgh5678"),
            ("password: hunter2hunter2", "hunter2hunter2"),
            (r#""password": "hunter2""#, "hunter2"),
            (r#"{"secret": "correct horse battery"}"#, "horse battery"),
            (r#"body "{\"access_token\":\"ya29a0Short_tok\"}""#, "ya29a0Short_tok"),
            ("Authorization: Bearer abcdef0123456789xyz", "abcdef0123456789xyz"),
            ("GET https://bob:pa55w0rdLong@example.com/x", "pa55w0rdLong"),
            ("redis://:s3cretpass@cache:6379", "s3cretpass"),
            ("cookie=sessid8f7e6d5c4b3a", "sessid8f7e6d5c4b3a"),
        ];
        for (line, secret) in cases {
            let out = r(line);
            assert!(!out.contains(secret), "{secret} survived: {out}");
        }
        // A secret that happens to look like a status word or a count — all
        // lowercase, all digits — is still a secret.
        for (line, secret) in [
            ("DB_PASSWORD=correcthorsebatterystaple", "correcthorsebatterystaple"),
            ("password: letmein-please", "letmein-please"),
            ("api_key=82374982374982341234", "82374982374982341234"),
            (r#"{"secret": "48213977"}"#, "48213977"),
            ("access_token=abcdefghijklmnop", "abcdefghijklmnop"),
        ] {
            let out = r(line);
            assert!(!out.contains(secret), "{secret} survived: {out}");
        }
        // The name stays, so a reader knows WHAT was there.
        assert_eq!(r("MAFOLD_BOT_TOKEN=mbXq81LmP0aZ7tY"), format!("MAFOLD_BOT_TOKEN={MASK}"));
        assert_eq!(r(r#"body "{\"access_token\":\"ya29a0Short_tok\"}""#), format!(r#"body "{{\"access_token\":\"{MASK}\"}}""#));
    }

    #[test]
    fn a_links_query_keeps_its_names_and_loses_its_values() {
        let line = "self-update to v0.9.150 failed (error sending request for url (https://objects.githubusercontent.com/a/b?X-Amz-Credential=AKIA%2F20261022&X-Amz-Signature=3f9a1c&response-content-disposition=x)) — cooling down";
        let out = r(line);
        assert!(!out.contains("3f9a1c") && !out.contains("20261022"), "{out}");
        assert!(out.contains(&format!("?X-Amz-Credential={MASK}&X-Amz-Signature={MASK}&response-content-disposition={MASK})")), "{out}");
        assert_eq!(r("ws connect wss://api.mafold.com/ws?code="), "ws connect wss://api.mafold.com/ws?code=", "nothing to hide");
    }

    #[test]
    fn emails_are_masked_handles_are_not() {
        let out = r("signed in as alice.smith+mafold@example.co.uk; replying to @opsdu:claude-code");
        assert!(!out.contains("alice.smith"), "{out}");
        assert!(!out.contains("example.co.uk"), "{out}");
        assert!(out.contains(EMAIL_MASK), "{out}");
        assert!(out.contains("@opsdu:claude-code"), "{out}");
    }

    #[test]
    fn ordinary_lines_stay_as_they_are() {
        for line in [
            "supervisor up (pid 4242) · base https://api.mafold.com",
            "↻ waiting for in-flight turns before restart: opsdu_codex",
            "→ finalized reply for chat 72355ef4-c43f-44ba-a0d5-b2c061026cd6",
            "token_count=1234 tokens: 12 (public_key=AAAAC3NzaC1lZDI1NTE5AAAA)",
            "refresh token: invalid_grant · password: required · context_tokens=150000/200000",
            "account health: ok",
            "harness run failed: couldn't start `codex` (not on PATH)",
            "every_spawn_goes_through_platform_and_nothing_else holds",
            "mafold agent ✓ connected · workdir=/opt/work/claude/ac-c6/mafold-cli/target/debug/build",
            "GET /api/getChatHistory/72355ef4c43f44baa0d5b2c061026cd6 → 200",
            "← @bob (not authorized → access-request card send FAILED: unavailable: try later — will retry)",
        ] {
            let home = home_pattern("/Users/alice");
            assert_eq!(redact_text(line, Some(&home), &[]), (line.to_string(), 0), "{line}");
        }
    }

    #[test]
    fn the_home_directory_becomes_a_tilde_however_it_is_spelled() {
        assert_eq!(r("log: /Users/alice/.mafold/daemons/x/log"), "log: ~/.mafold/daemons/x/log");
        assert_eq!(r("in /users/ALICE"), "in ~");
        // Not a longer name that starts the same.
        assert_eq!(r("/Users/alicexyz/x"), format!("/Users/{MASK}/x"));
        let win = home_pattern(r"C:\Users\Alice");
        let (out, _) = redact_text(r"exe C:\Users\alice\AppData\Local\mafold.exe and c:/users/Alice/.mafold", Some(&win), &[]);
        assert_eq!(out, r"exe ~\AppData\Local\mafold.exe and ~/.mafold");
        // Somebody else's home: the name goes, the rest stays — however the
        // path begins.
        assert_eq!(r("copied from /home/bob/.claude"), format!("copied from /home/{MASK}/.claude"));
        assert_eq!(r("file:///Users/bob/x"), format!("file:///Users/{MASK}/x"));
        assert_eq!(r("[/home/bob/.claude,/Users/carol]"), format!("[/home/{MASK}/.claude,/Users/{MASK}]"));
        // Windows: a name with a space, whole; a path `{:?}` printed with its
        // backslashes doubled; Git Bash's spelling.
        assert_eq!(r(r"C:\Users\John Smith\.mafold\x"), format!(r"C:\Users\{MASK}\.mafold\x"));
        assert_eq!(r("/c/Users/John Smith/.mafold"), format!("/c/Users/{MASK}/.mafold"));
        assert_eq!(r("/c/Users/bob/x"), format!("/c/Users/{MASK}/x"));
        assert_eq!(r(r#"path: "C:\\Users\\bob\\AppData""#), format!(r#"path: "C:\\Users\\{MASK}\\AppData""#));
        let (out, _) = redact_text(r#"exe "C:\\Users\\Alice\\AppData""#, Some(&win), &[]);
        assert_eq!(out, r#"exe "~\\AppData""#);
        // A space after a unix home ends the name: the words after it stay.
        assert_eq!(r("copied from /home/bob to /tmp"), format!("copied from /home/{MASK} to /tmp"));
    }

    #[test]
    fn this_computers_name_is_masked_and_a_common_word_is_not_a_name() {
        assert_eq!(r("bound Alices-MacBook-Pro → this machine"), format!("bound {COMPUTER_MASK} → this machine"));
        assert_eq!(r("ssh alices-macbook-pro.local"), format!("ssh {COMPUTER_MASK}.local"));
        // A name made of path words doesn't break the path rules before it.
        let mut v = json!(["/home/bob/.mafold/daemons/x/log", "http://localhost:4100", "~/.mafold/"]);
        let known = Known { credentials: vec![], home: None, computer: vec!["home".into(), "localhost".into(), "mafold".into()] };
        redact(&mut v, &known);
        assert_eq!(v, json!([format!("/home/{MASK}/.mafold/daemons/x/log"), "http://localhost:4100", "~/.mafold/"]));
    }

    #[test]
    fn a_quote_keeps_the_lines_words_and_its_length() {
        let line = crate::logfile::said("bob", "my card is 4111 1111 1111 1111, call me");
        assert_eq!(without_quotes(&line), "← @bob ⟫ [39 chars]");
        let stderr = crate::logfile::quoting("[cc-pool] hook handshake failed after 9ms (x) rid=3 — claude stderr", "MCP server said: here is the file");
        assert_eq!(without_quotes(&stderr), "[cc-pool] hook handshake failed after 9ms (x) rid=3 — claude stderr ⟫ [33 chars]");
        // Inside an error chain: what the runtime said goes, what we said stays.
        assert_eq!(
            without_quotes("handle error: turn failed: codex exited unsuccessfully: reading /secret/plan.md"),
            "handle error: turn failed: codex exited unsuccessfully: [23 chars]"
        );
        // Whichever comes first: what a runtime printed may hold ` ⟫ `, a
        // quote may hold «exited unsuccessfully: ».
        assert_eq!(
            without_quotes("harness run failed: codex exited unsuccessfully: reading /secret/plan.md ⟫ x"),
            "harness run failed: codex exited unsuccessfully: [27 chars]"
        );
        assert_eq!(without_quotes("← @bob ⟫ it exited unsuccessfully: my plan"), "← @bob ⟫ [33 chars]");
        // What a bot says in its own words, even starting like a message.
        for line in ["mafold agent ✓ connected as @bot", "← @bob (not authorized → ignored: not on the list)", "← @bob: [redacted, 6 chars]"] {
            assert_eq!(without_quotes(line), line);
        }
    }

    /// The whole path, as `run` takes it: a bundle that holds this machine's
    /// token (by value, and base64'd), a stranger's key, an email and a
    /// message comes out with none of them — and with what a helper needs.
    #[test]
    fn a_bundle_comes_out_without_secrets_emails_or_messages() {
        let mine = "mb_LocalToken_9a8b7c6d5e4f";
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD_NO_PAD, mine);
        let raw_lines = [
            "mafold agent ✓ connected as @helper".to_string(),
            format!("retrying with {mine}"),
            format!("header {b64}"),
            "OPENAI_API_KEY=sk-proj-AbCdEf0123456789XyZ".to_string(),
            "notify alice@example.com".to_string(),
            crate::logfile::said("alice", "here is my secret plan"),
        ];
        let mut v = json!({
            "status": { "supervisor": { "log": "/Users/alice/.mafold/supervisor.log" } },
            "config": { "daemons": [{ "name": "helper", "env": ["OPENAI_API_KEY"], "token": "3f2a…" }] },
            "logs": { "bots": { "helper": { "lines": raw_lines.iter().map(|l| without_quotes(l)).collect::<Vec<_>>() } } },
        });
        let known = Known { credentials: vec![mine.to_string()], home: Some("/Users/alice".into()), computer: vec![] };
        let n = redact(&mut v, &known);
        let text = v.to_string();
        for gone in [mine, b64.as_str(), "sk-proj-AbCdEf0123456789XyZ", "alice@example.com", "secret plan", "/Users/alice"] {
            assert!(!text.contains(gone), "{gone} survived: {text}");
        }
        assert!(n >= 4, "{n}");
        for kept in ["connected as @helper", "OPENAI_API_KEY", "~/.mafold/supervisor.log", "← @alice ⟫ [22 chars]"] {
            assert!(text.contains(kept), "{kept} lost: {text}");
        }
    }

    #[test]
    fn the_default_name_is_dated_in_utc() {
        assert_eq!(default_name(1_792_400_102), "mafold-diagnose-20261019-085502.json");
    }
}
