//! Mafold Harness's tools on its owner's computer: a shell, and the files in
//! the agent's working folder. Named and shaped like Claude Code's own
//! (`Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep`), so a step renders as the
//! same card whichever runtime took it, and a model that knows one set knows
//! the other.
//!
//! - **Its folder.** The file tools take paths inside the working folder —
//!   relative to it, or absolute within it — and refuse any other, a symlink
//!   that leads out of it included. The shell starts there; like Claude
//!   Code's, it is a shell.
//! - **Never the fuel.** The shell loses every provider credential the daemon
//!   inherited and the turn's own key: the loop pays with that key, and
//!   nothing the model runs may read it.
//! - **Only the owner's circle.** A guest's turn is never handed this plugin
//!   (`super::mafold_harness`).

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mafold_harness::{HarnessPlugin, ToolCall, ToolSpec, TurnCtx};
use mafold_transcript::{AgentEvent, Transcript};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

/// How long a `Bash` call runs when it names no `timeout`, and the most it may.
const BASH_DEFAULT: Duration = Duration::from_secs(120);
const BASH_MAX: Duration = Duration::from_secs(600);

/// The most of one tool's output the model is handed; a longer one loses its
/// middle, never its end (where the error usually is).
const OUTPUT_CAP: usize = 30_000;

/// What a `Bash` call's pipes are read into at most, each; the rest is read
/// and dropped, so a chatty command never blocks on a full pipe.
const PIPE_KEEP: usize = 2 * 1024 * 1024;

/// `Read`: lines handed back when no `limit` is given, and the longest line.
const READ_LINES: usize = 2000;
const LINE_CAP: usize = 2000;

/// `Read` refuses a file larger than this rather than load it.
const READ_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// `Grep` skips a file larger than this.
const GREP_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The most entries `Glob` and `Grep` list.
const LIST_CAP: usize = 200;

/// The shell and the files of one turn.
pub struct Computer {
    workdir: PathBuf,
    /// Laid over the daemon's own environment (scrubbed, [`shell_env`]): the
    /// seat without its fuel, where the turn is, the reply being written.
    env: Vec<(String, String)>,
    proc: super::TurnProc,
}

impl Computer {
    pub fn new(workdir: PathBuf, env: Vec<(String, String)>, proc: super::TurnProc) -> Self {
        Self { workdir, env, proc }
    }
}

/// Environment prefixes another provider's credentials live under, plus any
/// OpenCode setting and any wallet or Go value the daemon itself inherited:
/// none of it reaches the shell, from the daemon's environment or the seat's.
/// Everything else (PATH, HOME, tool logins) stays, so the agent's tools work.
const SCRUB: &[&str] =
    &["OPENAI_", "ANTHROPIC_", "DEEPSEEK_", "GEMINI_", "OPENROUTER_", "OPENCODE_", "MAFOLD_WALLET_", "MAFOLD_GO_"];

/// What the shell's environment adds to the daemon's: `seat` (the turn's env)
/// without the fuel — the wallet's reply key and the Go key the loop pays with
/// — and anything else under a [`SCRUB`] prefix.
pub fn shell_env(seat: &[(String, String)], extra: Vec<(String, String)>) -> Vec<(String, String)> {
    seat.iter()
        .filter(|(k, _)| !SCRUB.iter().any(|p| k.starts_with(p)))
        .cloned()
        .chain(extra)
        .collect()
}

/// The shell `Bash` runs a command in, and the flag that hands it one.
fn shell() -> (&'static str, &'static str) {
    if super::on_path("bash") {
        ("bash", "-c")
    } else if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    }
}

#[async_trait]
impl HarnessPlugin for Computer {
    fn id(&self) -> &'static str {
        "computer"
    }

    fn prompt_sections(&self, _ctx: &TurnCtx) -> Vec<String> {
        let (sh, _) = shell();
        vec![format!(
            "## This computer\n\
             You are working on your owner's computer ({os}), in the folder `{dir}`.\n\
             - `Bash` runs a command there in `{sh}`: up to 10 minutes (`timeout`, in milliseconds; 2 minutes when not given).\n\
             - `Read`, `Write` and `Edit` work on files inside that folder; `Glob` finds files in it by name and `Grep` searches what they say. Paths are relative to the folder, and anything outside it is refused.\n\
             - Read a file before you edit it. `Edit` replaces `old_string` with `new_string` exactly, and refuses when `old_string` is missing or appears more than once (unless `replace_all`).",
            os = std::env::consts::OS,
            dir = self.workdir.display(),
        )]
    }

    async fn tools(&self, _ctx: &TurnCtx) -> Vec<ToolSpec> {
        let spec = |name: &str, description: &str, parameters: Value, read_only: bool| ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
            read_only,
        };
        let path = |what: &str| json!({ "type": "string", "description": what });
        vec![
            spec(
                "Bash",
                "Run a shell command in the working folder and return what it printed and its exit code.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The command to run." },
                        "timeout": { "type": "number", "description": "Milliseconds before it is stopped (default 120000, at most 600000)." },
                        "description": { "type": "string", "description": "What it does, in a few words." }
                    },
                    "required": ["command"]
                }),
                false,
            ),
            spec(
                "Read",
                "Read a file in the working folder, with line numbers.",
                json!({
                    "type": "object",
                    "properties": {
                        "file_path": path("The file, relative to the working folder."),
                        "offset": { "type": "integer", "description": "The line to start at (1 = the first)." },
                        "limit": { "type": "integer", "description": "How many lines (default 2000)." }
                    },
                    "required": ["file_path"]
                }),
                true,
            ),
            spec(
                "Write",
                "Write a file in the working folder, replacing it if it exists (folders are made as needed).",
                json!({
                    "type": "object",
                    "properties": {
                        "file_path": path("The file, relative to the working folder."),
                        "content": { "type": "string", "description": "Everything the file will say." }
                    },
                    "required": ["file_path", "content"]
                }),
                false,
            ),
            spec(
                "Edit",
                "Replace an exact piece of a file in the working folder.",
                json!({
                    "type": "object",
                    "properties": {
                        "file_path": path("The file, relative to the working folder."),
                        "old_string": { "type": "string", "description": "The exact text to replace." },
                        "new_string": { "type": "string", "description": "What replaces it." },
                        "replace_all": { "type": "boolean", "description": "Replace every occurrence (default: exactly one must exist)." }
                    },
                    "required": ["file_path", "old_string", "new_string"]
                }),
                false,
            ),
            spec(
                "Glob",
                "Find files in the working folder by a glob pattern (`**/*.rs`), newest first. Ignored files (.gitignore) and hidden ones are skipped.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "The glob, relative to `path`." },
                        "path": path("The folder to look in (default: the working folder).")
                    },
                    "required": ["pattern"]
                }),
                true,
            ),
            spec(
                "Grep",
                "Search what the files in the working folder say, by regular expression. Ignored files (.gitignore) and hidden ones are skipped.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "The regular expression." },
                        "path": path("A file or folder to search (default: the working folder)."),
                        "glob": { "type": "string", "description": "Only files whose name matches this glob (`*.ts`)." },
                        "output_mode": { "type": "string", "enum": ["files_with_matches", "content", "count"], "description": "Matching files (default), matching lines, or a count per file." },
                        "case_insensitive": { "type": "boolean" }
                    },
                    "required": ["pattern"]
                }),
                true,
            ),
        ]
    }

    async fn execute(
        &self,
        _ctx: &TurnCtx,
        _state: &mut (dyn std::any::Any + Send),
        tx: &mut Transcript,
        call: &ToolCall,
    ) -> String {
        let input: Value = serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
        tx.push(&AgentEvent::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            input: super::model_tool_input(&call.name, &input),
        });
        let (text, failed) = match call.name.as_str() {
            "Bash" => self.bash(&input).await,
            name @ ("Read" | "Write" | "Edit" | "Glob" | "Grep") => {
                let (name, dir) = (name.to_string(), self.workdir.clone());
                tokio::task::spawn_blocking(move || files(&name, &dir, &input))
                    .await
                    .unwrap_or_else(|e| (format!("the tool stopped: {e}"), true))
            }
            other => (format!("unknown tool: {other}"), true),
        };
        let text = cap(text);
        tx.push(&AgentEvent::ToolResult { id: call.id.clone(), text: text.clone() });
        tx.push(&AgentEvent::ToolStatus { id: call.id.clone(), failed });
        text
    }
}

/// The middle of `text` cut out when it is longer than [`OUTPUT_CAP`].
fn cap(text: String) -> String {
    if text.len() <= OUTPUT_CAP {
        return text;
    }
    let half = OUTPUT_CAP / 2;
    let head_end = floor_char(&text, half);
    let tail_start = ceil_char(&text, text.len() - half);
    format!(
        "{}\n\n… {} characters cut …\n\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ───────────────────────────────── Bash ─────────────────────────────────

/// Kills the command's whole process group unless disarmed — a Stop drops the
/// tool's future mid-run, and a timeout gives up on it; either way, nothing it
/// started keeps running for a turn that is over. A command that ends by
/// itself is disarmed: what it put in the background is the user's.
struct GroupKill(Option<u32>);

impl GroupKill {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        #[cfg(unix)]
        // SAFETY: plain kill(2) on the group the command leads (process_group(0)).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
        // No process groups to aim at: the command itself goes.
        #[cfg(not(unix))]
        crate::platform::terminate(pid);
    }
}

/// A pipe read to its end in the background, kept up to [`PIPE_KEEP`].
fn drain<R: tokio::io::AsyncRead + Unpin + Send + 'static>(pipe: Option<R>) -> (Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>) {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let into = kept.clone();
    let task = tokio::spawn(async move {
        let Some(mut pipe) = pipe else { return };
        let mut buf = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let mut k = into.lock().unwrap();
            let room = PIPE_KEEP.saturating_sub(k.len());
            k.extend_from_slice(&buf[..n.min(room)]);
        }
    });
    (kept, task)
}

impl Computer {
    async fn bash(&self, input: &Value) -> (String, bool) {
        let Some(command) = input["command"].as_str().filter(|c| !c.trim().is_empty()) else {
            return ("`Bash` needs a `command`.".into(), true);
        };
        let limit = input["timeout"]
            .as_f64()
            .filter(|ms| *ms > 0.0)
            .map(|ms| Duration::from_millis(ms as u64))
            .unwrap_or(BASH_DEFAULT)
            .min(BASH_MAX);
        let (program, flag) = shell();
        let mut cmd = crate::platform::command(program);
        cmd.arg(flag).arg(command);
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if SCRUB.iter().any(|p| k.starts_with(p)) {
                cmd.env_remove(k.as_ref());
            }
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.current_dir(&self.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        super::opencode::die_with_daemon(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return (format!("couldn't start `{program}`: {e}"), true),
        };
        let pid = child.id();
        let _serving = self.proc.serve(pid);
        let mut group = GroupKill(pid);
        let (out, out_task) = drain(child.stdout.take());
        let (err, err_task) = drain(child.stderr.take());
        let status = match tokio::time::timeout(limit, child.wait()).await {
            Ok(Ok(status)) => {
                group.disarm();
                Some(status)
            }
            Ok(Err(e)) => return (format!("lost the command: {e}"), true),
            // `group` kills it, and all it started, when this returns.
            Err(_) => None,
        };
        // What it printed before it ended is in the pipes; a pipe still open
        // after this belongs to something it started and left running.
        let drained = tokio::time::timeout(super::EXIT_DRAIN, async {
            let _ = out_task.await;
            let _ = err_task.await;
        })
        .await
        .is_ok();
        let mut text = String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
        let err = String::from_utf8_lossy(&err.lock().unwrap()).into_owned();
        if !err.trim().is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&err);
        }
        text.truncate(text.trim_end().len());
        let mut failed = false;
        match status {
            None => {
                failed = true;
                text.push_str(&format!("\n\nStopped after {} seconds (the timeout).", limit.as_secs()));
            }
            Some(s) if !s.success() => {
                failed = true;
                match s.code() {
                    Some(code) => text.push_str(&format!("\n\nExit code {code}")),
                    None => text.push_str("\n\nEnded by a signal"),
                }
            }
            Some(_) => {}
        }
        if status.is_some() && !drained {
            text.push_str("\n\n(Something it started is still running in the background and holding its output.)");
        }
        if text.trim().is_empty() {
            text = "(no output)".into();
        }
        (text.trim_start_matches('\n').to_string(), failed)
    }
}

// ───────────────────────────────── files ─────────────────────────────────

/// A path the model gave, as the file it names inside `workdir` — or why not.
/// Lexically first (`..` never climbs out), then through the filesystem: the
/// deepest part of it that exists must still be inside once its symlinks are
/// followed.
pub fn resolve(workdir: &Path, raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("a path is needed".into());
    }
    let given = Path::new(raw);
    let joined = if given.is_absolute() { given.to_path_buf() } else { workdir.join(given) };
    let path = normalize(&joined);
    let root = normalize(workdir);
    let outside = || format!("`{raw}` is outside the working folder ({}), so it is refused.", workdir.display());
    if !path.starts_with(&root) {
        return Err(outside());
    }
    let real_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let mut probe = Some(path.as_path());
    while let Some(p) = probe {
        if let Ok(real) = std::fs::canonicalize(p) {
            if !real.starts_with(&real_root) {
                return Err(outside());
            }
            break;
        }
        probe = p.parent();
    }
    Ok(path)
}

/// `.` dropped and `..` applied, without asking the filesystem.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` as the model should see it: relative to the working folder.
fn shown(workdir: &Path, path: &Path) -> String {
    path.strip_prefix(normalize(workdir)).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// One of the file tools, run off the async runtime.
fn files(name: &str, workdir: &Path, input: &Value) -> (String, bool) {
    let r = match name {
        "Read" => read(workdir, input),
        "Write" => write(workdir, input),
        "Edit" => edit(workdir, input),
        "Glob" => glob(workdir, input),
        "Grep" => grep(workdir, input),
        other => Err(format!("unknown tool: {other}")),
    };
    match r {
        Ok(text) => (text, false),
        Err(why) => (why, true),
    }
}

fn arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input[key].as_str().ok_or_else(|| format!("`{key}` is needed"))
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

fn read(workdir: &Path, input: &Value) -> Result<String, String> {
    let raw = arg(input, "file_path")?;
    let path = resolve(workdir, raw)?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("can't read `{raw}`: {e}"))?;
    if meta.is_dir() {
        return Err(format!("`{raw}` is a folder — `Glob` lists what is in it."));
    }
    if meta.len() > READ_MAX_BYTES {
        return Err(format!("`{raw}` is {} bytes — too large to read whole; `Grep` it, or read part of it with `Bash`.", meta.len()));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("can't read `{raw}`: {e}"))?;
    if looks_binary(&bytes) {
        return Ok(format!("`{raw}` is a binary file ({} bytes), not shown.", bytes.len()));
    }
    let text = String::from_utf8_lossy(&bytes);
    if text.is_empty() {
        return Ok(format!("`{raw}` is empty."));
    }
    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = input["limit"].as_u64().map(|n| n as usize).filter(|n| *n > 0).unwrap_or(READ_LINES);
    let total = text.lines().count();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let line = if line.len() > LINE_CAP { format!("{}…", &line[..floor_char(line, LINE_CAP)]) } else { line.to_string() };
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
    }
    if out.is_empty() {
        return Ok(format!("`{raw}` has {total} lines; nothing from line {offset}."));
    }
    let last = (offset - 1 + limit).min(total);
    if last < total {
        out.push_str(&format!("… {} more lines (from line {}).\n", total - last, last + 1));
    }
    Ok(out)
}

fn write(workdir: &Path, input: &Value) -> Result<String, String> {
    let raw = arg(input, "file_path")?;
    let content = arg(input, "content")?;
    let path = resolve(workdir, raw)?;
    if path.is_dir() {
        return Err(format!("`{raw}` is a folder."));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("can't make the folder for `{raw}`: {e}"))?;
    }
    let existed = path.exists();
    std::fs::write(&path, content).map_err(|e| format!("can't write `{raw}`: {e}"))?;
    Ok(format!(
        "{} {} ({} lines).",
        if existed { "Rewrote" } else { "Created" },
        shown(workdir, &path),
        content.lines().count()
    ))
}

fn edit(workdir: &Path, input: &Value) -> Result<String, String> {
    let raw = arg(input, "file_path")?;
    let old = arg(input, "old_string")?;
    let new = arg(input, "new_string")?;
    let all = input["replace_all"].as_bool().unwrap_or(false);
    if old.is_empty() {
        return Err("`old_string` is empty — `Write` makes a new file.".into());
    }
    if old == new {
        return Err("`old_string` and `new_string` are the same; nothing to change.".into());
    }
    let path = resolve(workdir, raw)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("can't read `{raw}`: {e}"))?;
    let n = text.matches(old).count();
    let replaced = match (n, all) {
        (0, _) => return Err(format!("`old_string` is not in `{raw}` — read it again and copy the text exactly.")),
        (1, _) | (_, true) => {
            if all {
                text.replace(old, new)
            } else {
                text.replacen(old, new, 1)
            }
        }
        (n, false) => {
            return Err(format!(
                "`old_string` appears {n} times in `{raw}` — give more of the text around it, or set `replace_all`."
            ))
        }
    };
    std::fs::write(&path, replaced).map_err(|e| format!("can't write `{raw}`: {e}"))?;
    Ok(format!("Edited {} ({} replaced).", shown(workdir, &path), if all { n } else { 1 }))
}

/// Where a search starts: `path`, or the working folder.
fn base(workdir: &Path, input: &Value) -> Result<PathBuf, String> {
    match input["path"].as_str().filter(|p| !p.trim().is_empty()) {
        Some(p) => resolve(workdir, p),
        None => Ok(normalize(workdir)),
    }
}

fn matcher(pattern: &str) -> Result<globset::GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("`{pattern}` isn't a glob: {e}"))
}

fn glob(workdir: &Path, input: &Value) -> Result<String, String> {
    let pattern = arg(input, "pattern")?;
    let from = base(workdir, input)?;
    let m = matcher(pattern)?;
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in ignore::WalkBuilder::new(&from).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(&from).unwrap_or(entry.path());
        if m.is_match(rel) {
            let at = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
            found.push((at, entry.path().to_path_buf()));
        }
    }
    if found.is_empty() {
        return Ok(format!("No files match `{pattern}`."));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let more = found.len().saturating_sub(LIST_CAP);
    let mut out: Vec<String> = found.iter().take(LIST_CAP).map(|(_, p)| shown(workdir, p)).collect();
    if more > 0 {
        out.push(format!("… and {more} more."));
    }
    Ok(out.join("\n"))
}

fn grep(workdir: &Path, input: &Value) -> Result<String, String> {
    let pattern = arg(input, "pattern")?;
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(input["case_insensitive"].as_bool().unwrap_or(false))
        .build()
        .map_err(|e| format!("`{pattern}` isn't a regular expression: {e}"))?;
    let from = base(workdir, input)?;
    let only = match input["glob"].as_str().filter(|g| !g.trim().is_empty()) {
        Some(g) => Some(matcher(g)?),
        None => None,
    };
    let mode = input["output_mode"].as_str().unwrap_or("files_with_matches");
    let mut lines: Vec<String> = Vec::new();
    let mut more = 0usize;
    for entry in ignore::WalkBuilder::new(&from).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Some(m) = &only {
            let name = entry.path().file_name().map(Path::new).unwrap_or(entry.path());
            if !m.is_match(name) && !m.is_match(entry.path().strip_prefix(&from).unwrap_or(entry.path())) {
                continue;
            }
        }
        if entry.metadata().map(|m| m.len() > GREP_MAX_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        if looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let file = shown(workdir, entry.path());
        let hits: Vec<(usize, &str)> = text.lines().enumerate().filter(|(_, l)| re.is_match(l)).collect();
        if hits.is_empty() {
            continue;
        }
        let rows: Vec<String> = match mode {
            "content" => hits
                .iter()
                .map(|(i, l)| {
                    let l = if l.len() > 500 { format!("{}…", &l[..floor_char(l, 500)]) } else { l.to_string() };
                    format!("{file}:{}:{l}", i + 1)
                })
                .collect(),
            "count" => vec![format!("{file}:{}", hits.len())],
            _ => vec![file],
        };
        for row in rows {
            if lines.len() < LIST_CAP {
                lines.push(row);
            } else {
                more += 1;
            }
        }
    }
    if lines.is_empty() {
        return Ok(format!("Nothing matches `{pattern}`."));
    }
    if more > 0 {
        lines.push(format!("… and {more} more."));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mafold-computer-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn computer(dir: &Path) -> Computer {
        Computer::new(dir.to_path_buf(), vec![], super::super::TurnProc::default())
    }

    async fn run(c: &Computer, name: &str, input: Value) -> (String, bool, Vec<AgentEvent>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let into = events.clone();
        let mut tx = Transcript::new();
        tx.set_tap(Arc::new(move |e: &AgentEvent| into.lock().unwrap().push(e.clone())));
        let call = ToolCall { id: "t1".into(), name: name.into(), arguments: input.to_string(), ..Default::default() };
        let text = c.execute(&TurnCtx::default(), &mut (), &mut tx, &call).await;
        let events = events.lock().unwrap().clone();
        let failed = events.iter().any(|e| matches!(e, AgentEvent::ToolStatus { failed: true, .. }));
        (text, failed, events)
    }

    #[test]
    fn a_path_must_stay_inside_the_folder() {
        let dir = folder("resolve");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        assert_eq!(resolve(&dir, "src/a.rs").unwrap(), normalize(&dir.join("src/a.rs")));
        assert_eq!(resolve(&dir, "./src/../b.txt").unwrap(), normalize(&dir.join("b.txt")));
        assert!(resolve(&dir, dir.join("src/new.rs").to_str().unwrap()).is_ok(), "absolute, inside");
        assert!(resolve(&dir, "../outside.txt").unwrap_err().contains("outside the working folder"));
        assert!(resolve(&dir, "/etc/hosts").is_err());
        assert!(resolve(&dir, "src/../../x").is_err());
        assert!(resolve(&dir, " ").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_folder_is_refused() {
        let dir = folder("link");
        let out = folder("elsewhere");
        std::fs::write(out.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(&out, dir.join("door")).unwrap();
        assert!(resolve(&dir, "door/secret.txt").is_err(), "through a symlinked folder");
        assert!(resolve(&dir, "door/new.txt").is_err(), "a new file behind it too");
    }

    #[test]
    fn the_shell_never_gets_the_fuel() {
        let seat = vec![
            ("MAFOLD_WALLET_KEY".to_string(), "k".to_string()),
            ("MAFOLD_GO_KEY".to_string(), "g".to_string()),
            ("OPENAI_API_KEY".to_string(), "o".to_string()),
            ("CLAUDE_CONFIG_DIR".to_string(), "/x".to_string()),
        ];
        let env = shell_env(&seat, vec![("MAFOLD_DRAFT".into(), "d".into())]);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["CLAUDE_CONFIG_DIR", "MAFOLD_DRAFT"]);
    }

    #[tokio::test]
    async fn bash_runs_in_the_folder_and_says_how_it_ended() {
        let dir = folder("bash");
        std::fs::write(dir.join("here.txt"), "x").unwrap();
        let c = computer(&dir);
        let (text, failed, events) = run(&c, "Bash", json!({ "command": "ls && echo done" })).await;
        assert!(text.contains("here.txt") && text.contains("done"), "{text}");
        assert!(!failed);
        assert!(matches!(&events[0], AgentEvent::ToolCall { name, .. } if name == "Bash"), "the card first: {events:?}");
        let (text, failed, _) = run(&c, "Bash", json!({ "command": "echo oops >&2; exit 3" })).await;
        assert!(text.contains("oops") && text.contains("Exit code 3"), "{text}");
        assert!(failed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_is_stopped_at_its_timeout_with_what_it_had_said() {
        let dir = folder("timeout");
        let c = computer(&dir);
        let started = std::time::Instant::now();
        let (text, failed, _) = run(&c, "Bash", json!({ "command": "echo first; sleep 30", "timeout": 500 })).await;
        assert!(started.elapsed() < Duration::from_secs(10), "it did not wait the 30 s out");
        assert!(failed);
        assert!(text.contains("first") && text.contains("timeout"), "{text}");
    }

    #[tokio::test]
    async fn write_read_and_edit_a_file() {
        let dir = folder("files");
        let c = computer(&dir);
        let (text, failed, _) = run(&c, "Write", json!({ "file_path": "notes/a.md", "content": "one\ntwo\ntwo\n" })).await;
        assert!(!failed && text.starts_with("Created notes/a.md"), "{text}");
        let (text, _, _) = run(&c, "Read", json!({ "file_path": "notes/a.md" })).await;
        assert_eq!(text, "     1\tone\n     2\ttwo\n     3\ttwo\n");
        let (text, failed, _) = run(&c, "Edit", json!({ "file_path": "notes/a.md", "old_string": "two", "new_string": "2" })).await;
        assert!(failed && text.contains("appears 2 times"), "{text}");
        let (_, failed, _) = run(&c, "Edit", json!({ "file_path": "notes/a.md", "old_string": "one", "new_string": "1" })).await;
        assert!(!failed);
        let (_, failed, _) = run(&c, "Edit", json!({ "file_path": "notes/a.md", "old_string": "two", "new_string": "2", "replace_all": true })).await;
        assert!(!failed);
        assert_eq!(std::fs::read_to_string(dir.join("notes/a.md")).unwrap(), "1\n2\n2\n");
        let (text, failed, _) = run(&c, "Write", json!({ "file_path": "../escape.md", "content": "x" })).await;
        assert!(failed && text.contains("outside the working folder"), "{text}");
        assert!(!dir.parent().unwrap().join("escape.md").exists());
    }

    #[tokio::test]
    async fn glob_and_grep_skip_what_git_ignores() {
        let dir = folder("search");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() { alpha(); }\n").unwrap();
        std::fs::write(dir.join("target/gen.rs"), "fn alpha() {}\n").unwrap();
        // `ignore` honours .gitignore inside a git repository.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let c = computer(&dir);
        let (text, _, _) = run(&c, "Glob", json!({ "pattern": "**/*.rs" })).await;
        let mut found: Vec<&str> = text.lines().collect();
        found.sort();
        assert_eq!(found, ["src/lib.rs", "src/main.rs"]);
        let (text, _, _) = run(&c, "Grep", json!({ "pattern": "alpha", "output_mode": "content" })).await;
        let mut found: Vec<&str> = text.lines().collect();
        found.sort();
        assert_eq!(found, ["src/lib.rs:1:fn alpha() {}", "src/main.rs:1:fn main() { alpha(); }"]);
        let (text, _, _) = run(&c, "Grep", json!({ "pattern": "main", "glob": "*.rs" })).await;
        assert_eq!(text, "src/main.rs");
    }

    #[test]
    fn a_long_output_loses_its_middle_not_its_end() {
        let text = format!("{}{}", "a".repeat(OUTPUT_CAP), "THE END");
        let capped = cap(text);
        assert!(capped.len() < OUTPUT_CAP + 100);
        assert!(capped.starts_with('a') && capped.ends_with("THE END"));
        assert!(capped.contains("characters cut"));
    }
}
