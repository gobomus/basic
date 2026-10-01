//! Swap detection.
//!
//! * Pump curve and PumpSwap swaps are decoded **exactly** from the programs'
//!   own events, and carry an execution template so we can copy them without
//!   a single RPC round trip.
//! * Every other venue (Raydium, Meteora, Orca, aggregators…) is detected
//!   **universally** from the wallet's SOL / token balance changes, so a leader
//!   is never missed just because we lack a decoder for their DEX.

use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

use engine_core::types::{Side, Venue};

use crate::borsh::EVENT_IX_TAG;
use crate::consts::*;
use crate::model::ChainTx;
use crate::pda;
use crate::pump::{self, CurveCoin, CurveState, PumpEvent, TradeEvent};
use crate::pump_amm::{self, AmmCoin, SwapEventData};

// Venue labelling only (never used to move funds).
const RAYDIUM_AMM_V4: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
const RAYDIUM_CPMM: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
const RAYDIUM_CLMM: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
const RAYDIUM_LAUNCHLAB: Pubkey = pubkey!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
const METEORA_DBC: Pubkey = pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
const METEORA_DAMM_V2: Pubkey = pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
const METEORA_DLMM: Pubkey = pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const ORCA_WHIRLPOOL: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
const JUPITER_V6: Pubkey = pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

/// Programs whose transactions count as trades on the token (for the market firehose).
pub const DEX_PROGRAMS: [Pubkey; 11] = [
    PUMP_PROGRAM,
    PUMP_AMM_PROGRAM,
    RAYDIUM_AMM_V4,
    RAYDIUM_CPMM,
    RAYDIUM_CLMM,
    RAYDIUM_LAUNCHLAB,
    METEORA_DBC,
    METEORA_DAMM_V2,
    METEORA_DLMM,
    ORCA_WHIRLPOOL,
    JUPITER_V6,
];

pub fn venue_of(tx: &ChainTx) -> Venue {
    let order = [
        (PUMP_PROGRAM, Venue::PumpFunCurve),
        (PUMP_AMM_PROGRAM, Venue::PumpSwap),
        (RAYDIUM_LAUNCHLAB, Venue::RaydiumLaunchLab),
        (METEORA_DBC, Venue::MeteoraDbc),
        (METEORA_DAMM_V2, Venue::MeteoraDammV2),
        (METEORA_DLMM, Venue::MeteoraDlmm),
        (RAYDIUM_CPMM, Venue::RaydiumCpmm),
        (RAYDIUM_CLMM, Venue::RaydiumClmm),
        (RAYDIUM_AMM_V4, Venue::RaydiumAmmV4),
        (ORCA_WHIRLPOOL, Venue::OrcaWhirlpool),
        (JUPITER_V6, Venue::JupiterRoute),
    ];
    for (p, v) in order {
        if tx.invokes(&p) {
            return v;
        }
    }
    Venue::Other
}

/// How we can execute a copy of this swap.
#[derive(Debug, Clone, PartialEq)]
pub enum Template {
    Curve {
        coin: CurveCoin,
        state: CurveState,
        fee_bps: u64,
    },
    Amm {
        coin: AmmCoin,
        base_reserve: u64,
        quote_reserve: u128,
        fee_bps: u64,
    },
    /// No direct builder: route through an aggregator.
    Generic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DetectedSwap {
    pub wallet: Pubkey,
    pub mint: Pubkey,
    pub side: Side,
    pub venue: Venue,
    /// Lamports paid (buy) / received (sell). Exact when `exact`.
    pub sol_amount: u64,
    pub token_amount: u64,
    pub token_decimals: u8,
    pub token_program: Pubkey,
    /// Pool spot price after the swap (SOL per whole token); fill price if unknown.
    pub price_sol: f64,
    pub pool_sol: Option<u64>,
    /// For sells: fraction of the wallet's pre-trade balance sold.
    pub fraction_sold: Option<f64>,
    pub exact: bool,
    pub template: Template,
    pub creator: Option<Pubkey>,
    pub migrated: bool,
}

fn token_program_of(tx: &ChainTx, mint: &Pubkey) -> (Pubkey, u8) {
    tx.post_tokens
        .iter()
        .chain(tx.pre_tokens.iter())
        .find(|b| b.mint == *mint)
        .map(|b| {
            (
                if b.program == Pubkey::default() {
                    TOKEN_PROGRAM
                } else {
                    b.program
                },
                b.decimals,
            )
        })
        .unwrap_or((TOKEN_PROGRAM, PUMP_TOKEN_DECIMALS))
}

fn token_balance(bals: &[crate::model::TokenBal], owner: &Pubkey, mint: &Pubkey) -> u64 {
    bals.iter()
        .filter(|b| b.owner == *owner && b.mint == *mint)
        .map(|b| b.amount)
        .sum()
}

fn fraction_sold(tx: &ChainTx, owner: &Pubkey, mint: &Pubkey, sold: u64) -> Option<f64> {
    let pre = token_balance(&tx.pre_tokens, owner, mint);
    (pre > 0).then(|| (sold as f64 / pre as f64).min(1.0))
}

pub fn pump_events(tx: &ChainTx) -> Vec<PumpEvent> {
    let mut out: Vec<PumpEvent> = tx
        .all_ixs()
        .filter(|ix| ix.program == PUMP_PROGRAM && ix.data.starts_with(&EVENT_IX_TAG))
        .filter_map(|ix| pump::decode_event_ix(&ix.data))
        .collect();
    if out.is_empty() {
        out = program_data_logs(tx)
            .filter_map(|b| pump::decode_event(&b))
            .collect();
    }
    out
}

pub fn amm_events(tx: &ChainTx) -> Vec<SwapEventData> {
    let mut out: Vec<SwapEventData> = tx
        .all_ixs()
        .filter(|ix| ix.program == PUMP_AMM_PROGRAM && ix.data.starts_with(&EVENT_IX_TAG))
        .filter_map(|ix| pump_amm::decode_event_ix(&ix.data))
        .collect();
    if out.is_empty() {
        out = program_data_logs(tx)
            .filter_map(|b| pump_amm::decode_event(&b))
            .collect();
    }
    out
}

fn program_data_logs(tx: &ChainTx) -> impl Iterator<Item = Vec<u8>> + '_ {
    use base64::Engine;
    tx.logs
        .iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .filter_map(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
}

fn curve_swap(tx: &ChainTx, e: &TradeEvent) -> DetectedSwap {
    let (tp, dec) = token_program_of(tx, &e.mint);
    let coin = CurveCoin::sol_paired(e.mint, e.creator, tp, e.mayhem_mode);
    DetectedSwap {
        wallet: e.user,
        mint: e.mint,
        side: if e.is_buy { Side::Buy } else { Side::Sell },
        venue: Venue::PumpFunCurve,
        sol_amount: e.sol_amount,
        token_amount: e.token_amount,
        token_decimals: dec,
        token_program: tp,
        price_sol: e.price_sol(),
        pool_sol: Some(e.real_sol_reserves),
        fraction_sold: if e.is_buy {
            None
        } else {
            fraction_sold(tx, &e.user, &e.mint, e.token_amount)
        },
        exact: true,
        template: Template::Curve {
            coin,
            state: e.curve_state(),
            fee_bps: e.total_fee_bps(),
        },
        creator: Some(e.creator),
        migrated: false,
    }
}

/// Rebuild the AMM execution template from the leader's own instruction.
fn amm_template(tx: &ChainTx, e: &SwapEventData) -> Option<(AmmCoin, Pubkey)> {
    let buy = pump_amm::ix_disc("buy");
    let sell = pump_amm::ix_disc("sell");
    let bxq = pump_amm::ix_disc("buy_exact_quote_in");
    let ix = tx.all_ixs().find(|ix| {
        ix.program == PUMP_AMM_PROGRAM
            && ix.accounts.first() == Some(&e.pool)
            && (ix.data.starts_with(&buy)
                || ix.data.starts_with(&sell)
                || ix.data.starts_with(&bxq))
    })?;
    let a = &ix.accounts;
    let fixed = if ix.data.starts_with(&sell) { 21 } else { 23 };
    if a.len() < fixed + 2 {
        return None;
    }
    let base_mint = a[3];
    let quote_mint = a[4];
    let quote_tp = a[12];
    let remaining = a.len() - fixed;
    let base_remaining = 2 + usize::from(e.coin_creator != Pubkey::default());
    let is_cashback = remaining > base_remaining;
    let coin = AmmCoin {
        pool: e.pool,
        base_mint,
        quote_mint,
        pool_base_token_account: a[7],
        pool_quote_token_account: a[8],
        base_token_program: a[11],
        quote_token_program: quote_tp,
        coin_creator: e.coin_creator,
        is_mayhem_mode: false,
        is_cashback_coin: is_cashback,
        protocol_fee_recipient: a[9],
        buyback_fee_recipient: a[a.len() - 2],
    };
    // Sanity: the buyback ATA must be the last account.
    if a[a.len() - 1] != pda::ata(&coin.buyback_fee_recipient, &quote_mint, &quote_tp) {
        return None;
    }
    Some((coin, base_mint))
}

fn amm_swap(tx: &ChainTx, e: &SwapEventData) -> Option<DetectedSwap> {
    let (coin, mint) = amm_template(tx, e)?;
    if coin.quote_mint != WSOL_MINT {
        return None; // only SOL-quoted pools for now
    }
    let (tp, dec) = token_program_of(tx, &mint);
    Some(DetectedSwap {
        wallet: e.user,
        mint,
        side: if e.is_buy { Side::Buy } else { Side::Sell },
        venue: Venue::PumpSwap,
        sol_amount: e.user_quote_amount,
        token_amount: e.base_amount,
        token_decimals: dec,
        token_program: tp,
        price_sol: e.price(dec, 9),
        pool_sol: Some(e.effective_quote_reserves() as u64),
        fraction_sold: if e.is_buy {
            None
        } else {
            fraction_sold(tx, &e.user, &mint, e.base_amount)
        },
        exact: true,
        template: Template::Amm {
            base_reserve: e.pool_base_token_reserves,
            quote_reserve: e.effective_quote_reserves(),
            fee_bps: e.total_fee_bps(),
            coin,
        },
        creator: Some(e.coin_creator),
        migrated: true,
    })
}

/// Balance-delta detection for any venue. Also used for our own fills: it
/// captures the true SOL cost including fees, tips and rent.
pub fn balance_swaps(tx: &ChainTx, wallet: &Pubkey) -> Vec<DetectedSwap> {
    let Some(idx) = tx.key_index(wallet) else {
        return vec![];
    };
    let (Some(pre), Some(post)) = (tx.pre_balances.get(idx), tx.post_balances.get(idx)) else {
        return vec![];
    };
    let mut sol_delta = *post as i128 - *pre as i128;
    if tx.fee_payer() == Some(wallet) {
        sol_delta += tx.fee as i128;
    }
    sol_delta += token_balance(&tx.post_tokens, wallet, &WSOL_MINT) as i128
        - token_balance(&tx.pre_tokens, wallet, &WSOL_MINT) as i128;

    let mut mints: Vec<Pubkey> = tx
        .pre_tokens
        .iter()
        .chain(tx.post_tokens.iter())
        .filter(|b| b.owner == *wallet && b.mint != WSOL_MINT && b.mint != USDC_MINT)
        .map(|b| b.mint)
        .collect();
    mints.sort();
    mints.dedup();
    let deltas: Vec<(Pubkey, i128)> = mints
        .into_iter()
        .map(|m| {
            (
                m,
                token_balance(&tx.post_tokens, wallet, &m) as i128
                    - token_balance(&tx.pre_tokens, wallet, &m) as i128,
            )
        })
        .filter(|(_, d)| *d != 0)
        .collect();
    if deltas.len() != 1 {
        return vec![]; // transfers, multi-leg routes: not a clean swap
    }
    let (mint, tok) = deltas[0];
    let side = match (tok > 0, sol_delta < 0) {
        (true, true) => Side::Buy,
        (false, false) if sol_delta > 0 => Side::Sell,
        _ => return vec![],
    };
    let (tp, dec) = token_program_of(tx, &mint);
    let sol = sol_delta.unsigned_abs() as u64;
    let toks = tok.unsigned_abs() as u64;
    let price = (sol as f64 / 1e9) / (toks as f64 / 10f64.powi(dec as i32));
    vec![DetectedSwap {
        wallet: *wallet,
        mint,
        side,
        venue: venue_of(tx),
        sol_amount: sol,
        token_amount: toks,
        token_decimals: dec,
        token_program: tp,
        price_sol: price,
        pool_sol: None,
        fraction_sold: if side == Side::Sell {
            fraction_sold(tx, wallet, &mint, toks)
        } else {
            None
        },
        exact: false,
        template: Template::Generic,
        creator: None,
        migrated: false,
    }]
}

/// Swaps executed by `wallet` in this transaction.
pub fn swaps_by(tx: &ChainTx, wallet: &Pubkey) -> Vec<DetectedSwap> {
    if tx.failed {
        return vec![];
    }
    let mut out: Vec<DetectedSwap> = pump_events(tx)
        .iter()
        .filter_map(|e| match e {
            PumpEvent::Trade(t) if t.user == *wallet => Some(curve_swap(tx, t)),
            _ => None,
        })
        .collect();
    out.extend(
        amm_events(tx)
            .iter()
            .filter(|e| e.user == *wallet)
            .filter_map(|e| amm_swap(tx, e)),
    );
    if out.is_empty() && tx.has_meta {
        out = balance_swaps(tx, wallet);
    }
    out
}

/// Every swap in the transaction by anyone (market firehose / price tracking).
pub fn all_swaps(tx: &ChainTx) -> Vec<DetectedSwap> {
    if tx.failed {
        return vec![];
    }
    let mut out: Vec<DetectedSwap> = pump_events(tx)
        .iter()
        .filter_map(|e| match e {
            PumpEvent::Trade(t) => Some(curve_swap(tx, t)),
            _ => None,
        })
        .collect();
    out.extend(amm_events(tx).iter().filter_map(|e| amm_swap(tx, e)));
    if out.is_empty() && tx.has_meta {
        for s in tx.signers().to_vec() {
            out.extend(balance_swaps(tx, &s));
        }
    }
    out
}

/// Token creation / migration lifecycle events.
pub fn lifecycle(tx: &ChainTx) -> Vec<PumpEvent> {
    pump_events(tx)
        .into_iter()
        .filter(|e| !matches!(e, PumpEvent::Trade(_)))
        .collect()
}

/// A leader buy seen *before execution* (deshred feed): only the instruction
/// is known, so it carries the mint and the SOL the leader is willing to pay.
#[derive(Debug, Clone, PartialEq)]
pub struct PreExecBuy {
    pub wallet: Pubkey,
    pub mint: Pubkey,
    /// Upper bound of SOL the leader commits (max cost / exact spend).
    pub sol_amount: u64,
}

/// Decode top-level Pump curve buy instructions signed by `wallet`.
pub fn pre_exec_pump_buys(tx: &ChainTx, wallet: &Pubkey) -> Vec<PreExecBuy> {
    let legacy_buy = pump::ix_disc("buy");
    let legacy_exact = pump::ix_disc("buy_exact_sol_in");
    let v2_buy = pump::ix_disc("buy_v2");
    let v2_exact = pump::ix_disc("buy_exact_quote_in_v2");
    let mut out = Vec::new();
    for ix in tx
        .top
        .iter()
        .filter(|ix| ix.program == PUMP_PROGRAM && ix.data.len() >= 24)
    {
        let d = &ix.data;
        let arg = |i: usize| u64::from_le_bytes(d[8 + 8 * i..16 + 8 * i].try_into().unwrap());
        let (mint_i, user_i, sol) = if d.starts_with(&legacy_buy) {
            (2, 6, arg(1))
        } else if d.starts_with(&legacy_exact) {
            (2, 6, arg(0))
        } else if d.starts_with(&v2_buy) {
            (1, 13, arg(1))
        } else if d.starts_with(&v2_exact) {
            (1, 13, arg(0))
        } else {
            continue;
        };
        if ix.accounts.get(user_i) == Some(wallet) {
            if let Some(m) = ix.accounts.get(mint_i) {
                out.push(PreExecBuy {
                    wallet: *wallet,
                    mint: *m,
                    sol_amount: sol,
                });
            }
        }
    }
    out
}
