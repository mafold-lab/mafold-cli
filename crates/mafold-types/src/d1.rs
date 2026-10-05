//! D1 databases — the wire shapes, and the ONE rule for what a name may be.
//!
//! A Mafold database is a Cloudflare D1 database the platform creates on your
//! behalf. Like Cloudflare: you pick the name, it is unique within your account
//! (you and every bot you own share one list of names), and everything that
//! points at a database — a site's binding, a restore point — holds its `id`,
//! so a rename breaks nothing. See `.docs/d1-v1.md`.
//!
//! The name rule lives here so the api (which refuses a bad name) and the CLI
//! (which says so before it sends one) can never disagree.

use serde::{Deserialize, Serialize};

/// Longest name, in bytes.
pub const MAX_NAME_BYTES: usize = 63;

/// The regions a database's primary can be placed in (Cloudflare's location
/// hints). Every write goes to the primary, so the right one is where most of
/// the writes come from.
pub const LOCATIONS: &[&str] = &["wnam", "enam", "weur", "eeur", "apac", "oc"];

/// Why a name was refused. The `Display` text is what a user or an agent sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
    /// Doesn't start with a lowercase letter.
    Start,
    /// A character outside `a-z 0-9 - _`.
    Char(char),
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "a database needs a name"),
            Self::TooLong => write!(f, "a database name is at most {MAX_NAME_BYTES} characters"),
            Self::Start => write!(f, "a database name starts with a lowercase letter"),
            Self::Char(c) => write!(f, "a database name uses only a-z, 0-9, - and _ (not {c:?})"),
        }
    }
}

/// `[a-z][a-z0-9_-]{0,62}` — the same alphabet as Cloudflare's own D1 names.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(NameError::TooLong);
    }
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return Err(NameError::Start);
    }
    if let Some(c) = chars.find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')) {
        return Err(NameError::Char(c));
    }
    Ok(())
}

/// One database, as its owner sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct D1Database {
    pub id: String,
    pub name: String,
    /// The primary's region (one of [`LOCATIONS`]).
    pub location: String,
    /// The account that created it — the owner, or one of the owner's bots.
    pub created_by: String,
    pub created_at: i64,
    /// Bytes on disk the last time the platform looked.
    pub size_bytes: i64,
    /// Set once deleted. It stays restorable until `purge_after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_after: Option<i64>,
}

/// What one person may hold, and how much of it is in use. Bots draw on their
/// owner's allowance — databases belong to people.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct D1Quota {
    pub max_databases: u32,
    pub databases: u32,
    pub max_bytes: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct D1Listing {
    pub databases: Vec<D1Database>,
    /// Deleted ones still inside their restore window.
    pub deleted: Vec<D1Database>,
    pub quota: D1Quota,
}

/// A point a database can be put back to (Cloudflare Time Travel bookmark).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct D1RestorePoint {
    pub bookmark: String,
    pub reason: String,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct D1DatabaseDetail {
    pub database: D1Database,
    pub restore_points: Vec<D1RestorePoint>,
}

/// `restoreDatabase`'s answer. `previous_bookmark` undoes the restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct D1Restored {
    pub bookmark: String,
    pub previous_bookmark: String,
}

/// `events.databasesChanged` — the owner's list moved; refetch it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct D1Changed {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("ops-garden-db").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("notes_2026").is_ok());
        assert_eq!(validate_name(""), Err(NameError::Empty));
        assert_eq!(validate_name("Garden"), Err(NameError::Start));
        assert_eq!(validate_name("1db"), Err(NameError::Start));
        assert_eq!(validate_name("-db"), Err(NameError::Start));
        assert_eq!(validate_name("my db"), Err(NameError::Char(' ')));
        assert_eq!(validate_name("ops.db"), Err(NameError::Char('.')));
        assert_eq!(validate_name("园地"), Err(NameError::Start));
        assert!(validate_name(&"a".repeat(63)).is_ok());
        assert_eq!(validate_name(&"a".repeat(64)), Err(NameError::TooLong));
    }
}
