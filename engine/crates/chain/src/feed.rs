//! Feed-neutral types: what the engine receives, whichever source produced it
//! (Yellowstone gRPC, or the free WebSocket + RPC feed in `wsfeed`).

use solana_sdk::pubkey::Pubkey;

use crate::detect::Template;
use crate::model::ChainTx;

/// What the feed should watch. Updated live by the engine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    pub leaders: Vec<String>,
    pub mints: Vec<String>,
}

/// Fresh on-chain state of one coin's pool, read straight from its accounts.
#[derive(Debug, Clone)]
pub struct StateUpdate {
    pub mint: Pubkey,
    /// Pool state to quote against (and to build orders from).
    pub template: Template,
    /// SOL per whole token.
    pub price_sol: f64,
    /// SOL-side pool reserves, lamports.
    pub pool_sol: u64,
    pub decimals: u8,
    pub token_program: Pubkey,
    pub slot: u64,
    /// The bonding curve completed and the coin now trades on PumpSwap:
    /// `template` is the new pool.
    pub migrated: bool,
}

#[derive(Debug)]
pub enum FeedEvent {
    Tx(Box<ChainTx>),
    State(Box<StateUpdate>),
    Slot {
        slot: u64,
        status: i32,
    },
    Status {
        source: &'static str,
        connected: bool,
        detail: String,
    },
}
