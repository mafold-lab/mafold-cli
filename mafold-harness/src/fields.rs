//! The Customize-sheet field a profile or a plugin declares — the schema sent
//! to clients to render the form. Lives beside [`crate::HarnessPlugin`]
//! because a plugin declares its own fields (`config_fields`); mafold-api
//! re-exports it as `brains::FieldSpec`.

use serde::{Deserialize, Serialize};

// ───────────────── schema (sent to clients to render the form) ─────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct FieldOption {
    pub label: String,
    pub value: String,
    /// Optional langpack key for `label` — clients that resolve it show the
    /// translation and fall back to `label` verbatim otherwise (the same
    /// key-or-server-text pattern service notices use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_key: Option<String>,
    /// Where this option holds, when it was LEARNED rather than authored —
    /// `2.1.272 · ops's MacBook` for a model a reported harness accepts
    /// (`reportHarnessCaps`). Shown under the option; absent for authored
    /// options, ignored by a client without the notion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// One field in a template's form. `kind`: string | number | bool | secret |
/// select. `select` carries `options`.
#[derive(Serialize, Deserialize, Clone)]
pub struct FieldSpec {
    pub key: String,
    pub label: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Optional langpack keys for `label` / `placeholder` — see FieldOption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<FieldOption>,
    /// For `kind: "connection"` — which provider the picked connection must
    /// have (e.g. `codex-oauth`). A client renders the field as a picker over
    /// the CALLER's own connections filtered to this provider; the stored
    /// value is the connection's name. Meaningless for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Surface this field (label + its effective value) on the bot's PROFILE
    /// card, where everyone who can see the profile can read it — the owner's
    /// opt-in, per field. `secret` fields can never carry it (validate_schema
    /// rejects that): the whole point of a secret is that it has no readable
    /// value. Clients don't resolve this themselves; the server hands them
    /// display-ready rows via `getProfileFields`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_on_profile: bool,
    /// May a MEMBER set their own value for this field? The lock, to
    /// `show_on_profile`'s eye — and independent of it, because "you may not
    /// read what I set" and "you may not set your own" are different sentences.
    /// Defaults to TRUE: overriding a bot's model or bringing your own key has
    /// always been anyone's to do, and an owner locks it deliberately.
    #[serde(default = "yes")]
    pub member_editable: bool,
    /// A line shown under the field: what its author wants said about it
    /// (with `hint_key`), or — for a field whose options were LEARNED
    /// (`reportHarnessCaps`) rather than authored — which harness / version /
    /// machine answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Optional langpack key for `hint` — `tOr(hint_key, hint)`, like
    /// `label_key` / `placeholder_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint_key: Option<String>,
}
fn yes() -> bool {
    true
}
fn default_kind() -> String {
    "string".into()
}
