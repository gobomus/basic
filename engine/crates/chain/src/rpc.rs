//! Lean JSON-RPC client (reqwest, HTTP/2 keep-alive). Only the calls the
//! engine needs; every call is timed so latency is always observable.

use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;

#[derive(Clone)]
pub struct Rpc {
    pub url: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct AccountData {
    pub owner: Pubkey,
    pub lamports: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SimResult {
    pub err: Option<Value>,
    pub logs: Vec<String>,
    pub units_consumed: Option<u64>,
}

pub fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .build()
        .expect("http client")
}

impl Rpc {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            http: http_client(Duration::from_secs(10)),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut attempt = 0u32;
        loop {
            let t = Instant::now();
            let resp: Value = self
                .http
                .post(&self.url)
                .json(&body)
                .send()
                .await?
                .json()
                .await?;
            tracing::trace!(method, ms = t.elapsed().as_millis() as u64, "rpc");
            if let Some(e) = resp.get("error") {
                // rate limited (public / shared RPCs): back off and retry a few times
                if e["code"].as_i64() == Some(429) && attempt < 4 {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(400 * 2u64.pow(attempt))).await;
                    continue;
                }
                anyhow::bail!("{method}: {e}");
            }
            return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn get_slot(&self, commitment: &str) -> anyhow::Result<u64> {
        let v = self
            .call("getSlot", json!([{"commitment": commitment}]))
            .await?;
        v.as_u64().ok_or_else(|| anyhow::anyhow!("bad getSlot"))
    }

    pub async fn latest_blockhash(&self, commitment: &str) -> anyhow::Result<(Hash, u64)> {
        let v = self
            .call("getLatestBlockhash", json!([{"commitment": commitment}]))
            .await?;
        let bh = v["value"]["blockhash"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no blockhash"))?;
        let h = v["value"]["lastValidBlockHeight"].as_u64().unwrap_or(0);
        Ok((bh.parse()?, h))
    }

    pub async fn balance(&self, pk: &Pubkey) -> anyhow::Result<u64> {
        let v = self
            .call(
                "getBalance",
                json!([pk.to_string(), {"commitment": "processed"}]),
            )
            .await?;
        v["value"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("bad getBalance"))
    }

    pub async fn account(&self, pk: &Pubkey) -> anyhow::Result<Option<AccountData>> {
        let v = self
            .call(
                "getAccountInfo",
                json!([pk.to_string(), {"encoding": "base64", "commitment": "processed"}]),
            )
            .await?;
        parse_account(&v["value"])
    }

    pub async fn accounts(&self, pks: &[Pubkey]) -> anyhow::Result<Vec<Option<AccountData>>> {
        let keys: Vec<String> = pks.iter().map(|p| p.to_string()).collect();
        let v = self
            .call(
                "getMultipleAccounts",
                json!([keys, {"encoding": "base64", "commitment": "processed"}]),
            )
            .await?;
        v["value"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("bad getMultipleAccounts"))?
            .iter()
            .map(parse_account)
            .collect()
    }

    pub async fn send(&self, wire_b64: &str) -> anyhow::Result<String> {
        let v = self
            .call(
                "sendTransaction",
                json!([wire_b64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]),
            )
            .await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }

    pub async fn simulate(&self, wire_b64: &str, sig_verify: bool) -> anyhow::Result<SimResult> {
        let v = self
            .call(
                "simulateTransaction",
                json!([wire_b64, {"encoding": "base64", "sigVerify": sig_verify,
                    "replaceRecentBlockhash": !sig_verify, "commitment": "processed"}]),
            )
            .await?;
        let val = &v["value"];
        Ok(SimResult {
            err: val.get("err").filter(|e| !e.is_null()).cloned(),
            logs: val["logs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            units_consumed: val["unitsConsumed"].as_u64(),
        })
    }

    /// Returns (slot, err) per signature; `None` = not seen yet.
    pub async fn signature_statuses(
        &self,
        sigs: &[String],
    ) -> anyhow::Result<Vec<Option<(u64, Option<Value>)>>> {
        let v = self
            .call(
                "getSignatureStatuses",
                json!([sigs, {"searchTransactionHistory": false}]),
            )
            .await?;
        Ok(v["value"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|s| {
                        if s.is_null() {
                            None
                        } else {
                            Some((
                                s["slot"].as_u64().unwrap_or(0),
                                s.get("err").filter(|e| !e.is_null()).cloned(),
                            ))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// (token account, mint, raw amount, token program) for every token account the owner holds.
    pub async fn token_accounts(
        &self,
        owner: &Pubkey,
        program: &Pubkey,
    ) -> anyhow::Result<Vec<(Pubkey, Pubkey, u64)>> {
        let v = self
            .call(
                "getTokenAccountsByOwner",
                json!([owner.to_string(), {"programId": program.to_string()}, {"encoding": "jsonParsed", "commitment": "processed"}]),
            )
            .await?;
        let mut out = Vec::new();
        for it in v["value"].as_array().into_iter().flatten() {
            let info = &it["account"]["data"]["parsed"]["info"];
            let (Some(pk), Some(mint), Some(amt)) = (
                it["pubkey"].as_str(),
                info["mint"].as_str(),
                info["tokenAmount"]["amount"].as_str(),
            ) else {
                continue;
            };
            out.push((pk.parse()?, mint.parse()?, amt.parse().unwrap_or(0)));
        }
        Ok(out)
    }

    pub async fn signatures_for_address(
        &self,
        pk: &Pubkey,
        limit: usize,
        before: Option<&str>,
    ) -> anyhow::Result<Vec<Value>> {
        let mut opts = json!({"limit": limit, "commitment": "confirmed"});
        if let Some(b) = before {
            opts["before"] = json!(b);
        }
        let v = self
            .call("getSignaturesForAddress", json!([pk.to_string(), opts]))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub async fn transaction_json(&self, sig: &str) -> anyhow::Result<Value> {
        self.call(
            "getTransaction",
            json!([sig, {"encoding": "json", "maxSupportedTransactionVersion": 1, "commitment": "confirmed"}]),
        )
        .await
    }
}

fn parse_account(v: &Value) -> anyhow::Result<Option<AccountData>> {
    if v.is_null() {
        return Ok(None);
    }
    let data_b64 = v["data"][0].as_str().unwrap_or_default();
    Ok(Some(AccountData {
        owner: v["owner"].as_str().unwrap_or_default().parse()?,
        lamports: v["lamports"].as_u64().unwrap_or(0),
        data: base64::engine::general_purpose::STANDARD.decode(data_b64)?,
    }))
}
