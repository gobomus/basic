//! Counterfactual replay: run an exit policy over a recorded post-entry event
//! path and compute what it would have earned after costs.
//!
//! Because the exit is decoupled from the leader, every position's full
//! post-entry path (all pool trades, leader sells, dev sells, liquidity) is
//! recorded. Any candidate policy can then be scored on every historical
//! position — this is how exits get tuned and how challengers earn promotion.

use serde::{Deserialize, Serialize};

use crate::exit::{ExitAction, ExitPolicy, MarketEvent, PositionState};
use crate::stats::{mean, median, profit_factor};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    /// Venue + platform fee on a sale, in basis points (e.g. 125 on the Pump curve).
    pub fee_bps: f64,
    /// Assumed price impact of a sale, in basis points, below the observed price.
    pub slippage_bps: f64,
    /// Priority fee + tip per transaction, in SOL.
    pub fixed_sol_per_tx: f64,
    /// Time from the exit decision to the sale landing; it executes at the price
    /// the path shows then. Zero sells at the decision price.
    pub exit_latency_ms: i64,
}

impl CostModel {
    fn sell_haircut(&self) -> f64 {
        1.0 - (self.fee_bps + self.slippage_bps) / 10_000.0
    }
}

/// Latest price on the path at or before `t_ms`, looking forward from event `from`.
fn price_at(events: &[MarketEvent], from: usize, t_ms: i64, fallback: f64) -> f64 {
    let mut px = fallback;
    for ev in &events[from..] {
        if ev.t_ms() > t_ms {
            break;
        }
        if let MarketEvent::Price { price, .. } = ev {
            px = *price;
        }
    }
    px
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathInput<'a> {
    /// What was actually paid per token: the average fill price, fees and price
    /// impact included, so the buy needs no further cost here.
    pub entry_price: f64,
    pub entry_t_ms: i64,
    pub entry_pool_sol: Option<f64>,
    pub size_sol: f64,
    /// Events after entry, ordered by time.
    pub events: &'a [MarketEvent],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayResult {
    pub pnl_sol: f64,
    /// pnl / size.
    pub ret: f64,
    pub exits: Vec<ExitAction>,
    /// False if the path ended with tokens still held (marked to last price).
    pub fully_closed: bool,
    pub held_ms: i64,
}

pub fn replay(policy: &ExitPolicy, input: &PathInput, cost: &CostModel) -> ReplayResult {
    let mut st = PositionState::open(
        policy,
        input.entry_price,
        input.entry_t_ms,
        input.entry_pool_sol,
    );
    let tokens = input.size_sol / input.entry_price;
    let mut cash = -input.size_sol - cost.fixed_sol_per_tx;
    let mut exits = Vec::new();
    let mut last_t = input.entry_t_ms;

    for (i, ev) in input.events.iter().enumerate() {
        last_t = ev.t_ms();
        for a in policy.on_event(&mut st, ev) {
            let px = price_at(input.events, i, a.t_ms + cost.exit_latency_ms, a.ref_price);
            cash += tokens * a.sell_fraction * px * cost.sell_haircut() - cost.fixed_sol_per_tx;
            exits.push(a);
        }
        if st.is_closed() {
            break;
        }
    }

    let fully_closed = st.is_closed();
    if !fully_closed {
        // the path ended with tokens held: mark them out at the last price
        cash += tokens * st.remaining * st.last_price * cost.sell_haircut() - cost.fixed_sol_per_tx;
    }
    let held_ms = exits.last().map_or(last_t, |a| a.t_ms) - input.entry_t_ms;
    ReplayResult {
        pnl_sol: cash,
        ret: if input.size_sol > 0.0 {
            cash / input.size_sol
        } else {
            0.0
        },
        exits,
        fully_closed,
        held_ms,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyStats {
    pub n: usize,
    pub total_pnl_sol: f64,
    pub mean_ret: f64,
    pub median_ret: f64,
    pub win_rate: f64,
    pub profit_factor: Option<f64>,
    pub worst_ret: f64,
    pub open_at_end: usize,
}

pub fn evaluate(policy: &ExitPolicy, paths: &[PathInput], cost: &CostModel) -> Option<PolicyStats> {
    if paths.is_empty() {
        return None;
    }
    let results: Vec<ReplayResult> = paths.iter().map(|p| replay(policy, p, cost)).collect();
    let rets: Vec<f64> = results.iter().map(|r| r.ret).collect();
    let pnls: Vec<f64> = results.iter().map(|r| r.pnl_sol).collect();
    Some(PolicyStats {
        n: results.len(),
        total_pnl_sol: pnls.iter().sum(),
        mean_ret: mean(&rets)?,
        median_ret: median(&rets)?,
        win_rate: rets.iter().filter(|r| **r > 0.0).count() as f64 / rets.len() as f64,
        profit_factor: profit_factor(&pnls),
        worst_ret: rets.iter().copied().fold(f64::INFINITY, f64::min),
        open_at_end: results.iter().filter(|r| !r.fully_closed).count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::{TpLevel, Trailing};

    const FREE: CostModel = CostModel {
        fee_bps: 0.0,
        slippage_bps: 0.0,
        fixed_sol_per_tx: 0.0,
        exit_latency_ms: 0,
    };

    fn path() -> Vec<MarketEvent> {
        [1.5, 2.2, 3.0, 2.0, 0.5]
            .iter()
            .enumerate()
            .map(|(i, p)| MarketEvent::Price {
                t_ms: (i as i64 + 1) * 1000,
                price: *p,
            })
            .collect()
    }

    #[test]
    fn ladder_and_trail_without_costs() {
        let pol = ExitPolicy {
            take_profit: vec![TpLevel {
                at_multiple: 2.0,
                sell_fraction: 0.5,
            }],
            trailing: Some(Trailing {
                activate_at_multiple: 1.2,
                drawdown_pct: 0.3,
            }),
            ..Default::default()
        };
        let ev = path();
        let input = PathInput {
            entry_price: 1.0,
            entry_t_ms: 0,
            entry_pool_sol: None,
            size_sol: 1.0,
            events: &ev,
        };
        let r = replay(&pol, &input, &FREE);
        // half sold at 2.2, half trailed out at 2.0 (3.0 peak * 0.7 = 2.1 > 2.0)
        assert!(r.fully_closed);
        assert!((r.pnl_sol - (0.5 * 2.2 + 0.5 * 2.0 - 1.0)).abs() < 1e-9);
        assert_eq!(r.held_ms, 4000);
    }

    #[test]
    fn costs_and_mark_to_market() {
        let ev = path();
        let input = PathInput {
            entry_price: 1.0,
            entry_t_ms: 0,
            entry_pool_sol: None,
            size_sol: 1.0,
            events: &ev,
        };
        let hold = ExitPolicy::default();
        let r = replay(&hold, &input, &FREE);
        assert!(!r.fully_closed);
        assert!((r.pnl_sol - (0.5 - 1.0)).abs() < 1e-9);

        let cost = CostModel {
            fee_bps: 100.0,
            slippage_bps: 0.0,
            fixed_sol_per_tx: 0.001,
            exit_latency_ms: 0,
        };
        let r = replay(&hold, &input, &cost);
        // the entry price already holds the buy's costs: only the sale is charged
        let expected = 0.5 * 0.99 - 1.0 - 0.002;
        assert!((r.pnl_sol - expected).abs() < 1e-9);
    }

    #[test]
    fn a_sale_lands_at_the_price_after_the_latency_not_the_trigger_price() {
        // price: 1.5 @1s, 2.2 @2s, 3.0 @3s, 2.0 @4s, 0.5 @5s (see `path`)
        let tp = ExitPolicy {
            take_profit: vec![TpLevel {
                at_multiple: 2.0,
                sell_fraction: 1.0,
            }],
            ..Default::default()
        };
        let ev = path();
        let input = PathInput {
            entry_price: 1.0,
            entry_t_ms: 0,
            entry_pool_sol: None,
            size_sol: 1.0,
            events: &ev,
        };
        // triggers at 2.2 (t = 2 s)
        let instant = replay(&tp, &input, &FREE);
        assert!((instant.pnl_sol - 1.2).abs() < 1e-9);
        // lands 1 s later, when the path shows 3.0
        let later = replay(
            &tp,
            &input,
            &CostModel {
                exit_latency_ms: 1000,
                ..FREE
            },
        );
        assert!((later.pnl_sol - 2.0).abs() < 1e-9);
        // lands 2.5 s later: still the latest price at that moment (2.0 @ 4 s)
        let slow = replay(
            &tp,
            &input,
            &CostModel {
                exit_latency_ms: 2500,
                ..FREE
            },
        );
        assert!((slow.pnl_sol - 1.0).abs() < 1e-9);
        // after the path ends the last known price is used
        let beyond = replay(
            &tp,
            &input,
            &CostModel {
                exit_latency_ms: 60_000,
                ..FREE
            },
        );
        assert!((beyond.pnl_sol - (0.5 - 1.0)).abs() < 1e-9);
    }

    #[test]
    fn evaluate_aggregates() {
        let ev = path();
        let input = PathInput {
            entry_price: 1.0,
            entry_t_ms: 0,
            entry_pool_sol: None,
            size_sol: 1.0,
            events: &ev,
        };
        let tp = ExitPolicy {
            take_profit: vec![TpLevel {
                at_multiple: 2.0,
                sell_fraction: 1.0,
            }],
            ..Default::default()
        };
        let s = evaluate(&tp, &[input.clone(), input], &FREE).unwrap();
        assert_eq!(s.n, 2);
        assert!((s.median_ret - 1.2).abs() < 1e-9);
        assert_eq!(s.win_rate, 1.0);
        assert!(evaluate(&tp, &[], &FREE).is_none());
    }
}
