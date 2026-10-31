//! What may sign a new agent in — the Rust half of `client-shared/newAgent.ts`
//! `signInRuns`, held to the same table (`client-shared/newAgent.cases.json`)
//! so a combo the New Agent panel offers is one `createBot` takes, and the
//! other way round.
//!
//! On Mafold Cloud only the wallet. On a computer a login runs only its own
//! runtime (a Claude login runs Claude Code, a Codex login runs Codex, …).
//! OpenCode Go runs Mafold Harness (Mafold's pinned OpenCode, `opencode`) and
//! your own OpenCode install (`opencode-own`) where the owner has
//! `openCodeGo` on; the wallet fuels Mafold Harness alone, where the owner has
//! `walletOnComputers` on.

/// The runtimes New Agent offers on a computer, each with what can sign it in.
pub const RUNTIME_SIGN_INS: &[(&str, &[&str])] = &[
    ("claude-code", &["claude"]),
    ("codex", &["codex"]),
    ("kimi-code", &["kimi"]),
    // Mafold Harness: no login of its own — your OpenCode Go plan, or the
    // wallet (the router serves it on /v1/chat/completions, which OpenCode
    // speaks; Claude Code and Codex wait for PR 4d / 4e).
    ("opencode", &["opencode-go", "wallet"]),
    // Your own OpenCode: on OpenCode Go.
    ("opencode-own", &["opencode-go"]),
];

/// Every sign-in New Agent knows.
pub const SIGN_INS: &[&str] = &["claude", "codex", "kimi", "opencode-go", "wallet"];

/// The owner's flags that open a sign-in on a computer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gates {
    /// `walletOnComputers`: the wallet may fuel Mafold Harness.
    pub wallet_on_computers: bool,
    /// `openCodeGo`: OpenCode Go may sign in Mafold Harness and your own OpenCode.
    pub opencode_go: bool,
}

impl Gates {
    /// Whether `sign_in` is open to this owner at all.
    fn opens(self, sign_in: &str) -> bool {
        match sign_in {
            "wallet" => self.wallet_on_computers,
            "opencode-go" => self.opencode_go,
            _ => true,
        }
    }
}

/// Whether `sign_in` may sign in an agent that runs on Mafold Cloud (`cloud`)
/// or on a computer with `runtime`.
pub fn sign_in_runs(cloud: bool, runtime: Option<&str>, sign_in: &str, gates: Gates) -> bool {
    if cloud {
        return sign_in == "wallet";
    }
    let Some((_, ins)) = runtime.and_then(|r| RUNTIME_SIGN_INS.iter().find(|(id, _)| *id == r)) else {
        return false;
    };
    ins.contains(&sign_in) && gates.opens(sign_in)
}

/// A runtime with no login of its own on the computer: an agent on it runs on
/// what it was signed in with and nothing else, so it can't be made without
/// one. The others fall back to the computer's own login, as they always did.
pub fn needs_sign_in(runtime: &str) -> bool {
    matches!(runtime, "opencode" | "opencode-own")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const CASES: &str = include_str!("../../client-shared/newAgent.cases.json");

    #[test]
    fn the_shared_table_holds() {
        let table: Value = serde_json::from_str(CASES).expect("newAgent.cases.json parses");
        let cases = table["cases"].as_array().expect("cases");
        assert!(cases.len() >= 20, "the table is the whole rule, not a sample");
        for c in cases {
            let cloud = c["where"] == "cloud";
            let runtime = c["runtime"].as_str();
            let sign_in = c["sign_in"].as_str().unwrap();
            let gates = Gates {
                wallet_on_computers: c["wallet_on_computers"].as_bool().unwrap(),
                // Absent = off: the cases from before OpenCode Go never had it.
                opencode_go: c["opencode_go"].as_bool().unwrap_or(false),
            };
            assert_eq!(sign_in_runs(cloud, runtime, sign_in, gates), c["ok"].as_bool().unwrap(), "{c}");
        }
    }

    #[test]
    fn every_sign_in_the_table_names_is_known() {
        let table: Value = serde_json::from_str(CASES).unwrap();
        for c in table["cases"].as_array().unwrap() {
            assert!(SIGN_INS.contains(&c["sign_in"].as_str().unwrap()), "{c}");
        }
        for (_, ins) in RUNTIME_SIGN_INS {
            assert!(ins.iter().all(|s| SIGN_INS.contains(s)), "{ins:?}");
        }
    }

    #[test]
    fn a_runtime_without_a_login_of_its_own_needs_a_sign_in() {
        assert!(needs_sign_in("opencode") && needs_sign_in("opencode-own"));
        assert!(!needs_sign_in("claude-code") && !needs_sign_in("codex") && !needs_sign_in("kimi-code"));
    }
}
