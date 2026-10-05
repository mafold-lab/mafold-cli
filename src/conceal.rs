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
use std::process::{Command, Stdio};

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
    let mut child = Command::new(program)
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
        let child = Command::new("sh")
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
}
