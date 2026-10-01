//! Yellowstone gRPC feed (works with Triton, Helius LaserStream, Shyft,
//! QuickNode, RPC Fast… any Yellowstone-compatible endpoint).
//!
//! * `transactions` stream at processed commitment: leaders, held mints and
//!   (optionally) every Pump / PumpSwap transaction for the market firehose.
//! * optional `deshred` stream: leader transactions *before execution*.
//! * filters are hot-updated through the subscription sink (no reconnect),
//!   and the connection is re-established with backoff on any error.

use std::collections::HashMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, subscribe_update_deshred::UpdateOneof as DeshredOneof,
    CommitmentLevel, SubscribeDeshredRequest, SubscribeRequest,
    SubscribeRequestFilterDeshredTransactions, SubscribeRequestFilterSlots,
    SubscribeRequestFilterTransactions, SubscribeRequestPing,
};

use crate::consts::{PUMP_AMM_PROGRAM, PUMP_PROGRAM};
use crate::model::ChainTx;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeyserConfig {
    pub endpoint: String,
    /// x-token / API key. Prefer the env var named in `x_token_env`.
    #[serde(default)]
    pub x_token_env: Option<String>,
    /// Subscribe to every Pump, PumpSwap, Meteora DBC and Raydium LaunchLab
    /// transaction (market firehose).
    #[serde(default = "yes")]
    pub firehose: bool,
    /// Also open the pre-execution deshred stream (if the provider supports it).
    #[serde(default)]
    pub deshred: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    pub leaders: Vec<String>,
    pub mints: Vec<String>,
}

#[derive(Debug)]
pub enum FeedEvent {
    Tx(Box<ChainTx>),
    Slot {
        slot: u64,
        status: i32,
    },
    Status {
        source: &'static str,
        connected: bool,
        detail: String,
    },
}

fn token(cfg: &GeyserConfig) -> Option<String> {
    cfg.x_token_env
        .as_ref()
        .and_then(|v| std::env::var(v).ok())
        .filter(|s| !s.is_empty())
}

async fn connect(cfg: &GeyserConfig) -> anyhow::Result<GeyserGrpcClient> {
    let mut b = GeyserGrpcClient::build_from_shared(cfg.endpoint.clone())?
        .x_token(token(cfg))?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .http2_adaptive_window(true)
        .max_decoding_message_size(64 * 1024 * 1024);
    if cfg.endpoint.starts_with("https") {
        b = b.tls_config(ClientTlsConfig::new().with_native_roots())?;
    }
    Ok(b.connect().await?)
}

fn request(cfg: &GeyserConfig, f: &Filters) -> SubscribeRequest {
    let mut txs = HashMap::new();
    let base = SubscribeRequestFilterTransactions {
        vote: Some(false),
        failed: Some(false),
        ..Default::default()
    };
    if !f.leaders.is_empty() {
        txs.insert(
            "leaders".into(),
            SubscribeRequestFilterTransactions {
                account_include: f.leaders.clone(),
                ..base.clone()
            },
        );
    }
    if !f.mints.is_empty() {
        txs.insert(
            "mints".into(),
            SubscribeRequestFilterTransactions {
                account_include: f.mints.clone(),
                ..base.clone()
            },
        );
    }
    if cfg.firehose {
        txs.insert(
            "firehose".into(),
            SubscribeRequestFilterTransactions {
                account_include: vec![
                    PUMP_PROGRAM.to_string(),
                    PUMP_AMM_PROGRAM.to_string(),
                    crate::meteora_dbc::DBC_PROGRAM.to_string(),
                    crate::raydium_launchlab::LAUNCHLAB_PROGRAM.to_string(),
                ],
                ..base
            },
        );
    }
    let mut slots = HashMap::new();
    slots.insert(
        "slots".into(),
        SubscribeRequestFilterSlots {
            filter_by_commitment: Some(false),
            interslot_updates: Some(false),
        },
    );
    SubscribeRequest {
        transactions: txs,
        slots,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    }
}

/// Run the transactions stream forever (reconnecting), pushing into `out`.
pub async fn run(
    cfg: GeyserConfig,
    mut filters: watch::Receiver<Filters>,
    out: mpsc::Sender<FeedEvent>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        let res: anyhow::Result<()> = async {
            let mut client = connect(&cfg).await?;
            let current = filters.borrow_and_update().clone();
            let (mut sink, mut stream) = client.subscribe_with_request(Some(request(&cfg, &current))).await?;
            let _ = out.send(FeedEvent::Status { source: "geyser", connected: true, detail: cfg.endpoint.clone() }).await;
            backoff = Duration::from_millis(250);
            let mut ping = tokio::time::interval(Duration::from_secs(10));
            loop {
                tokio::select! {
                    msg = stream.next() => {
                        let Some(msg) = msg else { anyhow::bail!("stream ended") };
                        let msg = msg?;
                        let now = ChainTx::now_ns();
                        match msg.update_oneof {
                            Some(UpdateOneof::Transaction(u)) => {
                                if let Some(info) = u.transaction.as_ref() {
                                    if let Some(tx) = ChainTx::from_geyser(info, u.slot, now) {
                                        if out.send(FeedEvent::Tx(Box::new(tx))).await.is_err() { return Ok(()); }
                                    }
                                }
                            }
                            Some(UpdateOneof::Slot(s)) => {
                                let _ = out.try_send(FeedEvent::Slot { slot: s.slot, status: s.status });
                            }
                            _ => {}
                        }
                    }
                    changed = filters.changed() => {
                        if changed.is_err() { return Ok(()); }
                        let f = filters.borrow_and_update().clone();
                        sink.send(request(&cfg, &f)).await.map_err(|e| anyhow::anyhow!("filter update: {e:?}"))?;
                    }
                    _ = ping.tick() => {
                        let mut r = request(&cfg, &filters.borrow().clone());
                        r.ping = Some(SubscribeRequestPing { id: 1 });
                        sink.send(r).await.map_err(|e| anyhow::anyhow!("ping: {e:?}"))?;
                    }
                }
            }
        }
        .await;
        match res {
            Ok(()) => return,
            Err(e) => {
                let _ = out
                    .send(FeedEvent::Status {
                        source: "geyser",
                        connected: false,
                        detail: e.to_string(),
                    })
                    .await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

/// Pre-execution stream for leader wallets only.
pub async fn run_deshred(
    cfg: GeyserConfig,
    mut filters: watch::Receiver<Filters>,
    out: mpsc::Sender<FeedEvent>,
) {
    let mut backoff = Duration::from_millis(500);
    loop {
        let res: anyhow::Result<()> = async {
            let mut client = connect(&cfg).await?;
            let mk = |f: &Filters| {
                let mut m = HashMap::new();
                m.insert(
                    "leaders".to_string(),
                    SubscribeRequestFilterDeshredTransactions {
                        vote: Some(false),
                        account_include: f.leaders.clone(),
                        ..Default::default()
                    },
                );
                SubscribeDeshredRequest { deshred_transactions: m, ..Default::default() }
            };
            let current = filters.borrow_and_update().clone();
            let (mut sink, mut stream) = client.subscribe_deshred_with_request(Some(mk(&current))).await?;
            let _ = out.send(FeedEvent::Status { source: "deshred", connected: true, detail: cfg.endpoint.clone() }).await;
            loop {
                tokio::select! {
                    msg = stream.next() => {
                        let Some(msg) = msg else { anyhow::bail!("deshred stream ended") };
                        let now = ChainTx::now_ns();
                        if let Some(DeshredOneof::DeshredTransaction(u)) = msg?.update_oneof {
                            if let Some(info) = u.transaction.as_ref() {
                                if let Some(tx) = ChainTx::from_deshred(info, u.slot, now) {
                                    if out.send(FeedEvent::Tx(Box::new(tx))).await.is_err() { return Ok(()); }
                                }
                            }
                        }
                    }
                    changed = filters.changed() => {
                        if changed.is_err() { return Ok(()); }
                        let f = filters.borrow_and_update().clone();
                        sink.send(mk(&f)).await.map_err(|e| anyhow::anyhow!("deshred filter update: {e:?}"))?;
                    }
                }
            }
        }
        .await;
        match res {
            Ok(()) => return,
            Err(e) => {
                let _ = out
                    .send(FeedEvent::Status {
                        source: "deshred",
                        connected: false,
                        detail: e.to_string(),
                    })
                    .await;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}
