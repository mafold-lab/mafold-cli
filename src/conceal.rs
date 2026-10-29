//! `mafold connection run`: a connection's secret goes INTO a command, and
//! never comes back OUT of it.
//!
//! In a bot's turn everything a command prints is posted to the conversation
//! — the trace card is the tool's output, verbatim, for everyone in the room.
//! `mafold connection env` printed `export NOTION_TOKEN=…` there (2026-10-05:
//! live, in a group of two dozen). So the way an agent feeds a credential to a
//! vendor's CLI or REST API is now this: the value rides in the child's
//! environment, and the child's stdout/stderr pass through a [`Concealer`]
//! that replaces every spelling of it with the mask before a byte reaches the
//! agent — the same contract as 1Password's `op run`.
//!
//! What this does not stop, said plainly: a command that transforms the value
//! (reverses it, splits it into characters) before printing it. The server's
//! context scrub (`mafold-api/src/secrets.rs`) is the second net; neither is a
//! sandbox.

use std::io::{Read, Write};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use base64::Engine as _;

/// The same mask the api writes, so a hidden value reads the same everywhere.
pub const MASK: &str = "••••••••";

/// Shorter than this, a value would mask ordinary output (every `1` in a
/// listing) — it is still injected, just not masked, and `run` says so.
pub const MIN_MASKED: usize = 4;

/// Masks a fixed set of byte strings out of a stream fed to it in arbitrary
/// chunks — a value split across two reads is still caught.
pub struct Concealer {
    /// Every spelling to hide, longest first so the longest wins at a position.
    needles: Vec<Vec<u8>>,
    /// The tail of the last chunk that might be the start of a needle.
    carry: Vec<u8>,
    /// How many occurrences were masked — reported once the command exits.
    pub hidden: usize,
}

/// The ways a value turns up in output besides itself: base64 (an HTTP Basic
/// header, a k8s secret), and percent-encoded (a URL, a form body).
fn spellings(value: &str) -> Vec<Vec<u8>> {
    let raw = value.as_bytes();
    let mut out = vec![raw.to_vec()];
    for b64 in [
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(raw),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw),
    ] {
        out.push(b64.into_bytes());
    }
    let mut pct = String::new();
    for &b in raw {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            pct.push(b as char);
        } else {
            pct.push_str(&format!("%{b:02X}"));
        }
    }
    out.push(pct.into_bytes());
    out
}

impl Concealer {
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut needles: Vec<Vec<u8>> = values
            .into_iter()
            .filter(|v| v.len() >= MIN_MASKED)
            .flat_map(spellings)
            .collect();
        needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        needles.dedup();
        Self { needles, carry: Vec::new(), hidden: 0 }
    }

    fn match_at(&self, at: &[u8]) -> Option<usize> {
        self.needles.iter().find(|n| at.starts_with(n)).map(|n| n.len())
    }

    /// Whether `rest` — everything from here to the end of what we hold — is
    /// the beginning of a needle, so the next chunk could complete it.
    fn could_continue(&self, rest: &[u8]) -> bool {
        self.needles.iter().any(|n| rest.len() < n.len() && n.starts_with(rest))
    }

    fn process(&mut self, at_eof: bool) -> Vec<u8> {
        let buf = std::mem::take(&mut self.carry);
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if let Some(n) = self.match_at(&buf[i..]) {
                out.extend_from_slice(MASK.as_bytes());
                self.hidden += 1;
                i += n;
                continue;
            }
            if !at_eof && self.could_continue(&buf[i..]) {
                break;
            }
            out.push(buf[i]);
            i += 1;
        }
        self.carry = buf[i..].to_vec();
        out
    }

    /// Feed a chunk; get back what is safe to pass on now.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(chunk);
        self.process(false)
    }

    /// The stream ended: whatever was held back can't become a match any more.
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(true)
    }
}

/// Copy `from` to `to` through a concealer; returns how many it masked.
fn pump(mut from: impl Read, mut to: impl Write, values: &[String]) -> usize {
    let mut c = Concealer::new(values.iter().map(String::as_str));
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let out = c.feed(&buf[..n]);
                if to.write_all(&out).and_then(|_| to.flush()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = to.write_all(&c.finish());
    let _ = to.flush();
    c.hidden
}

/// Run `command` with `vars` added to its environment and its output masked.
/// Returns the exit code to leave with (128 + signal when it was killed, the
/// shell's convention).
pub fn run(command: &[String], vars: &[(String, String)]) -> Result<i32> {
    let Some((program, args)) = command.split_first() else {
        bail!("nothing to run — `mafold connection run <name> -- <command> [args…]`");
    };
    let values: Vec<String> = vars.iter().map(|(_, v)| v.clone()).collect();
    for (name, v) in vars {
        if v.len() < MIN_MASKED {
            eprintln!(
                "mafold: `{name}` is only {} characters — too short to mask without garbling \
                 the output, so it is injected but NOT masked. Don't print it.",
                v.len()
            );
        }
    }
    let mut child = crate::platform::console_std_command(program)
        .args(args)
        .envs(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("couldn't start `{program}`"))?;
    let out = child.stdout.take().expect("piped");
    let err = child.stderr.take().expect("piped");
    let (v1, v2) = (values.clone(), values);
    let t_out = std::thread::spawn(move || pump(out, std::io::stdout(), &v1));
    let t_err = std::thread::spawn(move || pump(err, std::io::stderr(), &v2));
    let status = child.wait().context("waiting for the command")?;
    let hidden = t_out.join().unwrap_or(0) + t_err.join().unwrap_or(0);
    if hidden > 0 {
        eprintln!("mafold: masked {hidden} occurrence(s) of a connection secret in this output");
    }
    #[cfg(unix)]
    if let Some(sig) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return Ok(128 + sig);
    }
    Ok(status.code().unwrap_or(1))
}

// ── What this machine holds never leaves it in something people read ──────
//
// `run` above keeps ONE connection's value out of ONE command's output. The
// leak it does not cover is the everyday one: an agent runs `env`, `cat`s a
// config, or curls with a bearer header, and the output — its own bot token
// included, since the daemon puts it in the turn's environment — becomes the
// trace card in a group of two dozen. 2026-10-05/06: three times in one night,
// each time behind a hand-written `grep | sed` filter that guessed the secret's
// shape and guessed wrong. Asking the producer to be careful has been tried.
//
// So the client takes it out on the way OUT, in the one place every write to
// a conversation passes (`Client::post`): before a draft snapshot, a delta, a
// `mafold send`, an alert or a room change leaves this machine, every
// credential the machine itself holds is replaced by the mask — by VALUE, so a
// token with no recognisable shape and no telling name beside it is caught
// just the same. All three harnesses stream their drafts through that call,
// and so does every agent's `mafold send`. The server's shape-and-context
// scrub (`mafold-api/src/secrets.rs`) stays as the second net; it can only
// recognise what announces itself, this only knows what is on this machine.
//
// What it cannot reach, plainly: the agent's own context. The harness reads a
// command's output before the daemon does, so the model has still seen it —
// this keeps it out of the chat, not out of the turn.

/// The RPCs whose bodies become something people read. Everything else — a
/// login, a vault blob, a connection deposit, a site upload — goes out exactly
/// as the caller built it: some of those carry a credential on purpose.
const VISIBLE_WRITES: &[&str] = &[
    "sendMessage",
    "editMessage",
    "botEditDraft",
    "botAppendDelta",
    "pushAlert",
    "answerInlineQuery",
    "answerCardAction",
];

/// A gathered value shorter than this is not treated as a credential: a
/// heuristic sweep (any field NAMED like a secret) would otherwise mask ordinary
/// words. Every credential this platform mints is far longer (`mb_` + 32 hex).
const MIN_LOCAL: usize = 12;

/// How long one sweep of the credential files is trusted. Short, because a
/// token is rotated under a running daemon (`take_rotation`) and the new one
/// must be masked from its first appearance; cheap enough, because it is a
/// handful of small files read at most this often, not per draft push — plus,
/// on a Mac, one `security` probe and one item read per Claude Code login
/// (~10ms each, run in line on the write that finds the sweep stale).
const RESWEEP: std::time::Duration = std::time::Duration::from_secs(5);

/// Mask every credential this machine holds out of `body`, if `method` writes
/// something people read. `own` is the token the client is calling with (a
/// `--token` that lives in no file is still this machine's). Returns how many
/// were masked.
pub fn scrub_outgoing(method: &str, body: &mut serde_json::Value, own: &str) -> usize {
    if !VISIBLE_WRITES.contains(&method) {
        return 0;
    }
    let mut values = local_credentials();
    if own.len() >= MIN_LOCAL {
        values.push(own.to_string());
    }
    scrub_value(body, &values)
}

/// Every string in `v`, in place — `mafold diagnose` runs its whole bundle
/// through this with [`local_credentials`].
pub(crate) fn scrub_with(v: &mut serde_json::Value, values: &[String]) -> usize {
    scrub_value(v, values)
}

/// Every string in `v`, in place.
fn scrub_value(v: &mut serde_json::Value, values: &[String]) -> usize {
    let mut concealer = Concealer::new(values.iter().map(String::as_str));
    fn walk(v: &mut serde_json::Value, c: &mut Concealer) {
        match v {
            serde_json::Value::String(s) => {
                if let Some(masked) = c.conceal_str(s) {
                    *s = masked;
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|i| walk(i, c)),
            serde_json::Value::Object(map) => map.values_mut().for_each(|i| walk(i, c)),
            _ => {}
        }
    }
    walk(v, &mut concealer);
    concealer.hidden
}

impl Concealer {
    /// A whole string at once rather than a stream. `None` when nothing in it
    /// is hidden — the common case, settled by one substring search per
    /// spelling instead of the byte-by-byte walk (a draft is pushed every few
    /// hundred milliseconds and can run to tens of kilobytes).
    fn conceal_str(&mut self, text: &str) -> Option<String> {
        let present = self
            .needles
            .iter()
            .any(|n| std::str::from_utf8(n).is_ok_and(|n| text.contains(n)));
        if !present {
            return None;
        }
        self.carry.clear();
        let mut out = self.feed(text.as_bytes());
        out.extend(self.finish());
        String::from_utf8(out).ok()
    }
}

/// The last sweep and when it was taken.
static SWEPT: std::sync::Mutex<Option<(std::time::Instant, Vec<String>)>> = std::sync::Mutex::new(None);

/// This machine's credentials, from a sweep at most [`RESWEEP`] old.
pub(crate) fn local_credentials() -> Vec<String> {
    use std::time::Instant;
    let mut cache = SWEPT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, values)) = cache.as_ref() {
        if at.elapsed() < RESWEEP {
            return values.clone();
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let env: Vec<(String, String)> = std::env::vars().collect();
    // Never the real Keychain under test: a test's failure message prints
    // what it scrubbed, and this is the path every client test's write takes.
    let keychain = if cfg!(test) { None } else { crate::accounts::keychain() };
    let mut seen = KEYCHAIN_SEEN.lock().unwrap_or_else(|p| p.into_inner());
    let values = sweep_with(&home, &env, keychain, seen.get_or_insert_with(Default::default));
    *cache = Some((Instant::now(), values.clone()));
    values
}

/// What each Keychain item said last, kept across sweeps (see [`sweep_with`]).
static KEYCHAIN_SEEN: std::sync::Mutex<Option<KeychainSeen>> = std::sync::Mutex::new(None);

/// Start the next call from a fresh sweep (a test that just planted a value).
#[cfg(test)]
pub(crate) fn forget_sweep() {
    *SWEPT.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// The last thing each Keychain item said, by service.
type KeychainSeen = std::collections::HashMap<String, String>;

/// Every credential under `home` and in `env`: the bot tokens the daemons on
/// this machine run with, the people signed in here, the app secrets, this
/// device's vault key, and each harness's own login — Claude Code (wherever
/// [`crate::accounts::LoginStore`] says: the Keychain on a Mac, with the MCP
/// grants filed beside it), Codex and Kimi, for this process and for every
/// daemon pinned elsewhere with `--env` — plus any environment variable
/// named like a secret.
///
/// `keychain` is handed in so tests never read the real one into a list their
/// failure messages print. `seen` is what each Keychain item said last: when
/// the Keychain stops answering mid-session (it locked, a read timed out) its
/// tokens stay on the list — they are as live as they were a sweep ago — and
/// only "not there", a sign-out, takes them off.
fn sweep_with(
    home: &std::path::Path,
    env: &[(String, String)],
    keychain: Option<crate::accounts::Keychain>,
    seen: &mut KeychainSeen,
) -> Vec<String> {
    use crate::accounts::{LoginStore, CONFIG_DIR_ENV, ENV as SECURESTORAGE_ENV};
    use std::path::PathBuf;
    let daemons = read_json(&home.join(".mafold/daemons.json"));
    let mut json_files: Vec<PathBuf> = [".mafold/session.json", ".mafold/app-secrets.json", ".mafold/device_key.json"]
        .iter()
        .map(|f| home.join(f))
        .collect();

    // The environments credentials are found through: this process's, and
    // each daemon's `--env` pins over it — what that daemon actually runs
    // with, and so whose output this sweep is masking.
    let mut envs: Vec<Vec<(String, String)>> = vec![env.to_vec()];
    for daemon in daemons.as_ref().and_then(|d| d["daemons"].as_array()).into_iter().flatten() {
        if let Some(pins) = daemon["env"].as_object().filter(|p| !p.is_empty()) {
            let mut pinned = env.to_vec();
            pinned.extend(pins.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))));
            envs.push(pinned);
        }
    }
    // Every Claude Code login this machine switches between: `~/.claude`'s,
    // the one each environment points at (a pinned seat, a
    // `CLAUDE_CONFIG_DIR`), and each registered seat (`accounts.rs`). Each is
    // read in BOTH its places — a leftover file's grant may still be live.
    let mut claude = vec![LoginStore::default_in(home)];
    let (mut codex_homes, mut kimi_homes) = (Vec::new(), Vec::new());
    for e in &envs {
        // The last value wins, as it does for the process the pins build.
        let var = |name: &str| e.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
        claude.push(LoginStore::resolve(home, var(SECURESTORAGE_ENV).as_deref(), var(CONFIG_DIR_ENV).as_deref()));
        codex_homes.push(var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex")));
        kimi_homes.push(var("KIMI_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".kimi")));
    }
    if let Some(registry) = read_json(&home.join(".mafold/claude-accounts.json")) {
        for account in registry["accounts"].as_array().into_iter().flatten() {
            if let Some(dir) = account["dir"].as_str() {
                claude.push(LoginStore::resolve(home, Some(dir), None));
            }
        }
    }
    // Each place asked once: a Keychain read is a process spawn.
    distinct(&mut claude);
    distinct(&mut codex_homes);
    distinct(&mut kimi_homes);
    json_files.extend(codex_homes.iter().map(|c| c.join("auth.json")));
    for kimi in &kimi_homes {
        if let Ok(dir) = std::fs::read_dir(kimi.join("credentials")) {
            json_files.extend(dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")));
        }
    }

    let mut found = Vec::new();
    if let Some(v) = &daemons {
        collect_named(v, false, &mut found);
    }
    for file in &json_files {
        if let Some(v) = read_json(file) {
            collect_named(&v, false, &mut found);
        }
    }
    for login in &claude {
        if let Some(read) = keychain {
            // A failure — locked (one probe settles that for every item),
            // or this item alone — leaves what it said last standing.
            match read(&login.keychain_service) {
                Ok(Some(blob)) => {
                    seen.insert(login.keychain_service.clone(), blob);
                }
                Ok(None) => {
                    seen.remove(&login.keychain_service);
                }
                Err(_) => {}
            }
        }
        if let Some(v) = seen.get(&login.keychain_service).and_then(|blob| serde_json::from_str(blob).ok()) {
            collect_named(&v, false, &mut found);
        }
        if let Some(v) = read_json(&login.file) {
            collect_named(&v, false, &mut found);
        }
    }
    // `key = "value"` lines whose key is named like a secret (Kimi's provider
    // keys) — enough TOML for that, without parsing the rest of the file.
    for kimi in &kimi_homes {
        if let Ok(text) = std::fs::read_to_string(kimi.join("config.toml")) {
            for line in text.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    if secret_name(key.trim()) {
                        found.push(value.trim().trim_matches(|c| c == '"' || c == '\'').to_string());
                    }
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(home.join(".mafold/cards-publisher.token")) {
        found.push(text.trim().to_string());
    }
    for (name, value) in env {
        if secret_name(name) {
            found.push(value.clone());
        }
    }
    found.retain(|v| plausible_credential(v));
    found.sort();
    found.dedup();
    found
}

/// Drop repeats, keeping first occurrences in order.
fn distinct<T: PartialEq + Clone>(items: &mut Vec<T>) {
    let mut kept: Vec<T> = Vec::with_capacity(items.len());
    items.retain(|i| !kept.contains(i) && {
        kept.push(i.clone());
        true
    });
}

fn read_json(path: &std::path::Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Every string under a key named like a secret — and everything beneath such
/// a key (`"tokens": {"access_token": …, "refresh_token": …}`).
fn collect_named(v: &serde_json::Value, inside: bool, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) if inside => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|i| collect_named(i, inside, out)),
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                collect_named(value, inside || secret_name(key), out);
            }
        }
        _ => {}
    }
}

/// Is this key or variable named like a credential? `accessToken`,
/// `refresh_token`, `MAFOLD_BOT_TOKEN`, `secret`, `OPENAI_API_KEY`… — but not a
/// public key, which is published on purpose.
fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    !n.contains("PUBLIC")
        && ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "APIKEY", "PRIVATE_KEY", "ACCESS_KEY", "CREDENTIAL"]
            .iter()
            .any(|w| n.contains(w))
}

/// Long enough to be one, and not something a secret-sounding NAME often
/// holds instead: a path (`…_TOKEN_FILE=/run/…`) or a sentence.
fn plausible_credential(v: &str) -> bool {
    let b = v.as_bytes();
    v.len() >= MIN_LOCAL
        && !v.starts_with('/')
        && !v.starts_with('~')
        && !(b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        && !v.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(c: &mut Concealer, chunks: &[&[u8]]) -> String {
        let mut out = Vec::new();
        for ch in chunks {
            out.extend(c.feed(ch));
        }
        out.extend(c.finish());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_value_is_masked_even_when_split_across_reads() {
        let secret = "ntn_Zq81mLr0Pp2xV7cT";
        let text = format!("export NOTION_TOKEN={secret}\nok\n");
        for split in 1..text.len() {
            let (a, b) = text.as_bytes().split_at(split);
            let mut c = Concealer::new([secret]);
            let out = all(&mut c, &[a, b]);
            assert_eq!(out, format!("export NOTION_TOKEN={MASK}\nok\n"), "split at {split}");
            assert_eq!(c.hidden, 1);
        }
    }

    #[test]
    fn its_base64_and_url_spellings_are_masked_too() {
        let secret = "p@ss:w0rd/42";
        let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(secret);
        let text = format!("Authorization: Basic {b64}==\n?pw=p%40ss%3Aw0rd%2F42&x=1\n");
        let mut c = Concealer::new([secret]);
        let out = all(&mut c, &[text.as_bytes()]);
        assert!(!out.contains(&b64) && !out.contains("p%40ss"), "{out}");
        assert_eq!(c.hidden, 2);
    }

    #[test]
    fn output_without_the_value_passes_through_byte_for_byte() {
        let text = "日志 ✓ 200 OK\n{\"results\":[]}\n\u{1b}[32mgreen\u{1b}[0m";
        let mut c = Concealer::new(["s3cr3t-value"]);
        // A chunk ending in what could be the start of the value is held back,
        // then released untouched once it turns out not to be.
        let out = all(&mut c, &[b"prefix s3cr3", b"T and more\n", text.as_bytes()]);
        assert_eq!(out, format!("prefix s3cr3T and more\n{text}"));
        assert_eq!(c.hidden, 0);
    }

    #[test]
    fn several_values_and_a_too_short_one() {
        let mut c = Concealer::new(["alpha-1234", "beta-56789", "xyz"]);
        let out = all(&mut c, &[b"a=alpha-1234 b=beta-56789 c=xyz"]);
        assert_eq!(out, format!("a={MASK} b={MASK} c=xyz"), "3 chars is not masked (and run warns)");
    }

    #[test]
    fn run_returns_the_childs_exit_code() {
        // The real path end to end: a child that prints its injected value.
        let vars = vec![("DEMO_TOKEN".to_string(), "tok_5f3a9c2e7b1d".to_string())];
        let cmd: Vec<String> = ["sh", "-c", "exit 3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(run(&cmd, &vars).unwrap(), 3, "the child's exit code comes back");
    }

    #[test]
    fn pump_masks_a_real_pipe() {
        let child = crate::platform::std_command("sh")
            .args(["-c", "printf 'key=%s\\n' \"$DEMO_TOKEN\"; printf 'err %s' \"$DEMO_TOKEN\" >&2"])
            .env("DEMO_TOKEN", "tok_5f3a9c2e7b1d")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let values = vec!["tok_5f3a9c2e7b1d".to_string()];
        let mut out = Vec::new();
        let n = pump(child.stdout.unwrap(), &mut out, &values);
        let mut err = Vec::new();
        let m = pump(child.stderr.unwrap(), &mut err, &values);
        assert_eq!(String::from_utf8(out).unwrap(), format!("key={MASK}\n"));
        assert_eq!(String::from_utf8(err).unwrap(), format!("err {MASK}"));
        assert_eq!((n, m), (1, 1));
    }

    // ── outgoing writes ─────────────────────────────────────────────────────

    /// A throwaway HOME laid out like a real machine's.
    fn fake_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("mafold-conceal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn put(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_sweep_finds_every_credential_this_machine_holds_and_nothing_else() {
        let home = fake_home("sweep");
        let codex = home.join("codex-home");
        let second_claude = home.join("claude-two");
        put(
            &home.join(".mafold/daemons.json"),
            r#"{"daemons":[
                {"name":"opsdu:claude-code","harness":"claude-code","workdir":"/Users/x/work","token":"mb_0000000000000000000000000000a001"},
                {"name":"opsdu:codex","harness":"codex","workdir":"/Users/x/work","token":"mb_0000000000000000000000000000a002"}]}"#,
        );
        put(
            &home.join(".mafold/session.json"),
            r#"{"username":"opsdu","token":"s_session_value_0001","device_id":"dev-0001",
                "accounts":[{"username":"opsdu","token":"s_session_value_0001"},{"username":"mafold","token":"s_session_value_0002"}]}"#,
        );
        put(
            &home.join(".mafold/app-secrets.json"),
            r#"{"mafold/linear":{"secret":"appsecret_linear_0001","url":"https://linear.example/hook","saved_at":1}}"#,
        );
        put(&home.join(".mafold/device_key.json"), r#"{"public":"device_public_key_0001","secret":"device_private_key_0001"}"#);
        put(
            &home.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-default-0001","refreshToken":"sk-ant-ort01-default-0001","expiresAt":1}}"#,
        );
        put(
            &home.join(".mafold/claude-accounts.json"),
            &serde_json::json!({"accounts": [{"name": "two", "dir": second_claude, "email": "two@example.com"}]}).to_string(),
        );
        put(&second_claude.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-second-0001"}}"#);
        put(
            &codex.join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"codex_access_0001","refresh_token":"codex_refresh_0001","id_token":"eyJhbGciOi.codex.id0001"}}"#,
        );
        put(&home.join(".kimi/credentials/kimi-code.json"), r#"{"access_token":"kimi_access_token_0001"}"#);
        put(
            &home.join(".kimi/config.toml"),
            "[providers.moonshot]\napi_key = \"sk-kimi-provider-0001\"\nbase_url = \"https://api.moonshot.example\"\n",
        );
        put(&home.join(".mafold/cards-publisher.token"), "mb_0000000000000000000000000000c003\n");
        let env: Vec<(String, String)> = [
            ("MAFOLD_BOT_TOKEN", "mb_0badc0de0badc0de0badc0de0badc0d"),
            ("CODEX_HOME", codex.to_str().unwrap()),
            ("OPENAI_API_KEY", "sk-proj-from-env-0001"),
            ("GH_TOKEN_FILE", "/run/secrets/gh-token"),
            ("MAFOLD_CONV", "72355ef4-c43f-44ba-a0d5-b2c061026cd6"),
            ("SHORT_TOKEN", "abc123"),
            ("SESSION_TOKEN_NOTE", "not a token at all"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let found = sweep_with(&home, &env, None, &mut KeychainSeen::new());
        for want in [
            "mb_0000000000000000000000000000a001", // every bot on this machine
            "mb_0000000000000000000000000000a002",
            "s_session_value_0001", // every person signed in here
            "s_session_value_0002",
            "appsecret_linear_0001",
            "device_private_key_0001",
            "sk-ant-oat01-default-0001", // Claude Code, default login
            "sk-ant-ort01-default-0001",
            "sk-ant-oat01-second-0001", // …and a second one via the registry
            "codex_access_0001", // Codex, under $CODEX_HOME
            "codex_refresh_0001",
            "eyJhbGciOi.codex.id0001",
            "kimi_access_token_0001", // Kimi Code
            "sk-kimi-provider-0001",
            "mb_0000000000000000000000000000c003",
            "mb_0badc0de0badc0de0badc0de0badc0d", // the turn's own token, from env
            "sk-proj-from-env-0001",
        ] {
            assert!(found.iter().any(|f| f == want), "missed {want}: {found:?}");
        }
        for not in [
            "device_public_key_0001",
            "/run/secrets/gh-token",
            "72355ef4-c43f-44ba-a0d5-b2c061026cd6",
            "abc123",
            "not a token at all",
            "opsdu",
            "https://linear.example/hook",
            "/Users/x/work",
            "two@example.com",
            "https://api.moonshot.example",
        ] {
            assert!(!found.iter().any(|f| f == not), "{not} is not a credential: {found:?}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// On a Mac Claude Code's logins are Keychain items and there is no file
    /// to sweep, so its tokens — and the MCP grants filed beside them — were
    /// never on the list: a bot printing one posted it to the room unmasked.
    /// Every login the machine switches between counts: `~/.claude`'s and each
    /// registered seat's (whose item is named after its directory).
    #[test]
    fn claude_logins_held_in_the_keychain_are_on_the_list() {
        let home = fake_home("keychain");
        let seat = home.join("claude-seat");
        put(
            &home.join(".mafold/claude-accounts.json"),
            &serde_json::json!({"accounts": [{"name": "seat", "dir": seat}]}).to_string(),
        );
        let seat_item = format!("Claude Code-credentials-{}", crate::accounts::hash8(seat.to_str().unwrap()));
        let keychain = move |service: &str| {
            Ok(match service {
                "Claude Code-credentials" => Some(
                    serde_json::json!({
                        "mcpOAuth": {"linear|abc": {"accessToken": "mcp_linear_access_0001", "clientId": "client-id-not-secret"}},
                        "claudeAiOauth": {"accessToken": "sk-ant-oat01-keychain-0001", "refreshToken": "sk-ant-ort01-keychain-0001"},
                    })
                    .to_string(),
                ),
                s if s == seat_item => Some(r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-seat-kc-0001"}}"#.to_string()),
                _ => None,
            })
        };
        let found = sweep_with(&home, &[], Some(&keychain), &mut KeychainSeen::new());
        for want in [
            "sk-ant-oat01-keychain-0001",
            "sk-ant-ort01-keychain-0001",
            "mcp_linear_access_0001",
            "sk-ant-oat01-seat-kc-0001",
        ] {
            assert!(found.iter().any(|f| f == want), "missed {want}: {found:?}");
        }
        assert!(!found.iter().any(|f| f == "client-id-not-secret"), "{found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `CLAUDE_CONFIG_DIR` puts the login somewhere else — its file inside
    /// that directory, its Keychain item named after it. Both are swept, and so
    /// is `~/.claude`'s: the same person runs `claude` with and without it.
    #[test]
    fn a_login_moved_by_claude_config_dir_is_on_the_list() {
        let home = fake_home("config-dir");
        let config = home.join("claude-elsewhere");
        put(&config.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-cfg-file-0001"}}"#);
        put(&home.join(".claude/.credentials.json"), r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-plain-0001"}}"#);
        let config = config.to_str().unwrap().to_string();
        let item = format!("Claude Code-credentials-{}", crate::accounts::hash8(&config));
        let keychain = move |service: &str| {
            Ok((service == item).then(|| r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-cfg-kc-0001"}}"#.to_string()))
        };
        let env = vec![("CLAUDE_CONFIG_DIR".to_string(), config.clone())];
        let found = sweep_with(&home, &env, Some(&keychain), &mut KeychainSeen::new());
        for want in ["sk-ant-oat01-cfg-file-0001", "sk-ant-oat01-cfg-kc-0001", "sk-ant-oat01-plain-0001"] {
            assert!(found.iter().any(|f| f == want), "missed {want}: {found:?}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A daemon pinned to its own login with `mafold add --env` (a seat, a
    /// `CLAUDE_CONFIG_DIR`, a `CODEX_HOME`) runs on credentials nothing else
    /// points at — and prints its turns through this same sweep.
    #[test]
    fn a_login_a_daemon_is_pinned_to_is_on_the_list() {
        let home = fake_home("pinned");
        let config = home.join("pinned-claude");
        let codex = home.join("pinned-codex");
        put(&config.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-pinned-file-0001"}}"#);
        put(&codex.join("auth.json"), r#"{"tokens":{"access_token":"codex_pinned_access_0001"}}"#);
        put(
            &home.join(".mafold/daemons.json"),
            &serde_json::json!({"daemons": [{
                "name": "opsdu:pinned", "harness": "claude-code", "workdir": "/w", "token": "mb_0000000000000000000000000000d001",
                "env": {"CLAUDE_CONFIG_DIR": config, "CODEX_HOME": codex},
            }]})
            .to_string(),
        );
        let config = config.to_str().unwrap().to_string();
        let item = format!("Claude Code-credentials-{}", crate::accounts::hash8(&config));
        let keychain = move |service: &str| {
            Ok((service == item).then(|| r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-pinned-kc-0001"}}"#.to_string()))
        };
        let found = sweep_with(&home, &[], Some(&keychain), &mut KeychainSeen::new());
        for want in ["sk-ant-oat01-pinned-file-0001", "sk-ant-oat01-pinned-kc-0001", "codex_pinned_access_0001"] {
            assert!(found.iter().any(|f| f == want), "missed {want}: {found:?}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A Keychain that stops answering (it locked, a read timed out) doesn't
    /// take its tokens off the list: they are as live as they were a sweep
    /// ago. Only "not there" — signed out — drops them.
    #[test]
    fn a_keychain_that_stops_answering_keeps_what_it_said_last() {
        let home = fake_home("seen");
        let mut seen = KeychainSeen::new();
        let says = |s: &'static str| {
            move |_: &str| -> Result<Option<String>, String> { Ok(Some(format!(r#"{{"claudeAiOauth":{{"accessToken":"{s}"}}}}"#))) }
        };
        let locked = |_: &str| Err("the login Keychain is locked".to_string());
        let gone = |_: &str| Ok(None);

        assert!(sweep_with(&home, &[], Some(&says("sk-ant-oat01-was-live-0001")), &mut seen).iter().any(|f| f == "sk-ant-oat01-was-live-0001"));
        let found = sweep_with(&home, &[], Some(&locked), &mut seen);
        assert!(found.iter().any(|f| f == "sk-ant-oat01-was-live-0001"), "dropped while locked: {found:?}");
        let found = sweep_with(&home, &[], Some(&gone), &mut seen);
        assert!(!found.iter().any(|f| f == "sk-ant-oat01-was-live-0001"), "kept after sign-out: {found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// One item that can't be read (an ACL another binary wrote, a read that
    /// timed out) is that item's problem: every other login is still asked
    /// for, or a seat's tokens stay off the list for as long as the first
    /// item keeps failing. (A LOCKED Keychain is the whole Keychain's, and is
    /// settled by one probe before any item is read — `accounts::keychain_lock`.)
    #[test]
    fn one_items_failure_doesnt_hide_the_others() {
        let home = fake_home("one-fails");
        let seats: Vec<_> = (0..3).map(|i| home.join(format!("seat-{i}"))).collect();
        put(
            &home.join(".mafold/claude-accounts.json"),
            &serde_json::json!({"accounts": seats.iter().enumerate().map(|(i, d)| serde_json::json!({"name": format!("s{i}"), "dir": d})).collect::<Vec<_>>()}).to_string(),
        );
        let items: Vec<(String, String)> = seats
            .iter()
            .enumerate()
            .map(|(i, d)| (format!("Claude Code-credentials-{}", crate::accounts::hash8(d.to_str().unwrap())), format!("sk-ant-oat01-seat{i}-only-kc")))
            .collect();
        let keychain = move |service: &str| match items.iter().find(|(s, _)| s == service) {
            Some((_, token)) => Ok(Some(format!(r#"{{"claudeAiOauth":{{"accessToken":"{token}"}}}}"#))),
            None => Err("no answer from the Keychain in 2s — is it locked?".to_string()),
        };
        let found = sweep_with(&home, &[], Some(&keychain), &mut KeychainSeen::new());
        for i in 0..3 {
            let want = format!("sk-ant-oat01-seat{i}-only-kc");
            assert!(found.contains(&want), "missed {want}: {found:?}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// This Mac, for real: its own Claude Code login is in the Keychain and in
    /// no file, and the sweep that masks a bot's output now holds it. Prints
    /// COUNTS only. `cargo test --bin mafold -- --ignored this_macs_keychain_login`.
    #[test]
    #[ignore = "reads this Mac's real Claude Code login out of the Keychain"]
    fn this_macs_keychain_login_is_on_the_list() {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
        let env: Vec<(String, String)> = std::env::vars().collect();
        let claude = |found: &[String]| found.iter().filter(|f| f.starts_with("sk-ant-o")).count();
        let without = claude(&sweep_with(&home, &env, None, &mut KeychainSeen::new()));
        let with = claude(&sweep_with(&home, &env, crate::accounts::keychain(), &mut KeychainSeen::new()));
        println!("Claude Code tokens on the list: without the Keychain {without}, with it {with}");
        assert!(with > without, "the Keychain added no Claude Code token");
    }

    #[test]
    fn a_written_body_loses_every_spelling_of_every_value_and_nothing_else() {
        let values = vec![
            "mb_0badc0de0badc0de0badc0de0badc0d".to_string(),
            "sk-ant-oat01-default-0001".to_string(),
        ];
        let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&values[1]);
        let mut body = serde_json::json!({
            "message_id": "m1",
            "content": format!("$ env | grep MAFOLD\nMAFOLD_BOT_TOKEN={}\nauth: Basic {b64}\n日志 ✓ 200", values[0]),
            "cards": [{"detail": format!("curl -H 'Authorization: Bearer {}' …", values[1])}],
            "writer_seq": 7,
        });
        assert_eq!(scrub_value(&mut body, &values), 3);
        let text = body.to_string();
        for leaked in [&values[0], &values[1], &b64] {
            assert!(!text.contains(leaked.as_str()), "{leaked} survived: {text}");
        }
        assert_eq!(
            body["content"],
            format!("$ env | grep MAFOLD\nMAFOLD_BOT_TOKEN={MASK}\nauth: Basic {MASK}\n日志 ✓ 200")
        );
        assert_eq!(body["writer_seq"], 7);
        assert_eq!(scrub_value(&mut body, &values), 0, "scrubbing is idempotent");
    }

    #[test]
    fn only_writes_people_read_are_scrubbed() {
        let own = "mb_0badc0de0badc0de0badc0de0badc0d";
        // A call that carries a credential ON PURPOSE goes out as built.
        let mut deposit = serde_json::json!({"payload": {"token": own}});
        assert_eq!(scrub_outgoing("depositConnectionCustody", &mut deposit, own), 0);
        assert_eq!(deposit["payload"]["token"], own);
        // Every write that ends up in a conversation does not.
        for method in VISIBLE_WRITES {
            let mut body = serde_json::json!({"content": format!("token={own}"), "text": own});
            assert!(scrub_outgoing(method, &mut body, own) >= 2, "{method}");
            assert_eq!(body["content"], format!("token={MASK}"), "{method}");
            assert_eq!(body["text"], MASK, "{method}");
        }
    }
}
