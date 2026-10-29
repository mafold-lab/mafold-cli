//! What may sign a new agent in — the Rust half of `client-shared/newAgent.ts`
//! `signInRuns`, held to the same table (`client-shared/newAgent.cases.json`)
//! so a combo the New Agent panel offers is one `createBot` takes, and the
//! other way round.
//!
//! On Mafold Cloud only the wallet. On a computer a login runs only its own
//! runtime (a Claude login runs Claude Code, a Codex login runs Codex, …), and
//! the wallet fuels any runtime but Kimi Code — once the router can
//! (`wallet_on_computers`).

/// The runtimes New Agent offers, each with the one login that runs it.
pub const RUNTIME_LOGINS: &[(&str, &str)] = &[
    ("claude-code", "claude"),
    ("codex", "codex"),
    ("kimi-code", "kimi"),
    ("opencode", "opencode-go"),
];

/// Every sign-in New Agent knows.
pub const SIGN_INS: &[&str] = &["claude", "codex", "kimi", "opencode-go", "wallet"];

/// Whether `sign_in` may sign in an agent that runs on Mafold Cloud (`cloud`)
/// or on a computer with `runtime`.
pub fn sign_in_runs(cloud: bool, runtime: Option<&str>, sign_in: &str, wallet_on_computers: bool) -> bool {
    if cloud {
        return sign_in == "wallet";
    }
    let Some(runtime) = runtime.filter(|r| RUNTIME_LOGINS.iter().any(|(id, _)| id == r)) else {
        return false;
    };
    if sign_in == "wallet" {
        return wallet_on_computers && runtime != "kimi-code";
    }
    RUNTIME_LOGINS.iter().any(|(id, login)| *id == runtime && *login == sign_in)
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
            let wallet = c["wallet_on_computers"].as_bool().unwrap();
            assert_eq!(sign_in_runs(cloud, runtime, sign_in, wallet), c["ok"].as_bool().unwrap(), "{c}");
        }
    }

    #[test]
    fn every_sign_in_the_table_names_is_known() {
        let table: Value = serde_json::from_str(CASES).unwrap();
        for c in table["cases"].as_array().unwrap() {
            assert!(SIGN_INS.contains(&c["sign_in"].as_str().unwrap()), "{c}");
        }
    }
}
