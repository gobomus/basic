//! First-minute microstructure of every Pump.fun coin, from the free RPC log stream.
//!
//! `logsSubscribe` on the pump program (the public endpoint works, about 1 s behind the
//! chain) carries every create, trade and graduation as `Program data:` events, so the
//! exact life of each curve is known without a paid feed. For every coin created while
//! the stream is up, a book follows its trades for an hour:
//!
//! * `micro` rows: the coin at 5, 15, 30, 60, 120, 300 and 900 s after its creation
//!   (block time): holders, buyers, flow, market cap, curve progress, holder and buyer
//!   concentration (HHI), top-10 share, dev and sniper behaviour. A snapshot only
//!   counts trades whose block time is at or before its moment.
//! * `curve_outcomes` rows, one hour after creation: the market cap at each snapshot
//!   and the highest market cap after it, and whether and when the coin graduated.
//!   These are the labels for the early-signal tables (no lookahead: a snapshot's
//!   outcome only uses later trades).
//! * `feed` rows each minute: transactions, failures, trades, creates, delay behind
//!   the chain, disconnections. A snapshot carries `gap_ms`, the time the stream was
//!   down during the coin's life, so incomplete coins can be left out.
//! * `trades-<hour>.jsonl.gz`: the decoded events themselves (see [`TradesMode`]).
//!
//! The coins that go far do so after graduation, on PumpSwap, so a second stream
//! follows the pump AMM program and every graduated coin's pool for a day:
//! `graduations` rows (the pool, the creator, the market cap it landed at), `candles`
//! rows (one per minute: open/high/low/close market cap, buy and sell SOL, buyers,
//! sellers, the creator's sells), `amm_outcomes` rows at 1, 6 and 24 h after
//! graduation (peak multiple and when, deepest drawdown, the creator's selling), and
//! the pool's trades on the trade tape (`ev: "amm"`).
//!
//! Every row carries `sol_usd` (the SOL price at the time, from `sol_price` rows the
//! census writes each minute) so market caps can be read in dollars, the unit traders
//! see.
//!
//! Holdings come from curve trades only. Tokens moved by plain transfers are not seen;
//! in a coin's first minutes they are rare, and a balance that would go negative counts
//! as zero.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use chain::consts::WSOL_MINT;
use chain::consts::{PUMP_AMM_PROGRAM, PUMP_PROGRAM};
use chain::pda::pump_pool_for;
use chain::pump::{CreateEvent, PumpEvent, TradeEvent};
use chain::pump_amm::SwapEventData;
use chain::solana_sdk::pubkey::Pubkey;
use futures::{SinkExt, StreamExt};
use reqwest_websocket::{Message, RequestBuilderExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;

use crate::census::{now_ms, Tape};

/// Seconds after creation at which a coin's book is written.
pub const SNAPSHOTS: [i64; 7] = [5, 15, 30, 60, 120, 300, 900];
/// The outcome row is written this long after creation; the book is then dropped.
pub const OUTCOME_SECS: i64 = 3600;
/// Wait this long past a snapshot's moment for trades still in flight. Block times
/// have 1 s resolution; the public stream runs 1-3 s behind, with a p99 up to ~9 s
/// when the network is busy. Trades that arrive later still are counted as `late`.
const GRACE_MS: i64 = 10_000;
/// Pump.fun coins have 1 billion tokens with 6 decimals.
const SUPPLY: f64 = 1e15;
/// A curve is full (graduates) at this many SOL of real reserves.
const CURVE_FULL_SOL: f64 = 85.005;
/// Buyers in the create slot or this many slots after it are snipers.
const SNIPER_SLOTS: u64 = 1;
/// Per-second flow is kept for the first minute.
const INFLOW_SECS: usize = 60;
/// A graduated coin's pool is followed this long.
const AMM_FOLLOW_SECS: i64 = 86_400;
/// Seconds after graduation at which an `amm_outcomes` row is written.
pub const AMM_OUTCOMES: [i64; 3] = [3600, 21_600, 86_400];
/// Market regime windows: launches in the last 10 min, graduations in the last hour.
const REGIME_LAUNCH_SECS: i64 = 600;
const REGIME_GRAD_SECS: i64 = 3600;

/// Which decoded events go to `trades-<hour>.jsonl.gz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TradesMode {
    /// Creates, graduations and the trades of coins in their first hour (default).
    FirstHour,
    /// Every pump trade (≈ 5 million a day; for a server with disk).
    All,
    /// Nothing (the micro rows and outcomes are still written).
    None,
}

#[derive(Default, Clone)]
struct Wallet {
    bal: i128,
    max_bal: i128,
    bought: u64,
    sold: u64,
    first_buy: Option<(u64, i64)>,
}

/// One curve trade as kept in a book.
#[derive(Clone)]
struct Trade {
    slot: u64,
    /// arrival order, to keep a block's trades in their order
    seq: u64,
    ts: i64,
    user: Pubkey,
    buy: bool,
    sol: u64,
    tokens: u64,
    mcap: f64,
    curve_sol: f64,
}

/// Market cap (SOL) of a curve before its first trade (30 SOL / 1.073 B tokens virtual).
const START_MCAP_SOL: f64 = 27.958;

struct Book {
    created_ts: i64,
    create_slot: u64,
    seen_ms: i64,
    creator: Pubkey,
    symbol: String,
    name: String,
    pump_suffix: bool,
    mayhem: bool,
    /// trades up to the last snapshot, in arrival order (dropped after it)
    trades: Vec<Trade>,
    /// the whole hour, for the outcome: net tokens and buyers per wallet
    end_bal: HashMap<Pubkey, (i128, bool)>,
    n_trades: u32,
    last: Option<(u64, u64, f64)>,
    mcap_at: [Option<f64>; SNAPSHOTS.len()],
    peak_raw: [f64; SNAPSHOTS.len()],
    next: usize,
    graduated_ts: Option<i64>,
    late: u32,
    gap_ms: i64,
    quote: Option<Pubkey>,
}

impl Book {
    fn new(e: &CreateEvent, slot: u64, seen_ms: i64) -> Self {
        Self {
            created_ts: e.timestamp,
            create_slot: slot,
            seen_ms,
            creator: e.creator,
            symbol: e.symbol.clone(),
            name: e.name.clone(),
            pump_suffix: e.mint.to_string().ends_with("pump"),
            mayhem: e.is_mayhem_mode,
            trades: vec![],
            end_bal: HashMap::new(),
            n_trades: 0,
            last: None,
            mcap_at: [None; SNAPSHOTS.len()],
            peak_raw: [0.0; SNAPSHOTS.len()],
            next: 0,
            graduated_ts: None,
            late: 0,
            gap_ms: 0,
            quote: None,
        }
    }

    /// Block time (s) up to which snapshot `i` counts trades.
    fn cutoff(&self, i: usize) -> i64 {
        self.created_ts + SNAPSHOTS[i]
    }

    fn add(&mut self, e: &TradeEvent, slot: u64, seq: u64) {
        if e.timestamp > self.created_ts + OUTCOME_SECS {
            return;
        }
        // stamped before a snapshot that is already written: it missed that snapshot
        if self.next > 0 && e.timestamp <= self.cutoff(self.next - 1) {
            self.late += 1;
        }
        if let Some(q) = e
            .quote_mint
            .filter(|q| *q != chain::consts::WSOL_MINT && *q != Pubkey::default())
        {
            self.quote = Some(q);
        }
        let mcap = e.price_sol() * 1e9;
        let t = Trade {
            slot,
            seq,
            ts: e.timestamp,
            user: e.user,
            buy: e.is_buy,
            sol: e.sol_amount,
            tokens: e.token_amount,
            mcap,
            curve_sol: e.real_sol_reserves as f64 / 1e9,
        };
        if e.timestamp <= self.cutoff(SNAPSHOTS.len() - 1) && self.next < SNAPSHOTS.len() {
            self.trades.push(t);
        }
        let w = self.end_bal.entry(e.user).or_default();
        w.0 += if e.is_buy {
            e.token_amount as i128
        } else {
            -(e.token_amount as i128)
        };
        w.1 |= e.is_buy;
        self.n_trades += 1;
        if self.last.is_none_or(|(s, q, _)| (slot, seq) > (s, q)) {
            self.last = Some((slot, seq, mcap));
        }
        for i in 0..SNAPSHOTS.len() {
            if e.timestamp > self.cutoff(i) {
                self.peak_raw[i] = self.peak_raw[i].max(mcap);
            }
        }
    }

    /// Snapshot `i`, rebuilt from the trades stamped at or before its moment, in chain
    /// order (slot, then position), whatever order they arrived in.
    fn snapshot(&mut self, i: usize, gap_ms: i64, ctx: &Ctx) -> Value {
        let t = SNAPSHOTS[i];
        let cutoff = self.cutoff(i);
        let mut tr: Vec<&Trade> = self.trades.iter().filter(|x| x.ts <= cutoff).collect();
        tr.sort_by_key(|x| (x.slot, x.seq));
        let mut w: HashMap<Pubkey, Wallet> = HashMap::new();
        let (mut buys, mut sells, mut buy_l, mut sell_l) = (0u32, 0u32, 0u64, 0u64);
        let mut inflow = [0i64; INFLOW_SECS];
        for x in &tr {
            let e = w.entry(x.user).or_default();
            if x.buy {
                e.bal += x.tokens as i128;
                e.bought += x.sol;
                e.first_buy.get_or_insert((x.slot, x.ts));
                buys += 1;
                buy_l += x.sol;
            } else {
                e.bal = (e.bal - x.tokens as i128).max(0);
                e.sold += x.sol;
                sells += 1;
                sell_l += x.sol;
            }
            e.max_bal = e.max_bal.max(e.bal);
            let sec = x.ts - self.created_ts;
            if (0..INFLOW_SECS as i64).contains(&sec) {
                inflow[sec as usize] += if x.buy { x.sol as i64 } else { -(x.sol as i64) };
            }
        }
        let (mcap, curve_sol) = tr
            .last()
            .map(|x| (x.mcap, x.curve_sol))
            .unwrap_or((START_MCAP_SOL, 0.0));
        self.mcap_at[i] = Some(mcap);
        let mut holders: Vec<i128> = w.values().map(|w| w.bal).filter(|b| *b > 0).collect();
        holders.sort_unstable_by(|a, b| b.cmp(a));
        let held: f64 = holders.iter().map(|b| *b as f64).sum();
        let hhi = if held > 0.0 {
            holders
                .iter()
                .map(|b| (*b as f64 / held).powi(2))
                .sum::<f64>()
        } else {
            0.0
        };
        let bought: f64 = w.values().map(|w| w.bought as f64).sum();
        let hhi_buys = if bought > 0.0 {
            w.values()
                .map(|w| (w.bought as f64 / bought).powi(2))
                .sum::<f64>()
        } else {
            0.0
        };
        let pct = |b: f64| (b / SUPPLY * 1e4).round() / 100.0;
        let top10: f64 = holders.iter().take(10).map(|b| *b as f64).sum();
        let dev = w.get(&self.creator).cloned().unwrap_or_default();
        let snipers: Vec<&Wallet> = w
            .iter()
            .filter(|(k, w)| {
                **k != self.creator
                    && w.first_buy
                        .is_some_and(|(s, _)| s <= self.create_slot + SNIPER_SLOTS)
            })
            .map(|(_, w)| w)
            .collect();
        let snipers_out = snipers
            .iter()
            .filter(|w| w.max_bal > 0 && w.bal * 10 <= w.max_bal)
            .count();
        let new_buyers_5s = w
            .values()
            .filter(|w| w.first_buy.is_some_and(|(_, ts)| ts > cutoff - 5))
            .count();
        let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
        let mut row = json!({
            "mint": Value::Null, // filled by the caller
            "t": t,
            "created_ts": self.created_ts,
            "holders": holders.len(),
            "buyers": w.values().filter(|w| w.bought > 0).count(),
            "sellers": w.values().filter(|w| w.sold > 0).count(),
            "buys": buys,
            "sells": sells,
            "buy_sol": r3(buy_l as f64 / 1e9),
            "sell_sol": r3(sell_l as f64 / 1e9),
            "net_sol": r3((buy_l as f64 - sell_l as f64) / 1e9),
            "mcap_sol": r3(mcap),
            "curve_sol": r3(curve_sol),
            "progress": r3(curve_sol / CURVE_FULL_SOL),
            "hhi": r3(hhi),
            "hhi_buys": r3(hhi_buys),
            "top1_pct": pct(holders.first().copied().unwrap_or(0) as f64),
            "top10_pct": pct(top10),
            "dev_pct": pct(dev.bal as f64),
            "dev_sold": dev.sold > 0,
            "snipers": snipers.len(),
            "snipers_pct": pct(snipers.iter().map(|w| w.bal as f64).sum()),
            "snipers_out": snipers_out,
            "new_buyers_5s": new_buyers_5s,
            "graduated": self.graduated_ts.is_some_and(|g| g <= cutoff),
            "late": self.late,
            "gap_ms": gap_ms,
        });
        if let Some(q) = self.quote {
            row["quote"] = json!(q.to_string());
        }
        if let Some(p) = ctx.sol_usd {
            row["sol_usd"] = json!(p);
            row["mcap_usd"] = json!((mcap * p).round());
        }
        if i == 0 {
            row["symbol"] = json!(self.symbol);
            row["name"] = json!(self.name);
            row["creator"] = json!(self.creator.to_string());
            row["create_slot"] = json!(self.create_slot);
            row["pump_suffix"] = json!(self.pump_suffix);
            row["mayhem"] = json!(self.mayhem);
            row["seen_delay_ms"] = json!(self.seen_ms - self.created_ts * 1000);
            // the create transaction: what the creator bought, who else was in that slot
            let in_slot: Vec<&&Trade> = tr.iter().filter(|x| x.slot == self.create_slot).collect();
            let creator_sol: u64 = in_slot
                .iter()
                .filter(|x| x.buy && x.user == self.creator)
                .map(|x| x.sol)
                .sum();
            let slot_sol: u64 = in_slot.iter().filter(|x| x.buy).map(|x| x.sol).sum();
            row["create_buy_sol"] = json!(r3(creator_sol as f64 / 1e9));
            row["create_slot_buys"] = json!(in_slot.iter().filter(|x| x.buy).count());
            row["create_slot_sol"] = json!(r3(slot_sol as f64 / 1e9));
            row["instant_grad"] = json!(self.graduated_ts == Some(self.created_ts));
            // the market around the launch
            row["launches_10m"] = json!(ctx.launches_10m);
            row["grads_1h"] = json!(ctx.grads_1h);
        }
        if t == INFLOW_SECS as i64 {
            row["inflow_1s"] = json!(inflow
                .iter()
                .map(|l| r3(*l as f64 / 1e9))
                .collect::<Vec<_>>());
        }
        if i == SNAPSHOTS.len() - 1 {
            self.trades = vec![];
        }
        row
    }

    fn outcome(&self, mint: &Pubkey, full_window: bool, gap_ms: i64) -> Value {
        let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
        let mut row = json!({
            "mint": mint.to_string(),
            "created_ts": self.created_ts,
            "create_slot": self.create_slot,
            "symbol": self.symbol,
            "name": self.name,
            "creator": self.creator.to_string(),
            "pump_suffix": self.pump_suffix,
            "mayhem": self.mayhem,
            "t": SNAPSHOTS,
            "mcap_at": self.mcap_at.iter().map(|m| m.map(r3)).collect::<Vec<_>>(),
            "peak_after": self.peak_raw.iter().zip(&self.mcap_at)
                .map(|(p, m)| m.map(|m| r3(p.max(m)))).collect::<Vec<_>>(),
            "graduated": self.graduated_ts.is_some(),
            "graduated_after_s": self.graduated_ts.map(|g| g - self.created_ts),
            "trades": self.n_trades,
            "holders": self.end_bal.values().filter(|w| w.0 > 0).count(),
            "buyers": self.end_bal.values().filter(|w| w.1).count(),
            "final_mcap_sol": self.last.map(|l| r3(l.2)),
            "late": self.late,
            "gap_ms": gap_ms,
            "full_window": full_window,
        });
        if let Some(q) = self.quote {
            row["quote"] = json!(q.to_string());
        }
        row
    }
}

/// Stream-down time during a coin's life up to `now` (`down` = down since, if down now).
fn gap_of(b: &Book, down: Option<i64>, now: i64) -> i64 {
    b.gap_ms
        + down
            .map(|d| (now - d.max(b.created_ts * 1000)).max(0))
            .unwrap_or(0)
}

/// What one step produced: rows for the tape (`kind`, row) and trade-tape lines.
#[derive(Default)]
pub struct Out {
    pub rows: Vec<(&'static str, Value)>,
    pub tape: Vec<Value>,
}

#[derive(Default)]
struct Minute {
    amm_tx: u64,
    amm_trades: u64,
    tx: u64,
    failed: u64,
    truncated: u64,
    trades: u64,
    creates: u64,
    completes: u64,
    lags_ms: Vec<i64>,
}

/// What a snapshot needs from outside its book: the SOL price and the market regime.
#[derive(Clone, Copy, Default)]
pub struct Ctx {
    pub sol_usd: Option<f64>,
    pub launches_10m: usize,
    pub grads_1h: usize,
}

/// The books of all coins in their first hour, the pools of graduated coins for a
/// day, and the streams' health.
pub struct Micro {
    books: HashMap<Pubkey, Book>,
    pools: HashMap<Pubkey, AmmBook>,
    mode: TradesMode,
    minute: Minute,
    minute_start: i64,
    down_since: Option<i64>,
    amm_down_since: Option<i64>,
    pub disconnects: u64,
    pub amm_disconnects: u64,
    seq: u64,
    pub sol_usd: Option<f64>,
    recent_creates: VecDeque<i64>,
    recent_grads: VecDeque<i64>,
}

impl Micro {
    pub fn new(mode: TradesMode, now: i64) -> Self {
        Self {
            books: HashMap::new(),
            pools: HashMap::new(),
            mode,
            minute: Minute::default(),
            minute_start: now,
            down_since: None,
            amm_down_since: None,
            disconnects: 0,
            amm_disconnects: 0,
            seq: 0,
            sol_usd: None,
            recent_creates: VecDeque::new(),
            recent_grads: VecDeque::new(),
        }
    }

    pub fn coins(&self) -> usize {
        self.books.len()
    }

    pub fn pools(&self) -> usize {
        self.pools.len()
    }

    fn ctx(&mut self, now_s: i64) -> Ctx {
        while self
            .recent_creates
            .front()
            .is_some_and(|t| *t < now_s - REGIME_LAUNCH_SECS)
        {
            self.recent_creates.pop_front();
        }
        while self
            .recent_grads
            .front()
            .is_some_and(|t| *t < now_s - REGIME_GRAD_SECS)
        {
            self.recent_grads.pop_front();
        }
        Ctx {
            sol_usd: self.sol_usd,
            launches_10m: self.recent_creates.len(),
            grads_1h: self.recent_grads.len(),
        }
    }

    pub fn set_amm_down(&mut self, now: i64) {
        if self.amm_down_since.is_none() {
            self.amm_down_since = Some(now);
            self.amm_disconnects += 1;
        }
    }

    pub fn set_amm_up(&mut self) {
        self.amm_down_since = None;
    }

    pub fn set_down(&mut self, now: i64) {
        if self.down_since.is_none() {
            self.down_since = Some(now);
            self.disconnects += 1;
        }
    }

    pub fn set_up(&mut self, now: i64) {
        if let Some(d) = self.down_since.take() {
            for b in self.books.values_mut() {
                b.gap_ms += (now - d.max(b.created_ts * 1000)).max(0);
            }
        }
    }

    pub fn on_failed(&mut self) {
        self.minute.tx += 1;
        self.minute.failed += 1;
    }

    /// One successful pump transaction from the stream.
    pub fn on_tx(
        &mut self,
        slot: u64,
        sig: &str,
        recv_ms: i64,
        events: Vec<PumpEvent>,
        truncated: bool,
        out: &mut Out,
    ) {
        self.minute.tx += 1;
        self.minute.truncated += truncated as u64;
        for ev in events {
            match ev {
                PumpEvent::Create(e) => {
                    self.minute.creates += 1;
                    if self.mode != TradesMode::None {
                        out.tape.push(json!({
                            "ev": "create", "slot": slot, "ts": e.timestamp, "sig": sig,
                            "mint": e.mint.to_string(), "creator": e.creator.to_string(),
                            "user": e.user.to_string(), "symbol": e.symbol, "name": e.name, "uri": e.uri,
                            "mayhem": e.is_mayhem_mode,
                        }));
                    }
                    self.recent_creates.push_back(e.timestamp);
                    self.books
                        .entry(e.mint)
                        .or_insert_with(|| Book::new(&e, slot, recv_ms));
                }
                PumpEvent::Trade(e) => {
                    self.minute.trades += 1;
                    self.minute.lags_ms.push(recv_ms - e.timestamp * 1000);
                    let tracked = self.books.contains_key(&e.mint);
                    if self.mode == TradesMode::All
                        || (tracked && self.mode == TradesMode::FirstHour)
                    {
                        let mut row = json!({
                            "ev": "trade", "slot": slot, "ts": e.timestamp, "sig": sig,
                            "mint": e.mint.to_string(), "user": e.user.to_string(), "buy": e.is_buy,
                            "sol": e.sol_amount, "tok": e.token_amount,
                            "vsol": e.virtual_sol_reserves, "vtok": e.virtual_token_reserves,
                            "rsol": e.real_sol_reserves, "rtok": e.real_token_reserves,
                            "fee_bps": e.total_fee_bps(),
                        });
                        if let Some(q) = e.quote_mint {
                            row["quote"] = json!(q.to_string());
                        }
                        out.tape.push(row);
                    }
                    if let Some(b) = self.books.get_mut(&e.mint) {
                        self.seq += 1;
                        b.add(&e, slot, self.seq);
                    }
                }
                PumpEvent::Complete(e) => {
                    self.minute.completes += 1;
                    if self.mode != TradesMode::None {
                        out.tape.push(json!({
                            "ev": "complete", "slot": slot, "ts": e.timestamp, "sig": sig,
                            "mint": e.mint.to_string(), "user": e.user.to_string(),
                        }));
                    }
                    if let Some(b) = self.books.get_mut(&e.mint) {
                        b.graduated_ts.get_or_insert(e.timestamp);
                    }
                    self.recent_grads.push_back(e.timestamp);
                    self.follow_pool(&e.mint, e.timestamp, out);
                }
            }
        }
    }

    /// Follow a graduated coin's PumpSwap pool from now on. A curve priced in another
    /// token (`quote` on its book) migrates into a pool quoted in that token; its prices
    /// are not in SOL, so it is recorded but not followed. A coin whose create we missed
    /// is taken as SOL-quoted (the great majority): if it was not, its pool never trades
    /// and the follow stays empty.
    fn follow_pool(&mut self, mint: &Pubkey, grad_ts: i64, out: &mut Out) {
        let book = self.books.get(mint);
        let quote = book.and_then(|b| b.quote).unwrap_or(WSOL_MINT);
        let pool = pump_pool_for(mint, &quote);
        if self.pools.contains_key(&pool) {
            return;
        }
        let sol_quoted = quote == WSOL_MINT;
        let mut row = json!({
            "mint": mint.to_string(), "pool": pool.to_string(), "grad_ts": grad_ts,
            "created_ts": book.map(|b| b.created_ts),
            "creator": book.map(|b| b.creator.to_string()),
            "symbol": book.map(|b| b.symbol.clone()),
            "instant": book.is_some_and(|b| b.created_ts == grad_ts),
            "followed": sol_quoted,
            "sol_usd": self.sol_usd,
        });
        if !sol_quoted {
            row["quote"] = json!(quote.to_string());
        }
        if let Some(b) = book {
            row["curve_trades"] = json!(b.n_trades);
            row["curve_holders"] = json!(b.end_bal.values().filter(|w| w.0 > 0).count());
        }
        out.rows.push(("graduations", row));
        if sol_quoted {
            let holders: HashSet<Pubkey> = book
                .map(|b| {
                    b.end_bal
                        .iter()
                        .filter(|(_, w)| w.0 > 0)
                        .map(|(k, _)| *k)
                        .collect()
                })
                .unwrap_or_default();
            self.pools.insert(
                pool,
                AmmBook::new(*mint, grad_ts, book.map(|b| b.creator), holders),
            );
        }
    }

    /// One successful PumpSwap transaction from the AMM stream.
    pub fn on_amm(
        &mut self,
        slot: u64,
        sig: &str,
        recv_ms: i64,
        events: Vec<SwapEventData>,
        out: &mut Out,
    ) {
        self.minute.amm_tx += 1;
        for e in events {
            let Some(b) = self.pools.get_mut(&e.pool) else {
                continue;
            };
            self.minute.amm_trades += 1;
            self.minute.lags_ms.push(recv_ms - e.timestamp * 1000);
            if b.creator.is_none() && e.coin_creator != Pubkey::default() {
                b.creator = Some(e.coin_creator);
            }
            let mcap = e.post_price(6, 9) * 1e9;
            let before = e.price(6, 9) * 1e9;
            let liq = e.post_reserves().1.min(u64::MAX as u128) as u64;
            if self.mode != TradesMode::None {
                out.tape.push(json!({
                    "ev": "amm", "slot": slot, "ts": e.timestamp, "sig": sig,
                    "mint": b.mint.to_string(), "pool": e.pool.to_string(), "user": e.user.to_string(),
                    "buy": e.is_buy, "sol": e.user_flow(), "base": e.base_amount,
                    "mcap_sol": (mcap * 1000.0).round() / 1000.0,
                    "liq": liq,
                    "fee_bps": e.total_fee_bps(),
                }));
            }
            let gap = self.amm_down_since.is_some();
            for row in b.add(&e, slot, before, mcap, liq, gap, self.sol_usd) {
                out.rows.push(("candles", row));
            }
        }
    }

    /// Snapshots and outcomes that are due by the clock, and the minute's feed row.
    pub fn tick(&mut self, now: i64, out: &mut Out) {
        let mut done = vec![];
        let down = self.down_since;
        let ctx = self.ctx(now / 1000);
        for (mint, b) in self.books.iter_mut() {
            let gap = gap_of(b, down, now);
            while b.next < SNAPSHOTS.len() && now >= b.cutoff(b.next) * 1000 + GRACE_MS {
                let mut row = b.snapshot(b.next, gap, &ctx);
                row["mint"] = json!(mint.to_string());
                out.rows.push(("micro", row));
                b.next += 1;
            }
            if now >= (b.created_ts + OUTCOME_SECS) * 1000 + GRACE_MS {
                out.rows
                    .push(("curve_outcomes", b.outcome(mint, true, gap)));
                done.push(*mint);
            }
        }
        for m in done {
            self.books.remove(&m);
        }
        // pools: closed candles, outcomes, and the end of the follow
        let amm_gap = self.amm_down_since.is_some();
        let mut gone = vec![];
        for (pool, b) in self.pools.iter_mut() {
            out.rows.extend(
                b.closed_candles(now, amm_gap, self.sol_usd)
                    .into_iter()
                    .map(|r| ("candles", r)),
            );
            while b.next_outcome < AMM_OUTCOMES.len()
                && now >= (b.grad_ts + AMM_OUTCOMES[b.next_outcome]) * 1000 + GRACE_MS
            {
                out.rows.push((
                    "amm_outcomes",
                    b.outcome(AMM_OUTCOMES[b.next_outcome], true, self.sol_usd),
                ));
                b.next_outcome += 1;
            }
            if now >= (b.grad_ts + AMM_FOLLOW_SECS) * 1000 + GRACE_MS {
                gone.push(*pool);
            }
        }
        for p in gone {
            self.pools.remove(&p);
        }
        if now - self.minute_start >= 60_000 {
            out.rows.push(("feed", self.feed_row(now)));
        }
    }

    fn feed_row(&mut self, now: i64) -> Value {
        let m = std::mem::take(&mut self.minute);
        let mut lags = m.lags_ms;
        lags.sort_unstable();
        let q = |p: f64| {
            (!lags.is_empty()).then(|| lags[((lags.len() - 1) as f64 * p).round() as usize])
        };
        let row = json!({
            "ts": now,
            "secs": (now - self.minute_start) / 1000,
            "tx": m.tx,
            "failed": m.failed,
            "truncated": m.truncated,
            "trades": m.trades,
            "creates": m.creates,
            "completes": m.completes,
            "lag_p50_ms": q(0.5),
            "lag_p90_ms": q(0.9),
            "lag_p99_ms": q(0.99),
            "connected": self.down_since.is_none(),
            "disconnects": self.disconnects,
            "coins": self.books.len(),
            "amm_tx": m.amm_tx,
            "amm_trades": m.amm_trades,
            "amm_connected": self.amm_down_since.is_none(),
            "amm_disconnects": self.amm_disconnects,
            "pools": self.pools.len(),
            "sol_usd": self.sol_usd,
        });
        self.minute_start = now;
        row
    }

    /// On stop: the outcome of every coin still open, marked as an incomplete window.
    pub fn finish(&mut self, now: i64, out: &mut Out) {
        self.tick(now, out);
        for (mint, b) in &self.books {
            out.rows.push((
                "curve_outcomes",
                b.outcome(mint, false, gap_of(b, self.down_since, now)),
            ));
        }
        self.books.clear();
        for b in self.pools.values_mut() {
            out.rows.extend(
                b.flush_candle(self.sol_usd)
                    .into_iter()
                    .map(|r| ("candles", r)),
            );
            let at = (now / 1000 - b.grad_ts).max(0);
            out.rows
                .push(("amm_outcomes", b.outcome(at, false, self.sol_usd)));
        }
        self.pools.clear();
        out.rows.push(("feed", self.feed_row(now)));
    }
}

// ------------------------------------------------------------------ after graduation

/// One minute of a pool, in SOL market cap.
struct Candle {
    minute: i64,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    buy_sol: u64,
    sell_sol: u64,
    buys: u32,
    sells: u32,
    buyers: HashSet<Pubkey>,
    sellers: HashSet<Pubkey>,
    creator_sold: u64,
    creator_bought: u64,
    /// sells by wallets that never bought on the curve or the pool (airdrops, insiders)
    unbought_sold: u64,
    unbought_sellers: HashSet<Pubkey>,
    /// SOL in the pool at the end of the minute
    liq: u64,
    gap: bool,
}

impl Candle {
    fn row(&self, mint: &Pubkey, grad_ts: i64, sol_usd: Option<f64>) -> Value {
        let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
        json!({
            "mint": mint.to_string(),
            "minute": self.minute,
            "age_min": (self.minute - grad_ts) / 60,
            "o": r3(self.o), "h": r3(self.h), "l": r3(self.l), "c": r3(self.c),
            "buy_sol": r3(self.buy_sol as f64 / 1e9),
            "sell_sol": r3(self.sell_sol as f64 / 1e9),
            "buys": self.buys,
            "sells": self.sells,
            "buyers": self.buyers.len(),
            "sellers": self.sellers.len(),
            "creator_sold_sol": r3(self.creator_sold as f64 / 1e9),
            "creator_bought_sol": r3(self.creator_bought as f64 / 1e9),
            "unbought_sell_sol": r3(self.unbought_sold as f64 / 1e9),
            "unbought_sellers": self.unbought_sellers.len(),
            "liq_sol": r3(self.liq as f64 / 1e9),
            "gap": self.gap,
            "sol_usd": sol_usd,
        })
    }
}

/// A graduated coin's pool: candles as they close, and the path's extremes.
struct AmmBook {
    mint: Pubkey,
    grad_ts: i64,
    creator: Option<Pubkey>,
    candle: Option<Candle>,
    first: Option<f64>,
    last: f64,
    peak: f64,
    peak_ts: i64,
    /// lowest market cap since the peak
    trough: f64,
    /// deepest fall from any earlier high, as a share
    max_dd: f64,
    trades: u64,
    buyers: HashSet<Pubkey>,
    creator_sold: u64,
    next_outcome: usize,
    /// the pool's price before its first swap: what a buyer at landing paid
    landing: Option<f64>,
    /// SOL in the pool after the last swap
    liq: u64,
    /// the pool's first slot, and what was bought in it (the creator and his bundle
    /// taking the float: a fabricated market cap)
    grad_slot: Option<u64>,
    grad_slot_base: u64,
    grad_slot_sol: u64,
    /// wallets that held the coin when it graduated (curve holders)
    curve_holders: HashSet<Pubkey>,
    unbought_sold: u64,
}

impl AmmBook {
    fn new(
        mint: Pubkey,
        grad_ts: i64,
        creator: Option<Pubkey>,
        curve_holders: HashSet<Pubkey>,
    ) -> Self {
        Self {
            mint,
            grad_ts,
            creator,
            candle: None,
            first: None,
            last: 0.0,
            peak: 0.0,
            peak_ts: grad_ts,
            trough: f64::INFINITY,
            max_dd: 0.0,
            trades: 0,
            buyers: HashSet::new(),
            creator_sold: 0,
            next_outcome: 0,
            landing: None,
            liq: 0,
            grad_slot: None,
            grad_slot_base: 0,
            grad_slot_sol: 0,
            curve_holders,
            unbought_sold: 0,
        }
    }

    /// Apply one swap (`before`: the price it met, `mcap`: the price it left, `liq`: the
    /// pool's SOL after it); returns the candle it closed, if any.
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        e: &SwapEventData,
        slot: u64,
        before: f64,
        mcap: f64,
        liq: u64,
        gap: bool,
        sol_usd: Option<f64>,
    ) -> Vec<Value> {
        let mut out = vec![];
        self.trades += 1;
        self.first.get_or_insert(mcap);
        self.landing.get_or_insert(before);
        self.liq = liq;
        let grad_slot = *self.grad_slot.get_or_insert(slot);
        if e.is_buy && slot == grad_slot {
            self.grad_slot_base += e.base_amount;
            self.grad_slot_sol += e.user_flow();
        }
        self.last = mcap;
        if mcap > self.peak {
            self.peak = mcap;
            self.peak_ts = e.timestamp;
            self.trough = mcap;
        } else {
            self.trough = self.trough.min(mcap);
            if self.peak > 0.0 {
                self.max_dd = self.max_dd.max(1.0 - mcap / self.peak);
            }
        }
        let flow = e.user_flow();
        let by_creator = self.creator == Some(e.user);
        let unbought =
            !e.is_buy && !self.buyers.contains(&e.user) && !self.curve_holders.contains(&e.user);
        if e.is_buy {
            self.buyers.insert(e.user);
        } else if by_creator {
            self.creator_sold += flow;
        }
        if unbought {
            self.unbought_sold += flow;
        }
        let minute = e.timestamp.div_euclid(60) * 60;
        if self.candle.as_ref().is_some_and(|c| c.minute != minute) {
            out.extend(self.flush_candle(sol_usd));
        }
        let c = self.candle.get_or_insert_with(|| Candle {
            minute,
            o: mcap,
            h: mcap,
            l: mcap,
            c: mcap,
            buy_sol: 0,
            sell_sol: 0,
            buys: 0,
            sells: 0,
            buyers: HashSet::new(),
            sellers: HashSet::new(),
            creator_sold: 0,
            creator_bought: 0,
            unbought_sold: 0,
            unbought_sellers: HashSet::new(),
            liq,
            gap: false,
        });
        c.h = c.h.max(mcap);
        c.l = c.l.min(mcap);
        c.c = mcap;
        c.liq = liq;
        c.gap |= gap;
        if e.is_buy {
            c.buy_sol += flow;
            c.buys += 1;
            c.buyers.insert(e.user);
            if by_creator {
                c.creator_bought += flow;
            }
        } else {
            c.sell_sol += flow;
            c.sells += 1;
            c.sellers.insert(e.user);
            if by_creator {
                c.creator_sold += flow;
            }
            if unbought {
                c.unbought_sold += flow;
                c.unbought_sellers.insert(e.user);
            }
        }
        out
    }

    fn flush_candle(&mut self, sol_usd: Option<f64>) -> Option<Value> {
        self.candle
            .take()
            .map(|c| c.row(&self.mint, self.grad_ts, sol_usd))
    }

    /// The open candle once its minute is over (plus the grace for late trades).
    fn closed_candles(&mut self, now_ms: i64, gap: bool, sol_usd: Option<f64>) -> Option<Value> {
        if let Some(c) = self.candle.as_mut() {
            c.gap |= gap;
            if now_ms >= (c.minute + 60) * 1000 + GRACE_MS {
                return self.flush_candle(sol_usd);
            }
        }
        None
    }

    fn outcome(&self, at_secs: i64, full_window: bool, sol_usd: Option<f64>) -> Value {
        let r3 = |x: f64| (x * 1000.0).round() / 1000.0;
        let first = self.first.unwrap_or(0.0);
        let landing = self.landing.unwrap_or(0.0);
        json!({
            "mint": self.mint.to_string(),
            "grad_ts": self.grad_ts,
            "at": at_secs,
            "creator": self.creator.map(|c| c.to_string()),
            "landing_mcap_sol": r3(landing),
            "first_mcap_sol": r3(first),
            "peak_mcap_sol": r3(self.peak),
            "peak_multiple": (landing > 0.0).then(|| r3(self.peak / landing)),
            "alive": landing > 0.0 && self.last >= 0.5 * landing,
            "liq_sol": r3(self.liq as f64 / 1e9),
            "insider_share": r3(self.grad_slot_base as f64 / SUPPLY),
            "grad_slot_sol": r3(self.grad_slot_sol as f64 / 1e9),
            "unbought_sell_sol": r3(self.unbought_sold as f64 / 1e9),
            "peak_after_s": self.peak_ts - self.grad_ts,
            "max_drawdown": r3(self.max_dd),
            "trough_after_peak_sol": if self.trough.is_finite() { r3(self.trough) } else { 0.0 },
            "last_mcap_sol": r3(self.last),
            "trades": self.trades,
            "buyers": self.buyers.len(),
            "creator_sold_sol": r3(self.creator_sold as f64 / 1e9),
            "full_window": full_window,
            "sol_usd": sol_usd,
        })
    }
}

// ------------------------------------------------------------------ the stream

#[derive(Deserialize)]
struct Notification {
    params: Option<Params>,
}
#[derive(Deserialize)]
struct Params {
    result: NotifResult,
}
#[derive(Deserialize)]
struct NotifResult {
    context: NotifCtx,
    value: LogValue,
}
#[derive(Deserialize)]
struct NotifCtx {
    slot: u64,
}
#[derive(Deserialize)]
struct LogValue {
    signature: String,
    err: Option<Value>,
    logs: Vec<String>,
}

/// One transaction from a stream.
pub struct StreamTx {
    pub slot: u64,
    pub sig: String,
    /// the program this stream follows (pump or pump AMM)
    pub program: Pubkey,
    pub events: Vec<PumpEvent>,
    pub amm: Vec<SwapEventData>,
    /// The node cut the logs short (events after the cut are missing).
    pub truncated: bool,
    pub failed: bool,
}

/// The `program`'s events in one `logsNotification` (`None` for any other message).
pub fn decode_notification(text: &str, program: &Pubkey) -> Option<StreamTx> {
    let n: Notification = serde_json::from_str(text).ok()?;
    let r = n.params?.result;
    let failed = r.value.err.is_some();
    let (mut events, mut amm) = (vec![], vec![]);
    if !failed {
        let data = chain::detect::program_data_in_logs(&r.value.logs, program);
        if *program == PUMP_AMM_PROGRAM {
            amm = data
                .iter()
                .filter_map(|b| chain::pump_amm::decode_event(b))
                .collect();
        } else {
            events = data
                .iter()
                .filter_map(|b| chain::pump::decode_event(b))
                .collect();
        }
    }
    Some(StreamTx {
        slot: r.context.slot,
        sig: r.value.signature,
        program: *program,
        events,
        amm,
        truncated: r.value.logs.iter().any(|l| l.starts_with("Log truncated")),
        failed,
    })
}

fn sig_hash(sig: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    sig.hash(&mut h);
    h.finish()
}

/// Buffered `trades-<hour>.jsonl.gz` writer: one gzip member per flush.
struct TradeTape {
    dir: PathBuf,
    buf: Vec<(i64, String)>,
}

impl TradeTape {
    fn flush(&mut self) -> anyhow::Result<()> {
        let mut by_file: HashMap<PathBuf, String> = HashMap::new();
        for (ms, line) in self.buf.drain(..) {
            let t = chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default();
            let d = self.dir.join(t.format("%Y-%m-%d").to_string());
            let p = d.join(format!("trades-{}.jsonl.gz", t.format("%H")));
            let s = by_file.entry(p).or_default();
            s.push_str(&line);
            s.push('\n');
        }
        for (p, s) in by_file {
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)?;
            let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            gz.write_all(s.as_bytes())?;
            gz.finish()?;
        }
        Ok(())
    }
}

enum Feed {
    Up(usize),
    Down(usize, String),
    Tx(i64, StreamTx),
}

/// One connection: subscribe, forward every notification, reconnect for ever. The
/// public endpoint recycles connections every minute or so; a connection that was
/// healthy reconnects at once, a failing one backs off.
async fn reader(id: usize, url: String, program: Pubkey, tx: tokio::sync::mpsc::Sender<Feed>) {
    let mut backoff = 0u64;
    loop {
        let conn = async {
            let client = reqwest::Client::builder()
                .http1_only()
                .connect_timeout(Duration::from_secs(10))
                .build()?;
            let mut ws = client
                .get(&url)
                .upgrade()
                .send()
                .await?
                .into_websocket()
                .await?;
            ws.send(Message::Text(
                json!({"jsonrpc": "2.0", "id": 1, "method": "logsSubscribe",
                       "params": [{"mentions": [program.to_string()]}, {"commitment": "processed"}]})
                .to_string(),
            ))
            .await?;
            anyhow::Ok(ws)
        };
        let reason = match tokio::time::timeout(Duration::from_secs(20), conn).await {
            Ok(Ok(mut ws)) => {
                if tx.send(Feed::Up(id)).await.is_err() {
                    return;
                }
                let started = std::time::Instant::now();
                let reason: String = loop {
                    let text = match tokio::time::timeout(Duration::from_secs(20), ws.next()).await
                    {
                        Err(_) => break "no message for 20 s".into(),
                        Ok(None) => break "closed".into(),
                        Ok(Some(Err(e))) => break e.to_string(),
                        Ok(Some(Ok(Message::Text(t)))) => t,
                        // tungstenite answers pings itself
                        Ok(Some(Ok(Message::Close { reason, .. }))) => {
                            break format!("closed by server: {reason}")
                        }
                        Ok(Some(Ok(_))) => continue,
                    };
                    let recv = now_ms();
                    match decode_notification(&text, &program) {
                        Some(t) => {
                            if tx.send(Feed::Tx(recv, t)).await.is_err() {
                                return;
                            }
                        }
                        None if text.contains("\"error\"") => {
                            break format!(
                                "subscription refused: {}",
                                text.chars().take(200).collect::<String>()
                            )
                        }
                        None => {}
                    }
                };
                backoff = if started.elapsed() > Duration::from_secs(10) {
                    0
                } else {
                    (backoff * 2).clamp(1, 30)
                };
                reason
            }
            Ok(Err(e)) => {
                backoff = (backoff * 2).clamp(1, 30);
                e.to_string()
            }
            Err(_) => {
                backoff = (backoff * 2).clamp(1, 30);
                "connect timed out".into()
            }
        };
        if tx.send(Feed::Down(id, reason)).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
    }
}

/// Follow the pump program's logs until `stop` fires, writing the micro rows, outcomes,
/// feed rows and trade tape. `conns` connections are kept to each URL at once and every
/// transaction is taken from whichever delivers it first, so one connection being
/// recycled leaves no hole; a gap is only counted while all of them are down.
pub async fn run(
    urls: Vec<String>,
    conns: usize,
    tape: Tape,
    dir: PathBuf,
    mode: TradesMode,
    sol_usd: watch::Receiver<Option<f64>>,
    mut stop: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Feed>(50_000);
    let mut readers = vec![];
    let mut is_amm = vec![];
    for program in [PUMP_PROGRAM, PUMP_AMM_PROGRAM] {
        for url in urls
            .iter()
            .flat_map(|u| std::iter::repeat_n(u, conns.max(1)))
        {
            let id = readers.len();
            readers.push(tokio::spawn(reader(id, url.clone(), program, tx.clone())));
            is_amm.push(program == PUMP_AMM_PROGRAM);
        }
    }
    drop(tx);
    let mut up = vec![false; readers.len()];
    let live_of = |up: &[bool], amm: bool| {
        up.iter()
            .zip(&is_amm)
            .filter(|(u, a)| **u && **a == amm)
            .count()
    };
    let mut micro = Micro::new(mode, now_ms());
    micro.set_down(now_ms());
    micro.set_amm_down(now_ms());
    micro.disconnects = 0;
    micro.amm_disconnects = 0;
    let mut trades = TradeTape { dir, buf: vec![] };
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    // signatures seen recently (hashed), to take each transaction once
    let (mut seen, mut seen_prev) = (
        std::collections::HashSet::<u64>::new(),
        std::collections::HashSet::<u64>::new(),
    );
    let (mut last_flush, mut last_log) = (now_ms(), now_ms());
    let (mut recycles, mut dupes) = (0u64, 0u64);
    let mut totals = (0u64, 0u64, 0u64); // creates, trades taped, outcomes
    let write = |out: Out,
                 totals: &mut (u64, u64, u64),
                 trades: &mut TradeTape,
                 live: usize,
                 recycles: u64,
                 dupes: u64|
     -> anyhow::Result<()> {
        let now = now_ms();
        for (kind, mut row) in out.rows {
            match kind {
                "curve_outcomes" => totals.2 += 1,
                "feed" => {
                    row["connections_up"] = json!(live);
                    // since the recorder started (the other counts are per row)
                    row["recycles_total"] = json!(recycles);
                    row["duplicates_total"] = json!(dupes);
                }
                _ => {}
            }
            tape.row(kind, now, &row)?;
        }
        for row in out.tape {
            match row["ev"].as_str() {
                Some("create") => totals.0 += 1,
                Some("trade") => totals.1 += 1,
                _ => {}
            }
            trades.buf.push((now, row.to_string()));
        }
        Ok(())
    };
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            f = rx.recv() => {
                let Some(f) = f else { break };
                let live = live_of(&up, false);
                match f {
                    Feed::Up(i) => {
                        if !up[i] {
                            up[i] = true;
                            if is_amm[i] {
                                if live_of(&up, true) == 1 { micro.set_amm_up(); eprintln!("pump AMM stream up"); }
                            } else if live == 0 {
                                micro.set_up(now_ms());
                                eprintln!("pump stream up ({} connection(s) per program to {})", conns.max(1), urls.join(", "));
                            }
                        }
                    }
                    Feed::Down(i, reason) => {
                        if up[i] {
                            up[i] = false;
                            recycles += 1;
                            if is_amm[i] {
                                if live_of(&up, true) == 0 { micro.set_amm_down(now_ms()); eprintln!("pump AMM stream: every connection down ({reason})"); }
                            } else if live == 1 {
                                micro.set_down(now_ms());
                                eprintln!("pump stream: every connection down ({reason})");
                            }
                        } else if recycles == 0 || live == 0 {
                            eprintln!("stream connection {i}: {reason}");
                        }
                    }
                    Feed::Tx(recv, t) => {
                        // one transaction can touch both programs: each stream keeps its own events
                        let h = sig_hash(&t.sig) ^ (t.program == PUMP_AMM_PROGRAM) as u64;
                        if seen.contains(&h) || seen_prev.contains(&h) {
                            dupes += 1;
                            continue;
                        }
                        if seen.len() >= 200_000 {
                            seen_prev = std::mem::take(&mut seen);
                        }
                        seen.insert(h);
                        if t.failed {
                            micro.on_failed();
                            continue;
                        }
                        let mut out = Out::default();
                        if t.program == PUMP_AMM_PROGRAM {
                            micro.on_amm(t.slot, &t.sig, recv, t.amm, &mut out);
                        } else {
                            micro.on_tx(t.slot, &t.sig, recv, t.events, t.truncated, &mut out);
                        }
                        write(out, &mut totals, &mut trades, live, recycles, dupes)?;
                    }
                }
            }
            _ = tick.tick() => {
                let now = now_ms();
                let live = live_of(&up, false);
                micro.sol_usd = *sol_usd.borrow();
                let mut out = Out::default();
                micro.tick(now, &mut out);
                write(out, &mut totals, &mut trades, live, recycles, dupes)?;
                if now - last_flush >= 5_000 {
                    last_flush = now;
                    trades.flush()?;
                }
                if now - last_log >= 60_000 {
                    last_log = now;
                    eprintln!(
                        "pump stream: {} creates, {} trades taped, {} outcomes, {} coins open, {} pools followed · {live}+{}/{} connections up, {recycles} recycled, {} gaps · SOL {}",
                        totals.0, totals.1, totals.2, micro.coins(), micro.pools(), live_of(&up, true), up.len(), micro.disconnects,
                        micro.sol_usd.map(|p| format!("${p:.2}")).unwrap_or_else(|| "?".into())
                    );
                }
            }
        }
    }
    for r in readers {
        r.abort();
    }
    let live = live_of(&up, false);
    let mut out = Out::default();
    micro.finish(now_ms(), &mut out);
    write(out, &mut totals, &mut trades, live, recycles, dupes)?;
    trades.flush()?;
    eprintln!(
        "pump stream stopped: {} creates, {} trades taped, {} outcomes, {} gaps",
        totals.0, totals.1, totals.2, micro.disconnects
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_791_500_000;

    pub(super) fn create(mint: Pubkey, creator: Pubkey) -> PumpEvent {
        PumpEvent::Create(CreateEvent {
            name: "Test".into(),
            symbol: "TST".into(),
            uri: String::new(),
            mint,
            bonding_curve: Pubkey::new_unique(),
            user: creator,
            creator,
            timestamp: T0,
            token_program: None,
            is_mayhem_mode: false,
        })
    }

    /// A trade that leaves the curve at `vsol` SOL of virtual reserves (price grows with it).
    pub(super) fn trade(
        mint: Pubkey,
        user: Pubkey,
        buy: bool,
        tokens: u64,
        sol: f64,
        ts: i64,
        vsol: f64,
    ) -> PumpEvent {
        let vsol = (vsol * 1e9) as u64;
        PumpEvent::Trade(TradeEvent {
            mint,
            sol_amount: (sol * 1e9) as u64,
            token_amount: tokens,
            is_buy: buy,
            user,
            timestamp: ts,
            virtual_sol_reserves: vsol,
            virtual_token_reserves: (1_073_000_000_000_000u128 * 30_000_000_000 / vsol as u128)
                as u64,
            real_sol_reserves: vsol - 30_000_000_000,
            real_token_reserves: 0,
            fee_recipient: Pubkey::default(),
            fee_basis_points: 95,
            fee: 0,
            creator: Pubkey::default(),
            creator_fee_basis_points: 0,
            creator_fee: 0,
            ix_name: "buy".into(),
            mayhem_mode: false,
            buyback_fee_basis_points: 0,
            quote_mint: None,
        })
    }

    fn feed(m: &mut Micro, slot: u64, ev: PumpEvent, out: &mut Out) {
        let ts = match &ev {
            PumpEvent::Trade(t) => t.timestamp,
            PumpEvent::Create(c) => c.timestamp,
            PumpEvent::Complete(c) => c.timestamp,
        };
        m.on_tx(slot, "sig", ts * 1000 + 900, vec![ev], false, out);
    }

    fn rows<'a>(out: &'a Out, kind: &str) -> Vec<&'a Value> {
        out.rows
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, v)| v)
            .collect()
    }

    #[test]
    fn snapshots_count_only_trades_up_to_their_moment_in_chain_order() {
        let (mint, dev, sniper, a, b, c) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut m = Micro::new(TradesMode::FirstHour, T0 * 1000);
        let mut out = Out::default();
        let e9 = 1_000_000_000_000u64; // 1M tokens (6 decimals)
        feed(&mut m, 100, create(mint, dev), &mut out);
        feed(
            &mut m,
            100,
            trade(mint, dev, true, 50 * e9, 1.5, T0, 31.5),
            &mut out,
        );
        feed(
            &mut m,
            101,
            trade(mint, sniper, true, 30 * e9, 1.0, T0, 32.5),
            &mut out,
        );
        // b's trade (t+11) arrives before a's (t+3): the stream is not in chain order
        feed(
            &mut m,
            125,
            trade(mint, b, true, 10 * e9, 0.4, T0 + 11, 33.6),
            &mut out,
        );
        feed(
            &mut m,
            108,
            trade(mint, a, true, 20 * e9, 0.7, T0 + 3, 33.2),
            &mut out,
        );
        feed(
            &mut m,
            130,
            trade(mint, sniper, false, 30 * e9, 1.1, T0 + 12, 32.5),
            &mut out,
        );
        feed(
            &mut m,
            140,
            trade(mint, c, true, 40 * e9, 1.5, T0 + 16, 34.0),
            &mut out,
        );
        assert!(
            rows(&out, "micro").is_empty(),
            "snapshots wait for the clock"
        );
        m.tick((T0 + 5) * 1000 + GRACE_MS - 1, &mut out);
        assert!(
            rows(&out, "micro").is_empty(),
            "not before the grace period"
        );
        m.tick((T0 + 15) * 1000 + GRACE_MS, &mut out);
        let snaps = rows(&out, "micro");
        assert_eq!(snaps.len(), 2, "5 s and 15 s");
        let s5 = snaps[0];
        assert_eq!(s5["t"], 5);
        assert_eq!(s5["mint"], mint.to_string());
        assert_eq!(s5["holders"], 3);
        assert_eq!(s5["buys"], 3);
        assert_eq!(s5["snipers"], 1, "bought in the slot after the create");
        assert_eq!(s5["snipers_out"], 0);
        assert_eq!(s5["dev_pct"], 5.0);
        assert_eq!(s5["creator"], dev.to_string());
        // the create transaction (slot 100): the dev's 1.5 SOL; the sniper came a slot later
        assert_eq!(s5["create_buy_sol"], 1.5);
        assert_eq!(s5["create_slot_buys"], 1);
        assert_eq!(s5["create_slot_sol"], 1.5);
        assert_eq!(s5["instant_grad"], false);
        assert_eq!(s5["launches_10m"], 1);
        assert_eq!(s5["grads_1h"], 0);
        assert!(s5.get("sol_usd").is_none(), "no price known yet");
        // holdings 50/30/20 → HHI 0.25 + 0.09 + 0.04
        assert_eq!(s5["hhi"], 0.38);
        // market cap after the last trade in chain order (a's, slot 108), not the last to arrive
        let mcap5 = s5["mcap_sol"].as_f64().unwrap();
        assert!(
            (mcap5 - 33.2f64.powi(2) / 30.0 / 1.073).abs() < 0.01,
            "{mcap5}"
        );
        let s15 = snaps[1];
        assert_eq!(s15["t"], 15);
        assert_eq!(
            s15["holders"], 3,
            "the sniper sold out; c bought after 15 s"
        );
        assert_eq!(s15["buyers"], 4);
        assert_eq!(s15["sellers"], 1);
        assert_eq!(s15["snipers_out"], 1);
        assert_eq!(s15["dev_sold"], false);
        assert_eq!(
            s15["new_buyers_5s"], 1,
            "b at t+11 (the last 5 s are t+11 … t+15)"
        );
        assert_eq!(s15["top10_pct"], 8.0);
        assert_eq!(s15["gap_ms"], 0);
        assert_eq!(s15["late"], 0);
        assert!(s15.get("creator").is_none());
        let mcap = s15["mcap_sol"].as_f64().unwrap();
        assert!(
            (mcap - 32.5f64.powi(2) / 30.0 / 1.073).abs() < 0.01,
            "{mcap}"
        );
        // a trade stamped before an already written snapshot is counted as late, and
        // still counts in the later snapshots
        feed(
            &mut m,
            141,
            trade(mint, a, true, e9, 0.01, T0 + 14, 34.0),
            &mut out,
        );
        let mut later = Out::default();
        m.tick((T0 + 30) * 1000 + GRACE_MS, &mut later);
        let s30 = rows(&later, "micro");
        assert_eq!(s30.len(), 1);
        assert_eq!(s30[0]["late"], 1);
        assert_eq!(s30[0]["buys"], 6);
    }

    #[test]
    fn outcome_holds_the_peak_after_each_snapshot_and_graduation() {
        let (mint, dev, a) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut m = Micro::new(TradesMode::None, T0 * 1000);
        let mut out = Out::default();
        let e9 = 1_000_000_000_000u64;
        feed(&mut m, 1, create(mint, dev), &mut out);
        feed(
            &mut m,
            1,
            trade(mint, dev, true, 10 * e9, 0.3, T0, 30.3),
            &mut out,
        );
        // 15 s and 60 s snapshots pass with no trades
        m.tick((T0 + 61) * 1000 + GRACE_MS, &mut out);
        assert_eq!(rows(&out, "micro").len(), 4, "5, 15, 30, 60 s");
        // then the price runs: virtual SOL 30.3 → 60 (market cap ≈ 3.9x)
        feed(
            &mut m,
            500,
            trade(mint, a, true, 100 * e9, 30.0, T0 + 200, 60.0),
            &mut out,
        );
        feed(
            &mut m,
            501,
            trade(mint, a, false, 100 * e9, 20.0, T0 + 210, 45.0),
            &mut out,
        );
        feed(
            &mut m,
            502,
            PumpEvent::Complete(chain::pump::CompleteEvent {
                user: a,
                mint,
                bonding_curve: Pubkey::new_unique(),
                timestamp: T0 + 400,
            }),
            &mut out,
        );
        // a trade after the hour does not count
        feed(
            &mut m,
            9000,
            trade(mint, a, true, e9, 1.0, T0 + 3601, 200.0),
            &mut out,
        );
        assert!(rows(&out, "curve_outcomes").is_empty());
        m.tick((T0 + OUTCOME_SECS) * 1000 + GRACE_MS, &mut out);
        let o = rows(&out, "curve_outcomes");
        assert_eq!(o.len(), 1);
        let o = o[0];
        assert_eq!(o["full_window"], true);
        assert_eq!(o["graduated"], true);
        assert_eq!(o["graduated_after_s"], 400);
        let at15 = o["mcap_at"][1].as_f64().unwrap();
        let peak15 = o["peak_after"][1].as_f64().unwrap();
        assert!(
            peak15 / at15 > 3.8 && peak15 / at15 < 4.0,
            "{at15} → {peak15}"
        );
        // the 900 s snapshot came after the run: its peak is its own market cap
        assert_eq!(o["peak_after"][6], o["mcap_at"][6]);
        assert_eq!(m.coins(), 0, "book dropped");
        assert!(out.tape.is_empty(), "TradesMode::None writes no tape");
    }

    #[test]
    fn stream_gaps_mark_the_coins_alive_during_them() {
        let (mint, dev) = (Pubkey::new_unique(), Pubkey::new_unique());
        let mut m = Micro::new(TradesMode::FirstHour, T0 * 1000);
        let mut out = Out::default();
        feed(&mut m, 1, create(mint, dev), &mut out);
        m.set_down((T0 + 2) * 1000);
        m.tick((T0 + 5) * 1000 + GRACE_MS, &mut out);
        let s5 = rows(&out, "micro")[0].clone();
        assert_eq!(s5["gap_ms"], 3000 + GRACE_MS);
        m.set_up((T0 + 12) * 1000);
        m.tick((T0 + 15) * 1000 + GRACE_MS, &mut out);
        assert_eq!(rows(&out, "micro")[1]["gap_ms"], 10_000);
        // a coin created after the gap is clean; the stop marks open windows incomplete
        let other = Pubkey::new_unique();
        let mut c = create(other, dev);
        if let PumpEvent::Create(e) = &mut c {
            e.timestamp = T0 + 20;
        }
        m.on_tx(2, "sig", (T0 + 20) * 1000, vec![c], false, &mut out);
        let mut fin = Out::default();
        m.finish((T0 + 30) * 1000, &mut fin);
        let o = rows(&fin, "curve_outcomes");
        assert_eq!(o.len(), 2);
        for r in o {
            assert_eq!(r["full_window"], false);
            let want = if r["mint"] == mint.to_string() {
                10_000
            } else {
                0
            };
            assert_eq!(r["gap_ms"], want);
        }
        assert_eq!(rows(&fin, "feed").len(), 1);
        assert_eq!(out.tape.len(), 2, "two creates on the tape");
    }

    #[test]
    fn decodes_a_real_create_and_buy_from_a_log_notification() {
        let tx: Value =
            serde_json::from_str(include_str!("../../chain/tests/fixtures/pump_curve_1.json"))
                .unwrap();
        let tx = if tx.get("result").is_some() {
            &tx["result"]
        } else {
            &tx
        };
        let note = |err: Value| {
            json!({"jsonrpc": "2.0", "method": "logsNotification", "params": {"result": {
                "context": {"slot": tx["slot"]},
                "value": {"signature": tx["transaction"]["signatures"][0], "err": err,
                          "logs": tx["meta"]["logMessages"]}}, "subscription": 7}})
            .to_string()
        };
        let ok = decode_notification(&note(Value::Null), &PUMP_PROGRAM).unwrap();
        assert!(!ok.failed);
        assert_eq!(ok.slot, 452455845);
        let (mut creates, mut trades) = (vec![], vec![]);
        for e in &ok.events {
            match e {
                PumpEvent::Create(c) => creates.push(c.mint),
                PumpEvent::Trade(t) => trades.push(t.mint),
                _ => {}
            }
        }
        assert_eq!(creates.len(), 1);
        assert!(!trades.is_empty());
        assert!(
            trades.iter().all(|m| *m == creates[0]),
            "the creator's first buy"
        );
        let failed = decode_notification(
            &note(json!({"InstructionError": [3, {"Custom": 6002}]})),
            &PUMP_PROGRAM,
        )
        .unwrap();
        assert!(failed.failed && failed.events.is_empty());
        assert!(
            decode_notification(r#"{"jsonrpc":"2.0","result":7,"id":1}"#, &PUMP_PROGRAM).is_none()
        );
    }
}

#[cfg(test)]
mod amm_tests {
    use super::*;

    const T0: i64 = 1_791_500_000;

    /// A swap leaving the pool at `quote_sol` SOL against `base_tokens` million tokens.
    fn swap(
        pool: Pubkey,
        user: Pubkey,
        buy: bool,
        sol: f64,
        ts: i64,
        base_m: f64,
        quote_sol: f64,
    ) -> SwapEventData {
        // the event carries the reserves from before the swap; make "after" land where asked
        let base_after = (base_m * 1e6 * 1e6) as u64;
        let quote_after = (quote_sol * 1e9) as u64;
        let amount = (sol * 1e9) as u64;
        let base_amount = 1_000_000_000u64;
        SwapEventData {
            is_buy: buy,
            timestamp: ts,
            base_amount,
            user_quote_amount: amount,
            quote_amount: amount,
            pool_quote_delta: amount,
            pool_base_token_reserves: if buy {
                base_after + base_amount
            } else {
                base_after - base_amount
            },
            pool_quote_token_reserves: if buy {
                quote_after - amount
            } else {
                quote_after + amount
            },
            lp_fee_basis_points: 20,
            protocol_fee_basis_points: 5,
            pool,
            user,
            protocol_fee_recipient: Pubkey::default(),
            coin_creator: Pubkey::default(),
            coin_creator_fee_basis_points: 5,
            buyback_fee_basis_points: 0,
            virtual_quote_reserves: 0,
        }
    }

    fn rows<'a>(out: &'a Out, kind: &str) -> Vec<&'a Value> {
        out.rows
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, v)| v)
            .collect()
    }

    #[test]
    fn a_graduation_opens_a_pool_book_with_candles_and_an_outcome() {
        let (mint, dev, a, b) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut m = Micro::new(TradesMode::FirstHour, T0 * 1000);
        m.sol_usd = Some(100.0);
        let mut out = Out::default();
        // the create and the graduation of a coin whose curve we saw
        let mut c = super::tests::create(mint, dev);
        if let PumpEvent::Create(e) = &mut c {
            e.timestamp = T0 - 300;
        }
        m.on_tx(1, "s0", (T0 - 300) * 1000, vec![c], false, &mut out);
        m.on_tx(
            2,
            "s1",
            T0 * 1000,
            vec![PumpEvent::Complete(chain::pump::CompleteEvent {
                user: a,
                mint,
                bonding_curve: Pubkey::new_unique(),
                timestamp: T0,
            })],
            false,
            &mut out,
        );
        let g = rows(&out, "graduations");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0]["mint"], mint.to_string());
        assert_eq!(g[0]["creator"], dev.to_string());
        assert_eq!(g[0]["instant"], false);
        assert_eq!(g[0]["sol_usd"], 100.0);
        assert_eq!(g[0]["followed"], true);
        let pool: Pubkey = g[0]["pool"].as_str().unwrap().parse().unwrap();
        assert_eq!(pool, pump_pool_for(&mint, &WSOL_MINT));
        assert_eq!(m.pools(), 1);
        // trades on the pool: 200M tokens against 80 SOL = 400 SOL market cap, then up, then the dev sells
        let mut out = Out::default();
        m.on_amm(
            3,
            "a1",
            T0 * 1000 + 5000,
            vec![swap(pool, a, true, 2.0, T0 + 5, 200.0, 80.0)],
            &mut out,
        );
        m.on_amm(
            4,
            "a2",
            T0 * 1000 + 30_000,
            vec![swap(pool, b, true, 5.0, T0 + 30, 180.0, 90.0)],
            &mut out,
        );
        m.on_amm(
            5,
            "a3",
            T0 * 1000 + 70_000,
            vec![swap(pool, dev, false, 20.0, T0 + 70, 220.0, 60.0)],
            &mut out,
        );
        // a swap on a pool we do not follow is ignored
        m.on_amm(
            6,
            "a4",
            T0 * 1000 + 71_000,
            vec![swap(
                Pubkey::new_unique(),
                a,
                true,
                1.0,
                T0 + 71,
                200.0,
                80.0,
            )],
            &mut out,
        );
        let candles = rows(&out, "candles");
        assert_eq!(
            candles.len(),
            1,
            "the first minute closed when the third trade opened the next"
        );
        let c0 = candles[0];
        assert!(
            (c0["liq_sol"].as_f64().unwrap() - 90.0).abs() < 1e-6,
            "pool SOL after the last swap of the minute"
        );
        assert_eq!(c0["unbought_sell_sol"], 0.0);
        assert_eq!(c0["mint"], mint.to_string());
        assert_eq!(c0["minute"], T0.div_euclid(60) * 60);
        assert_eq!(c0["o"], 400.0);
        assert_eq!(c0["c"], 500.0);
        assert_eq!(c0["h"], 500.0);
        assert_eq!(c0["buys"], 2);
        assert_eq!(c0["buyers"], 2);
        assert_eq!(c0["buy_sol"], 7.0);
        assert_eq!(c0["sol_usd"], 100.0);
        assert_eq!(
            out.tape.iter().filter(|r| r["ev"] == "amm").count(),
            3,
            "followed pools only"
        );
        // the open candle closes by the clock; the outcome at 1 h carries peak and drawdown
        let mut later = Out::default();
        m.tick((T0 + 3600) * 1000 + GRACE_MS, &mut later);
        let c1 = rows(&later, "candles");
        assert_eq!(c1.len(), 1);
        assert_eq!(c1[0]["creator_sold_sol"], 20.0);
        assert_eq!(c1[0]["sellers"], 1);
        let o = rows(&later, "amm_outcomes");
        assert_eq!(o.len(), 1);
        let o = o[0];
        assert_eq!(o["at"], 3600);
        assert_eq!(o["first_mcap_sol"], 400.0);
        // landing: the price the first swap met (its pre-swap reserves)
        let e0 = swap(pool, a, true, 2.0, T0 + 5, 200.0, 80.0);
        let landing = e0.price(6, 9) * 1e9;
        assert!((o["landing_mcap_sol"].as_f64().unwrap() - landing).abs() < 0.01);
        assert_eq!(o["peak_mcap_sol"], 500.0);
        assert!((o["peak_multiple"].as_f64().unwrap() - 500.0 / landing).abs() < 0.01);
        // the first slot's buys: 2 tokens-worth of base (1 token each) of 1 B supply
        assert_eq!(o["insider_share"], 0.0);
        assert_eq!(o["grad_slot_sol"], 2.0);
        // the dev sold without buying on the pool and was not a curve holder in this test
        assert_eq!(o["unbought_sell_sol"], 20.0);
        // landing was 390 (78 SOL against 200 M tokens): 272 is above half of it
        assert_eq!(o["alive"], true);
        assert_eq!(c1[0]["unbought_sellers"], 1);
        assert_eq!(o["peak_after_s"], 30);
        // 500 → 272.7 (60 SOL / 220M tokens)
        let dd = o["max_drawdown"].as_f64().unwrap();
        assert!((dd - (1.0 - 272.727 / 500.0)).abs() < 0.002, "{dd}");
        assert_eq!(o["creator_sold_sol"], 20.0);
        assert_eq!(o["buyers"], 2);
        assert_eq!(o["full_window"], true);
        // the follow ends after a day
        let mut end = Out::default();
        m.tick((T0 + AMM_FOLLOW_SECS) * 1000 + GRACE_MS, &mut end);
        assert_eq!(rows(&end, "amm_outcomes").len(), 2, "6 h and 24 h");
        assert_eq!(m.pools(), 0);
    }

    #[test]
    fn a_coin_priced_in_another_token_is_recorded_but_not_followed() {
        let (mint, dev, quote) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut m = Micro::new(TradesMode::None, T0 * 1000);
        let mut out = Out::default();
        m.on_tx(
            1,
            "s0",
            T0 * 1000,
            vec![super::tests::create(mint, dev)],
            false,
            &mut out,
        );
        let mut t = super::tests::trade(mint, dev, true, 1_000_000_000_000, 1.0, T0, 31.0);
        if let PumpEvent::Trade(e) = &mut t {
            e.quote_mint = Some(quote);
        }
        m.on_tx(1, "s1", T0 * 1000, vec![t], false, &mut out);
        m.on_tx(
            2,
            "s2",
            (T0 + 60) * 1000,
            vec![PumpEvent::Complete(chain::pump::CompleteEvent {
                user: dev,
                mint,
                bonding_curve: Pubkey::new_unique(),
                timestamp: T0 + 60,
            })],
            false,
            &mut out,
        );
        let g = rows(&out, "graduations");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0]["followed"], false);
        assert_eq!(g[0]["quote"], quote.to_string());
        assert_eq!(g[0]["pool"], pump_pool_for(&mint, &quote).to_string());
        assert_eq!(m.pools(), 0);
    }

    #[test]
    fn a_pumpswap_notification_decodes_on_the_amm_stream_only() {
        let tx: Value =
            serde_json::from_str(include_str!("../../chain/tests/fixtures/pumpswap_0.json"))
                .unwrap();
        let tx = if tx.get("result").is_some() {
            &tx["result"]
        } else {
            &tx
        };
        let note = json!({"jsonrpc": "2.0", "method": "logsNotification", "params": {"result": {
            "context": {"slot": tx["slot"]},
            "value": {"signature": tx["transaction"]["signatures"][0], "err": null, "logs": tx["meta"]["logMessages"]}},
            "subscription": 9}})
        .to_string();
        let amm = decode_notification(&note, &PUMP_AMM_PROGRAM).unwrap();
        assert_eq!(amm.program, PUMP_AMM_PROGRAM);
        assert!(!amm.amm.is_empty(), "a swap");
        assert!(amm.events.is_empty());
        let pump = decode_notification(&note, &PUMP_PROGRAM).unwrap();
        assert!(
            pump.amm.is_empty(),
            "the pump stream never decodes AMM events"
        );
    }
}
