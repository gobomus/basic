//! Multi-sender fan-out. One signed transaction (one signature) is pushed to
//! every configured endpoint in parallel, so it can execute at most once no
//! matter how many paths deliver it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

use crate::rpc::http_client;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SenderConfig {
    pub name: String,
    /// JSON-RPC endpoint accepting `sendTransaction` (Jito `/api/v1/transactions`,
    /// Helius Sender, Nozomi, 0slot, Astralane, or a plain RPC).
    pub url: String,
    /// Extra HTTP headers (API keys), e.g. { "x-api-key" = "..." }.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// True if this endpoint accepts a tip paid to a Jito tip account.
    #[serde(default)]
    pub accepts_jito_tip: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
pub struct SendReport {
    pub sender: String,
    pub ok: bool,
    pub ms: u64,
    pub detail: String,
}

#[derive(Clone)]
pub struct Fanout {
    senders: Arc<Vec<SenderConfig>>,
    http: reqwest::Client,
}

impl Fanout {
    pub fn new(senders: Vec<SenderConfig>) -> Self {
        Self { senders: Arc::new(senders.into_iter().filter(|s| s.enabled).collect()), http: http_client(Duration::from_secs(5)) }
    }

    pub fn names(&self) -> Vec<String> {
        self.senders.iter().map(|s| s.name.clone()).collect()
    }

    pub fn any_accepts_jito_tip(&self) -> bool {
        self.senders.iter().any(|s| s.accepts_jito_tip)
    }

    /// Send to all endpoints concurrently; returns as soon as all have answered.
    pub async fn send(&self, wire_b64: &str) -> Vec<SendReport> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "sendTransaction",
            "params": [wire_b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]});
        let futs = self.senders.iter().map(|s| {
            let mut req = self.http.post(&s.url).json(&body);
            for (k, v) in &s.headers {
                req = req.header(k, v);
            }
            let name = s.name.clone();
            async move {
                let t = Instant::now();
                let res = async { req.send().await?.json::<Value>().await }.await;
                let ms = t.elapsed().as_millis() as u64;
                match res {
                    Ok(v) if v.get("error").is_none() => SendReport { sender: name, ok: true, ms, detail: v["result"].to_string() },
                    Ok(v) => SendReport { sender: name, ok: false, ms, detail: v["error"].to_string() },
                    Err(e) => SendReport { sender: name, ok: false, ms, detail: e.to_string() },
                }
            }
        });
        futures::future::join_all(futs).await
    }
}

/// Fetch Jito tip accounts from a block engine (`getTipAccounts`).
pub async fn fetch_jito_tip_accounts(block_engine_url: &str) -> anyhow::Result<Vec<Pubkey>> {
    let http = http_client(Duration::from_secs(5));
    let base = block_engine_url.trim_end_matches('/');
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "getTipAccounts", "params": []});
    let mut last = None;
    for path in ["/api/v1/getTipAccounts", "/api/v1/bundles"] {
        match http.post(format!("{base}{path}")).json(&body).send().await {
            Ok(r) => {
                let v: Value = r.json().await?;
                if let Some(arr) = v["result"].as_array() {
                    let out: Vec<Pubkey> = arr.iter().filter_map(|x| x.as_str()?.parse().ok()).collect();
                    if !out.is_empty() {
                        return Ok(out);
                    }
                }
                last = Some(anyhow::anyhow!("unexpected response: {v}"));
            }
            Err(e) => last = Some(e.into()),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no tip accounts")))
}
