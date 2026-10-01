//! Trade journal. Every record goes to an append-only daily JSONL file
//! (always on, zero dependencies), and optionally to Postgres (journal tables)
//! and ClickHouse (market firehose). Writers run off the hot path.

use std::io::Write;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct Journal {
    tx: mpsc::UnboundedSender<Value>,
    swaps: Option<mpsc::Sender<Value>>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn ts_str(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

impl Journal {
    pub async fn start(
        dir: &str,
        pg_url: Option<String>,
        clickhouse: Option<String>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let pg = match pg_url {
            Some(url) => match tokio_postgres::connect(&url, tokio_postgres::NoTls).await {
                Ok((client, conn)) => {
                    tokio::spawn(async move {
                        if let Err(e) = conn.await {
                            tracing::error!("postgres connection: {e}");
                        }
                    });
                    tracing::info!("journal: postgres connected");
                    Some(client)
                }
                Err(e) => {
                    tracing::warn!("journal: postgres unavailable ({e}); JSONL only");
                    None
                }
            },
            None => None,
        };
        let dir = dir.to_string();
        tokio::spawn(async move {
            let mut day = String::new();
            let mut file: Option<std::fs::File> = None;
            while let Some(rec) = rx.recv().await {
                let d = chrono::Utc::now().format("%Y-%m-%d").to_string();
                if d != day || file.is_none() {
                    day = d;
                    file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(format!("{dir}/{day}.jsonl"))
                        .ok();
                }
                if let Some(f) = file.as_mut() {
                    let _ = writeln!(f, "{rec}");
                }
                if let Some(c) = &pg {
                    if let Err(e) = pg_write(c, &rec).await {
                        tracing::warn!("postgres write ({}): {e}", rec["kind"]);
                    }
                }
            }
        });

        let swaps = match clickhouse.filter(|u| !u.is_empty()) {
            Some(url) => {
                let (stx, srx) = mpsc::channel::<Value>(100_000);
                tokio::spawn(clickhouse_writer(url, srx));
                Some(stx)
            }
            None => None,
        };
        Ok(Self { tx, swaps })
    }

    pub fn record(&self, kind: &str, mut v: Value) {
        v["kind"] = json!(kind);
        if v.get("ts").is_none() {
            v["ts"] = json!(now_ms());
        }
        let _ = self.tx.send(v);
    }

    /// Market firehose row for `mkt.swaps` (dropped if ClickHouse is not configured or saturated).
    pub fn market_swap(&self, row: Value) {
        if let Some(s) = &self.swaps {
            let _ = s.try_send(row);
        }
    }
}

async fn clickhouse_writer(url: String, mut rx: mpsc::Receiver<Value>) {
    let http = reqwest::Client::new();
    let endpoint = format!(
        "{}/?query={}&date_time_input_format=best_effort",
        url.trim_end_matches('/'),
        "INSERT%20INTO%20mkt.swaps%20FORMAT%20JSONEachRow"
    );
    let mut buf: Vec<Value> = Vec::with_capacity(4096);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            r = rx.recv() => match r { Some(v) => buf.push(v), None => break },
            _ = tick.tick() => {
                if buf.is_empty() { continue; }
                let body: String = buf.drain(..).map(|v| v.to_string() + "\n").collect();
                if let Err(e) = http.post(&endpoint).body(body).send().await.and_then(|r| r.error_for_status()) {
                    tracing::warn!("clickhouse insert: {e}");
                }
            }
        }
    }
}

/// Convert a detected swap into an `mkt.swaps` row.
pub fn swap_row(
    tx: &chain::model::ChainTx,
    s: &chain::detect::DetectedSwap,
    tracked_leader: bool,
) -> Value {
    let observed_ms = (tx.observed_at_ns / 1_000_000) as i64;
    json!({
        "slot": tx.slot,
        "tx_index": tx.tx_index.unwrap_or(0),
        "ix_index": 0,
        "block_time": ts_str(tx.block_time_ms.unwrap_or(observed_ms)),
        "observed_at": ts_str(observed_ms),
        "source": format!("{:?}", tx.source).to_lowercase(),
        "signature": tx.signature,
        "signer": s.wallet.to_string(),
        "fee_payer": tx.fee_payer().map(|p| p.to_string()).unwrap_or_default(),
        "mint": s.mint.to_string(),
        "pool": "",
        "venue": serde_json::to_value(s.venue).unwrap_or_default(),
        "side": if s.side == engine_core::types::Side::Buy { "buy" } else { "sell" },
        "sol_amount": s.sol_amount,
        "token_amount": s.token_amount,
        "price_sol": s.price_sol,
        "pool_sol_after": s.pool_sol,
        "priority_fee": 0,
        "jito_tip": 0,
        "cu_consumed": 0,
        "via_aggregator": "",
        "is_tracked_leader": tracked_leader,
        "wallet_tags": [],
    })
}

async fn pg_write(c: &tokio_postgres::Client, r: &Value) -> anyhow::Result<()> {
    let s = |k: &str| r[k].as_str().map(String::from);
    let i = |k: &str| r[k].as_i64();
    let f = |k: &str| r[k].as_f64();
    let ts = |k: &str| r[k].as_i64().map(|ms| ms as f64 / 1000.0);
    match r["kind"].as_str().unwrap_or("") {
        "wallet" => {
            c.execute(
                "INSERT INTO trading_wallets (pubkey, role, custody, status, max_balance_sol) VALUES ($1,'hot','local_keystore','active',$2::float8::numeric) ON CONFLICT (pubkey) DO NOTHING",
                &[&s("pubkey"), &f("max_balance_sol").unwrap_or(0.0)],
            )
            .await?;
        }
        "signal" => {
            c.execute(
                "INSERT INTO copy_signals (leader, leader_signature, leader_slot, leader_tx_index, mint, venue, side, leader_sol, leader_price, detected_at, detect_source, detect_latency_ms, detect_slot_lag, decision, skip_reason, size_lamports, capped_by)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,to_timestamp($10),$11,$12,$13,$14,$15,$16,$17) ON CONFLICT (leader_signature) DO NOTHING",
                &[
                    &s("leader"), &s("signature"), &i("slot"), &i("tx_index").map(|x| x as i32), &s("mint"), &s("venue"), &s("side"),
                    &i("leader_sol"), &f("leader_price"), &ts("ts"), &s("source"), &f("detect_latency_ms").map(|x| x as f32),
                    &i("slot_lag").map(|x| x as i32), &s("decision"), &s("skip_reason"), &i("size_lamports"), &s("capped_by"),
                ],
            )
            .await?;
        }
        "order" => {
            let senders: Vec<String> = r["senders"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            c.execute(
                "INSERT INTO orders (side, reason, created_at, signature, senders, tip_lamports, cu_limit, expected_price, status, error, landed_slot)
                 VALUES ($1,$2,to_timestamp($3),$4,$5,$6,$7,$8,$9,$10,$11)",
                &[
                    &s("side"), &s("reason"), &ts("ts"), &s("signature"), &senders, &i("tip_lamports"), &i("cu_limit").map(|x| x as i32),
                    &f("expected_price"), &s("status").unwrap_or_else(|| "pending".into()), &s("error"), &i("landed_slot"),
                ],
            )
            .await?;
        }
        "position_close" => {
            let row = c
                .query_one(
                    "INSERT INTO positions (mint, leader, wallet, mode, exit_policy, exit_policy_hash, opened_at, closed_at, entry_price, entry_pool_sol, cost_lamports, proceeds_lamports, fees_lamports, peak_multiple, trough_multiple, realized_pnl_sol)
                     VALUES ($1,$2,$3,$4,$5,$6,to_timestamp($7),to_timestamp($8),$9,$10,$11,$12,$13,$14,$15,$16) RETURNING id",
                    &[
                        &s("mint"), &s("leader"), &s("wallet"), &s("mode"), &s("exit_policy"), &s("exit_policy_hash"), &ts("opened_at"), &ts("ts"),
                        &f("entry_price"), &f("entry_pool_sol"), &i("cost_lamports"), &i("proceeds_lamports"), &i("fees_lamports"),
                        &f("peak_multiple").map(|x| x as f32), &f("trough_multiple").map(|x| x as f32), &f("realized_pnl_sol"),
                    ],
                )
                .await?;
            let id: i64 = row.get(0);
            for sh in r["shadows"].as_array().into_iter().flatten() {
                c.execute(
                    "INSERT INTO shadow_exits (position_id, exit_policy, policy_hash, pnl_sol, ret, held_ms, fully_closed, exits) VALUES ($1,$2,$3,$4,$5,$6,$7,$8::text::jsonb) ON CONFLICT DO NOTHING",
                    &[
                        &id, &sh["policy"].as_str(), &sh["policy_hash"].as_str(), &sh["pnl_sol"].as_f64(), &sh["ret"].as_f64().map(|x| x as f32),
                        &sh["held_ms"].as_i64(), &sh["fully_closed"].as_bool(), &sh["exits"].to_string(),
                    ],
                )
                .await?;
            }
        }
        "risk" => {
            c.execute(
                "INSERT INTO risk_events (kind, detail) VALUES ($1, $2::text::jsonb)",
                &[&s("risk"), &r.to_string()],
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}
