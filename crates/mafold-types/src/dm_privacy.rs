//! Who can send me messages — a person's rule for who may start, or keep
//! going, a direct chat with them, and for who may pull them into a group
//! (Settings ▸ Privacy and Security; the api's `dm_privacy.rs` enforces it).
//!
//! Three settings, after Telegram's "Who can send me messages":
//! - **everyone** — the default.
//! - **premium** — only Premium members. A Premium or a verified account may
//!   choose it.
//! - **list** — only the accounts, and the members of the groups, the person
//!   put on their list. Verified accounts only.
//!
//! An AI account is judged by its owner (`Account::owner()`): it counts as
//! Premium when its owner is, a person on a list brings the AI accounts they
//! own along, and an AI account's own setting is its owner's to choose.
//!
//! Only new contact is held back. Whoever the person has written to in their
//! direct chat — an exchange they took part in, or one they started — keeps
//! writing; so does anyone of the same owner.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The choice in Settings ▸ Privacy and Security ▸ Who can message me.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmMode {
    #[default]
    Everyone,
    Premium,
    List,
}

/// Why a sender is turned away — what a client says, and whether it offers
/// Premium as the way in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DmRestriction {
    /// Only Premium members may message this account.
    Premium,
    /// Only the accounts on its list may.
    List,
}

impl DmRestriction {
    /// The `error_key` of a refusal for this reason — one per reason, so a
    /// client knows from the key alone whether to offer Premium.
    pub const fn error_key(self) -> &'static str {
        match self {
            DmRestriction::Premium => "DM_PREMIUM_REQUIRED",
            DmRestriction::List => "DM_LIST_ONLY",
        }
    }
}

/// The stored setting of one account. The list stays when the mode moves off
/// `list`, so switching back finds it as it was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmPrivacy {
    #[serde(default)]
    pub mode: DmMode,
    /// Usernames, lowercased. A person here brings the AI accounts they own.
    #[serde(default)]
    pub users: Vec<String>,
    /// Groups whose members are let through — whoever is in the group at the
    /// time, while the account itself is still in it.
    #[serde(default)]
    pub groups: Vec<Uuid>,
}

/// How many accounts, and how many groups, one list holds.
pub const LIST_USERS_MAX: usize = 500;
pub const LIST_GROUPS_MAX: usize = 50;

/// What may be chosen by an account that is (by its owner, for an AI account)
/// Premium and/or verified.
pub const fn may_choose(mode: DmMode, premium: bool, verified: bool) -> bool {
    match mode {
        DmMode::Everyone => true,
        DmMode::Premium => premium || verified,
        DmMode::List => verified,
    }
}

/// The setting that applies right now. One the account may no longer choose —
/// its Premium ran out, its verification was taken back — is paused: kept,
/// but everyone is let through until it may be chosen again.
pub const fn effective(mode: DmMode, premium: bool, verified: bool) -> DmMode {
    if may_choose(mode, premium, verified) {
        mode
    } else {
        DmMode::Everyone
    }
}

/// What the recipient's setting decides about one sender, given the facts the
/// api gathered. `mode` is the [`effective`] one.
///
/// - `same_owner`: sender and recipient are one person, or AI accounts of one.
/// - `recipient_wrote`: the recipient has a message in their direct chat with
///   the sender — an exchange they are part of, or one they started.
/// - `listed`: the sender, or its owner, is on the recipient's list or in one
///   of its groups. Counts only for a verified recipient (the list is theirs).
/// - `sender_premium`: the sender, judged by its owner, is Premium.
pub const fn decide(
    mode: DmMode,
    same_owner: bool,
    recipient_wrote: bool,
    listed: bool,
    sender_premium: bool,
) -> Option<DmRestriction> {
    match mode {
        DmMode::Everyone => None,
        _ if same_owner || recipient_wrote || listed => None,
        DmMode::Premium if sender_premium => None,
        DmMode::Premium => Some(DmRestriction::Premium),
        DmMode::List => Some(DmRestriction::List),
    }
}

/// One account as `getDmAccess` reports it: may the caller message it?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmAccess {
    pub username: String,
    /// `None` = the caller may message it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restricted: Option<DmRestriction>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use DmMode::*;

    #[test]
    fn who_may_choose_what() {
        // Anyone may stay open; Premium or verified may ask for Premium; only
        // verified may keep a list.
        for (p, v) in [(false, false), (true, false), (false, true), (true, true)] {
            assert!(may_choose(Everyone, p, v));
            assert_eq!(may_choose(Premium, p, v), p || v, "premium={p} verified={v}");
            assert_eq!(may_choose(List, p, v), v, "premium={p} verified={v}");
        }
    }

    #[test]
    fn a_setting_one_may_no_longer_choose_is_paused_not_lost() {
        assert_eq!(effective(Premium, true, false), Premium);
        assert_eq!(effective(Premium, false, false), Everyone, "Premium ran out");
        assert_eq!(effective(List, true, true), List);
        assert_eq!(effective(List, true, false), Everyone, "verification taken back");
    }

    #[test]
    fn premium_lets_premium_and_the_exceptions_through() {
        assert_eq!(decide(Premium, false, false, false, false), Some(DmRestriction::Premium));
        assert_eq!(decide(Premium, false, false, false, true), None, "a Premium sender");
        assert_eq!(decide(Premium, true, false, false, false), None, "the same owner");
        assert_eq!(decide(Premium, false, true, false, false), None, "the recipient wrote first / back");
        assert_eq!(decide(Premium, false, false, true, false), None, "on the list");
    }

    #[test]
    fn list_lets_only_the_list_and_the_exceptions_through() {
        assert_eq!(decide(List, false, false, false, true), Some(DmRestriction::List), "Premium is not enough");
        assert_eq!(decide(List, false, false, true, false), None);
        assert_eq!(decide(List, true, false, false, false), None);
        assert_eq!(decide(List, false, true, false, false), None);
    }

    #[test]
    fn everyone_turns_nobody_away() {
        assert_eq!(decide(Everyone, false, false, false, false), None);
    }

    #[test]
    fn wire_shapes() {
        let p: DmPrivacy = serde_json::from_str("{}").unwrap();
        assert_eq!(p, DmPrivacy::default(), "an empty row is the default");
        assert_eq!(serde_json::to_string(&Premium).unwrap(), "\"premium\"");
        assert_eq!(DmRestriction::Premium.error_key(), "DM_PREMIUM_REQUIRED");
        assert_eq!(DmRestriction::List.error_key(), "DM_LIST_ONLY");
        let open = DmAccess { username: "a".into(), restricted: None };
        assert_eq!(serde_json::to_string(&open).unwrap(), r#"{"username":"a"}"#);
    }
}
