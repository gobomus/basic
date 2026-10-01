//! Leader-wallet selection: is this wallet worth copying *for us*?
//!
//! A wallet's own PnL is not the target. What matters is the return a copier
//! gets after entering a few slots later at a worse price (the imitation
//! penalty). `copy_ret` on each round trip is that simulated copier return,
//! computed by the research pipeline from recorded pool trades.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::stats::{median, profit_factor};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundTrip {
    pub entry_t_ms: i64,
    pub exit_t_ms: i64,
    pub cost_sol: f64,
    pub proceeds_sol: f64,
    /// Slots between token creation and the leader's first buy.
    pub entry_slots_after_creation: Option<u64>,
    /// Simulated return for a copier entering `copy_delay_slots` later.
    pub copy_ret: Option<f64>,
}

impl RoundTrip {
    pub fn ret(&self) -> f64 {
        if self.cost_sol > 0.0 {
            self.proceeds_sol / self.cost_sol - 1.0
        } else {
            f64::NAN
        }
    }
    pub fn hold_secs(&self) -> f64 {
        (self.exit_t_ms - self.entry_t_ms) as f64 / 1000.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalletScoreConfig {
    pub min_trades: usize,
    /// Wallets that flip in seconds can't be copied profitably.
    pub min_median_hold_secs: f64,
    /// An entry within this many slots of creation counts as a snipe.
    pub sniper_slot_window: u64,
    /// Reject wallets whose entries are mostly snipes (insider / bundle risk).
    pub max_sniper_share: f64,
    /// Reject wallets active in more distinct UTC hours per day than this (bots).
    pub max_active_hours_per_day: f64,
    pub min_copy_median_ret: f64,
    /// Bayesian-style shrinkage: score *= n / (n + prior_trades).
    pub prior_trades: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    TooFewTrades,
    HoldTooShort,
    MostlySnipes,
    LooksLikeBot,
    NoCopySimulation,
    CopyReturnTooLow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalletReport {
    pub n: usize,
    pub win_rate: f64,
    pub median_ret: f64,
    pub median_hold_secs: f64,
    pub profit_factor: Option<f64>,
    pub sniper_share: f64,
    pub active_hours_per_day: f64,
    pub copy_n: usize,
    pub copy_median_ret: Option<f64>,
    /// Imitation penalty: leader median return minus copier median return.
    pub imitation_penalty: Option<f64>,
    pub score: f64,
    pub rejects: Vec<RejectReason>,
}

impl WalletReport {
    pub fn eligible(&self) -> bool {
        self.rejects.is_empty()
    }
}

/// Average number of distinct UTC hours with activity, per active UTC day.
fn active_hours_per_day(trips: &[RoundTrip]) -> f64 {
    let mut days: BTreeMap<i64, u32> = BTreeMap::new();
    for t in trips {
        for ts in [t.entry_t_ms, t.exit_t_ms] {
            let hour = ts.div_euclid(3_600_000);
            let day = hour.div_euclid(24);
            let bit = 1u32 << hour.rem_euclid(24);
            *days.entry(day).or_default() |= bit;
        }
    }
    if days.is_empty() {
        return 0.0;
    }
    days.values().map(|m| m.count_ones() as f64).sum::<f64>() / days.len() as f64
}

pub fn score_wallet(cfg: &WalletScoreConfig, trips: &[RoundTrip]) -> WalletReport {
    let rets: Vec<f64> = trips.iter().map(RoundTrip::ret).collect();
    let pnls: Vec<f64> = trips.iter().map(|t| t.proceeds_sol - t.cost_sol).collect();
    let holds: Vec<f64> = trips.iter().map(RoundTrip::hold_secs).collect();
    let copy: Vec<f64> = trips.iter().filter_map(|t| t.copy_ret).collect();
    let n = trips.len();

    let snipes = trips
        .iter()
        .filter(|t| {
            t.entry_slots_after_creation
                .is_some_and(|s| s <= cfg.sniper_slot_window)
        })
        .count();
    let sniper_share = if n > 0 { snipes as f64 / n as f64 } else { 0.0 };
    let median_ret = median(&rets).unwrap_or(f64::NAN);
    let median_hold_secs = median(&holds).unwrap_or(0.0);
    let copy_median_ret = median(&copy);
    let hours = active_hours_per_day(trips);

    let mut rejects = Vec::new();
    if n < cfg.min_trades {
        rejects.push(RejectReason::TooFewTrades);
    }
    if median_hold_secs < cfg.min_median_hold_secs {
        rejects.push(RejectReason::HoldTooShort);
    }
    if sniper_share > cfg.max_sniper_share {
        rejects.push(RejectReason::MostlySnipes);
    }
    if hours > cfg.max_active_hours_per_day {
        rejects.push(RejectReason::LooksLikeBot);
    }
    match copy_median_ret {
        _ if copy.len() < cfg.min_trades => rejects.push(RejectReason::NoCopySimulation),
        Some(c) if c < cfg.min_copy_median_ret => rejects.push(RejectReason::CopyReturnTooLow),
        _ => {}
    }

    let shrink = copy.len() as f64 / (copy.len() as f64 + cfg.prior_trades);
    let score = copy_median_ret.map_or(0.0, |c| c * shrink);

    WalletReport {
        n,
        win_rate: if n > 0 {
            rets.iter().filter(|r| **r > 0.0).count() as f64 / n as f64
        } else {
            0.0
        },
        median_ret,
        median_hold_secs,
        profit_factor: profit_factor(&pnls),
        sniper_share,
        active_hours_per_day: hours,
        copy_n: copy.len(),
        copy_median_ret,
        imitation_penalty: copy_median_ret.map(|c| median_ret - c),
        score,
        rejects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WalletScoreConfig {
        WalletScoreConfig {
            min_trades: 3,
            min_median_hold_secs: 60.0,
            sniper_slot_window: 2,
            max_sniper_share: 0.5,
            max_active_hours_per_day: 16.0,
            min_copy_median_ret: 0.05,
            prior_trades: 20.0,
        }
    }

    fn trip(
        entry_h: i64,
        hold_s: i64,
        ret: f64,
        copy: Option<f64>,
        slots: Option<u64>,
    ) -> RoundTrip {
        let e = entry_h * 3_600_000;
        RoundTrip {
            entry_t_ms: e,
            exit_t_ms: e + hold_s * 1000,
            cost_sol: 1.0,
            proceeds_sol: 1.0 + ret,
            entry_slots_after_creation: slots,
            copy_ret: copy,
        }
    }

    #[test]
    fn good_wallet_is_eligible() {
        let trips = vec![
            trip(1, 600, 0.8, Some(0.4), Some(500)),
            trip(3, 900, -0.3, Some(-0.4), Some(800)),
            trip(5, 1200, 1.5, Some(0.9), None),
            trip(30, 300, 0.2, Some(0.1), Some(50)),
        ];
        let r = score_wallet(&cfg(), &trips);
        assert!(r.eligible(), "{:?}", r.rejects);
        assert_eq!(r.n, 4);
        assert!((r.copy_median_ret.unwrap() - 0.25).abs() < 1e-12);
        assert!((r.imitation_penalty.unwrap() - 0.25).abs() < 1e-12);
        assert!(r.score > 0.0 && r.score < 0.25);
    }

    #[test]
    fn flipper_sniper_bot_rejected() {
        let trips: Vec<RoundTrip> = (0..24)
            .map(|h| trip(h, 5, 0.3, Some(-0.2), Some(0)))
            .collect();
        let r = score_wallet(&cfg(), &trips);
        assert!(r.rejects.contains(&RejectReason::HoldTooShort));
        assert!(r.rejects.contains(&RejectReason::MostlySnipes));
        assert!(r.rejects.contains(&RejectReason::LooksLikeBot));
        assert!(r.rejects.contains(&RejectReason::CopyReturnTooLow));
    }

    #[test]
    fn needs_copy_simulation() {
        let trips: Vec<RoundTrip> = (0..5).map(|i| trip(i * 2, 600, 0.5, None, None)).collect();
        let r = score_wallet(&cfg(), &trips);
        assert_eq!(r.rejects, vec![RejectReason::NoCopySimulation]);
        assert_eq!(r.score, 0.0);
    }
}
