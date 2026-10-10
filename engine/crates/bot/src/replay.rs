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
    /// Market cap in SOL at block time `ts`.
    pub fn mcap_sol_at(&self, ts: i64) -> f64 {
        self.at(ts).mcap_sol()
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
    /// end of the tape for this coin
    end: i64,
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
        end,
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
    /// A sell landing at `fill_ts`: what it returns, and whether the coin had graduated
    /// by then (then it sells into the pool at the final curve price).
    fn close(&self, c: &Coin, fill_ts: i64) -> (u64, bool) {
        let fill_ts = fill_ts.min(self.end);
        match c.graduated_ts.filter(|g| *g <= fill_ts) {
            Some(g) => {
                let pool = pool_after_graduation(with_us(c.at(g).state(), self.input, self.tokens));
                (sell_quote_for_tokens(&pool, self.tokens, AMM_FEE_BPS), true)
            }
            None => (self.value(&c.at(fill_ts)), false),
        }
    }
}

/// Buy with a fill at `entry_ts` and hold: the position's return at the end of the
/// hour (or in the pool, if the coin graduated) and at its peak on the way, both on
/// what we paid (size plus the transaction fees of the buy and one sell).
pub fn path_from(c: &Coin, entry_ts: i64, k: &Costs) -> Option<(f64, f64)> {
    let pos = open(c, entry_ts, k)?;
    let cost = (k.size_lamports + 2 * k.tx_lamports) as f64;
    let until = c.graduated_ts.map_or(pos.end, |g| g.min(pos.end));
    let first = c.ticks.partition_point(|x| x.ts <= entry_ts);
    let mut peak = 0u64;
    for x in &c.ticks[first..] {
        if x.ts > until {
            break;
        }
        peak = peak.max(pos.value(x));
    }
    let (last, _) = pos.close(c, pos.end);
    let ret = |v: u64| v as f64 / cost - 1.0;
    Some((ret(last), ret(peak.max(last))))
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
        let (last, peak) = path_from(&c, T0 + 19, &k(4)).unwrap();
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
        let (last, peak) = path_from(&flat, T0 + 17, &k(2)).unwrap();
        assert!(last < -0.02 && last > -0.04, "{last}");
        assert_eq!(peak, last, "nothing traded after us: the peak is the end");
    }

    #[test]
    fn graduation_sells_into_the_pool_and_late_or_post_graduation_entries_are_none() {
        let up = coin(&[(5, 40.0), (100, 90.0), (200, 115.0)], Some(200));
        let (last, peak) = path_from(&up, T0 + 17, &k(2)).unwrap();
        assert!(last > 3.0, "{last}");
        assert!(peak >= last);
        // already graduated at the fill: no trade; after the window: no trade
        assert!(path_from(&coin(&[(5, 115.0)], Some(10)), T0 + 17, &k(2)).is_none());
        assert!(path_from(&up, T0 + WINDOW_SECS, &k(0)).is_none());
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
