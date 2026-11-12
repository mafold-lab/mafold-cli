//! Account-level tutorial progress. Business evidence is recorded by the API;
//! clients may acknowledge only the explicitly enumerated learning events.
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TutorialMark {
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TutorialProgress {
    #[serde(default)]
    pub marks: BTreeMap<String, TutorialMark>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TutorialChapter {
    pub id: String,
    pub step: u8,
    pub completed: bool,
    pub started: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TutorialState {
    pub version: u32,
    pub progress: TutorialProgress,
    pub chapters: Vec<TutorialChapter>,
    pub official_chat_id: Option<String>,
    pub welcome_message_id: Option<String>,
    pub gift_message_id: Option<String>,
    pub gift_status: String,
    pub agent_username: Option<String>,
    pub agent_chat_id: Option<String>,
    pub usage_tx_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TutorialEvent {
    pub version: u32,
    pub event: String,
    pub subject: Option<String>,
}
