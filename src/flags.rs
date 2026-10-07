//! The server's feature flags as THIS daemon sees them: `getFlags` when it
//! starts and whenever it reconnects, `events.flagsChanged` in between — the
//! same delivery every other client gets (.docs/feature-flags.md). A flag the
//! server has no record for falls back to its default in mafold-core's
//! registry, the one home of a flag's default (invariant ①).
//!
//! Evaluated for the bot's own handle; an allowlist entry naming its owner
//! covers it too, so a flag is switched on per person from the server.

use std::collections::BTreeMap;
use std::sync::RwLock;

struct Held {
    values: BTreeMap<String, bool>,
    version: u64,
}

static HELD: RwLock<Held> = RwLock::new(Held { values: BTreeMap::new(), version: 0 });

/// Take a server `FlagState` (`{values, version}`). One older than what is
/// held is ignored; version 0 ("unversioned") always applies — as in the core
/// engine.
pub fn ingest(state: &serde_json::Value) {
    let Ok(state) = serde_json::from_value::<mafold_core::mafold_types::FlagState>(state.clone()) else {
        return;
    };
    let mut held = HELD.write().unwrap_or_else(|e| e.into_inner());
    if state.version == 0 || state.version > held.version {
        held.values = state.values;
        held.version = state.version;
    }
}

/// Server value, else the registry default.
pub fn enabled(key: &str) -> bool {
    let held = HELD.read().unwrap_or_else(|e| e.into_inner());
    held.values.get(key).copied().unwrap_or_else(|| mafold_core::flags::prod_default(key))
}

/// Ask the server again. A failure keeps what is held (defaults at first).
pub async fn refresh(client: &crate::client::Client) {
    match client.call("getFlags", serde_json::json!({})).await {
        Ok(state) => ingest(&state),
        Err(e) => eprintln!("⚠ getFlags failed, keeping the flags already held: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, because the held flags are process-wide.
    #[test]
    fn server_value_then_default_and_no_going_back_in_version() {
        let key = "windowsBackground";
        assert!(!mafold_core::flags::prod_default(key), "registered, off by default");
        ingest(&serde_json::json!({ "values": {}, "version": 0 }));
        assert!(!enabled(key), "no server record: the registry default");

        ingest(&serde_json::json!({ "values": { key: true }, "version": 5 }));
        assert!(enabled(key), "the server turned it on");

        ingest(&serde_json::json!({ "values": { key: false }, "version": 4 }));
        assert!(enabled(key), "an older push is ignored");

        ingest(&serde_json::json!({ "values": { key: false }, "version": 6 }));
        assert!(!enabled(key), "a newer one is not");

        ingest(&serde_json::json!({ "garbage": true }));
        assert!(!enabled(key), "something that isn't a FlagState changes nothing");
        assert!(!enabled("no-such-flag"));
    }
}
