//! What needs a person on this machine — the one list `mafold status --json`
//! gives as `issues` (activity center C5). The computers card pins it as
//! «要你处理的事» and the desktop tray counts it; neither works it out again,
//! so the rules live here and only here.
//!
//! Codes only, no prose — each reader says it in its own words. One entry per
//! root cause, worst layer first, and what a deeper fault makes true is not
//! said again on top of it:
//!
//! | code | scope | what is wrong |
//! |---|---|---|
//! | `service_crash_loop` | machine | the supervisor keeps falling over (≥3 restarts after a crash in 10 minutes) |
//! | `service_stopped` | machine | the supervisor isn't running: no bot here answers |
//! | `mafold_signed_out` | account | Mafold refused this machine's login for `subject` (expired, or revoked) |
//! | `heartbeat_stale` | machine | Mafold hasn't taken this machine's report lately: it reads offline for `accounts` |
//! | `runtime_missing` | runtime | `subject` isn't installed, and `bots` run on it |
//! | `runtime_signed_out` | runtime | `subject` needs signing in again |
//! | `runtime_rejected` | runtime | `subject`'s account may not use it any more (signing in again won't help) |
//! | `runtime_exhausted` | runtime | `subject`'s usage is full, `until` it resets |
//! | `bot_crash_loop` | bot | `subject` keeps dying right after it starts |
//! | `bot_down` | bot | `subject` isn't running |
//! | `update_failing` | machine | installing `subject` (a newer mafold) keeps failing |
//!
//! With the supervisor down or crash-looping, only the logins Mafold refused
//! are said besides it: the rest is either its doing or can't be told while
//! it's down.

use crate::account_health::Health;
use crate::lifecycle::{Service, CRASH_LOOP, CRASH_WINDOW};
use mafold_core::mafold_types::computers::{HarnessInfo, SeatHealth};
use serde::Serialize;

/// The api's online window (`harness_api.rs` `ONLINE_WINDOW_SECS`): past it
/// without an accepted report, a machine reads offline for that account.
pub const ONLINE_WINDOW: i64 = 90;

/// How long a supervisor has to bring its daemons up before one that isn't
/// running is news — its loop starts any that are down every 10 seconds.
pub const BOT_GRACE: i64 = 30;

/// One Mafold login on this machine, as its heartbeat left it.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Login {
    pub username: String,
    /// The account commands act as unless told otherwise.
    pub current: bool,
    /// `online` — Mafold took a report inside the online window · `offline` —
    /// not lately · `signed_out` — Mafold refused the login · `unknown` — no
    /// report taken yet.
    pub state: &'static str,
    /// When Mafold last took this machine's report, epoch seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<i64>,
    /// When Mafold first refused it, epoch seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_out_at: Option<i64>,
}

impl Login {
    pub fn of(username: &str, current: bool, h: &Health, now: i64) -> Login {
        let state = match (&h.refused, h.ok_at) {
            (Some(_), _) => "signed_out",
            (None, Some(at)) if now - at <= ONLINE_WINDOW => "online",
            (None, Some(_)) => "offline",
            (None, None) => "unknown",
        };
        Login {
            username: username.to_string(),
            current,
            state,
            heartbeat_at: h.ok_at,
            signed_out_at: h.refused.as_ref().map(|r| r.at),
        }
    }
}

/// A configured bot, as far as [`issues`] needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Bot {
    pub name: String,
    pub harness: String,
    pub running: bool,
    /// When it died without anyone stopping it, epoch seconds.
    pub deaths: Vec<i64>,
}

/// An update that keeps failing to install.
#[derive(Debug, Clone, PartialEq)]
pub struct Failing {
    pub version: String,
    pub since: i64,
}

/// Everything [`issues`] decides from — gathered by `status`, so the rules
/// can be tested on their own.
pub struct Facts<'a> {
    pub now: i64,
    /// The supervisor's pid, when it's running.
    pub running: Option<u32>,
    pub service: &'a Service,
    pub logins: &'a [Login],
    pub bots: &'a [Bot],
    /// `harness::report_rows`, as the api would read them.
    pub runtimes: &'a [HarnessInfo],
    pub update: Option<&'a Failing>,
}

/// One thing that needs a person.
#[derive(Serialize, Debug, Clone, PartialEq, Default)]
pub struct Issue {
    pub code: &'static str,
    /// `machine` · `account` · `runtime` · `bot`.
    pub scope: &'static str,
    /// The account, runtime, bot or version it is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The bots it stops from answering.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bots: Vec<String>,
    /// The logins it leaves this machine offline for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<String>,
    /// Since when, epoch seconds, where that is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// `runtime_exhausted`: when it can be used again, epoch seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

/// The order runtime problems are said in when several runtimes have one.
const RUNTIME_ORDER: [&str; 4] = ["runtime_missing", "runtime_signed_out", "runtime_rejected", "runtime_exhausted"];

/// What needs a person here, worst layer first — see the module doc.
pub fn issues(f: &Facts) -> Vec<Issue> {
    let mut out = Vec::new();
    // No login and no bot: this machine isn't set up yet. That's a first run
    // to walk through, not something broken.
    if f.logins.is_empty() && f.bots.is_empty() {
        return out;
    }
    let all_bots = || f.bots.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
    let service_down = if f.service.crashes.len() >= CRASH_LOOP {
        out.push(Issue {
            code: "service_crash_loop",
            scope: "machine",
            bots: all_bots(),
            since: f.service.crashes.iter().copied().min(),
            ..Default::default()
        });
        true
    } else if f.running.is_none() {
        out.push(Issue {
            code: "service_stopped",
            scope: "machine",
            bots: all_bots(),
            since: f.service.last_exit.as_ref().and_then(|x| x.at),
            ..Default::default()
        });
        true
    } else {
        false
    };

    // A login Mafold refused is its own cause, whatever the supervisor does:
    // it takes signing that account in again.
    for l in f.logins.iter().filter(|l| l.state == "signed_out") {
        out.push(Issue {
            code: "mafold_signed_out",
            scope: "account",
            subject: Some(l.username.clone()),
            since: l.signed_out_at,
            ..Default::default()
        });
    }
    if service_down {
        return out;
    }

    // Mafold hasn't taken this machine's report lately: one cause — the way
    // to Mafold — however many logins it leaves offline. A supervisor that
    // just started gets the online window to send its first. One from before
    // these records (no run on file) doesn't write down what Mafold answers
    // it either, so its heartbeat can't be judged from here.
    let up_since = f.service.run.as_ref().map(|r| r.started_at);
    let stale: Vec<&Login> = f
        .logins
        .iter()
        .filter(|_| up_since.is_some())
        .filter(|l| l.state != "signed_out")
        .filter(|l| l.heartbeat_at.max(up_since).is_some_and(|t| f.now - t > ONLINE_WINDOW))
        .collect();
    if !stale.is_empty() {
        out.push(Issue {
            code: "heartbeat_stale",
            scope: "machine",
            accounts: stale.iter().map(|l| l.username.clone()).collect(),
            since: stale.iter().filter_map(|l| l.heartbeat_at).max(),
            ..Default::default()
        });
    }

    // A runtime that stops bots: one row per runtime, naming the bots on it.
    // A runtime no bot here uses stops nobody (the computers card's rule).
    let mut used: Vec<&str> = Vec::new();
    for b in f.bots {
        if !used.contains(&b.harness.as_str()) {
            used.push(&b.harness);
        }
    }
    let mut runtime = Vec::new();
    for id in used {
        let row = f.runtimes.iter().find(|h| h.id == id);
        let (code, until) = match row {
            Some(h) if h.available => match h.health.as_ref().map(|s| s.state.as_str()) {
                Some("unauthenticated") => ("runtime_signed_out", None),
                Some("rejected") => ("runtime_rejected", None),
                Some("exhausted") => ("runtime_exhausted", resets_at(h.health.as_ref())),
                // `unreachable` / `unknown` say nothing sure about the seat.
                _ => continue,
            },
            _ => ("runtime_missing", None),
        };
        runtime.push(Issue {
            code,
            scope: "runtime",
            subject: Some(id.to_string()),
            bots: f.bots.iter().filter(|b| b.harness == id).map(|b| b.name.clone()).collect(),
            until,
            ..Default::default()
        });
    }
    runtime.sort_by_key(|i| RUNTIME_ORDER.iter().position(|c| *c == i.code));
    let missing: Vec<String> = runtime.iter().filter(|i| i.code == "runtime_missing").flat_map(|i| i.bots.clone()).collect();
    out.extend(runtime);

    // A bot that won't stay up — once the supervisor has had time to start
    // it. A bot whose runtime isn't installed is that runtime's row already.
    if up_since.is_none_or(|t| f.now - t >= BOT_GRACE) {
        let mut bots = Vec::new();
        for b in f.bots.iter().filter(|b| !missing.contains(&b.name)) {
            let deaths: Vec<i64> = b.deaths.iter().copied().filter(|t| f.now - t < CRASH_WINDOW).collect();
            let (code, since) = if deaths.len() >= CRASH_LOOP {
                ("bot_crash_loop", deaths.first().copied())
            } else if !b.running {
                // Since it last died, when that was lately; an older death
                // says nothing about why it's down now.
                ("bot_down", deaths.last().copied())
            } else {
                continue;
            };
            bots.push(Issue { code, scope: "bot", subject: Some(b.name.clone()), since, ..Default::default() });
        }
        // Worst first across bots too; the config's order within a code.
        bots.sort_by_key(|i| i.code != "bot_crash_loop");
        out.extend(bots);
    }

    if let Some(u) = f.update {
        out.push(Issue {
            code: "update_failing",
            scope: "machine",
            subject: Some(u.version.clone()),
            since: Some(u.since),
            ..Default::default()
        });
    }
    out
}

/// When an exhausted seat can be used again (epoch seconds): every full
/// window has to reset, so the latest reset among the full ones; when none
/// says it's full, the nearest reset there is — the computers card's
/// `resetsAt`, so both say the same time.
pub fn resets_at(h: Option<&SeatHealth>) -> Option<i64> {
    let limits: Vec<_> = h.map(|h| h.limits.iter().filter(|l| l.resets_at.is_some()).collect()).unwrap_or_default();
    let full = limits.iter().filter(|l| l.percent >= 100.0).filter_map(|l| l.resets_at).max();
    full.or_else(|| limits.iter().filter_map(|l| l.resets_at).min())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_health::Refused;
    use crate::lifecycle::{Exit, Run, Why};

    const NOW: i64 = 100_000;

    fn up(started_at: i64) -> Service {
        Service {
            run: Some(Run { pid: 7, started_at, version: "0.9.150".into(), auto_update: true, crashes: vec![] }),
            last_exit: None,
            crashes: vec![],
        }
    }

    fn login(name: &str, ok_at: Option<i64>, refused_at: Option<i64>) -> Login {
        let h = Health { ok_at, refused: refused_at.map(|at| Refused { at, token: "fp".into() }) };
        Login::of(name, false, &h, NOW)
    }

    fn bot(name: &str, harness: &str, running: bool) -> Bot {
        Bot { name: name.into(), harness: harness.into(), running, deaths: vec![] }
    }

    fn runtime(id: &str, available: bool, state: Option<&str>) -> HarnessInfo {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "available": available,
            "version": "1.0",
            "health": state.map(|s| serde_json::json!({ "state": s, "limits": [], "checked_at": NOW })),
        }))
        .unwrap()
    }

    fn codes(f: &Facts) -> Vec<(&'static str, Option<String>)> {
        issues(f).into_iter().map(|i| (i.code, i.subject)).collect()
    }

    fn facts<'a>(
        running: bool,
        service: &'a Service,
        logins: &'a [Login],
        bots: &'a [Bot],
        runtimes: &'a [HarnessInfo],
    ) -> Facts<'a> {
        Facts { now: NOW, running: running.then_some(7), service, logins, bots, runtimes, update: None }
    }

    #[test]
    fn all_well_is_an_empty_list() {
        let s = up(NOW - 3600);
        let logins = [login("ops", Some(NOW - 10), None)];
        let bots = [bot("ops:a", "claude-code", true)];
        let rt = [runtime("claude-code", true, Some("ok"))];
        assert_eq!(issues(&facts(true, &s, &logins, &bots, &rt)), vec![]);
    }

    #[test]
    fn a_machine_not_set_up_yet_has_nothing_to_fix() {
        let s = Service::default();
        assert_eq!(issues(&facts(false, &s, &[], &[], &[])), vec![]);
    }

    /// The C5 acceptance rule: with the supervisor stopped, no bot-level
    /// issue — and no runtime or heartbeat one either: no bot runs, and
    /// Mafold can't hear a machine whose supervisor is down.
    #[test]
    fn a_stopped_supervisor_hides_everything_it_explains() {
        let s = Service {
            run: None,
            last_exit: Some(Exit { pid: 7, reason: Why::Stopped, at: Some(NOW - 60), version: "v".into(), detail: None }),
            crashes: vec![],
        };
        let logins = [login("ops", Some(NOW - 900), None)];
        let bots = [bot("ops:a", "claude-code", false), bot("ops:b", "codex", false)];
        let rt = [runtime("claude-code", true, Some("unauthenticated"))];
        let got = issues(&facts(false, &s, &logins, &bots, &rt));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].code, "service_stopped");
        assert_eq!(got[0].bots, vec!["ops:a", "ops:b"]);
        assert_eq!(got[0].since, Some(NOW - 60));
    }

    #[test]
    fn a_refused_login_is_said_even_with_the_supervisor_down() {
        let s = Service::default();
        let logins = [login("ops", Some(NOW - 900), Some(NOW - 800)), login("fei", Some(NOW - 900), None)];
        let got = codes(&facts(false, &s, &logins, &[], &[]));
        assert_eq!(got, vec![("service_stopped", None), ("mafold_signed_out", Some("ops".into()))]);
    }

    #[test]
    fn a_crash_loop_is_one_row_and_outranks_down() {
        let s = Service { run: None, last_exit: None, crashes: vec![NOW - 300, NOW - 200, NOW - 100] };
        let bots = [bot("ops:a", "claude-code", false)];
        let got = issues(&facts(false, &s, &[], &bots, &[]));
        assert_eq!(got.iter().map(|i| i.code).collect::<Vec<_>>(), vec!["service_crash_loop"]);
        assert_eq!(got[0].since, Some(NOW - 300));
        // Two restarts after a crash aren't a loop yet.
        let s = Service { crashes: vec![NOW - 200, NOW - 100], ..up(NOW - 100) };
        assert_eq!(codes(&facts(true, &s, &[], &bots, &[runtime("claude-code", true, None)])), vec![("bot_down", Some("ops:a".into()))]);
    }

    #[test]
    fn stale_heartbeats_are_one_cause_and_a_new_supervisor_gets_its_window() {
        let logins = [login("ops", Some(NOW - 600), None), login("fei", Some(NOW - 400), None), login("x", None, Some(NOW - 5))];
        // Up for an hour: both readable logins are offline — one row.
        let s = up(NOW - 3600);
        let got = issues(&facts(true, &s, &logins, &[], &[]));
        let stale = got.iter().find(|i| i.code == "heartbeat_stale").unwrap();
        assert_eq!(stale.accounts, vec!["ops", "fei"], "the refused login is its own row, not stale");
        assert_eq!(stale.since, Some(NOW - 400), "when Mafold last heard from this machine");
        assert_eq!(got.iter().filter(|i| i.code == "mafold_signed_out").count(), 1);
        // Started 20 seconds ago: its first report may not be in yet.
        let s = up(NOW - 20);
        assert!(!codes(&facts(true, &s, &logins, &[], &[])).iter().any(|(c, _)| *c == "heartbeat_stale"));
    }

    #[test]
    fn a_runtime_problem_is_one_row_for_the_bots_on_it_and_only_if_a_bot_uses_it() {
        let s = up(NOW - 3600);
        let bots = [bot("ops:a", "claude-code", true), bot("ops:b", "claude-code", true), bot("ops:c", "codex", true)];
        let rt = [
            runtime("claude-code", true, Some("exhausted")),
            runtime("codex", true, Some("unauthenticated")),
            // Signed out, but no bot here runs on it: it stops nobody.
            runtime("opencode", true, Some("unauthenticated")),
        ];
        let got = issues(&facts(true, &s, &[], &bots, &rt));
        assert_eq!(
            got.iter().map(|i| (i.code, i.subject.as_deref().unwrap(), i.bots.clone())).collect::<Vec<_>>(),
            vec![
                ("runtime_signed_out", "codex", vec!["ops:c".to_string()]),
                ("runtime_exhausted", "claude-code", vec!["ops:a".to_string(), "ops:b".to_string()]),
            ]
        );
        // Unreachable / unknown say nothing sure about the seat.
        let rt = [runtime("claude-code", true, Some("unreachable")), runtime("codex", true, Some("unknown"))];
        assert_eq!(issues(&facts(true, &s, &[], &bots, &rt)), vec![]);
    }

    #[test]
    fn a_missing_runtime_covers_its_bots_not_running() {
        let s = up(NOW - 3600);
        let bots = [bot("ops:a", "codex", false), bot("ops:b", "claude-code", false)];
        let rt = [runtime("claude-code", true, Some("ok")), runtime("codex", false, None)];
        assert_eq!(
            codes(&facts(true, &s, &[], &bots, &rt)),
            vec![("runtime_missing", Some("codex".into())), ("bot_down", Some("ops:b".into()))]
        );
    }

    #[test]
    fn bots_get_the_grace_period_and_a_crash_loop_beats_down() {
        let rt = [runtime("claude-code", true, Some("ok"))];
        let mut looping = bot("ops:a", "claude-code", true);
        looping.deaths = vec![NOW - 2000, NOW - 300, NOW - 200, NOW - 100];
        let mut died_once = bot("ops:b", "claude-code", false);
        died_once.deaths = vec![NOW - 50];
        let mut died_long_ago = bot("ops:c", "claude-code", false);
        died_long_ago.deaths = vec![NOW - 5 * 86_400];
        // The down ones come first in the config; the loop is still said first.
        let bots = [died_once, died_long_ago, looping];
        let s = up(NOW - 3600);
        let got = issues(&facts(true, &s, &[], &bots, &rt));
        assert_eq!(
            got.iter().map(|i| (i.code, i.subject.as_deref().unwrap(), i.since)).collect::<Vec<_>>(),
            vec![
                ("bot_crash_loop", "ops:a", Some(NOW - 300)),
                ("bot_down", "ops:b", Some(NOW - 50)),
                ("bot_down", "ops:c", None),
            ]
        );
        // Ten seconds after the supervisor started: not news yet.
        let s = up(NOW - 10);
        assert_eq!(issues(&facts(true, &s, &[], &bots, &rt)), vec![]);
    }

    /// A supervisor from before these records doesn't write down what Mafold
    /// answers its heartbeat: a login's last `mafold report` is no measure.
    #[test]
    fn a_supervisor_without_records_is_not_judged_on_its_heartbeat() {
        let s = Service::default(); // running, nothing on file
        let logins = [login("ops", Some(NOW - 3600), None)];
        assert_eq!(issues(&facts(true, &s, &logins, &[], &[])), vec![]);
    }

    #[test]
    fn a_failing_update_is_said_last() {
        let s = up(NOW - 3600);
        let bots = [bot("ops:a", "claude-code", false)];
        let rt = [runtime("claude-code", true, Some("ok"))];
        let failing = Failing { version: "0.9.151".into(), since: NOW - 900 };
        let f = Facts { update: Some(&failing), ..facts(true, &s, &[], &bots, &rt) };
        assert_eq!(codes(&f), vec![("bot_down", Some("ops:a".into())), ("update_failing", Some("0.9.151".into()))]);
    }

    #[test]
    fn login_states() {
        assert_eq!(login("a", Some(NOW - 30), None).state, "online");
        assert_eq!(login("a", Some(NOW - 91), None).state, "offline");
        assert_eq!(login("a", None, None).state, "unknown");
        let out = login("a", Some(NOW - 30), Some(NOW - 5));
        assert_eq!((out.state, out.signed_out_at, out.heartbeat_at), ("signed_out", Some(NOW - 5), Some(NOW - 30)));
    }

    #[test]
    fn the_reset_time_matches_the_cards() {
        let h: SeatHealth = serde_json::from_value(serde_json::json!({
            "state": "exhausted", "checked_at": 0,
            "limits": [
                { "kind": "five_hour", "label": "5h", "percent": 100.0, "resets_at": 500 },
                { "kind": "seven_day", "label": "7d", "percent": 100.0, "resets_at": 900 },
                { "kind": "opus", "label": "Opus", "percent": 40.0, "resets_at": 100 },
            ]
        }))
        .unwrap();
        assert_eq!(resets_at(Some(&h)), Some(900), "every full window has to reset");
        let h: SeatHealth = serde_json::from_value(serde_json::json!({
            "state": "exhausted", "checked_at": 0,
            "limits": [{ "kind": "a", "label": "a", "percent": 90.0, "resets_at": 700 }, { "kind": "b", "label": "b", "percent": 80.0, "resets_at": 300 }]
        }))
        .unwrap();
        assert_eq!(resets_at(Some(&h)), Some(300));
        assert_eq!(resets_at(None), None);
    }
}
