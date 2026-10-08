//! `copybot wallet-audit`: sort wallets into bots, snipers, traders and holders from
//! their own on-chain history, and decide which ones the engine may follow.
//!
//! Leaderboards and vendor PnL are candidate sources, not evidence. Every wallet is
//! judged on what our decoder can verify: the transactions it signed itself, the
//! round trips they add up to, and how long it holds. Re-run it weekly: strategies
//! decay and wallets change hands.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::time::Duration;

use chain::detect::{self, DetectedSwap};
use chain::model::ChainTx;
use chain::rpc::Rpc;
use chain::solana_sdk::pubkey::Pubkey;
use engine_core::stats::{median, profit_factor};
use engine_core::types::{lamports_to_sol, Side};
use serde::Serialize;
use serde_json::Value;

/// Thresholds behind every verdict, in one place.
pub mod rules {
    /// No transaction for this long: inactive.
    pub const INACTIVE_DAYS: f64 = 7.0;
    /// A bot fires many transactions at once and most of them fail.
    pub const BOT_FAILED_SHARE: f64 = 0.5;
    pub const BOT_PEAK_PER_SEC: usize = 10;
    /// 1,000 signatures inside this many seconds is machine speed whatever the failures.
    pub const BOT_SPAN_FOR_1000_SECS: i64 = 600;
    /// Fewer round trips than this and there is nothing to judge.
    pub const MIN_TRIPS_TO_JUDGE: usize = 10;
    /// Holds shorter than this cannot be copied at any feed speed.
    pub const SNIPER_HOLD_SECS: f64 = 20.0;
    /// Leader requirements.
    pub const LEADER_MIN_TRIPS: usize = 30;
    pub const LEADER_MIN_PROFIT_FACTOR: f64 = 1.5;
    pub const LEADER_MIN_HOLD_SECS: f64 = 30.0;
    pub const LEADER_MIN_COVERAGE: f64 = 0.9;
    /// Below this hold, only a fast (paid) feed copies the wallet well.
    pub const FAST_HOLD_SECS: f64 = 60.0;
}

// ------------------------------------------------------------------ input

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub wallet: Pubkey,
}

/// One wallet per line: `name: address`, `name address` or just `address`.
/// `#` starts a comment. Names default to the first 6 characters of the address.
pub fn parse_list(text: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = vec![];
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some((i, wallet)) = words
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, w)| w.parse::<Pubkey>().ok().map(|p| (i, p)))
        else {
            continue;
        };
        let name = words[..i]
            .join(" ")
            .trim_end_matches(':')
            .trim()
            .to_string();
        if out.iter().any(|c| c.wallet == wallet) {
            continue;
        }
        out.push(Candidate {
            name: if name.is_empty() {
                wallet.to_string()[..6].to_string()
            } else {
                name
            },
            wallet,
        });
    }
    out
}

// ------------------------------------------------------------------ activity

/// What the latest page of signatures says, failed transactions included.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Activity {
    pub sigs: usize,
    pub failed: usize,
    /// Seconds between the oldest and newest signature on the page.
    pub span_secs: i64,
    /// Most signatures in any one second.
    pub peak_per_sec: usize,
    /// Unix seconds of the newest signature (0 = none).
    pub last_ts: i64,
}

impl Activity {
    pub fn failed_share(&self) -> f64 {
        if self.sigs == 0 {
            0.0
        } else {
            self.failed as f64 / self.sigs as f64
        }
    }
    /// Fires many transactions at once and most fail, or 1,000 in minutes.
    pub fn looks_like_bot(&self) -> bool {
        (self.failed_share() >= rules::BOT_FAILED_SHARE
            && self.peak_per_sec >= rules::BOT_PEAK_PER_SEC)
            || (self.sigs >= 1000 && self.span_secs < rules::BOT_SPAN_FOR_1000_SECS)
    }
    pub fn per_day(&self) -> f64 {
        if self.span_secs <= 0 {
            self.sigs as f64 * 86_400.0
        } else {
            self.sigs as f64 * 86_400.0 / self.span_secs as f64
        }
    }
}

pub fn activity(page: &[Value]) -> Activity {
    let times: Vec<i64> = page
        .iter()
        .filter_map(|v| v["blockTime"].as_i64())
        .collect();
    let mut per_sec: HashMap<i64, usize> = HashMap::new();
    for t in &times {
        *per_sec.entry(*t).or_default() += 1;
    }
    Activity {
        sigs: page.len(),
        failed: page.iter().filter(|v| !v["err"].is_null()).count(),
        span_secs: match (times.iter().max(), times.iter().min()) {
            (Some(a), Some(b)) => a - b,
            _ => 0,
        },
        peak_per_sec: per_sec.values().copied().max().unwrap_or(0),
        last_ts: times.iter().copied().max().unwrap_or(0),
    }
}

// ------------------------------------------------------------------ history

/// The wallet's decoded swaps and how complete the read was.
#[derive(Debug, Clone, Default)]
pub struct History {
    /// Transactions fetched successfully.
    pub fetched: usize,
    /// Transactions the RPC never served, even after retrying.
    pub fetch_failed: usize,
    /// Fetched transactions the wallet signed itself.
    pub own_txs: usize,
    /// Fetched transactions sent by someone else (transfers to it, fee payouts, airdrops).
    pub foreign_txs: usize,
    /// (block time ms, swap), oldest first; base assets (USDC, staked SOL…) excluded.
    pub swaps: Vec<(i64, DetectedSwap)>,
}

impl History {
    pub fn coverage(&self) -> f64 {
        let n = self.fetched + self.fetch_failed;
        if n == 0 {
            1.0
        } else {
            self.fetched as f64 / n as f64
        }
    }
}

/// Signature pages read at most (a bot can bury its few successes under failures).
const MAX_PAGES: usize = 15;
/// Own transactions read from a wallet whose first page already shows a bot.
const BOT_SAMPLE: usize = 40;
/// Retries spent on transactions the RPC refused the first time.
const MAX_RETRIES: usize = 200;

/// Read the wallet's history until `want_own` of its own successful transactions are
/// decoded (or the read budget runs out). Returns the first signature page's activity
/// profile too.
pub async fn history(
    rpc: &Rpc,
    wallet: &Pubkey,
    want_own: usize,
) -> anyhow::Result<(Activity, History)> {
    let mut h = History::default();
    let mut act = Activity::default();
    let mut want_own = want_own;
    let mut budget = want_own.saturating_mul(3).max(100);
    let mut before: Option<String> = None;
    let mut missed: Vec<String> = vec![];
    'pages: for page_no in 0..MAX_PAGES {
        let page = rpc
            .signatures_for_address(wallet, 1000, before.as_deref())
            .await?;
        if page.is_empty() {
            break;
        }
        if page_no == 0 {
            act = activity(&page);
            if act.looks_like_bot() {
                // the verdict is already clear: a small sample of its trades is enough
                want_own = want_own.min(BOT_SAMPLE);
                budget = budget.min(BOT_SAMPLE * 3);
            }
        }
        before = page
            .last()
            .and_then(|v| v["signature"].as_str().map(String::from));
        let ok: Vec<String> = page
            .iter()
            .filter(|v| v["err"].is_null())
            .filter_map(|v| v["signature"].as_str().map(String::from))
            .collect();
        // ~4 transactions per second: what a free RPC tolerates per method
        for chunk in ok.chunks(4) {
            let futs = chunk.iter().map(|s| rpc.transaction_json(s));
            for (sig, res) in chunk.iter().zip(futures::future::join_all(futs).await) {
                match res {
                    Ok(v) if !v.is_null() => absorb(&mut h, wallet, &v),
                    _ => missed.push(sig.clone()),
                }
            }
            if h.own_txs >= want_own || h.fetched + missed.len() >= budget {
                break 'pages;
            }
            tokio::time::sleep(Duration::from_millis(900)).await;
        }
        if page.len() < 1000 {
            break;
        }
    }
    // second pass, slowly, over what the RPC refused
    for (i, sig) in missed.iter().enumerate() {
        if i >= MAX_RETRIES {
            h.fetch_failed += missed.len() - i;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        match rpc.transaction_json(sig).await {
            Ok(v) if !v.is_null() => absorb(&mut h, wallet, &v),
            _ => h.fetch_failed += 1,
        }
    }
    h.swaps.sort_by_key(|(t, _)| *t);
    Ok((act, h))
}

fn absorb(h: &mut History, wallet: &Pubkey, v: &Value) {
    let Ok(t) = ChainTx::from_rpc_json(v) else {
        h.fetch_failed += 1;
        return;
    };
    h.fetched += 1;
    if t.signers().contains(wallet) {
        h.own_txs += 1;
    } else {
        h.foreign_txs += 1;
    }
    for s in detect::swaps_by(&t, wallet) {
        // parking SOL in USDC or staking it says nothing about coin picking
        if !chain::base_assets::is_base_asset(&s.mint) {
            h.swaps.push((t.block_time_ms.unwrap_or(0), s));
        }
    }
}

// ------------------------------------------------------------------ trading

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Trading {
    pub swaps: usize,
    pub buys: usize,
    pub sells: usize,
    pub round_trips: usize,
    /// Realised: what it received minus what it paid, fees included, closed parts only.
    pub pnl_sol: f64,
    pub win_rate: f64,
    pub median_ret: f64,
    pub profit_factor: Option<f64>,
    pub median_hold_secs: f64,
    pub median_entry_sol: f64,
    /// Time from the first to the last decoded swap.
    pub window_secs: f64,
    pub open_positions: usize,
    pub venues: BTreeMap<String, u32>,
}

impl Trading {
    /// Realised SOL per day over the window read (the window is often well under a day).
    pub fn pnl_per_day(&self) -> Option<f64> {
        (self.window_secs >= 3600.0).then(|| self.pnl_sol * 86_400.0 / self.window_secs)
    }
}

struct Trip {
    entry_ms: i64,
    exit_ms: i64,
    cost: f64,
    proceeds: f64,
}

/// FIFO round trips per coin: each sale closes the matching share of the open cost.
pub fn trading(swaps: &[(i64, DetectedSwap)]) -> Trading {
    let mut open: HashMap<Pubkey, (i64, f64, u64)> = HashMap::new(); // first entry ms, cost, tokens
    let mut trips: Vec<Trip> = vec![];
    let mut venues: BTreeMap<String, u32> = BTreeMap::new();
    let mut entries: Vec<f64> = vec![];
    let (mut buys, mut sells) = (0usize, 0usize);
    for (t, s) in swaps {
        *venues.entry(format!("{:?}", s.venue)).or_default() += 1;
        match s.side {
            Side::Buy => {
                buys += 1;
                entries.push(lamports_to_sol(s.sol_amount));
                let e = open.entry(s.mint).or_insert((*t, 0.0, 0));
                e.1 += lamports_to_sol(s.sol_amount);
                e.2 += s.token_amount;
            }
            Side::Sell => {
                sells += 1;
                let Some(e) = open.get_mut(&s.mint) else {
                    continue; // bought before the window we read
                };
                if e.2 == 0 {
                    continue;
                }
                let frac = (s.token_amount as f64 / e.2 as f64).min(1.0);
                let cost = e.1 * frac;
                trips.push(Trip {
                    entry_ms: e.0,
                    exit_ms: *t,
                    cost,
                    proceeds: lamports_to_sol(s.sol_amount),
                });
                e.1 -= cost;
                e.2 = e.2.saturating_sub(s.token_amount);
                if e.2 == 0 {
                    open.remove(&s.mint);
                }
            }
        }
    }
    let rets: Vec<f64> = trips
        .iter()
        .filter(|t| t.cost > 0.0)
        .map(|t| t.proceeds / t.cost - 1.0)
        .collect();
    let pnls: Vec<f64> = trips.iter().map(|t| t.proceeds - t.cost).collect();
    let holds: Vec<f64> = trips
        .iter()
        .map(|t| (t.exit_ms - t.entry_ms) as f64 / 1000.0)
        .collect();
    let (first, last) = (
        swaps.first().map(|s| s.0).unwrap_or(0),
        swaps.last().map(|s| s.0).unwrap_or(0),
    );
    Trading {
        swaps: swaps.len(),
        buys,
        sells,
        round_trips: trips.len(),
        pnl_sol: pnls.iter().fold(0.0, |a, b| a + b),
        win_rate: if pnls.is_empty() {
            0.0
        } else {
            pnls.iter().filter(|p| **p > 0.0).count() as f64 / pnls.len() as f64
        },
        median_ret: median(&rets).unwrap_or(0.0),
        profit_factor: profit_factor(&pnls),
        median_hold_secs: median(&holds).unwrap_or(0.0),
        median_entry_sol: median(&entries).unwrap_or(0.0),
        window_secs: (last - first) as f64 / 1000.0,
        open_positions: open.len(),
        venues,
    }
}

// ------------------------------------------------------------------ verdict

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Trader,
    Holder,
    Sniper,
    Bot,
    Inactive,
    Unclear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Leader,
    Watch,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assessment {
    pub class: Class,
    pub verdict: Verdict,
    /// Why it is not a leader (empty for leaders).
    pub reasons: Vec<String>,
    /// Things to know even when it qualifies.
    pub notes: Vec<String>,
}

pub fn assess(a: &Activity, h: &History, t: &Trading, now_secs: i64) -> Assessment {
    use rules::*;
    let mut reasons = vec![];
    let mut notes = vec![];
    let idle_days = (now_secs - a.last_ts) as f64 / 86_400.0;
    let foreign = h.foreign_txs as f64 / (h.own_txs + h.foreign_txs).max(1) as f64;
    if foreign > 0.5 {
        notes.push(format!(
            "{:.0}% of the transactions read were sent by other wallets (transfers to it, fee payouts); only its own count",
            foreign * 100.0
        ));
    }
    let class = if a.sigs == 0 || idle_days > INACTIVE_DAYS {
        reasons.push(format!("no transaction for {idle_days:.0} days"));
        Class::Inactive
    } else if a.looks_like_bot() {
        reasons.push(format!(
            "machine speed: {:.0} transactions/day, {:.0}% failed, up to {} per second",
            a.per_day(),
            a.failed_share() * 100.0,
            a.peak_per_sec
        ));
        Class::Bot
    } else if t.buys == 0 && t.sells > 0
        || (t.sells >= 3 * t.buys.max(1) && t.round_trips < MIN_TRIPS_TO_JUDGE)
    {
        reasons.push(format!(
            "selling an existing position ({} buys, {} sells in the window)",
            t.buys, t.sells
        ));
        Class::Holder
    } else if t.round_trips < MIN_TRIPS_TO_JUDGE {
        reasons.push(format!(
            "{} decoded round trips: too few to judge",
            t.round_trips
        ));
        Class::Unclear
    } else if t.median_hold_secs < SNIPER_HOLD_SECS {
        reasons.push(format!(
            "median hold {:.0} s: cannot be copied at any feed speed",
            t.median_hold_secs
        ));
        Class::Sniper
    } else {
        Class::Trader
    };
    let verdict = match class {
        Class::Bot | Class::Sniper | Class::Holder | Class::Inactive => Verdict::Reject,
        Class::Unclear => Verdict::Watch,
        Class::Trader => {
            let mut watch = vec![];
            if t.pnl_sol <= 0.0 {
                reasons.push(format!("losing over the window ({:+.2} SOL)", t.pnl_sol));
            }
            if t.round_trips < LEADER_MIN_TRIPS {
                watch.push(format!(
                    "{} round trips (needs {LEADER_MIN_TRIPS})",
                    t.round_trips
                ));
            }
            if t.profit_factor
                .is_some_and(|pf| pf < LEADER_MIN_PROFIT_FACTOR)
                && t.pnl_sol > 0.0
            {
                watch.push(format!(
                    "profit factor {:.2} (needs {LEADER_MIN_PROFIT_FACTOR})",
                    t.profit_factor.unwrap_or(0.0)
                ));
            }
            if t.median_hold_secs < LEADER_MIN_HOLD_SECS {
                watch.push(format!(
                    "median hold {:.0} s (needs {LEADER_MIN_HOLD_SECS:.0})",
                    t.median_hold_secs
                ));
            }
            if h.coverage() < LEADER_MIN_COVERAGE {
                watch.push(format!(
                    "only {:.0}% of its transactions could be read",
                    h.coverage() * 100.0
                ));
            }
            if t.median_hold_secs < FAST_HOLD_SECS {
                notes.push(format!(
                    "fast (median hold {:.0} s): a 1-6 s free feed gives up part of each trade",
                    t.median_hold_secs
                ));
            }
            if t.pnl_sol <= 0.0 && t.round_trips >= LEADER_MIN_TRIPS {
                Verdict::Reject
            } else if t.pnl_sol <= 0.0 || !watch.is_empty() {
                reasons.extend(watch);
                Verdict::Watch
            } else {
                Verdict::Leader
            }
        }
    };
    Assessment {
        class,
        verdict,
        reasons,
        notes,
    }
}

// ------------------------------------------------------------------ report

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub name: String,
    pub wallet: String,
    pub activity: Activity,
    pub fetched: usize,
    pub fetch_failed: usize,
    pub own_txs: usize,
    pub foreign_txs: usize,
    pub trading: Trading,
    pub assessment: Assessment,
}

pub async fn audit_one(rpc: &Rpc, c: &Candidate, want_own: usize) -> anyhow::Result<Row> {
    let (a, h) = history(rpc, &c.wallet, want_own).await?;
    let t = trading(&h.swaps);
    let now = chrono::Utc::now().timestamp();
    let assessment = assess(&a, &h, &t, now);
    Ok(Row {
        name: c.name.clone(),
        wallet: c.wallet.to_string(),
        activity: a,
        fetched: h.fetched,
        fetch_failed: h.fetch_failed,
        own_txs: h.own_txs,
        foreign_txs: h.foreign_txs,
        trading: t,
        assessment,
    })
}

fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::Leader => "LEADER",
        Verdict::Watch => "watch",
        Verdict::Reject => "reject",
    }
}

/// The side-by-side table, leaders first.
pub fn table(rows: &[Row]) -> String {
    let mut rows: Vec<&Row> = rows.iter().collect();
    rows.sort_by(|a, b| {
        (a.assessment.verdict, a.assessment.class)
            .cmp(&(b.assessment.verdict, b.assessment.class))
            .then(
                b.trading
                    .pnl_sol
                    .partial_cmp(&a.trading.pnl_sol)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<18} {:<8} {:<8} {:>6} {:>10} {:>9} {:>5} {:>7} {:>8} {:>6}  why",
        "wallet",
        "verdict",
        "class",
        "trips",
        "PnL SOL",
        "SOL/day",
        "win%",
        "hold s",
        "entry",
        "read%"
    );
    for r in rows {
        let t = &r.trading;
        let mut why = r.assessment.reasons.join("; ");
        if !r.assessment.notes.is_empty() {
            if !why.is_empty() {
                why.push_str("; ");
            }
            why.push_str(&r.assessment.notes.join("; "));
        }
        let _ = writeln!(
            o,
            "{:<18} {:<8} {:<8} {:>6} {:>+10.2} {:>9} {:>4.0}% {:>7.0} {:>8.3} {:>5.0}%  {}",
            r.name.chars().take(18).collect::<String>(),
            verdict_word(r.assessment.verdict),
            format!("{:?}", r.assessment.class).to_lowercase(),
            t.round_trips,
            t.pnl_sol,
            t.pnl_per_day()
                .map(|x| format!("{x:+.1}"))
                .unwrap_or_else(|| "-".into()),
            t.win_rate * 100.0,
            t.median_hold_secs,
            t.median_entry_sol,
            (r.fetched as f64 / (r.fetched + r.fetch_failed).max(1) as f64) * 100.0,
            why
        );
    }
    o
}

/// `[[leaders]]` entries for the engine (`leaders_file = "…"` in its config).
pub fn leaders_toml(rows: &[Row], generated: &str) -> String {
    let leaders: Vec<&Row> = rows
        .iter()
        .filter(|r| r.assessment.verdict == Verdict::Leader)
        .collect();
    let mut o = format!(
        "# Written by `copybot wallet-audit` on {generated}: {} leader(s) out of {} wallets audited.\n\
         # Re-run it weekly; wallets drop out by themselves when they stop qualifying.\n\
         # Load it from the engine config with:  leaders_file = \"<this file>\"\n",
        leaders.len(),
        rows.len()
    );
    for r in leaders {
        let t = &r.trading;
        let label: String = r
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let _ = write!(
            o,
            "\n# {} round trips, {:+.2} SOL realised, {:.0}% win, median hold {:.0} s\n[[leaders]]\naddress = \"{}\"\nlabel = \"{}\"\n",
            t.round_trips,
            t.pnl_sol,
            t.win_rate * 100.0,
            t.median_hold_secs,
            r.wallet,
            label.trim_matches('-')
        );
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain::detect::Template;
    use engine_core::types::Venue;
    use serde_json::json;

    #[test]
    fn list_lines_in_every_common_shape() {
        let text = "\
# hub wallets
decu: 4vw54BmAogeRV3vPKWyFet5yf8DTLcREzdSzx4rw9Ud9
mofo chad: 3VUNtVtjjx5ckUojT7UocJ5fbuAJRsNUXNfTBnPte9vC
FoulMidiPumper:  2XZycmQxaEBr2n6xZpkxtv5PMtapZKU9bpAZsTA72pJR
AYZryL5JYFhAJ7PtA9uvebDqcFrifcPTucvDyjbd7xzN   # no name
not a wallet line
decu again 4vw54BmAogeRV3vPKWyFet5yf8DTLcREzdSzx4rw9Ud9
";
        let l = parse_list(text);
        assert_eq!(l.len(), 4, "duplicates and junk dropped");
        assert_eq!(l[0].name, "decu");
        assert_eq!(l[1].name, "mofo chad");
        assert_eq!(l[2].name, "FoulMidiPumper");
        assert_eq!(l[3].name, "AYZryL");
    }

    #[test]
    fn activity_profile_from_a_signature_page() {
        let page: Vec<Value> = (0..10)
            .map(|i| json!({"blockTime": 1000 + i / 5, "err": if i % 2 == 0 { Value::Null } else { json!({"x": 1}) }}))
            .collect();
        let a = activity(&page);
        assert_eq!(a.sigs, 10);
        assert_eq!(a.failed, 5);
        assert_eq!(a.span_secs, 1);
        assert_eq!(a.peak_per_sec, 5);
        assert_eq!(a.last_ts, 1001);
    }

    fn swap(mint: Pubkey, side: Side, sol: f64, tokens: u64) -> DetectedSwap {
        DetectedSwap {
            wallet: Pubkey::new_unique(),
            mint,
            side,
            sol_amount: (sol * 1e9) as u64,
            token_amount: tokens,
            token_decimals: 6,
            token_program: Pubkey::default(),
            price_sol: 0.0,
            venue: Venue::PumpFunCurve,
            pool_sol: None,
            fraction_sold: None,
            exact: true,
            template: Template::Generic,
            creator: None,
            migrated: false,
        }
    }

    #[test]
    fn fifo_round_trips_partial_sells_and_unmatched_sells() {
        let (a, b, c) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let s = vec![
            (0, swap(a, Side::Buy, 1.0, 100)),
            (10_000, swap(a, Side::Sell, 1.0, 50)), // half for 1.0: +0.5
            (70_000, swap(a, Side::Sell, 0.25, 50)), // rest for 0.25: -0.25
            (1_000, swap(b, Side::Buy, 2.0, 10)),
            (5_000, swap(c, Side::Sell, 9.0, 10)), // bought before the window: ignored
        ];
        let mut s = s;
        s.sort_by_key(|x| x.0);
        let t = trading(&s);
        assert_eq!((t.buys, t.sells, t.round_trips), (2, 3, 2));
        assert!((t.pnl_sol - 0.25).abs() < 1e-9);
        assert_eq!(t.open_positions, 1, "b is still held");
        assert!((t.win_rate - 0.5).abs() < 1e-9);
        assert!((t.median_hold_secs - 40.0).abs() < 1e-9);
        assert!((t.median_entry_sol - 1.5).abs() < 1e-9);
        assert_eq!(t.window_secs, 70.0);
    }

    fn active(now: i64) -> Activity {
        Activity {
            sigs: 1000,
            failed: 50,
            span_secs: 86_400,
            peak_per_sec: 2,
            last_ts: now - 60,
        }
    }

    fn trader(trips: usize, pnl: f64, pf: f64, hold: f64) -> Trading {
        Trading {
            buys: trips,
            sells: trips,
            round_trips: trips,
            pnl_sol: pnl,
            profit_factor: Some(pf),
            median_hold_secs: hold,
            win_rate: 0.6,
            ..Default::default()
        }
    }

    fn read(fetched: usize, failed: usize) -> History {
        History {
            fetched,
            fetch_failed: failed,
            own_txs: fetched,
            ..Default::default()
        }
    }

    #[test]
    fn verdicts_follow_the_rules() {
        let now = 2_000_000_000;
        let ok = read(400, 0);
        // a profitable, patient trader
        let v = assess(&active(now), &ok, &trader(100, 133.0, 3.8, 123.0), now);
        assert_eq!((v.class, v.verdict), (Class::Trader, Verdict::Leader));
        assert!(v.reasons.is_empty());
        // fast but qualifying: leader with a note
        let v = assess(&active(now), &ok, &trader(182, 74.0, 5.6, 35.0), now);
        assert_eq!(v.verdict, Verdict::Leader);
        assert!(v.notes.iter().any(|n| n.contains("fast")));
        // profitable sniper: rejected, it holds for seconds
        let v = assess(&active(now), &ok, &trader(135, 47.0, 11.0, 6.0), now);
        assert_eq!((v.class, v.verdict), (Class::Sniper, Verdict::Reject));
        // machine-gun bot: 1,000 signatures in 18 s, 97% failed
        let bot = Activity {
            sigs: 1000,
            failed: 970,
            span_secs: 18,
            peak_per_sec: 141,
            last_ts: now,
        };
        let v = assess(&bot, &ok, &Trading::default(), now);
        assert_eq!((v.class, v.verdict), (Class::Bot, Verdict::Reject));
        // a losing trader with enough trades
        let v = assess(&active(now), &ok, &trader(65, -20.0, 0.6, 90.0), now);
        assert_eq!((v.class, v.verdict), (Class::Trader, Verdict::Reject));
        // too few trades, or an incomplete read: watch
        let v = assess(&active(now), &ok, &trader(15, 6.0, 2.7, 300.0), now);
        assert_eq!(v.verdict, Verdict::Watch);
        let v = assess(
            &active(now),
            &read(130, 170),
            &trader(65, 9.0, 2.6, 152.0),
            now,
        );
        assert_eq!(v.verdict, Verdict::Watch);
        assert!(v.reasons.iter().any(|r| r.contains("could be read")));
        // selling a position it already had
        let holder = Trading {
            sells: 12,
            ..Default::default()
        };
        let v = assess(&active(now), &ok, &holder, now);
        assert_eq!((v.class, v.verdict), (Class::Holder, Verdict::Reject));
        // nothing for weeks
        let mut idle = active(now);
        idle.last_ts = now - 30 * 86_400;
        let v = assess(&idle, &ok, &trader(100, 50.0, 3.0, 200.0), now);
        assert_eq!(v.class, Class::Inactive);
        // mostly other people's transactions: flagged, too few own trades
        let noisy = History {
            fetched: 400,
            own_txs: 20,
            foreign_txs: 380,
            ..Default::default()
        };
        let v = assess(&active(now), &noisy, &Trading::default(), now);
        assert_eq!(v.class, Class::Unclear);
        assert!(v.notes.iter().any(|n| n.contains("sent by other wallets")));
    }

    #[test]
    fn leaders_file_loads_into_the_engine_config() {
        let now = 2_000_000_000;
        let ok = read(400, 0);
        let row = |name: &str, t: Trading| Row {
            name: name.into(),
            wallet: Pubkey::new_unique().to_string(),
            activity: active(now),
            fetched: 400,
            fetch_failed: 0,
            own_txs: 400,
            foreign_txs: 0,
            assessment: assess(&active(now), &ok, &t, now),
            trading: t,
        };
        let rows = vec![
            row("early biddy", trader(100, 133.0, 3.8, 123.0)),
            row("slyorca", trader(135, 47.0, 11.0, 6.0)),
        ];
        let toml_text = leaders_toml(&rows, "2026-10-08");
        let t: toml::Table = toml::from_str(&toml_text).unwrap();
        let l = t["leaders"].as_array().unwrap();
        assert_eq!(l.len(), 1, "only the leader is written");
        assert_eq!(l[0]["label"].as_str(), Some("early-biddy"));
        assert!(table(&rows).lines().nth(1).unwrap().contains("LEADER"));
    }
}
