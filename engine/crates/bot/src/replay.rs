//! `copybot replay`: entry and exit rules tested on the recorded trade tape.
//!
//! The census keeps every Pump.fun trade of each new coin's first hour and each coin's
//! book at fixed moments (`micro.jsonl`). Replaying them answers "would this rule have
//! made money, after fees and our delay?" without risking any:
//!
//! * entry: a rule looks at a coin's book at 15, 30 or 60 s; when it passes, our buy
//!   lands `delay` later and fills on the curve as it was then (exact curve formula and
//!   fee, so our own size moves the price against us);
//! * exit: take-profit, stop-loss, trailing stop or time limit, checked on every later
//!   trade; the sell lands `delay` after the trigger and fills on the curve as it is
//!   then; a coin that graduates is sold at its final price into a pool as deep as the
//!   SOL in its curve (minus the AMM fee);
//! * every entry rule × exit pair is scored, and one is chosen the honest way:
//!   walk-forward (picked on earlier blocks of time, scored only on the next block), with
//!   the newest data locked away until `--final`, and the number of pairs tried stated;
//! * from the walk-forward trades: SOL per day at the replay size, a bankroll run from
//!   1 SOL, and how the result changes with trade size and delay.
//!
//! Model: our buy stays in the curve until we sell (the other trades are taken as
//! recorded, so a round trip with nothing in between costs exactly the two fees);
//! trades in the same second as our entry are not exit chances; block times have 1 s
//! resolution, so the delay is rounded up to whole seconds. One assumption in our
//! favour: the book a rule reads was written 10 s after its moment, so it includes
//! trades a live engine would still be waiting for (the stream runs 1-3 s behind).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use chain::pump::{buy_tokens_for_quote, sell_quote_for_tokens, CurveState};
use serde::Deserialize;
use serde_json::Value;

/// Moments (s after creation) at which a rule may decide.
pub const DECIDE_AT: [i64; 3] = [15, 30, 60];
/// Pump.fun's standard curve: virtual token reserves exceed the real ones by this much.
const VIRTUAL_TOKEN_OFFSET: u64 = 279_900_000_000_000;
const START_VSOL: u64 = 30_000_000_000;
const START_VTOK: u64 = 1_073_000_000_000_000;
/// Curve fee per side (protocol + creator) when the tape does not carry it.
const DEFAULT_FEE_BPS: u64 = 125;
/// Fee for selling a graduated coin on the AMM.
const AMM_FEE_BPS: u64 = 30;
/// The tape covers each coin's first hour.
const WINDOW_SECS: i64 = 3600;
/// A pair needs this many trades in its training blocks to be picked.
pub(crate) const MIN_TRADES: usize = 30;
const LAMPORTS: f64 = 1e9;

// ------------------------------------------------------------------ the tape

/// The curve after one trade.
#[derive(Clone, Copy, Debug)]
struct Tick {
    ts: i64,
    slot: u64,
    seq: u64,
    vsol: u64,
    vtok: u64,
    rsol: u64,
    rtok: u64,
    fee_bps: u64,
}

impl Tick {
    fn start(ts: i64) -> Self {
        Self {
            ts,
            slot: 0,
            seq: 0,
            vsol: START_VSOL,
            vtok: START_VTOK,
            rsol: 0,
            rtok: START_VTOK - VIRTUAL_TOKEN_OFFSET,
            fee_bps: DEFAULT_FEE_BPS,
        }
    }
    fn state(&self) -> CurveState {
        CurveState {
            virtual_token_reserves: self.vtok,
            virtual_quote_reserves: self.vsol,
            real_token_reserves: self.rtok,
            real_quote_reserves: self.rsol,
        }
    }
}

/// The AMM pool a graduated curve lands in: its real SOL, at the curve's final price.
fn pool_after_graduation(s: CurveState) -> CurveState {
    let sol = s.real_quote_reserves.max(1);
    let tokens = (sol as u128 * s.virtual_token_reserves as u128
        / s.virtual_quote_reserves.max(1) as u128) as u64;
    CurveState {
        virtual_token_reserves: tokens,
        virtual_quote_reserves: sol,
        real_token_reserves: tokens,
        real_quote_reserves: sol,
    }
}

/// The curve with our position in it: `input` lamports added, `tokens` taken out.
fn with_us(s: CurveState, input: u64, tokens: u64) -> CurveState {
    CurveState {
        virtual_token_reserves: s.virtual_token_reserves.saturating_sub(tokens),
        virtual_quote_reserves: s.virtual_quote_reserves + input,
        real_token_reserves: s.real_token_reserves.saturating_sub(tokens),
        real_quote_reserves: s.real_quote_reserves + input,
    }
}

/// Lamports of a buy of `size` that reach the curve (the fee is taken on top).
fn curve_input(size: u64, fee_bps: u64) -> u64 {
    ((size.saturating_sub(1)) as u128 * 10_000 / (fee_bps as u128 + 10_000)) as u64
}

/// A coin's book at a decision moment (the `micro.jsonl` row).
#[derive(Clone, Debug, Default)]
pub struct Feat {
    pub holders: u64,
    pub hhi: f64,
    pub dev_sold: bool,
    pub snipers_pct: f64,
    pub progress: f64,
    pub gap_ms: i64,
}

impl Feat {
    fn from_row(r: &Value) -> Self {
        Self {
            holders: r["holders"].as_u64().unwrap_or(0),
            hhi: r["hhi"].as_f64().unwrap_or(1.0),
            dev_sold: r["dev_sold"] == true,
            snipers_pct: r["snipers_pct"].as_f64().unwrap_or(0.0),
            progress: r["progress"].as_f64().unwrap_or(0.0),
            gap_ms: r["gap_ms"].as_i64().unwrap_or(0),
        }
    }
}

/// One coin as the tape knows it.
#[derive(Default)]
pub struct Coin {
    pub mint: String,
    pub created_ts: i64,
    ticks: Vec<Tick>,
    pub graduated_ts: Option<i64>,
    /// priced in another token (amounts are not SOL)
    pub quoted: bool,
    pub feats: HashMap<i64, Feat>,
}

impl Coin {
    /// The curve as it was at block time `ts` (after the last trade at or before it).
    fn at(&self, ts: i64) -> Tick {
        let i = self.ticks.partition_point(|t| t.ts <= ts);
        if i == 0 {
            Tick::start(self.created_ts)
        } else {
            self.ticks[i - 1]
        }
    }
}

#[derive(Deserialize)]
struct TapeRow {
    ev: String,
    #[serde(default)]
    slot: u64,
    #[serde(default)]
    ts: i64,
    mint: String,
    vsol: Option<u64>,
    vtok: Option<u64>,
    rsol: Option<u64>,
    rtok: Option<u64>,
    fee_bps: Option<u64>,
    quote: Option<String>,
}

fn files(dir: &Path, keep: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(keep) {
                out.push(p);
            }
        }
    }
    // day folders and hour files sort in time order
    out.sort();
    out
}

fn day_of_secs(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

/// Every coin whose create, trades and books are on the tape under `dir`, created on
/// a day in `[from, to]` (UTC, inclusive; `None` = no bound).
pub fn load(dir: &Path, from: Option<&str>, to: Option<&str>) -> anyhow::Result<Vec<Coin>> {
    let mut coins: HashMap<String, Coin> = HashMap::new();
    let mut seq = 0u64;
    for p in files(dir, &|n| {
        n.starts_with("trades-") && n.ends_with(".jsonl.gz")
    }) {
        let f = std::fs::File::open(&p)?;
        let r = std::io::BufReader::new(flate2::read::MultiGzDecoder::new(f));
        for line in r.lines() {
            let Ok(line) = line else { break };
            let Ok(row) = serde_json::from_str::<TapeRow>(&line) else {
                continue;
            };
            seq += 1;
            let c = coins.entry(row.mint.clone()).or_default();
            match row.ev.as_str() {
                "create" => c.created_ts = row.ts,
                "complete" => {
                    c.graduated_ts.get_or_insert(row.ts);
                }
                "trade" => {
                    let (Some(vsol), Some(vtok)) = (row.vsol, row.vtok) else {
                        continue;
                    };
                    c.quoted |= row.quote.as_deref().is_some_and(|q| {
                        q != "So11111111111111111111111111111111111111112"
                            && q != "11111111111111111111111111111111"
                    });
                    c.ticks.push(Tick {
                        ts: row.ts,
                        slot: row.slot,
                        seq,
                        vsol,
                        vtok,
                        rsol: row.rsol.unwrap_or(vsol.saturating_sub(START_VSOL)),
                        rtok: row
                            .rtok
                            .unwrap_or(vtok.saturating_sub(VIRTUAL_TOKEN_OFFSET)),
                        fee_bps: row.fee_bps.unwrap_or(DEFAULT_FEE_BPS),
                    });
                }
                _ => {}
            }
        }
    }
    for p in files(dir, &|n| n == "micro.jsonl") {
        let f = std::fs::File::open(&p)?;
        for line in std::io::BufReader::new(f).lines() {
            let Ok(line) = line else { break };
            let Ok(r) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let (Some(mint), Some(t)) = (r["mint"].as_str(), r["t"].as_i64()) else {
                continue;
            };
            if !DECIDE_AT.contains(&t) {
                continue;
            }
            if let Some(c) = coins.get_mut(mint) {
                c.feats.insert(t, Feat::from_row(&r));
            }
        }
    }
    let mut out: Vec<Coin> = coins
        .into_iter()
        .filter(|(_, c)| c.created_ts > 0 && !c.feats.is_empty())
        .filter(|(_, c)| {
            let d = day_of_secs(c.created_ts);
            from.is_none_or(|f| d.as_str() >= f) && to.is_none_or(|t| d.as_str() <= t)
        })
        .map(|(mint, mut c)| {
            c.mint = mint;
            c.ticks.sort_by_key(|t| (t.slot, t.seq));
            c
        })
        .collect();
    out.sort_by_key(|c| c.created_ts);
    Ok(out)
}

// ------------------------------------------------------------------ one trade

/// When to sell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exit {
    /// take profit at this return (0.6 = +60%)
    pub tp: Option<f64>,
    /// stop loss at this loss (0.3 = -30%)
    pub sl: Option<f64>,
    /// sell when the value falls this far below its peak since entry
    pub trail: Option<f64>,
    /// sell this many seconds after entry at the latest
    pub max_hold: i64,
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
        .chain([format!("max {}s", self.max_hold)])
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

/// What a trade costs us beyond the curve.
#[derive(Clone, Copy, Debug)]
pub struct Costs {
    pub size_lamports: u64,
    /// seconds from the decision moment (or an exit trigger) to our transaction landing
    pub delay_s: i64,
    /// network fee + priority fee per transaction
    pub tx_lamports: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    TakeProfit,
    StopLoss,
    Trail,
    Time,
    Graduated,
    TapeEnd,
}

impl Why {
    fn label(&self) -> &'static str {
        match self {
            Why::TakeProfit => "take profit",
            Why::StopLoss => "stop loss",
            Why::Trail => "trailing stop",
            Why::Time => "time",
            Why::Graduated => "graduated",
            Why::TapeEnd => "end of tape",
        }
    }
}

/// One simulated round trip.
#[derive(Clone, Copy, Debug)]
pub struct Trade {
    pub entry_ts: i64,
    pub exit_ts: i64,
    /// SOL spent (size + transaction cost)
    pub cost: i64,
    /// SOL received after fees and transaction cost
    pub proceeds: i64,
    pub why: Why,
}

impl Trade {
    pub fn pnl_sol(&self) -> f64 {
        (self.proceeds - self.cost) as f64 / LAMPORTS
    }
    pub fn ret(&self) -> f64 {
        (self.proceeds - self.cost) as f64 / self.cost.max(1) as f64
    }
}

/// Buy at `t` s after creation (landing `delay` later) and sell by `exit`.
pub fn simulate(c: &Coin, t: i64, exit: &Exit, k: &Costs) -> Option<Trade> {
    let end = c.created_ts + WINDOW_SECS;
    let entry_ts = c.created_ts + t + k.delay_s;
    if entry_ts >= end || c.graduated_ts.is_some_and(|g| g <= entry_ts) {
        return None;
    }
    let at_entry = c.at(entry_ts);
    let tokens = buy_tokens_for_quote(&at_entry.state(), k.size_lamports, at_entry.fee_bps);
    if tokens == 0 {
        return None;
    }
    let input = curve_input(k.size_lamports, at_entry.fee_bps);
    let value_at =
        |x: &Tick| sell_quote_for_tokens(&with_us(x.state(), input, tokens), tokens, x.fee_bps);
    let cost = (k.size_lamports + k.tx_lamports) as i64;
    let size = k.size_lamports as f64;
    let deadline = (entry_ts + exit.max_hold).min(end);
    let first = c.ticks.partition_point(|x| x.ts <= entry_ts);
    let mut peak = 0f64;
    let mut trigger: Option<(i64, Why)> = None;
    for x in &c.ticks[first..] {
        if x.ts > deadline {
            break;
        }
        let v = value_at(x) as f64;
        peak = peak.max(v);
        let r = v / size - 1.0;
        let why = if exit.tp.is_some_and(|tp| r >= tp) {
            Some(Why::TakeProfit)
        } else if exit.sl.is_some_and(|sl| r <= -sl) {
            Some(Why::StopLoss)
        } else if exit.trail.is_some_and(|tr| v <= peak * (1.0 - tr)) {
            Some(Why::Trail)
        } else {
            None
        };
        if let Some(w) = why {
            trigger = Some((x.ts, w));
            break;
        }
    }
    let (when, mut why) = trigger.unwrap_or(if deadline >= end {
        (end, Why::TapeEnd)
    } else {
        (deadline, Why::Time)
    });
    let fill_ts = (when + k.delay_s).min(end);
    let (exit_ts, value) = match c.graduated_ts.filter(|g| *g <= fill_ts) {
        Some(g) => {
            why = Why::Graduated;
            let pool = pool_after_graduation(with_us(c.at(g).state(), input, tokens));
            (g, sell_quote_for_tokens(&pool, tokens, AMM_FEE_BPS))
        }
        None => (fill_ts, value_at(&c.at(fill_ts))),
    };
    Some(Trade {
        entry_ts,
        exit_ts,
        cost,
        proceeds: value as i64 - k.tx_lamports as i64,
        why,
    })
}

// ------------------------------------------------------------------ rules

/// Which coins to buy, from their book at moment `t`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub t: i64,
    pub min_holders: u64,
    pub max_hhi: f64,
    /// only if the dev has not sold
    pub dev_kept: bool,
    pub max_snipers_pct: f64,
    pub max_progress: f64,
}

impl Entry {
    pub fn passes(&self, f: &Feat) -> bool {
        f.gap_ms == 0
            && f.holders >= self.min_holders
            && f.hhi <= self.max_hhi
            && (!self.dev_kept || !f.dev_sold)
            && f.snipers_pct <= self.max_snipers_pct
            && f.progress <= self.max_progress
    }
    pub fn label(&self) -> String {
        let mut v = vec![format!("@{}s holders ≥ {}", self.t, self.min_holders)];
        if self.max_hhi < 1.0 {
            v.push(format!("HHI ≤ {}", self.max_hhi));
        }
        if self.dev_kept {
            v.push("dev kept".into());
        }
        if self.max_snipers_pct < 100.0 {
            v.push(format!("snipers ≤ {}%", self.max_snipers_pct));
        }
        if self.max_progress < 1.0 {
            v.push(format!("curve ≤ {:.0}%", self.max_progress * 100.0));
        }
        v.join(" · ")
    }
}

/// The entry rules searched (hypotheses from the studies: breadth, concentration, dev
/// and sniper behaviour, how far along the curve is).
pub fn entry_grid() -> Vec<Entry> {
    let mut v = vec![];
    for t in DECIDE_AT {
        for min_holders in [5, 10, 20, 30, 50] {
            for max_hhi in [1.0, 0.5, 0.3, 0.15] {
                for dev_kept in [false, true] {
                    for max_snipers_pct in [100.0, 10.0] {
                        for max_progress in [1.0, 0.5] {
                            v.push(Entry {
                                t,
                                min_holders,
                                max_hhi,
                                dev_kept,
                                max_snipers_pct,
                                max_progress,
                            });
                        }
                    }
                }
            }
        }
    }
    v
}

/// The exit policies searched.
pub fn exit_grid() -> Vec<Exit> {
    let mut v = vec![];
    for tp in [Some(0.3), Some(0.6), Some(1.0), Some(2.0), None] {
        for sl in [Some(0.15), Some(0.3), None] {
            for trail in [Some(0.25), None] {
                for max_hold in [60, 180, 600] {
                    v.push(Exit {
                        tp,
                        sl,
                        trail,
                        max_hold,
                    });
                }
            }
        }
    }
    v
}

/// Every coin's trade under every exit, for each decision moment (`None`: no trade).
pub struct Book {
    /// [coin][moment index][exit index]
    trades: Vec<Vec<Vec<Option<Trade>>>>,
}

/// The loosest entry any rule in the grid uses: other coins are never simulated.
fn may_enter(c: &Coin, t: i64) -> bool {
    !c.quoted
        && c.feats
            .get(&t)
            .is_some_and(|f| f.gap_ms == 0 && f.holders >= 5)
}

pub fn book(coins: &[Coin], exits: &[Exit], k: &Costs) -> Book {
    let trades = coins
        .iter()
        .map(|c| {
            DECIDE_AT
                .iter()
                .map(|t| {
                    if may_enter(c, *t) {
                        exits.iter().map(|e| simulate(c, *t, e, k)).collect()
                    } else {
                        vec![]
                    }
                })
                .collect()
        })
        .collect();
    Book { trades }
}

/// Mean, sample standard deviation and count of returns.
#[derive(Clone, Copy, Debug, Default)]
pub struct Score {
    pub n: usize,
    pub wins: usize,
    pub sum_ret: f64,
    pub sum_ret2: f64,
    pub pnl: f64,
    pub gross_win: f64,
    pub gross_loss: f64,
}

impl Score {
    fn add(&mut self, t: &Trade) {
        self.add_ret(t.ret(), t.pnl_sol());
    }
    /// One trade by its return and its SOL result.
    pub(crate) fn add_ret(&mut self, r: f64, p: f64) {
        self.n += 1;
        self.wins += (r > 0.0) as usize;
        self.sum_ret += r;
        self.sum_ret2 += r * r;
        self.pnl += p;
        if p > 0.0 {
            self.gross_win += p;
        } else {
            self.gross_loss -= p;
        }
    }
    pub fn mean(&self) -> f64 {
        self.sum_ret / self.n.max(1) as f64
    }
    pub fn sd(&self) -> f64 {
        if self.n < 2 {
            return f64::NAN;
        }
        let m = self.mean();
        ((self.sum_ret2 - self.n as f64 * m * m) / (self.n - 1) as f64)
            .max(0.0)
            .sqrt()
    }
    /// One-sided 95% lower bound of the mean return per trade.
    pub fn lower(&self) -> f64 {
        self.mean() - 1.645 * self.sd() / (self.n as f64).sqrt()
    }
    pub fn pf(&self) -> f64 {
        if self.gross_loss > 0.0 {
            self.gross_win / self.gross_loss
        } else {
            f64::INFINITY
        }
    }
}

fn t_index(t: i64) -> usize {
    DECIDE_AT
        .iter()
        .position(|x| *x == t)
        .expect("decision moment")
}

/// The trades rule `e` × exit `x` makes on the coins in `idx`.
fn trades_of<'a>(
    coins: &'a [Coin],
    book: &'a Book,
    idx: &'a [usize],
    e: &'a Entry,
    x: usize,
) -> impl Iterator<Item = (usize, Trade)> + 'a {
    let ti = t_index(e.t);
    idx.iter().filter_map(move |&i| {
        let f = coins[i].feats.get(&e.t)?;
        if !e.passes(f) {
            return None;
        }
        book.trades[i]
            .get(ti)?
            .get(x)
            .copied()
            .flatten()
            .map(|t| (i, t))
    })
}

fn score(coins: &[Coin], book: &Book, idx: &[usize], e: &Entry, x: usize) -> Score {
    let mut s = Score::default();
    for (_, t) in trades_of(coins, book, idx, e, x) {
        s.add(&t);
    }
    s
}

/// The pair with the best lower bound on `idx` (at least `MIN_TRADES` trades).
fn best_pair(
    coins: &[Coin],
    book: &Book,
    idx: &[usize],
    entries: &[Entry],
    exits: &[Exit],
) -> Option<(usize, usize, Score)> {
    let mut best: Option<(usize, usize, Score)> = None;
    for (ei, e) in entries.iter().enumerate() {
        for xi in 0..exits.len() {
            let s = score(coins, book, idx, e, xi);
            if s.n < MIN_TRADES {
                continue;
            }
            if best.as_ref().is_none_or(|b| s.lower() > b.2.lower()) {
                best = Some((ei, xi, s));
            }
        }
    }
    best
}

// ------------------------------------------------------------------ the report

pub struct ReplayArgs {
    pub dir: PathBuf,
    pub from: Option<String>,
    pub to: Option<String>,
    pub size_sol: f64,
    pub delay_s: f64,
    pub tx_cost_sol: f64,
    pub block_hours: i64,
    /// newest share of the coins kept out of everything until `final_run`
    pub holdout: f64,
    pub final_run: bool,
    pub bankroll_sol: f64,
    pub bet_share: f64,
    pub max_open: usize,
}

/// One walk-forward block: the pair picked on the blocks before it and what it did here.
struct Block {
    start: i64,
    coins: usize,
    pick: Option<(usize, usize, Score)>,
    result: Score,
    holdout: bool,
}

/// Bankroll run over trades in time order: bet `share` of the bankroll per trade, at
/// most `max_open` at once; returns (final bankroll, deepest drawdown, trades taken).
pub fn bankroll(
    trades: &[(i64, i64, f64)],
    start: f64,
    share: f64,
    max_open: usize,
) -> (f64, f64, usize) {
    let mut cash = start;
    let mut open: Vec<(i64, f64, f64)> = vec![]; // (exit ts, stake, return)
    let (mut peak, mut dd, mut taken) = (start, 0f64, 0usize);
    let mut sorted: Vec<&(i64, i64, f64)> = trades.iter().collect();
    sorted.sort_by_key(|t| t.0);
    let settle = |cash: &mut f64,
                  open: &mut Vec<(i64, f64, f64)>,
                  until: i64,
                  peak: &mut f64,
                  dd: &mut f64| {
        open.sort_by_key(|o| o.0);
        while open.first().is_some_and(|o| o.0 <= until) {
            let (_, stake, r) = open.remove(0);
            *cash += stake * (1.0 + r);
            let equity = *cash + open.iter().map(|o| o.1).sum::<f64>();
            *peak = peak.max(equity);
            *dd = dd.max(1.0 - equity / *peak);
        }
    };
    for &&(entry, exit, r) in &sorted {
        settle(&mut cash, &mut open, entry, &mut peak, &mut dd);
        let equity = cash + open.iter().map(|o| o.1).sum::<f64>();
        let stake = equity * share;
        if open.len() >= max_open || stake > cash || stake <= 0.0 {
            continue;
        }
        cash -= stake;
        open.push((exit, stake, r));
        taken += 1;
    }
    settle(&mut cash, &mut open, i64::MAX, &mut peak, &mut dd);
    (cash, dd, taken)
}

/// Per-day figures need at least this many hours behind them.
const MIN_HOURS_FOR_DAILY: f64 = 6.0;

/// SOL per day at the pace of `hours`, or why it is not stated.
pub(crate) fn per_day(sol: f64, hours: f64) -> String {
    if hours < MIN_HOURS_FOR_DAILY {
        format!("n/a (only {hours:.1} h; needs {MIN_HOURS_FOR_DAILY:.0})")
    } else {
        format!("{:+.2}", sol / hours * 24.0)
    }
}

/// Hours the recorder was running, judged from the creation times of the coins it saw
/// (launches come every few seconds; a silence over 10 minutes means it was down).
pub fn recorded_hours(mut created: Vec<i64>) -> f64 {
    created.sort_unstable();
    let secs: i64 = created
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g <= 600)
        .sum();
    (secs as f64 / 3600.0).max(1.0 / 60.0)
}

pub(crate) fn pct(x: f64) -> String {
    if x.is_finite() {
        format!("{:+.1}%", x * 100.0)
    } else {
        "-".into()
    }
}

pub(crate) fn hm(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

pub fn run(a: &ReplayArgs) -> anyhow::Result<String> {
    let coins = load(&a.dir, a.from.as_deref(), a.to.as_deref())?;
    Ok(run_on(coins, a))
}

/// The report on coins already loaded.
pub fn run_on(coins: Vec<Coin>, a: &ReplayArgs) -> String {
    let k = Costs {
        size_lamports: (a.size_sol * LAMPORTS) as u64,
        delay_s: a.delay_s.ceil() as i64,
        tx_lamports: (a.tx_cost_sol * LAMPORTS) as u64,
    };
    let entries = entry_grid();
    let exits = exit_grid();
    let mut o = String::new();
    let _ = writeln!(o, "# Replay of the trade tape\n");
    if coins.is_empty() {
        let _ = writeln!(
            o,
            "No coins with a recorded create, trades and book under {} yet.",
            a.dir.display()
        );
        return o;
    }
    // the newest coins stay locked until --final
    let lock_from = coins[((coins.len() as f64) * (1.0 - a.holdout)).floor() as usize..]
        .first()
        .map(|c| c.created_ts)
        .unwrap_or(i64::MAX);
    let usable: Vec<usize> = (0..coins.len())
        .filter(|&i| a.final_run || coins[i].created_ts < lock_from)
        .collect();
    let first_ts = coins[usable[0]].created_ts;
    let last_ts = coins[*usable.last().unwrap_or(&0)].created_ts;
    let hours = recorded_hours(usable.iter().map(|&i| coins[i].created_ts).collect());
    let ticks: usize = usable.iter().map(|&i| coins[i].ticks.len()).sum();
    let _ = writeln!(
        o,
        "- data: **{}** coins created {} → {} UTC, **{:.1} h of recording**, {} trades on tape",
        usable.len(),
        hm(first_ts),
        hm(last_ts),
        hours,
        ticks
    );
    let locked = coins.len() - usable.len();
    let _ = writeln!(
        o,
        "- holdout: {}",
        if a.final_run {
            format!("**included** (`--final`): the newest {:.0}% of coins are scored below as the final, one-time test", a.holdout * 100.0)
        } else {
            format!("{locked} newest coins (from {} UTC) locked away; nothing below has seen them. Run with `--final` once, after the rules stop changing", hm(lock_from))
        }
    );
    let _ = writeln!(
        o,
        "- costs: {} SOL per trade, our transaction lands {} s after the decision and after each exit trigger, curve fee as recorded (1.25% when missing), {} SOL network/priority fee per transaction",
        a.size_sol, k.delay_s, a.tx_cost_sol
    );
    let _ = writeln!(
        o,
        "- searched: {} entry rules × {} exits = **{} pairs**. The best in-sample pair is optimistic by construction; only the walk-forward and holdout numbers count.",
        entries.len(),
        exits.len(),
        entries.len() * exits.len()
    );
    let bk = book(&coins, &exits, &k);

    // in sample, for orientation
    let in_sample = best_pairs(&coins, &bk, &usable, &entries, &exits, 10);
    let _ = writeln!(
        o,
        "\n## Best pairs on all unlocked data (in sample, optimistic)"
    );
    let _ = writeln!(
        o,
        "| entry | exit | trades | win | mean | 95% low | profit factor | SOL at {} |",
        a.size_sol
    );
    let _ = writeln!(o, "|---|---|---:|---:|---:|---:|---:|---:|");
    for (ei, xi, s) in &in_sample {
        let _ = writeln!(
            o,
            "| {} | {} | {} | {:.0}% | {} | {} | {:.2} | {:+.3} |",
            entries[*ei].label(),
            exits[*xi].label(),
            s.n,
            100.0 * s.wins as f64 / s.n.max(1) as f64,
            pct(s.mean()),
            pct(s.lower()),
            s.pf(),
            s.pnl
        );
    }
    if in_sample.is_empty() {
        let _ = writeln!(
            o,
            "| _no pair has {MIN_TRADES}+ trades yet_ | | | | | | | |"
        );
    } else if in_sample[0].2.lower() <= 0.0 {
        let _ = writeln!(
            o,
            "\n_Not one of the {} pairs is profitable at 95% confidence even on the data it was picked on._",
            entries.len() * exits.len()
        );
    }

    // walk-forward
    let block_s = a.block_hours.max(1) * 3600;
    let mut blocks: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    let all_idx: Vec<usize> = (0..coins.len())
        .filter(|&i| a.final_run || coins[i].created_ts < lock_from)
        .collect();
    for &i in &all_idx {
        blocks
            .entry((coins[i].created_ts - first_ts).div_euclid(block_s))
            .or_default()
            .push(i);
    }
    let mut seen: Vec<usize> = vec![];
    let mut oos_created: Vec<i64> = vec![];
    let mut wf: Vec<Block> = vec![];
    let mut oos: Vec<(i64, i64, f64)> = vec![];
    let mut oos_score = Score::default();
    let mut hold_score = Score::default();
    let mut reasons: BTreeMap<&'static str, (usize, f64)> = BTreeMap::new();
    let mut last_pick: Option<(usize, usize)> = None;
    for (b, idx) in &blocks {
        let pick = if seen.is_empty() {
            None
        } else {
            best_pair(&coins, &bk, &seen, &entries, &exits)
        };
        let mut result = Score::default();
        let holdout = coins[idx[0]].created_ts >= lock_from;
        if !seen.is_empty() {
            oos_created.extend(idx.iter().map(|&i| coins[i].created_ts));
        }
        if let Some((ei, xi, _)) = &pick {
            for (_, t) in trades_of(&coins, &bk, idx, &entries[*ei], *xi) {
                result.add(&t);
                oos_score.add(&t);
                if holdout {
                    hold_score.add(&t);
                }
                oos.push((t.entry_ts, t.exit_ts, t.ret()));
                let r = reasons.entry(t.why.label()).or_default();
                r.0 += 1;
                r.1 += t.pnl_sol();
            }
            last_pick = Some((*ei, *xi));
        }
        wf.push(Block {
            start: first_ts + b * block_s,
            coins: idx.len(),
            pick,
            result,
            holdout,
        });
        seen.extend(idx);
    }
    let _ = writeln!(
        o,
        "\n## Walk-forward ({} h blocks): the pair picked on all earlier blocks, traded on the next one",
        a.block_hours
    );
    let _ = writeln!(o, "| block (UTC) | coins | pair picked on the blocks before | its trades here | win | mean | SOL |");
    let _ = writeln!(o, "|---|---:|---|---:|---:|---:|---:|");
    for b in &wf {
        let pick = match &b.pick {
            Some((ei, xi, _)) => format!("{} → {}", entries[*ei].label(), exits[*xi].label()),
            None => "_nothing qualifies yet_".into(),
        };
        let _ = writeln!(
            o,
            "| {}{} | {} | {} | {} | {} | {} | {:+.3} |",
            hm(b.start),
            if b.holdout { " (holdout)" } else { "" },
            b.coins,
            pick,
            b.result.n,
            if b.result.n > 0 {
                format!("{:.0}%", 100.0 * b.result.wins as f64 / b.result.n as f64)
            } else {
                "-".into()
            },
            if b.result.n > 0 {
                pct(b.result.mean())
            } else {
                "-".into()
            },
            b.result.pnl
        );
    }
    let oos_hours = recorded_hours(oos_created);
    let _ = writeln!(
        o,
        "\n**Out of sample: {} trades · win {:.0}% · mean {} per trade (95% low {}) · profit factor {:.2} · {:+.3} SOL at {} SOL a trade · SOL/day at this pace: {}**",
        oos_score.n,
        100.0 * oos_score.wins as f64 / oos_score.n.max(1) as f64,
        pct(oos_score.mean()),
        pct(oos_score.lower()),
        oos_score.pf(),
        oos_score.pnl,
        a.size_sol,
        per_day(oos_score.pnl, oos_hours)
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
    if a.final_run && hold_score.n > 0 {
        let _ = writeln!(
            o,
            "\n**Holdout (final test): {} trades · mean {} (95% low {}) · {:+.3} SOL**",
            hold_score.n,
            pct(hold_score.mean()),
            pct(hold_score.lower()),
            hold_score.pnl
        );
    }
    if oos_score.n < MIN_TRADES {
        let _ = writeln!(o, "_Too few out-of-sample trades to conclude anything; the recorder needs to run longer._");
    }

    // bankroll from the walk-forward trades
    if !oos.is_empty() {
        let (end, dd, taken) = bankroll(&oos, a.bankroll_sol, a.bet_share, a.max_open);
        let _ = writeln!(
            o,
            "\n## Bankroll: {} SOL, {:.0}% of it per trade, at most {} open (walk-forward trades)\n- ends at **{:.3} SOL** after {} trades ({} skipped while capital was busy) · deepest drawdown {:.0}% · {:.1} h of trading",
            a.bankroll_sol,
            a.bet_share * 100.0,
            a.max_open,
            end,
            taken,
            oos.len() - taken,
            dd * 100.0,
            oos_hours
        );
    }

    // how the last pick depends on size and delay
    if let Some((ei, xi)) = last_pick {
        let e = &entries[ei];
        let x = exits[xi];
        let _ = writeln!(
            o,
            "\n## Size and delay for the latest pick ({} → {}), on all unlocked data",
            e.label(),
            x.label()
        );
        let _ = writeln!(o, "| size SOL | trades | mean | SOL per trade | SOL/day |");
        let _ = writeln!(o, "|---:|---:|---:|---:|---:|");
        for size in [0.1, 0.25, 0.5, 1.0, 2.0, 5.0] {
            let kk = Costs {
                size_lamports: (size * LAMPORTS) as u64,
                ..k
            };
            let s = rule_score(&coins, &usable, e, &x, &kk);
            let _ = writeln!(
                o,
                "| {size} | {} | {} | {:+.4} | {} |",
                s.n,
                pct(s.mean()),
                s.pnl / s.n.max(1) as f64,
                per_day(s.pnl, hours)
            );
        }
        let _ = writeln!(o, "\n| delay s | trades | mean |");
        let _ = writeln!(o, "|---:|---:|---:|");
        for d in [1, 2, 4, 8] {
            let kk = Costs { delay_s: d, ..k };
            let s = rule_score(&coins, &usable, e, &x, &kk);
            let _ = writeln!(o, "| {d} | {} | {} |", s.n, pct(s.mean()));
        }
    }
    o
}

/// One rule and exit simulated afresh (for other sizes and delays).
fn rule_score(coins: &[Coin], idx: &[usize], e: &Entry, x: &Exit, k: &Costs) -> Score {
    let mut s = Score::default();
    for &i in idx {
        let c = &coins[i];
        if c.quoted || !c.feats.get(&e.t).is_some_and(|f| e.passes(f)) {
            continue;
        }
        if let Some(t) = simulate(c, e.t, x, k) {
            s.add(&t);
        }
    }
    s
}

/// The `n` best pairs by lower bound on `idx`.
fn best_pairs(
    coins: &[Coin],
    book: &Book,
    idx: &[usize],
    entries: &[Entry],
    exits: &[Exit],
    n: usize,
) -> Vec<(usize, usize, Score)> {
    let mut v = vec![];
    for (ei, e) in entries.iter().enumerate() {
        for xi in 0..exits.len() {
            let s = score(coins, book, idx, e, xi);
            if s.n >= MIN_TRADES {
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

/// Write the report next to the tape and return it.
pub fn write(a: &ReplayArgs) -> anyhow::Result<String> {
    let text = run(a)?;
    let d = a.dir.join("replay");
    std::fs::create_dir_all(&d)?;
    let name = chrono::Utc::now().format("%Y-%m-%dT%H%M%S").to_string();
    std::fs::write(d.join(format!("{name}.md")), &text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_791_500_000;

    /// A coin whose curve sits at `vsol` SOL of virtual reserves at each listed second.
    fn coin(path: &[(i64, f64)], graduated: Option<i64>) -> Coin {
        let mut c = Coin {
            mint: "M".into(),
            created_ts: T0,
            graduated_ts: graduated.map(|g| T0 + g),
            ..Default::default()
        };
        for (i, (s, vsol)) in path.iter().enumerate() {
            let vsol = (vsol * LAMPORTS) as u64;
            let vtok = (START_VSOL as u128 * START_VTOK as u128 / vsol as u128) as u64;
            c.ticks.push(Tick {
                ts: T0 + s,
                slot: i as u64 + 1,
                seq: i as u64,
                vsol,
                vtok,
                rsol: vsol - START_VSOL,
                rtok: vtok - VIRTUAL_TOKEN_OFFSET,
                fee_bps: 125,
            });
        }
        c.feats.insert(
            15,
            Feat {
                holders: 40,
                hhi: 0.1,
                ..Default::default()
            },
        );
        c
    }

    fn k(delay: i64) -> Costs {
        Costs {
            size_lamports: 500_000_000,
            delay_s: delay,
            tx_lamports: 1_000_000,
        }
    }

    #[test]
    fn a_buy_fills_after_the_delay_and_a_take_profit_sells_after_it_too() {
        // the curve doubles in SOL (price x4) between 20 and 40 s, then falls back
        let c = coin(
            &[
                (10, 32.0),
                (17, 34.0),
                (19, 36.0),
                (25, 50.0),
                (40, 64.0),
                (44, 40.0),
            ],
            None,
        );
        let x = Exit {
            tp: Some(1.0),
            sl: None,
            trail: None,
            max_hold: 600,
        };
        let t = simulate(&c, 15, &x, &k(4)).unwrap();
        assert_eq!(t.entry_ts, T0 + 19, "decision at 15 s + 4 s delay");
        // bought on the curve as it was at 19 s (36 SOL virtual), exact formula
        let buy_state = c.at(T0 + 19);
        assert_eq!(buy_state.vsol, 36_000_000_000);
        let tokens = buy_tokens_for_quote(&buy_state.state(), 500_000_000, 125);
        let input = curve_input(500_000_000, 125);
        // +100% first reached at 40 s (64 SOL); the sell lands at 44 s, when it fell back
        assert_eq!(t.why, Why::TakeProfit);
        assert_eq!(t.exit_ts, T0 + 44);
        let sold =
            sell_quote_for_tokens(&with_us(c.at(T0 + 44).state(), input, tokens), tokens, 125)
                as i64;
        assert_eq!(t.proceeds, sold - 1_000_000);
        assert_eq!(t.cost, 501_000_000);
        assert!(t.ret() > 0.1 && t.ret() < 0.25, "{}", t.ret());
        // with no delay the sell meets the top
        let fast = simulate(&c, 15, &x, &k(0)).unwrap();
        assert!(fast.ret() > t.ret());
    }

    #[test]
    fn stops_time_limits_and_graduation_close_the_trade() {
        let x = Exit {
            tp: None,
            sl: Some(0.3),
            trail: None,
            max_hold: 600,
        };
        let falling = coin(&[(5, 40.0), (30, 35.0), (60, 30.5), (90, 30.1)], None);
        let t = simulate(&falling, 15, &x, &k(2)).unwrap();
        assert_eq!(t.why, Why::StopLoss);
        assert!(t.ret() < -0.3);
        // time limit with nothing happening: sold at the deadline + delay
        let flat = coin(&[(5, 40.0), (16, 40.0)], None);
        let t = simulate(
            &flat,
            15,
            &Exit {
                tp: None,
                sl: None,
                trail: None,
                max_hold: 60,
            },
            &k(2),
        )
        .unwrap();
        assert_eq!(t.why, Why::Time);
        assert_eq!(t.exit_ts, T0 + 17 + 60 + 2);
        // round trip on a flat curve costs the two fees and two transaction fees
        assert!(t.ret() < -0.02 && t.ret() > -0.04, "{}", t.ret());
        // a coin that graduates while held is sold at its final price on the AMM
        let up = coin(&[(5, 40.0), (100, 90.0), (200, 115.0)], Some(200));
        let t = simulate(
            &up,
            15,
            &Exit {
                tp: Some(5.0),
                sl: None,
                trail: None,
                max_hold: 600,
            },
            &k(2),
        )
        .unwrap();
        assert_eq!(t.why, Why::Graduated);
        assert_eq!(t.exit_ts, T0 + 200);
        assert!(t.ret() > 3.0, "{}", t.ret());
        // already graduated at the decision: no trade
        assert!(simulate(&coin(&[(5, 115.0)], Some(10)), 15, &x, &k(2)).is_none());
    }

    #[test]
    fn walk_forward_picks_on_earlier_blocks_only_and_the_holdout_stays_locked() {
        // 3 blocks of 80 coins; coins with 40 holders double, coins with 6 die
        let mut coins = vec![];
        for b in 0..3i64 {
            for j in 0..80i64 {
                let strong = j % 2 == 0;
                let mut c = coin(
                    if strong {
                        &[(5, 35.0), (18, 36.0), (60, 60.0), (200, 60.0)]
                    } else {
                        &[(5, 35.0), (18, 34.0), (60, 30.2), (200, 30.1)]
                    },
                    None,
                );
                c.created_ts = T0 + b * 6 * 3600 + j * 60;
                for t in c.ticks.iter_mut() {
                    t.ts += b * 6 * 3600 + j * 60;
                }
                c.feats.insert(
                    15,
                    Feat {
                        holders: if strong { 40 } else { 6 },
                        hhi: 0.1,
                        ..Default::default()
                    },
                );
                coins.push(c);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let a = ReplayArgs {
            dir: dir.path().into(),
            from: None,
            to: None,
            size_sol: 0.5,
            delay_s: 2.0,
            tx_cost_sol: 0.001,
            block_hours: 6,
            holdout: 0.34,
            final_run: false,
            bankroll_sol: 1.0,
            bet_share: 0.1,
            max_open: 10,
        };
        let text = run_on(coins, &a);
        assert!(text.contains("82 newest coins"), "{text}");
        // block 0 has nothing to learn from; block 1 trades the pick from block 0
        assert!(text.contains("_nothing qualifies yet_"), "{text}");
        let wf = text.split("## Walk-forward").nth(1).unwrap();
        let row1 = wf
            .lines()
            .filter(|l| l.starts_with("| 10-"))
            .nth(1)
            .unwrap();
        assert!(
            row1.contains("holders ≥ 10")
                || row1.contains("holders ≥ 20")
                || row1.contains("holders ≥ 30"),
            "{row1}"
        );
        assert!(
            row1.contains("| 78 | ") && row1.contains("| 39 |"),
            "39 strong coins of the 78 unlocked traded in block 1: {row1}"
        );
        assert!(!wf.contains("(holdout)"), "locked: {wf}");
        assert!(
            text.contains("**Out of sample: 39 trades · win 100%"),
            "{text}"
        );
    }

    #[test]
    fn bankroll_bets_a_share_and_respects_capital() {
        // three overlapping +50% trades and one -100%: 50% bet size, at most 2 open
        let trades = vec![(0, 10, 0.5), (1, 11, 0.5), (2, 12, 0.5), (20, 30, -1.0)];
        let (end, dd, taken) = bankroll(&trades, 1.0, 0.5, 2);
        assert_eq!(taken, 3, "the third overlapping trade had no free capital");
        // 1.0 → two stakes of 0.5 (second is 50% of equity 1.0 = 0.5) → 0.75 each back = 1.5;
        // then lose half of 1.5
        assert!((end - 0.75).abs() < 1e-9, "{end}");
        assert!((dd - 0.5).abs() < 1e-9, "{dd}");
    }
}
