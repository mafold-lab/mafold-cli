//! What each Claude account has used, for `/usage`.
//!
//! ops 11-13: «我主要需要当前的usage情况 你自己的claude还剩多少» — how much is
//! left, for every login turns can move to, and how long it lasts. So `/usage`
//! sends, per account and in the order a turn tries them: its plan, whether
//! the next turn would run on it, every window Anthropic reports for it, and
//! how fast turns are using things up. Facts only: the stats card
//! (`cards/_agent/usage.ts`, which also has the line grammar) works out the
//! run-out time, draws the rings and says it in the reader's language and
//! time zone.
//!
//! The pace needs readings over time, and a probe is one moment. Every live
//! answer from `/api/oauth/usage` — the heartbeat asks it for each login every
//! five minutes, a turn start asks it, `/usage` asks it — is appended to
//! `~/.mafold/claude-usage/<account>.jsonl`, a week of it kept. The same file
//! stands in for an account that can't be read right now (an idle backup seat
//! whose access token has lapsed, a 429): what it last said, and when.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// One window as Anthropic reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Claude's own name: `session` (the 5-hour window), `weekly_all`,
    /// `weekly_scoped`, …
    pub kind: String,
    /// Percent used.
    pub pct: f64,
    /// When it rolls over, epoch seconds.
    pub resets: Option<i64>,
    /// The model a scoped window is for ("Fable").
    pub model: Option<String>,
}

/// An account's windows at one moment (epoch seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub t: i64,
    pub windows: Vec<Window>,
}

impl Reading {
    fn get(&self, kind: &str) -> Option<&Window> {
        self.windows.iter().find(|w| w.kind == kind)
    }
}

const FIVE_HOURS: i64 = 5 * 3600;
const WEEK: i64 = 7 * 86_400;
/// Two readings belong to the same window when their resets agree this
/// closely (Anthropic's `resets_at` jitters by a second or so between asks).
const SAME_RESET: i64 = 120;

/// `limits[]` of an `/api/oauth/usage` answer as windows.
pub fn windows_from_usage(v: &Value) -> Vec<Window> {
    v["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let pct = l["percent"].as_f64().filter(|p| p.is_finite() && *p >= 0.0)?;
            let model = l["scope"]["model"]["display_name"]
                .as_str()
                .or_else(|| l["scope"]["model"]["id"].as_str())
                .map(clean)
                .filter(|s| !s.is_empty());
            Some(Window {
                kind: clean(l["kind"].as_str().unwrap_or("usage")),
                pct,
                resets: l["resets_at"].as_str().and_then(crate::commands::iso_epoch_secs),
                model,
            })
        })
        .collect()
}

/// A value going into a `|`-separated card line: no separators, no breaks.
fn clean(s: &str) -> String {
    s.chars().filter(|c| *c != '|' && !c.is_control()).take(40).collect()
}

// ───────────────────────── the history file ─────────────────────────

/// `~/.mafold/claude-usage/<account>.jsonl`.
pub fn history_path(name: &str) -> PathBuf {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    home.join(".mafold/claude-usage").join(format!("{name}.jsonl"))
}

/// Readings closer together than this add nothing to a pace: the heartbeat,
/// a turn start and `/usage` often ask within the same minute.
const MIN_GAP: i64 = 120;
/// How much history is kept: a week, the longest window there is.
const KEEP: i64 = WEEK + 86_400;
/// Rewrite (and so trim) the file once it grows past this — a week of
/// five-minute readings is ~2,000 lines of ~150 bytes.
const MAX_BYTES: u64 = 450_000;

/// Remember what `name`'s windows said at `now`. Best effort: a reading that
/// can't be written is a pace that is a little less sure, never an error.
pub fn record(name: &str, v: &Value, now: i64) {
    if !crate::accounts::valid_name(name) {
        return;
    }
    record_at(&history_path(name), &windows_from_usage(v), now);
}

/// [`record`] into a given file (the tests' own).
pub fn record_at(path: &Path, windows: &[Window], now: i64) {
    if windows.is_empty() {
        return;
    }
    // The last line is the newest (appends only); one line read, not all.
    let last = std::fs::read_to_string(path).ok().and_then(|raw| raw.lines().rev().find_map(decode)).map(|r| r.t);
    if last.is_some_and(|t| now - t < MIN_GAP) {
        return;
    }
    let line = encode(&Reading { t: now, windows: windows.to_vec() });
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    // One small `O_APPEND` write per reading: several daemons on one machine
    // append to the same file without interleaving.
    use std::io::Write;
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
    if appended.is_err() {
        return;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let keep: Vec<String> = read_history(path).iter().filter(|r| r.t >= now - KEEP).map(encode).collect();
        let tmp = path.with_extension(format!("jsonl.{}", std::process::id()));
        if std::fs::write(&tmp, keep.join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn encode(r: &Reading) -> String {
    let w: Vec<Value> = r
        .windows
        .iter()
        .map(|w| serde_json::json!([w.kind, w.pct, w.resets, w.model]))
        .collect();
    serde_json::json!({ "t": r.t, "w": w }).to_string()
}

fn decode(line: &str) -> Option<Reading> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let t = v["t"].as_i64()?;
    let windows = v["w"]
        .as_array()?
        .iter()
        .filter_map(|w| {
            Some(Window {
                kind: w[0].as_str()?.to_string(),
                pct: w[1].as_f64()?,
                resets: w[2].as_i64(),
                model: w[3].as_str().map(str::to_string),
            })
        })
        .collect();
    Some(Reading { t, windows })
}

/// Every reading in `path`, oldest first; lines that don't read are skipped.
pub fn read_history(path: &Path) -> Vec<Reading> {
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<Reading> = raw.lines().filter_map(decode).collect();
    out.sort_by_key(|r| r.t);
    out
}

// ───────────────────────── pace ─────────────────────────

/// How fast turns are using things up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    /// 5-hour percent per hour on a cap-1 plan (see [`plan_of`]): work moved
    /// to a plan with four times the room drains it a quarter as fast.
    pub work: f64,
    /// How much of a week one percent of 5-hour costs.
    pub wk: f64,
    /// Minutes it was measured over.
    pub span: i64,
}

/// When too little has moved to measure it: on redq (Team, Max 5×) on 11-13
/// the week went 64 → 72% while the 5-hour went 24 → 71%, 0.17 of a week per
/// percent of 5-hour.
pub const DEFAULT_WK: f64 = 0.17;

/// One account's share of the pace: its plan's room and its readings,
/// oldest first, the current one last.
pub struct Track<'a> {
    pub cap: f64,
    pub readings: &'a [Reading],
}

/// 5-hour percent used between two readings, when it can be told: the same
/// window (its reset agrees), or a window that started after the first.
fn used5(a: &Reading, b: &Reading) -> Option<f64> {
    match (running5(a), running5(b)) {
        (_, None) => {
            // No window running at b: none was used since its last reset —
            // unless a's was still running and has since ended, which
            // leaves what happened after a unknown.
            match running5(a).and_then(|w| w.resets) {
                Some(end) if end <= b.t => None,
                _ => Some(0.0),
            }
        }
        (Some(wa), Some(wb)) if same(wa.resets, wb.resets) => Some((wb.pct - wa.pct).max(0.0)),
        (_, Some(wb)) => match wb.resets {
            Some(end) if end - FIVE_HOURS >= a.t - SAME_RESET => Some(wb.pct),
            _ => None,
        },
    }
}

/// The 5-hour window still running at the reading's own moment.
fn running5(r: &Reading) -> Option<&Window> {
    r.get("session").filter(|w| w.resets.is_none_or(|x| x > r.t))
}

/// Week percent used between two readings of the same week.
fn used_week(a: &Reading, b: &Reading) -> Option<f64> {
    let (wa, wb) = (a.get("weekly_all")?, b.get("weekly_all")?);
    same(wa.resets, wb.resets).then(|| (wb.pct - wa.pct).max(0.0))
}

fn same(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() <= SAME_RESET,
        _ => false,
    }
}

/// How much of a week a percent of 5-hour costs, from the last two days of
/// readings; [`DEFAULT_WK`] until enough has moved to say.
pub fn week_ratio(tracks: &[Track], now: i64) -> f64 {
    let (mut five, mut week) = (0.0, 0.0);
    for tr in tracks {
        let rs: Vec<&Reading> = tr.readings.iter().filter(|r| r.t >= now - 2 * 86_400).collect();
        for pair in rs.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if b.t - a.t > 90 * 60 {
                continue;
            }
            if let (Some(f), Some(w)) = (used5(a, b), used_week(a, b)) {
                five += f * tr.cap;
                week += w * tr.cap;
            }
        }
    }
    if five >= 40.0 {
        (week / five).clamp(0.02, 1.0)
    } else {
        DEFAULT_WK
    }
}

/// This hour's pace: what each account used over about the last hour, added
/// up. `in_use` is the account the next turn runs on — when no account has
/// two readings an hour apart, its current 5-hour window, averaged since the
/// window began, stands in. None when nothing can be said.
pub fn recent(tracks: &[Track], in_use: Option<usize>, wk: f64, now: i64) -> Option<Pace> {
    let mut work = 0.0;
    let mut span = 0;
    let mut measured = false;
    for tr in tracks {
        let Some(b) = tr.readings.last().filter(|b| b.t >= now - 15 * 60) else { continue };
        // The reading closest to an hour before the newest, 20 to 75 minutes back.
        let a = tr
            .readings
            .iter()
            .filter(|a| (20 * 60..=75 * 60).contains(&(b.t - a.t)))
            .min_by_key(|a| (b.t - a.t - 3600).abs());
        let Some(a) = a else { continue };
        let Some(used) = used5(a, b) else { continue };
        let hours = (b.t - a.t) as f64 / 3600.0;
        work += used / hours * tr.cap;
        span = span.max((b.t - a.t) / 60);
        measured = true;
    }
    if measured {
        return Some(Pace { work, wk, span });
    }
    let tr = tracks.get(in_use?)?;
    let b = tr.readings.last()?;
    match b.get("session").and_then(|w| w.resets.filter(|r| *r > now).map(|r| (w.pct, r))) {
        Some((pct, end)) => {
            let elapsed = now - (end - FIVE_HOURS);
            (elapsed >= 15 * 60).then(|| Pace { work: pct / (elapsed as f64 / 3600.0) * tr.cap, wk, span: elapsed / 60 })
        }
        // No 5-hour window running on the account turns use: nothing has
        // been used in the last five hours.
        None => Some(Pace { work: 0.0, wk, span: 0 }),
    }
}

/// The week's average pace: each account's week so far, per hour, added up
/// and turned into 5-hour terms through `wk`.
pub fn week_average(tracks: &[Track], wk: f64, now: i64) -> Option<Pace> {
    let mut work_w = 0.0;
    let mut span = 0;
    for tr in tracks {
        let Some(b) = tr.readings.last() else { continue };
        let Some(w) = b.get("weekly_all") else { continue };
        let Some(end) = w.resets.filter(|r| *r > now) else { continue };
        let elapsed = (now - (end - WEEK)).max(3600);
        work_w += w.pct / (elapsed as f64 / 3600.0) * tr.cap;
        span = span.max(elapsed / 60);
    }
    (span > 0).then(|| Pace { work: work_w / wk, wk, span })
}

// ───────────────────────── plans ─────────────────────────

/// "Claude Team · Max 5×", and the plan's room as a multiple of Pro's (Max
/// 5× is five times Pro, Max 20× twenty) — None when the tier doesn't say.
pub fn plan_of(org_type: Option<&str>, tier: Option<&str>) -> (String, Option<f64>) {
    let tier = tier.unwrap_or("");
    let (size, cap) = if tier.contains("20x") {
        (Some("Max 20×"), Some(20.0))
    } else if tier.contains("5x") {
        (Some("Max 5×"), Some(5.0))
    } else if org_type == Some("claude_pro") || tier == "default_claude_ai" || tier.ends_with("_pro") {
        (Some("Pro"), Some(1.0))
    } else {
        (None, None)
    };
    let org = match org_type {
        Some("claude_team") => Some("Team"),
        Some("claude_enterprise") => Some("Enterprise"),
        _ => None,
    };
    let label = match (org, size) {
        (Some(o), Some(s)) => format!("Claude {o} · {s}"),
        (Some(o), None) => format!("Claude {o}"),
        (None, Some(s)) => format!("Claude {s}"),
        (None, None) => match org_type {
            Some("claude_max") => "Claude Max".to_string(),
            _ => "Claude".to_string(),
        },
    };
    (label, cap)
}

// ───────────────────────── the card's lines ─────────────────────────

/// One account as the card draws it.
#[derive(Debug, Clone)]
pub struct Seat {
    pub name: String,
    pub plan: String,
    /// `on` · `next` · `full` · `out` (`cards/_agent/usage.ts`).
    pub state: &'static str,
    pub cap: f64,
    pub reading: Option<Reading>,
}

/// The card's lines for these accounts (already in the order turns try
/// them) and paces.
pub fn lines(seats: &[Seat], recent: Option<Pace>, week: Option<Pace>) -> String {
    let mut out = String::new();
    for s in seats {
        let at = s.reading.as_ref().map(|r| r.t).unwrap_or(0);
        out.push_str(&format!("acct|{}|{}|{}|{}|{at}\n", s.name, clean(&s.plan), s.state, num(s.cap)));
        for w in s.reading.iter().flat_map(|r| &r.windows) {
            out.push_str(&format!(
                "win|{}|{}|{}|{}|{}\n",
                s.name,
                w.kind,
                num(w.pct),
                w.resets.map(|r| r.to_string()).unwrap_or_default(),
                w.model.as_deref().unwrap_or(""),
            ));
        }
    }
    for (which, p) in [("recent", recent), ("week", week)] {
        if let Some(p) = p {
            out.push_str(&format!("pace|{which}|{}|{}|{}\n", num(p.work), num(p.wk), p.span));
        }
    }
    out
}

/// A number for a card line: at most three decimals, no trailing zeros.
fn num(v: f64) -> String {
    let s = format!("{:.3}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" { "0".into() } else { s.to_string() }
}

// ───────────────────────── gathering ─────────────────────────

/// What `/usage` asks of every account.
pub struct Gathered {
    /// `acct|` / `win|` / `pace|` lines.
    pub lines: String,
    /// The account the next turn runs on, when it was read live just now —
    /// the first look's limit rows (and its plan chip) come from it.
    pub in_use: Option<(Value, String)>,
}

/// Plans, remembered per account for as long as it holds the same login
/// (its credential's fingerprint), so `/usage` asks the profile once.
fn plans() -> &'static Mutex<HashMap<String, (Option<String>, String, Option<f64>)>> {
    static P: OnceLock<Mutex<HashMap<String, (Option<String>, String, Option<f64>)>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

async fn plan_for(acct: &crate::accounts::Account) -> (String, Option<f64>) {
    let fp = crate::commands::credential_fingerprint(&acct.env());
    if let Some((was, label, cap)) = plans().lock().unwrap_or_else(|p| p.into_inner()).get(&acct.name) {
        if *was == fp {
            return (label.clone(), *cap);
        }
    }
    let found = match crate::commands::login_identity(&acct.env()).await {
        crate::commands::WhoProbe::Known(id) => Some(plan_of(id.org_type.as_deref(), id.tier.as_deref())),
        _ => None,
    };
    match found {
        Some((label, cap)) => {
            plans().lock().unwrap_or_else(|p| p.into_inner()).insert(acct.name.clone(), (fp, label.clone(), cap));
            (label, cap)
        }
        // Couldn't ask (signed out, a lapsed token): unnamed until it can be.
        // Not `~/.claude.json`'s tier — that belongs to whichever login
        // signed in last, which is how a second seat used to wear the
        // first one's plan.
        None => ("Claude".into(), None),
    }
}

/// Every account, in the order a turn on `preferred` tries them, with the
/// same verdict the turn's chooser reaches (`crate::accounts::list_states`).
pub async fn gather(preferred: &str) -> Gathered {
    use crate::accounts::SeatState as S;
    let now = crate::accounts::now();
    let order: Vec<String> = crate::accounts::load().ordered(Some(preferred)).into_iter().map(|a| a.name).collect();
    let mut rows = crate::accounts::list_states().await;
    rows.sort_by_key(|(a, _, _)| order.iter().position(|n| *n == a.name).unwrap_or(usize::MAX));
    let plans = futures_util::future::join_all(rows.iter().map(|(a, _, _)| plan_for(a))).await;
    let known: Vec<f64> = plans.iter().filter_map(|(_, c)| *c).collect();
    // A plan nobody could name counts as the middle of the ones that could.
    let fallback = if known.is_empty() {
        1.0
    } else {
        let mut k = known.clone();
        k.sort_by(f64::total_cmp);
        k[k.len() / 2]
    };

    let mut seats: Vec<Seat> = Vec::new();
    let mut histories: Vec<Vec<Reading>> = Vec::new();
    let mut in_use: Option<usize> = None;
    let mut live_in_use: Option<(Value, String)> = None;
    for (i, ((acct, snap, _), (plan, cap))) in rows.iter().zip(plans).enumerate() {
        let out = matches!(snap.state, S::NoCredential | S::AuthRefused | S::CredentialUnreadable(_) | S::Rejected(_));
        let full = !out && snap.blocks(None).is_some();
        let state = if out {
            "out"
        } else if full {
            "full"
        } else if in_use.is_none() {
            in_use = Some(i);
            "on"
        } else {
            "next"
        };
        let mut history = read_history(&history_path(&acct.name));
        history.retain(|r| r.t >= now - KEEP);
        let live = snap.usage.as_ref().map(|(t, v)| Reading { t: *t, windows: windows_from_usage(v) });
        if let Some(r) = &live {
            if history.last().is_none_or(|last| r.t - last.t >= MIN_GAP) {
                history.push(r.clone());
            }
        }
        if state == "on" {
            live_in_use = snap.usage.as_ref().map(|(_, v)| (v.clone(), plan.clone()));
        }
        // Signed out: whatever it last said is about a login that isn't there.
        let reading = if out { None } else { live.or_else(|| history.last().cloned()) };
        seats.push(Seat { name: acct.name.clone(), plan, state, cap: cap.unwrap_or(fallback), reading });
        histories.push(history);
    }
    let tracks: Vec<Track> = seats.iter().zip(&histories).map(|(s, h)| Track { cap: s.cap, readings: h }).collect();
    let wk = week_ratio(&tracks, now);
    let r = recent(&tracks, in_use, wk, now);
    let w = week_average(&tracks, wk, now);
    Gathered { lines: lines(&seats, r, w), in_use: live_in_use }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_794_576_163; // 2026-11-13 13:22:43 UTC
    const R5: i64 = 1_794_582_000; // redq's 5-hour window ends 15:00 UTC
    const RW: i64 = 1_794_625_200; // its week, 11-14 03:00 UTC

    fn win(kind: &str, pct: f64, resets: Option<i64>) -> Window {
        Window { kind: kind.into(), pct, resets, model: None }
    }

    fn redq(t: i64, five: f64, week: f64) -> Reading {
        Reading { t, windows: vec![win("session", five, Some(R5)), win("weekly_all", week, Some(RW))] }
    }

    #[test]
    fn windows_come_from_the_usage_answer() {
        let v = serde_json::json!({ "limits": [
            { "kind": "session", "percent": 71.0, "resets_at": "2026-11-13T15:00:00Z" },
            { "kind": "weekly_scoped", "percent": 9.0, "resets_at": "2026-11-14T03:00:00Z", "scope": { "model": { "display_name": "Fa|ble" } } },
            { "kind": "weekly_all", "percent": "nope" },
        ]});
        let w = windows_from_usage(&v);
        assert_eq!(w.len(), 2, "a window without a number is skipped");
        assert_eq!(w[0].resets, Some(R5));
        // A name from upstream can't break a card line.
        assert_eq!(w[1].model.as_deref(), Some("Fable"));
    }

    #[test]
    fn history_round_trips_and_skips_readings_too_close() {
        let dir = std::env::temp_dir().join(format!("mafold-seat-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("redq.jsonl");
        record_at(&p, &redq(T0, 24.0, 64.0).windows, T0);
        record_at(&p, &redq(T0 + 30, 25.0, 64.0).windows, T0 + 30);
        record_at(&p, &redq(T0 + 600, 27.0, 65.0).windows, T0 + 600);
        std::fs::OpenOptions::new().append(true).open(&p).and_then(|mut f| {
            use std::io::Write;
            f.write_all(b"not json\n")
        }).unwrap();
        let h = read_history(&p);
        assert_eq!(h.iter().map(|r| r.t).collect::<Vec<_>>(), vec![T0, T0 + 600], "30 s apart is the same moment; junk lines skipped");
        assert_eq!(h[1], redq(T0 + 600, 27.0, 65.0));
        // Past its size the file keeps only the last eight days.
        let big = dir.join("big.jsonl");
        let old: Vec<String> = (0..6_000).map(|k| encode(&redq(T0 - 30 * 86_400 + k, 1.0, 1.0))).collect();
        std::fs::write(&big, old.join("\n") + "\n").unwrap();
        record_at(&big, &redq(T0, 2.0, 2.0).windows, T0);
        assert_eq!(read_history(&big).iter().map(|r| r.t).collect::<Vec<_>>(), vec![T0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_hours_pace_is_the_last_hour_on_every_account() {
        // redq 61% → 71% over the hour before 13:22; Max 5× = cap 5.
        let h = vec![redq(T0 - 3600, 61.0, 70.0), redq(T0 - 1800, 66.0, 71.0), redq(T0, 71.0, 72.0)];
        let p = recent(&[Track { cap: 5.0, readings: &h }], Some(0), 0.17, T0).unwrap();
        assert!((p.work - 50.0).abs() < 1e-9, "10% an hour on a cap-5 plan: {}", p.work);
        assert_eq!(p.span, 60);
        // A window that ended 20 minutes after the earlier reading, and a
        // new one that began half an hour ago: the new one counts in full.
        let before = Reading { t: T0 - 3600, windows: vec![win("session", 90.0, Some(T0 - 2400)), win("weekly_all", 72.0, Some(RW))] };
        let after = Reading { t: T0, windows: vec![win("session", 12.0, Some(T0 + FIVE_HOURS - 1800)), win("weekly_all", 73.0, Some(RW))] };
        let h2 = vec![before, after];
        let p2 = recent(&[Track { cap: 5.0, readings: &h2 }], Some(0), 0.17, T0).unwrap();
        assert!((p2.work - 60.0).abs() < 1e-9, "{}", p2.work);
    }

    #[test]
    fn with_no_history_the_running_window_stands_in() {
        // 71% three hours 23 minutes into the window, on a cap-5 plan: the
        // demo's 105 an hour.
        let h = vec![redq(T0, 71.0, 72.0)];
        let p = recent(&[Track { cap: 5.0, readings: &h }], Some(0), 0.17, T0).unwrap();
        assert!((p.work - 105.07).abs() < 0.01, "{}", p.work);
        assert_eq!(p.span, 202);
        // No window running on the account in use: nothing used lately.
        let idle = vec![Reading { t: T0, windows: vec![win("weekly_all", 22.0, Some(RW + 86_400))] }];
        assert_eq!(recent(&[Track { cap: 20.0, readings: &idle }], Some(0), 0.17, T0).unwrap().work, 0.0);
        // A stale reading is not "now".
        let old = vec![redq(T0 - 3 * 3600, 71.0, 72.0)];
        assert!(recent(&[Track { cap: 5.0, readings: &old }], None, 0.17, T0).is_none());
    }

    #[test]
    fn the_weeks_pace_and_its_ratio() {
        let h = vec![redq(T0, 71.0, 72.0)];
        let p = week_average(&[Track { cap: 5.0, readings: &h }], 0.17, T0).unwrap();
        // 72% over 154.4 hours, ×5, in 5-hour terms: the demo's 13.7.
        assert!((p.work - 13.72).abs() < 0.01, "{}", p.work);
        // Not enough movement: the default ratio.
        assert_eq!(week_ratio(&[Track { cap: 5.0, readings: &h }], T0), DEFAULT_WK);
        // Five-minute readings across two hours: 5-hour +48, week +8 ⇒ 1/6.
        let steps: Vec<Reading> = (0..=24).map(|k| redq(T0 - 7200 + k * 300, 23.0 + 2.0 * k as f64, 64.0 + (k / 3) as f64)).collect();
        let wk = week_ratio(&[Track { cap: 5.0, readings: &steps }], T0);
        assert!((wk - 8.0 / 48.0).abs() < 0.01, "{wk}");
    }

    #[test]
    fn plans_name_the_provider_and_their_room() {
        assert_eq!(plan_of(Some("claude_team"), Some("default_claude_max_5x")), ("Claude Team · Max 5×".into(), Some(5.0)));
        assert_eq!(plan_of(Some("claude_max"), Some("default_claude_max_20x")), ("Claude Max 20×".into(), Some(20.0)));
        assert_eq!(plan_of(Some("claude_pro"), None), ("Claude Pro".into(), Some(1.0)));
        assert_eq!(plan_of(None, None), ("Claude".into(), None));
    }

    #[test]
    fn the_lines_are_facts_in_order() {
        let seats = vec![
            Seat { name: "default".into(), plan: "Claude Max 20×".into(), state: "on", cap: 20.0, reading: Some(Reading { t: T0, windows: vec![win("weekly_all", 22.0, Some(1_794_729_600))] }) },
            Seat { name: "redq".into(), plan: "Claude Team · Max 5×".into(), state: "next", cap: 5.0, reading: Some(redq(T0, 71.0, 72.0)) },
            Seat { name: "old".into(), plan: "Claude".into(), state: "out", cap: 5.0, reading: None },
        ];
        let s = lines(&seats, Some(Pace { work: 105.0712, wk: 0.17, span: 60 }), None);
        assert_eq!(
            s,
            "acct|default|Claude Max 20×|on|20|1794576163\nwin|default|weekly_all|22|1794729600|\n\
             acct|redq|Claude Team · Max 5×|next|5|1794576163\nwin|redq|session|71|1794582000|\nwin|redq|weekly_all|72|1794625200|\n\
             acct|old|Claude|out|5|0\n\
             pace|recent|105.071|0.17|60\n"
        );
    }
}
