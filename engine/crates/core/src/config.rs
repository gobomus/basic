//! Engine configuration. `config/engine.example.toml` at the repo root is the
//! reference file and is parsed by a test below, so the two cannot drift apart.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::exit::ExitPolicy;
use crate::types::Venue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// Decide and log everything, send nothing.
    Shadow,
    /// Simulate fills against live pool state, send nothing.
    Paper,
    /// Send real transactions.
    Live,
}

/// How the raw copy size is derived, before the caps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizingMode {
    /// `copy_pct` × the leader's SOL amount.
    #[default]
    LeaderPct,
    /// `copy_amount_sol` × the share of their own SOL balance the leader spent
    /// (BasedBot "Buy %"): a leader going 10% of their stack → 10% of ours.
    BalanceFraction,
    /// `copy_amount_sol` on every copy, whatever the leader spent.
    Fixed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizingConfig {
    #[serde(default)]
    pub mode: SizingMode,
    /// Fraction of the leader's SOL amount to copy, e.g. 0.10 = 10%.
    pub copy_pct: f64,
    /// Base amount for `balance_fraction` and `fixed` modes.
    #[serde(default)]
    pub copy_amount_sol: Option<f64>,
    /// Skip buys smaller than this: fixed fees and tips dominate below it.
    pub min_buy_sol: f64,
    pub max_buy_sol: f64,
    /// Cap our buy at this fraction of the pool's SOL reserves (price impact).
    pub max_pool_impact_pct: f64,
    /// Copy the leader's follow-up buys in a token we already hold.
    pub follow_adds: bool,
    pub max_adds: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterConfig {
    pub min_leader_buy_sol: f64,
    pub max_leader_buy_sol: Option<f64>,
    /// Empty = all venues allowed.
    #[serde(default)]
    pub venues: Vec<Venue>,
    pub min_pool_sol: Option<f64>,
    /// Skip pools deeper than this (less upside left).
    #[serde(default)]
    pub max_pool_sol: Option<f64>,
    /// Market-cap range in SOL. Applied where supply is fixed and known
    /// (Pump.fun / PumpSwap: 1B tokens); otherwise not applied.
    #[serde(default)]
    pub min_market_cap_sol: Option<f64>,
    #[serde(default)]
    pub max_market_cap_sol: Option<f64>,
    /// Enter each token at most once per run, across all leaders (adds by the
    /// same leader still follow `sizing.follow_adds`).
    #[serde(default)]
    pub one_entry_per_token: bool,
    /// Never buy these mints, or tokens created by these wallets. More can be
    /// added live with `copybot ctl blacklist <address>`.
    #[serde(default)]
    pub blacklist_mints: Vec<String>,
    #[serde(default)]
    pub blacklist_devs: Vec<String>,
    pub min_token_age_secs: Option<u64>,
    pub max_token_age_secs: Option<u64>,
    pub max_detection_slot_lag: u64,
    /// Skip when the price is already this far above the leader's fill.
    pub max_price_drift_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    pub max_position_sol: f64,
    pub max_total_exposure_sol: f64,
    pub max_open_positions: u32,
    /// Always keep this much SOL for fees, rent and emergency sells.
    pub min_sol_reserve: f64,
    /// Realised loss in a UTC day that trips the kill switch.
    pub daily_loss_limit_sol: f64,
    pub max_buy_slippage_bps: u32,
    pub max_sell_slippage_bps: u32,
}

/// Per-leader overrides on top of the defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaderConfig {
    pub address: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub copy_pct: Option<f64>,
    pub max_buy_sol: Option<f64>,
    /// Name of the exit policy in `[exits]`; falls back to `default_exit`.
    pub exit: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub mode: RunMode,
    pub sizing: SizingConfig,
    pub filters: FilterConfig,
    pub risk: RiskConfig,
    pub default_exit: String,
    /// Policies evaluated counterfactually on every position (never traded).
    #[serde(default)]
    pub shadow_exits: Vec<String>,
    pub exits: BTreeMap<String, ExitPolicy>,
    #[serde(default)]
    pub leaders: Vec<LeaderConfig>,
}

impl EngineConfig {
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let cfg: EngineConfig = toml::from_str(s).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        let s = &self.sizing;
        if !(0.0 < s.copy_pct && s.copy_pct <= 10.0) {
            return Err("sizing.copy_pct must be in (0, 10]".into());
        }
        if s.mode != SizingMode::LeaderPct && !s.copy_amount_sol.is_some_and(|a| a > 0.0) {
            return Err("sizing.copy_amount_sol (> 0) is required for this sizing.mode".into());
        }
        if let (Some(a), Some(b)) = (
            self.filters.min_market_cap_sol,
            self.filters.max_market_cap_sol,
        ) {
            if a > b {
                return Err("filters.min_market_cap_sol > filters.max_market_cap_sol".into());
            }
        }
        if let (Some(a), Some(b)) = (self.filters.min_pool_sol, self.filters.max_pool_sol) {
            if a > b {
                return Err("filters.min_pool_sol > filters.max_pool_sol".into());
            }
        }
        if s.min_buy_sol > s.max_buy_sol {
            return Err("sizing.min_buy_sol > sizing.max_buy_sol".into());
        }
        for (name, p) in &self.exits {
            p.validate().map_err(|e| format!("exits.{name}: {e}"))?;
        }
        let known = |n: &String| self.exits.contains_key(n);
        if !known(&self.default_exit) {
            return Err(format!(
                "default_exit '{}' not defined in [exits]",
                self.default_exit
            ));
        }
        for n in &self.shadow_exits {
            if !known(n) {
                return Err(format!("shadow_exits: '{n}' not defined in [exits]"));
            }
        }
        for l in &self.leaders {
            if let Some(n) = &l.exit {
                if !known(n) {
                    return Err(format!("leader {}: exit '{n}' not defined", l.address));
                }
            }
        }
        Ok(())
    }

    pub fn exit_for(&self, leader: &LeaderConfig) -> &ExitPolicy {
        let name = leader.exit.as_ref().unwrap_or(&self.default_exit);
        &self.exits[name]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../../config/engine.example.toml");

    #[test]
    fn example_config_parses_and_validates() {
        let cfg = EngineConfig::from_toml(EXAMPLE).expect("example config must stay valid");
        assert_eq!(cfg.mode, RunMode::Shadow);
        assert!(!cfg.leaders.is_empty());
        for l in &cfg.leaders {
            cfg.exit_for(l);
        }
    }

    #[test]
    fn unknown_exit_rejected() {
        let bad = EXAMPLE.replace("default_exit = \"ladder_trail\"", "default_exit = \"nope\"");
        assert!(EngineConfig::from_toml(&bad).is_err());
    }
}
