//! Entry decision: should we copy this leader buy, and with how much SOL?
//!
//! Sizing is "a percentage of the leader's volume" clamped by a stack of hard
//! caps. Every skip and every binding cap is returned explicitly so it can be
//! written to the trade log — skipped trades are data too.

use serde::{Deserialize, Serialize};

use crate::config::{FilterConfig, RiskConfig, SizingConfig, SizingMode};
use crate::types::{sol_to_lamports, Lamports, Venue};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    LeaderBuyTooSmall,
    LeaderBuyTooLarge,
    VenueNotAllowed,
    LiquidityTooLow,
    LiquidityTooHigh,
    MarketCapTooLow,
    MarketCapTooHigh,
    Blacklisted,
    AlreadyTraded,
    LeaderBalanceUnknown,
    TokenTooYoung,
    TokenTooOld,
    DetectionTooLate,
    PriceAlreadyMoved,
    AddsDisabled,
    MaxAddsReached,
    MaxOpenPositions,
    PositionCapReached,
    ExposureCapReached,
    InsufficientBalance,
    BelowMinimum,
    KillSwitch,
}

/// Which limit bound the final size (for logging / tuning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    MaxBuy,
    Position,
    Exposure,
    PoolImpact,
    Balance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum SizeDecision {
    Buy {
        lamports: Lamports,
        capped_by: Option<Cap>,
    },
    Skip {
        reason: SkipReason,
    },
}

/// Market state at the moment we decide, as seen by the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntryContext {
    pub venue: Venue,
    pub leader_buy: Lamports,
    /// Leader's average fill price.
    pub leader_price: f64,
    /// Best current price estimate (latest pool state).
    pub current_price: f64,
    /// SOL side of the pool reserves, if known.
    pub pool_sol: Option<Lamports>,
    pub token_age_secs: Option<u64>,
    /// Slots between the leader's transaction and our detection of it.
    pub detection_slot_lag: u64,
    /// Leader's SOL balance just before the trade (for `balance_fraction`).
    pub leader_sol_before: Option<Lamports>,
    /// Current market cap in SOL, when the supply is known.
    pub market_cap_sol: Option<f64>,
}

/// Our own book at the moment we decide.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BookContext {
    /// Cost basis already held in this mint (0 for a fresh entry).
    pub position_cost: Lamports,
    pub adds_so_far: u32,
    pub open_positions: u32,
    pub open_exposure: Lamports,
    pub free_balance: Lamports,
    pub kill_switch: bool,
}

pub fn pre_trade_filters(f: &FilterConfig, e: &EntryContext) -> Result<(), SkipReason> {
    if e.leader_buy < sol_to_lamports(f.min_leader_buy_sol) {
        return Err(SkipReason::LeaderBuyTooSmall);
    }
    if let Some(max) = f.max_leader_buy_sol {
        if e.leader_buy > sol_to_lamports(max) {
            return Err(SkipReason::LeaderBuyTooLarge);
        }
    }
    if !f.venues.is_empty() && !f.venues.contains(&e.venue) {
        return Err(SkipReason::VenueNotAllowed);
    }
    if let (Some(min), Some(pool)) = (f.min_pool_sol, e.pool_sol) {
        if pool < sol_to_lamports(min) {
            return Err(SkipReason::LiquidityTooLow);
        }
    }
    if let (Some(max), Some(pool)) = (f.max_pool_sol, e.pool_sol) {
        if pool > sol_to_lamports(max) {
            return Err(SkipReason::LiquidityTooHigh);
        }
    }
    if let Some(mc) = e.market_cap_sol {
        if f.min_market_cap_sol.is_some_and(|m| mc < m) {
            return Err(SkipReason::MarketCapTooLow);
        }
        if f.max_market_cap_sol.is_some_and(|m| mc > m) {
            return Err(SkipReason::MarketCapTooHigh);
        }
    }
    if let Some(age) = e.token_age_secs {
        if f.min_token_age_secs.is_some_and(|m| age < m) {
            return Err(SkipReason::TokenTooYoung);
        }
        if f.max_token_age_secs.is_some_and(|m| age > m) {
            return Err(SkipReason::TokenTooOld);
        }
    }
    if e.detection_slot_lag > f.max_detection_slot_lag {
        return Err(SkipReason::DetectionTooLate);
    }
    if e.leader_price > 0.0 && e.current_price / e.leader_price - 1.0 > f.max_price_drift_pct {
        return Err(SkipReason::PriceAlreadyMoved);
    }
    Ok(())
}

pub fn size_buy(
    s: &SizingConfig,
    r: &RiskConfig,
    e: &EntryContext,
    b: &BookContext,
) -> SizeDecision {
    let skip = |reason| SizeDecision::Skip { reason };

    if b.kill_switch {
        return skip(SkipReason::KillSwitch);
    }
    let is_add = b.position_cost > 0;
    if is_add {
        if !s.follow_adds {
            return skip(SkipReason::AddsDisabled);
        }
        if b.adds_so_far >= s.max_adds {
            return skip(SkipReason::MaxAddsReached);
        }
    } else if b.open_positions >= r.max_open_positions {
        return skip(SkipReason::MaxOpenPositions);
    }

    let base = s.copy_amount_sol.map(sol_to_lamports).unwrap_or(0) as f64;
    let raw = match s.mode {
        SizingMode::LeaderPct => (e.leader_buy as f64 * s.copy_pct).round() as Lamports,
        SizingMode::Fixed => base as Lamports,
        SizingMode::BalanceFraction => match e.leader_sol_before.filter(|b| *b > 0) {
            Some(bal) => (base * (e.leader_buy as f64 / bal as f64).min(1.0)).round() as Lamports,
            None => return skip(SkipReason::LeaderBalanceUnknown),
        },
    };
    let mut size = raw;
    let mut capped_by = None;
    let mut cap = |limit: Lamports, which: Cap, size: &mut Lamports| {
        if *size > limit {
            *size = limit;
            capped_by = Some(which);
        }
    };

    cap(sol_to_lamports(s.max_buy_sol), Cap::MaxBuy, &mut size);
    let pos_room = sol_to_lamports(r.max_position_sol).saturating_sub(b.position_cost);
    if pos_room == 0 {
        return skip(SkipReason::PositionCapReached);
    }
    cap(pos_room, Cap::Position, &mut size);
    let exp_room = sol_to_lamports(r.max_total_exposure_sol).saturating_sub(b.open_exposure);
    if exp_room == 0 {
        return skip(SkipReason::ExposureCapReached);
    }
    cap(exp_room, Cap::Exposure, &mut size);
    if let Some(pool) = e.pool_sol {
        let impact = (pool as f64 * s.max_pool_impact_pct).floor() as Lamports;
        cap(impact, Cap::PoolImpact, &mut size);
    }
    let spendable = b
        .free_balance
        .saturating_sub(sol_to_lamports(r.min_sol_reserve));
    if spendable == 0 {
        return skip(SkipReason::InsufficientBalance);
    }
    cap(spendable, Cap::Balance, &mut size);

    if size < sol_to_lamports(s.min_buy_sol) {
        return skip(SkipReason::BelowMinimum);
    }
    SizeDecision::Buy {
        lamports: size,
        capped_by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LAMPORTS_PER_SOL as SOL;

    fn cfgs() -> (SizingConfig, RiskConfig, FilterConfig) {
        (
            SizingConfig {
                mode: SizingMode::LeaderPct,
                copy_amount_sol: None,
                copy_pct: 0.10,
                min_buy_sol: 0.05,
                max_buy_sol: 1.0,
                max_pool_impact_pct: 0.01,
                follow_adds: true,
                max_adds: 2,
            },
            RiskConfig {
                max_position_sol: 1.5,
                max_total_exposure_sol: 5.0,
                max_open_positions: 10,
                min_sol_reserve: 0.1,
                daily_loss_limit_sol: 2.0,
                max_buy_slippage_bps: 1500,
                max_sell_slippage_bps: 2500,
            },
            FilterConfig {
                min_leader_buy_sol: 0.5,
                max_leader_buy_sol: Some(100.0),
                venues: vec![],
                min_pool_sol: Some(20.0),
                max_pool_sol: None,
                min_market_cap_sol: None,
                max_market_cap_sol: None,
                one_entry_per_token: false,
                blacklist_mints: vec![],
                blacklist_devs: vec![],
                min_token_age_secs: None,
                max_token_age_secs: Some(86_400),
                max_detection_slot_lag: 2,
                max_price_drift_pct: 0.15,
            },
        )
    }

    fn entry(leader_sol: u64) -> EntryContext {
        EntryContext {
            venue: Venue::PumpSwap,
            leader_buy: leader_sol * SOL,
            leader_price: 1.0,
            current_price: 1.05,
            pool_sol: Some(500 * SOL),
            token_age_secs: Some(600),
            detection_slot_lag: 1,
            leader_sol_before: Some(100 * SOL),
            market_cap_sol: Some(400.0),
        }
    }

    fn book() -> BookContext {
        BookContext {
            free_balance: 10 * SOL,
            ..Default::default()
        }
    }

    #[test]
    fn proportional_size() {
        let (s, r, _) = cfgs();
        let d = size_buy(&s, &r, &entry(3), &book());
        assert_eq!(
            d,
            SizeDecision::Buy {
                lamports: 300_000_000,
                capped_by: None
            }
        );
    }

    #[test]
    fn caps_apply_in_order() {
        let (s, r, _) = cfgs();
        // 10% of 50 SOL = 5 SOL → max_buy 1.0
        let d = size_buy(&s, &r, &entry(50), &book());
        assert_eq!(
            d,
            SizeDecision::Buy {
                lamports: SOL,
                capped_by: Some(Cap::MaxBuy)
            }
        );
        // Thin pool: 1% of 30 SOL = 0.3 SOL
        let mut e = entry(50);
        e.pool_sol = Some(30 * SOL);
        let d = size_buy(&s, &r, &e, &book());
        assert_eq!(
            d,
            SizeDecision::Buy {
                lamports: 300_000_000,
                capped_by: Some(Cap::PoolImpact)
            }
        );
    }

    #[test]
    fn skips() {
        let (s, r, f) = cfgs();
        let mut e = entry(1);
        e.current_price = 1.2;
        assert_eq!(
            pre_trade_filters(&f, &e),
            Err(SkipReason::PriceAlreadyMoved)
        );
        e.current_price = 1.0;
        e.detection_slot_lag = 3;
        assert_eq!(pre_trade_filters(&f, &e), Err(SkipReason::DetectionTooLate));
        assert!(pre_trade_filters(&f, &entry(1)).is_ok());

        // 10% of 0.4 SOL = 0.04 < 0.05 minimum
        let mut e = entry(1);
        e.leader_buy = 400_000_000;
        assert_eq!(
            size_buy(&s, &r, &e, &book()),
            SizeDecision::Skip {
                reason: SkipReason::BelowMinimum
            }
        );

        let b = BookContext {
            position_cost: SOL,
            adds_so_far: 2,
            ..book()
        };
        assert_eq!(
            size_buy(&s, &r, &entry(3), &b),
            SizeDecision::Skip {
                reason: SkipReason::MaxAddsReached
            }
        );

        let b = BookContext {
            free_balance: SOL / 10,
            ..book()
        };
        assert_eq!(
            size_buy(&s, &r, &entry(3), &b),
            SizeDecision::Skip {
                reason: SkipReason::InsufficientBalance
            }
        );
    }

    #[test]
    fn balance_fraction_mode() {
        let (mut s, r, _) = cfgs();
        s.mode = SizingMode::BalanceFraction;
        s.copy_amount_sol = Some(2.0);
        // leader spends 10 of 100 SOL → 10% of our 2 SOL base
        let d = size_buy(&s, &r, &entry(10), &book());
        assert_eq!(
            d,
            SizeDecision::Buy {
                lamports: 200_000_000,
                capped_by: None
            }
        );
        let mut e = entry(10);
        e.leader_sol_before = None;
        assert_eq!(
            size_buy(&s, &r, &e, &book()),
            SizeDecision::Skip {
                reason: SkipReason::LeaderBalanceUnknown
            }
        );
    }

    #[test]
    fn fixed_mode_ignores_leader_size() {
        let (mut s, r, _) = cfgs();
        s.mode = SizingMode::Fixed;
        s.copy_amount_sol = Some(0.25);
        for l in [1, 7] {
            assert_eq!(
                size_buy(&s, &r, &entry(l), &book()),
                SizeDecision::Buy {
                    lamports: 250_000_000,
                    capped_by: None
                }
            );
        }
    }

    #[test]
    fn market_cap_and_max_liquidity_filters() {
        let (_, _, mut f) = cfgs();
        f.min_market_cap_sol = Some(500.0);
        assert_eq!(
            pre_trade_filters(&f, &entry(3)),
            Err(SkipReason::MarketCapTooLow)
        );
        f.min_market_cap_sol = None;
        f.max_market_cap_sol = Some(300.0);
        assert_eq!(
            pre_trade_filters(&f, &entry(3)),
            Err(SkipReason::MarketCapTooHigh)
        );
        f.max_market_cap_sol = None;
        f.max_pool_sol = Some(100.0);
        assert_eq!(
            pre_trade_filters(&f, &entry(3)),
            Err(SkipReason::LiquidityTooHigh)
        );
        let mut e = entry(3);
        e.market_cap_sol = None; // unknown supply: filter not applied
        f.max_pool_sol = None;
        f.min_market_cap_sol = Some(500.0);
        assert_eq!(pre_trade_filters(&f, &e), Ok(()));
    }
}
