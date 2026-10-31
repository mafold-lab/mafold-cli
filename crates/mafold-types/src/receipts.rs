//! What agents did for a person today, one row per reply — the wire shapes
//! behind the activity center's 回报 tab (the receipts card,
//! `mafold/receipts`, asking `receipts|today`).
//!
//! A row is a reply, not a wallet line: what was asked, by whom, when it
//! started and finished, what it delivered, and what it cost — the cost only
//! when the server can tie it to this reply (`billing`). Additive from here on,
//! like `computers.rs`: the golden tests at the bottom pin the bytes.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::computers::ViewerConv;

/// What the receipts card is told when it asks (`receipts|today`) — about the
/// person who opened it, wherever it is open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptsView {
    /// The server's clock (unix seconds), so «2 分钟» counts on one clock.
    pub now: i64,
    /// The window's start actually used (unix seconds): the card's own
    /// «today began at», held to the last 36 hours.
    pub since: i64,
    /// Running first, then newest finished first; at most 200.
    pub items: Vec<Receipt>,
}

/// One reply: an agent's answer the person may see a receipt for — one of
/// their own agents' (whoever asked), or any agent's answering them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    /// The reply's message id (a running one: its draft's).
    pub id: Uuid,
    /// Where it was written; the title only when the person may see one
    /// (the same rule as `ViewerAgent::working_in`).
    pub conv: ViewerConv,
    /// The person is in that conversation, so it opens for them
    /// (`host|chat|<conv.id>`). An owner is not in a stranger's DM with
    /// their agent: no 打开对话 there.
    #[serde(default)]
    pub can_open: bool,
    /// That conversation is a DM the person is in (私聊) — which a missing
    /// `conv.title` alone doesn't say: a group may have none, and a room the
    /// person isn't in has none either.
    #[serde(default)]
    pub dm: bool,
    pub agent: ReceiptAgent,
    /// The agent is the person's own.
    pub mine: bool,
    /// The person started the chain it answers (its `_origin`).
    pub asked_by_me: bool,
    /// The first line of the message that asked, at most 80 characters —
    /// only when the person can read that message where it was said: an
    /// owner is not shown what a stranger asked their agent elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// When the message that asked was sent (unix ms). Replies from before
    /// the server kept their trigger have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_at: Option<i64>,
    /// When the turn began (unix ms) — a steer doesn't restart it.
    pub started_at: i64,
    /// When the reply was finished (unix ms); none while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    pub state: ReceiptState,
    /// The names of the files attached to the reply, in order.
    #[serde(default)]
    pub files: Vec<String>,
    pub billing: Billing,
    /// What it cost, when `billing` is `charged`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge: Option<Charge>,
}

/// The agent that wrote a [`Receipt`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptAgent {
    pub username: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptState {
    /// Being written right now.
    Running,
    Done,
    /// Someone stopped it.
    Stopped,
    /// It died on an error.
    Failed,
}

/// How a reply was paid for, as far as the server can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Billing {
    /// A metered turn with its own ledger row: see `charge`.
    Charged,
    /// The person's own agent, run on their own computer, nothing charged —
    /// their own subscription.
    Own,
    /// Nothing drawn from the person's wallet, and not run on their own
    /// computer: someone else's agent, or one hosted here on a house key or
    /// on its owner's own vendor key.
    Free,
    /// A hosted agent whose wallet debits can't be tied to one reply yet. Never
    /// «own subscription»: money may well have moved.
    Unmatched,
}

/// The ledger row behind a `charged` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Charge {
    /// The wallet currency — a model id (`claude-opus-5`).
    pub currency: String,
    /// Its display name (`Opus 5`).
    pub currency_name: String,
    /// How much moved, positive.
    pub amount: i64,
    /// Which side of the row the person is on: `true` = they paid it (a
    /// debit, and theirs to dispute while `pending`); `false` = their agent
    /// was paid it (income — `meter|dispute` refuses the payee).
    pub paid_by_me: bool,
    /// `pending` | `held` | `void` | `paid`.
    pub status: String,
    /// The payable's id — what `meter|dispute|<payable>` names.
    pub payable: Uuid,
    pub in_tokens: u64,
    pub out_tokens: u64,
}

#[cfg(test)]
mod tests {
    //! The bytes `receipts|today` puts on the wire, pinned: the receipts card
    //! is built against exactly these.
    use super::*;

    #[test]
    fn view_bytes() {
        let charged = Receipt {
            id: Uuid::from_u128(1),
            conv: ViewerConv { id: Uuid::from_u128(2), title: Some("#dev".into()) },
            can_open: true,
            dm: false,
            agent: ReceiptAgent { username: "ops:cc".into(), display_name: "Claude 助手".into(), avatar_url: Some("u".into()) },
            mine: false,
            asked_by_me: true,
            title: Some("帮我看下 #1036 的 CI".into()),
            trigger_at: Some(1_760_000_000_000),
            started_at: 1_760_000_001_000,
            finished_at: Some(1_760_000_060_000),
            state: ReceiptState::Done,
            files: vec!["report.md".into()],
            billing: Billing::Charged,
            charge: Some(Charge {
                currency: "claude-opus-5".into(),
                currency_name: "Opus 5".into(),
                amount: 2900,
                paid_by_me: true,
                status: "held".into(),
                payable: Uuid::from_u128(3),
                in_tokens: 12,
                out_tokens: 340,
            }),
        };
        let running = Receipt {
            id: Uuid::from_u128(4),
            conv: ViewerConv { id: Uuid::from_u128(5), title: None },
            can_open: true,
            dm: true,
            agent: ReceiptAgent { username: "ops:x".into(), display_name: "X".into(), avatar_url: None },
            mine: true,
            asked_by_me: false,
            title: None,
            trigger_at: None,
            started_at: 7,
            finished_at: None,
            state: ReceiptState::Running,
            files: vec![],
            billing: Billing::Own,
            charge: None,
        };
        let v = ReceiptsView { now: 1_760_000_100, since: 1_759_996_800, items: vec![running, charged] };
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r##"{"now":1760000100,"since":1759996800,"items":[{"id":"00000000-0000-0000-0000-000000000004","conv":{"id":"00000000-0000-0000-0000-000000000005"},"can_open":true,"dm":true,"agent":{"username":"ops:x","display_name":"X"},"mine":true,"asked_by_me":false,"started_at":7,"state":"running","files":[],"billing":"own"},{"id":"00000000-0000-0000-0000-000000000001","conv":{"id":"00000000-0000-0000-0000-000000000002","title":"#dev"},"can_open":true,"dm":false,"agent":{"username":"ops:cc","display_name":"Claude 助手","avatar_url":"u"},"mine":false,"asked_by_me":true,"title":"帮我看下 #1036 的 CI","trigger_at":1760000000000,"started_at":1760000001000,"finished_at":1760000060000,"state":"done","files":["report.md"],"billing":"charged","charge":{"currency":"claude-opus-5","currency_name":"Opus 5","amount":2900,"paid_by_me":true,"status":"held","payable":"00000000-0000-0000-0000-000000000003","in_tokens":12,"out_tokens":340}}]}"##
        );
        // And it reads back.
        let back: ReceiptsView = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(back.items[1].charge.as_ref().unwrap().payable, Uuid::from_u128(3));
    }

    /// Every word the card switches on, as it is spelled on the wire.
    #[test]
    fn the_words() {
        assert_eq!(
            serde_json::to_string(&[ReceiptState::Running, ReceiptState::Done, ReceiptState::Stopped, ReceiptState::Failed]).unwrap(),
            r#"["running","done","stopped","failed"]"#
        );
        assert_eq!(
            serde_json::to_string(&[Billing::Charged, Billing::Own, Billing::Free, Billing::Unmatched]).unwrap(),
            r#"["charged","own","free","unmatched"]"#
        );
    }
}
