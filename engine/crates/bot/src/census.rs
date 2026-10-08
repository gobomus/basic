//! `copybot census`: the tape. Every launch, every coin's state at fixed checkpoints
//! of its life, and what is trending, recorded from free sources at receipt time.
//!
//! Sources (all free, no keys):
//! * PumpPortal WebSocket: Pump.fun creations and migrations as they happen.
//! * Jupiter tokens API: the newest tokens of every launchpad (polled), batch stats
//!   for up to 100 coins per call (holders, traders, buy/sell flow, market cap,
//!   liquidity, top-holder share, dev balance), and the trending lists.
//! * The coin's Pump.fun bonding curve read straight from the chain (exact SOL in the
//!   curve, graduated or not).
//! * DexScreener boosts, profiles and community takeovers; Pump.fun live streams.
//!
//! Output, one folder per UTC day: `launches.jsonl`, `migrations.jsonl`,
//! `checkpoints.jsonl`, `trending.jsonl`, and `raw-<hour>.jsonl.gz` with every
//! response body as received plus its SHA-256 (rows point at it). Labels and the daily
//! top lists come from `copybot census-report`. Pending checkpoints are saved to
//! `state.json`, so a later run (e.g. the next GitHub Actions run) picks them up.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chain::pda;
use chain::pump::BondingCurve;
use chain::rpc::Rpc;
use chain::solana_sdk::pubkey::Pubkey;
use futures::{SinkExt, StreamExt};
use reqwest_websocket::{Message, RequestBuilderExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

/// Seconds after creation at which a coin's state is recorded.
pub const CHECKPOINTS: [i64; 11] = [
    15, 30, 60, 120, 300, 900, 1800, 3600, 10_800, 21_600, 86_400,
];
/// From this checkpoint on, a coin with no traders and at most this many holders is dead.
const DEAD_FROM_SECS: i64 = 300;
const DEAD_MAX_HOLDERS: u64 = 3;
/// Coins tracked at once (oldest dropped beyond this).
const MAX_TRACKED: usize = 60_000;
const JUP: &str = "https://lite-api.jup.ag/tokens/v2";
const PUMPPORTAL: &str = "wss://pumpportal.fun/api/data";

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

// ------------------------------------------------------------------ storage

/// Append-only day files under `dir/<YYYY-MM-DD>/`.
#[derive(Clone)]
pub struct Tape {
    dir: PathBuf,
    raw: RawMode,
}

impl Tape {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            raw: RawMode::All,
        }
    }

    pub fn with_raw(mut self, raw: RawMode) -> Self {
        self.raw = raw;
        self
    }

    fn day_dir(&self, ms: i64) -> anyhow::Result<PathBuf> {
        let d = self.dir.join(day_of(ms));
        std::fs::create_dir_all(&d)?;
        Ok(d)
    }

    pub fn row(&self, kind: &str, ts_ms: i64, v: &Value) -> anyhow::Result<()> {
        let p = self.day_dir(ts_ms)?.join(format!("{kind}.jsonl"));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)?;
        writeln!(f, "{v}")?;
        Ok(())
    }

    /// Store a response body exactly as received; returns its SHA-256. Each record is
    /// its own gzip member, so a crash never corrupts what was already written.
    pub fn raw(&self, ts_ms: i64, source: &str, body: &str) -> anyhow::Result<String> {
        let sha = sha256_hex(body.as_bytes());
        let keep = match self.raw {
            RawMode::All => true,
            RawMode::Lists => source != "jup_search",
            RawMode::None => false,
        };
        if !keep {
            return Ok(sha);
        }
        let hour = chrono::DateTime::from_timestamp_millis(ts_ms)
            .unwrap_or_default()
            .format("%H")
            .to_string();
        let p = self.day_dir(ts_ms)?.join(format!("raw-{hour}.jsonl.gz"));
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)?;
        let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        writeln!(
            gz,
            "{}",
            json!({"ts": ts_ms, "source": source, "sha256": sha, "body": body})
        )?;
        gz.finish()?;
        Ok(sha)
    }
}

// ------------------------------------------------------------------ tracking

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tracked {
    pub created_ms: i64,
    /// Index into CHECKPOINTS of the next checkpoint to take.
    pub next: usize,
    pub source: String,
    pub launchpad: String,
    /// The coin has a Pump.fun bonding curve we can read from the chain.
    pub pump_curve: bool,
}

/// Coins whose checkpoints are pending, ordered by due time.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Schedule {
    pub coins: HashMap<String, Tracked>,
}

impl Schedule {
    /// Start tracking a new coin; false if it is already known.
    pub fn add(&mut self, mint: &str, t: Tracked) -> bool {
        if self.coins.contains_key(mint) {
            return false;
        }
        if self.coins.len() >= MAX_TRACKED {
            if let Some(oldest) = self
                .coins
                .iter()
                .min_by_key(|(_, c)| c.created_ms)
                .map(|(m, _)| m.clone())
            {
                self.coins.remove(&oldest);
            }
        }
        self.coins.insert(mint.to_string(), t);
        true
    }

    pub fn due_ms(t: &Tracked) -> Option<i64> {
        CHECKPOINTS.get(t.next).map(|s| t.created_ms + s * 1000)
    }

    /// Up to `max` coins whose next checkpoint is due at `now`, most overdue first.
    pub fn due(&self, now: i64, max: usize) -> Vec<(String, usize, i64)> {
        let mut v: Vec<(String, usize, i64)> = self
            .coins
            .iter()
            .filter_map(|(m, t)| {
                Self::due_ms(t)
                    .filter(|d| *d <= now)
                    .map(|d| (m.clone(), t.next, d))
            })
            .collect();
        v.sort_by_key(|x| x.2);
        v.truncate(max);
        v
    }

    /// The checkpoint was taken: move on, or stop when the coin is done or dead.
    pub fn advance(&mut self, mint: &str, dead: bool) {
        let stop = match self.coins.get_mut(mint) {
            Some(t) => {
                t.next += 1;
                dead || t.next >= CHECKPOINTS.len()
            }
            None => false,
        };
        if stop {
            self.coins.remove(mint);
        }
    }

    pub fn save(&self, p: &Path) -> anyhow::Result<()> {
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn load(p: &Path) -> Self {
        std::fs::read(p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

// ------------------------------------------------------------------ extraction

fn f(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn ts_of(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
}

/// The compact row kept for one coin at one checkpoint (all values as Jupiter reports
/// them; the raw body is in the raw archive).
pub fn checkpoint_row(tok: &Value) -> Value {
    let s5 = &tok["stats5m"];
    let s1h = &tok["stats1h"];
    let a = &tok["audit"];
    json!({
        "holders": tok["holderCount"],
        "mcap": f(&tok["mcap"]),
        "fdv": f(&tok["fdv"]),
        "liquidity": f(&tok["liquidity"]),
        "price_usd": f(&tok["usdPrice"]),
        "organic_score": f(&tok["organicScore"]),
        "buys_5m": s5["numBuys"], "sells_5m": s5["numSells"],
        "buy_vol_5m": f(&s5["buyVolume"]), "sell_vol_5m": f(&s5["sellVolume"]),
        "organic_buy_vol_5m": f(&s5["buyOrganicVolume"]),
        "traders_5m": s5["numTraders"], "net_buyers_5m": s5["numNetBuyers"],
        "organic_buyers_5m": s5["numOrganicBuyers"],
        "price_change_5m": f(&s5["priceChange"]), "holder_change_5m": f(&s5["holderChange"]),
        "buy_vol_1h": f(&s1h["buyVolume"]), "sell_vol_1h": f(&s1h["sellVolume"]),
        "traders_1h": s1h["numTraders"], "price_change_1h": f(&s1h["priceChange"]),
        "top_holders_pct": f(&a["topHoldersPercentage"]),
        "dev_balance_pct": f(&a["devBalancePercentage"]),
        "dev_mints": a["devMints"],
        "mint_authority_disabled": a["mintAuthorityDisabled"],
        "freeze_authority_disabled": a["freezeAuthorityDisabled"],
        "graduated_pool": tok["graduatedPool"],
        "graduated_at": tok["graduatedAt"],
    })
}

/// No trading and (almost) no holders after the first minutes: stop following it.
pub fn looks_dead(cp_secs: i64, row: &Value) -> bool {
    cp_secs >= DEAD_FROM_SECS
        && row["traders_5m"].as_u64().unwrap_or(0) == 0
        && row["holders"].as_u64().unwrap_or(0) <= DEAD_MAX_HOLDERS
}

fn is_pump_mint(mint: &str, launchpad: &str) -> bool {
    launchpad == "pump.fun" || mint.ends_with("pump")
}

// ------------------------------------------------------------------ recorder

pub struct CensusArgs {
    pub out: PathBuf,
    pub minutes: Option<u64>,
    pub rpc_url: Option<String>,
    /// Seconds between trending captures.
    pub trending_secs: u64,
    pub raw: RawMode,
    /// WebSocket endpoints for the pump program's logs (empty: no trade stream), and
    /// how many connections to keep to each.
    pub trades_ws: Vec<String>,
    pub trades_ws_conns: usize,
    pub trades: crate::micro::TradesMode,
}

/// Which response bodies go to the raw archive. Checkpoint lookups are ~95% of the
/// volume (~0.5 GB a day gzipped); their rows keep the body's SHA-256 either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RawMode {
    /// Every body, checkpoint lookups included.
    All,
    /// Trending/attention lists and the newest-token polls (default).
    Lists,
    /// None.
    None,
}

#[derive(Default)]
struct Counters {
    launches_pp: u64,
    launches_jup: u64,
    migrations: u64,
    checkpoints: u64,
    late_checkpoints: u64,
    not_found: u64,
    dead: u64,
    trending_captures: u64,
    errors: u64,
}

enum PpEvent {
    Create { text: String, v: Value },
    Migrate { text: String, v: Value },
    Status(String),
}

async fn pumpportal(tx: mpsc::Sender<PpEvent>) {
    let mut backoff = 1u64;
    loop {
        let res: anyhow::Result<()> = async {
            let client = reqwest::Client::builder()
                .http1_only()
                .connect_timeout(Duration::from_secs(10))
                .build()?;
            let mut ws = client
                .get(PUMPPORTAL)
                .upgrade()
                .send()
                .await?
                .into_websocket()
                .await?;
            ws.send(Message::Text(
                json!({"method": "subscribeNewToken"}).to_string(),
            ))
            .await?;
            ws.send(Message::Text(
                json!({"method": "subscribeMigration"}).to_string(),
            ))
            .await?;
            let _ = tx
                .send(PpEvent::Status("pumpportal connected".into()))
                .await;
            let mut last = std::time::Instant::now();
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(60), ws.next()).await;
                let msg = match msg {
                    Ok(Some(m)) => m?,
                    Ok(None) => anyhow::bail!("closed"),
                    Err(_) => anyhow::bail!("no message for 60 s"),
                };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Ping(p) => {
                        ws.send(Message::Pong(p)).await?;
                        continue;
                    }
                    Message::Close { reason, .. } => anyhow::bail!("closed by server: {reason}"),
                    _ => continue,
                };
                if last.elapsed() > Duration::from_secs(1) {
                    backoff = 1;
                    last = std::time::Instant::now();
                }
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let ev = match v["txType"].as_str() {
                    Some("create") => PpEvent::Create { text, v },
                    Some("migrate") => PpEvent::Migrate { text, v },
                    _ => continue,
                };
                if tx.send(ev).await.is_err() {
                    return Ok(());
                }
            }
        }
        .await;
        if let Err(e) = res {
            let _ = tx
                .send(PpEvent::Status(format!("pumpportal: {e}; reconnecting")))
                .await;
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

struct Http {
    c: reqwest::Client,
}

impl Http {
    fn new() -> Self {
        Self {
            c: reqwest::Client::builder()
                .timeout(Duration::from_secs(25))
                .user_agent("copybot-census/0.1")
                .build()
                .expect("http client"),
        }
    }
    /// GET returning the body text; the caller archives it before parsing.
    async fn get(&self, url: &str) -> anyhow::Result<String> {
        let r = self.c.get(url).send().await?;
        let status = r.status();
        let body = r.text().await?;
        anyhow::ensure!(
            status.is_success(),
            "{url}: HTTP {status}: {}",
            body.chars().take(120).collect::<String>()
        );
        Ok(body)
    }
}

/// The lists captured every few minutes: (name, url, path to the token address in each row).
pub fn trending_sources() -> Vec<(&'static str, String, &'static str)> {
    let mut v = vec![];
    for (name, path) in [
        ("jup_trending_5m", "toptrending/5m?limit=100"),
        ("jup_trending_1h", "toptrending/1h?limit=100"),
        ("jup_trending_24h", "toptrending/24h?limit=100"),
        ("jup_organic_1h", "toporganicscore/1h?limit=100"),
        ("jup_traded_1h", "toptraded/1h?limit=100"),
    ] {
        v.push((name, format!("{JUP}/{path}"), "id"));
    }
    v.push((
        "dex_boosts_top",
        "https://api.dexscreener.com/token-boosts/top/v1".into(),
        "tokenAddress",
    ));
    v.push((
        "dex_boosts_latest",
        "https://api.dexscreener.com/token-boosts/latest/v1".into(),
        "tokenAddress",
    ));
    v.push((
        "dex_profiles_latest",
        "https://api.dexscreener.com/token-profiles/latest/v1".into(),
        "tokenAddress",
    ));
    v.push((
        "dex_takeovers_latest",
        "https://api.dexscreener.com/community-takeovers/latest/v1".into(),
        "tokenAddress",
    ));
    v.push((
        "pump_live",
        "https://frontend-api-v3.pump.fun/coins/currently-live?limit=100&offset=0&includeNsfw=false"
            .into(),
        "mint",
    ));
    v
}

/// One trending/attention row: rank in its list plus the metrics the list carries.
pub fn trending_row(list: &str, rank: usize, item: &Value, key: &str) -> Option<Value> {
    let mint = item[key].as_str()?.to_string();
    if item["chainId"].as_str().is_some_and(|c| c != "solana") {
        return None;
    }
    let mut row = json!({"list": list, "rank": rank + 1, "mint": mint});
    if list.starts_with("jup_") {
        let r = checkpoint_row(item);
        for (k, v) in r.as_object().into_iter().flatten() {
            row[k] = v.clone();
        }
        row["symbol"] = item["symbol"].clone();
        row["launchpad"] = item["launchpad"].clone();
        row["created_ms"] =
            json!(ts_of(&item["createdAt"]).or_else(|| ts_of(&item["firstPool"]["createdAt"])));
    } else if list == "pump_live" {
        row["symbol"] = item["symbol"].clone();
        row["mcap"] = item["usd_market_cap"].clone();
        row["participants"] = item["num_participants"].clone();
        row["created_ms"] = item["created_timestamp"].clone();
        row["complete"] = item["complete"].clone();
    } else {
        row["boost_amount"] = item["totalAmount"].clone();
        row["amount"] = item["amount"].clone();
    }
    Some(row)
}

pub async fn run(args: CensusArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out)?;
    let tape = Tape::new(&args.out).with_raw(args.raw);
    let state_path = args.out.join("state.json");
    let mut sched = Schedule::load(&state_path);
    let resumed = sched.coins.len();
    let http = Http::new();
    let rpc = args.rpc_url.clone().map(Rpc::new);
    let mut seen_recent: HashSet<String> = sched.coins.keys().cloned().collect();
    let mut c = Counters::default();
    let (pp_tx, mut pp_rx) = mpsc::channel::<PpEvent>(10_000);
    let pp = tokio::spawn(pumpportal(pp_tx));
    // every create and trade on the pump program, for the first-minute books
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stream = (!args.trades_ws.is_empty()).then(|| {
        tokio::spawn(crate::micro::run(
            args.trades_ws.clone(),
            args.trades_ws_conns,
            tape.clone(),
            args.out.clone(),
            args.trades,
            stop_rx,
        ))
    });
    let deadline = args
        .minutes
        .map(|m| tokio::time::Instant::now() + Duration::from_secs(m * 60));
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut last_recent = 0i64;
    let mut last_trending = 0i64;
    let mut last_save = now_ms();
    let mut last_log = now_ms();
    eprintln!(
        "census → {} (resumed {resumed} coins with pending checkpoints){}",
        args.out.display(),
        if rpc.is_some() {
            ""
        } else {
            "; no RPC_URL: curve state is not read"
        }
    );
    let stop = shutdown();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => break,
            ev = pp_rx.recv() => {
                let Some(ev) = ev else { break };
                let ts = now_ms();
                match ev {
                    PpEvent::Create { text, v } => {
                        let Some(mint) = v["mint"].as_str().map(String::from) else { continue };
                        let pool = v["pool"].as_str().unwrap_or("pump").to_string();
                        let lp = if pool == "pump" { "pump.fun".to_string() } else { pool };
                        tape.row("launches", ts, &json!({
                            "ts": ts, "source": "pumpportal", "mint": mint, "launchpad": lp,
                            "creator": v["traderPublicKey"], "signature": v["signature"],
                            "initial_buy_sol": v["solAmount"], "mcap_sol": v["marketCapSol"],
                            "name": v["name"], "symbol": v["symbol"], "uri": v["uri"],
                            "mayhem": v["is_mayhem_mode"], "sha256": sha256_hex(text.as_bytes()), "raw": text,
                        }))?;
                        c.launches_pp += 1;
                        seen_recent.insert(mint.clone());
                        sched.add(&mint, Tracked {
                            created_ms: ts, next: 0, source: "pumpportal".into(),
                            pump_curve: is_pump_mint(&mint, &lp), launchpad: lp,
                        });
                    }
                    PpEvent::Migrate { text, v } => {
                        c.migrations += 1;
                        tape.row("migrations", ts, &json!({
                            "ts": ts, "source": "pumpportal", "mint": v["mint"], "pool": v["pool"],
                            "signature": v["signature"], "sha256": sha256_hex(text.as_bytes()), "raw": text,
                        }))?;
                    }
                    PpEvent::Status(s) => eprintln!("{s}"),
                }
            }
            _ = tick.tick() => {
                let now = now_ms();
                // newest tokens of every launchpad (PumpPortal only covers Pump.fun)
                if now - last_recent >= 10_000 {
                    last_recent = now;
                    match http.get(&format!("{JUP}/recent")).await {
                        Ok(body) => {
                            let sha = tape.raw(now, "jup_recent", &body)?;
                            let toks: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
                            for t in &toks {
                                let Some(mint) = t["id"].as_str() else { continue };
                                if !seen_recent.insert(mint.to_string()) {
                                    continue;
                                }
                                let created = ts_of(&t["createdAt"]).or_else(|| ts_of(&t["firstPool"]["createdAt"])).unwrap_or(now);
                                let lp = t["launchpad"].as_str().unwrap_or("-").to_string();
                                let mut row = json!({
                                    "ts": now, "source": "jupiter", "mint": mint, "launchpad": lp,
                                    "creator": t["dev"], "created_ms": created, "symbol": t["symbol"], "name": t["name"],
                                    "raw_sha256": sha,
                                });
                                row["first_seen"] = checkpoint_row(t);
                                tape.row("launches", now, &row)?;
                                c.launches_jup += 1;
                                // coins already hours old when first seen are not launches
                                if now - created < 120_000 {
                                    let mut tr = Tracked { created_ms: created, next: 0, source: "jupiter".into(), pump_curve: is_pump_mint(mint, &lp), launchpad: lp };
                                    while Schedule::due_ms(&tr).is_some_and(|d| d < now - 30_000) { tr.next += 1; }
                                    sched.add(mint, tr);
                                }
                            }
                            if seen_recent.len() > 200_000 { seen_recent.clear(); }
                        }
                        Err(e) => { c.errors += 1; tracing::debug!("jupiter recent: {e}"); }
                    }
                }
                // checkpoints due
                let due = sched.due(now, 100);
                if !due.is_empty() {
                    if let Err(e) = take_checkpoints(&tape, &http, rpc.as_ref(), &mut sched, &due, &mut c).await {
                        c.errors += 1;
                        eprintln!("checkpoints: {e}");
                    }
                }
                // trending and attention lists
                if now - last_trending >= args.trending_secs as i64 * 1000 {
                    last_trending = now;
                    for (name, url, key) in trending_sources() {
                        match http.get(&url).await {
                            Ok(body) => {
                                let ts = now_ms();
                                let sha = tape.raw(ts, name, &body)?;
                                let items: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
                                for (i, it) in items.iter().enumerate() {
                                    if let Some(mut row) = trending_row(name, i, it, key) {
                                        row["ts"] = json!(ts);
                                        row["raw_sha256"] = json!(sha);
                                        tape.row("trending", ts, &row)?;
                                    }
                                }
                                c.trending_captures += 1;
                            }
                            Err(e) => { c.errors += 1; tracing::debug!("{name}: {e}"); }
                        }
                    }
                }
                if now - last_save > 300_000 {
                    last_save = now;
                    sched.save(&state_path)?;
                }
                if now - last_log > 60_000 {
                    last_log = now;
                    eprintln!(
                        "launches {} pumpportal + {} jupiter · migrations {} · checkpoints {} ({} late, {} not indexed yet) · dead {} · tracked {} · trending captures {} · errors {}",
                        c.launches_pp, c.launches_jup, c.migrations, c.checkpoints, c.late_checkpoints, c.not_found, c.dead, sched.coins.len(), c.trending_captures, c.errors
                    );
                }
            }
        }
    }
    pp.abort();
    sched.save(&state_path)?;
    if let Some(h) = stream {
        let _ = stop_tx.send(true);
        match tokio::time::timeout(Duration::from_secs(30), h).await {
            Ok(Ok(Err(e))) => eprintln!("pump stream: {e}"),
            Ok(Err(e)) => eprintln!("pump stream task: {e}"),
            Err(_) => eprintln!("pump stream did not stop within 30 s"),
            Ok(Ok(Ok(()))) => {}
        }
    }
    eprintln!(
        "census stopped: {} launches, {} checkpoints, {} trending captures; {} coins pending in {}",
        c.launches_pp + c.launches_jup,
        c.checkpoints,
        c.trending_captures,
        sched.coins.len(),
        state_path.display()
    );
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn take_checkpoints(
    tape: &Tape,
    http: &Http,
    rpc: Option<&Rpc>,
    sched: &mut Schedule,
    due: &[(String, usize, i64)],
    c: &mut Counters,
) -> anyhow::Result<()> {
    let ts = now_ms();
    let mints: Vec<&str> = due.iter().map(|d| d.0.as_str()).collect();
    let body = http
        .get(&format!("{JUP}/search?query={}", mints.join(",")))
        .await?;
    let sha = tape.raw(ts, "jup_search", &body)?;
    let toks: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
    let by_mint: HashMap<&str, &Value> = toks
        .iter()
        .filter_map(|t| t["id"].as_str().map(|m| (m, t)))
        .collect();
    // exact curve state for Pump.fun coins, one RPC call for all of them
    let mut curves: HashMap<String, (u64, bool)> = HashMap::new();
    if let Some(rpc) = rpc {
        let pump: Vec<(String, Pubkey)> = due
            .iter()
            .filter(|d| sched.coins.get(&d.0).is_some_and(|t| t.pump_curve))
            .filter_map(|d| {
                d.0.parse::<Pubkey>()
                    .ok()
                    .map(|pk| (d.0.clone(), pda::bonding_curve(&pk)))
            })
            .collect();
        if !pump.is_empty() {
            let keys: Vec<Pubkey> = pump.iter().map(|p| p.1).collect();
            if let Ok((_, accs)) = rpc.accounts_at_fast(&keys).await {
                for ((m, _), a) in pump.iter().zip(accs) {
                    if let Some(bc) = a.and_then(|a| BondingCurve::decode(&a.data).ok()) {
                        curves.insert(m.clone(), (bc.state.real_quote_reserves, bc.complete));
                    }
                }
            }
        }
    }
    for (mint, idx, due_ms) in due {
        let Some(t) = sched.coins.get(mint) else {
            continue;
        };
        let cp = CHECKPOINTS[*idx];
        let late = ts - due_ms;
        let mut row = match by_mint.get(mint.as_str()) {
            Some(tok) => checkpoint_row(tok),
            None => json!({"found": false}),
        };
        if row["found"] == false {
            c.not_found += 1;
        }
        if let Some((sol, complete)) = curves.get(mint) {
            row["curve_sol"] = json!(*sol as f64 / 1e9);
            row["curve_complete"] = json!(complete);
        }
        let dead = looks_dead(cp, &row);
        row["ts"] = json!(ts);
        row["mint"] = json!(mint);
        row["cp"] = json!(cp);
        row["age_ms"] = json!(ts - t.created_ms);
        row["late_ms"] = json!(late);
        row["launchpad"] = json!(t.launchpad);
        row["raw_sha256"] = json!(sha);
        if dead {
            row["stop"] = json!("dead");
            c.dead += 1;
        }
        tape.row("checkpoints", ts, &row)?;
        c.checkpoints += 1;
        if late > 10_000 {
            c.late_checkpoints += 1;
        }
        sched.advance(mint, dead);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(created: i64) -> Tracked {
        Tracked {
            created_ms: created,
            next: 0,
            source: "t".into(),
            launchpad: "pump.fun".into(),
            pump_curve: true,
        }
    }

    #[test]
    fn checkpoints_come_due_in_order_and_stop_when_done_or_dead() {
        let mut s = Schedule::default();
        assert!(s.add("A", tr(0)));
        assert!(!s.add("A", tr(0)), "known coins are not re-added");
        s.add("B", tr(5_000));
        assert!(s.due(14_999, 10).is_empty());
        let d = s.due(19_999, 10);
        assert_eq!(d, vec![("A".to_string(), 0, 15_000)]);
        s.advance("A", false);
        assert_eq!(Schedule::due_ms(&s.coins["A"]), Some(30_000));
        let d = s.due(40_000, 10);
        assert_eq!(
            d.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            vec!["B", "A"],
            "most overdue first"
        );
        s.advance("B", true);
        assert!(!s.coins.contains_key("B"), "dead coins stop");
        for _ in 1..CHECKPOINTS.len() {
            s.advance("A", false);
        }
        assert!(!s.coins.contains_key("A"), "done after the 24 h checkpoint");
    }

    #[test]
    fn state_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let mut s = Schedule::default();
        s.add("A", tr(1));
        s.advance("A", false);
        s.save(&p).unwrap();
        let back = Schedule::load(&p);
        assert_eq!(back.coins["A"].next, 1);
        assert!(Schedule::load(&dir.path().join("missing.json"))
            .coins
            .is_empty());
    }

    #[test]
    fn rows_and_dead_coins() {
        let tok = json!({
            "id": "M", "holderCount": 2, "mcap": 3100.5, "liquidity": "123.4",
            "stats5m": {"numBuys": 0, "numSells": 0, "numTraders": 0, "buyVolume": 0.0},
            "audit": {"topHoldersPercentage": 41.2, "devBalancePercentage": 3.0}
        });
        let r = checkpoint_row(&tok);
        assert_eq!(r["holders"], 2);
        assert_eq!(r["liquidity"], 123.4);
        assert_eq!(r["top_holders_pct"], 41.2);
        assert!(!looks_dead(60, &r), "too early to call");
        assert!(looks_dead(300, &r));
        let mut alive = r.clone();
        alive["traders_5m"] = json!(4);
        assert!(!looks_dead(300, &alive));
    }

    #[test]
    fn tape_rows_and_raw_archive() {
        let dir = tempfile::tempdir().unwrap();
        let t = Tape::new(dir.path());
        let ts = 1_791_500_000_000; // 2026-10-08
        t.row("launches", ts, &json!({"a": 1})).unwrap();
        t.row("launches", ts, &json!({"a": 2})).unwrap();
        let sha = t.raw(ts, "x", "{\"body\":true}").unwrap();
        let sha2 = t.raw(ts, "x", "second").unwrap();
        let day = dir.path().join(day_of(ts));
        let rows = std::fs::read_to_string(day.join("launches.jsonl")).unwrap();
        assert_eq!(rows.lines().count(), 2);
        // two gzip members, both readable, body kept byte for byte with its hash
        let gz = std::fs::read(
            std::fs::read_dir(&day)
                .unwrap()
                .filter_map(|e| e.ok())
                .find(|e| e.file_name().to_string_lossy().starts_with("raw-"))
                .unwrap()
                .path(),
        )
        .unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut flate2::read::MultiGzDecoder::new(&gz[..]), &mut text)
            .unwrap();
        let recs: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["body"], "{\"body\":true}");
        assert_eq!(recs[0]["sha256"], sha);
        assert_eq!(
            sha256_hex(recs[1]["body"].as_str().unwrap().as_bytes()),
            sha2
        );
    }

    #[test]
    fn trending_rows_keep_rank_and_skip_other_chains() {
        let jup = json!({"id": "M", "symbol": "X", "holderCount": 900, "launchpad": "pump.fun", "createdAt": "2026-10-08T10:00:00Z"});
        let r = trending_row("jup_trending_1h", 2, &jup, "id").unwrap();
        assert_eq!(
            (r["rank"].as_u64(), r["holders"].as_u64()),
            (Some(3), Some(900))
        );
        assert!(r["created_ms"].as_i64().is_some());
        let dex = json!({"chainId": "base", "tokenAddress": "0xabc"});
        assert!(trending_row("dex_boosts_top", 0, &dex, "tokenAddress").is_none());
        let dex = json!({"chainId": "solana", "tokenAddress": "M", "totalAmount": 500});
        assert_eq!(
            trending_row("dex_boosts_top", 0, &dex, "tokenAddress").unwrap()["boost_amount"],
            500
        );
    }

    #[test]
    fn pump_curves_are_recognised() {
        assert!(is_pump_mint("abcpump", "-"));
        assert!(is_pump_mint("abc", "pump.fun"));
        assert!(!is_pump_mint("abc", "met-dbc"));
    }
}
