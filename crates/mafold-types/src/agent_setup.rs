//! What may sign a new agent in — the Rust half of `client-shared/newAgent.ts`
//! `signInRuns`, held to the same table (`client-shared/newAgent.cases.json`)
//! so a combo the New Agent panel offers is one `createBot` takes, and the
//! other way round.
//!
//! On Mafold Cloud the wallet, or your own model key where the owner has
//! `cloudOwnKey` on (typed in, or handed over from Connections). On a computer a login runs only its own
//! runtime (a Claude login runs Claude Code, a Codex login runs Codex, …).
//! OpenCode Go runs Mafold Harness ([`MAFOLD_HARNESS`]: the loop the official
//! bots run on, carried by mafold-cli) where the owner has `openCodeGo` on;
//! the wallet fuels Mafold Harness alone, where the owner has
//! `walletOnComputers` on; and a Claude login runs Mafold Harness too — each
//! step on the real `claude` there — where the owner has `harnessOnClaude` on.
//!
//! Also here: the id a runtime goes by ([`runtime_id`]), for every side that
//! reads one an older build wrote.

/// Mafold Harness's runtime id. Until 11-20 it went by `opencode`, the name of
/// the OpenCode build it took over from on 11-03 (#1424): agents stored then,
/// an older mafold-cli's report and an older client's createBot still say it,
/// and [`runtime_id`] reads them as this.
pub const MAFOLD_HARNESS: &str = "mafold-harness";

/// Your own OpenCode install, a runtime until 11-20 (ops: "Remove it"). An
/// agent stored with it runs on Mafold Harness, on the same OpenCode Go plan
/// ([`runtime_id`]); a computer's report of it is dropped
/// (`computers::DeviceHarnesses::in_current_ids`).
pub const OWN_OPENCODE: &str = "opencode-own";

/// The id a runtime goes by now: Mafold Harness's old one (`opencode`) and
/// your own OpenCode's ([`OWN_OPENCODE`]) are [`MAFOLD_HARNESS`]; any other
/// id is itself. Ids come in lowercase (every caller trims and lowercases).
pub fn runtime_id(id: &str) -> &str {
    match id {
        "opencode" | OWN_OPENCODE => MAFOLD_HARNESS,
        other => other,
    }
}

/// The id a mafold-cli from before 11-20 knows a runtime by: Mafold Harness
/// only as `opencode` — and such a CLI refuses every turn of an agent whose
/// runtime it has never heard of. What `getMe` tells a daemon that doesn't
/// say it knows the new id (`knows_mafold_harness`).
pub fn runtime_id_before_1120(id: &str) -> &str {
    if id == MAFOLD_HARNESS {
        "opencode"
    } else {
        id
    }
}

/// The runtimes New Agent offers on a computer, each with what can sign it in.
pub const RUNTIME_SIGN_INS: &[(&str, &[&str])] = &[
    ("claude-code", &["claude"]),
    ("codex", &["codex"]),
    ("kimi-code", &["kimi"]),
    // Mafold Harness: no login of its own — your OpenCode Go plan, the wallet
    // (the router serves it on /v1/chat/completions, which its model client
    // speaks), or your Claude Code login, each step run by the real `claude`
    // on that computer.
    (MAFOLD_HARNESS, &["opencode-go", "wallet", "claude"]),
];

/// Every sign-in New Agent knows.
pub const SIGN_INS: &[&str] = &["claude", "codex", "kimi", "opencode-go", "wallet", KEY];

/// Your own model key on Mafold Cloud: an Anthropic- or OpenAI-format key and
/// the Base URL it's for, sealed on the server (`mafold-api` `own_key`).
pub const KEY: &str = "key";

/// The owner's flags that open a sign-in on a computer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gates {
    /// `walletOnComputers`: the wallet may fuel Mafold Harness.
    pub wallet_on_computers: bool,
    /// `openCodeGo`: OpenCode Go may sign in Mafold Harness.
    pub opencode_go: bool,
    /// `harnessOnClaude`: a Claude login may sign in Mafold Harness. Its own
    /// runtime, Claude Code, needs no flag.
    pub harness_on_claude: bool,
    /// `cloudOwnKey`: your own model key may pay for a Mafold Cloud agent.
    pub cloud_own_key: bool,
}

impl Gates {
    /// Whether `sign_in` is open to this owner on `runtime`.
    fn opens(self, runtime: &str, sign_in: &str) -> bool {
        match (runtime, sign_in) {
            (_, "wallet") => self.wallet_on_computers,
            (_, "opencode-go") => self.opencode_go,
            (MAFOLD_HARNESS, "claude") => self.harness_on_claude,
            _ => true,
        }
    }
}

/// Whether `sign_in` may sign in an agent that runs on Mafold Cloud (`cloud`)
/// or on a computer with `runtime`.
pub fn sign_in_runs(cloud: bool, runtime: Option<&str>, sign_in: &str, gates: Gates) -> bool {
    if cloud {
        return sign_in == "wallet" || (sign_in == KEY && gates.cloud_own_key);
    }
    let Some((id, ins)) = runtime.and_then(|r| RUNTIME_SIGN_INS.iter().find(|(id, _)| *id == r)) else {
        return false;
    };
    ins.contains(&sign_in) && gates.opens(id, sign_in)
}

/// A runtime with no login of its own on the computer: an agent on it runs on
/// what it was signed in with and nothing else, so it can't be made without
/// one. The others fall back to the computer's own login, as they always did.
pub fn needs_sign_in(runtime: &str) -> bool {
    runtime == MAFOLD_HARNESS
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
                // …nor the ones from before Mafold Harness on Claude Code.
                harness_on_claude: c["harness_on_claude"].as_bool().unwrap_or(false),
                // …nor the ones from before your own key on Mafold Cloud.
                cloud_own_key: c["cloud_own_key"].as_bool().unwrap_or(false),
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
        assert!(needs_sign_in(MAFOLD_HARNESS));
        assert!(!needs_sign_in("claude-code") && !needs_sign_in("codex") && !needs_sign_in("kimi-code"));
    }

    /// What older builds wrote reads as today's ids: Mafold Harness's old
    /// name, and your own OpenCode (gone, its agents on Mafold Harness).
    #[test]
    fn old_runtime_ids_read_as_todays() {
        assert_eq!(runtime_id("opencode"), MAFOLD_HARNESS);
        assert_eq!(runtime_id(OWN_OPENCODE), MAFOLD_HARNESS);
        for id in [MAFOLD_HARNESS, "claude-code", "codex", "kimi-code", "openclaw", "something-new"] {
            assert_eq!(runtime_id(id), id);
        }
        assert!(RUNTIME_SIGN_INS.iter().all(|(id, _)| runtime_id(id) == *id), "the table is in today's ids");
    }

    /// A CLI from before the rename hears the one id it knows; every other
    /// id goes to it as it is.
    #[test]
    fn a_cli_from_before_1120_hears_opencode() {
        assert_eq!(runtime_id_before_1120(MAFOLD_HARNESS), "opencode");
        for id in ["claude-code", "codex", "kimi-code", "openclaw"] {
            assert_eq!(runtime_id_before_1120(id), id);
        }
    }
}
