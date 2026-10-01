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
    /// This service's own tip accounts (e.g. Helius Sender, Nozomi, Astralane).
    /// Copy them from the provider's docs/SDK. Overrides `accepts_jito_tip`.
    #[serde(default)]
    pub tip_accounts: Vec<String>,
    /// Provider minimum tip (lamports), e.g. 1_000_000 for Helius Sender Max.
    #[serde(default)]
    pub min_tip_lamports: u64,
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

/// Senders that accept the same tip accounts (one signed variant per group).
#[derive(Debug, Clone, PartialEq)]
pub struct TipGroup {
    pub senders: Vec<usize>,
    pub tip_accounts: Vec<Pubkey>,
    pub min_tip_lamports: u64,
    pub label: String,
}

#[derive(Clone)]
pub struct Fanout {
    senders: Arc<Vec<SenderConfig>>,
    http: reqwest::Client,
}

impl Fanout {
    pub fn new(senders: Vec<SenderConfig>) -> Self {
        Self {
            senders: Arc::new(senders.into_iter().filter(|s| s.enabled).collect()),
            http: http_client(Duration::from_secs(5)),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.senders.iter().map(|s| s.name.clone()).collect()
    }

    /// Group senders by tip family. Plain endpoints (no tip rule) ride along
    /// with the first tipped group.
    pub fn groups(&self, jito_tips: &[Pubkey]) -> Vec<TipGroup> {
        let mut out: Vec<TipGroup> = Vec::new();
        let mut plain = Vec::new();
        for (i, s) in self.senders.iter().enumerate() {
            let tips: Vec<Pubkey> = if !s.tip_accounts.is_empty() {
                s.tip_accounts
                    .iter()
                    .filter_map(|t| t.parse().ok())
                    .collect()
            } else if s.accepts_jito_tip {
                jito_tips.to_vec()
            } else {
                plain.push(i);
                continue;
            };
            if tips.is_empty() {
                plain.push(i);
                continue;
            }
            match out.iter_mut().find(|g| g.tip_accounts == tips) {
                Some(g) => {
                    g.senders.push(i);
                    g.min_tip_lamports = g.min_tip_lamports.max(s.min_tip_lamports);
                    g.label = format!("{}+{}", g.label, s.name);
                }
                None => out.push(TipGroup {
                    senders: vec![i],
                    tip_accounts: tips,
                    min_tip_lamports: s.min_tip_lamports,
                    label: s.name.clone(),
                }),
            }
        }
        match out.first_mut() {
            Some(g) => g.senders.extend(plain),
            None => out.push(TipGroup {
                senders: plain,
                tip_accounts: vec![],
                min_tip_lamports: 0,
                label: "plain".into(),
            }),
        }
        out
    }

    /// Send one wire transaction to the given sender indices.
    pub async fn send_to(&self, idx: &[usize], wire_b64: &str) -> Vec<SendReport> {
        let picked: Vec<SenderConfig> = idx
            .iter()
            .filter_map(|i| self.senders.get(*i).cloned())
            .collect();
        Fanout {
            senders: Arc::new(picked),
            http: self.http.clone(),
        }
        .send(wire_b64)
        .await
    }

    pub fn any_accepts_jito_tip(&self) -> bool {
        self.senders
            .iter()
            .any(|s| s.accepts_jito_tip || !s.tip_accounts.is_empty())
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
                    Ok(v) if v.get("error").is_none() => SendReport {
                        sender: name,
                        ok: true,
                        ms,
                        detail: v["result"].to_string(),
                    },
                    Ok(v) => SendReport {
                        sender: name,
                        ok: false,
                        ms,
                        detail: v["error"].to_string(),
                    },
                    Err(e) => SendReport {
                        sender: name,
                        ok: false,
                        ms,
                        detail: e.to_string(),
                    },
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
                    let out: Vec<Pubkey> = arr
                        .iter()
                        .filter_map(|x| x.as_str()?.parse().ok())
                        .collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(name: &str, jito: bool, tips: &[&str]) -> SenderConfig {
        SenderConfig {
            name: name.into(),
            url: "http://x".into(),
            headers: Default::default(),
            accepts_jito_tip: jito,
            tip_accounts: tips.iter().map(|s| s.to_string()).collect(),
            min_tip_lamports: 0,
            enabled: true,
        }
    }

    #[test]
    fn groups_by_tip_family() {
        let h = Pubkey::new_unique().to_string();
        let j = vec![Pubkey::new_unique()];
        let f = Fanout::new(vec![
            sc("jito-fra", true, &[]),
            sc("helius", false, &[&h]),
            sc("rpc", false, &[]),
            sc("jito-ams", true, &[]),
        ]);
        let g = f.groups(&j);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].senders, vec![0, 3, 2], "jito group + plain rpc");
        assert_eq!(g[0].tip_accounts, j);
        assert_eq!(g[1].senders, vec![1]);
        assert_eq!(g[1].tip_accounts, vec![h.parse::<Pubkey>().unwrap()]);
        // no tips anywhere → one plain group
        let f = Fanout::new(vec![sc("rpc", false, &[])]);
        assert_eq!(
            f.groups(&[]),
            vec![TipGroup {
                senders: vec![0],
                tip_accounts: vec![],
                min_tip_lamports: 0,
                label: "plain".into()
            }]
        );
    }
}
