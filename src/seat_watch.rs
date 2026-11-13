//! Telling a person when a Claude login on this machine signs out, or is
//! about to — `~/.mafold/claude-seat-watch.json`.
//!
//! 2026-11-11 08:23Z mafold.cicd's `default` login went blank — Claude Code
//! wipes a login whose renewal Anthropic refuses — with 13 days left on its
//! sign-in. Every reply after that fell through to `redq` for two days, and
//! the only trace was a `↻ Using account` line that leaves the reply once it
//! is finished: the owner found out when the Team seat's week ran low. The
//! supervisor already looks at every login on its heartbeat
//! (`harness::report_rows`); this keeps what it saw last and says a change
//! ONCE, from one of the owner's Claude Code bots, in their DM with it:
//!
//! - a login that was signed in and isn't any more — blank, gone, past its
//!   sign-in end, or refused by Anthropic — unless a person signed it out or
//!   started signing it in themselves ([`touched`]: `/logout`, `/login`);
//! - a login whose sign-in ends within [`crate::accounts::EXPIRY_WARN_SECS`],
//!   once per sign-in.
//!
//! A login seen for the first time is only recorded: nobody saw it signed
//! in, so its being out is nothing that changed. A message waits here until
//! it is delivered — its `client_msg_id` makes a retry land once — and is
//! dropped after [`STALE`].

use crate::accounts::{Account, Registry};
use crate::commands::StoredLogin;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How long a signed-in login has to stay out before it is said: one look
/// can land while Claude Code is rewriting the credential.
const SETTLE: i64 = 60;

/// After a person signs a login out or starts signing it in, a sign-out is
/// theirs for this long — a sign-in waits up to six minutes for its code
/// (`runtime_login::CODE_WAIT`).
const OWN_DOING: i64 = 15 * 60;

/// A message nobody could be given for this long is dropped.
const STALE: i64 = 3 * 86400;

/// Two ends this close are one sign-in. Claude Code writes the end down
/// again on every renewal — its clock plus the seconds Anthropic says are
/// left — so the same sign-in's end drifts by a second or so; a new sign-in
/// moves it by weeks.
const SAME_SIGN_IN: i64 = 3600;

/// The longest a failed send waits before it is tried again.
const RETRY_CAP: i64 = 3600;

pub(crate) fn path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".mafold/claude-seat-watch.json")
}

/// What one look at a login found. `end` is when its sign-in ends, epoch
/// seconds (`commands::login_expiry_from`).
#[derive(Clone, Debug, PartialEq)]
pub enum Seen {
    /// A renewable credential is on file, and nothing has refused it.
    In { end: Option<i64> },
    /// Blank, gone, past its sign-in end, or refused by Anthropic.
    Out { end: Option<i64> },
    /// Couldn't tell (storage unreadable): changes nothing.
    Unknown,
}

/// One login's record.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Seat {
    /// Signed in, as last settled. None until it is first seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<bool>,
    /// When a look first found it out while it was signed in, epoch seconds.
    /// It is said once it is still out [`SETTLE`] later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_seen: Option<i64>,
    /// When a person last signed it out or began signing it in, epoch
    /// seconds: a sign-out within [`OWN_DOING`] of it is theirs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touched_at: Option<i64>,
    /// The sign-in end (epoch seconds) a warning already went out for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warned_end: Option<i64>,
}

/// Something to tell, kept until it is delivered.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct News {
    /// One per event, and the send's `client_msg_id`.
    pub key: String,
    pub text: String,
    /// When it was decided, epoch seconds.
    pub at: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct Disk {
    #[serde(default)]
    seats: BTreeMap<String, Seat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<News>,
}

/// What changed for one login since the last look.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// It was signed in and isn't any more, since `at`.
    SignedOut { at: i64, end: Option<i64> },
    /// Its sign-in ends at `end`, inside the warning window.
    Ending { end: i64 },
}

/// One login's record moved on by one look — the rules, without any I/O.
pub(crate) fn step(seat: &mut Seat, seen: &Seen, now: i64) -> Option<Event> {
    match *seen {
        Seen::Unknown => None,
        Seen::In { end } => {
            seat.signed_in = Some(true);
            seat.out_seen = None;
            let end = end.filter(|e| *e > now && e - now <= crate::accounts::EXPIRY_WARN_SECS)?;
            if seat.warned_end.is_some_and(|w| (w - end).abs() <= SAME_SIGN_IN) {
                return None;
            }
            seat.warned_end = Some(end);
            Some(Event::Ending { end })
        }
        Seen::Out { end } => {
            if seat.signed_in != Some(true) {
                // Seen for the first time, or out already.
                seat.signed_in = Some(false);
                return None;
            }
            if seat.touched_at.is_some_and(|t| now - t <= OWN_DOING) {
                seat.signed_in = Some(false);
                seat.out_seen = None;
                return None;
            }
            let since = *seat.out_seen.get_or_insert(now);
            if now - since < SETTLE {
                return None;
            }
            seat.signed_in = Some(false);
            seat.out_seen = None;
            Some(Event::SignedOut { at: since, end })
        }
    }
}

/// Move every login's record on by its look (`looks`, read outside the
/// lock), queue what that changed, and hand back everything still waiting
/// to be said, oldest first.
pub(crate) fn observe_at(path: &Path, looks: &[(Account, Seen)], device: &str, now: i64) -> Vec<News> {
    let signed_in: Vec<&str> =
        looks.iter().filter(|(_, s)| matches!(s, Seen::In { .. })).map(|(a, _)| a.name.as_str()).collect();
    crate::lifecycle::update(path, |d: &mut Disk| {
        let before = (d.seats.clone(), d.pending.clone());
        // A login forgotten here (`/account forget`) takes its record along.
        d.seats.retain(|name, _| looks.iter().any(|(a, _)| a.name == *name));
        for (acct, seen) in looks {
            let seat = d.seats.entry(acct.name.clone()).or_default();
            match step(seat, seen, now) {
                Some(Event::SignedOut { at, end }) => {
                    let others: Vec<&str> = signed_in.iter().copied().filter(|n| *n != acct.name).collect();
                    d.pending.push(News {
                        key: format!("claude-seat:{}:out:{at}", acct.name),
                        text: out_text(acct, device, at, end, &others),
                        at: now,
                    });
                }
                Some(Event::Ending { end }) => d.pending.push(News {
                    key: format!("claude-seat:{}:ending:{end}", acct.name),
                    text: ending_text(acct, device, end, now),
                    at: now,
                }),
                None => {}
            }
        }
        d.pending.retain(|n| now - n.at <= STALE);
        let dirty = before != (d.seats.clone(), d.pending.clone());
        (d.pending.clone(), dirty)
    })
}

/// Strike what was delivered (or can't be, by anyone).
pub(crate) fn done_at(path: &Path, keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    crate::lifecycle::update(path, |d: &mut Disk| {
        let n = d.pending.len();
        d.pending.retain(|x| !keys.contains(&x.key));
        ((), d.pending.len() != n)
    })
}

/// A person is signing `name` out, or in again (`/logout`, `/login`, from a
/// chat or a terminal): a sign-out in the next [`OWN_DOING`] is theirs.
pub fn touched(name: &str) {
    touched_at(&path(), name, crate::accounts::now())
}

pub(crate) fn touched_at(path: &Path, name: &str, now: i64) {
    crate::lifecycle::update(path, |d: &mut Disk| {
        d.seats.entry(name.to_string()).or_default().touched_at = Some(now);
        ((), true)
    })
}

/// One login as its storage says it now. `health` is what the heartbeat's
/// probe last reported for it (`SeatHealth::state`): `unauthenticated` with
/// a credential on file means Anthropic refused that credential, as does a
/// refusal a turn remembered against it (`Registry::refused`).
fn look(acct: &Account, reg: &Registry, health: Option<&str>) -> Seen {
    let (login, end_ms, fingerprint) = crate::commands::credential_facts(&acct.env());
    let end = end_ms.map(|ms| ms / 1000);
    match login {
        StoredLogin::Missing => Seen::Out { end },
        StoredLogin::Unreadable(_) => Seen::Unknown,
        StoredLogin::Fresh(_) | StoredLogin::Stale => {
            if health == Some("unauthenticated") || reg.refused(&acct.name, fingerprint.as_deref()) {
                Seen::Out { end }
            } else {
                Seen::In { end }
            }
        }
    }
}

/// What this supervisor's heartbeat last reported for each Claude login
/// (`lifecycle::Runtimes`), by name.
fn reported_health(now: i64) -> BTreeMap<String, String> {
    let rows = crate::lifecycle::read_runtimes(&crate::lifecycle::runtimes_path(), std::process::id(), now, 90)
        .unwrap_or_default();
    rows.iter()
        .filter(|r| r["id"] == "claude-code")
        .flat_map(|r| r["accounts"].as_array().cloned().unwrap_or_default())
        .filter_map(|a| Some((a["name"].as_str()?.to_string(), a["health"]["state"].as_str()?.to_string())))
        .collect()
}

/// Who news goes out through.
#[derive(Clone, Debug, PartialEq)]
pub struct Speaker {
    /// The person told — signed in to Mafold on this machine.
    pub owner: String,
    /// Their bot that tells them, in their DM with it.
    pub bot: String,
    pub token: String,
}

/// For each person signed in to Mafold here (`people`), the first of their
/// Claude Code bots on this machine in the supervisor's order: the logins
/// watched here are what those bots run on. Someone with no Claude Code bot
/// here is not told — nothing of theirs runs on these logins.
pub(crate) fn speakers(daemons: &[crate::supervisor::DaemonCfg], people: &[String]) -> Vec<Speaker> {
    let mut out: Vec<Speaker> = Vec::new();
    for person in people {
        let person = person.to_lowercase();
        if out.iter().any(|s| s.owner == person) {
            continue;
        }
        let bot = daemons.iter().find(|d| {
            d.name.split_once(':').is_some_and(|(owner, _)| owner.eq_ignore_ascii_case(&person))
                && crate::harness::select(d.harness.as_deref().unwrap_or("claude-code")).id() == "claude-code"
        });
        if let Some(d) = bot {
            out.push(Speaker { owner: person, bot: d.name.clone(), token: d.token.clone() });
        }
    }
    out
}

/// Sends that failed: key → (failures so far, next try, epoch seconds). In
/// memory: after a restart whatever waits is simply tried again.
fn retries() -> &'static Mutex<HashMap<String, (u32, i64)>> {
    static R: std::sync::OnceLock<Mutex<HashMap<String, (u32, i64)>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default)
}

/// One look at every Claude login here, and what changed said — the
/// supervisor runs this beside its heartbeat. `device` names this machine
/// the way Mafold shows it.
pub async fn beat(base: &str, speakers: Vec<Speaker>, device: String) {
    let now = crate::accounts::now();
    // Credential reads (a Keychain on a Mac) and the file lock block.
    let waiting = tokio::task::spawn_blocking(move || {
        let reg = crate::accounts::load();
        let health = reported_health(now);
        let looks: Vec<(Account, Seen)> =
            reg.accounts.iter().map(|a| (a.clone(), look(a, &reg, health.get(&a.name).map(String::as_str)))).collect();
        observe_at(&path(), &looks, &device, now)
    })
    .await
    .unwrap_or_default();
    if waiting.is_empty() {
        return;
    }
    if speakers.is_empty() {
        for n in &waiting {
            println!("· no Claude Code bot of anyone signed in here to say it through — {}", n.text.replace('\n', " "));
        }
        done_at(&path(), &waiting.iter().map(|n| n.key.clone()).collect::<Vec<_>>());
        return;
    }
    let mut done = Vec::new();
    for n in &waiting {
        let due = retries().lock().unwrap_or_else(|p| p.into_inner()).get(&n.key).is_none_or(|(_, at)| *at <= now);
        if !due {
            continue;
        }
        if deliver(base, n, &speakers).await {
            retries().lock().unwrap_or_else(|p| p.into_inner()).remove(&n.key);
            done.push(n.key.clone());
        } else {
            let mut r = retries().lock().unwrap_or_else(|p| p.into_inner());
            let fails = r.get(&n.key).map_or(0, |(f, _)| *f) + 1;
            let wait = (30_i64 << fails.min(7)).min(RETRY_CAP);
            r.insert(n.key.clone(), (fails, now + wait));
        }
    }
    done_at(&path(), &done);
}

/// Put `news` in front of every speaker's owner, in their DM with that bot.
/// True when all of them have it; a retry re-sends to all, and the key makes
/// the ones that already landed land once.
async fn deliver(base: &str, news: &News, speakers: &[Speaker]) -> bool {
    let mut all = true;
    for s in speakers {
        let client = crate::client::Client::new(base.to_string(), s.token.clone());
        let sent = async {
            let dm = client.resolve_chat(&s.owner).await?;
            client.send_message(crate::client::Dest::chat(&dm), &news.text, &[], Some(&news.key)).await
        }
        .await;
        match sent {
            Ok(_) => println!("→ told @{} through {}: {}", s.owner, s.bot, news.key),
            Err(e) => {
                all = false;
                eprintln!("✗ couldn't tell @{} through {} ({}): {e:#} — trying again later", s.owner, s.bot, news.key);
            }
        }
    }
    all
}

/// `Your Claude login `redq` (ops@…) on mafold.cicd`.
fn whose(acct: &Account, device: &str) -> String {
    match &acct.email {
        Some(email) => format!("Your Claude login `{}` ({email}) on {device}", acct.name),
        None => format!("Your Claude login `{}` on {device}", acct.name),
    }
}

/// `Nov 11, 08:23 UTC` — the reader's timezone isn't known here.
fn utc(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.format("%b %-d, %H:%M UTC").to_string()).unwrap_or_default()
}

/// The message for a login that went out at `at`. `others` are the logins
/// still signed in here, in the order turns fall back to them.
pub(crate) fn out_text(acct: &Account, device: &str, at: i64, end: Option<i64>, others: &[&str]) -> String {
    let when = match end {
        Some(end) if end <= at => format!("at {}: its sign-in reached its end", utc(at)),
        Some(end) => format!("at {}, {} before its sign-in would have ended", utc(at), crate::commands::fmt_dur(end - at)),
        None => format!("at {}", utc(at)),
    };
    let meanwhile = match others {
        [] => "No other Claude login is signed in on this computer, so its Claude Code bots can't reply until one is.".to_string(),
        names => format!(
            "Claude Code replies on this computer fall back to {} until then.",
            names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", then ")
        ),
    };
    format!(
        "⚠️ {} was signed out {when}.\n{meanwhile}\n`{}` here signs it back in.",
        whose(acct, device),
        crate::accounts::login_command(&acct.name)
    )
}

/// The message for a login whose sign-in ends at `end`.
pub(crate) fn ending_text(acct: &Account, device: &str, end: i64, now: i64) -> String {
    format!(
        "⏳ {} reaches the end of its sign-in in {} ({}), and signs out by itself then.\n`{}` here before that renews it for about four weeks.",
        whose(acct, device),
        crate::commands::fmt_dur(end - now),
        utc(end),
        crate::accounts::login_command(&acct.name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_385_380; // 2026-11-11 08:23 UTC
    const DAY: i64 = 86_400;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mafold-seat-watch-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("claude-seat-watch.json")
    }

    fn seat(name: &str) -> Account {
        if name == crate::accounts::DEFAULT {
            Account::default_login()
        } else {
            Account { name: name.into(), dir: Some(format!("/x/{name}")), email: Some(format!("{name}@example.com")), added_at: 1 }
        }
    }

    fn signed_in() -> Seat {
        Seat { signed_in: Some(true), ..Default::default() }
    }

    /// 11-11, replayed: `default` signed in with 13 days left goes blank
    /// while nothing runs. Said once, a minute later, from when it was first
    /// seen out — and never again while it stays out.
    #[test]
    fn a_signed_in_login_that_goes_blank_is_said_once_after_it_settles() {
        let mut s = signed_in();
        let end = Some(NOW + 13 * DAY);
        assert_eq!(step(&mut s, &Seen::Out { end }, NOW), None, "one look is not enough");
        assert_eq!(step(&mut s, &Seen::Out { end }, NOW + 30), None);
        assert_eq!(step(&mut s, &Seen::Out { end }, NOW + 60), Some(Event::SignedOut { at: NOW, end }));
        for t in [NOW + 90, NOW + DAY] {
            assert_eq!(step(&mut s, &Seen::Out { end }, t), None, "said once");
        }
    }

    #[test]
    fn a_blip_that_reads_signed_in_again_is_not_news() {
        let mut s = signed_in();
        assert_eq!(step(&mut s, &Seen::Out { end: None }, NOW), None);
        assert_eq!(step(&mut s, &Seen::In { end: None }, NOW + 30), None);
        assert_eq!(step(&mut s, &Seen::Out { end: None }, NOW + 60), None, "the clock starts over");
        assert_eq!(s.out_seen, Some(NOW + 60));
    }

    /// Nobody saw it signed in, so its being out changed nothing — on first
    /// sight, and on a login that was already out when the watch began.
    #[test]
    fn a_login_first_seen_out_is_only_recorded() {
        let mut s = Seat::default();
        for t in [NOW, NOW + 60, NOW + 600] {
            assert_eq!(step(&mut s, &Seen::Out { end: None }, t), None);
        }
        assert_eq!(s.signed_in, Some(false));
    }

    /// Signed back in: nothing is said (they just did it), and the next time
    /// it goes out is news again.
    #[test]
    fn signing_back_in_rearms_it() {
        let mut s = signed_in();
        step(&mut s, &Seen::Out { end: None }, NOW);
        assert!(step(&mut s, &Seen::Out { end: None }, NOW + 60).is_some());
        assert_eq!(step(&mut s, &Seen::In { end: None }, NOW + 3600), None);
        step(&mut s, &Seen::Out { end: None }, NOW + 7200);
        assert_eq!(step(&mut s, &Seen::Out { end: None }, NOW + 7260), Some(Event::SignedOut { at: NOW + 7200, end: None }));
    }

    /// `/logout`, or a `/login` that empties the slot while it waits for the
    /// code: the person's own doing.
    #[test]
    fn a_sign_out_right_after_a_person_touched_the_login_is_theirs() {
        let mut s = Seat { touched_at: Some(NOW - 10), ..signed_in() };
        assert_eq!(step(&mut s, &Seen::Out { end: None }, NOW), None);
        assert_eq!(s.signed_in, Some(false));
        assert_eq!(step(&mut s, &Seen::Out { end: None }, NOW + 120), None);
        // Long after a touch, a sign-out is news again.
        let mut s = Seat { touched_at: Some(NOW - OWN_DOING - 1), ..signed_in() };
        step(&mut s, &Seen::Out { end: None }, NOW);
        assert!(step(&mut s, &Seen::Out { end: None }, NOW + 60).is_some());
    }

    #[test]
    fn a_look_that_could_not_tell_changes_nothing() {
        let mut s = Seat { out_seen: Some(NOW), ..signed_in() };
        assert_eq!(step(&mut s, &Seen::Unknown, NOW + 600), None);
        assert_eq!(s, Seat { out_seen: Some(NOW), ..signed_in() });
    }

    /// Inside three days of its sign-in's end: said once for that sign-in. A
    /// new sign-in has a new end, and is warned about in its own time.
    #[test]
    fn a_sign_in_about_to_end_is_warned_about_once_per_sign_in() {
        let mut s = Seat::default();
        let end = NOW + 2 * DAY;
        assert_eq!(step(&mut s, &Seen::In { end: Some(NOW + 16 * DAY) }, NOW), None, "not yet");
        assert_eq!(step(&mut s, &Seen::In { end: Some(end) }, NOW), Some(Event::Ending { end }));
        assert_eq!(step(&mut s, &Seen::In { end: Some(end) }, NOW + 3600), None);
        // Renewing the access token writes the same end down a second off.
        assert_eq!(step(&mut s, &Seen::In { end: Some(end + 1) }, NOW + 8 * 3600), None);
        assert_eq!(step(&mut s, &Seen::In { end: Some(end - 1) }, NOW + 16 * 3600), None);
        let renewed = NOW + 29 * DAY;
        assert_eq!(step(&mut s, &Seen::In { end: Some(renewed) }, NOW + 7200), None);
        assert_eq!(
            step(&mut s, &Seen::In { end: Some(renewed) }, renewed - DAY),
            Some(Event::Ending { end: renewed })
        );
    }

    #[test]
    fn an_end_already_past_is_not_a_warning() {
        let mut s = Seat::default();
        assert_eq!(step(&mut s, &Seen::In { end: Some(NOW - 1) }, NOW), None);
    }

    /// Through the file: queued once, kept across beats until delivered,
    /// struck when it is, dropped when nobody could be told for three days.
    #[test]
    fn news_waits_on_disk_until_delivered() {
        let p = tmp("queue");
        let looks = |seen: Seen| vec![(seat("default"), seen), (seat("redq"), Seen::In { end: Some(NOW + 18 * DAY) })];
        let end = Some(NOW + 13 * DAY);
        assert!(observe_at(&p, &looks(Seen::In { end }), "mafold.cicd", NOW - 30).is_empty());
        assert!(observe_at(&p, &looks(Seen::Out { end }), "mafold.cicd", NOW).is_empty());
        let waiting = observe_at(&p, &looks(Seen::Out { end }), "mafold.cicd", NOW + 60);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].key, format!("claude-seat:default:out:{NOW}"));
        assert!(waiting[0].text.contains("fall back to `redq`"), "{}", waiting[0].text);
        // Not delivered yet: the next beat hands it back again, not twice.
        assert_eq!(observe_at(&p, &looks(Seen::Out { end }), "mafold.cicd", NOW + 90), waiting);
        done_at(&p, &[waiting[0].key.clone()]);
        assert!(observe_at(&p, &looks(Seen::Out { end }), "mafold.cicd", NOW + 120).is_empty());

        // Back in, out again, never delivered: gone after STALE.
        observe_at(&p, &looks(Seen::In { end }), "mafold.cicd", NOW + 200);
        observe_at(&p, &looks(Seen::Out { end }), "mafold.cicd", NOW + 300);
        assert_eq!(observe_at(&p, &looks(Seen::Out { end }), "mafold.cicd", NOW + 360).len(), 1);
        assert!(observe_at(&p, &looks(Seen::Out { end }), "mafold.cicd", NOW + 360 + STALE + 1).is_empty());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    /// `/logout` in another process lands in the same file and keeps the
    /// sign-out it causes from being news.
    #[test]
    fn a_touch_from_another_process_reaches_the_watch() {
        let p = tmp("touch");
        let looks = |seen: Seen| vec![(seat("default"), seen)];
        observe_at(&p, &looks(Seen::In { end: None }), "box", NOW);
        touched_at(&p, "default", NOW + 5);
        observe_at(&p, &looks(Seen::Out { end: None }), "box", NOW + 30);
        assert!(observe_at(&p, &looks(Seen::Out { end: None }), "box", NOW + 120).is_empty());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_forgotten_login_takes_its_record_along() {
        let p = tmp("forget");
        observe_at(&p, &[(seat("default"), Seen::In { end: None }), (seat("work"), Seen::In { end: None })], "box", NOW);
        observe_at(&p, &[(seat("default"), Seen::In { end: None })], "box", NOW + 30);
        let d: Disk = crate::lifecycle::read(&p).unwrap();
        assert_eq!(d.seats.keys().collect::<Vec<_>>(), ["default"]);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn the_messages_say_what_happened_and_what_to_send() {
        let end = NOW + 13 * DAY + 15 * 3600;
        assert_eq!(
            out_text(&seat("default"), "mafold.cicd", NOW, Some(end), &["redq"]),
            "⚠️ Your Claude login `default` on mafold.cicd was signed out at Nov 11, 08:23 UTC, 13d 15h before its sign-in would have ended.\n\
             Claude Code replies on this computer fall back to `redq` until then.\n\
             `/login` here signs it back in."
        );
        let text = out_text(&seat("redq"), "box", NOW, Some(NOW - 5), &[]);
        assert!(text.starts_with("⚠️ Your Claude login `redq` (redq@example.com) on box was signed out at Nov 11, 08:23 UTC: its sign-in reached its end."), "{text}");
        assert!(text.contains("No other Claude login is signed in"), "{text}");
        assert!(text.ends_with("`/login redq` here signs it back in."), "{text}");
        assert!(out_text(&seat("work"), "box", NOW, None, &["default", "redq"]).contains("fall back to `default`, then `redq`"));
        assert_eq!(
            ending_text(&seat("redq"), "mafold.cicd", NOW + 2 * DAY + 23 * 3600, NOW),
            "⏳ Your Claude login `redq` (redq@example.com) on mafold.cicd reaches the end of its sign-in in 2d 23h (Nov 14, 07:23 UTC), and signs out by itself then.\n\
             `/login redq` here before that renews it for about four weeks."
        );
    }

    fn daemon(name: &str, harness: Option<&str>) -> crate::supervisor::DaemonCfg {
        crate::supervisor::DaemonCfg {
            name: name.into(),
            token: format!("mb_{name}"),
            workdir: "/w".into(),
            harness: harness.map(str::to_string),
            env: Default::default(),
        }
    }

    /// mafold.cicd's own list: the first Claude Code bot of each person
    /// signed in here speaks; codex and opencode bots don't run on these
    /// logins, and a person with no Claude Code bot here isn't told.
    #[test]
    fn each_person_hears_it_once_from_their_first_claude_code_bot() {
        let daemons = [
            daemon("opsdu:codex", Some("codex")),
            daemon("opsdu:claude-code", Some("claude-code")),
            daemon("opsdu:claude333", Some("claude-code")),
            daemon("opsdu:mafoldtest", Some("opencode")),
            daemon("fei:helper", None),
        ];
        let got = speakers(&daemons, &["opsdu".into(), "OpsDu".into(), "fei".into(), "linsky".into()]);
        assert_eq!(
            got.iter().map(|s| (s.owner.as_str(), s.bot.as_str())).collect::<Vec<_>>(),
            [("opsdu", "opsdu:claude-code"), ("fei", "fei:helper")]
        );
        assert_eq!(got[0].token, "mb_opsdu:claude-code");
    }
}
