//! `copybot replay-trending`: entry and exit rules for the trending / established tier,
//! tested on the census's 5-minute captures.
//!
//! Every 5 minutes the census records Jupiter's trending (5 m / 1 h / 24 h), organic and
//! most-traded lists (100 coins each, with price, liquidity, holders, 5-minute flow,
//! organic score, holder concentration) and the attention lists (DexScreener boosts and
//! profiles, pump.fun live streams). A coin's path is its sequence of captures while it
//! is on any list, extended by the census's own checkpoints for coins under a day old.
//!
//! A rule decides at a capture (the coin just entered a list, or ranks high on the
//! 5-minute list) and filters on what that capture shows; the buy fills at that
//! capture's price moved by our size against the pool (average fill `1 + size/liquidity`,
//! the constant-product midpoint) plus the swap fee; exits are checked at every later
//! price point and fill the same way. A coin that drops off every list and has no later
//! price is sold at its last price with a haircut (`--delist-haircut`), flagged as
//! `delisted` in the report: those exits are the model's weakest point, and the recorder
//! keeps following listed coins so future data has no such holes.
//!
//! Selection is the same as the curve replay: every entry rule × exit pair is scored,
//! the pair is picked walk-forward (on earlier blocks, scored on the next), the newest
//! coins stay locked until `--final`, and the count of pairs tried is stated.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::census_report::read_kind_where;
use crate::replay::{bankroll, hm, pct, per_day, recorded_hours, Score, MIN_TRADES};

/// Rows closer than this belong to the same capture.
const CAPTURE_GAP_MS: i64 = 60_000;
/// A coin absent from the lists for longer than this has left them.
const LISTED_GAP_MS: i64 = 12 * 60_000;
/// Later prices (a reappearance, a checkpoint) are used up to this long after a
/// delisting; beyond it the haircut applies.
const LATER_PRICE_MS: i64 = 24 * 3_600_000;

/// One price point of a coin: a capture on the lists, or a census checkpoint.
#[derive(Clone, Debug, Default)]
pub struct Cap {
    pub ts: i64,
    pub price: f64,
    pub mcap: f64,
    pub liq: f64,
    pub holders: u64,
    pub buy5: f64,
    pub sell5: f64,
    pub net_buyers5: i64,
    pub organic: f64,
    pub top10: f64,
    pub rank_1h: Option<u32>,
    pub rank_5m: Option<u32>,
    pub rank_org: Option<u32>,
    pub rank_24h: Option<u32>,
    pub boosted: bool,
    pub live: bool,
    /// on a list at this capture (a checkpoint point is not)
    pub listed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Coin {
    pub symbol: String,
    pub launchpad: String,
    pub created_ms: Option<i64>,
    pub caps: Vec<Cap>,
}

impl Coin {
    fn age_min(&self, ts: i64) -> Option<f64> {
        self.created_ms.map(|c| (ts - c) as f64 / 60_000.0)
    }
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// Every coin with a price path, from the `trending` rows (all days under `dir`) and
/// the checkpoints. Returns the coins (by first capture) and the capture times.
pub fn load(dir: &Path) -> (Vec<Coin>, Vec<i64>) {
    let mut rows = read_kind_where(dir, "trending", |r| {
        r["ts"].is_i64() && r["mint"].is_string()
    });
    rows.sort_by_key(|r| r["ts"].as_i64().unwrap_or(0));
    // cluster rows into captures
    let mut captures: Vec<i64> = vec![];
    let mut cluster_of: Vec<usize> = Vec::with_capacity(rows.len());
    for r in &rows {
        let ts = r["ts"].as_i64().unwrap_or(0);
        if captures
            .last()
            .is_none_or(|last| ts - last > CAPTURE_GAP_MS)
        {
            captures.push(ts);
        }
        cluster_of.push(captures.len() - 1);
    }
    let mut coins: HashMap<String, Coin> = HashMap::new();
    let mut at: HashMap<(String, usize), Cap> = HashMap::new();
    let mut attention: HashMap<(String, usize), (bool, bool)> = HashMap::new();
    for (r, &ci) in rows.iter().zip(&cluster_of) {
        let mint = r["mint"].as_str().unwrap_or_default().to_string();
        let list = r["list"].as_str().unwrap_or_default();
        if let Ok(pk) = mint.parse() {
            if chain::base_assets::is_base_asset(&pk) {
                continue;
            }
        }
        let rank = r["rank"].as_u64().map(|x| x as u32);
        match list {
            "dex_boosts_top" | "dex_boosts_latest" | "dex_profiles_latest" => {
                attention.entry((mint, ci)).or_default().0 = true;
            }
            "pump_live" => {
                attention.entry((mint, ci)).or_default().1 = true;
            }
            l if l.starts_with("jup_") || l == "follow" => {
                let c = coins.entry(mint.clone()).or_default();
                if c.symbol.is_empty() {
                    c.symbol = r["symbol"].as_str().unwrap_or_default().to_string();
                }
                if c.launchpad.is_empty() {
                    c.launchpad = r["launchpad"].as_str().unwrap_or_default().to_string();
                }
                if c.created_ms.is_none() {
                    c.created_ms = r["created_ms"].as_i64();
                }
                let cap = at.entry((mint, ci)).or_insert_with(|| Cap {
                    ts: captures[ci],
                    price: f(&r["price_usd"]),
                    mcap: f(&r["mcap"]),
                    liq: f(&r["liquidity"]),
                    holders: r["holders"].as_u64().unwrap_or(0),
                    buy5: f(&r["buy_vol_5m"]),
                    sell5: f(&r["sell_vol_5m"]),
                    net_buyers5: r["net_buyers_5m"].as_i64().unwrap_or(0),
                    organic: f(&r["organic_score"]),
                    top10: f(&r["top_holders_pct"]),
                    listed: l != "follow",
                    ..Default::default()
                });
                match l {
                    "jup_trending_1h" => cap.rank_1h = rank,
                    "jup_trending_5m" => cap.rank_5m = rank,
                    "jup_organic_1h" => cap.rank_org = rank,
                    "jup_trending_24h" => cap.rank_24h = rank,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    for ((mint, ci), cap) in at {
        let (boosted, live) = attention
            .get(&(mint.clone(), ci))
            .copied()
            .unwrap_or_default();
        let mut cap = cap;
        cap.boosted = boosted;
        cap.live = live;
        if let Some(c) = coins.get_mut(&mint) {
            c.caps.push(cap);
        }
    }
    // the census checkpoints as extra price points (young coins only)
    let mints: HashSet<String> = coins.keys().cloned().collect();
    for r in read_kind_where(dir, "checkpoints", |r| {
        r["missed"] != true && r["price_usd"].as_f64().is_some_and(|p| p > 0.0)
    }) {
        let (Some(mint), Some(ts)) = (r["mint"].as_str(), r["ts"].as_i64()) else {
            continue;
        };
        if !mints.contains(mint) {
            continue;
        }
        if let Some(c) = coins.get_mut(mint) {
            c.caps.push(Cap {
                ts,
                price: f(&r["price_usd"]),
                mcap: f(&r["mcap"]),
                liq: f(&r["liquidity"]),
                holders: r["holders"].as_u64().unwrap_or(0),
                listed: false,
                ..Default::default()
            });
        }
    }
    let mut out: Vec<Coin> = coins
        .into_values()
        .filter(|c| c.caps.iter().any(|x| x.listed && x.price > 0.0))
        .map(|mut c| {
            // one point per minute at most, the listed one when both exist: a checkpoint
            // taken in the same minute as a capture adds nothing
            c.caps.sort_by_key(|x| (x.ts / 60_000, !x.listed, x.ts));
            c.caps.dedup_by_key(|x| x.ts / 60_000);
            c
        })
        .collect();
    out.sort_by_key(|c| c.caps.iter().find(|x| x.listed).map(|x| x.ts).unwrap_or(0));
    (out, captures)
}

// ------------------------------------------------------------------ rules

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// first capture with a rank ≤ N on the 1 h trending list
    Enter1h(u32),
    /// first capture with a rank ≤ N on the organic list
    EnterOrganic(u32),
    /// any capture in the top N of the 5-minute list
    Top5m(u32),
}

impl Trigger {
    fn label(&self) -> String {
        match self {
            Trigger::Enter1h(n) => format!("enters 1 h top {n}"),
            Trigger::EnterOrganic(n) => format!("enters organic top {n}"),
            Trigger::Top5m(n) => format!("in 5 m top {n}"),
        }
    }
    /// The capture indices (into `c.caps`) at which this trigger fires. `first_ts` is
    /// the first capture of the window: a coin already listed then has an unknown past.
    fn fires(&self, c: &Coin, first_ts: i64) -> Vec<usize> {
        let mut out = vec![];
        match self {
            Trigger::Enter1h(n) | Trigger::EnterOrganic(n) => {
                let rank = |x: &Cap| match self {
                    Trigger::Enter1h(_) => x.rank_1h,
                    _ => x.rank_org,
                };
                let mut was_in = false;
                for (i, x) in c.caps.iter().enumerate() {
                    if !x.listed {
                        continue;
                    }
                    let now_in = rank(x).is_some_and(|r| r <= *n);
                    if now_in && !was_in && x.ts > first_ts {
                        out.push(i);
                    }
                    was_in = now_in;
                }
            }
            Trigger::Top5m(n) => {
                for (i, x) in c.caps.iter().enumerate() {
                    if x.listed && x.rank_5m.is_some_and(|r| r <= *n) && x.ts > first_ts {
                        out.push(i);
                    }
                }
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub trigger: Trigger,
    pub min_liq: f64,
    pub min_age_min: f64,
    pub max_age_h: f64,
    pub min_organic: f64,
    pub max_top10: f64,
    pub min_net_buyers5: i64,
    pub min_buy_sell: f64,
}

impl Entry {
    fn passes(&self, c: &Coin, x: &Cap) -> bool {
        let age = c.age_min(x.ts);
        x.price > 0.0
            && x.liq >= self.min_liq
            && age.is_some_and(|a| a >= self.min_age_min && a <= self.max_age_h * 60.0)
            && x.organic >= self.min_organic
            && (self.max_top10 >= 100.0 || (x.top10 > 0.0 && x.top10 <= self.max_top10))
            && x.net_buyers5 >= self.min_net_buyers5
            && (self.min_buy_sell <= 1.0 || x.buy5 >= self.min_buy_sell * x.sell5.max(1.0))
    }
    pub fn label(&self) -> String {
        let mut v = vec![
            self.trigger.label(),
            format!("liq ≥ ${:.0}k", self.min_liq / 1e3),
        ];
        v.push(format!(
            "age {:.0} min – {:.0} h",
            self.min_age_min, self.max_age_h
        ));
        if self.min_organic > 0.0 {
            v.push(format!("organic ≥ {:.0}", self.min_organic));
        }
        if self.max_top10 < 100.0 {
            v.push(format!("top-10 ≤ {:.0}%", self.max_top10));
        }
        if self.min_net_buyers5 > 0 {
            v.push(format!("net buyers 5 m ≥ {}", self.min_net_buyers5));
        }
        if self.min_buy_sell > 1.0 {
            v.push(format!("buy/sell ≥ {:.1}", self.min_buy_sell));
        }
        v.join(" · ")
    }
}

pub fn entry_grid() -> Vec<Entry> {
    let mut v = vec![];
    for trigger in [
        Trigger::Enter1h(20),
        Trigger::Enter1h(100),
        Trigger::EnterOrganic(20),
        Trigger::EnterOrganic(100),
        Trigger::Top5m(20),
    ] {
        for min_liq in [10e3, 50e3, 150e3] {
            for max_age_h in [6.0, 24.0, 168.0] {
                for min_organic in [0.0, 50.0] {
                    for max_top10 in [100.0, 40.0] {
                        for min_net_buyers5 in [0, 10] {
                            for min_buy_sell in [1.0, 1.3] {
                                v.push(Entry {
                                    trigger,
                                    min_liq,
                                    min_age_min: 30.0,
                                    max_age_h,
                                    min_organic,
                                    max_top10,
                                    min_net_buyers5,
                                    min_buy_sell,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    v
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exit {
    pub tp: Option<f64>,
    pub sl: Option<f64>,
    pub trail: Option<f64>,
    pub max_hold_min: i64,
}

impl Exit {
    pub fn label(&self) -> String {
        let p = |x: Option<f64>, s: &str| x.map(|v| format!("{s}{:.0}%", v * 100.0));
        [
            p(self.tp, "tp "),
            p(self.sl, "sl "),
            p(self.trail, "trail "),
        ]
        .into_iter()
        .flatten()
        .chain([format!(
            "max {}",
            crate::census_report::age(self.max_hold_min * 60_000)
        )])
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

pub fn exit_grid() -> Vec<Exit> {
    let mut v = vec![];
    for tp in [Some(0.25), Some(0.5), Some(1.0), None] {
        for sl in [Some(0.15), Some(0.3), None] {
            for trail in [Some(0.25), None] {
                for max_hold_min in [60, 360, 1440] {
                    v.push(Exit {
                        tp,
                        sl,
                        trail,
                        max_hold_min,
                    });
                }
            }
        }
    }
    v
}

// ------------------------------------------------------------------ one trade

#[derive(Clone, Copy, Debug)]
pub struct Costs {
    pub size_sol: f64,
    pub sol_usd: f64,
    /// swap fee per side
    pub fee: f64,
    pub tx_sol: f64,
    /// what a position loses when its coin leaves every list and never prices again
    pub delist_haircut: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    TakeProfit,
    StopLoss,
    Trail,
    Time,
    Delisted,
    TapeEnd,
}

impl Why {
    fn label(&self) -> &'static str {
        match self {
            Why::TakeProfit => "take profit",
            Why::StopLoss => "stop loss",
            Why::Trail => "trailing stop",
            Why::Time => "time",
            Why::Delisted => "delisted (haircut)",
            Why::TapeEnd => "end of tape",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Trade {
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub ret: f64,
    pub pnl_sol: f64,
    pub why: Why,
}

/// Buy at capture `i` of `c`, sell by `exit`. `None` when the pool cannot take the size.
pub fn simulate(c: &Coin, i: usize, exit: &Exit, k: &Costs) -> Option<Trade> {
    let e = &c.caps[i];
    let size_usd = k.size_sol * k.sol_usd;
    if e.price <= 0.0 || e.liq <= 0.0 || size_usd > 0.2 * e.liq {
        return None;
    }
    // average fill against a constant-product pool, plus the fee
    let buy_price = e.price * (1.0 + size_usd / e.liq) * (1.0 + k.fee);
    let tokens = size_usd / buy_price;
    let deadline = e.ts + exit.max_hold_min * 60_000;
    let mut peak = e.price;
    let mut last_listed_ts = e.ts;
    let mut last_price = e.price;
    let mut last_liq = e.liq;
    let mut fill: Option<(i64, f64, f64, Why)> = None; // (ts, price, liq, why)
    for x in &c.caps[i + 1..] {
        if x.price <= 0.0 {
            continue;
        }
        // left every list for a while and this is the next known price: a delisting
        // that priced again counts as a normal point
        if x.ts > deadline {
            break;
        }
        let r = x.price / e.price - 1.0;
        peak = peak.max(x.price);
        let why = if exit.tp.is_some_and(|tp| r >= tp) {
            Some(Why::TakeProfit)
        } else if exit.sl.is_some_and(|sl| r <= -sl) {
            Some(Why::StopLoss)
        } else if exit.trail.is_some_and(|tr| x.price <= peak * (1.0 - tr)) {
            Some(Why::Trail)
        } else {
            None
        };
        last_price = x.price;
        last_liq = if x.liq > 0.0 { x.liq } else { last_liq };
        if x.listed {
            last_listed_ts = x.ts;
        }
        if let Some(w) = why {
            fill = Some((x.ts, x.price, last_liq, w));
            break;
        }
    }
    let (exit_ts, price, liq, why) = match fill {
        Some(f) => f,
        None => {
            // no trigger: the time limit, the end of the tape, or the coin vanished
            let last_ts = c.caps.last().map(|x| x.ts).unwrap_or(e.ts);
            let gone = last_listed_ts + LISTED_GAP_MS < deadline.min(last_ts)
                && c.caps[i + 1..]
                    .iter()
                    .all(|x| x.ts <= last_listed_ts || x.ts > deadline);
            if gone || (last_ts < deadline && last_listed_ts + LATER_PRICE_MS < deadline) {
                (
                    last_listed_ts,
                    last_price * (1.0 - k.delist_haircut),
                    last_liq,
                    Why::Delisted,
                )
            } else if last_ts < deadline {
                (last_ts, last_price, last_liq, Why::TapeEnd)
            } else {
                (deadline, last_price, last_liq, Why::Time)
            }
        }
    };
    let sell_price = price * (1.0 - size_usd / liq.max(size_usd)) * (1.0 - k.fee);
    let proceeds_usd = tokens * sell_price;
    let cost_sol = k.size_sol + k.tx_sol;
    let proceeds_sol = proceeds_usd / k.sol_usd - k.tx_sol;
    Some(Trade {
        entry_ts: e.ts,
        exit_ts,
        ret: proceeds_sol / cost_sol - 1.0,
        pnl_sol: proceeds_sol - cost_sol,
        why,
    })
}

// ------------------------------------------------------------------ the search

/// Every coin's trades per trigger and exit: `book[coin][trigger][k]` = (capture index, per-exit trade).
/// One firing of a trigger on a coin: the capture index and the trade under each exit.
type Firing = (usize, Vec<Option<Trade>>);

struct Book {
    trades: Vec<Vec<Vec<Firing>>>,
}

fn triggers() -> Vec<Trigger> {
    vec![
        Trigger::Enter1h(20),
        Trigger::Enter1h(100),
        Trigger::EnterOrganic(20),
        Trigger::EnterOrganic(100),
        Trigger::Top5m(20),
    ]
}

fn book(coins: &[Coin], first_ts: i64, exits: &[Exit], k: &Costs) -> Book {
    let trig = triggers();
    let trades = coins
        .iter()
        .map(|c| {
            trig.iter()
                .map(|t| {
                    t.fires(c, first_ts)
                        .into_iter()
                        .map(|i| (i, exits.iter().map(|x| simulate(c, i, x, k)).collect()))
                        .collect()
                })
                .collect()
        })
        .collect();
    Book { trades }
}

/// The trades of rule `e` × exit `xi` on the coins in `idx`. A rule takes a coin once:
/// its first qualifying trigger.
fn trades_of<'a>(
    coins: &'a [Coin],
    book: &'a Book,
    idx: &'a [usize],
    e: &'a Entry,
    xi: usize,
) -> impl Iterator<Item = Trade> + 'a {
    let ti = triggers()
        .iter()
        .position(|t| *t == e.trigger)
        .expect("trigger");
    idx.iter().filter_map(move |&ci| {
        let c = &coins[ci];
        book.trades[ci][ti]
            .iter()
            .find(|(i, _)| e.passes(c, &c.caps[*i]))
            .and_then(|(_, per_exit)| per_exit[xi])
    })
}

fn score(coins: &[Coin], book: &Book, idx: &[usize], e: &Entry, xi: usize) -> Score {
    let mut s = Score::default();
    for t in trades_of(coins, book, idx, e, xi) {
        s.add_ret(t.ret, t.pnl_sol);
    }
    s
}

fn best_pairs(
    coins: &[Coin],
    book: &Book,
    idx: &[usize],
    entries: &[Entry],
    exits: &[Exit],
    min_trades: usize,
    n: usize,
) -> Vec<(usize, usize, Score)> {
    let mut v = vec![];
    for (ei, e) in entries.iter().enumerate() {
        for xi in 0..exits.len() {
            let s = score(coins, book, idx, e, xi);
            if s.n >= min_trades {
                v.push((ei, xi, s));
            }
        }
    }
    v.sort_by(|a, b| {
        b.2.lower()
            .partial_cmp(&a.2.lower())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v.truncate(n);
    v
}

// ------------------------------------------------------------------ the report

pub struct Args {
    pub dir: PathBuf,
    pub size_sol: f64,
    pub sol_usd: Option<f64>,
    pub fee: f64,
    pub tx_sol: f64,
    pub delist_haircut: f64,
    pub block_hours: i64,
    pub holdout: f64,
    pub final_run: bool,
    pub min_trades: usize,
    pub bankroll_sol: f64,
    pub bet_share: f64,
    pub max_open: usize,
}

/// The SOL price over the window, from the census's `sol_price` rows.
fn sol_price_of(dir: &Path) -> Option<f64> {
    let mut v: Vec<f64> = read_kind_where(dir, "sol_price", |_| true)
        .iter()
        .filter_map(|r| r["usd"].as_f64())
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

pub fn run(a: &Args) -> String {
    let (coins, captures) = load(&a.dir);
    let sol_usd = a.sol_usd.or_else(|| sol_price_of(&a.dir)).unwrap_or(110.0);
    let k = Costs {
        size_sol: a.size_sol,
        sol_usd,
        fee: a.fee,
        tx_sol: a.tx_sol,
        delist_haircut: a.delist_haircut,
    };
    let mut o = String::new();
    let _ = writeln!(o, "# Replay of the trending tier (5-minute captures)\n");
    if coins.is_empty() || captures.len() < 3 {
        let _ = writeln!(o, "No trending captures under {} yet.", a.dir.display());
        return o;
    }
    let first_ts = captures[0];
    let hours = recorded_hours(captures.iter().map(|t| t / 1000).collect());
    let lock_from = coins[((coins.len() as f64) * (1.0 - a.holdout)).floor() as usize..]
        .first()
        .and_then(|c| c.caps.iter().find(|x| x.listed).map(|x| x.ts))
        .unwrap_or(i64::MAX);
    let first_listed = |c: &Coin| c.caps.iter().find(|x| x.listed).map(|x| x.ts).unwrap_or(0);
    let usable: Vec<usize> = (0..coins.len())
        .filter(|&i| a.final_run || first_listed(&coins[i]) < lock_from)
        .collect();
    let points: usize = usable.iter().map(|&i| coins[i].caps.len()).sum();
    let _ = writeln!(
        o,
        "- data: **{}** captures from {} to {} UTC, **{:.1} h of recording**; {} coins with a path ({} price points)",
        captures.len(),
        hm(captures[0] / 1000),
        hm(captures[captures.len() - 1] / 1000),
        hours,
        usable.len(),
        points
    );
    let _ = writeln!(
        o,
        "- holdout: {}",
        if a.final_run {
            format!(
                "**included** (`--final`): the newest {:.0}% of coins are the final, one-time test",
                a.holdout * 100.0
            )
        } else {
            format!(
                "{} newest coins (listed from {} UTC) locked away; nothing below has seen them",
                coins.len() - usable.len(),
                hm(lock_from / 1000)
            )
        }
    );
    let _ = writeln!(
        o,
        "- costs: {} SOL a trade at ${sol_usd:.0}/SOL, fill moved by size/liquidity against the pool, {:.2}% fee a side, {} SOL per transaction; a coin that leaves every list and never prices again is sold at its last price less {:.0}%",
        a.size_sol,
        a.fee * 100.0,
        a.tx_sol,
        a.delist_haircut * 100.0
    );

    // base rates from the first entry into the 1 h top 100
    let mut n_enter = 0usize;
    let (mut up15, mut up2, mut held, mut gone) = (0usize, 0usize, 0usize, 0usize);
    let (mut mcaps, mut holders, mut liqs) = (vec![], vec![], vec![]);
    for &i in &usable {
        let c = &coins[i];
        for e in Trigger::Enter1h(100).fires(c, first_ts).into_iter().take(1) {
            let base = c.caps[e].price;
            if base <= 0.0 {
                continue;
            }
            n_enter += 1;
            mcaps.push(c.caps[e].mcap);
            holders.push(c.caps[e].holders as f64);
            liqs.push(c.caps[e].liq);
            let later: Vec<&Cap> = c.caps[e + 1..].iter().filter(|x| x.price > 0.0).collect();
            let peak = later.iter().map(|x| x.price).fold(0.0, f64::max);
            up15 += (peak >= 1.5 * base) as usize;
            up2 += (peak >= 2.0 * base) as usize;
            held += later.last().is_some_and(|x| x.price >= base) as usize;
            gone += later.is_empty() as usize;
        }
    }
    let _ = writeln!(
        o,
        "- from the first entry into the 1 h top 100 ({n_enter} coins): later peak ≥ 1.5× {:.0}% · ≥ 2× {:.0}% · last price ≥ entry {:.0}% · never priced again {:.0}%",
        100.0 * up15 as f64 / n_enter.max(1) as f64,
        100.0 * up2 as f64 / n_enter.max(1) as f64,
        100.0 * held as f64 / n_enter.max(1) as f64,
        100.0 * gone as f64 / n_enter.max(1) as f64
    );
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let _ = writeln!(
        o,
        "- at that entry (medians): market cap ${:.0}k · liquidity ${:.0}k · holders {:.0}",
        med(&mut mcaps) / 1e3,
        med(&mut liqs) / 1e3,
        med(&mut holders)
    );
    let entries = entry_grid();
    let exits = exit_grid();
    let _ = writeln!(
        o,
        "- searched: {} entry rules × {} exits = **{} pairs**; a pair needs {}+ trades to be picked",
        entries.len(),
        exits.len(),
        entries.len() * exits.len(),
        a.min_trades
    );
    let bk = book(&coins, first_ts, &exits, &k);

    let top = best_pairs(&coins, &bk, &usable, &entries, &exits, a.min_trades, 10);
    let _ = writeln!(
        o,
        "\n## Best pairs on all unlocked data (in sample, optimistic)"
    );
    let _ = writeln!(
        o,
        "| entry | exit | trades | win | mean | 95% low | profit factor | SOL at {} | delisted |",
        a.size_sol
    );
    let _ = writeln!(o, "|---|---|---:|---:|---:|---:|---:|---:|---:|");
    for (ei, xi, s) in &top {
        let delisted = trades_of(&coins, &bk, &usable, &entries[*ei], *xi)
            .filter(|t| t.why == Why::Delisted)
            .count();
        let _ = writeln!(
            o,
            "| {} | {} | {} | {:.0}% | {} | {} | {:.2} | {:+.3} | {} |",
            entries[*ei].label(),
            exits[*xi].label(),
            s.n,
            100.0 * s.wins as f64 / s.n.max(1) as f64,
            pct(s.mean()),
            pct(s.lower()),
            s.pf(),
            s.pnl,
            delisted
        );
    }
    if top.is_empty() {
        let _ = writeln!(
            o,
            "| _no pair has {}+ trades yet_ | | | | | | | | |",
            a.min_trades
        );
    } else if top[0].2.lower() <= 0.0 {
        let _ = writeln!(
            o,
            "\n_Not one of the {} pairs is profitable at 95% confidence even on the data it was picked on._",
            entries.len() * exits.len()
        );
    }

    // walk-forward by the time a coin first listed
    let block_ms = a.block_hours.max(1) * 3_600_000;
    let mut blocks: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for &i in &usable {
        blocks
            .entry((first_listed(&coins[i]) - first_ts).div_euclid(block_ms))
            .or_default()
            .push(i);
    }
    let mut seen: Vec<usize> = vec![];
    let mut oos = Score::default();
    let mut hold = Score::default();
    let mut oos_trades: Vec<(i64, i64, f64)> = vec![];
    let mut oos_first: Vec<i64> = vec![];
    let mut reasons: BTreeMap<&'static str, (usize, f64)> = BTreeMap::new();
    let mut last_pick: Option<(usize, usize)> = None;
    let _ = writeln!(o, "\n## Walk-forward ({} h blocks by listing time): the pair picked on earlier blocks, traded on the next", a.block_hours);
    let _ = writeln!(
        o,
        "| block (UTC) | coins | pair picked before | trades here | win | mean | SOL |"
    );
    let _ = writeln!(o, "|---|---:|---|---:|---:|---:|---:|");
    for (b, idx) in &blocks {
        let pick = if seen.is_empty() {
            None
        } else {
            best_pairs(&coins, &bk, &seen, &entries, &exits, a.min_trades, 1).pop()
        };
        let mut here = Score::default();
        let holdout = first_listed(&coins[idx[0]]) >= lock_from;
        if let Some((ei, xi, _)) = &pick {
            for t in trades_of(&coins, &bk, idx, &entries[*ei], *xi) {
                here.add_ret(t.ret, t.pnl_sol);
                oos.add_ret(t.ret, t.pnl_sol);
                if holdout {
                    hold.add_ret(t.ret, t.pnl_sol);
                }
                oos_trades.push((t.entry_ts / 1000, t.exit_ts / 1000, t.ret));
                let r = reasons.entry(t.why.label()).or_default();
                r.0 += 1;
                r.1 += t.pnl_sol;
            }
            last_pick = Some((*ei, *xi));
            oos_first.extend(idx.iter().map(|&i| first_listed(&coins[i]) / 1000));
        }
        let _ = writeln!(
            o,
            "| {}{} | {} | {} | {} | {} | {} | {:+.3} |",
            hm((first_ts + b * block_ms) / 1000),
            if holdout { " (holdout)" } else { "" },
            idx.len(),
            pick.as_ref()
                .map(|(ei, xi, _)| format!("{} → {}", entries[*ei].label(), exits[*xi].label()))
                .unwrap_or_else(|| "_nothing qualifies yet_".into()),
            here.n,
            if here.n > 0 {
                format!("{:.0}%", 100.0 * here.wins as f64 / here.n as f64)
            } else {
                "-".into()
            },
            if here.n > 0 {
                pct(here.mean())
            } else {
                "-".into()
            },
            here.pnl
        );
        seen.extend(idx);
    }
    let oos_hours = recorded_hours(oos_first);
    let _ = writeln!(
        o,
        "\n**Out of sample: {} trades · win {:.0}% · mean {} per trade (95% low {}) · profit factor {:.2} · {:+.3} SOL at {} SOL a trade · SOL/day at this pace: {}**",
        oos.n,
        100.0 * oos.wins as f64 / oos.n.max(1) as f64,
        pct(oos.mean()),
        pct(oos.lower()),
        oos.pf(),
        oos.pnl,
        a.size_sol,
        per_day(oos.pnl, oos_hours)
    );
    if !reasons.is_empty() {
        let _ = writeln!(
            o,
            "- how they ended: {}",
            reasons
                .iter()
                .map(|(k, (n, p))| format!("{k} {n} ({p:+.3} SOL)"))
                .collect::<Vec<_>>()
                .join(" · ")
        );
    }
    if a.final_run && hold.n > 0 {
        let _ = writeln!(
            o,
            "\n**Holdout (final test): {} trades · mean {} (95% low {}) · {:+.3} SOL**",
            hold.n,
            pct(hold.mean()),
            pct(hold.lower()),
            hold.pnl
        );
    }
    if oos.n < MIN_TRADES {
        let _ = writeln!(o, "_Too few out-of-sample trades to conclude anything; the recorder needs to run longer._");
    }
    if !oos_trades.is_empty() {
        let (end, dd, taken) = bankroll(&oos_trades, a.bankroll_sol, a.bet_share, a.max_open);
        let _ = writeln!(
            o,
            "\n## Bankroll: {} SOL, {:.0}% of it per trade, at most {} open (walk-forward trades)\n- ends at **{:.3} SOL** after {} trades ({} skipped while capital was busy) · deepest drawdown {:.0}% · {:.1} h",
            a.bankroll_sol,
            a.bet_share * 100.0,
            a.max_open,
            end,
            taken,
            oos_trades.len() - taken,
            dd * 100.0,
            oos_hours
        );
    }
    if let Some((ei, xi)) = last_pick {
        let e = &entries[ei];
        let x = exits[xi];
        let _ = writeln!(o, "\n## Size for the latest pick ({} → {}), on all unlocked data: the pool's depth is the limit", e.label(), x.label());
        let _ = writeln!(
            o,
            "| size SOL | trades (pool deep enough) | mean | SOL per trade | SOL/day |"
        );
        let _ = writeln!(o, "|---:|---:|---:|---:|---:|");
        for size in [1.0, 5.0, 20.0, 50.0] {
            let kk = Costs {
                size_sol: size,
                ..k
            };
            let mut s = Score::default();
            for &ci in &usable {
                let c = &coins[ci];
                if let Some(i) = e
                    .trigger
                    .fires(c, first_ts)
                    .into_iter()
                    .find(|&i| e.passes(c, &c.caps[i]))
                {
                    if let Some(t) = simulate(c, i, &x, &kk) {
                        s.add_ret(t.ret, t.pnl_sol);
                    }
                }
            }
            let _ = writeln!(
                o,
                "| {size} | {} | {} | {:+.4} | {} |",
                s.n,
                pct(s.mean()),
                s.pnl / s.n.max(1) as f64,
                per_day(s.pnl, hours)
            );
        }
    }
    o
}

pub fn write(a: &Args) -> anyhow::Result<String> {
    let text = run(a);
    let d = a.dir.join("replay");
    std::fs::create_dir_all(&d)?;
    let name = chrono::Utc::now().format("%Y-%m-%dT%H%M%S").to_string();
    std::fs::write(d.join(format!("trending-{name}.md")), &text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_791_600_000_000;

    fn cap(ts: i64, price: f64, rank_1h: Option<u32>) -> Cap {
        Cap {
            ts,
            price,
            mcap: price * 1e9,
            liq: 100_000.0,
            holders: 500,
            organic: 80.0,
            top10: 20.0,
            rank_1h,
            listed: true,
            ..Default::default()
        }
    }

    fn costs() -> Costs {
        Costs {
            size_sol: 5.0,
            sol_usd: 100.0,
            fee: 0.005,
            tx_sol: 0.001,
            delist_haircut: 0.3,
        }
    }

    fn coin(path: &[(i64, f64, Option<u32>)]) -> Coin {
        Coin {
            symbol: "MMM".into(),
            launchpad: "pump.fun".into(),
            created_ms: Some(T0 - 3_600_000),
            caps: path
                .iter()
                .map(|(m, p, r)| cap(T0 + m * 60_000, *p, *r))
                .collect(),
        }
    }

    #[test]
    fn fills_move_with_size_against_the_pool_and_pay_the_fee() {
        // flat price: the round trip costs the two fees and the two impacts
        let c = coin(&[(0, 1.0, Some(5)), (5, 1.0, Some(5)), (10, 1.0, Some(5))]);
        let x = Exit {
            tp: None,
            sl: None,
            trail: None,
            max_hold_min: 10,
        };
        let t = simulate(&c, 0, &x, &costs()).unwrap();
        assert_eq!(t.why, Why::Time);
        // 5 SOL = $500 into $100k: 0.5% impact each way, 0.5% fee each way, 0.001 SOL twice
        let want = (1.0 - 0.005) * (1.0 - 0.005) / ((1.0 + 0.005) * (1.0 + 0.005));
        let got = (t.pnl_sol + 5.0 + 0.001 + 0.001) / 5.0;
        assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        assert!(t.ret < -0.02 && t.ret > -0.025, "{}", t.ret);
        // a position a fifth of the pool or more is not filled
        assert!(simulate(
            &c,
            0,
            &x,
            &Costs {
                size_sol: 250.0,
                ..costs()
            }
        )
        .is_none());
    }

    #[test]
    fn take_profit_stop_and_delisting() {
        let x = Exit {
            tp: Some(0.5),
            sl: Some(0.3),
            trail: None,
            max_hold_min: 120,
        };
        let up = coin(&[
            (0, 1.0, Some(5)),
            (5, 1.2, Some(3)),
            (10, 1.6, Some(1)),
            (15, 1.4, Some(2)),
        ]);
        let t = simulate(&up, 0, &x, &costs()).unwrap();
        assert_eq!(t.why, Why::TakeProfit);
        assert_eq!(t.exit_ts, T0 + 10 * 60_000);
        assert!(t.ret > 0.5 && t.ret < 0.6, "{}", t.ret);
        let down = coin(&[(0, 1.0, Some(5)), (5, 0.8, Some(30)), (10, 0.65, Some(80))]);
        let t = simulate(&down, 0, &x, &costs()).unwrap();
        assert_eq!(t.why, Why::StopLoss);
        // listed once, then gone for good within the window: the haircut applies
        let mut gone = coin(&[(0, 1.0, Some(5)), (5, 1.1, Some(4))]);
        gone.caps.push(Cap {
            ts: T0 + 600 * 60_000,
            price: 0.0,
            listed: false,
            ..Default::default()
        });
        let t = simulate(&gone, 0, &x, &costs()).unwrap();
        assert_eq!(t.why, Why::Delisted);
        assert!(
            t.ret < -0.2 && t.ret > -0.26,
            "1.1 × 0.7 less costs: {}",
            t.ret
        );
        // a checkpoint price after the delisting is used instead
        let mut later = coin(&[(0, 1.0, Some(5)), (5, 1.1, Some(4))]);
        later.caps.push(Cap {
            ts: T0 + 60 * 60_000,
            price: 0.5,
            liq: 50_000.0,
            listed: false,
            ..Default::default()
        });
        let t = simulate(&later, 0, &x, &costs()).unwrap();
        assert_eq!(t.why, Why::StopLoss);
        assert_eq!(t.exit_ts, T0 + 60 * 60_000);
    }

    #[test]
    fn triggers_fire_on_entering_a_list_but_not_for_coins_listed_from_the_start() {
        let c = coin(&[
            (0, 1.0, Some(50)),
            (5, 1.0, Some(15)),
            (10, 1.0, Some(12)),
            (15, 1.0, Some(40)),
            (20, 1.0, Some(10)),
        ]);
        // the window starts at T0: the first capture has an unknown past
        assert_eq!(Trigger::Enter1h(100).fires(&c, T0), Vec::<usize>::new());
        assert_eq!(Trigger::Enter1h(20).fires(&c, T0), vec![1, 4]);
        assert_eq!(Trigger::Enter1h(20).fires(&c, T0 - 1), vec![1, 4]);
        assert_eq!(Trigger::Enter1h(100).fires(&c, T0 - 1), vec![0]);
        let e = Entry {
            trigger: Trigger::Enter1h(20),
            min_liq: 50_000.0,
            min_age_min: 30.0,
            max_age_h: 24.0,
            min_organic: 50.0,
            max_top10: 40.0,
            min_net_buyers5: 0,
            min_buy_sell: 1.0,
        };
        assert!(e.passes(&c, &c.caps[1]));
        let strict = Entry {
            min_net_buyers5: 10,
            ..e.clone()
        };
        assert!(!strict.passes(&c, &c.caps[1]));
        assert!(e.label().contains("enters 1 h top 20"));
    }

    #[test]
    fn captures_are_clustered_and_checkpoints_extend_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let t = crate::census::Tape::new(dir.path());
        let row = |ts: i64, list: &str, mint: &str, rank: u32, price: f64| {
            serde_json::json!({"ts": ts, "list": list, "mint": mint, "rank": rank, "price_usd": price, "mcap": price * 1e9,
                "liquidity": 80_000.0, "holders": 300, "organic_score": 60.0, "top_holders_pct": 25.0,
                "net_buyers_5m": 12, "buy_vol_5m": 900.0, "sell_vol_5m": 500.0, "symbol": "AAA", "launchpad": "pump.fun",
                "created_ms": T0 - 7_200_000})
        };
        for k in 0..3i64 {
            let ts = T0 + k * 300_000;
            t.row(
                "trending",
                ts,
                &row(
                    ts,
                    "jup_trending_1h",
                    "Apump",
                    10 + k as u32,
                    1.0 + 0.1 * k as f64,
                ),
            )
            .unwrap();
            t.row(
                "trending",
                ts + 2_000,
                &row(
                    ts + 2_000,
                    "jup_trending_5m",
                    "Apump",
                    3,
                    1.0 + 0.1 * k as f64,
                ),
            )
            .unwrap();
            t.row("trending", ts + 3_000, &serde_json::json!({"ts": ts + 3_000, "list": "pump_live", "mint": "Apump", "rank": 1})).unwrap();
        }
        // a stablecoin on the list is not a coin
        t.row(
            "trending",
            T0,
            &row(
                T0,
                "jup_trending_1h",
                "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                1,
                1.0,
            ),
        )
        .unwrap();
        // a checkpoint an hour later extends the path
        t.row("checkpoints", T0 + 3_600_000, &serde_json::json!({"ts": T0 + 3_600_000, "mint": "Apump", "cp": 3600, "price_usd": 0.6, "mcap": 6e8, "liquidity": 40_000.0})).unwrap();
        // a checkpoint in the same minute as the second capture must not replace the listed point
        t.row("checkpoints", T0 + 300_000 + 10_000, &serde_json::json!({"ts": T0 + 300_000 + 10_000, "mint": "Apump", "cp": 300, "price_usd": 1.05, "mcap": 1.05e9, "liquidity": 70_000.0})).unwrap();
        let (coins, captures) = load(dir.path());
        assert_eq!(
            captures.len(),
            3,
            "three captures, rows seconds apart clustered"
        );
        assert_eq!(coins.len(), 1);
        let c = &coins[0];
        assert_eq!(c.caps.len(), 4);
        assert!(
            c.caps[1].listed && c.caps[1].rank_1h == Some(11),
            "the capture won over the checkpoint in its minute"
        );
        assert_eq!(c.caps[0].rank_1h, Some(10));
        assert_eq!(c.caps[0].rank_5m, Some(3));
        assert!(c.caps[0].live);
        assert!(!c.caps[3].listed);
        assert_eq!(c.caps[3].price, 0.6);
        assert_eq!(c.created_ms, Some(T0 - 7_200_000));
    }
}
