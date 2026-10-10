//! `copybot winners`: work backwards from the winners.
//!
//! The question is not "which exit grid makes money" but "what singles the day's
//! winners out at the earliest moment, and what does entering at that moment pay".
//! From the census tape:
//!
//! * winners: standard coins (not Mayhem-mode, not pre-funded or insider-held) whose
//!   liquidity-backed market cap reached a label ($30k, $100k) at any checkpoint or on
//!   the curve; Mayhem-mode coins are a different population and get their own table;
//! * fingerprint: at 5, 15 and 60 s, the winners' books against every coin's (p10 /
//!   median / p90), from the exact first-minute books;
//! * detector: fixed rules on those books, each with its fires per day, recall,
//!   precision with a Wilson interval, lift, the winners' peak from that moment and
//!   what holding every fire to the hour would have paid;
//! * lead: when the signal first holds against when the coin first shows on a trending
//!   list and when it first reads above the label;
//! * the exit, worked backwards from the peak: what the live state (money flow, the
//!   bundle and the creator selling, holders, new buyers, SOL in the curve or pool)
//!   read at the winners' peak and as it went, then every entry signal with every
//!   exit rule on the trade tape, curve and pool, every fire a trade. Exits are
//!   readings of that state, never a clock.
//!
//! No grid. The rules are fixed before looking (a short list, stated), not searched;
//! nothing is picked, so there is no holdout; the chronological halves of the detector
//! tables show whether a number holds over time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::census_report::age;
use crate::replay::{self, hm, pct, recorded_hours, Costs, EntryRule, ExitRule, Signal};

/// Book moments looked at (seconds after creation).
pub const T: [i64; 6] = [5, 15, 30, 60, 120, 300];
/// All book moments on the tape; index into the curve outcome arrays.
const T_ALL: [i64; 7] = [5, 15, 30, 60, 120, 300, 900];
/// Curve fee per side.
const FEE: f64 = 0.0125;
/// A market cap counts only when the pool holds at least this share of it.
const MIN_LIQUIDITY_SHARE: f64 = 0.01;
/// A curve this full at 5 s was filled in the create transaction (pre-funded).
const INSIDER_PROGRESS: f64 = 0.95;
/// The winner labels, dollars of liquidity-backed market cap.
pub const LABELS_USD: [f64; 2] = [30_000.0, 100_000.0];
/// Rows with fewer fires are not printed.
const MIN_FIRES: usize = 8;
/// Fingerprint moments.
const FINGERPRINT_T: [i64; 3] = [5, 15, 60];

/// A coin's book at one moment (the `micro.jsonl` row, the parts used here).
#[derive(Clone, Debug, Default)]
pub struct Book {
    pub mcap_sol: f64,
    pub holders: u64,
    pub buyers: u64,
    pub net_sol: f64,
    pub new5: u64,
    pub hhi: f64,
    pub dev_pct: f64,
    pub snipers_pct: f64,
    pub top10_pct: f64,
}

impl Book {
    fn from_row(r: &Value) -> Self {
        let f = |k: &str| r[k].as_f64().unwrap_or(0.0);
        let u = |k: &str| r[k].as_u64().unwrap_or(0);
        Self {
            mcap_sol: f("mcap_sol"),
            holders: u("holders"),
            buyers: u("buyers"),
            net_sol: f("net_sol"),
            new5: u("new_buyers_5s"),
            hhi: f("hhi"),
            dev_pct: f("dev_pct"),
            snipers_pct: f("snipers_pct"),
            top10_pct: f("top10_pct"),
        }
    }
}

/// One coin as the books, the curve outcome, the checkpoints and the lists know it.
#[derive(Clone, Debug, Default)]
pub struct Coin {
    pub mint: String,
    pub symbol: String,
    pub created_ts: i64,
    pub sol_usd: f64,
    pub mayhem: bool,
    /// pre-funded (curve filled in the create transaction) or graduated in its first slot
    pub insider: bool,
    /// priced in another token
    pub quoted: bool,
    pub books: BTreeMap<i64, Book>,
    mcap_at: Vec<f64>,
    peak_after: Vec<f64>,
    final_mcap: Option<f64>,
    /// (seconds after creation, liquidity-backed market cap in dollars) per checkpoint
    cps: Vec<(i64, f64)>,
    /// first time on any trending list, ms
    listed_ms: Option<i64>,
}

impl Coin {
    /// Highest liquidity-backed market cap in dollars, from the checkpoints and the curve.
    pub fn peak_usd(&self) -> f64 {
        let cp = self.cps.iter().map(|c| c.1).fold(0.0, f64::max);
        let curve = self.peak_after.iter().copied().fold(0.0, f64::max) * self.sol_usd;
        cp.max(curve)
    }
    /// Seconds after creation of the first checkpoint reading at or above `usd`.
    fn first_above(&self, usd: f64) -> Option<i64> {
        self.cps.iter().filter(|c| c.1 >= usd).map(|c| c.0).min()
    }
    /// Liquidity-backed market cap in dollars at checkpoint `cp` (0 when no pool backs it
    /// any more); `None` without that checkpoint.
    fn cp_usd(&self, cp: i64) -> Option<f64> {
        self.cps.iter().find(|c| c.0 == cp).map(|c| c.1)
    }
    /// `cp_usd` over the market cap at `t`.
    fn cp_multiple(&self, t: i64, cp: i64) -> Option<f64> {
        Some(self.cp_usd(cp)? / self.mcap_usd(t)?)
    }
    pub fn mcap_usd(&self, t: i64) -> Option<f64> {
        let b = self.books.get(&t)?;
        (b.mcap_sol > 0.0 && self.sol_usd > 0.0).then_some(b.mcap_sol * self.sol_usd)
    }
    /// (peak after `t`, value at the end of the hour), as multiples of the price at `t`.
    fn hour_from(&self, t: i64) -> Option<(f64, f64)> {
        let i = T_ALL.iter().position(|x| *x == t)?;
        let at = *self.mcap_at.get(i)?;
        if at <= 0.0 {
            return None;
        }
        let peak = *self.peak_after.get(i)?;
        Some((peak / at, self.final_mcap.unwrap_or(0.0) / at))
    }
    fn standard(&self) -> bool {
        !self.mayhem && !self.insider && !self.quoted
    }
    fn day(&self) -> String {
        chrono::DateTime::from_timestamp(self.created_ts, 0)
            .unwrap_or_default()
            .format("%Y-%m-%d")
            .to_string()
    }
}

/// Every `<kind>.jsonl` under `dir`, one parsed row at a time.
fn for_each_row(dir: &Path, kind: &str, mut f: impl FnMut(Value)) {
    let name = format!("{kind}.jsonl");
    let mut stack = vec![dir.to_path_buf()];
    let mut files = vec![];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().and_then(|n| n.to_str()) == Some(name.as_str()) {
                files.push(p);
            }
        }
    }
    files.sort();
    for p in files {
        let Ok(file) = std::fs::File::open(&p) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                f(v);
            }
        }
    }
}

/// Coins with a complete 5 s and 15 s book, their outcomes, checkpoints and listings.
pub fn load(dir: &Path, day: Option<&str>) -> Vec<Coin> {
    let mut coins: HashMap<String, Coin> = HashMap::new();
    for_each_row(dir, "micro", |r| {
        let (Some(mint), Some(t)) = (r["mint"].as_str(), r["t"].as_i64()) else {
            return;
        };
        if !T.contains(&t) || r["gap_ms"].as_i64() != Some(0) {
            return;
        }
        let c = coins.entry(mint.to_string()).or_default();
        if t == 5 {
            c.symbol = r["symbol"].as_str().unwrap_or("").to_string();
            c.created_ts = r["created_ts"].as_i64().unwrap_or(0);
            c.sol_usd = r["sol_usd"].as_f64().unwrap_or(0.0);
            c.mayhem = r["mayhem"] == true;
            c.insider = r["instant_grad"] == true
                || r["progress"].as_f64().unwrap_or(0.0) >= INSIDER_PROGRESS;
            c.quoted |= !r["quote"].is_null();
        }
        c.books.insert(t, Book::from_row(&r));
    });
    for_each_row(dir, "curve_outcomes", |r| {
        if r["full_window"] != true {
            return;
        }
        let Some(c) = r["mint"].as_str().and_then(|m| coins.get_mut(m)) else {
            return;
        };
        c.quoted |= !r["quote"].is_null();
        let arr = |k: &str| -> Vec<f64> {
            r[k].as_array()
                .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect())
                .unwrap_or_default()
        };
        c.mcap_at = arr("mcap_at");
        c.peak_after = arr("peak_after");
        c.final_mcap = r["final_mcap_sol"].as_f64();
    });
    for_each_row(dir, "checkpoints", |r| {
        if r["missed"] == true || r["stop"] == "not_new" {
            return;
        }
        let Some(c) = r["mint"].as_str().and_then(|m| coins.get_mut(m)) else {
            return;
        };
        let (Some(cp), Some(m)) = (r["cp"].as_i64(), r["mcap"].as_f64()) else {
            return;
        };
        let backed = r["liquidity"].as_f64().unwrap_or(0.0) >= MIN_LIQUIDITY_SHARE * m;
        c.cps.push((cp, if backed { m } else { 0.0 }));
    });
    for_each_row(dir, "trending", |r| {
        if r["list"] == "follow" {
            return;
        }
        let Some(c) = r["mint"].as_str().and_then(|m| coins.get_mut(m)) else {
            return;
        };
        if let Some(ts) = r["ts"].as_i64() {
            c.listed_ms = Some(c.listed_ms.map_or(ts, |x| x.min(ts)));
        }
    });
    let mut sol: Vec<f64> = coins
        .values()
        .map(|c| c.sol_usd)
        .filter(|s| *s > 0.0)
        .collect();
    sol.sort_by(f64::total_cmp);
    let sol_fallback = sol.get(sol.len() / 2).copied().unwrap_or(0.0);
    let mut out: Vec<Coin> = coins
        .into_iter()
        .filter(|(_, c)| c.created_ts > 0 && c.books.contains_key(&5) && c.books.contains_key(&15))
        .map(|(mint, mut c)| {
            c.mint = mint;
            if c.sol_usd <= 0.0 {
                c.sol_usd = sol_fallback;
            }
            c
        })
        .filter(|c| day.is_none_or(|d| c.day() == d))
        .collect();
    out.sort_by_key(|c| c.created_ts);
    out
}

// ------------------------------------------------------------------ statistics

/// Wilson 95% interval of a proportion.
pub fn wilson(k: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let z = 1.96f64;
    let (k, n) = (k as f64, n as f64);
    let p = k / n;
    let d = 1.0 + z * z / n;
    let c = (p + z * z / (2.0 * n)) / d;
    let h = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / d;
    ((c - h).max(0.0), (c + h).min(1.0))
}

fn quantile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn median(v: &mut [f64]) -> f64 {
    quantile(v, 0.5)
}

/// (mean, half-width of the 95% interval).
fn mean_ci(v: &[f64]) -> (f64, f64) {
    let n = v.len();
    if n == 0 {
        return (f64::NAN, f64::NAN);
    }
    let m = v.iter().sum::<f64>() / n as f64;
    if n < 2 {
        return (m, f64::NAN);
    }
    let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt();
    (m, 1.96 * sd / (n as f64).sqrt())
}

fn x(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.1}×")
    } else {
        "-".into()
    }
}

fn money(v: f64) -> String {
    if !v.is_finite() {
        "-".into()
    } else if v >= 1e6 {
        format!("${:.2}M", v / 1e6)
    } else if v >= 1e3 {
        format!("${:.0}k", v / 1e3)
    } else {
        format!("${v:.0}")
    }
}

// ------------------------------------------------------------------ rules on the books

type Rule = (String, Box<dyn Fn(&Book) -> bool>);
type Feature = (&'static str, fn(&Book) -> f64);
/// A wallet signal to test: label, wallets, how many must have bought, the coins it is
/// tested on, and the create time from which the entry replay may use it.
type WalletCase<'a> = (&'a str, &'a HashSet<u64>, usize, &'a [usize], i64);
/// Mean return, 95% half-width, median, share at or above 2×, and count.
type Held = (f64, f64, f64, f64, usize);

/// The fixed list of rules tried at every moment: breadth (holders, buyers, fresh
/// buyers), money (net SOL in the curve), concentration (HHI, dev, snipers) and pairs.
pub fn rules() -> Vec<Rule> {
    let mut v: Vec<Rule> = vec![];
    for k in [10u64, 20, 30, 50] {
        v.push((format!("holders ≥ {k}"), Box::new(move |b| b.holders >= k)));
    }
    for k in [20u64, 40] {
        v.push((format!("buyers ≥ {k}"), Box::new(move |b| b.buyers >= k)));
    }
    for k in [5.0, 10.0, 20.0, 30.0] {
        v.push((format!("net SOL ≥ {k}"), Box::new(move |b| b.net_sol >= k)));
    }
    for k in [6u64, 10] {
        v.push((
            format!("new buyers last 5 s ≥ {k}"),
            Box::new(move |b| b.new5 >= k),
        ));
    }
    v.push((
        "holders ≥ 20 & HHI ≤ 0.3".into(),
        Box::new(|b| b.holders >= 20 && b.hhi <= 0.3),
    ));
    v.push((
        "holders ≥ 20 & dev ≤ 5% & snipers ≤ 15%".into(),
        Box::new(|b| b.holders >= 20 && b.dev_pct <= 5.0 && b.snipers_pct <= 15.0),
    ));
    v.push((
        "holders ≥ 30 & net SOL ≥ 20".into(),
        Box::new(|b| b.holders >= 30 && b.net_sol >= 20.0),
    ));
    v.push((
        "holders ≥ 50 & net SOL ≥ 10".into(),
        Box::new(|b| b.holders >= 50 && b.net_sol >= 10.0),
    ));
    v.push((
        "net SOL ≥ 20 & new buyers last 5 s ≥ 6".into(),
        Box::new(|b| b.net_sol >= 20.0 && b.new5 >= 6),
    ));
    v
}

/// One rule at one moment against one winner label.
struct Row {
    t: i64,
    rule: String,
    fires: usize,
    hits: usize,
    recall: f64,
    lift: f64,
    /// winners' peak from the moment, median multiple
    win_peak: f64,
    /// every fire held to the end of the hour, after fees: mean and 95% half-width
    hold: (f64, f64),
    /// every fire held to the 6 h and 24 h checkpoints, after fees
    hold6: Held,
    hold24: Held,
    /// share of the fires whose peak reached 5× their market cap at t
    x5: f64,
    /// the losers' value at the end of the hour, median multiple
    lose_1h: f64,
}

impl Row {
    fn precision(&self) -> f64 {
        self.hits as f64 / self.fires.max(1) as f64
    }
}

fn detector_rows(coins: &[&Coin], winners: &HashSet<&str>, base: f64) -> Vec<Row> {
    let mut out = vec![];
    for t in T {
        for (name, pass) in rules() {
            let fired: Vec<&Coin> = coins
                .iter()
                .copied()
                .filter(|c| c.books.get(&t).is_some_and(&pass))
                .collect();
            if fired.len() < MIN_FIRES {
                continue;
            }
            let hits: Vec<&Coin> = fired
                .iter()
                .copied()
                .filter(|c| winners.contains(c.mint.as_str()))
                .collect();
            let mut win_peak: Vec<f64> = hits
                .iter()
                .filter_map(|c| c.mcap_usd(t).map(|m| c.peak_usd() / m))
                .collect();
            let hold: Vec<f64> = fired
                .iter()
                .filter_map(|c| c.hour_from(t))
                .map(|(_, last)| last * (1.0 - FEE) * (1.0 - FEE) - 1.0)
                .collect();
            let mut lose: Vec<f64> = fired
                .iter()
                .filter(|c| !winners.contains(c.mint.as_str()))
                .filter_map(|c| c.hour_from(t))
                .map(|(_, last)| last)
                .collect();
            let held = |cp: i64| -> Held {
                let mut v: Vec<f64> = fired
                    .iter()
                    .filter_map(|c| c.cp_multiple(t, cp))
                    .map(|m| m * (1.0 - FEE) * (1.0 - FEE) - 1.0)
                    .collect();
                let (m, ci) = mean_ci(&v);
                let doubled =
                    v.iter().filter(|r| **r >= 1.0).count() as f64 / v.len().max(1) as f64;
                (m, ci, median(&mut v), doubled, v.len())
            };
            let x5 = fired
                .iter()
                .filter_map(|c| c.mcap_usd(t).map(|m| c.peak_usd() / m))
                .filter(|m| *m >= 5.0)
                .count() as f64
                / fired.len() as f64;
            let precision = hits.len() as f64 / fired.len() as f64;
            out.push(Row {
                t,
                rule: name,
                fires: fired.len(),
                hits: hits.len(),
                recall: hits.len() as f64 / winners.len().max(1) as f64,
                lift: if base > 0.0 {
                    precision / base
                } else {
                    f64::NAN
                },
                win_peak: median(&mut win_peak),
                hold: mean_ci(&hold),
                hold6: held(21_600),
                hold24: held(86_400),
                x5,
                lose_1h: median(&mut lose),
            });
        }
    }
    out
}

fn held_cell(h: Held) -> String {
    if h.4 == 0 {
        return "-".into();
    }
    format!(
        "{} (±{:.0}%), median {}, ≥ 2×: {:.0}% ({})",
        pct(h.0),
        100.0 * h.1,
        pct(h.2),
        100.0 * h.3,
        h.4
    )
}

fn detector_table(o: &mut String, rows: &[Row], hours: f64, winners: usize) {
    let _ = writeln!(
        o,
        "| t | rule | fires | /day | recall | precision (95%) | lift | winners: peak from t | fires with peak ≥ 5× | all fires held to 1 h, after fees | held to 6 h (n) | held to 24 h (n) | losers at 1 h |"
    );
    let _ = writeln!(
        o,
        "|---:|---|---:|---:|---:|---|---:|---:|---:|---|---|---|---:|"
    );
    for r in rows {
        let (lo, hi) = wilson(r.hits, r.fires);
        let _ = writeln!(
            o,
            "| {} | {} | {} | {:.0} | {:.0}% | **{:.1}%** [{:.1}–{:.1}] | {:.0}× | {} | {:.1}% | {} (±{:.0}%) | {} | {} | {} |",
            r.t,
            r.rule,
            r.fires,
            r.fires as f64 / hours * 24.0,
            100.0 * r.recall,
            100.0 * r.precision(),
            100.0 * lo,
            100.0 * hi,
            r.lift,
            x(r.win_peak),
            100.0 * r.x5,
            pct(r.hold.0),
            100.0 * r.hold.1,
            held_cell(r.hold6),
            held_cell(r.hold24),
            x(r.lose_1h)
        );
    }
    let _ = writeln!(
        o,
        "_{} winners; precision = share of the rule's fires that became winners, lift = precision over the base rate; \"peak from t\" is the winners' median peak over their market cap at t; \"held to …\" buys every fire at its book price at t and sells at the end of the hour or at that checkpoint's liquidity-backed market cap (0 when no pool backs it any more), both sides at the curve fee; (n) fires old enough to have that checkpoint._",
        winners
    );
}

/// Per moment, the rule with the highest precision lower bound among those catching
/// at least a quarter of the winners.
fn picks(rows: &[Row]) -> Vec<&Row> {
    T.iter()
        .filter_map(|t| {
            rows.iter()
                .filter(|r| r.t == *t && r.recall >= 0.25)
                .max_by(|a, b| {
                    wilson(a.hits, a.fires)
                        .0
                        .total_cmp(&wilson(b.hits, b.fires).0)
                })
        })
        .collect()
}

// ------------------------------------------------------------------ fingerprint

fn fingerprint(o: &mut String, coins: &[&Coin], w30: &HashSet<&str>, w100: &HashSet<&str>) {
    let feats: [Feature; 9] = [
        ("holders", |b| b.holders as f64),
        ("buyers", |b| b.buyers as f64),
        ("net SOL in curve", |b| b.net_sol),
        ("new buyers last 5 s", |b| b.new5 as f64),
        ("HHI", |b| b.hhi),
        ("dev %", |b| b.dev_pct),
        ("snipers %", |b| b.snipers_pct),
        ("top 10 holders %", |b| b.top10_pct),
        ("market cap SOL", |b| b.mcap_sol),
    ];
    for t in FINGERPRINT_T {
        let _ = writeln!(
            o,
            "\n| at {t} s | ≥ $100k p10 | **median** | p90 | $30k–$100k p10 | **median** | p90 | all coins p10 | **median** | p90 |"
        );
        let _ = writeln!(o, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
        for (name, f) in feats {
            let pick = |keep: &dyn Fn(&Coin) -> bool| -> Vec<f64> {
                coins
                    .iter()
                    .filter(|c| keep(c))
                    .filter_map(|c| c.books.get(&t).map(f))
                    .collect()
            };
            let mut big = pick(&|c| w100.contains(c.mint.as_str()));
            let mut mid =
                pick(&|c| w30.contains(c.mint.as_str()) && !w100.contains(c.mint.as_str()));
            let mut all = pick(&|_| true);
            let q = |v: &mut Vec<f64>, p: f64| {
                let x = quantile(v, p);
                if x.is_finite() {
                    format!("{x:.2}")
                } else {
                    "-".into()
                }
            };
            let _ = writeln!(
                o,
                "| {name} | {} | **{}** | {} | {} | **{}** | {} | {} | **{}** | {} |",
                q(&mut big, 0.1),
                q(&mut big, 0.5),
                q(&mut big, 0.9),
                q(&mut mid, 0.1),
                q(&mut mid, 0.5),
                q(&mut mid, 0.9),
                q(&mut all, 0.1),
                q(&mut all, 0.5),
                q(&mut all, 0.9)
            );
        }
    }
}

// ------------------------------------------------------------------ the signal as the entry

struct Fire {
    coin: usize,
    signal_ts: i64,
}

/// Seconds after the create within which a buyer counts as early.
const EARLY_S: i64 = 60;
/// A wallet needs this many winners among its early buys to be listed or used...
const MIN_WALLET_WINS: usize = 2;
/// ...and this share of its early buys must be winners: the bots that buy every launch
/// in its first second hit many winners by volume and are no signal.
const MIN_WALLET_PRECISION: f64 = 0.25;

/// Early buyers of the winners: which wallets were in the first minute of several
/// winners, and whether that repeats out of sample (wallets found on the first half of
/// the tape, tested on the second half; the leaders file, chosen before the tape, on
/// all of it). Returns the wallet signals for the entry replay with the time from which
/// each may fire.
fn wallet_section(
    o: &mut String,
    tape: &[replay::Coin],
    idx: &[usize],
    w30: &HashSet<&str>,
    w100: &HashSet<&str>,
    names: &HashMap<u64, String>,
    leaders: &HashSet<u64>,
) -> Vec<(Signal, i64)> {
    let _ = writeln!(o, "\n## Who is in the first minute of the winners\n");
    if idx.len() < 2 {
        let _ = writeln!(o, "_Needs the trade tape._");
        return vec![];
    }
    #[derive(Default, Clone, Copy)]
    struct W {
        buys: usize,
        w30: usize,
        w100: usize,
    }
    let tally = |coins: &[usize]| -> HashMap<u64, W> {
        let mut m: HashMap<u64, W> = HashMap::new();
        for &i in coins {
            let c = &tape[i];
            let (a, b) = (
                w30.contains(c.mint.as_str()),
                w100.contains(c.mint.as_str()),
            );
            for h in c.early_buyers(EARLY_S) {
                let w = m.entry(h).or_default();
                w.buys += 1;
                w.w30 += a as usize;
                w.w100 += b as usize;
            }
        }
        m
    };
    let selective =
        |w: &W| w.w30 >= MIN_WALLET_WINS && w.w30 as f64 >= MIN_WALLET_PRECISION * w.buys as f64;
    let all = tally(idx);
    let mut top: Vec<(&u64, &W)> = all.iter().filter(|(_, w)| selective(w)).collect();
    top.sort_by_key(|(_, w)| std::cmp::Reverse((w.w30, w.w100)));
    let _ = writeln!(
        o,
        "| wallet | coins bought within {EARLY_S} s | of them ≥ $30k | ≥ $100k | precision ≥ $30k (95%) |"
    );
    let _ = writeln!(o, "|---|---:|---:|---:|---|");
    for (h, w) in top.iter().take(15) {
        let (lo, hi) = wilson(w.w30, w.buys);
        let _ = writeln!(
            o,
            "| `{}` | {} | {} | {} | {:.0}% [{:.0}–{:.0}] |",
            names.get(h).map(String::as_str).unwrap_or("?"),
            w.buys,
            w.w30,
            w.w100,
            100.0 * w.w30 as f64 / w.buys.max(1) as f64,
            100.0 * lo,
            100.0 * hi
        );
    }
    let _ = writeln!(
        o,
        "_{} wallets bought ≥ {MIN_WALLET_WINS} winners within {EARLY_S} s with winners ≥ {:.0}% of their early buys, of {} wallets that bought any coin that early ({} of them bought ≥ {MIN_WALLET_WINS} winners; the rest are bots buying hundreds of launches). In sample: the winners defined the list, so the precision column is optimistic._",
        top.len(),
        100.0 * MIN_WALLET_PRECISION,
        all.len(),
        all.values().filter(|w| w.w30 >= MIN_WALLET_WINS).count()
    );
    let (first, second) = idx.split_at(idx.len() / 2);
    let split_ts = tape[second[0]].created_ts;
    let found = tally(first);
    let known: HashSet<u64> = found
        .iter()
        .filter(|(_, w)| selective(w))
        .map(|(h, _)| *h)
        .collect();
    let _ = writeln!(
        o,
        "\nOut of sample: wallets selective by the same test on the first half of the tape ({} wallets, coins created before {} UTC) tested on the second half; the leaders file (chosen before the tape) on all of it:\n",
        known.len(),
        hm(split_ts)
    );
    let _ = writeln!(
        o,
        "| signal | coins | fires | precision ≥ $30k (95%) | base | lift | precision ≥ $100k (95%) | base | lift | recall ≥ $30k |"
    );
    let _ = writeln!(o, "|---|---:|---:|---|---:|---:|---|---:|---:|---:|");
    let count = |coins: &[usize], w: &HashSet<&str>| {
        coins
            .iter()
            .filter(|&&i| w.contains(tape[i].mint.as_str()))
            .count()
    };
    let mut out = vec![];
    let cases: [WalletCase; 3] = [
        ("selective early wallets", &known, 1, second, split_ts),
        ("selective early wallets", &known, 2, second, split_ts),
        ("leaders", leaders, 1, idx, 0),
    ];
    for (label, set, min, pool, from_ts) in cases {
        if set.is_empty() {
            continue;
        }
        let sig = Signal::wallets(set.clone(), min, EARLY_S, label);
        let fires: Vec<usize> = pool
            .iter()
            .copied()
            .filter(|&i| tape[i].signal_ts(&sig).is_some())
            .collect();
        let n = pool.len().max(1) as f64;
        let (b30, b100) = (count(pool, w30), count(pool, w100));
        let (h30, h100) = (count(&fires, w30), count(&fires, w100));
        let p = |k: usize| {
            let (lo, hi) = wilson(k, fires.len());
            format!(
                "{:.1}% [{:.1}–{:.1}]",
                100.0 * k as f64 / fires.len().max(1) as f64,
                100.0 * lo,
                100.0 * hi
            )
        };
        let lift = |k: usize, b: usize| {
            if b > 0 && !fires.is_empty() {
                format!("{:.0}×", (k as f64 / fires.len() as f64) / (b as f64 / n))
            } else {
                "-".into()
            }
        };
        let _ = writeln!(
            o,
            "| {} | {} | {} | {} | {:.2}% | {} | {} | {:.2}% | {} | {:.0}% |",
            sig.label(),
            pool.len(),
            fires.len(),
            p(h30),
            100.0 * b30 as f64 / n,
            lift(h30, b30),
            p(h100),
            100.0 * b100 as f64 / n,
            lift(h100, b100),
            100.0 * h30 as f64 / b30.max(1) as f64
        );
        out.push((sig, from_ts));
    }
    out
}

/// The entries the exit side is worked on: the first-seconds signals, then the state
/// machine's later stages, fixed before looking.
fn entry_rules() -> Vec<EntryRule> {
    vec![
        EntryRule::Signal(Signal::simple(20.0, 0, 0, 60)),
        EntryRule::Signal(Signal::simple(30.0, 0, 0, 60)),
        EntryRule::Signal(Signal::simple(20.0, 30, 0, 60)),
        EntryRule::SecondWave {
            max_bundle_left: 0.5,
            min_sol_vs_peak: 0.9,
            min_new_buyers_w: 15,
            min_net_sol: 15.0,
            within_s: 600,
        },
        EntryRule::SecondWave {
            max_bundle_left: 0.3,
            min_sol_vs_peak: 0.95,
            min_new_buyers_w: 25,
            min_net_sol: 25.0,
            within_s: 600,
        },
        EntryRule::Organic {
            max_bundle_share: 0.02,
            min_holders: 30,
            min_new_buyers_w: 10,
            within_s: 600,
        },
        EntryRule::Organic {
            max_bundle_share: 0.02,
            min_holders: 60,
            min_new_buyers_w: 15,
            within_s: 1800,
        },
    ]
}

/// The exit rules, fixed before looking: readings of the live state, no clock.
fn exit_rules() -> Vec<ExitRule> {
    vec![
        ExitRule::MoneyLeaving(2.0),
        ExitRule::BundleOut(0.5),
        ExitRule::DevOut(0.5),
        ExitRule::HoldersFalling(0.9),
        ExitRule::NoNewBuyer(120),
        ExitRule::SolLeaving(0.8),
        ExitRule::Either(
            Box::new(ExitRule::MoneyLeaving(2.0)),
            Box::new(ExitRule::BundleOut(0.5)),
        ),
        ExitRule::Drawdown(0.5),
        ExitRule::None,
    ]
}

fn fires_of(tape: &[replay::Coin], idx: &[usize], r: &EntryRule) -> Vec<Fire> {
    idx.iter()
        .filter_map(|&i| {
            tape[i]
                .entry_ts(r)
                .map(|signal_ts| Fire { coin: i, signal_ts })
        })
        .collect()
}

/// Working backwards from the peak: what the live state read at the winners' peak, a
/// minute after it, and when half the peak was gone, against the losers. Then every
/// entry signal with every exit rule, every fire a trade, curve and pool, no clock.
fn exit_section(
    o: &mut String,
    tape: &[replay::Coin],
    idx: &[usize],
    k: &Costs,
    sol_usd: f64,
    hours: f64,
    extra: &[(Signal, i64)],
) {
    let _ = writeln!(
        o,
        "\n## Exit: work backwards from the peak (trade tape, curve and pool)\n"
    );
    if idx.is_empty() {
        let _ = writeln!(o, "_No trade tape for these coins (pass `--trades`)._");
        return;
    }
    let rules_for_fingerprint = entry_rules();
    for main in [&rules_for_fingerprint[0], &rules_for_fingerprint[3]] {
        let fires = fires_of(tape, idx, main);
        let paths: Vec<replay::Trail> = fires
            .iter()
            .filter_map(|f| {
                replay::ride_all(&tape[f.coin], f.signal_ts + k.delay_s, &[ExitRule::None], k)
            })
            .map(|(_, p)| p)
            .collect();
        let winners: Vec<&replay::Trail> = paths.iter().filter(|p| p.peak_ret >= 1.0).collect();
        let losers: Vec<&replay::Trail> = paths.iter().filter(|p| p.peak_ret < 0.2).collect();
        let _ = writeln!(
        o,
        "Entry: **{}**, {} fires. Winners here are the rides whose position was worth 2× or more at some point ({}); losers never saw +20% ({}). The live state an engine holding the coin reads, at the winners' peak, a minute after it, and at the first trade at which half the peak was gone; the losers at their half-way point:\n",
        main.label(),
        paths.len(),
        winners.len(),
        losers.len()
    );
        let _ = writeln!(
        o,
        "| reading (median) | winners at the peak | winners 60 s after | winners at −50% from the peak | losers at −50% |"
    );
        let _ = writeln!(o, "|---|---:|---:|---:|---:|");
        type Pick = fn(&replay::Snap) -> f64;
        let readings: [(&str, Pick); 9] = [
            ("age, s", |s| s.age as f64),
            ("sells / buys in SOL, last 60 s", |s| s.flow_ratio),
            ("bundle (first-5 s buyers) still holds", |s| s.bundle_left),
            ("creator still holds", |s| s.dev_left),
            ("holders vs their peak", |s| s.holders_vs_peak),
            ("holders", |s| s.holders as f64),
            ("new buyers, last 60 s", |s| s.new_buyers_w as f64),
            ("seconds since the last new buyer", |s| {
                s.since_new_buyer as f64
            }),
            ("SOL in curve/pool vs its peak", |s| s.net_sol_vs_peak),
        ];
        let med = |v: Vec<f64>| {
            let mut v = v;
            if v.is_empty() {
                "-".to_string()
            } else {
                format!("{:.2} ({})", median(&mut v), v.len())
            }
        };
        for (name, f) in readings {
            let _ = writeln!(
                o,
                "| {name} | {} | {} | {} | {} |",
                med(winners.iter().map(|p| f(&p.at_peak)).collect()),
                med(winners
                    .iter()
                    .filter_map(|p| p.after_peak.map(|s| f(&s)))
                    .collect()),
                med(winners
                    .iter()
                    .filter_map(|p| p.at_half.map(|s| f(&s)))
                    .collect()),
                med(losers
                    .iter()
                    .filter_map(|p| p.at_half.map(|s| f(&s)))
                    .collect())
            );
        }
        let mut to_half: Vec<f64> = winners
            .iter()
            .filter_map(|p| p.at_half.map(|s| (s.age - p.at_peak.age) as f64))
            .collect();
        let mut kept: Vec<f64> = winners
            .iter()
            .map(|p| (1.0 + p.end_ret) / (1.0 + p.peak_ret))
            .collect();
        let _ = writeln!(
        o,
        "\n- winners: half the peak is gone a median **{:.0} s** after it ({} of {} got there on tape); a winner never sold keeps a median **{:.0}%** of its peak value at the end of the data",
        median(&mut to_half),
        to_half.len(),
        winners.len(),
        100.0 * median(&mut kept)
    );
        let _ = writeln!(
        o,
        "_The reading that moves first after the peak is the exit. Bundle and creator shares are of their own peak holdings (1 = untouched); flow is SOL sold over SOL bought in the last minute (99 = sells with no buys)._"
    );
    }

    // ---- entry × exit
    let rules = exit_rules();
    let entries: Vec<(EntryRule, i64)> = entry_rules()
        .into_iter()
        .map(|s| (s, 0))
        .chain(
            extra
                .iter()
                .map(|(s, t)| (EntryRule::Signal(s.clone()), *t)),
        )
        .collect();
    let _ = writeln!(o, "\n### Entry signal × exit signal\n");
    let _ = writeln!(
        o,
        "- {} standard coins with their trades on tape ({} trades); our buy lands {} s after the signal's trade with {} SOL in the curve, fees as recorded, {} SOL per transaction; the sell lands {} s after the first trade at which the exit rule holds, on the curve or in the pool as it is then; a rule that never holds closes at the last trade on tape (the pool is followed for a day after graduation, the curve for its first hour); {} entries × {} exits, all fixed in advance; every fire is a trade",
        idx.len(),
        idx.iter().map(|&i| tape[i].trades()).sum::<usize>(),
        k.delay_s,
        k.size_lamports as f64 / 1e9,
        k.tx_lamports as f64 / 1e9,
        k.delay_s,
        entries.len(),
        rules.len()
    );
    let _ = writeln!(
        o,
        "\n| entry | exit | trades | /day | closed by the rule | graduated | held (median) | mean (±) | median | ≥ 2× | ≤ 0.1× | kept of the peak (winners, median) | SOL/day at size |"
    );
    let _ = writeln!(
        o,
        "|---|---|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|"
    );
    for (s, from_ts) in &entries {
        let pool: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|&i| tape[i].created_ts >= *from_ts)
            .collect();
        let hours = if *from_ts > 0 {
            recorded_hours(pool.iter().map(|&i| tape[i].created_ts).collect())
        } else {
            hours
        };
        let fires = fires_of(tape, &pool, s);
        let rides: Vec<Vec<replay::Ride>> = fires
            .iter()
            .filter_map(|f| replay::ride_all(&tape[f.coin], f.signal_ts + k.delay_s, &rules, k))
            .map(|(r, _)| r)
            .collect();
        if rides.len() < MIN_FIRES {
            continue;
        }
        let mut entry_mcap: Vec<f64> = fires
            .iter()
            .map(|f| tape[f.coin].mcap_sol_at(f.signal_ts + k.delay_s) * sol_usd)
            .collect();
        let _ = writeln!(
            o,
            "| **{}{}** | _{} fires, entry mcap median {}_ | | | | | | | | | | | |",
            s.label(),
            if *from_ts > 0 { " (2nd half)" } else { "" },
            rides.len(),
            money(median(&mut entry_mcap))
        );
        for (r, rule) in rules.iter().enumerate() {
            let mut rets: Vec<f64> = rides.iter().map(|v| v[r].ret).collect();
            let mut held: Vec<f64> = rides
                .iter()
                .map(|v| (v[r].exit_ts - v[r].entry_ts) as f64)
                .collect();
            let by_rule = rides.iter().filter(|v| v[r].by_rule).count();
            let graduated = rides.iter().filter(|v| v[r].graduated).count();
            let mut kept: Vec<f64> = rides
                .iter()
                .filter(|v| v[r].peak_ret >= 1.0)
                .map(|v| (1.0 + v[r].ret) / (1.0 + v[r].peak_ret))
                .collect();
            let (m, ci) = mean_ci(&rets);
            let med = median(&mut rets);
            let n = rets.len() as f64;
            let share =
                |f: &dyn Fn(f64) -> bool| 100.0 * rets.iter().filter(|x| f(**x)).count() as f64 / n;
            let sol_day = rets.iter().sum::<f64>() * k.size_lamports as f64 / 1e9 / hours * 24.0;
            let _ = writeln!(
                o,
                "| | {} | {} | {:.0} | {:.0}% | {:.0}% | {} | **{}** (±{:.0}%) | {} | {:.0}% | {:.0}% | {} | {} |",
                rule.label(),
                rets.len(),
                n / hours * 24.0,
                100.0 * by_rule as f64 / n,
                100.0 * graduated as f64 / n,
                age((median(&mut held) * 1000.0) as i64),
                pct(m),
                100.0 * ci,
                pct(med),
                share(&|x| x >= 1.0),
                share(&|x| x <= -0.9),
                if kept.is_empty() { "-".into() } else { format!("{:.0}% ({})", 100.0 * median(&mut kept), kept.len()) },
                if hours >= 6.0 { format!("{sol_day:+.1}") } else { "n/a".into() }
            );
        }
    }
    let _ = writeln!(
        o,
        "_Returns are on what we paid, after fees and our own impact on the curve (the pool sell is constant product against the pool's SOL at that trade, at the pool fee). \"Held\" is an outcome, not an input. \"Kept of the peak\" is the exit value over the best value seen before it, on rides that reached 2×. A ride the rule never closed is marked at the last trade on tape with the position still open._"
    );
}

// ------------------------------------------------------------------ the report

pub struct Args {
    pub dir: PathBuf,
    pub trades: Option<PathBuf>,
    pub day: Option<String>,
    pub size_sol: f64,
    pub delay_s: f64,
    pub tx_cost_sol: f64,
    pub leaders_file: Option<PathBuf>,
}

/// The wallet hashes of a leaders file (`[[leaders]] address = "..."`).
fn leader_hashes(path: &Path) -> anyhow::Result<HashSet<u64>> {
    let v: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(v.get("leaders")
        .and_then(|l| l.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.get("address").and_then(|x| x.as_str()))
                .map(replay::wallet_hash)
                .collect()
        })
        .unwrap_or_default())
}

pub fn run(a: &Args) -> anyhow::Result<String> {
    let coins = load(&a.dir, a.day.as_deref());
    let (tape, names) =
        replay::load_with_wallets(a.trades.as_deref().unwrap_or(&a.dir), None, None)?;
    let leaders = match &a.leaders_file {
        Some(p) => leader_hashes(p)?,
        None => HashSet::new(),
    };
    Ok(report(&coins, &tape, &names, &leaders, a))
}

pub fn report(
    coins: &[Coin],
    tape: &[replay::Coin],
    names: &HashMap<u64, String>,
    leaders: &HashSet<u64>,
    a: &Args,
) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# Working backwards from the winners\n");
    if coins.is_empty() {
        let _ = writeln!(
            o,
            "No coins with complete 5 s and 15 s books under {} yet.",
            a.dir.display()
        );
        return o;
    }
    let hours = recorded_hours(coins.iter().map(|c| c.created_ts).collect());
    let mut sol: Vec<f64> = coins.iter().map(|c| c.sol_usd).collect();
    let sol_usd = median(&mut sol);
    let standard: Vec<&Coin> = coins.iter().filter(|c| c.standard()).collect();
    let mayhem: Vec<&Coin> = coins
        .iter()
        .filter(|c| c.mayhem && !c.insider && !c.quoted)
        .collect();
    let insider = coins.iter().filter(|c| c.insider).count();
    let followed = coins
        .iter()
        .filter(|c| c.cps.iter().any(|x| x.0 >= 3 * 3600))
        .count();
    let _ = writeln!(
        o,
        "- data: **{}** coins with complete 5 s and 15 s books, created {} → {} UTC, **{:.1} h of recording**; standard {} · Mayhem-mode {} · pre-funded or insider-held {} (left out) · priced in another token {} (left out); followed 3 h or longer: {}; SOL at ${sol_usd:.0}",
        coins.len(),
        hm(coins[0].created_ts),
        hm(coins[coins.len() - 1].created_ts),
        hours,
        standard.len(),
        mayhem.len(),
        insider,
        coins.iter().filter(|c| c.quoted).count(),
        followed
    );
    let _ = writeln!(
        o,
        "- a winner reached the label in liquidity-backed market cap (the pool holds ≥ 1% of it) at a Jupiter checkpoint or on the curve; the checkpoints run to 24 h, so a coin that only moved later is not counted yet"
    );
    for (name, grp) in [("standard", &standard), ("Mayhem-mode", &mayhem)] {
        let n = grp.len().max(1);
        let cnt = |usd: f64| grp.iter().filter(|c| c.peak_usd() >= usd).count();
        let _ = writeln!(
            o,
            "- {name} coins reaching ≥ $30k: **{}** ({:.2}%) · ≥ $100k: **{}** ({:.2}%) · ≥ $250k: {} · ≥ $1M: {}",
            cnt(30e3),
            100.0 * cnt(30e3) as f64 / n as f64,
            cnt(100e3),
            100.0 * cnt(100e3) as f64 / n as f64,
            cnt(250e3),
            cnt(1e6)
        );
    }

    // ---- the winners, by day
    let _ = writeln!(o, "\n## The winners (standard coins, by day)\n");
    let mut days: Vec<String> = standard.iter().map(|c| c.day()).collect();
    days.sort();
    days.dedup();
    let net20 = |c: &Coin| {
        T.iter()
            .find(|t| c.books.get(t).is_some_and(|b| b.net_sol >= 20.0))
            .copied()
    };
    let tape_ix: HashMap<&str, usize> = tape
        .iter()
        .enumerate()
        .map(|(i, c)| (c.mint.as_str(), i))
        .collect();
    let exact = |c: &Coin| {
        tape_ix.get(c.mint.as_str()).and_then(|&i| {
            tape[i]
                .signal_ts(&Signal::simple(20.0, 0, 0, 3600))
                .map(|ts| ts - c.created_ts)
        })
    };
    for day in days.iter().rev().take(3) {
        let mut top: Vec<&Coin> = standard
            .iter()
            .copied()
            .filter(|c| c.day() == *day)
            .collect();
        top.sort_by(|a, b| b.peak_usd().total_cmp(&a.peak_usd()));
        top.truncate(10);
        let _ = writeln!(o, "**{day}** · top 10 by peak\n");
        let _ = writeln!(
            o,
            "| symbol | mint | peak | ≥ $30k first seen | first on a trending list | net SOL ≥ 20 first held | holders @15 s | net SOL @15 s | buyers @15 s | mcap @15 s |"
        );
        let _ = writeln!(o, "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|");
        for c in top {
            let b15 = &c.books[&15];
            let _ = writeln!(
                o,
                "| {} | `{}` | {} | {} | {} | {} | {} | {:.1} | {} | {} |",
                if c.symbol.is_empty() { "?" } else { &c.symbol },
                c.mint,
                money(c.peak_usd()),
                c.first_above(30e3)
                    .map_or("curve only / not yet".into(), |s| age(s * 1000)),
                c.listed_ms
                    .map_or("never".into(), |ms| age(ms - c.created_ts * 1000)),
                match (exact(c), net20(c)) {
                    (Some(s), _) => format!("{s} s"),
                    (None, Some(t)) => format!("by {t} s"),
                    (None, None) => "never in 5 min".into(),
                },
                b15.holders,
                b15.net_sol,
                b15.buyers,
                money(b15.mcap_sol * c.sol_usd)
            );
        }
        let _ = writeln!(o);
    }

    // ---- lead
    let w30: Vec<&Coin> = standard
        .iter()
        .copied()
        .filter(|c| c.peak_usd() >= 30e3)
        .collect();
    if !w30.is_empty() {
        let mut sig: Vec<f64> = w30
            .iter()
            .filter_map(|c| exact(c).map(|s| s as f64).or(net20(c).map(|t| t as f64)))
            .collect();
        let mut above: Vec<f64> = w30
            .iter()
            .filter_map(|c| c.first_above(30e3).map(|s| s as f64))
            .collect();
        let mut listed: Vec<f64> = w30
            .iter()
            .filter_map(|c| {
                c.listed_ms
                    .map(|ms| (ms - c.created_ts * 1000) as f64 / 1000.0)
            })
            .collect();
        let _ = writeln!(
            o,
            "## Lead: the signal against the crowd (winners ≥ $30k)\n"
        );
        let _ = writeln!(
            o,
            "- net SOL ≥ 20 first holds: {} of {} winners, median **{} s** after creation",
            sig.len(),
            w30.len(),
            median(&mut sig)
        );
        let _ = writeln!(
            o,
            "- first checkpoint reading ≥ $30k: {} winners, median **{}** after creation (checkpoints are at 15 s … 24 h, so this is an upper bound)",
            above.len(),
            age((median(&mut above) * 1000.0) as i64)
        );
        let _ = writeln!(
            o,
            "- first on any trending list: {} of {} winners ever ({:.0}%), median **{}** after creation",
            listed.len(),
            w30.len(),
            100.0 * listed.len() as f64 / w30.len() as f64,
            if listed.is_empty() { "-".into() } else { age((median(&mut listed) * 1000.0) as i64) }
        );
        let _ = writeln!(
            o,
            "_The lists are where the crowd sees a coin. The gap between the first line and the third is how long the engine has before them._\n"
        );
    }

    // ---- fingerprint
    let _ = writeln!(o, "## Fingerprint: the winners against all standard coins");
    let w30_set: HashSet<&str> = w30.iter().map(|c| c.mint.as_str()).collect();
    let w100_set: HashSet<&str> = standard
        .iter()
        .filter(|c| c.peak_usd() >= 100e3)
        .map(|c| c.mint.as_str())
        .collect();
    fingerprint(&mut o, &standard, &w30_set, &w100_set);

    // ---- detector
    for usd in LABELS_USD {
        let w: HashSet<&str> = standard
            .iter()
            .filter(|c| c.peak_usd() >= usd)
            .map(|c| c.mint.as_str())
            .collect();
        let base = w.len() as f64 / standard.len().max(1) as f64;
        let _ = writeln!(
            o,
            "\n## Detector: standard coins, winners = reached {} ({} of {}, {:.2}%)\n",
            money(usd),
            w.len(),
            standard.len(),
            100.0 * base
        );
        if w.is_empty() {
            let _ = writeln!(o, "_No winner at this label yet._");
            continue;
        }
        let rows = detector_rows(&standard, &w, base);
        detector_table(&mut o, &rows, hours, w.len());
        let p = picks(&rows);
        if !p.is_empty() {
            let _ = writeln!(
                o,
                "\nBest precision lower bound per moment among rules catching ≥ 25% of the winners (chosen from {} rules, so a touch optimistic):",
                rules().len()
            );
            for r in p {
                let (lo, _) = wilson(r.hits, r.fires);
                let _ = writeln!(
                    o,
                    "- at **{} s**: {} → {:.1}% precision (lower bound {:.1}%), recall {:.0}%, {:.0} fires/day, winners' median peak from there {}",
                    r.t,
                    r.rule,
                    100.0 * r.precision(),
                    100.0 * lo,
                    100.0 * r.recall,
                    r.fires as f64 / hours * 24.0,
                    x(r.win_peak)
                );
            }
        }
    }
    // Mayhem, the short version
    let wm: HashSet<&str> = mayhem
        .iter()
        .filter(|c| c.peak_usd() >= 30e3)
        .map(|c| c.mint.as_str())
        .collect();
    if !wm.is_empty() {
        let base = wm.len() as f64 / mayhem.len() as f64;
        let _ = writeln!(
            o,
            "\n## Detector: Mayhem-mode coins, winners = reached $30k ({} of {}, {:.2}%)\n",
            wm.len(),
            mayhem.len(),
            100.0 * base
        );
        let rows: Vec<Row> = detector_rows(&mayhem, &wm, base)
            .into_iter()
            .filter(|r| {
                r.t <= 60
                    && (r.rule.starts_with("net SOL")
                        || r.rule.starts_with("holders ≥ 3")
                        || r.rule.starts_with("holders ≥ 5"))
            })
            .collect();
        detector_table(&mut o, &rows, hours, wm.len());
    }

    // ---- the signal as the entry
    let k = Costs {
        size_lamports: (a.size_sol * 1e9) as u64,
        delay_s: a.delay_s.ceil() as i64,
        tx_lamports: (a.tx_cost_sol * 1e9) as u64,
    };
    let by_mint: HashMap<&str, &Coin> = standard.iter().map(|c| (c.mint.as_str(), *c)).collect();
    let std_idx: Vec<usize> = (0..tape.len())
        .filter(|&i| by_mint.contains_key(tape[i].mint.as_str()))
        .collect();
    let extra = wallet_section(&mut o, tape, &std_idx, &w30_set, &w100_set, names, leaders);
    exit_section(&mut o, tape, &std_idx, &k, sol_usd, hours, &extra);
    o
}

/// Write the report under `<dir>/winners/` and return it.
pub fn write(a: &Args) -> anyhow::Result<String> {
    let text = run(a)?;
    let d = a.dir.join("winners");
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("report.md"), &text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilson_brackets_the_proportion() {
        let (lo, hi) = wilson(30, 100);
        assert!(lo < 0.3 && hi > 0.3);
        assert!(lo > 0.21 && hi < 0.40, "{lo} {hi}");
        assert_eq!(wilson(0, 0), (0.0, 0.0));
    }

    #[test]
    fn peak_takes_backed_checkpoints_and_the_curve() {
        let c = Coin {
            sol_usd: 100.0,
            peak_after: vec![400.0, 300.0],
            cps: vec![(900, 0.0), (3600, 120_000.0)],
            ..Default::default()
        };
        assert_eq!(c.peak_usd(), 120_000.0);
        assert_eq!(c.first_above(30e3), Some(3600));
        let curve_only = Coin {
            sol_usd: 100.0,
            peak_after: vec![400.0],
            ..Default::default()
        };
        assert_eq!(curve_only.peak_usd(), 40_000.0);
        assert_eq!(curve_only.first_above(30e3), None);
    }

    #[test]
    fn loads_books_outcomes_checkpoints_and_lists() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026-10-10");
        std::fs::create_dir_all(&day).unwrap();
        let row = |t: i64, mint: &str, net: f64, holders: u64, extra: &str| {
            format!(
                r#"{{"mint":"{mint}","t":{t},"gap_ms":0,"created_ts":1791634945,"sol_usd":110.0,"symbol":"X","mcap_sol":{},"holders":{holders},"buyers":{holders},"net_sol":{net},"new_buyers_5s":2,"hhi":0.2,"dev_pct":1.0,"snipers_pct":3.0,"top10_pct":20.0,"progress":0.1{extra}}}"#,
                30.0 + net
            )
        };
        let mut micro = String::new();
        for t in T {
            micro += &row(
                t,
                "WIN",
                25.0,
                40,
                ",\"mayhem\":false,\"instant_grad\":false",
            );
            micro.push('\n');
            micro += &row(t, "DUD", 1.0, 3, ",\"mayhem\":false,\"instant_grad\":false");
            micro.push('\n');
            micro += &row(
                t,
                "MAY",
                25.0,
                40,
                ",\"mayhem\":true,\"instant_grad\":false",
            );
            micro.push('\n');
        }
        std::fs::write(day.join("micro.jsonl"), micro).unwrap();
        std::fs::write(
            day.join("curve_outcomes.jsonl"),
            r#"{"mint":"WIN","full_window":true,"mcap_at":[55,55,55,55,55,55,55],"peak_after":[400,400,400,400,400,400,400],"final_mcap_sol":380}
{"mint":"DUD","full_window":true,"mcap_at":[31,31,31,31,31,31,31],"peak_after":[31,31,31,31,31,31,31],"final_mcap_sol":28}
"#,
        )
        .unwrap();
        std::fs::write(
            day.join("checkpoints.jsonl"),
            r#"{"mint":"WIN","cp":3600,"mcap":150000,"liquidity":40000}
{"mint":"WIN","cp":10800,"mcap":9000000,"liquidity":40000}
{"mint":"DUD","cp":3600,"missed":true}
"#,
        )
        .unwrap();
        std::fs::write(
            day.join("trending.jsonl"),
            r#"{"mint":"WIN","list":"jup_trending_5m","ts":1791635545000}
{"mint":"WIN","list":"follow","ts":1791635000000}
"#,
        )
        .unwrap();
        let coins = load(dir.path(), None);
        assert_eq!(coins.len(), 3);
        let win = coins.iter().find(|c| c.mint == "WIN").unwrap();
        // the $9M reading has 0.4% liquidity behind it and does not count
        assert_eq!(win.peak_usd(), 150_000.0);
        assert_eq!(win.listed_ms, Some(1791635545000));
        assert_eq!(win.hour_from(15), Some((400.0 / 55.0, 380.0 / 55.0)));
        assert!(win.standard());
        assert!(coins.iter().find(|c| c.mint == "MAY").unwrap().mayhem);
        let a = Args {
            dir: dir.path().into(),
            trades: None,
            day: None,
            size_sol: 0.5,
            delay_s: 4.0,
            tx_cost_sol: 0.001,
            leaders_file: None,
        };
        let text = report(&coins, &[], &HashMap::new(), &HashSet::new(), &a);
        assert!(
            text.contains("standard coins reaching ≥ $30k: **1**"),
            "{text}"
        );
        assert!(text.contains("| X | `WIN` | $150k |"), "{text}");
        assert!(text.contains("No trade tape"), "{text}");
        assert!(load(dir.path(), Some("2026-10-09")).is_empty());
    }

    #[test]
    fn detector_rows_count_fires_and_hits() {
        let mk = |mint: &str, net: f64, peak: f64| {
            let mut c = Coin {
                mint: mint.into(),
                sol_usd: 100.0,
                cps: vec![(3600, peak)],
                mcap_at: vec![50.0; 7],
                peak_after: vec![peak / 100.0; 7],
                final_mcap: Some(40.0),
                ..Default::default()
            };
            for t in T {
                c.books.insert(
                    t,
                    Book {
                        mcap_sol: 50.0,
                        net_sol: net,
                        holders: 10,
                        ..Default::default()
                    },
                );
            }
            c
        };
        let coins: Vec<Coin> = (0..10)
            .map(|i| {
                mk(
                    &format!("C{i}"),
                    if i < 8 { 25.0 } else { 1.0 },
                    if i < 2 { 50_000.0 } else { 0.0 },
                )
            })
            .collect();
        let refs: Vec<&Coin> = coins.iter().collect();
        let w: HashSet<&str> = ["C0", "C1"].into();
        let rows = detector_rows(&refs, &w, 0.2);
        let r = rows
            .iter()
            .find(|r| r.t == 15 && r.rule == "net SOL ≥ 20")
            .unwrap();
        assert_eq!((r.fires, r.hits), (8, 2));
        assert!((r.recall - 1.0).abs() < 1e-9);
        assert!((r.lift - 1.25).abs() < 1e-9);
        // held to the hour: 40/50 after two fees
        assert!((r.hold.0 - (0.8 * (1.0 - FEE) * (1.0 - FEE) - 1.0)).abs() < 1e-9);
        assert!(
            rows.iter().all(|r| r.rule != "holders ≥ 50"),
            "too few fires are dropped"
        );
    }
}
