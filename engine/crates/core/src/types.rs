use serde::{Deserialize, Serialize};

pub type Lamports = u64;
pub const LAMPORTS_PER_SOL: Lamports = 1_000_000_000;

pub fn sol_to_lamports(sol: f64) -> Lamports {
    if sol <= 0.0 {
        0
    } else {
        (sol * LAMPORTS_PER_SOL as f64).round() as Lamports
    }
}

pub fn lamports_to_sol(lamports: Lamports) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL as f64
}

/// Where a swap executed. Each variant needs its own instruction decoder and
/// its own buy-instruction builder on the execution side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    PumpFunCurve,
    PumpSwap,
    RaydiumLaunchLab,
    RaydiumAmmV4,
    RaydiumCpmm,
    RaydiumClmm,
    MeteoraDbc,
    MeteoraDammV2,
    MeteoraDlmm,
    OrcaWhirlpool,
    /// Leader went through an aggregator; we only know the outer route.
    JupiterRoute,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// Which feed delivered an event first. Recorded on every event so detection
/// latency can be attributed per source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedSource {
    /// Pre-execution, reconstructed from shreds (no logs / balances yet).
    Shred,
    /// Post-execution Geyser gRPC (Yellowstone / LaserStream).
    Geyser,
    /// RPC / websocket fallback.
    Rpc,
}

/// A decoded swap by any wallet on any tracked venue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwapEvent {
    pub signature: String,
    pub slot: u64,
    /// Position of the transaction inside its block. Needed to measure how far
    /// behind the leader a copier lands (the "imitation penalty").
    pub tx_index: Option<u32>,
    pub block_time_ms: Option<i64>,
    /// Local monotonic receive time.
    pub observed_at_ns: u64,
    pub source: FeedSource,
    pub wallet: String,
    pub mint: String,
    pub venue: Venue,
    pub side: Side,
    pub sol_amount: Lamports,
    /// Raw token amount (base units).
    pub token_amount: u64,
    pub token_decimals: u8,
}

impl SwapEvent {
    /// Execution price in SOL per whole token (fees excluded).
    pub fn price_sol_per_token(&self) -> Option<f64> {
        if self.token_amount == 0 {
            return None;
        }
        let tokens = self.token_amount as f64 / 10f64.powi(self.token_decimals as i32);
        Some(lamports_to_sol(self.sol_amount) / tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_uses_decimals() {
        let ev = SwapEvent {
            signature: "sig".into(),
            slot: 1,
            tx_index: Some(3),
            block_time_ms: None,
            observed_at_ns: 0,
            source: FeedSource::Geyser,
            wallet: "w".into(),
            mint: "m".into(),
            venue: Venue::PumpFunCurve,
            side: Side::Buy,
            sol_amount: LAMPORTS_PER_SOL,        // 1 SOL
            token_amount: 2_000_000 * 1_000_000, // 2M tokens @ 6 decimals
            token_decimals: 6,
        };
        let p = ev.price_sol_per_token().unwrap();
        assert!((p - 5e-7).abs() < 1e-15);
    }

    #[test]
    fn sol_lamport_roundtrip() {
        assert_eq!(sol_to_lamports(0.25), 250_000_000);
        assert_eq!(sol_to_lamports(-1.0), 0);
        assert!((lamports_to_sol(1_500_000_000) - 1.5).abs() < 1e-12);
    }
}
