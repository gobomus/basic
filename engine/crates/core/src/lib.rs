//! Pure domain logic for the copy-trading engine.
//!
//! Everything in this crate is deterministic and I/O-free so the exact same code
//! runs in three places:
//!
//! * the live engine (decides what to buy and when to sell),
//! * shadow mode (evaluates alternative exit policies on every live position), and
//! * offline replay (re-runs any exit policy over recorded post-entry price paths).
//!
//! Keeping one implementation is what makes the trade log trustworthy for tuning:
//! a policy that wins in replay behaves identically when promoted to live.

pub mod config;
pub mod exit;
pub mod replay;
pub mod sizing;
pub mod stats;
pub mod types;
pub mod wallet_score;

pub use config::EngineConfig;
pub use exit::{ExitAction, ExitPolicy, ExitReason, MarketEvent, PositionState};
pub use sizing::{SizeDecision, SkipReason};
pub use types::{Lamports, Side, SwapEvent, Venue, LAMPORTS_PER_SOL};
