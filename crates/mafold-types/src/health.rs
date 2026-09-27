//! Health — one vocabulary for «something that should have happened didn't».
//!
//! 2026-09-27, Notion P1「失败不可见」: eight failures in three weeks (an update
//! check rate-limited for four days, messages steered into a turn whose process
//! was gone, a reminder job that crashed nine days running, a collector hung for
//! three and a half days, a daemon offline for seven hours behind a bubble that
//! only said «No signal», …). None was an uncaught error. Every one was caught
//! and then died somewhere nobody looks: a local log without timestamps, a
//! launchd exit code, the model's context. And the side that failed was, each
//! time, the side responsible for saying so.
//!
//! So the judging moves to the side that is still up — the server — and judges
//! by absence against a declared expectation, not by waiting for an error
//! signal that a hung, killed or never-started process cannot send. Everything
//! reports into ONE ledger, every surface reads from it, and every surface says
//! it with ONE closed set of reasons (below) whose words live in the one
//! language pack. See `.docs/failure-visibility-v1.md`.
//!
//! A `subject` is only a key. Nothing renders differently by what kind of
//! subject it is — a state and a reason are all a surface ever looks at.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How a subject is doing. Graded on purpose: «not authorized», «not ready» and
/// «working on it» are three different things to tell a person, and the one
/// sentence that used to cover all three (`dictation offline`) is item ⑤ of the
/// P1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Ok,
    /// In progress, nothing to act on.
    Working,
    /// Blocked on the person (a login, an approval, a permission).
    Waiting,
    /// Running, but not as it should.
    Degraded,
    /// It said it failed.
    Failed,
    /// It said nothing when it should have. Only the server can judge this —
    /// absence is not something the absent side can report.
    Silent,
}

impl HealthState {
    /// Something the owner should hear about — including `waiting`, which is
    /// the one only they can fix.
    pub fn wants_attention(self) -> bool {
        matches!(self, Self::Waiting | Self::Degraded | Self::Failed | Self::Silent)
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Silent => "silent",
        }
    }

    /// `health.state.<code>` in the language pack.
    pub fn lang_key(self) -> String {
        format!("health.state.{}", self.code())
    }
}

/// Why. A closed set: a surface never shows a free-text error as the headline
/// (that is how «not logged in» came to mean «HOME is not set»); the raw text
/// rides along in [`Health::detail`].
///
/// Adding a reason = a variant here + `health.reason.<code>` in both packs (the
/// i18n gate checks the second half).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthReason {
    // ── said by whoever failed (`reportHealth`) ──────────────────────────
    RateLimited,
    ChecksumMismatch,
    NotLoggedIn,
    EnvHomeMissing,
    NotAuthorized,
    NotReady,
    JobFailed,
    JobTimeout,
    JobNeverStarted,
    ToolCallTruncated,
    // ── seen only by the server ─────────────────────────────────────────
    /// Declared an interval, then went quiet past it.
    JobMissed,
    /// A reply is open and the machine producing it can't reach us.
    MachineOffline,
    /// A reply is open, its machine is connected, nothing has been written.
    TurnStalled,
    /// A machine keeps reporting an older mafold than the one published.
    UpdateStuck,
    /// A count that is normally >0 came back 0 — a failure that looks like «none».
    CountDroppedToZero,
    /// This process restarted in the middle of a reply it was writing.
    ServerRestart,
    RoutineSendFailed,
    RoutineUndeliverable,
    RoutineOwnerGone,
    RoutineOccurrenceStale,
}

impl HealthReason {
    pub const ALL: &'static [HealthReason] = &[
        Self::RateLimited,
        Self::ChecksumMismatch,
        Self::NotLoggedIn,
        Self::EnvHomeMissing,
        Self::NotAuthorized,
        Self::NotReady,
        Self::JobFailed,
        Self::JobTimeout,
        Self::JobNeverStarted,
        Self::ToolCallTruncated,
        Self::JobMissed,
        Self::MachineOffline,
        Self::TurnStalled,
        Self::UpdateStuck,
        Self::CountDroppedToZero,
        Self::ServerRestart,
        Self::RoutineSendFailed,
        Self::RoutineUndeliverable,
        Self::RoutineOwnerGone,
        Self::RoutineOccurrenceStale,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::NotLoggedIn => "not_logged_in",
            Self::EnvHomeMissing => "env_home_missing",
            Self::NotAuthorized => "not_authorized",
            Self::NotReady => "not_ready",
            Self::JobFailed => "job_failed",
            Self::JobTimeout => "job_timeout",
            Self::JobNeverStarted => "job_never_started",
            Self::ToolCallTruncated => "tool_call_truncated",
            Self::JobMissed => "job_missed",
            Self::MachineOffline => "machine_offline",
            Self::TurnStalled => "turn_stalled",
            Self::UpdateStuck => "update_stuck",
            Self::CountDroppedToZero => "count_dropped_to_zero",
            Self::ServerRestart => "server_restart",
            Self::RoutineSendFailed => "routine_send_failed",
            Self::RoutineUndeliverable => "routine_undeliverable",
            Self::RoutineOwnerGone => "routine_owner_gone",
            Self::RoutineOccurrenceStale => "routine_occurrence_stale",
        }
    }

    /// `health.reason.<code>` in the language pack.
    pub fn lang_key(self) -> String {
        format!("health.reason.{}", self.code())
    }

    /// Reasons only the server can observe — absence, and its own insides. A
    /// report claiming one is refused: a machine cannot know it went offline.
    pub fn server_only(self) -> bool {
        matches!(
            self,
            Self::JobMissed
                | Self::MachineOffline
                | Self::TurnStalled
                | Self::UpdateStuck
                | Self::CountDroppedToZero
                | Self::ServerRestart
                | Self::RoutineSendFailed
                | Self::RoutineUndeliverable
                | Self::RoutineOwnerGone
                | Self::RoutineOccurrenceStale
        )
    }
}

/// Subject prefixes the server keeps for what it judges itself. A report can't
/// write into them — `run:<draft>` is about a reply, and whether a reply's
/// machine is gone is exactly what that machine can't tell us.
pub const SERVER_SUBJECT_PREFIXES: &[&str] = &["run:", "routine:", "delivery:"];

pub const SUBJECT_MAX: usize = 120;
pub const TITLE_MAX: usize = 80;
pub const DETAIL_MAX: usize = 4000;
pub const COUNTS_MAX: usize = 16;
pub const COUNT_NAME_MAX: usize = 40;
/// Shortest and longest interval a job may promise to report in.
pub const EXPECT_EVERY_MIN_SECS: u64 = 60;
pub const EXPECT_EVERY_MAX_SECS: u64 = 31 * 86_400;

/// One report, from anyone: a supervisor, a daemon, `mafold job run`, a script.
/// Same shape for a person and a bot (§1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthReport {
    /// A stable key in the reporter's own namespace: `job:hot-collector`,
    /// `device:<id>/update`, …
    pub subject: String,
    pub state: HealthState,
    /// Required when `state` wants attention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<HealthReason>,
    /// What a person calls it («社媒热点采集»). Falls back to the subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The raw error, for «details». Never the headline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// «You will hear from me at least this often.» What turns silence into a
    /// verdict (`job_missed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_every_secs: Option<u64>,
    /// Named counts from this run (`youtube=37`). A count that has been >0 and
    /// comes back 0 is judged `count_dropped_to_zero` — «nothing» and «broke»
    /// look the same from the inside.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, u64>,
}

impl HealthReport {
    /// Everything the server refuses, in one place. `Err` is the reason, in
    /// words a developer reads.
    pub fn validate(&self) -> Result<(), String> {
        let s = self.subject.as_str();
        if s.is_empty() || s.len() > SUBJECT_MAX {
            return Err(format!("subject must be 1–{SUBJECT_MAX} bytes"));
        }
        if !s.chars().all(|c| c.is_ascii_alphanumeric() || ":_./@-".contains(c)) {
            return Err("subject may only use A–Z a–z 0–9 and : _ . / @ -".into());
        }
        if let Some(p) = SERVER_SUBJECT_PREFIXES.iter().find(|p| s.starts_with(**p)) {
            return Err(format!("`{p}` subjects are the server's own"));
        }
        if self.state == HealthState::Silent {
            return Err("`silent` is a verdict the server reaches, not something a reporter can say".into());
        }
        match self.reason {
            Some(r) if r.server_only() => {
                return Err(format!("`{}` is only observable by the server", r.code()));
            }
            None if self.state.wants_attention() => {
                return Err(format!("state `{}` needs a reason", self.state.code()));
            }
            _ => {}
        }
        if self.title.as_deref().is_some_and(|t| t.chars().count() > TITLE_MAX) {
            return Err(format!("title is at most {TITLE_MAX} characters"));
        }
        if self.detail.as_deref().is_some_and(|d| d.len() > DETAIL_MAX) {
            return Err(format!("detail is at most {DETAIL_MAX} bytes"));
        }
        if let Some(e) = self.expect_every_secs {
            if !(EXPECT_EVERY_MIN_SECS..=EXPECT_EVERY_MAX_SECS).contains(&e) {
                return Err(format!(
                    "expect_every_secs must be {EXPECT_EVERY_MIN_SECS}–{EXPECT_EVERY_MAX_SECS}"
                ));
            }
        }
        if self.counts.len() > COUNTS_MAX {
            return Err(format!("at most {COUNTS_MAX} counts"));
        }
        for k in self.counts.keys() {
            if k.is_empty()
                || k.len() > COUNT_NAME_MAX
                || !k.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            {
                return Err(format!("count name {k:?} must be 1–{COUNT_NAME_MAX} of A–Z a–z 0–9 _ . -"));
            }
        }
        Ok(())
    }
}

/// A ledger entry as the owner sees it (`listHealth`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Health {
    pub subject: String,
    /// The account the subject belongs to (lowercased) — the caller, or one of
    /// the caller's bots.
    pub owner: String,
    /// Display-ready (§3): the reporter's title, or the server's words for what
    /// it judged, in the owner's language.
    pub title: String,
    pub state: HealthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<HealthReason>,
    /// Display-ready sentence for `reason`, in the owner's language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_every_secs: Option<u64>,
    /// When the current state began.
    pub since: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportHealthParams {
    pub items: Vec<HealthReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthList {
    pub items: Vec<Health>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(subject: &str) -> HealthReport {
        HealthReport {
            subject: subject.into(),
            state: HealthState::Ok,
            reason: None,
            title: None,
            detail: None,
            expect_every_secs: None,
            counts: BTreeMap::new(),
        }
    }

    #[test]
    fn every_reason_has_a_distinct_code() {
        let mut seen = std::collections::HashSet::new();
        for r in HealthReason::ALL {
            assert!(seen.insert(r.code()), "{} twice", r.code());
            let json = serde_json::to_string(r).unwrap();
            assert_eq!(json, format!("\"{}\"", r.code()), "serde and code() disagree");
        }
    }

    #[test]
    fn a_reporter_cannot_claim_what_only_the_server_sees() {
        let silent = HealthReport { state: HealthState::Silent, reason: Some(HealthReason::JobFailed), ..ok("job:a") };
        assert!(silent.validate().is_err());
        let offline = HealthReport { state: HealthState::Failed, reason: Some(HealthReason::MachineOffline), ..ok("job:a") };
        assert!(offline.validate().is_err());
        assert!(ok("run:0f0f").validate().is_err(), "run: belongs to the server");
        assert!(ok("routine:x").validate().is_err());
    }

    #[test]
    fn a_bad_state_needs_a_reason_and_a_good_one_does_not() {
        let bare = HealthReport { state: HealthState::Failed, ..ok("job:a") };
        assert!(bare.validate().is_err());
        let said = HealthReport { state: HealthState::Failed, reason: Some(HealthReason::JobTimeout), ..ok("job:a") };
        assert!(said.validate().is_ok());
        assert!(ok("job:hot-collector").validate().is_ok());
        assert!(ok("device:MacBook-Pro-3/update").validate().is_ok());
    }

    #[test]
    fn subjects_intervals_and_counts_are_bounded() {
        assert!(ok("").validate().is_err());
        assert!(ok("job:has space").validate().is_err());
        assert!(ok(&"x".repeat(SUBJECT_MAX + 1)).validate().is_err());
        let fast = HealthReport { expect_every_secs: Some(5), ..ok("job:a") };
        assert!(fast.validate().is_err());
        let daily = HealthReport { expect_every_secs: Some(86_400), ..ok("job:a") };
        assert!(daily.validate().is_ok());
        let mut counts = ok("job:a");
        counts.counts.insert("you tube".into(), 1);
        assert!(counts.validate().is_err());
    }
}
