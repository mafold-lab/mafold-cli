//! How one `sandboxRun` answer (`mafold-api/src/sandbox_api.rs`) reads to the
//! agent that ran it. One copy for both doors a run goes through: the hosted
//! kernel's `python` tool (`mafold-api/src/brains/sandbox.rs`), whose model
//! reads it as the tool's result, and `mafold sandbox run`, which prints it for
//! an agent on a computer. Two copies would drift, and then the same run would
//! say different things depending on where the agent happens to run.

use serde_json::Value;

/// What the image has (`mafold-sandbox/image/requirements.in`), as both doors'
/// prompts name it.
pub const INSTALLED: &str = "numpy, pandas, polars, pyarrow, scipy, scikit-learn, statsmodels, matplotlib \
(Chinese text renders), seaborn, plotly, openpyxl, xlsxwriter, xlrd, python-docx, python-pptx, pypdf, \
pdfplumber, reportlab, pillow, sympy, networkx, beautifulsoup4, lxml, chardet, tabulate";

/// What an agent reads of one stream, head and tail — `open_url`'s page
/// budget. The Worker already keeps only the first and last 10 KiB.
pub const STREAM_CHARS: usize = 12_000;

/// The names of the outputs a run attached to the reply.
pub fn attached(v: &Value) -> impl Iterator<Item = &str> {
    v["outputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["attached"].as_bool() == Some(true))
        .filter_map(|o| o["name"].as_str())
}

/// `text`, cut to its first and last part when it is long — said, never
/// silent: a model that doesn't know the middle is missing reasons from it.
pub fn cut(text: &str, budget: usize) -> String {
    let n = text.chars().count();
    if n <= budget {
        return text.to_string();
    }
    let half = budget / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(n - half).collect();
    format!("{head}\n[… {} characters cut …]\n{tail}", n - 2 * half)
}

/// What the agent reads back from a run.
pub fn run_text(v: &Value) -> String {
    let mut out = String::new();
    let exit = v["exit"].as_i64();
    let secs = v["wall_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
    match (v["timed_out"].as_bool() == Some(true), exit) {
        (true, _) => out.push_str(&format!("Stopped at its time limit after {secs:.1} s (exit {}).\n", exit.unwrap_or(137))),
        (false, Some(0)) => out.push_str(&format!("exit 0 in {secs:.1} s\n")),
        (false, Some(e)) => out.push_str(&format!("exit {e} in {secs:.1} s — the code failed\n")),
        (false, None) => out.push_str(&format!("killed by signal {} after {secs:.1} s\n", v["signal"].as_i64().unwrap_or(0))),
    }
    if v["restarted"].as_bool() == Some(true) {
        out.push_str(
            "The sandbox had stopped after 5 idle minutes and started fresh: files earlier runs \
             wrote are gone (inputs were copied in again).\n",
        );
    }
    for (name, key) in [("stdout", "stdout"), ("stderr", "stderr")] {
        let text = v[key].as_str().unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(&format!("\n{name}:\n{}", cut(text, STREAM_CHARS)));
        if v[format!("{key}_cut")].as_bool() == Some(true) {
            out.push_str(&format!(
                "\n[{name} was {} bytes; only its first and last 10 KiB came back]",
                v[format!("{key}_bytes")].as_u64().unwrap_or(0)
            ));
        }
        out.push('\n');
    }
    let files: Vec<&str> = attached(v).collect();
    if !files.is_empty() {
        out.push_str(&format!("\nAttached to your reply: {}\n", files.join(", ")));
    }
    for o in v["outputs"].as_array().into_iter().flatten() {
        if let (Some(name), Some(err)) = (o["name"].as_str(), o["error"].as_str()) {
            out.push_str(&format!("Not attached: {name} — {err}\n"));
        }
    }
    if let Some(n) = v["outputs_more"].as_u64().filter(|n| *n > 0) {
        out.push_str(&format!("{n} more files in /mnt/data/out weren't looked at (100 per run).\n"));
    }
    if let Some(skipped) = v["outputs_skipped"].as_array().filter(|a| !a.is_empty()) {
        let names: Vec<&str> = skipped.iter().filter_map(|s| s.as_str().or_else(|| s["name"].as_str())).collect();
        out.push_str(&format!("Skipped in /mnt/data/out (folders, links, hidden files): {}\n", names.join(", ")));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_failed_run_reads_as_its_traceback_and_a_good_one_as_its_files() {
        let failed = json!({
            "exit": 1, "timed_out": false, "wall_ms": 300, "stdout": "",
            "stderr": "Traceback (most recent call last):\n  File \"cell-1.py\", line 1\nKeyError: 'amount'\n",
            "outputs": [],
        });
        let text = run_text(&failed);
        assert!(text.starts_with("exit 1 in 0.3 s — the code failed"), "{text}");
        assert!(text.contains("KeyError: 'amount'"), "{text}");

        let ok = json!({
            "exit": 0, "timed_out": false, "wall_ms": 1400, "stdout": "rows: 12\n", "stderr": "",
            "outputs": [
                { "name": "chart.png", "attached": true },
                { "name": "huge.bin", "error": "over 100 MiB" },
            ],
            "outputs_more": 0, "outputs_skipped": [], "restarted": false,
        });
        let text = run_text(&ok);
        assert!(text.contains("stdout:\nrows: 12"), "{text}");
        assert!(text.contains("Attached to your reply: chart.png"), "{text}");
        assert!(text.contains("Not attached: huge.bin — over 100 MiB"), "{text}");
        assert!(!text.contains("stderr"), "an empty stream is left out: {text}");
        assert_eq!(attached(&ok).collect::<Vec<_>>(), vec!["chart.png"]);

        let slow = json!({ "exit": 137, "timed_out": true, "wall_ms": 120_000, "stdout": "", "stderr": "", "outputs": [] });
        assert!(run_text(&slow).starts_with("Stopped at its time limit after 120.0 s (exit 137)."));
    }

    #[test]
    fn long_output_is_cut_out_loud() {
        let long: String = (0..5000).map(|i| format!("{i}\n")).collect();
        let c = cut(&long, 100);
        assert!(c.starts_with("0\n1\n"));
        assert!(c.ends_with("4999\n"));
        assert!(c.contains("characters cut"));
        assert_eq!(cut("short", 100), "short");
    }
}
