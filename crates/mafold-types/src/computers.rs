//! A person's computers and what their agents are doing on them — the wire
//! shapes behind the activity center's 电脑 tab, New-Bot's device picker,
//! presence, and the garden's "who is working where".
//!
//! One place for them (宪法: a wire format has ONE source): until now each lived
//! next to the api handler that first needed it (`store.rs`, `harness_api.rs`,
//! `chat_api.rs`, `apps_api.rs`), so a field added for one surface had no path
//! to the others. Fields grow HERE, additively — every shape below is already
//! stored or already on the wire, and the golden tests at the bottom pin the
//! bytes so a move or a new optional field can never quietly change them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ───────────────── a computer's report (mafold-cli → api) ─────────────────

/// One rate-limit window on a subscription seat, as reported by the host
/// machine. Normalized by mafold-cli across harnesses; opaque here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatLimit {
    /// Harness-native window id (`"session"`, `"weekly_all"`, …). Used for
    /// grouping only — the server never branches on it.
    pub kind: String,
    pub label: String,
    /// 0–100.
    pub percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

/// Liveness of the subscription seat behind a harness — "is this seat usable
/// right now", as opposed to `HarnessInfo::available` ("is the CLI installed").
///
/// ⚠️ SELF-REPORTED by the host machine, which mafold does not control. This is
/// operational telemetry so a seat's owner can see their own credential expire,
/// their account lose eligibility, or their window fill up. It is **never** an
/// authorization input (§15) and **never** billing evidence — anything that
/// spends or grants must be decided server-side from data the server holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatHealth {
    /// `ok` | `exhausted` | `unauthenticated` | `rejected` | `unreachable` |
    /// `unknown`. Stored as a string, not an enum: a CLI newer than the api
    /// must be able to report a state this build has never heard of without the
    /// whole report failing to deserialize.
    pub state: String,
    /// Upstream HTTP status behind `state`, when the probe got one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Plan/tier label the seat reports ("Max (20x)").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Rate-limit windows, most-occupied first.
    #[serde(default)]
    pub limits: Vec<SeatLimit>,
    /// Epoch seconds the probe ran (may lag `reported_at` — a stale probe on a
    /// fresh heartbeat is itself a signal).
    pub checked_at: i64,
}

/// One login behind a harness on a host — a machine can hold several Claude
/// accounts for the same `claude` (mafold-cli `accounts.rs`), and each one is
/// its own seat with its own windows. Reported side by side so the owner can
/// see which of their logins is the one that is out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSeat {
    /// The host's name for it (`default` is the machine's own login).
    pub name: String,
    /// The email the login reported, when the host knows it. Display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<SeatHealth>,
}

/// One coding-agent harness as reported by a logged-in mafold-cli host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessInfo {
    /// Stable id: `"claude-code" | "opencode" | "codex" | "openclaw"`.
    pub id: String,
    /// Installed / runnable on that machine right now.
    pub available: bool,
    #[serde(default)]
    pub version: Option<String>,
    /// Seat liveness, when the reporting CLI probes it (≥0.9.86). `None` from
    /// older CLIs and for harnesses that aren't installed — which is why this is
    /// `Option` rather than a `SeatHealth::unknown()` default: "this build never
    /// reported health" and "this build looked and couldn't tell" are different
    /// facts, and only the second one means anything about the seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<SeatHealth>,
    /// Every login the host holds for this harness, each with its own seat
    /// health (≥0.9.109). `health` above is the seat a turn would land on
    /// right now — the first healthy one — so a reader that only knows the
    /// single-seat shape still gets the answer that matters. Empty from older
    /// CLIs and for harnesses that have no account registry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<AccountSeat>,
}

/// A mafold-cli device's harness capability report (the control plane). Keyed by
/// (account_lc, device_id). Freshness of `reported_at` → the device's "online".
///
/// This is also the STORED row (`store_kv` kind `device_harness`), so every
/// field added later must be `#[serde(default)]`: rows written by older builds
/// have to keep loading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceHarnesses {
    pub device_id: String,
    pub device_name: String,
    pub harnesses: Vec<HarnessInfo>,
    /// The mafold-cli version running on the device (e.g. "0.9.60") — shown
    /// next to the machine name in New-Bot. `default` for pre-0.9.60 reports.
    #[serde(default)]
    pub cli_version: Option<String>,
    /// Epoch seconds of the last report.
    pub reported_at: i64,
}

impl DeviceHarnesses {
    /// Keep what the last report knew that this one doesn't say.
    ///
    /// A cli before C1 reported in two sizes: a full report with each
    /// harness's seat health and logins, and the 30-second heartbeat that
    /// carried only id / installed / version (mafold-cli `supervisor.rs`;
    /// from C1 the heartbeat is the full report). Storing the
    /// heartbeat as the whole row wiped the health a full report had just
    /// written, so «Claude Code needs signing in again» lived half a minute
    /// (activity center K2). Per harness: a report that says nothing about
    /// health or logins keeps the previous ones — as long as it is the same
    /// harness, still installed, at the same version. A new version is a new
    /// program whose login nobody has looked at yet: it starts with nothing.
    pub fn carry_forward(&mut self, prev: &DeviceHarnesses) {
        for h in self.harnesses.iter_mut() {
            if !h.available || (h.health.is_some() || !h.accounts.is_empty()) {
                continue;
            }
            let Some(p) = prev.harnesses.iter().find(|p| p.id == h.id) else { continue };
            if p.available && p.version == h.version {
                h.health = p.health.clone();
                h.accounts = p.accounts.clone();
            }
        }
    }
}

// ───────────────── the computers list (`listHarnessHosts`) ─────────────────

/// One of the caller's computers, as `listHarnessHosts` answers it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessHost {
    pub device_id: String,
    pub device_name: String,
    pub harnesses: Vec<HarnessInfo>,
    /// The device's mafold-cli version, when it reports one (≥0.9.60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    /// Reported within the freshness window (the cli heartbeats periodically).
    pub online: bool,
    pub reported_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessHostsResult {
    pub items: Vec<HarnessHost>,
}

// ───────────────── presence (`getUserStatus`) ─────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStatus {
    pub username: String,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
}

// ───────────────── an agent at work (`getAppViewerProfile`) ─────────────────

/// One of the person's agents, as a page the person opened sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerAgent {
    pub username: String,
    pub display_name: String,
    /// The agent's own one-paragraph introduction (its profile bio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    pub online: bool,
    /// `claude-code` / `codex` / … for a bot a daemon drives; the hosted
    /// template (`claude`, `deepseek`) otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The model the owner set, by its harness's display name when a daemon
    /// reported one (`Opus (1M context)`), else as stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The machine its daemon last reported from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// That machine's `device_id` — the key of its row in `listHarnessHosts`,
    /// so a computer card can put the agent under the computer it runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// The viewer's direct conversation with it, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dm: Option<Uuid>,
    /// Writing a reply right now.
    pub working: bool,
    /// Where, when it is a conversation the person is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_in: Option<ViewerConv>,
    /// When the turn it is writing began (RFC 3339) — the one in `working_in`
    /// when there is one, else its newest; a steer doesn't restart it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_since: Option<String>,
    /// When it last finished a reply, anywhere (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<String>,
    /// When it finished each reply of the last `ACTIVITY_DAYS` days (the api's
    /// `apps_api.rs`; unix ms, newest first) — the page buckets them into its
    /// own local days.
    #[serde(default)]
    pub replied_at: Vec<i64>,
}

/// A conversation named to a viewer: its id, and its title only when the
/// viewer may see one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerConv {
    pub id: Uuid,
    /// None for a direct conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

// ───────────────── the computer card (`computers|list`) ─────────────────

/// What the activity center's computer card (`mafold/computers`) is told when
/// it asks — about the person who opened it, wherever it is open: their own
/// computers as `listHarnessHosts` lists them, and their own agents as
/// `getAppViewerProfile` describes them. The agents' history (`last_active_at`,
/// `replied_at`) is left empty: it costs a pass over every message, and this
/// card asks each time it opens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputersView {
    /// The server's clock (unix seconds), so «3 分钟没上报» is measured
    /// against the same clock as `reported_at`, not the device's.
    pub now: i64,
    /// Online first, then most recently seen.
    pub computers: Vec<HarnessHost>,
    pub agents: Vec<ViewerAgent>,
}

#[cfg(test)]
mod tests {
    //! The bytes these shapes put on the wire / in the store, pinned. They were
    //! captured from the api's own definitions immediately before the move
    //! (activity center K1), so equality here is "nothing changed for any
    //! client or any stored row".
    use super::*;

    fn host() -> DeviceHarnesses {
        DeviceHarnesses {
            device_id: "dev-1".into(),
            device_name: "MacBook".into(),
            harnesses: vec![
                HarnessInfo {
                    id: "claude-code".into(),
                    available: true,
                    version: Some("2.1.0".into()),
                    health: Some(SeatHealth {
                        state: "exhausted".into(),
                        status: Some(429),
                        tier: Some("Max (20x)".into()),
                        limits: vec![SeatLimit { kind: "session".into(), label: "5h".into(), percent: 100.0, resets_at: Some(1760000000) }],
                        checked_at: 1760000001,
                    }),
                    accounts: vec![AccountSeat { name: "default".into(), email: Some("a@b.c".into()), health: None }],
                },
                HarnessInfo { id: "codex".into(), available: false, version: None, health: None, accounts: vec![] },
            ],
            cli_version: Some("0.9.147".into()),
            reported_at: 1760000002,
        }
    }

    const DEVICE: &str = r#"{"device_id":"dev-1","device_name":"MacBook","harnesses":[{"id":"claude-code","available":true,"version":"2.1.0","health":{"state":"exhausted","status":429,"tier":"Max (20x)","limits":[{"kind":"session","label":"5h","percent":100.0,"resets_at":1760000000}],"checked_at":1760000001},"accounts":[{"name":"default","email":"a@b.c"}]},{"id":"codex","available":false,"version":null}],"cli_version":"0.9.147","reported_at":1760000002}"#;

    #[test]
    fn device_report_bytes_unchanged() {
        assert_eq!(serde_json::to_string(&host()).unwrap(), DEVICE);
        // And the stored row round-trips through the moved type.
        let back: DeviceHarnesses = serde_json::from_str(DEVICE).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), DEVICE);
    }

    fn heartbeat(version: Option<&str>, available: bool) -> DeviceHarnesses {
        let mut d = host();
        d.harnesses = vec![HarnessInfo { id: "claude-code".into(), available, version: version.map(Into::into), health: None, accounts: vec![] }];
        d
    }

    #[test]
    fn a_heartbeat_keeps_the_login_state_a_full_report_wrote() {
        let full = host();
        let mut beat = heartbeat(Some("2.1.0"), true);
        beat.carry_forward(&full);
        let h = &beat.harnesses[0];
        assert_eq!(h.health.as_ref().map(|s| s.state.as_str()), Some("exhausted"), "kept across the heartbeat");
        assert_eq!(h.accounts.len(), 1);
    }

    #[test]
    fn a_new_version_or_an_uninstall_starts_from_nothing() {
        let full = host();
        let mut upgraded = heartbeat(Some("2.2.0"), true);
        upgraded.carry_forward(&full);
        assert!(upgraded.harnesses[0].health.is_none() && upgraded.harnesses[0].accounts.is_empty(), "new version: cleared");
        let mut gone = heartbeat(Some("2.1.0"), false);
        gone.carry_forward(&full);
        assert!(gone.harnesses[0].health.is_none(), "not installed: nothing to keep");
    }

    #[test]
    fn a_report_that_says_something_is_taken_as_said() {
        let full = host();
        let mut fresh = heartbeat(Some("2.1.0"), true);
        fresh.harnesses[0].health = Some(SeatHealth { state: "ok".into(), status: None, tier: None, limits: vec![], checked_at: 9 });
        fresh.carry_forward(&full);
        assert_eq!(fresh.harnesses[0].health.as_ref().unwrap().state, "ok", "the newer word wins");
        assert!(fresh.harnesses[0].accounts.is_empty(), "and isn't padded with the old logins");
    }

    #[test]
    fn rows_from_older_clis_still_load() {
        // pre-0.9.60: no cli_version; pre-0.9.86: no health; pre-0.9.109: no accounts.
        let old = r#"{"device_id":"d","device_name":"n","harnesses":[{"id":"claude-code","available":true}],"reported_at":1}"#;
        let d: DeviceHarnesses = serde_json::from_str(old).unwrap();
        assert!(d.cli_version.is_none());
        assert!(d.harnesses[0].health.is_none() && d.harnesses[0].accounts.is_empty() && d.harnesses[0].version.is_none());
    }

    #[test]
    fn hosts_list_bytes_unchanged() {
        let d = host();
        let r = HarnessHostsResult {
            items: vec![
                HarnessHost { device_id: d.device_id.clone(), device_name: d.device_name.clone(), harnesses: d.harnesses.clone(), cli_version: d.cli_version.clone(), online: true, reported_at: d.reported_at },
                HarnessHost { device_id: "dev-2".into(), device_name: "PC".into(), harnesses: vec![], cli_version: None, online: false, reported_at: 5 },
            ],
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"items":[{"device_id":"dev-1","device_name":"MacBook","harnesses":[{"id":"claude-code","available":true,"version":"2.1.0","health":{"state":"exhausted","status":429,"tier":"Max (20x)","limits":[{"kind":"session","label":"5h","percent":100.0,"resets_at":1760000000}],"checked_at":1760000001},"accounts":[{"name":"default","email":"a@b.c"}]},{"id":"codex","available":false,"version":null}],"cli_version":"0.9.147","online":true,"reported_at":1760000002},{"device_id":"dev-2","device_name":"PC","harnesses":[],"online":false,"reported_at":5}]}"#
        );
    }

    #[test]
    fn presence_bytes_unchanged() {
        let s = vec![
            UserStatus { username: "a".into(), online: true, last_seen: None },
            UserStatus { username: "b".into(), online: false, last_seen: DateTime::from_timestamp(1760000000, 0) },
        ];
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"[{"username":"a","online":true},{"username":"b","online":false,"last_seen":"2025-10-09T08:53:20Z"}]"#
        );
    }

    #[test]
    fn agent_bytes_unchanged() {
        let a = ViewerAgent {
            username: "ops:bot".into(), display_name: "Bot".into(), bio: Some("hi".into()), avatar_url: Some("u".into()), online: true,
            harness: Some("claude-code".into()), model: Some("Opus".into()), machine: Some("MacBook".into()), device_id: None,
            dm: Some(Uuid::from_u128(1)), working: true,
            working_in: Some(ViewerConv { id: Uuid::from_u128(2), title: Some("#dev".into()) }), working_since: None,
            last_active_at: Some("2026-10-21T00:00:00Z".into()), replied_at: vec![3, 2],
        };
        let b = ViewerAgent {
            username: "ops:b2".into(), display_name: "B2".into(), bio: None, avatar_url: None, online: false, harness: None, model: None,
            machine: None, device_id: None, dm: None, working: false, working_in: Some(ViewerConv { id: Uuid::from_u128(3), title: None }),
            working_since: None, last_active_at: None, replied_at: vec![],
        };
        assert_eq!(
            serde_json::to_string(&vec![a, b]).unwrap(),
            r##"[{"username":"ops:bot","display_name":"Bot","bio":"hi","avatar_url":"u","online":true,"harness":"claude-code","model":"Opus","machine":"MacBook","dm":"00000000-0000-0000-0000-000000000001","working":true,"working_in":{"id":"00000000-0000-0000-0000-000000000002","title":"#dev"},"last_active_at":"2026-10-21T00:00:00Z","replied_at":[3,2]},{"username":"ops:b2","display_name":"B2","online":false,"working":false,"working_in":{"id":"00000000-0000-0000-0000-000000000003"},"replied_at":[]}]"##
        );
    }

    /// K8's two fields are additions only: absent, the bytes above stand;
    /// present, each sits beside what it qualifies.
    #[test]
    fn agent_where_and_since() {
        let a = ViewerAgent {
            username: "ops:bot".into(), display_name: "Bot".into(), bio: None, avatar_url: None, online: true, harness: None, model: None,
            machine: Some("MacBook".into()), device_id: Some("dev-1".into()), dm: None, working: true, working_in: None,
            working_since: Some("2026-10-21T00:00:00+00:00".into()), last_active_at: None, replied_at: vec![],
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"username":"ops:bot","display_name":"Bot","online":true,"machine":"MacBook","device_id":"dev-1","working":true,"working_since":"2026-10-21T00:00:00+00:00","replied_at":[]}"#
        );
        let old: ViewerAgent = serde_json::from_str(r#"{"username":"x","display_name":"X","online":false,"working":false}"#).unwrap();
        assert!(old.device_id.is_none() && old.working_since.is_none());
    }
}
