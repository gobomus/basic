//! The trade tape as a model: every Pump.fun trade of each new coin's first hour, the
//! curve as it was after each one, and our own position in it.
//!
//! * a signal is the first trade after which its conditions hold (SOL in the curve,
//!   holders, buyers, known wallets), within a window of the create;
//! * our buy lands `delay` after that trade and fills on the curve as it was then,
//!   with the exact curve formula and the recorded fee, so our own size moves the
//!   price against us; our tokens stay in the curve until we sell;
//! * from there the position is valued on every later trade: at the end of the hour
//!   (a coin that graduates is sold into a pool as deep as the SOL in its curve, at the
//!   AMM fee) and at its peak on the way.
//!
//! No rule search lives here. `winners` reads the tape through this module and prints
//! what the coins do after each fixed signal.
//!
//! Model notes: trades in the same second as our entry are not valuation points;
//! block times have 1 s resolution, so the delay is rounded up to whole seconds. One
//! assumption in our favour: the book a rule reads was written 10 s after its moment,
//! so it includes trades a live engine would still be waiting for (the stream runs
//! 1-3 s behind).

use std::collections::HashMap;
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
    buy: bool,
    /// hash of the trading wallet
    user: u64,
    /// tokens traded (base units)
    tok: u64,
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
            buy: false,
            user: 0,
            tok: 0,
        }
    }
    /// Market cap in SOL (1e9 tokens of 1e6 units at the curve's spot price).
    fn mcap_sol(&self) -> f64 {
        self.vsol as f64 / self.vtok.max(1) as f64 * 1e6
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

/// One coin as the tape knows it.
#[derive(Default)]
pub struct Coin {
    pub mint: String,
    pub created_ts: i64,
    ticks: Vec<Tick>,
    pub graduated_ts: Option<i64>,
    /// the creator's wallet (hash)
    pub creator: u64,
    /// the pool's trades after graduation, in chain order
    pool: Vec<PoolTick>,
    /// priced in another token (amounts are not SOL)
    pub quoted: bool,
    /// moments (s after creation) with a complete book on the tape
    pub books: Vec<i64>,
}

/// A live signal: the first trade after which every condition holds, within `within_s`
/// of the create. Net SOL is the SOL in the curve (buys minus sells, fees aside);
/// holders are wallets with a positive balance; buyers are distinct buying wallets.
#[derive(Clone, Debug, PartialEq)]
pub struct Signal {
    pub min_net_sol: f64,
    pub min_holders: usize,
    pub min_buyers: usize,
    pub within_s: i64,
    /// wallets whose buys count toward `min_known` (hashes, see `wallet_hash`)
    pub known: std::collections::HashSet<u64>,
    pub min_known: usize,
    /// what the known wallets are called in the report
    pub known_label: String,
}

impl Signal {
    pub fn simple(min_net_sol: f64, min_holders: usize, min_buyers: usize, within_s: i64) -> Self {
        Self {
            min_net_sol,
            min_holders,
            min_buyers,
            within_s,
            known: Default::default(),
            min_known: 0,
            known_label: String::new(),
        }
    }
    /// At least `min_known` of the `known` wallets have bought within `within_s`.
    pub fn wallets(
        known: std::collections::HashSet<u64>,
        min_known: usize,
        within_s: i64,
        label: &str,
    ) -> Self {
        Self {
            known,
            min_known,
            known_label: label.to_string(),
            ..Self::simple(0.0, 0, 0, within_s)
        }
    }
    pub fn label(&self) -> String {
        let mut v = vec![];
        if self.min_net_sol > 0.0 {
            v.push(format!("net SOL ≥ {}", self.min_net_sol));
        }
        if self.min_holders > 0 {
            v.push(format!("holders ≥ {}", self.min_holders));
        }
        if self.min_buyers > 0 {
            v.push(format!("buyers ≥ {}", self.min_buyers));
        }
        if self.min_known > 0 {
            v.push(format!("{} ≥ {} bought", self.known_label, self.min_known));
        }
        format!("{} within {} s", v.join(" & "), self.within_s)
    }
}

impl Coin {
    /// Block time of the first trade after which `s` holds, if any within its window.
    pub fn signal_ts(&self, s: &Signal) -> Option<i64> {
        let mut bal: HashMap<u64, i128> = HashMap::new();
        let mut buyers: std::collections::HashSet<u64> = Default::default();
        let mut holders = 0usize;
        let mut known: std::collections::HashSet<u64> = Default::default();
        for x in &self.ticks {
            if x.ts - self.created_ts > s.within_s {
                return None;
            }
            if x.buy && s.min_known > 0 && s.known.contains(&x.user) {
                known.insert(x.user);
            }
            let b = bal.entry(x.user).or_insert(0);
            let before = *b > 0;
            *b += if x.buy {
                x.tok as i128
            } else {
                -(x.tok as i128)
            };
            let after = *b > 0;
            holders = (holders + after as usize).saturating_sub(before as usize);
            if x.buy {
                buyers.insert(x.user);
            }
            if x.rsol as f64 / LAMPORTS >= s.min_net_sol
                && holders >= s.min_holders
                && buyers.len() >= s.min_buyers
                && known.len() >= s.min_known
            {
                return Some(x.ts);
            }
        }
        None
    }
    /// Distinct wallets that bought within `within_s` of the create (hashes).
    pub fn early_buyers(&self, within_s: i64) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .ticks
            .iter()
            .take_while(|x| x.ts - self.created_ts <= within_s)
            .filter(|x| x.buy)
            .map(|x| x.user)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
    /// Market cap in SOL at block time `ts`, on the curve or, after graduation, in the pool.
    pub fn mcap_sol_at(&self, ts: i64) -> f64 {
        let i = self.pool.partition_point(|x| x.ts <= ts);
        if i > 0 && self.graduated_ts.is_some_and(|g| g <= ts) {
            return self.pool[i - 1].mcap_sol;
        }
        self.at(ts).mcap_sol()
    }
    /// The highest market cap in SOL on tape after block time `ts` (curve and pool).
    pub fn peak_mcap_sol_after(&self, ts: i64) -> f64 {
        let curve = self
            .ticks
            .iter()
            .filter(|x| x.ts > ts)
            .map(|x| x.mcap_sol())
            .fold(0.0, f64::max);
        let pool = self
            .pool
            .iter()
            .filter(|x| x.ts > ts)
            .map(|x| x.mcap_sol)
            .fold(0.0, f64::max);
        curve.max(pool)
    }
    /// Seconds after the create of the last trade on tape.
    pub fn last_trade_age(&self) -> i64 {
        self.ticks
            .last()
            .map(|x| x.ts)
            .into_iter()
            .chain(self.pool.last().map(|x| x.ts))
            .max()
            .map_or(0, |t| t - self.created_ts)
    }
    pub fn trades(&self) -> usize {
        self.ticks.len()
    }
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
    buy: Option<bool>,
    user: Option<String>,
    tok: Option<u64>,
    creator: Option<String>,
    sol: Option<u64>,
    base: Option<u64>,
    mcap_sol: Option<f64>,
    liq: Option<u64>,
}

/// One pool trade after graduation (the `ev: "amm"` row).
#[derive(Clone, Copy, Debug)]
pub struct PoolTick {
    ts: i64,
    slot: u64,
    seq: u64,
    buy: bool,
    user: u64,
    /// lamports the user paid or received
    sol: u64,
    /// tokens traded (base units)
    tok: u64,
    /// market cap in SOL after the trade
    mcap_sol: f64,
    /// the pool's SOL after the trade, lamports
    liq: u64,
    fee_bps: u64,
}

/// The tape keeps wallets as 64-bit hashes; the map from `load_with_wallets` names them.
pub fn wallet_hash(user: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    user.hash(&mut h);
    h.finish()
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

/// Every coin whose create, trades and a complete book are on the tape under `dir`,
/// created on a day in `[from, to]` (UTC, inclusive; `None` = no bound), plus the
/// wallets seen trading, by hash.
pub fn load_with_wallets(
    dir: &Path,
    from: Option<&str>,
    to: Option<&str>,
) -> anyhow::Result<(Vec<Coin>, HashMap<u64, String>)> {
    let mut coins: HashMap<String, Coin> = HashMap::new();
    let mut wallets: HashMap<u64, String> = HashMap::new();
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
                "create" => {
                    c.created_ts = row.ts;
                    if let Some(u) = row.creator.as_deref() {
                        c.creator = wallet_hash(u);
                        wallets.entry(c.creator).or_insert_with(|| u.to_string());
                    }
                }
                "amm" => {
                    let Some(mcap_sol) = row.mcap_sol.filter(|m| *m > 0.0) else {
                        continue;
                    };
                    // older tapes carry no pool SOL: a fresh pool holds about the curve's
                    // SOL and, at constant product, its SOL grows with the square root
                    // of the price
                    let liq = row.liq.unwrap_or_else(|| {
                        let landing = c.pool.first().map_or(mcap_sol, |p| p.mcap_sol);
                        (LANDING_POOL_LAMPORTS as f64 * (mcap_sol / landing.max(1e-9)).sqrt())
                            as u64
                    });
                    let user = row.user.as_deref().map(wallet_hash).unwrap_or(0);
                    if let Some(u) = row.user.as_deref() {
                        wallets.entry(user).or_insert_with(|| u.to_string());
                    }
                    c.pool.push(PoolTick {
                        ts: row.ts,
                        slot: row.slot,
                        seq,
                        buy: row.buy.unwrap_or(false),
                        user,
                        sol: row.sol.unwrap_or(0),
                        tok: row.base.unwrap_or(0),
                        mcap_sol,
                        liq,
                        fee_bps: row.fee_bps.unwrap_or(DEFAULT_POOL_FEE_BPS),
                    });
                }
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
                    let user = row.user.as_deref().map(wallet_hash).unwrap_or(0);
                    if let Some(u) = row.user.as_deref() {
                        wallets.entry(user).or_insert_with(|| u.to_string());
                    }
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
                        buy: row.buy.unwrap_or(false),
                        user,
                        tok: row.tok.unwrap_or(0),
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
            if r["gap_ms"].as_i64() != Some(0) {
                continue;
            }
            if let Some(c) = coins.get_mut(mint) {
                c.books.push(t);
            }
        }
    }
    let mut out: Vec<Coin> = coins
        .into_iter()
        .filter(|(_, c)| c.created_ts > 0 && !c.books.is_empty())
        .filter(|(_, c)| {
            let d = day_of_secs(c.created_ts);
            from.is_none_or(|f| d.as_str() >= f) && to.is_none_or(|t| d.as_str() <= t)
        })
        .map(|(mint, mut c)| {
            c.mint = mint;
            c.ticks.sort_by_key(|t| (t.slot, t.seq));
            c.pool.sort_by_key(|t| (t.slot, t.seq));
            c
        })
        .collect();
    out.sort_by_key(|c| c.created_ts);
    Ok((out, wallets))
}

// ------------------------------------------------------------------ our position

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

/// What a trade costs us beyond the curve.
#[derive(Clone, Copy, Debug)]
pub struct Costs {
    pub size_lamports: u64,
    /// seconds from the signal's trade to our transaction landing
    pub delay_s: i64,
    /// network fee + priority fee per transaction
    pub tx_lamports: u64,
}

/// Our position: `input` lamports in the curve, `tokens` out of it.
struct Position {
    input: u64,
    tokens: u64,
}

/// Buy with a fill at block time `entry_ts`; `None` after the window or after graduation.
fn open(c: &Coin, entry_ts: i64, k: &Costs) -> Option<Position> {
    let end = c.created_ts + WINDOW_SECS;
    if entry_ts >= end || c.graduated_ts.is_some_and(|g| g <= entry_ts) {
        return None;
    }
    let at_entry = c.at(entry_ts);
    let tokens = buy_tokens_for_quote(&at_entry.state(), k.size_lamports, at_entry.fee_bps);
    if tokens == 0 {
        return None;
    }
    Some(Position {
        input: curve_input(k.size_lamports, at_entry.fee_bps),
        tokens,
    })
}

impl Position {
    /// What selling the whole position into the curve as it is at `x` would return.
    fn value(&self, x: &Tick) -> u64 {
        sell_quote_for_tokens(
            &with_us(x.state(), self.input, self.tokens),
            self.tokens,
            x.fee_bps,
        )
    }
}

/// Wallets that bought within this many seconds of the create are "the bundle".
const BUNDLE_S: i64 = 5;
/// A Pump.fun coin's supply in base units (1e9 tokens of 6 decimals).
const SUPPLY_BASE_UNITS: f64 = 1e15;
/// The flow window for money leaving and new buyers.
const FLOW_S: i64 = 60;
/// Pool fee when the row does not carry it.
const DEFAULT_POOL_FEE_BPS: u64 = 25;
/// SOL a fresh PumpSwap pool holds when the row does not say (the curve's real SOL at
/// graduation, less the migration fee).
const LANDING_POOL_LAMPORTS: u64 = 84_000_000_000;

/// The live state of a coin as the tape unfolds: what an engine holding it would see.
#[derive(Clone, Debug, Default)]
struct State {
    bal: HashMap<u64, i128>,
    holders: usize,
    holders_peak: usize,
    buyers: std::collections::HashSet<u64>,
    /// the bundle's tokens: at their peak, and now
    bundle: std::collections::HashSet<u64>,
    bundle_peak: i128,
    bundle_now: i128,
    dev_peak: i128,
    dev_now: i128,
    /// (ts, buy, lamports, first buy of a new wallet) in the last `FLOW_S`
    window: std::collections::VecDeque<(i64, bool, u64, bool)>,
    buy_w: u64,
    sell_w: u64,
    new_buyers_w: usize,
    last_new_buyer_ts: i64,
    /// SOL in the curve, then the pool's SOL after graduation (lamports)
    net_sol: u64,
    net_sol_peak: u64,
    mcap_sol: f64,
    graduated: bool,
}

/// The state's readings at one moment, for the fingerprints.
#[derive(Clone, Copy, Debug, Default)]
pub struct Snap {
    /// seconds after the create
    pub age: i64,
    /// sells over buys in SOL over the last minute (sells with no buys: 99)
    pub flow_ratio: f64,
    /// what the bundle still holds of its peak (1 = all, 0 = sold out)
    pub bundle_left: f64,
    /// what the creator still holds of their peak; 1 when they never bought
    pub dev_left: f64,
    /// holders over their peak
    pub holders_vs_peak: f64,
    pub new_buyers_w: usize,
    /// seconds since the last new buyer
    pub since_new_buyer: i64,
    /// SOL in the curve or pool over its peak
    pub net_sol_vs_peak: f64,
    pub holders: usize,
}

impl State {
    fn apply(&mut self, c: &Coin, ts: i64, buy: bool, user: u64, sol: u64, tok: u64) {
        let b = self.bal.entry(user).or_insert(0);
        let before = *b;
        *b += if buy { tok as i128 } else { -(tok as i128) };
        let after = *b;
        self.holders = (self.holders + (after > 0) as usize).saturating_sub((before > 0) as usize);
        self.holders_peak = self.holders_peak.max(self.holders);
        let mut fresh = false;
        if buy {
            fresh = self.buyers.insert(user);
            if ts - c.created_ts <= BUNDLE_S {
                self.bundle.insert(user);
            }
            if fresh {
                self.last_new_buyer_ts = ts;
            }
        }
        if self.bundle.contains(&user) {
            self.bundle_now += after - before;
            self.bundle_peak = self.bundle_peak.max(self.bundle_now);
        }
        if user == c.creator && c.creator != 0 {
            self.dev_now += after - before;
            self.dev_peak = self.dev_peak.max(self.dev_now);
        }
        self.window.push_back((ts, buy, sol, fresh));
        if buy {
            self.buy_w += sol;
        } else {
            self.sell_w += sol;
        }
        self.new_buyers_w += fresh as usize;
        while self.window.front().is_some_and(|w| w.0 < ts - FLOW_S) {
            let (_, b, s, f) = self.window.pop_front().unwrap();
            if b {
                self.buy_w -= s;
            } else {
                self.sell_w -= s;
            }
            self.new_buyers_w -= f as usize;
        }
    }
    fn curve(&mut self, c: &Coin, x: &Tick) {
        self.apply(c, x.ts, x.buy, x.user, 0, x.tok);
        // the window's SOL comes from the curve's own reserves on this leg
        if let Some(w) = self.window.back_mut() {
            w.2 = x.rsol.abs_diff(self.net_sol);
            if x.buy {
                self.buy_w += w.2;
            } else {
                self.sell_w += w.2;
            }
        }
        self.net_sol = x.rsol;
        self.net_sol_peak = self.net_sol_peak.max(self.net_sol);
        self.mcap_sol = x.mcap_sol();
    }
    fn pool(&mut self, c: &Coin, x: &PoolTick) {
        self.apply(c, x.ts, x.buy, x.user, x.sol, x.tok);
        self.graduated = true;
        self.net_sol = x.liq;
        self.net_sol_peak = self.net_sol_peak.max(self.net_sol);
        self.mcap_sol = x.mcap_sol;
    }
    fn snap(&self, c: &Coin, ts: i64) -> Snap {
        let share = |now: i128, peak: i128| {
            if peak <= 0 {
                1.0
            } else {
                (now.max(0) as f64 / peak as f64).min(1.0)
            }
        };
        Snap {
            age: ts - c.created_ts,
            flow_ratio: if self.buy_w == 0 {
                if self.sell_w == 0 {
                    1.0
                } else {
                    99.0
                }
            } else {
                self.sell_w as f64 / self.buy_w as f64
            },
            bundle_left: share(self.bundle_now, self.bundle_peak),
            dev_left: share(self.dev_now, self.dev_peak),
            holders_vs_peak: if self.holders_peak == 0 {
                1.0
            } else {
                self.holders as f64 / self.holders_peak as f64
            },
            new_buyers_w: self.new_buyers_w,
            since_new_buyer: if self.last_new_buyer_ts == 0 {
                ts - c.created_ts
            } else {
                ts - self.last_new_buyer_ts
            },
            net_sol_vs_peak: if self.net_sol_peak == 0 {
                1.0
            } else {
                self.net_sol as f64 / self.net_sol_peak as f64
            },
            holders: self.holders,
        }
    }
}

/// When to buy: a reading of the live state within a window of the create.
#[derive(Clone, Debug, PartialEq)]
pub enum EntryRule {
    /// the plain signal: SOL in the curve, holders, buyers, known wallets
    Signal(Signal),
    /// the second wave: the bundle has sold at least half, SOL in the curve is still at
    /// or near its peak, new buyers keep coming, and the curve holds this much SOL
    SecondWave {
        max_bundle_left: f64,
        min_sol_vs_peak: f64,
        min_new_buyers_w: usize,
        min_net_sol: f64,
        within_s: i64,
    },
    /// no real bundle (the first-5 s buyers took at most this share of the supply at
    /// their peak), and breadth: holders and new buyers
    Organic {
        max_bundle_share: f64,
        min_holders: usize,
        min_new_buyers_w: usize,
        within_s: i64,
    },
    /// after an exit: money flowing back in (buys over sells in SOL over the last
    /// minute by this ratio, at least 1 SOL bought), new buyers still coming, and SOL
    /// in the curve or pool back at or above this share of its peak
    Resume {
        min_buy_ratio: f64,
        min_new_buyers_w: usize,
        min_sol_vs_peak: f64,
    },
    /// the first of several to hold
    Any(Vec<EntryRule>),
}

impl EntryRule {
    pub fn label(&self) -> String {
        match self {
            EntryRule::Signal(s) => s.label(),
            EntryRule::SecondWave {
                max_bundle_left,
                min_sol_vs_peak,
                min_new_buyers_w,
                min_net_sol,
                within_s,
            } => format!(
                "second wave: bundle ≤ {:.0}% left & SOL ≥ {:.0}% of its peak & new buyers 60 s ≥ {} & net SOL ≥ {} within {} s",
                max_bundle_left * 100.0,
                min_sol_vs_peak * 100.0,
                min_new_buyers_w,
                min_net_sol,
                within_s
            ),
            EntryRule::Organic {
                max_bundle_share,
                min_holders,
                min_new_buyers_w,
                within_s,
            } => format!(
                "organic: bundle ≤ {:.0}% of supply & holders ≥ {} & new buyers 60 s ≥ {} within {} s",
                max_bundle_share * 100.0,
                min_holders,
                min_new_buyers_w,
                within_s
            ),
            EntryRule::Resume {
                min_buy_ratio,
                min_new_buyers_w,
                min_sol_vs_peak,
            } => format!(
                "resume: buys ≥ {min_buy_ratio}× sells over 60 s & new buyers 60 s ≥ {min_new_buyers_w} & SOL ≥ {:.0}% of its peak",
                min_sol_vs_peak * 100.0
            ),
            EntryRule::Any(v) => v
                .iter()
                .map(|r| r.label())
                .collect::<Vec<_>>()
                .join("  OR  "),
        }
    }
    fn within(&self) -> i64 {
        match self {
            EntryRule::Signal(s) => s.within_s,
            EntryRule::SecondWave { within_s, .. } | EntryRule::Organic { within_s, .. } => {
                *within_s
            }
            EntryRule::Resume { .. } => i64::MAX,
            EntryRule::Any(v) => v.iter().map(|r| r.within()).max().unwrap_or(0),
        }
    }
    fn holds(&self, st: &State, snap: &Snap) -> bool {
        match self {
            EntryRule::Signal(_) => false,
            EntryRule::SecondWave {
                max_bundle_left,
                min_sol_vs_peak,
                min_new_buyers_w,
                min_net_sol,
                ..
            } => {
                st.bundle_peak > 0
                    && snap.bundle_left <= *max_bundle_left
                    && snap.net_sol_vs_peak >= *min_sol_vs_peak
                    && snap.new_buyers_w >= *min_new_buyers_w
                    && st.net_sol as f64 / LAMPORTS >= *min_net_sol
            }
            EntryRule::Organic {
                max_bundle_share,
                min_holders,
                min_new_buyers_w,
                ..
            } => {
                st.bundle_peak as f64 <= max_bundle_share * SUPPLY_BASE_UNITS
                    && st.holders >= *min_holders
                    && snap.new_buyers_w >= *min_new_buyers_w
            }
            EntryRule::Resume {
                min_buy_ratio,
                min_new_buyers_w,
                min_sol_vs_peak,
            } => {
                st.buy_w >= LAMPORTS as u64
                    && st.buy_w as f64 >= min_buy_ratio * st.sell_w as f64
                    && snap.new_buyers_w >= *min_new_buyers_w
                    && snap.net_sol_vs_peak >= *min_sol_vs_peak
            }
            EntryRule::Any(v) => v.iter().any(|r| r.holds(st, snap)),
        }
    }
}

impl Coin {
    /// Block time of the first trade after which the entry rule holds (curve only:
    /// an entry is a curve buy), within its window of the create.
    pub fn entry_ts(&self, rule: &EntryRule) -> Option<i64> {
        match rule {
            EntryRule::Signal(s) => return self.signal_ts(s),
            EntryRule::Any(v) => return v.iter().filter_map(|r| self.entry_ts(r)).min(),
            _ => {}
        }
        let mut st = State::default();
        for x in &self.ticks {
            if x.ts - self.created_ts > rule.within() {
                return None;
            }
            st.curve(self, x);
            if rule.holds(&st, &st.snap(self, x.ts)) {
                return Some(x.ts);
            }
        }
        None
    }
}

/// When to sell: a reading of the live state, not a clock.
#[derive(Clone, Debug, PartialEq)]
pub enum ExitRule {
    /// sells exceed buys in SOL over the last minute by this ratio (at least 1 SOL sold)
    MoneyLeaving(f64),
    /// the bundle (buyers in the first 5 s) holds at most this share of its peak
    BundleOut(f64),
    /// the creator holds at most this share of their peak (only when they bought)
    DevOut(f64),
    /// holders at most this share of their peak (peak ≥ 20)
    HoldersFalling(f64),
    /// no new buyer for this many seconds
    NoNewBuyer(i64),
    /// SOL in the curve or pool at most this share of its peak
    SolLeaving(f64),
    /// the position at most this share of its peak value (the price reference)
    Drawdown(f64),
    /// either of two
    Either(Box<ExitRule>, Box<ExitRule>),
    /// both of two
    All(Box<ExitRule>, Box<ExitRule>),
    /// never: ride to the end of the data
    None,
}

impl ExitRule {
    pub fn label(&self) -> String {
        match self {
            ExitRule::MoneyLeaving(r) => format!("money leaving: sells ≥ {r}× buys over 60 s"),
            ExitRule::BundleOut(x) => {
                format!("bundle out: first-5 s buyers hold ≤ {:.0}%", x * 100.0)
            }
            ExitRule::DevOut(x) => format!("dev out: creator holds ≤ {:.0}%", x * 100.0),
            ExitRule::HoldersFalling(x) => format!("holders ≤ {:.0}% of their peak", x * 100.0),
            ExitRule::NoNewBuyer(s) => format!("no new buyer for {s} s"),
            ExitRule::SolLeaving(x) => format!("SOL in curve/pool ≤ {:.0}% of its peak", x * 100.0),
            ExitRule::Drawdown(x) => format!("price ≤ {:.0}% of the peak", x * 100.0),
            ExitRule::Either(a, b) => format!("{} | {}", a.label(), b.label()),
            ExitRule::All(a, b) => format!("{} & {}", a.label(), b.label()),
            ExitRule::None => "ride to the end of the data".into(),
        }
    }
    fn holds(&self, st: &State, snap: &Snap, value_vs_peak: f64) -> bool {
        match self {
            ExitRule::MoneyLeaving(r) => {
                st.sell_w >= LAMPORTS as u64 && st.sell_w as f64 >= r * st.buy_w as f64
            }
            ExitRule::BundleOut(x) => st.bundle_peak > 0 && snap.bundle_left <= *x,
            ExitRule::DevOut(x) => st.dev_peak > 0 && snap.dev_left <= *x,
            ExitRule::HoldersFalling(x) => st.holders_peak >= 20 && snap.holders_vs_peak <= *x,
            ExitRule::NoNewBuyer(s) => snap.since_new_buyer >= *s,
            ExitRule::SolLeaving(x) => snap.net_sol_vs_peak <= *x,
            ExitRule::Drawdown(x) => value_vs_peak <= *x,
            ExitRule::Either(a, b) => {
                a.holds(st, snap, value_vs_peak) || b.holds(st, snap, value_vs_peak)
            }
            ExitRule::All(a, b) => {
                a.holds(st, snap, value_vs_peak) && b.holds(st, snap, value_vs_peak)
            }
            ExitRule::None => false,
        }
    }
}

/// One ride from the signal entry to an exit rule's sell (or the end of the data).
#[derive(Clone, Copy, Debug)]
pub struct Ride {
    pub entry_ts: i64,
    pub exit_ts: i64,
    /// return on what we paid (size plus two transaction fees)
    pub ret: f64,
    /// the best return seen before the exit
    pub peak_ret: f64,
    /// the rule fired (else the data ended with the position open)
    pub by_rule: bool,
    pub graduated: bool,
}

/// What the whole path looked like, for working backwards from the peak.
#[derive(Clone, Copy, Debug, Default)]
pub struct Trail {
    pub peak_ret: f64,
    pub peak_ts: i64,
    pub at_peak: Snap,
    /// the first reading a minute or more after the peak
    pub after_peak: Option<Snap>,
    /// the first reading at which the position was worth half its peak
    pub at_half: Option<Snap>,
    pub end_ret: f64,
}

/// Our tokens sold into the pool as it is after `x` (constant product, the pool fee).
fn pool_value(x: &PoolTick, tokens: u64) -> u64 {
    if x.mcap_sol <= 0.0 || x.liq == 0 {
        return 0;
    }
    // lamports per base unit = mcap_sol / 1e6
    let r_tok = x.liq as f64 / (x.mcap_sol / 1e6);
    let out = x.liq as f64 * tokens as f64 / (r_tok + tokens as f64);
    (out * (1.0 - x.fee_bps as f64 / 10_000.0)) as u64
}

/// Our tokens from a buy of `size` lamports into the pool as it is after `x`.
fn pool_buy(x: &PoolTick, size: u64) -> u64 {
    if x.mcap_sol <= 0.0 || x.liq == 0 {
        return 0;
    }
    let r_tok = x.liq as f64 / (x.mcap_sol / 1e6);
    let sol_in = size as f64 * (1.0 - x.fee_bps as f64 / 10_000.0);
    (r_tok * sol_in / (x.liq as f64 + sol_in)) as u64
}

/// A buy landing at `fill_ts`: on the curve before graduation, in the pool after it
/// (at the pool as its last trade left it; none before the pool's first trade).
fn open_at(c: &Coin, fill_ts: i64, k: &Costs) -> Option<Position> {
    match c.graduated_ts {
        Some(g) if g <= fill_ts => {
            let i = c.pool.partition_point(|x| x.ts <= fill_ts);
            if i == 0 {
                return None;
            }
            let tokens = pool_buy(&c.pool[i - 1], k.size_lamports);
            (tokens > 0).then_some(Position { input: 0, tokens })
        }
        _ => open(c, fill_ts, k),
    }
}

/// Every trade of the coin in time order: (ts, in the pool, index).
fn events_of(c: &Coin) -> Vec<(i64, bool, usize)> {
    let mut events: Vec<(i64, bool, usize)> = Vec::with_capacity(c.ticks.len() + c.pool.len());
    events.extend(c.ticks.iter().enumerate().map(|(i, x)| (x.ts, false, i)));
    events.extend(c.pool.iter().enumerate().map(|(i, x)| (x.ts, true, i)));
    events.sort_by_key(|e| (e.0, e.1));
    events
}

/// The position's sell value after the trade `(is_pool, i)`.
fn event_value(c: &Coin, pos: &Position, is_pool: bool, i: usize) -> u64 {
    if is_pool {
        pool_value(&c.pool[i], pos.tokens)
    } else {
        pos.value(&c.ticks[i])
    }
}

/// A sell landing at `ts`: when it fills (the last trade at or before it), what it
/// returns, and whether that was in the pool.
fn value_with(
    c: &Coin,
    events: &[(i64, bool, usize)],
    pos: &Position,
    entry_ts: i64,
    ts: i64,
) -> (i64, u64, bool) {
    let n = events.partition_point(|e| e.0 <= ts);
    if n == 0 {
        return (entry_ts, pos.value(&c.at(entry_ts)), false);
    }
    let (ets, is_pool, i) = events[n - 1];
    if !is_pool && c.graduated_ts.is_some_and(|g| g <= ts) {
        // graduated, no pool trade yet: the pool as the curve left it
        let pool = pool_after_graduation(with_us(c.ticks[i].state(), pos.input, pos.tokens));
        return (
            ets,
            sell_quote_for_tokens(&pool, pos.tokens, AMM_FEE_BPS),
            true,
        );
    }
    (ets.max(entry_ts), event_value(c, pos, is_pool, i), is_pool)
}

/// One leg of a ride with re-entry: a buy and the sell that closed it.
#[derive(Clone, Copy, Debug)]
pub struct Leg {
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub entry_mcap_sol: f64,
    pub exit_mcap_sol: f64,
    /// return on what we paid (size plus two transaction fees)
    pub ret: f64,
    /// closed by the exit rule (else the data ended with the position open)
    pub by_rule: bool,
}

/// The state machine with re-entry: buy `delay` after the first trade at which `entry`
/// holds, sell `delay` after the first later trade at which `exit` holds, buy again
/// `delay` after `reentry` next holds, and so on to the end of the data; a position
/// still open at the end is marked at the last trade. Every leg is returned.
pub fn legs(
    c: &Coin,
    entry: &EntryRule,
    reentry: &EntryRule,
    exit: &ExitRule,
    k: &Costs,
) -> Vec<Leg> {
    let Some(first) = c.entry_ts(entry) else {
        return vec![];
    };
    let events = events_of(c);
    let cost = (k.size_lamports + 2 * k.tx_lamports) as f64;
    let mut st = State::default();
    let mut out = vec![];
    let mut next_buy: Option<i64> = Some(first + k.delay_s);
    let mut next_sell: Option<i64> = None;
    let mut open_pos: Option<(Position, i64, u64)> = None; // position, its entry, its peak value
    let mut last_fill = first + k.delay_s;
    let close = |pos: &Position, entry_ts: i64, sell_ts: i64, by_rule: bool, out: &mut Vec<Leg>| {
        let (exit_ts, value, _) = value_with(c, &events, pos, entry_ts, sell_ts);
        out.push(Leg {
            entry_ts,
            exit_ts,
            entry_mcap_sol: c.mcap_sol_at(entry_ts),
            exit_mcap_sol: c.mcap_sol_at(exit_ts),
            ret: value as f64 / cost - 1.0,
            by_rule,
        });
    };
    for &(ts, is_pool, i) in &events {
        if is_pool {
            st.pool(c, &c.pool[i]);
        } else {
            st.curve(c, &c.ticks[i]);
        }
        // fills decided earlier land before this trade is seen
        if let Some(t) = next_buy.filter(|t| ts > *t) {
            next_buy = None;
            if let Some(pos) = open_at(c, t, k) {
                open_pos = Some((pos, t, 0));
            }
            last_fill = t;
        }
        if let Some(t) = next_sell.filter(|t| ts > *t) {
            next_sell = None;
            if let Some((pos, entry_ts, _)) = open_pos.take() {
                close(&pos, entry_ts, t, true, &mut out);
            }
            last_fill = t;
        }
        if ts <= last_fill {
            continue;
        }
        let snap = st.snap(c, ts);
        match (&mut open_pos, next_sell, next_buy) {
            (Some((pos, _, peak)), None, _) => {
                let v = event_value(c, pos, is_pool, i);
                *peak = (*peak).max(v);
                if exit.holds(&st, &snap, v as f64 / (*peak).max(1) as f64) {
                    next_sell = Some(ts + k.delay_s);
                }
            }
            (None, _, None) if !out.is_empty() && reentry.holds(&st, &snap) => {
                next_buy = Some(ts + k.delay_s);
            }
            _ => {}
        }
    }
    if let Some((pos, entry_ts, _)) = open_pos.take() {
        let sell_ts = next_sell.unwrap_or(i64::MAX);
        close(&pos, entry_ts, sell_ts, next_sell.is_some(), &mut out);
    }
    out
}

/// Buy with a fill at `entry_ts`, then watch the live state on every later trade,
/// curve and pool, and sell `k.delay_s` after the first trade at which each rule
/// holds; a rule that never holds closes at the last trade on tape. One pass serves
/// every rule. `None` when the entry cannot fill.
pub fn ride_all(
    c: &Coin,
    entry_ts: i64,
    rules: &[ExitRule],
    k: &Costs,
) -> Option<(Vec<Ride>, Trail)> {
    let pos = open(c, entry_ts, k)?;
    let cost = (k.size_lamports + 2 * k.tx_lamports) as f64;
    let events = events_of(c);
    let value_of = |is_pool: bool, i: usize| -> u64 { event_value(c, &pos, is_pool, i) };
    // the value a sell landing at `ts` gets: the last trade at or before it
    let value_at = |ts: i64| -> (i64, u64, bool) { value_with(c, &events, &pos, entry_ts, ts) };
    let mut st = State::default();
    let mut path = Trail::default();
    let mut fired: Vec<Option<i64>> = vec![None; rules.len()];
    let mut peak = 0u64;
    let mut all_fired = false;
    for &(ts, is_pool, i) in &events {
        if is_pool {
            st.pool(c, &c.pool[i]);
        } else {
            st.curve(c, &c.ticks[i]);
        }
        if ts <= entry_ts {
            continue;
        }
        let v = value_of(is_pool, i);
        let snap = st.snap(c, ts);
        if v > peak {
            peak = v;
            path.peak_ret = v as f64 / cost - 1.0;
            path.peak_ts = ts;
            path.at_peak = snap;
            path.after_peak = None;
            path.at_half = None;
        } else {
            if path.after_peak.is_none() && ts >= path.peak_ts + 60 {
                path.after_peak = Some(snap);
            }
            if path.at_half.is_none() && (v as f64) <= 0.5 * peak as f64 {
                path.at_half = Some(snap);
            }
        }
        if all_fired {
            continue;
        }
        let vs_peak = v as f64 / peak.max(1) as f64;
        all_fired = true;
        for (r, rule) in rules.iter().enumerate() {
            if fired[r].is_none() {
                if rule.holds(&st, &snap, vs_peak) {
                    fired[r] = Some(ts);
                } else {
                    all_fired = false;
                }
            }
        }
    }
    let (end_ts, end_value, end_graduated) = value_at(i64::MAX);
    path.end_ret = end_value as f64 / cost - 1.0;
    let rides = fired
        .iter()
        .map(|f| {
            let (exit_ts, value, graduated) = match f {
                Some(ts) => value_at(ts + k.delay_s),
                Option::None => (end_ts, end_value, end_graduated),
            };
            let until = f.map_or(i64::MAX, |t| t + k.delay_s);
            let peak_before: u64 = events
                .iter()
                .filter(|e| e.0 > entry_ts && e.0 <= until)
                .map(|e| value_of(e.1, e.2))
                .max()
                .unwrap_or(0);
            Ride {
                entry_ts,
                exit_ts,
                ret: value as f64 / cost - 1.0,
                peak_ret: peak_before.max(value) as f64 / cost - 1.0,
                by_rule: f.is_some(),
                graduated,
            }
        })
        .collect();
    Some((rides, path))
}

// ------------------------------------------------------------------ helpers

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
                buy: true,
                user: i as u64 + 1,
                tok: 10,
            });
        }
        c.books.push(15);
        c
    }

    fn k(delay: i64) -> Costs {
        Costs {
            size_lamports: 500_000_000,
            delay_s: delay,
            tx_lamports: 1_000_000,
        }
    }

    /// Ride with no exit rule: (return at the end of the data, peak return).
    fn hold(c: &Coin, entry_ts: i64, k: &Costs) -> Option<(f64, f64)> {
        let (r, p) = ride_all(c, entry_ts, &[ExitRule::None], k)?;
        assert!(!r[0].by_rule);
        assert!((r[0].peak_ret - p.peak_ret.max(r[0].ret)).abs() < 1e-12 || p.peak_ret >= r[0].ret);
        Some((r[0].ret, r[0].peak_ret))
    }

    #[test]
    fn exit_rules_read_the_live_state_and_sell_after_the_delay() {
        // wallet 1 buys at 3 s (the bundle), wallets 2 and 3 at 10 and 20 s; the curve
        // rises to 60 SOL; then wallet 1 sells everything at 40 s and wallet 2 some at 85 s
        let mut c = coin(
            &[(3, 40.0), (10, 50.0), (20, 60.0), (40, 45.0), (85, 44.0)],
            None,
        );
        c.ticks[3].buy = false;
        c.ticks[3].user = 1;
        c.ticks[3].tok = 10;
        c.ticks[4].buy = false;
        c.ticks[4].user = 2;
        c.ticks[4].tok = 5;
        let rules = [
            ExitRule::BundleOut(0.5),
            ExitRule::MoneyLeaving(2.0),
            ExitRule::Drawdown(0.5),
            ExitRule::NoNewBuyer(15),
            ExitRule::None,
        ];
        let (r, p) = ride_all(&c, T0 + 12, &rules, &k(2)).unwrap();
        // the bundle sold out at 40 s: sell lands at 42 s on the 45 SOL curve
        assert!(r[0].by_rule);
        assert_eq!(r[0].exit_ts, T0 + 40, "last trade at or before 42 s");
        let (hold_ret, _) = hold(&c, T0 + 12, &k(2)).unwrap();
        assert!(
            r[0].ret > hold_ret,
            "out before the 44 SOL end: {} vs {hold_ret}",
            r[0].ret
        );
        // at 40 s the 30 SOL bought are still in the window: money is not leaving yet;
        // by 85 s the buys have aged out and 16 SOL has been sold against none bought
        assert!(r[1].by_rule && r[1].exit_ts == T0 + 85);
        // the price never fell to half its peak; no new buyer between 20 and 40 s fires at 40 s
        assert!(!r[2].by_rule);
        assert!(r[3].by_rule && r[3].exit_ts == T0 + 40);
        assert!(!r[4].by_rule);
        // the path: peak at 20 s with the bundle whole, half the bundle gone after it
        assert_eq!(p.peak_ts, T0 + 20);
        assert!((p.at_peak.bundle_left - 1.0).abs() < 1e-9);
        assert_eq!(p.at_peak.holders, 3);
        assert!(p.after_peak.unwrap().bundle_left < 0.01);
        assert!(p.at_half.is_none());
        assert!(p.end_ret < p.peak_ret);
    }

    #[test]
    fn the_buy_fills_on_the_curve_as_it_was_and_the_path_is_valued_from_it() {
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
        // signal at 15 s, our fill 4 s later: bought on the curve as it was at 19 s
        let (last, peak) = hold(&c, T0 + 19, &k(4)).unwrap();
        let buy_state = c.at(T0 + 19);
        assert_eq!(buy_state.vsol, 36_000_000_000);
        let tokens = buy_tokens_for_quote(&buy_state.state(), 500_000_000, 125);
        let input = curve_input(500_000_000, 125);
        // the hour ends on the last trade (44 s, 40 SOL): exact sell with us in the curve
        let sold =
            sell_quote_for_tokens(&with_us(c.at(T0 + 44).state(), input, tokens), tokens, 125);
        assert!((last - (sold as f64 / 502_000_000.0 - 1.0)).abs() < 1e-12);
        assert!(last > 0.1 && last < 0.25, "{last}");
        // the peak is the 64 SOL trade at 40 s: price x3.2 on our entry, less fees and impact
        let top =
            sell_quote_for_tokens(&with_us(c.at(T0 + 40).state(), input, tokens), tokens, 125);
        assert!((peak - (top as f64 / 502_000_000.0 - 1.0)).abs() < 1e-12);
        assert!(peak > 1.5 && peak > last, "{peak}");
        // a round trip on a flat curve costs the two fees and two transaction fees
        let flat = coin(&[(5, 40.0), (16, 40.0)], None);
        let (last, peak) = hold(&flat, T0 + 17, &k(2)).unwrap();
        assert!(last < -0.02 && last > -0.04, "{last}");
        assert_eq!(peak, last, "nothing traded after us: the peak is the end");
    }

    #[test]
    fn graduation_sells_into_the_pool_and_late_or_post_graduation_entries_are_none() {
        let up = coin(&[(5, 40.0), (100, 90.0), (200, 115.0)], Some(200));
        let (last, peak) = hold(&up, T0 + 17, &k(2)).unwrap();
        assert!(last > 3.0, "{last}");
        assert!(peak >= last);
        // already graduated at the fill: no trade; after the window: no trade
        assert!(hold(&coin(&[(5, 115.0)], Some(10)), T0 + 17, &k(2)).is_none());
        assert!(hold(&up, T0 + WINDOW_SECS, &k(0)).is_none());
    }

    #[test]
    fn a_signal_is_the_first_trade_after_which_it_holds() {
        // three buys by three wallets at 3, 4 and 10 s; the curve holds 20 SOL after the second
        let c = coin(&[(3, 40.0), (4, 50.0), (10, 52.0)], None);
        assert_eq!(c.signal_ts(&Signal::simple(20.0, 0, 0, 60)), Some(T0 + 4));
        assert_eq!(c.signal_ts(&Signal::simple(0.0, 3, 0, 60)), Some(T0 + 10));
        assert_eq!(
            c.signal_ts(&Signal::simple(0.0, 3, 0, 5)),
            None,
            "outside the window"
        );
        assert_eq!(c.signal_ts(&Signal::simple(20.0, 2, 2, 60)), Some(T0 + 4));
        let known = Signal::wallets([2u64].into(), 1, 60, "test");
        assert_eq!(c.signal_ts(&known), Some(T0 + 4));
        assert_eq!(known.label(), "test ≥ 1 bought within 60 s");
        assert_eq!(c.early_buyers(5), vec![1, 2]);
        // market cap scales with the square of the virtual SOL: 27.96 SOL at 30
        let start = START_VSOL as f64 * 1e6 / START_VTOK as f64 * 1e9 / LAMPORTS;
        assert!(
            (c.mcap_sol_at(T0 + 4) - start * (50.0 / 30.0) * (50.0 / 30.0)).abs() < 0.01,
            "{}",
            c.mcap_sol_at(T0 + 4)
        );
        assert_eq!(c.trades(), 3);
    }
}
