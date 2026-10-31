//! `mafold wallet` — the token wallet from the CLI. Currency = model id
//! ("claude-fable-5"); 1 unit = 1 output-equivalent unit of that model, which
//! is a token for everything billed per token and whatever the vendor actually
//! meters otherwise — `rates` prints each currency's own unit rather than
//! captioning the whole column "tokens".
//! Amounts accept 1M/2.5B/1000 shorthand. Account-symmetric: `mint` works for
//! anyone to TYPE but only the @mafold first-party account passes the server
//! check — there is no privileged client path.

use anyhow::{bail, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use mafold_core::mafold_types::units::{self, EN};

#[derive(Subcommand)]
pub enum WalletCmd {
    /// Show your balances.
    Balance,
    /// Transfer tokens: `mafold wallet transfer @friend 1M claude-fable-5`.
    Transfer {
        to: String,
        amount: String,
        currency: String,
        /// Optional note recorded on both ledgers.
        #[arg(long)]
        memo: Option<String>,
    },
    /// Convert between models at the official-price ratio:
    /// `mafold wallet convert 1M claude-opus-4-8 claude-fable-5`.
    Convert {
        amount: String,
        from: String,
        to: String,
    },
    /// Show the official price table (USD / 1M units, per currency).
    Rates,
    /// Show your ledger (newest first).
    History {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// List / revoke standing debit authorizations you've given.
    Grants {
        /// Revoke this spender's authorization.
        #[arg(long)]
        revoke: Option<String>,
    },
    /// First-party issuance (server-checked: @mafold only).
    Mint {
        to: String,
        amount: String,
        currency: String,
        #[arg(long)]
        memo: Option<String>,
    },
}

/// "1M" / "2.5B" / "1000" → raw tokens. Read as decimal digits, not a float:
/// 4.1 × 1e6 is 4099999.9999999995 in floating point, and flooring that sent
/// one token less than the "4.1M" the wallet itself prints. A fraction finer
/// than one token is dropped.
fn parse_amount(s: &str) -> Result<i64> {
    let s = s.trim();
    let bad = || anyhow::anyhow!("bad amount `{s}` — try 1M / 2.5B / 1000");
    let (num, zeros) = match s.chars().last().map(|c| c.to_ascii_lowercase()) {
        Some('k') => (&s[..s.len() - 1], 3),
        Some('m') => (&s[..s.len() - 1], 6),
        Some('b') => (&s[..s.len() - 1], 9),
        _ => (s, 0),
    };
    let (whole, frac) = num.trim().split_once('.').unwrap_or((num.trim(), ""));
    let digits = |d: &str| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !(frac.is_empty() || digits(frac)) {
        return Err(bad());
    }
    let frac: String = frac.chars().chain(std::iter::repeat('0')).take(zeros).collect();
    let raw = whole
        .parse::<i64>()
        .ok()
        .and_then(|w| w.checked_mul(10i64.pow(zeros as u32)))
        .and_then(|w| w.checked_add(if frac.is_empty() { 0 } else { frac.parse::<i64>().ok()? }))
        .ok_or_else(bad)?;
    if raw <= 0 {
        bail!("amount must be positive");
    }
    Ok(raw)
}

/// A wallet amount on the shared ruler: "2.5M" / "1B" / "999" — one decimal,
/// the same as every client's wallet (owner, 2026-10-22: no exception).
fn fmt(n: i64) -> String {
    EN.compact(n as f64)
}

fn items(v: &Value) -> Vec<Value> {
    v["items"].as_array().cloned().unwrap_or_default()
}

/// `walletBalances.passes` that can still pay: active, something left. An
/// older api sends none.
fn live_passes(v: &Value) -> Vec<Value> {
    v["passes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p["state"].as_str() == Some("active") && p["left"].as_i64().unwrap_or(0) > 0)
        .collect()
}

/// One Pass as a line: "Premium Pass  $8.40 of $10.00 left" when it was sold
/// at a face value, else what's left in its own units.
fn pass_line(p: &Value) -> String {
    let title = p["title"].as_str().unwrap_or("Pass");
    let money = |c: i64| format!("${}.{:02}", c / 100, c % 100);
    match (p["left_usd_cents"].as_i64(), p["face_usd_cents"].as_i64()) {
        (Some(left), Some(face)) => format!("{title:<14}  {} of {} left", money(left), money(face)),
        _ => format!(
            "{title:<14}  {} {} left",
            fmt(p["left"].as_i64().unwrap_or(0)),
            p["currency"].as_str().unwrap_or("?")
        ),
    }
}

/// Each currency's own unit ("token", "second"), from `walletRates`. A failed
/// call is no units, and every amount then reads as tokens.
async fn units_of(client: &Client) -> std::collections::HashMap<String, String> {
    let Ok(r) = client.call("walletRates", json!({})).await else { return Default::default() };
    items(&r)
        .iter()
        .filter_map(|row| {
            let cur = row["currency"].as_str()?;
            Some((cur.to_string(), row["unit"].as_str().unwrap_or("token").to_string()))
        })
        .collect()
}

/// An amount with its unit — "500K gpt-5.6-terra tokens": a bare "500K
/// gpt-5.6-terra" left the reader asking 500K of what (owner, 2026-10-22).
/// The id stays: it is what the reader types back.
fn amount_of(n: i64, cur: &str, units: &std::collections::HashMap<String, String>) -> String {
    EN.amount(n as f64, cur, units.get(cur).map(String::as_str), false)
}

/// A unit as a plural noun: "token" → "tokens", "second" → "seconds".
///
/// Add an "s" unless there already is one. The unit vocabulary is open by
/// design (the server takes any word), so there is nothing to look the answer
/// up IN — and an English pluralisation table would be a hardcoded list of the
/// exact kind the open vocabulary exists to avoid. This is wrong for an
/// irregular noun; no vendor bills in one, and a wrong plural is a typo where a
/// wrong UNIT would be a lie about what somebody holds.
fn plural(unit: &str) -> String {
    if unit.ends_with('s') { unit.to_string() } else { format!("{unit}s") }
}

pub async fn run(cmd: WalletCmd, client: &Client) -> Result<()> {
    match cmd {
        WalletCmd::Balance => {
            let r = client.call("walletBalances", json!({})).await?;
            let rows = items(&r);
            let passes = live_passes(&r);
            if rows.is_empty() && passes.is_empty() {
                println!("(empty — ask a friend for a transfer, e.g. `mafold wallet transfer @you 1M claude-fable-5`)");
                return Ok(());
            }
            for b in rows {
                println!(
                    "  {:>10}  {}",
                    fmt(b["amount"].as_i64().unwrap_or(0)),
                    b["currency"].as_str().unwrap_or("?")
                );
            }
            // Passes after the balances, apart: only you can spend them, and
            // `transfer` / `convert` never see them.
            if !passes.is_empty() {
                println!("\n  Passes (only you can spend them; they pay first and can't be sent)");
                for p in passes {
                    println!("  {}", pass_line(&p));
                }
            }
        }
        WalletCmd::Transfer {
            to,
            amount,
            currency,
            memo,
        } => {
            let amt = parse_amount(&amount)?;
            client
                .call(
                    "walletTransfer",
                    json!({ "to": to, "currency": currency, "amount": amt, "memo": memo }),
                )
                .await?;
            println!("✓ sent {} → {to}", amount_of(amt, &currency, &units_of(client).await));
        }
        WalletCmd::Convert { amount, from, to } => {
            let amt = parse_amount(&amount)?;
            let r = client
                .call(
                    "walletConvert",
                    json!({ "currency": from, "to_currency": to, "amount": amt }),
                )
                .await?;
            let units = units_of(client).await;
            println!(
                "✓ {} → {}  (rate {:.4})",
                amount_of(amt, &from, &units),
                amount_of(r["to_amount"].as_i64().unwrap_or(0), &to, &units),
                r["rate"].as_f64().unwrap_or(0.0)
            );
        }
        WalletCmd::Rates => {
            let r = client.call("walletRates", json!({})).await?;
            // "USD / 1M units" in the header, and each row names its own unit,
            // because they are no longer all the same word: a currency the
            // vendor bills per second of video prices per 1M SECONDS through
            // this same table. Printing "tokens" over that column would be the
            // one thing this field exists to stop.
            println!("official prices (USD / 1M units):");
            for row in items(&r) {
                let unit = row["unit"].as_str().unwrap_or("token");
                println!(
                    "  {:<22} in {:<8} out {:<10} per 1M {}",
                    row["currency"].as_str().unwrap_or("?"),
                    units::usd(row["usd_in"].as_f64().unwrap_or(0.0)),
                    units::usd(row["usd_out"].as_f64().unwrap_or(0.0)),
                    plural(unit)
                );
            }
        }
        WalletCmd::History { limit } => {
            let r = client
                .call("walletHistory", json!({ "limit": limit }))
                .await?;
            let rows = items(&r);
            if rows.is_empty() {
                println!("(no transactions)");
                return Ok(());
            }
            let units = units_of(client).await;
            for tx in rows {
                let kind = tx["kind"].as_str().unwrap_or("?");
                // The SERVER stamps direction (`credit`). Keeping our own list
                // of kinds here is what drew claimed red packets as withdrawals;
                // the fallback is the naming rule walletHistory documents.
                let credit = tx["credit"]
                    .as_bool()
                    .unwrap_or_else(|| kind == "mint" || kind.ends_with("_in"));
                let sign = if credit { "+" } else { "−" };
                let peer = tx["peer"]
                    .as_str()
                    .map(|p| format!(" @{p}"))
                    .unwrap_or_default();
                // `auto`: the wallet ran this convert to fund a payment — the
                // server's field (it used to ride in the memo as "auto"), and
                // the memo is then the bill it paid for.
                let auto = tx["auto"].as_bool() == Some(true);
                let memo = match (auto, tx["memo"].as_str()) {
                    (true, Some(m)) => format!("  (auto · {m})"),
                    (true, None) => "  (auto)".to_string(),
                    (false, Some(m)) => format!("  ({m})"),
                    (false, None) => String::new(),
                };
                let conv = match (tx["to_amount"].as_i64(), tx["to_currency"].as_str()) {
                    (Some(a), Some(c)) => format!(" → +{}", amount_of(a, c, &units)),
                    _ => String::new(),
                };
                println!(
                    "  {:<10} {sign}{}{conv}{peer}{memo}",
                    kind,
                    amount_of(tx["amount"].as_i64().unwrap_or(0), tx["currency"].as_str().unwrap_or("?"), &units),
                );
            }
        }
        WalletCmd::Grants { revoke } => {
            if let Some(spender) = revoke {
                let r = client
                    .call("walletGrantRevoke", json!({ "spender": spender }))
                    .await?;
                if r["removed"].as_bool().unwrap_or(false) {
                    println!("✓ revoked @{spender}");
                } else {
                    println!("(no grant for @{spender})");
                }
                return Ok(());
            }
            let r = client.call("walletGrants", json!({})).await?;
            let rows = items(&r);
            if rows.is_empty() {
                println!("(no one is authorized to debit your wallet)");
                return Ok(());
            }
            for g in rows {
                // A grant's cap belongs to no one currency: "1M tokens".
                let cap = g["monthly_cap"]
                    .as_i64()
                    .map(|n| EN.tokens(n as f64))
                    .unwrap_or_else(|| "uncapped".into());
                println!(
                    "  @{:<20} cap {cap:<12} used this month {}",
                    g["spender"].as_str().unwrap_or("?"),
                    EN.tokens(g["used_month"].as_i64().unwrap_or(0) as f64)
                );
            }
        }
        WalletCmd::Mint {
            to,
            amount,
            currency,
            memo,
        } => {
            let amt = parse_amount(&amount)?;
            client
                .call(
                    "walletMint",
                    json!({ "to": to, "currency": currency, "amount": amt, "memo": memo }),
                )
                .await?;
            println!("✓ minted {} → {to}", amount_of(amt, &currency, &units_of(client).await));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wallet holding only a Pass isn't empty, and a Pass reads in the
    /// dollars it was sold for.
    #[test]
    fn passes_print_apart_and_at_their_face() {
        let r = json!({ "items": [], "passes": [
            { "title": "Premium Pass", "state": "active", "left": 700000, "currency": "gpt-5.6-terra", "left_usd_cents": 840, "face_usd_cents": 1000 },
            { "title": "Kimi Pass", "state": "active", "left": 47600, "currency": "kimi-k3" },
            { "title": "Premium Pass", "state": "ended", "left": 0, "currency": "gpt-5.6-terra" },
        ]});
        let live = live_passes(&r);
        assert_eq!(live.len(), 2);
        assert_eq!(pass_line(&live[0]), "Premium Pass    $8.40 of $10.00 left");
        assert!(pass_line(&live[1]).starts_with("Kimi Pass       "), "{}", pass_line(&live[1]));
        assert!(pass_line(&live[1]).ends_with("kimi-k3 left"));
        assert!(live_passes(&json!({ "items": [] })).is_empty(), "an older api sends none");
    }

    #[test]
    fn amounts_read_exactly_what_the_wallet_prints() {
        assert_eq!(parse_amount("4.1M").unwrap(), 4_100_000, "not 4,099,999");
        assert_eq!(parse_amount("8.2m").unwrap(), 8_200_000);
        assert_eq!(parse_amount("2.5B").unwrap(), 2_500_000_000);
        assert_eq!(parse_amount("64K").unwrap(), 64_000);
        assert_eq!(parse_amount(" 1000 ").unwrap(), 1000);
        assert_eq!(parse_amount("1.5").unwrap(), 1, "a fraction of a token is dropped");
        assert_eq!(parse_amount("1.23456M").unwrap(), 1_234_560);
        for bad in ["abc", "1,000", "-5", "0", "0.4", "1e3", "M", "1.2.3K"] {
            assert!(parse_amount(bad).is_err(), "{bad}");
        }
        assert_eq!(fmt(4_100_000), "4.1M", "and the wallet prints it back the same");
    }

    #[test]
    fn amounts_say_their_unit() {
        let units: std::collections::HashMap<String, String> =
            [("seedance-2".to_string(), "second".to_string())].into();
        assert_eq!(amount_of(500_000, "gpt-5.6-terra", &units), "500K gpt-5.6-terra tokens");
        assert_eq!(amount_of(1, "opus-5.5", &units), "1 opus-5.5 token");
        assert_eq!(amount_of(30, "seedance-2", &units), "30 seedance-2 seconds");
        assert_eq!(
            amount_of(4_100_000, "opus-5.5", &Default::default()),
            "4.1M opus-5.5 tokens",
            "no price list ⇒ token"
        );
        assert_eq!(EN.tokens(2_000_000.0), "2M tokens", "a grant's cap owns no currency");
    }
}
