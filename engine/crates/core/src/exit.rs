//! Exit engine: decides when to sell, independently of the copied wallet.
//!
//! A policy is a set of composable rules evaluated on every market event for
//! an open position. The leader selling is just one more input event; how we
//! react to it is a policy parameter, not a hard-wired mirror.
//!
//! All sell sizes are expressed as a fraction of the *initial* position so that
//! take-profit ladders are easy to reason about ("sell 50% of the original
//! bag at 2x"), and are always clamped to what is still held.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TpLevel {
    /// Price multiple of entry that triggers this level, e.g. 2.0 for a 2x.
    pub at_multiple: f64,
    /// Fraction of the initial position to sell, e.g. 0.5.
    pub sell_fraction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trailing {
    /// Trailing stop only arms once the peak reaches this multiple of entry.
    pub activate_at_multiple: f64,
    /// Exit everything when price falls this fraction below the peak.
    pub drawdown_pct: f64,
}

/// What to do when the copied wallet sells.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum LeaderSellReaction {
    /// Treat it as information only (it is still logged as a feature).
    #[default]
    Ignore,
    /// Sell the same fraction of our initial position that the leader sold of theirs.
    Mirror,
    /// Exit the whole remaining position.
    SellAll,
    /// Arm a (tighter) trailing stop immediately, regardless of activation level.
    Tighten { drawdown_pct: f64 },
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitPolicy {
    /// Exit everything at this loss from entry, e.g. 0.35 = -35%.
    pub stop_loss_pct: Option<f64>,
    #[serde(default)]
    pub take_profit: Vec<TpLevel>,
    pub trailing: Option<Trailing>,
    /// Exit everything after holding this long.
    pub max_hold_secs: Option<u64>,
    /// Exit everything if no new price peak has been made for this long.
    pub stale_secs: Option<u64>,
    #[serde(default)]
    pub on_leader_sell: LeaderSellReaction,
    /// Exit everything when the token's creator sells.
    #[serde(default)]
    pub exit_on_dev_sell: bool,
    /// Exit everything when pool SOL liquidity falls this fraction below its value at entry.
    pub liquidity_drop_pct: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarketEvent {
    /// A trade on the token's pool (anyone's), giving a new price.
    Price { t_ms: i64, price: f64 },
    /// The copied wallet sold `fraction` of its holding in this token.
    LeaderSell { t_ms: i64, fraction: f64 },
    /// The token creator sold.
    DevSell { t_ms: i64 },
    /// Pool SOL reserves update.
    Liquidity { t_ms: i64, pool_sol: f64 },
    /// Timer tick so time-based rules fire even when the token stops trading.
    Clock { t_ms: i64 },
}

impl MarketEvent {
    pub fn t_ms(&self) -> i64 {
        match *self {
            MarketEvent::Price { t_ms, .. }
            | MarketEvent::LeaderSell { t_ms, .. }
            | MarketEvent::DevSell { t_ms }
            | MarketEvent::Liquidity { t_ms, .. }
            | MarketEvent::Clock { t_ms } => t_ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "level", rename_all = "snake_case")]
pub enum ExitReason {
    StopLoss,
    TakeProfit(u8),
    Trailing,
    MaxHold,
    Stale,
    LeaderSell,
    DevSell,
    LiquidityDrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExitAction {
    pub t_ms: i64,
    /// Fraction of the initial position to sell (already clamped to what is held).
    pub sell_fraction: f64,
    /// Last observed price when the action was decided.
    pub ref_price: f64,
    pub reason: ExitReason,
}

/// Mutable per-position state the policy evaluates against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionState {
    pub entry_price: f64,
    pub entry_t_ms: i64,
    pub entry_pool_sol: Option<f64>,
    /// Fraction of the initial position still held, 0..=1.
    pub remaining: f64,
    pub last_price: f64,
    pub peak_price: f64,
    pub peak_t_ms: i64,
    pub tp_hit: Vec<bool>,
    /// Trailing drawdown armed by a leader-sell `Tighten` reaction.
    pub trail_override: Option<f64>,
}

const EPS: f64 = 1e-9;

impl PositionState {
    pub fn open(
        policy: &ExitPolicy,
        entry_price: f64,
        entry_t_ms: i64,
        entry_pool_sol: Option<f64>,
    ) -> Self {
        Self {
            entry_price,
            entry_t_ms,
            entry_pool_sol,
            remaining: 1.0,
            last_price: entry_price,
            peak_price: entry_price,
            peak_t_ms: entry_t_ms,
            tp_hit: vec![false; policy.take_profit.len()],
            trail_override: None,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.remaining <= EPS
    }

    pub fn multiple(&self) -> f64 {
        self.last_price / self.entry_price
    }
}

impl ExitPolicy {
    /// Check the policy for internal consistency. Called when config is loaded.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(sl) = self.stop_loss_pct {
            if !(0.0 < sl && sl < 1.0) {
                return Err(format!("stop_loss_pct must be in (0,1), got {sl}"));
            }
        }
        let mut prev = 1.0;
        let mut total = 0.0;
        for (i, tp) in self.take_profit.iter().enumerate() {
            if tp.at_multiple <= prev {
                return Err(format!(
                    "take_profit[{i}].at_multiple must be > 1 and ascending"
                ));
            }
            if !(0.0 < tp.sell_fraction && tp.sell_fraction <= 1.0) {
                return Err(format!("take_profit[{i}].sell_fraction must be in (0,1]"));
            }
            prev = tp.at_multiple;
            total += tp.sell_fraction;
        }
        if total > 1.0 + EPS {
            return Err(format!("take_profit fractions sum to {total:.3} > 1"));
        }
        if let Some(tr) = &self.trailing {
            if !(0.0 < tr.drawdown_pct && tr.drawdown_pct < 1.0) || tr.activate_at_multiple <= 0.0 {
                return Err(
                    "trailing needs drawdown_pct in (0,1) and activate_at_multiple > 0".into(),
                );
            }
        }
        if let LeaderSellReaction::Tighten { drawdown_pct } = self.on_leader_sell {
            if !(0.0 < drawdown_pct && drawdown_pct < 1.0) {
                return Err("on_leader_sell.drawdown_pct must be in (0,1)".into());
            }
        }
        if let Some(d) = self.liquidity_drop_pct {
            if !(0.0 < d && d < 1.0) {
                return Err("liquidity_drop_pct must be in (0,1)".into());
            }
        }
        Ok(())
    }

    /// Feed one event; returns the sells to execute (possibly none).
    pub fn on_event(&self, st: &mut PositionState, ev: &MarketEvent) -> Vec<ExitAction> {
        let mut out = Vec::new();
        if st.is_closed() {
            return out;
        }
        let t = ev.t_ms();

        match *ev {
            MarketEvent::Price { price, .. } => {
                if price.is_finite() && price > 0.0 {
                    st.last_price = price;
                    if price > st.peak_price {
                        st.peak_price = price;
                        st.peak_t_ms = t;
                    }
                }
                self.price_rules(st, t, &mut out);
            }
            MarketEvent::LeaderSell { fraction, .. } => match self.on_leader_sell {
                LeaderSellReaction::Ignore => {}
                LeaderSellReaction::Mirror => sell(
                    st,
                    t,
                    fraction.clamp(0.0, 1.0),
                    ExitReason::LeaderSell,
                    &mut out,
                ),
                LeaderSellReaction::SellAll => sell(st, t, 1.0, ExitReason::LeaderSell, &mut out),
                LeaderSellReaction::Tighten { drawdown_pct } => {
                    st.trail_override = Some(match st.trail_override {
                        Some(cur) => cur.min(drawdown_pct),
                        None => drawdown_pct,
                    });
                    self.price_rules(st, t, &mut out);
                }
            },
            MarketEvent::DevSell { .. } => {
                if self.exit_on_dev_sell {
                    sell(st, t, 1.0, ExitReason::DevSell, &mut out);
                }
            }
            MarketEvent::Liquidity { pool_sol, .. } => {
                if let (Some(drop), Some(entry)) = (self.liquidity_drop_pct, st.entry_pool_sol) {
                    if entry > 0.0 && pool_sol <= entry * (1.0 - drop) {
                        sell(st, t, 1.0, ExitReason::LiquidityDrop, &mut out);
                    }
                }
            }
            MarketEvent::Clock { .. } => {}
        }

        self.time_rules(st, t, &mut out);
        out
    }

    fn price_rules(&self, st: &mut PositionState, t: i64, out: &mut Vec<ExitAction>) {
        let m = st.multiple();

        if let Some(sl) = self.stop_loss_pct {
            if m <= 1.0 - sl {
                sell(st, t, 1.0, ExitReason::StopLoss, out);
                return;
            }
        }

        for (i, tp) in self.take_profit.iter().enumerate() {
            if !st.tp_hit[i] && m >= tp.at_multiple {
                st.tp_hit[i] = true;
                sell(
                    st,
                    t,
                    tp.sell_fraction,
                    ExitReason::TakeProfit(i as u8),
                    out,
                );
            }
        }

        let armed = match (&self.trailing, st.trail_override) {
            (_, Some(dd)) => Some(dd),
            (Some(tr), None) if st.peak_price >= st.entry_price * tr.activate_at_multiple => {
                Some(tr.drawdown_pct)
            }
            _ => None,
        };
        if let Some(dd) = armed {
            if st.last_price <= st.peak_price * (1.0 - dd) {
                sell(st, t, 1.0, ExitReason::Trailing, out);
            }
        }
    }

    fn time_rules(&self, st: &mut PositionState, t: i64, out: &mut Vec<ExitAction>) {
        if let Some(max) = self.max_hold_secs {
            if t - st.entry_t_ms >= (max as i64) * 1000 {
                sell(st, t, 1.0, ExitReason::MaxHold, out);
            }
        }
        if let Some(stale) = self.stale_secs {
            if t - st.peak_t_ms >= (stale as i64) * 1000 {
                sell(st, t, 1.0, ExitReason::Stale, out);
            }
        }
    }
}

fn sell(
    st: &mut PositionState,
    t: i64,
    fraction: f64,
    reason: ExitReason,
    out: &mut Vec<ExitAction>,
) {
    let f = fraction.min(st.remaining);
    if f <= EPS {
        return;
    }
    st.remaining -= f;
    if st.remaining < EPS {
        st.remaining = 0.0;
    }
    out.push(ExitAction {
        t_ms: t,
        sell_fraction: f,
        ref_price: st.last_price,
        reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(t_ms: i64, price: f64) -> MarketEvent {
        MarketEvent::Price { t_ms, price }
    }

    fn ladder() -> ExitPolicy {
        ExitPolicy {
            stop_loss_pct: Some(0.3),
            take_profit: vec![
                TpLevel {
                    at_multiple: 2.0,
                    sell_fraction: 0.5,
                },
                TpLevel {
                    at_multiple: 4.0,
                    sell_fraction: 0.25,
                },
            ],
            trailing: Some(Trailing {
                activate_at_multiple: 1.5,
                drawdown_pct: 0.25,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn stop_loss_sells_everything() {
        let p = ladder();
        let mut st = PositionState::open(&p, 1.0, 0, None);
        assert!(p.on_event(&mut st, &px(1, 0.8)).is_empty());
        let a = p.on_event(&mut st, &px(2, 0.7));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].reason, ExitReason::StopLoss);
        assert!((a[0].sell_fraction - 1.0).abs() < 1e-12);
        assert!(st.is_closed());
        assert!(p.on_event(&mut st, &px(3, 0.1)).is_empty());
    }

    #[test]
    fn ladder_then_trailing() {
        let p = ladder();
        let mut st = PositionState::open(&p, 1.0, 0, None);
        let a = p.on_event(&mut st, &px(1, 2.1));
        assert_eq!(a[0].reason, ExitReason::TakeProfit(0));
        assert!((st.remaining - 0.5).abs() < 1e-12);
        // Gap straight through 4x on one print: second level fires too.
        let a = p.on_event(&mut st, &px(2, 5.0));
        assert_eq!(a[0].reason, ExitReason::TakeProfit(1));
        assert!((st.remaining - 0.25).abs() < 1e-12);
        // 25% off the 5.0 peak = 3.75 → trailing exits the rest.
        assert!(p.on_event(&mut st, &px(3, 3.8)).is_empty());
        let a = p.on_event(&mut st, &px(4, 3.7));
        assert_eq!(a[0].reason, ExitReason::Trailing);
        assert!((a[0].sell_fraction - 0.25).abs() < 1e-12);
        assert!(st.is_closed());
    }

    #[test]
    fn trailing_not_armed_below_activation() {
        let p = ladder();
        let mut st = PositionState::open(&p, 1.0, 0, None);
        p.on_event(&mut st, &px(1, 1.4)); // below 1.5x activation
        assert!(p.on_event(&mut st, &px(2, 1.0)).is_empty()); // -28% from peak, not armed
    }

    #[test]
    fn leader_sell_reactions() {
        let mirror = ExitPolicy {
            on_leader_sell: LeaderSellReaction::Mirror,
            ..Default::default()
        };
        let mut st = PositionState::open(&mirror, 1.0, 0, None);
        let a = mirror.on_event(
            &mut st,
            &MarketEvent::LeaderSell {
                t_ms: 1,
                fraction: 0.4,
            },
        );
        assert!((a[0].sell_fraction - 0.4).abs() < 1e-12);

        let ignore = ExitPolicy::default();
        let mut st = PositionState::open(&ignore, 1.0, 0, None);
        assert!(ignore
            .on_event(
                &mut st,
                &MarketEvent::LeaderSell {
                    t_ms: 1,
                    fraction: 1.0
                }
            )
            .is_empty());

        let tighten = ExitPolicy {
            on_leader_sell: LeaderSellReaction::Tighten { drawdown_pct: 0.1 },
            ..Default::default()
        };
        let mut st = PositionState::open(&tighten, 1.0, 0, None);
        tighten.on_event(&mut st, &px(1, 1.2));
        assert!(tighten
            .on_event(
                &mut st,
                &MarketEvent::LeaderSell {
                    t_ms: 2,
                    fraction: 1.0
                }
            )
            .is_empty());
        let a = tighten.on_event(&mut st, &px(3, 1.07)); // >10% below 1.2 peak
        assert_eq!(a[0].reason, ExitReason::Trailing);
    }

    #[test]
    fn time_and_liquidity_rules() {
        let p = ExitPolicy {
            max_hold_secs: Some(60),
            stale_secs: Some(20),
            ..Default::default()
        };
        let mut st = PositionState::open(&p, 1.0, 0, None);
        p.on_event(&mut st, &px(10_000, 1.1)); // new peak at t=10s
        assert!(p
            .on_event(&mut st, &MarketEvent::Clock { t_ms: 29_000 })
            .is_empty());
        let a = p.on_event(&mut st, &MarketEvent::Clock { t_ms: 30_000 });
        assert_eq!(a[0].reason, ExitReason::Stale);

        let p = ExitPolicy {
            liquidity_drop_pct: Some(0.5),
            exit_on_dev_sell: true,
            ..Default::default()
        };
        let mut st = PositionState::open(&p, 1.0, 0, Some(80.0));
        assert!(p
            .on_event(
                &mut st,
                &MarketEvent::Liquidity {
                    t_ms: 1,
                    pool_sol: 41.0
                }
            )
            .is_empty());
        let a = p.on_event(
            &mut st,
            &MarketEvent::Liquidity {
                t_ms: 2,
                pool_sol: 40.0,
            },
        );
        assert_eq!(a[0].reason, ExitReason::LiquidityDrop);

        let mut st = PositionState::open(&p, 1.0, 0, None);
        let a = p.on_event(&mut st, &MarketEvent::DevSell { t_ms: 5 });
        assert_eq!(a[0].reason, ExitReason::DevSell);
    }

    #[test]
    fn validation() {
        assert!(ladder().validate().is_ok());
        let bad = ExitPolicy {
            take_profit: vec![
                TpLevel {
                    at_multiple: 3.0,
                    sell_fraction: 0.5,
                },
                TpLevel {
                    at_multiple: 2.0,
                    sell_fraction: 0.5,
                },
            ],
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        let over = ExitPolicy {
            take_profit: vec![
                TpLevel {
                    at_multiple: 2.0,
                    sell_fraction: 0.7,
                },
                TpLevel {
                    at_multiple: 3.0,
                    sell_fraction: 0.7,
                },
            ],
            ..Default::default()
        };
        assert!(over.validate().is_err());
    }
}
