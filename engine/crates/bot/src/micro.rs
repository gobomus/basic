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
//! Holdings come from curve trades only. Tokens moved by plain transfers are not seen;
//! in a coin's first minutes they are rare, and a balance that would go negative counts
//! as zero.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use chain::consts::PUMP_PROGRAM;
use chain::pump::{CreateEvent, PumpEvent, TradeEvent};
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
    fn snapshot(&mut self, i: usize, gap_ms: i64) -> Value {
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
        if i == 0 {
            row["symbol"] = json!(self.symbol);
            row["name"] = json!(self.name);
            row["creator"] = json!(self.creator.to_string());
            row["create_slot"] = json!(self.create_slot);
            row["pump_suffix"] = json!(self.pump_suffix);
            row["mayhem"] = json!(self.mayhem);
            row["seen_delay_ms"] = json!(self.seen_ms - self.created_ts * 1000);
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
    tx: u64,
    failed: u64,
    truncated: u64,
    trades: u64,
    creates: u64,
    completes: u64,
    lags_ms: Vec<i64>,
}

/// The books of all coins in their first hour, and the stream's health.
pub struct Micro {
    books: HashMap<Pubkey, Book>,
    mode: TradesMode,
    minute: Minute,
    minute_start: i64,
    down_since: Option<i64>,
    pub disconnects: u64,
    seq: u64,
}

impl Micro {
    pub fn new(mode: TradesMode, now: i64) -> Self {
        Self {
            books: HashMap::new(),
            mode,
            minute: Minute::default(),
            minute_start: now,
            down_since: None,
            disconnects: 0,
            seq: 0,
        }
    }

    pub fn coins(&self) -> usize {
        self.books.len()
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
                            "rsol": e.real_sol_reserves,
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
                }
            }
        }
    }

    /// Snapshots and outcomes that are due by the clock, and the minute's feed row.
    pub fn tick(&mut self, now: i64, out: &mut Out) {
        let mut done = vec![];
        let down = self.down_since;
        for (mint, b) in self.books.iter_mut() {
            let gap = gap_of(b, down, now);
            while b.next < SNAPSHOTS.len() && now >= b.cutoff(b.next) * 1000 + GRACE_MS {
                let mut row = b.snapshot(b.next, gap);
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
        out.rows.push(("feed", self.feed_row(now)));
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
    context: Ctx,
    value: LogValue,
}
#[derive(Deserialize)]
struct Ctx {
    slot: u64,
}
#[derive(Deserialize)]
struct LogValue {
    signature: String,
    err: Option<Value>,
    logs: Vec<String>,
}

/// One transaction from the stream.
pub struct StreamTx {
    pub slot: u64,
    pub sig: String,
    pub events: Vec<PumpEvent>,
    /// The node cut the logs short (events after the cut are missing).
    pub truncated: bool,
    pub failed: bool,
}

/// The pump events of one `logsNotification` (`None` for any other message).
pub fn decode_notification(text: &str) -> Option<StreamTx> {
    let n: Notification = serde_json::from_str(text).ok()?;
    let r = n.params?.result;
    let failed = r.value.err.is_some();
    let events = if failed {
        vec![]
    } else {
        chain::detect::program_data_in_logs(&r.value.logs, &PUMP_PROGRAM)
            .iter()
            .filter_map(|b| chain::pump::decode_event(b))
            .collect()
    };
    Some(StreamTx {
        slot: r.context.slot,
        sig: r.value.signature,
        events,
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
async fn reader(id: usize, url: String, tx: tokio::sync::mpsc::Sender<Feed>) {
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
                       "params": [{"mentions": [PUMP_PROGRAM.to_string()]}, {"commitment": "processed"}]})
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
                    match decode_notification(&text) {
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
    mut stop: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Feed>(50_000);
    let mut readers = vec![];
    for (i, url) in urls
        .iter()
        .flat_map(|u| std::iter::repeat_n(u, conns.max(1)))
        .enumerate()
    {
        readers.push(tokio::spawn(reader(i, url.clone(), tx.clone())));
    }
    drop(tx);
    let mut up = vec![false; readers.len()];
    let mut micro = Micro::new(mode, now_ms());
    micro.set_down(now_ms());
    micro.disconnects = 0;
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
                    row["recycles"] = json!(recycles);
                    row["duplicates"] = json!(dupes);
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
                let live = up.iter().filter(|u| **u).count();
                match f {
                    Feed::Up(i) => {
                        if !up[i] {
                            up[i] = true;
                            if live == 0 {
                                micro.set_up(now_ms());
                                eprintln!("pump stream up ({} connection(s) to {})", up.len(), urls.join(", "));
                            }
                        }
                    }
                    Feed::Down(i, reason) => {
                        if up[i] {
                            up[i] = false;
                            recycles += 1;
                            if live == 1 {
                                micro.set_down(now_ms());
                                eprintln!("pump stream: every connection down ({reason})");
                            }
                        } else if recycles == 0 || live == 0 {
                            eprintln!("pump stream connection {i}: {reason}");
                        }
                    }
                    Feed::Tx(recv, t) => {
                        let h = sig_hash(&t.sig);
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
                        micro.on_tx(t.slot, &t.sig, recv, t.events, t.truncated, &mut out);
                        write(out, &mut totals, &mut trades, live, recycles, dupes)?;
                    }
                }
            }
            _ = tick.tick() => {
                let now = now_ms();
                let live = up.iter().filter(|u| **u).count();
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
                        "pump stream: {} creates, {} trades taped, {} outcomes, {} coins open · {live}/{} connections up, {recycles} recycled, {} gaps",
                        totals.0, totals.1, totals.2, micro.coins(), up.len(), micro.disconnects
                    );
                }
            }
        }
    }
    for r in readers {
        r.abort();
    }
    let live = up.iter().filter(|u| **u).count();
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

    fn create(mint: Pubkey, creator: Pubkey) -> PumpEvent {
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
    fn trade(
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
        let ok = decode_notification(&note(Value::Null)).unwrap();
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
        let failed =
            decode_notification(&note(json!({"InstructionError": [3, {"Custom": 6002}]}))).unwrap();
        assert!(failed.failed && failed.events.is_empty());
        assert!(decode_notification(r#"{"jsonrpc":"2.0","result":7,"id":1}"#).is_none());
    }
}
