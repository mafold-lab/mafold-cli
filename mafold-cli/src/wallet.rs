//! `mafold wallet` — the Mafold wallet from the CLI, in dollars (ops 11-11):
//! `balance` is one dollar figure, `transfer` sends dollars, and `history` and
//! `grants` read in dollars at each model's official output price — the way
//! every client's wallet shows them.
//! Underneath, the ledger still keeps each model's own units (currency = model
//! id, 1 unit = 1 output-equivalent unit of that model); `rates`, `convert` and
//! `mint` still speak those, with 1M/2.5B/1000 shorthand. Account-symmetric:
//! `mint` works for anyone to TYPE but only the @mafold first-party account
//! passes the server check — there is no privileged client path.

use anyhow::{bail, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;
use mafold_core::mafold_types::units::{self, EN};

#[derive(Subcommand)]
pub enum WalletCmd {
    /// Show your balance, in dollars.
    Balance,
    /// Send dollars from your wallet: `mafold wallet transfer @friend 5` (or `5.25`, `$5`).
    Transfer {
        to: String,
        /// Dollars: `5`, `5.25` or `$5`.
        amount: String,
        /// A model id here is how transfers were sent before they were in
        /// dollars; it is refused with the way to say it now.
        #[arg(hide = true)]
        currency: Option<String>,
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

/// "5" / "5.25" / "$5" → cents: the amount a transfer sends. A cent is the
/// finest one is stated in — anything finer is refused, not rounded into an
/// amount nobody typed.
fn parse_dollars(s: &str) -> Result<i64> {
    let t = s.trim().trim_start_matches('$').trim();
    let bad = || anyhow::anyhow!("bad amount `{s}` — dollars, like 5 or 5.25");
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && frac.is_empty()) || !digits(whole) || !digits(frac) || frac.len() > 2 {
        return Err(bad());
    }
    let whole: i64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| bad())? };
    let frac: i64 = format!("{frac:0<2}").parse().map_err(|_| bad())?;
    let cents = whole.checked_mul(100).and_then(|w| w.checked_add(frac)).ok_or_else(bad)?;
    if cents <= 0 {
        bail!("amount must be positive");
    }
    Ok(cents)
}

/// Each currency's official output price per 1M of its units, from
/// `walletRates`: what turns a holding into dollars, the way the wallet does.
async fn prices_of(client: &Client) -> std::collections::HashMap<String, f64> {
    let Ok(r) = client.call("walletRates", json!({})).await else { return Default::default() };
    items(&r)
        .iter()
        .filter_map(|row| Some((row["currency"].as_str()?.to_string(), row["usd_out"].as_f64().filter(|p| *p > 0.0)?)))
        .collect()
}

/// `n` of `cur` in dollars, when there is a price for it.
fn usd_of(n: i64, cur: &str, prices: &std::collections::HashMap<String, f64>) -> Option<f64> {
    prices.get(cur).map(|p| n as f64 * p / 1_000_000.0)
}

/// Every balance at its official output price, added up: the one number the
/// wallet shows. A holding with no price adds nothing.
fn total_usd(rows: &[Value], prices: &std::collections::HashMap<String, f64>) -> f64 {
    rows.iter().filter_map(|b| usd_of(b["amount"].as_i64()?, b["currency"].as_str()?, prices)).sum()
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
                println!("  $0 — add money with Deposit in your Mafold wallet");
                return Ok(());
            }
            println!("  {}", units::usd(total_usd(&rows, &prices_of(client).await)));
            // Passes after the balance, apart: only you can spend them, and
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
            if let Some(cur) = currency {
                bail!(
                    "transfers are in dollars now: `mafold wallet transfer {to} 5` sends $5.00 out of whatever you hold \
                     (`{amount} {cur}` was an amount of one model)"
                );
            }
            let cents = parse_dollars(&amount)?;
            let usd = format!("{}.{:02}", cents / 100, cents % 100);
            client.call("walletTransfer", json!({ "to": to, "usd": usd, "memo": memo })).await?;
            println!("✓ sent {} → {to}", units::usd(cents as f64 / 100.0));
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
            let prices = prices_of(client).await;
            for tx in rows {
                let kind = tx["kind"].as_str().unwrap_or("?");
                // A convert turns one model's units into another's at the same
                // worth: in dollars it moves nothing, and the wallet lists none.
                if kind == "convert" {
                    continue;
                }
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
                let memo = tx["memo"].as_str().map(|m| format!("  ({m})")).unwrap_or_default();
                // In dollars at the official output price; a model with no
                // price keeps its own units, which is all there is to say.
                let (n, cur) = (tx["amount"].as_i64().unwrap_or(0), tx["currency"].as_str().unwrap_or("?"));
                let amount = usd_of(n, cur, &prices).map(units::usd).unwrap_or_else(|| amount_of(n, cur, &units));
                println!("  {kind:<10} {sign}{amount}{peer}{memo}");
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
            // A grant's cap counts units of whatever each bill was in. The
            // wallet states it in dollars at the price of the model the
            // spender last billed in; one that never billed has no price yet,
            // and its cap stays a count: "1M tokens".
            let prices = prices_of(client).await;
            let history = match client.call("walletHistory", json!({ "limit": 200 })).await {
                Ok(h) => items(&h),
                Err(_) => Vec::new(),
            };
            for g in rows {
                let spender = g["spender"].as_str().unwrap_or("?");
                let price = history
                    .iter()
                    .find(|t| t["kind"].as_str() == Some("debit") && t["peer"].as_str() == Some(spender))
                    .and_then(|t| prices.get(t["currency"].as_str()?))
                    .copied();
                let money = |n: i64| match price {
                    Some(p) => units::usd(n as f64 * p / 1_000_000.0),
                    None => EN.tokens(n as f64),
                };
                let cap = g["monthly_cap"].as_i64().map(money).unwrap_or_else(|| "uncapped".into());
                println!("  @{spender:<20} cap {cap:<12} used this month {}", money(g["used_month"].as_i64().unwrap_or(0)));
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

    /// A transfer is stated in dollars, to the cent — and nothing else passes
    /// for one (a model amount like `1M` least of all).
    #[test]
    fn transfers_read_dollars_to_the_cent() {
        for (s, c) in [("5", 500), ("5.25", 525), ("$5", 500), (" $0.50 ", 50), (".5", 50), ("5.", 500)] {
            assert_eq!(parse_dollars(s).unwrap(), c, "{s:?}");
        }
        for bad in ["0", "0.00", "-5", "5.255", "1M", "five", "", "$", "1e3", "5,00"] {
            assert!(parse_dollars(bad).is_err(), "{bad:?}");
        }
    }

    /// The balance is one number: every holding at its official output price,
    /// added up, the way every client's wallet shows it.
    #[test]
    fn the_balance_is_one_dollar_figure() {
        let prices: std::collections::HashMap<String, f64> =
            [("gpt-5.6-terra".to_string(), 12.0), ("claude-fable-5".to_string(), 25.0)].into();
        let rows = vec![
            json!({ "currency": "gpt-5.6-terra", "amount": 1_000_000 }),
            json!({ "currency": "claude-fable-5", "amount": 200_000 }),
            json!({ "currency": "unpriced-model", "amount": 5_000 }),
        ];
        assert_eq!(units::usd(total_usd(&rows, &prices)), "$17.00");
        assert_eq!(usd_of(1, "unpriced-model", &prices), None);
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
