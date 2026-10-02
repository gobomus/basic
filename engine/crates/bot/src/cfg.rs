//! Full bot configuration: the strategy config (engine-core, validated there)
//! plus `[infra]` (endpoints, fees, wallet, alerts, storage). Secrets are never
//! stored in the file, only the *names* of environment variables holding them.

use std::path::Path;

use chain::geyser::GeyserConfig;
use chain::sender::SenderConfig;
use chain::wsfeed::WsConfig;
use engine_core::config::EngineConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeeConfig {
    pub cu_limit_buy: u32,
    pub cu_limit_sell: u32,
    pub cu_price_micro_lamports: u64,
    /// Tip per transaction (lamports) paid to a Jito tip account.
    pub tip_lamports_buy: u64,
    pub tip_lamports_sell: u64,
    /// Tip multiplier for stop-loss / emergency exits.
    #[serde(default = "two")]
    pub urgent_tip_multiplier: u64,
}

fn two() -> u64 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalletConfig {
    /// Encrypted keystore file created with `copybot wallet new`.
    pub keystore: String,
    /// Env var holding the keystore passphrase (prompted if unset).
    #[serde(default = "default_pass_env")]
    pub passphrase_env: String,
}

fn default_pass_env() -> String {
    "KEYSTORE_PASSPHRASE".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Always-on append-only JSONL journal.
    #[serde(default = "default_journal")]
    pub journal_dir: String,
    /// Env var with a Postgres URL (schema/postgres). Optional.
    #[serde(default)]
    pub postgres_url_env: Option<String>,
    /// ClickHouse HTTP endpoint for the market firehose (schema/clickhouse). Optional.
    #[serde(default)]
    pub clickhouse_url: Option<String>,
}

fn default_journal() -> String {
    "data/journal".into()
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            journal_dir: default_journal(),
            postgres_url_env: None,
            clickhouse_url: None,
        }
    }
}

/// Simulated execution for `paper` / `shadow` modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperConfig {
    /// Time from detecting a leader trade to our simulated transaction landing,
    /// in ms. The fill is priced from the pool as it is *then*, and a buy whose
    /// price ran past `max_buy_slippage_bps` fails, exactly as on-chain.
    /// 0 = instant fill at the leader's post-trade price (optimistic).
    #[serde(default = "default_paper_latency")]
    pub latency_ms: u64,
    /// Keep tracking a closed position's price this long so every alternative
    /// exit policy can be scored on the same path.
    #[serde(default = "default_afterlife")]
    pub afterlife_secs: u64,
}

fn default_paper_latency() -> u64 {
    1000
}
fn default_afterlife() -> u64 {
    7200
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            latency_ms: default_paper_latency(),
            afterlife_secs: default_afterlife(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JupiterConfig {
    /// e.g. https://lite-api.jup.ag/swap/v1 (free) or https://api.jup.ag/swap/v1 (keyed)
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfraConfig {
    /// Env var holding the RPC URL (often contains an API key).
    pub rpc_url_env: String,
    /// Paid Yellowstone gRPC feed (fastest). Leave out to use the free `[infra.ws]` feed.
    #[serde(default)]
    pub geyser: Option<GeyserConfig>,
    /// Free feed: standard RPC WebSocket + `getTransaction`. Used when `geyser` is absent.
    #[serde(default)]
    pub ws: Option<WsConfig>,
    #[serde(default)]
    pub paper: PaperConfig,
    /// Block engine used to fetch Jito tip accounts.
    #[serde(default = "default_block_engine")]
    pub jito_block_engine: String,
    /// Explicit tip accounts (skip the fetch). Leave empty to fetch.
    #[serde(default)]
    pub tip_accounts: Vec<String>,
    pub senders: Vec<SenderConfig>,
    pub fees: FeeConfig,
    pub wallet: WalletConfig,
    /// Durable nonce accounts (created with `copybot wallet nonce-create`). With
    /// two or more tip families among the senders, each order is signed once per
    /// family on a shared nonce, so only one variant can ever land.
    #[serde(default)]
    pub nonce_accounts: Vec<String>,
    /// Unix socket for `copybot ctl` (status, pause, flatten, ...).
    #[serde(default = "default_socket")]
    pub control_socket: String,
    #[serde(default)]
    pub storage: StorageConfig,
    /// GMGN OpenAPI: token intelligence + wallet analytics (optional).
    #[serde(default)]
    pub gmgn: Option<crate::gmgn::GmgnConfig>,
    /// Fallback router for venues without a direct builder.
    #[serde(default)]
    pub jupiter: Option<JupiterConfig>,
    /// Seconds without a slot update before entries are paused.
    #[serde(default = "five")]
    pub feed_stale_secs: u64,
}

fn default_socket() -> String {
    "data/copybot.sock".into()
}

fn default_block_engine() -> String {
    "https://mainnet.block-engine.jito.wtf".into()
}
fn five() -> u64 {
    5
}

#[derive(Debug, Clone)]
pub struct BotConfig {
    pub engine: EngineConfig,
    pub infra: InfraConfig,
}

impl BotConfig {
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        let mut v: toml::Table = toml::from_str(s)?;
        let infra = v
            .remove("infra")
            .ok_or_else(|| anyhow::anyhow!("missing [infra] section"))?;
        let infra: InfraConfig = infra.try_into()?;
        let engine =
            EngineConfig::from_toml(&toml::to_string(&v)?).map_err(|e| anyhow::anyhow!(e))?;
        anyhow::ensure!(!infra.senders.is_empty(), "infra.senders is empty");
        anyhow::ensure!(
            infra.geyser.is_some() || infra.ws.is_some(),
            "no data feed: add [infra.ws] (free) or [infra.geyser] (paid gRPC)"
        );
        for snd in &infra.senders {
            for t in &snd.tip_accounts {
                t.parse::<chain::solana_sdk::pubkey::Pubkey>()
                    .map_err(|e| {
                        anyhow::anyhow!("sender {}: bad tip account {t}: {e}", snd.name)
                    })?;
            }
        }
        for n in &infra.nonce_accounts {
            n.parse::<chain::solana_sdk::pubkey::Pubkey>()
                .map_err(|e| anyhow::anyhow!("nonce account {n}: {e}"))?;
        }
        anyhow::ensure!(
            infra.fees.tip_lamports_buy == 0
                || infra
                    .senders
                    .iter()
                    .any(|s| (s.accepts_jito_tip || !s.tip_accounts.is_empty()) && s.enabled),
            "tips configured but no sender accepts Jito tips"
        );
        Ok(Self { engine, infra })
    }

    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let p = path.as_ref();
        Self::from_toml(
            &std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?,
        )
    }

    pub fn rpc_url(&self) -> anyhow::Result<String> {
        env(&self.infra.rpc_url_env)
    }
}

pub fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("environment variable {name} is not set"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poc_example_parses_and_uses_the_free_feed() {
        let s = include_str!("../../../../config/poc.example.toml");
        let c = BotConfig::from_toml(s).expect("config/poc.example.toml must stay valid");
        assert_eq!(c.engine.mode, engine_core::config::RunMode::Paper);
        assert!(c.infra.geyser.is_none() && c.infra.ws.is_some());
        assert_eq!(c.infra.fees.tip_lamports_buy, 0, "free POC pays no tips");
        assert_eq!(c.engine.filters.venues.len(), 2);
        assert!(c.infra.paper.latency_ms > 0);
    }

    #[test]
    fn a_feed_is_required() {
        let s = include_str!("../../../../config/poc.example.toml")
            .replace("[infra.ws]", "[infra.unused_ws]");
        assert!(BotConfig::from_toml(&s).is_err());
    }

    #[test]
    fn production_example_parses() {
        let s = include_str!("../../../../config/copybot.example.toml");
        let c = BotConfig::from_toml(s).expect("config/copybot.example.toml must stay valid");
        assert!(!c.infra.senders.is_empty());
        assert_eq!(c.engine.mode, engine_core::config::RunMode::Shadow);
    }
}
