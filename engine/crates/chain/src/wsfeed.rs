//! Free feed: the standard Solana WebSocket + RPC, no paid streaming plan.
//!
//! * `logsSubscribe` per leader → `getTransaction` → the same `ChainTx` the
//!   gRPC feed produces, so detection, sizing and exits are identical.
//! * `slotSubscribe` keeps the engine's idea of "now" (detection lag, stale feed).
//! * For every leader buy the coin's pool accounts are read *before* the
//!   transaction is handed to the engine, so it decides on the current price
//!   and builds from current reserves, not the leader's.
//! * A poller prices the coins we hold: one batched `getMultipleAccounts` per
//!   tick, emitted as `FeedEvent::State` (price, liquidity, migration).
//!
//! Slower than gRPC (a confirmation plus two RPC round trips behind the
//! leader), which is the price of free. It works on any RPC with WebSocket
//! support, including the public endpoint.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::types::Side;
use futures::{SinkExt, StreamExt};
use reqwest_websocket::{Bytes, Message, RequestBuilderExt, WebSocket};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{mpsc, watch};

use crate::detect;
use crate::feed::{FeedEvent, Filters, StateUpdate};
use crate::model::ChainTx;
use crate::pump_amm::GlobalConfig;
use crate::rpc::Rpc;
use crate::state::{self, Watch};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WsConfig {
    /// Env var holding the `wss://` URL. Default: the RPC URL with `https` → `wss`.
    #[serde(default)]
    pub url_env: Option<String>,
    /// Commitment for leader notifications. `confirmed` is the earliest at which
    /// `getTransaction` can return the transaction.
    #[serde(default = "d_commitment")]
    pub commitment: String,
    /// Pool-state poll interval while a position is open.
    #[serde(default = "d_poll")]
    pub poll_ms: u64,
    /// Poll interval when only closed positions are still being tracked for scoring.
    #[serde(default = "d_idle")]
    pub idle_poll_ms: u64,
}

fn d_commitment() -> String {
    "confirmed".into()
}
fn d_poll() -> u64 {
    1200
}
fn d_idle() -> u64 {
    10_000
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            url_env: None,
            commitment: d_commitment(),
            poll_ms: d_poll(),
            idle_poll_ms: d_idle(),
        }
    }
}

/// WebSocket URL for `cfg`: the named env var, else the RPC URL with the scheme swapped.
pub fn ws_url(cfg: &WsConfig, rpc_url: &str) -> String {
    if let Some(v) = cfg
        .url_env
        .as_ref()
        .and_then(|e| std::env::var(e).ok())
        .filter(|s| !s.is_empty())
    {
        return v;
    }
    if let Some(rest) = rpc_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = rpc_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        rpc_url.to_string()
    }
}

// ------------------------------------------------------------------ messages

#[derive(Debug, PartialEq)]
pub enum WsMsg {
    Subscribed {
        id: u64,
        sub: u64,
    },
    Slot(u64),
    Logs {
        slot: u64,
        signature: String,
        failed: bool,
        /// The transaction invoked a program we can read swaps from.
        swap: bool,
    },
    Failed {
        id: Option<u64>,
        message: String,
    },
    Other,
}

pub fn parse_msg(text: &str) -> WsMsg {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return WsMsg::Other;
    };
    if let Some(m) = v["method"].as_str() {
        let r = &v["params"]["result"];
        return match m {
            "slotNotification" => r["slot"].as_u64().map(WsMsg::Slot).unwrap_or(WsMsg::Other),
            "logsNotification" => WsMsg::Logs {
                slot: r["context"]["slot"].as_u64().unwrap_or(0),
                signature: r["value"]["signature"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                failed: !r["value"]["err"].is_null(),
                swap: invokes_swap_program(&r["value"]["logs"]),
            },
            _ => WsMsg::Other,
        };
    }
    if let Some(e) = v.get("error") {
        return WsMsg::Failed {
            id: v["id"].as_u64(),
            message: e.to_string(),
        };
    }
    match (v["id"].as_u64(), v["result"].as_u64()) {
        (Some(id), Some(sub)) => WsMsg::Subscribed { id, sub },
        _ => WsMsg::Other,
    }
}

/// True if any log line shows a swap venue program being invoked. Transfers,
/// airdrops and everything else are skipped without a single RPC call.
fn invokes_swap_program(logs: &Value) -> bool {
    let ids = detect::venue_program_ids();
    logs.as_array().is_some_and(|ls| {
        ls.iter().filter_map(|l| l.as_str()).any(|l| {
            l.strip_prefix("Program ")
                .and_then(|r| r.split_once(' '))
                .is_some_and(|(id, rest)| rest.starts_with("invoke") && ids.iter().any(|x| x == id))
        })
    })
}

/// Bounded set of recently seen signatures.
struct Seen {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl Seen {
    fn new() -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
        }
    }
    /// True if `sig` was not seen before.
    fn insert(&mut self, sig: &str) -> bool {
        if !self.set.insert(sig.to_string()) {
            return false;
        }
        self.order.push_back(sig.to_string());
        if self.order.len() > 8192 {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

// ------------------------------------------------------------------ entry point

/// Start the free feed: WebSocket session (reconnecting) and the state poller.
/// Returns the task handles (abort them to stop the feed).
pub fn spawn(
    cfg: WsConfig,
    rpc: Rpc,
    filters: watch::Receiver<Filters>,
    watch_list: watch::Receiver<Vec<Watch>>,
    out: mpsc::Sender<FeedEvent>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let url = ws_url(&cfg, &rpc.url);
    vec![
        tokio::spawn(poll_loop(cfg.clone(), rpc.clone(), watch_list, out.clone())),
        tokio::spawn(ws_loop(cfg, url, rpc, filters, out)),
    ]
}

#[derive(Debug, Clone, PartialEq)]
enum Sub {
    Slot,
    Leader(String),
}

async fn ws_loop(
    cfg: WsConfig,
    url: String,
    rpc: Rpc,
    mut filters: watch::Receiver<Filters>,
    out: mpsc::Sender<FeedEvent>,
) {
    let mut seen = Seen::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let res = session(&cfg, &url, &rpc, &mut filters, &out, &mut seen).await;
        let detail = match res {
            Ok(()) => "closed".to_string(),
            Err(e) => e.to_string(),
        };
        let _ = out
            .send(FeedEvent::Status {
                source: "ws",
                connected: false,
                detail,
            })
            .await;
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn send(ws: &mut WebSocket, v: Value) -> anyhow::Result<()> {
    ws.send(Message::Text(v.to_string())).await?;
    Ok(())
}

struct Subs {
    next_id: u64,
    /// request id → what was asked
    pending: HashMap<u64, Sub>,
    /// subscription id → what it delivers
    active: HashMap<u64, Sub>,
}

impl Subs {
    fn has_leader(&self, l: &str) -> bool {
        self.pending
            .values()
            .chain(self.active.values())
            .any(|s| matches!(s, Sub::Leader(x) if x == l))
    }
}

/// Make the live subscriptions match the engine's leader list.
async fn reconcile(
    ws: &mut WebSocket,
    cfg: &WsConfig,
    subs: &mut Subs,
    want: &[String],
) -> anyhow::Result<()> {
    for l in want {
        if !subs.has_leader(l) {
            let id = subs.next_id;
            subs.next_id += 1;
            subs.pending.insert(id, Sub::Leader(l.clone()));
            send(
                ws,
                json!({"jsonrpc": "2.0", "id": id, "method": "logsSubscribe",
                       "params": [{"mentions": [l]}, {"commitment": cfg.commitment}]}),
            )
            .await?;
        }
    }
    let stale: Vec<u64> = subs
        .active
        .iter()
        .filter(|(_, s)| matches!(s, Sub::Leader(l) if !want.contains(l)))
        .map(|(id, _)| *id)
        .collect();
    for sub in stale {
        subs.active.remove(&sub);
        let id = subs.next_id;
        subs.next_id += 1;
        send(
            ws,
            json!({"jsonrpc": "2.0", "id": id, "method": "logsUnsubscribe", "params": [sub]}),
        )
        .await?;
    }
    Ok(())
}

async fn session(
    cfg: &WsConfig,
    url: &str,
    rpc: &Rpc,
    filters: &mut watch::Receiver<Filters>,
    out: &mpsc::Sender<FeedEvent>,
    seen: &mut Seen,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .http1_only()
        .connect_timeout(Duration::from_secs(8))
        .build()?;
    let mut ws = client
        .get(url)
        .upgrade()
        .send()
        .await?
        .into_websocket()
        .await?;
    let _ = out
        .send(FeedEvent::Status {
            source: "ws",
            connected: true,
            detail: String::new(),
        })
        .await;

    let mut subs = Subs {
        next_id: 2,
        pending: HashMap::new(),
        active: HashMap::new(),
    };
    subs.pending.insert(1, Sub::Slot);
    send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 1, "method": "slotSubscribe"}),
    )
    .await?;
    let mut want = filters.borrow_and_update().leaders.clone();
    let mut leaders: Arc<HashSet<Pubkey>> = Arc::new(parse_leaders(&want));
    reconcile(&mut ws, cfg, &mut subs, &want).await?;

    let gate = Arc::new(tokio::sync::Semaphore::new(MAX_FETCHES));
    let mut ping = tokio::time::interval(Duration::from_secs(10));
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { anyhow::bail!("connection closed") };
                last_rx = Instant::now();
                let text = match msg? {
                    Message::Text(t) => t,
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue }
                    Message::Pong(_) => continue,
                    Message::Close { reason, .. } => anyhow::bail!("closed by server: {reason}"),
                };
                tracing::trace!("ws rx: {}", &text[..text.len().min(300)]);
                match parse_msg(&text) {
                    WsMsg::Subscribed { id, sub } => {
                        if let Some(k) = subs.pending.remove(&id) {
                            if let Sub::Leader(l) = &k {
                                let _ = out
                                    .send(FeedEvent::Status {
                                        source: "ws",
                                        connected: true,
                                        detail: format!("subscribed {l}"),
                                    })
                                    .await;
                            }
                            subs.active.insert(sub, k);
                        }
                    }
                    WsMsg::Slot(slot) => {
                        let _ = out.send(FeedEvent::Slot { slot, status: 0 }).await;
                    }
                    WsMsg::Logs { slot, signature, failed, swap } => {
                        if !failed && swap && !signature.is_empty() && seen.insert(&signature) {
                            tokio::spawn(fetch_and_emit(
                                rpc.clone(), signature, slot, ChainTx::now_ns(),
                                leaders.clone(), out.clone(), gate.clone(),
                            ));
                        }
                    }
                    WsMsg::Failed { id, message } => {
                        if let Some(what) = id.and_then(|i| subs.pending.remove(&i)) {
                            tracing::warn!("ws subscribe {what:?} refused: {message}");
                        }
                    }
                    WsMsg::Other => {}
                }
            }
            r = filters.changed() => {
                r?;
                want = filters.borrow_and_update().leaders.clone();
                leaders = Arc::new(parse_leaders(&want));
                reconcile(&mut ws, cfg, &mut subs, &want).await?;
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > Duration::from_secs(20) {
                    anyhow::bail!("no data for 20s");
                }
                ws.send(Message::Ping(Bytes::new())).await?;
                // retry subscriptions the server refused (rate limits)
                reconcile(&mut ws, cfg, &mut subs, &want).await?;
            }
        }
    }
}

fn parse_leaders(v: &[String]) -> HashSet<Pubkey> {
    v.iter().filter_map(|s| s.parse().ok()).collect()
}

// ------------------------------------------------------------------ fetching

/// Waits between `getTransaction` attempts: a confirmed notification can
/// arrive slightly before the node can serve the transaction.
const FETCH_STEPS_MS: [u64; 9] = [0, 100, 100, 150, 150, 300, 400, 800, 1000];

/// Transactions fetched concurrently. A leader that fires many swaps per
/// second (a bot) cannot flood a free RPC: excess notifications are dropped.
const MAX_FETCHES: usize = 6;

async fn fetch_and_emit(
    rpc: Rpc,
    sig: String,
    slot_hint: u64,
    observed_ns: u64,
    leaders: Arc<HashSet<Pubkey>>,
    out: mpsc::Sender<FeedEvent>,
    gate: Arc<tokio::sync::Semaphore>,
) {
    let _permit = match tokio::time::timeout(Duration::from_secs(2), gate.acquire_owned()).await {
        Ok(Ok(p)) => p,
        _ => {
            tracing::warn!(
                "fetch queue full: dropping {sig} (leader trading too fast for this RPC)"
            );
            return;
        }
    };
    let mut found = None;
    for step in FETCH_STEPS_MS {
        if step > 0 {
            tokio::time::sleep(Duration::from_millis(step)).await;
        }
        match rpc.transaction_json_once(&sig).await {
            Ok(v) if !v.is_null() => {
                found = Some(v);
                break;
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("getTransaction {sig}: {e}"),
        }
    }
    let Some(v) = found else {
        tracing::warn!("transaction {sig} (slot {slot_hint}) never became available");
        return;
    };
    let mut tx = match ChainTx::from_rpc_json(&v) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("transaction {sig}: {e}");
            return;
        }
    };
    tx.observed_at_ns = observed_ns;

    // Current pool state for the coins leaders are buying.
    let mut want: Vec<(Pubkey, crate::detect::Template, u8, Pubkey)> = Vec::new();
    for s in detect::all_swaps(&tx) {
        if s.side == Side::Buy
            && leaders.contains(&s.wallet)
            && state::supported(&s.template)
            && !want.iter().any(|w| w.0 == s.mint)
        {
            want.push((s.mint, s.template, s.token_decimals, s.token_program));
        }
    }
    if !want.is_empty() {
        let items: Vec<_> = want.iter().map(|w| (w.1.clone(), w.2)).collect();
        match state::read(&rpc, &items).await {
            Ok((slot, refreshed)) => {
                for (w, r) in want.iter().zip(refreshed) {
                    if let Some(r) = r.filter(|r| !r.complete) {
                        let _ = out
                            .send(FeedEvent::State(Box::new(StateUpdate {
                                mint: w.0,
                                template: r.template,
                                price_sol: r.price_sol,
                                pool_sol: r.pool_sol,
                                decimals: w.2,
                                token_program: w.3,
                                slot,
                                migrated: false,
                            })))
                            .await;
                    }
                }
            }
            Err(e) => tracing::debug!("pool state for {sig}: {e}"),
        }
    }
    let _ = out.send(FeedEvent::Tx(Box::new(tx))).await;
}

// ------------------------------------------------------------------ poller

async fn poll_loop(
    cfg: WsConfig,
    rpc: Rpc,
    watch_list: watch::Receiver<Vec<Watch>>,
    out: mpsc::Sender<FeedEvent>,
) {
    let mut global: Option<GlobalConfig> = None;
    let mut last = Instant::now() - Duration::from_secs(3600);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let list = watch_list.borrow().clone();
        if list.is_empty() {
            continue;
        }
        let every = if list.iter().any(|w| w.held) {
            cfg.poll_ms
        } else {
            cfg.idle_poll_ms
        };
        if last.elapsed() < Duration::from_millis(every) {
            continue;
        }
        last = Instant::now();
        poll_once(&rpc, &list, &mut global, &out).await;
    }
}

async fn poll_once(
    rpc: &Rpc,
    list: &[Watch],
    global: &mut Option<GlobalConfig>,
    out: &mpsc::Sender<FeedEvent>,
) {
    let items: Vec<_> = list
        .iter()
        .map(|w| (w.template.clone(), w.decimals))
        .collect();
    let (slot, refreshed) = match state::read(rpc, &items).await {
        Ok(x) => x,
        Err(e) => {
            tracing::debug!("pool state poll: {e}");
            return;
        }
    };
    for (w, r) in list.iter().zip(refreshed) {
        let Some(r) = r else { continue };
        let (template, price_sol, pool_sol, migrated) = if r.complete {
            // The curve is done: the coin lives on its PumpSwap pool now.
            if global.is_none() {
                *global = state::load_amm_global(rpc).await.ok();
            }
            let Some(g) = global.as_ref() else { continue };
            match state::amm_template(
                rpc,
                g,
                &w.mint,
                &w.token_program,
                w.decimals,
                ChainTx::now_ns(),
            )
            .await
            {
                Ok((t, ar)) => (t, ar.price_sol, ar.pool_sol, true),
                Err(e) => {
                    tracing::debug!("migrated pool for {}: {e}", w.mint);
                    continue; // not visible yet; retry next tick
                }
            }
        } else {
            (r.template, r.price_sol, r.pool_sol, false)
        };
        let _ = out
            .send(FeedEvent::State(Box::new(StateUpdate {
                mint: w.mint,
                template,
                price_sol,
                pool_sol,
                decimals: w.decimals,
                token_program: w.token_program,
                slot,
                migrated,
            })))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subscription_ack_slot_and_logs() {
        assert_eq!(
            parse_msg(r#"{"jsonrpc":"2.0","result":23784,"id":3}"#),
            WsMsg::Subscribed { id: 3, sub: 23784 }
        );
        assert_eq!(
            parse_msg(
                r#"{"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"parent":75,"root":44,"slot":76},"subscription":0}}"#
            ),
            WsMsg::Slot(76)
        );
        let ok = r#"{"jsonrpc":"2.0","method":"logsNotification","params":{"result":{"context":{"slot":5208469},"value":{"signature":"5h6x","err":null,"logs":["Program x invoke [1]"]}},"subscription":24040}}"#;
        assert_eq!(
            parse_msg(ok),
            WsMsg::Logs {
                slot: 5_208_469,
                signature: "5h6x".into(),
                failed: false,
                swap: false
            }
        );
        let pump = ok.replace(
            "Program x invoke [1]",
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]",
        );
        assert!(matches!(parse_msg(&pump), WsMsg::Logs { swap: true, .. }));
        // a success line or a mere mention is not an invocation
        let mention = ok.replace(
            "Program x invoke [1]",
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P success",
        );
        assert!(matches!(
            parse_msg(&mention),
            WsMsg::Logs { swap: false, .. }
        ));
        let bad = ok.replace(
            r#""err":null"#,
            r#""err":{"InstructionError":[0,"Custom"]}"#,
        );
        assert!(matches!(parse_msg(&bad), WsMsg::Logs { failed: true, .. }));
    }

    #[test]
    fn parses_errors_and_ignores_noise() {
        match parse_msg(r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"bad"},"id":7}"#) {
            WsMsg::Failed { id, message } => {
                assert_eq!(id, Some(7));
                assert!(message.contains("-32602"));
            }
            other => panic!("{other:?}"),
        }
        // unsubscribe ack is `result: true`
        assert_eq!(
            parse_msg(r#"{"jsonrpc":"2.0","result":true,"id":9}"#),
            WsMsg::Other
        );
        assert_eq!(parse_msg("not json"), WsMsg::Other);
    }

    #[test]
    fn ws_url_from_rpc_url() {
        let c = WsConfig::default();
        assert_eq!(
            ws_url(&c, "https://mainnet.helius-rpc.com/?api-key=k"),
            "wss://mainnet.helius-rpc.com/?api-key=k"
        );
        assert_eq!(ws_url(&c, "http://127.0.0.1:8899"), "ws://127.0.0.1:8899");
        std::env::set_var("WSFEED_TEST_URL", "wss://example.org/ws");
        let c = WsConfig {
            url_env: Some("WSFEED_TEST_URL".into()),
            ..Default::default()
        };
        assert_eq!(ws_url(&c, "https://x"), "wss://example.org/ws");
    }

    #[test]
    fn seen_is_bounded_and_dedupes() {
        let mut s = Seen::new();
        assert!(s.insert("a"));
        assert!(!s.insert("a"));
        for i in 0..9000 {
            s.insert(&format!("sig{i}"));
        }
        assert!(s.set.len() <= 8192);
        assert!(s.insert("a"), "old entries age out");
    }
}
