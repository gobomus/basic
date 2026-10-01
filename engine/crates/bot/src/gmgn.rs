//! GMGN OpenAPI client (https://openapi.gmgn.ai) — mirrors the official
//! `gmgn-cli` (npm, github.com/GMGNAI/gmgn-skills): `X-APIKEY` header plus
//! `timestamp` (unix s, ±5 s) and `client_id` (UUID, replay-protected) query
//! params, `{code, data}` envelope. Read-only "exist auth" routes only: token
//! intelligence and wallet analytics. Never on the execution path.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

const HOST: &str = "https://openapi.gmgn.ai";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GmgnConfig {
    #[serde(default = "default_key_env")]
    pub api_key_env: String,
    /// Client-side request pacing.
    #[serde(default = "default_rps")]
    pub requests_per_sec: f64,
    /// Fetch token intelligence for every copy signal and journal it.
    #[serde(default = "yes")]
    pub enrich: bool,
    /// Optional entry gate evaluated on GMGN token intelligence.
    #[serde(default)]
    pub gate: Option<IntelGate>,
}

fn default_key_env() -> String {
    "GMGN_API_KEY".into()
}
fn default_rps() -> f64 {
    4.0
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntelGate {
    /// Max time the entry waits for GMGN data.
    pub timeout_ms: u64,
    /// When data does not arrive in time: true = enter anyway, false = skip.
    #[serde(default = "yes")]
    pub allow_on_timeout: bool,
    pub max_top10_rate: Option<f64>,
    pub max_dev_team_hold_rate: Option<f64>,
    pub max_creator_hold_rate: Option<f64>,
    pub max_insider_hold_rate: Option<f64>,
    pub max_bundler_rate: Option<f64>,
    pub max_rat_trader_rate: Option<f64>,
    pub max_rug_ratio: Option<f64>,
    pub max_sniper_count: Option<u64>,
    pub max_fresh_wallet_rate: Option<f64>,
    pub min_holders: Option<u64>,
    #[serde(default)]
    pub skip_wash_trading: bool,
    #[serde(default)]
    pub require_mint_renounced: bool,
    #[serde(default)]
    pub require_freeze_renounced: bool,
    /// Skip if the creator has already sold out.
    #[serde(default)]
    pub skip_if_dev_sold: bool,
}

/// The GMGN / Axiom "token panel" at decision time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenIntel {
    pub holders: Option<u64>,
    pub top10_rate: Option<f64>,
    pub dev_team_hold_rate: Option<f64>,
    pub creator_hold_rate: Option<f64>,
    pub creator_sold: Option<bool>,
    pub insider_hold_rate: Option<f64>,
    pub bundler_rate: Option<f64>,
    pub rat_trader_rate: Option<f64>,
    pub entrapment_rate: Option<f64>,
    pub fresh_wallet_rate: Option<f64>,
    pub bot_degen_rate: Option<f64>,
    pub sniper_count: Option<u64>,
    pub rug_ratio: Option<f64>,
    pub wash_trading: Option<bool>,
    pub mint_renounced: Option<bool>,
    pub freeze_renounced: Option<bool>,
    pub lp_burned: Option<bool>,
    pub smart_wallets: Option<u64>,
    pub kol_wallets: Option<u64>,
    pub sniper_wallets: Option<u64>,
    pub bundler_wallets: Option<u64>,
    pub whale_wallets: Option<u64>,
    pub creator_open_count: Option<u64>,
    pub creator_ath_mc_usd: Option<f64>,
    pub cto: Option<bool>,
    pub dexscreener_ad: Option<bool>,
    pub dexscreener_boost: Option<bool>,
    pub twitter_renames: Option<u64>,
    pub twitter: Option<String>,
    pub telegram: Option<String>,
    pub website: Option<String>,
    pub launchpad: Option<String>,
    pub launchpad_progress: Option<f64>,
    pub liquidity_usd: Option<f64>,
    pub price_usd: Option<f64>,
    pub volume_5m_usd: Option<f64>,
    pub buys_5m: Option<u64>,
    pub sells_5m: Option<u64>,
    pub hot_level: Option<u64>,
    pub creation_ts: Option<i64>,
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.is_empty() => s.parse().ok(),
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    }
}
fn int(v: &Value) -> Option<u64> {
    num(v).filter(|x| *x >= 0.0).map(|x| x as u64)
}
fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|x| x != 0.0),
        Value::String(s) => match s.to_lowercase().as_str() {
            "yes" | "true" | "1" | "burn" => Some(true),
            "no" | "false" | "0" => Some(false),
            "" | "unknown" => None,
            _ => None,
        },
        _ => None,
    }
}
fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

impl TokenIntel {
    /// Build from `GET /v1/token/info` and `GET /v1/token/security` payloads.
    /// Every field is optional: unknown or renamed fields degrade to `None`.
    pub fn from_payloads(info: &Value, sec: &Value) -> Self {
        let stat = &info["stat"];
        let tags = &info["wallet_tags_stat"];
        let dev = &info["dev"];
        let link = &info["link"];
        let price = &info["price"];
        Self {
            holders: int(&info["holder_count"]).or_else(|| int(&stat["holder_count"])),
            top10_rate: num(&sec["top_10_holder_rate"])
                .or_else(|| num(&stat["top_10_holder_rate"])),
            dev_team_hold_rate: num(&sec["dev_team_hold_rate"])
                .or_else(|| num(&stat["dev_team_hold_rate"])),
            creator_hold_rate: num(&sec["creator_balance_rate"])
                .or_else(|| num(&stat["creator_hold_rate"])),
            creator_sold: text(&sec["creator_token_status"])
                .map(|s| s == "creator_close")
                .or_else(|| text(&dev["creator_token_status"]).map(|s| s == "sell")),
            insider_hold_rate: num(&sec["suspected_insider_hold_rate"]),
            bundler_rate: num(&sec["bundler_trader_amount_rate"])
                .or_else(|| num(&stat["top_bundler_trader_percentage"])),
            rat_trader_rate: num(&sec["rat_trader_amount_rate"])
                .or_else(|| num(&stat["top_rat_trader_percentage"])),
            entrapment_rate: num(&stat["top_entrapment_trader_percentage"]),
            fresh_wallet_rate: num(&stat["fresh_wallet_rate"]),
            bot_degen_rate: num(&stat["bot_degen_rate"]),
            sniper_count: int(&sec["sniper_count"]),
            rug_ratio: num(&sec["rug_ratio"]),
            wash_trading: flag(&sec["is_wash_trading"]),
            mint_renounced: flag(&sec["renounced_mint"]),
            freeze_renounced: flag(&sec["renounced_freeze_account"]),
            lp_burned: flag(&sec["burn_status"]),
            smart_wallets: int(&tags["smart_wallets"]),
            kol_wallets: int(&tags["renowned_wallets"]),
            sniper_wallets: int(&tags["sniper_wallets"]),
            bundler_wallets: int(&tags["bundler_wallets"]),
            whale_wallets: int(&tags["whale_wallets"]),
            creator_open_count: int(&dev["creator_open_count"]),
            creator_ath_mc_usd: num(&dev["ath_token_info"]["ath_mc"]),
            cto: flag(&dev["cto_flag"]),
            dexscreener_ad: flag(&dev["dexscr_ad"]),
            dexscreener_boost: flag(&dev["dexscr_boost_fee"]),
            twitter_renames: dev["twitter_name_change_history"]
                .as_array()
                .map(|a| a.len() as u64),
            twitter: text(&link["twitter_username"]),
            telegram: text(&link["telegram"]),
            website: text(&link["website"]),
            launchpad: text(&info["launchpad"]),
            launchpad_progress: num(&info["launchpad_progress"]),
            liquidity_usd: num(&info["liquidity"]),
            price_usd: num(&price["price"]),
            volume_5m_usd: num(&price["volume_5m"]),
            buys_5m: int(&price["buys_5m"]),
            sells_5m: int(&price["sells_5m"]),
            hot_level: int(&price["hot_level"]),
            creation_ts: num(&info["creation_timestamp"]).map(|x| x as i64),
        }
    }
}

impl IntelGate {
    /// `Ok(())` to enter, `Err(reason)` to skip.
    pub fn check(&self, t: &TokenIntel) -> Result<(), String> {
        fn over(name: &str, v: Option<f64>, max: Option<f64>) -> Result<(), String> {
            match (v, max) {
                (Some(v), Some(m)) if v > m => Err(format!("{name} {v:.3} > {m}")),
                _ => Ok(()),
            }
        }
        over("top10", t.top10_rate, self.max_top10_rate)?;
        over(
            "dev_team_hold",
            t.dev_team_hold_rate,
            self.max_dev_team_hold_rate,
        )?;
        over(
            "creator_hold",
            t.creator_hold_rate,
            self.max_creator_hold_rate,
        )?;
        over(
            "insider_hold",
            t.insider_hold_rate,
            self.max_insider_hold_rate,
        )?;
        over("bundler", t.bundler_rate, self.max_bundler_rate)?;
        over("rat_trader", t.rat_trader_rate, self.max_rat_trader_rate)?;
        over("rug_ratio", t.rug_ratio, self.max_rug_ratio)?;
        over(
            "fresh_wallets",
            t.fresh_wallet_rate,
            self.max_fresh_wallet_rate,
        )?;
        over(
            "snipers",
            t.sniper_count.map(|x| x as f64),
            self.max_sniper_count.map(|x| x as f64),
        )?;
        if let (Some(h), Some(m)) = (t.holders, self.min_holders) {
            if h < m {
                return Err(format!("holders {h} < {m}"));
            }
        }
        if self.skip_wash_trading && t.wash_trading == Some(true) {
            return Err("wash trading detected".into());
        }
        if self.require_mint_renounced && t.mint_renounced == Some(false) {
            return Err("mint authority not renounced".into());
        }
        if self.require_freeze_renounced && t.freeze_renounced == Some(false) {
            return Err("freeze authority not renounced".into());
        }
        if self.skip_if_dev_sold && t.creator_sold == Some(true) {
            return Err("creator already sold".into());
        }
        Ok(())
    }
}

pub struct Gmgn {
    key: String,
    http: reqwest::Client,
    min_gap: Duration,
    last: Mutex<tokio::time::Instant>,
}

fn uuid_v4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

impl Gmgn {
    pub fn new(key: String, requests_per_sec: f64) -> Self {
        Self {
            key,
            http: chain::rpc::http_client(Duration::from_secs(8)),
            min_gap: Duration::from_secs_f64(1.0 / requests_per_sec.max(0.1)),
            last: Mutex::new(tokio::time::Instant::now() - Duration::from_secs(10)),
        }
    }

    pub fn from_config(c: &GmgnConfig) -> Option<Self> {
        std::env::var(&c.api_key_env)
            .ok()
            .filter(|k| !k.is_empty())
            .map(|k| Self::new(k, c.requests_per_sec))
    }

    async fn pace(&self) {
        let mut last = self.last.lock().await;
        let next = *last + self.min_gap;
        let now = tokio::time::Instant::now();
        if next > now {
            tokio::time::sleep_until(next).await;
        }
        *last = tokio::time::Instant::now();
    }

    pub fn auth_query() -> Vec<(String, String)> {
        vec![
            (
                "timestamp".into(),
                chrono::Utc::now().timestamp().to_string(),
            ),
            ("client_id".into(), uuid_v4()),
        ]
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        mut query: Vec<(String, String)>,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        for attempt in 0..2 {
            self.pace().await;
            query.retain(|(k, _)| k != "timestamp" && k != "client_id");
            query.extend(Self::auth_query());
            let mut req = self
                .http
                .request(method.clone(), format!("{HOST}{path}"))
                .query(&query)
                .header("X-APIKEY", &self.key)
                .header("Content-Type", "application/json")
                .header("User-Agent", "copybot");
            if let Some(b) = &body {
                req = req.json(b);
            }
            let res = req.send().await?;
            let status = res.status();
            let reset = res
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok());
            let v: Value = res
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("{path}: HTTP {status}, non-JSON body ({e})"))?;
            if v["code"].as_i64() == Some(0) {
                return Ok(v["data"].clone());
            }
            if status.as_u16() == 429 && attempt == 0 {
                let wait = reset
                    .map(|r| (r - chrono::Utc::now().timestamp()).clamp(1, 10) as u64)
                    .unwrap_or(2);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            anyhow::bail!(
                "{path}: HTTP {status} code {} {}",
                v["code"],
                v["message"].as_str().or(v["msg"].as_str()).unwrap_or("")
            );
        }
        anyhow::bail!("{path}: rate limited")
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    pub async fn token_info(&self, mint: &str) -> anyhow::Result<Value> {
        self.request(
            reqwest::Method::GET,
            "/v1/token/info",
            Self::q(&[("chain", "sol"), ("address", mint)]),
            None,
        )
        .await
    }

    pub async fn token_security(&self, mint: &str) -> anyhow::Result<Value> {
        self.request(
            reqwest::Method::GET,
            "/v1/token/security",
            Self::q(&[("chain", "sol"), ("address", mint)]),
            None,
        )
        .await
    }

    /// Both calls in parallel → the decision-time token panel (+ raw payloads for the journal).
    pub async fn token_intel(&self, mint: &str) -> anyhow::Result<(TokenIntel, Value)> {
        let (i, s) = tokio::join!(self.token_info(mint), self.token_security(mint));
        let (i, s) = (i.unwrap_or(Value::Null), s.unwrap_or(Value::Null));
        anyhow::ensure!(!(i.is_null() && s.is_null()), "no GMGN data for {mint}");
        Ok((
            TokenIntel::from_payloads(&i, &s),
            serde_json::json!({"info": i, "security": s}),
        ))
    }

    pub async fn top_holders(
        &self,
        mint: &str,
        tag: Option<&str>,
        limit: u32,
    ) -> anyhow::Result<Value> {
        let lim = limit.to_string();
        let mut q = Self::q(&[("chain", "sol"), ("address", mint), ("limit", &lim)]);
        if let Some(t) = tag {
            q.push(("tag".into(), t.into()));
        }
        self.request(
            reqwest::Method::GET,
            "/v1/market/token_top_holders",
            q,
            None,
        )
        .await
    }

    /// Batch trading stats (`period` = 7d / 30d). Wallets are repeated query params.
    pub async fn wallet_stats(&self, wallets: &[String], period: &str) -> anyhow::Result<Value> {
        let mut q = Self::q(&[("chain", "sol"), ("period", period)]);
        q.extend(
            wallets
                .iter()
                .map(|w| ("wallet_address".to_string(), w.clone())),
        );
        self.request(reqwest::Method::GET, "/v1/user/wallet_stats", q, None)
            .await
    }

    pub async fn smartmoney_trades(&self, limit: u32) -> anyhow::Result<Value> {
        self.request(
            reqwest::Method::GET,
            "/v1/user/smartmoney",
            Self::q(&[("chain", "sol"), ("limit", &limit.to_string())]),
            None,
        )
        .await
    }

    pub async fn kol_trades(&self, limit: u32) -> anyhow::Result<Value> {
        self.request(
            reqwest::Method::GET,
            "/v1/user/kol",
            Self::q(&[("chain", "sol"), ("limit", &limit.to_string())]),
            None,
        )
        .await
    }

    pub async fn created_tokens(&self, wallet: &str) -> anyhow::Result<Value> {
        self.request(
            reqwest::Method::GET,
            "/v1/user/created_tokens",
            Self::q(&[("chain", "sol"), ("wallet_address", wallet)]),
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intel_parses_documented_fields_and_tolerates_strings() {
        let info = json!({
            "holder_count": 1234, "launchpad": "pump", "launchpad_progress": "0.82", "liquidity": "45000.5",
            "stat": {"fresh_wallet_rate": 0.21, "bot_degen_rate": "0.05", "top_entrapment_trader_percentage": 0.01},
            "wallet_tags_stat": {"smart_wallets": 4, "renowned_wallets": 2, "sniper_wallets": 9, "bundler_wallets": 3, "whale_wallets": 1},
            "dev": {"creator_open_count": 17, "cto_flag": 0, "dexscr_ad": 1, "dexscr_boost_fee": 0, "creator_token_status": "hold",
                    "twitter_name_change_history": [{"twitter_username": "a"}, {"twitter_username": "b"}],
                    "ath_token_info": {"ath_mc": "1250000"}},
            "link": {"twitter_username": "coin", "telegram": "", "website": "https://x.y"},
            "price": {"price": "0.0001", "volume_5m": 9000, "buys_5m": 120, "sells_5m": 80, "hot_level": 2},
            "creation_timestamp": 1790000000
        });
        let sec = json!({
            "top_10_holder_rate": "0.31", "dev_team_hold_rate": 0.02, "creator_balance_rate": 0.0, "creator_token_status": "creator_close",
            "suspected_insider_hold_rate": 0.08, "rug_ratio": 0.1, "is_wash_trading": false, "rat_trader_amount_rate": 0.04,
            "bundler_trader_amount_rate": 0.12, "sniper_count": 6, "renounced_mint": true, "renounced_freeze_account": true, "burn_status": "burn"
        });
        let t = TokenIntel::from_payloads(&info, &sec);
        assert_eq!(t.holders, Some(1234));
        assert_eq!(t.top10_rate, Some(0.31));
        assert_eq!(t.creator_sold, Some(true));
        assert_eq!(t.smart_wallets, Some(4));
        assert_eq!(t.kol_wallets, Some(2));
        assert_eq!(t.creator_open_count, Some(17));
        assert_eq!(t.creator_ath_mc_usd, Some(1_250_000.0));
        assert_eq!(t.dexscreener_ad, Some(true));
        assert_eq!(t.twitter_renames, Some(2));
        assert_eq!(t.telegram, None);
        assert_eq!(t.lp_burned, Some(true));
        assert_eq!(t.launchpad_progress, Some(0.82));
        assert_eq!(t.bot_degen_rate, Some(0.05));

        let empty = TokenIntel::from_payloads(&Value::Null, &Value::Null);
        assert_eq!(empty, TokenIntel::default());
    }

    #[test]
    fn gate_rules() {
        let g = IntelGate {
            timeout_ms: 300,
            allow_on_timeout: true,
            max_top10_rate: Some(0.5),
            max_dev_team_hold_rate: None,
            max_creator_hold_rate: None,
            max_insider_hold_rate: Some(0.2),
            max_bundler_rate: Some(0.3),
            max_rat_trader_rate: None,
            max_rug_ratio: Some(0.5),
            max_sniper_count: Some(20),
            max_fresh_wallet_rate: None,
            min_holders: Some(50),
            skip_wash_trading: true,
            require_mint_renounced: true,
            require_freeze_renounced: true,
            skip_if_dev_sold: false,
        };
        let ok = TokenIntel {
            top10_rate: Some(0.3),
            holders: Some(400),
            mint_renounced: Some(true),
            freeze_renounced: Some(true),
            ..Default::default()
        };
        assert!(g.check(&ok).is_ok());
        assert!(g
            .check(&TokenIntel {
                top10_rate: Some(0.7),
                ..ok.clone()
            })
            .unwrap_err()
            .contains("top10"));
        assert!(g
            .check(&TokenIntel {
                holders: Some(10),
                ..ok.clone()
            })
            .is_err());
        assert!(g
            .check(&TokenIntel {
                wash_trading: Some(true),
                ..ok.clone()
            })
            .is_err());
        assert!(g
            .check(&TokenIntel {
                freeze_renounced: Some(false),
                ..ok.clone()
            })
            .is_err());
        // unknown values never block
        assert!(g.check(&TokenIntel::default()).is_ok());
    }

    #[test]
    fn uuid_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }
}
