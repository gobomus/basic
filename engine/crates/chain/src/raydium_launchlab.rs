//! Raydium LaunchLab (the curve behind LetsBONK.fun and other platforms).
//!
//! Sources of truth: the program IDL (github.com/raydium-io/raydium-idl,
//! vendored as engine/idl/raydium_launchpad.json) for the 15 declared
//! accounts and events, and `@raydium-io/raydium-sdk-v2` (launchpad/
//! instrument.ts, Sep 2026) which appends three accounts the deployed
//! program now expects: system program, platform fee vault, creator fee
//! vault (preceded by an optional share-fee receiver we never use).

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

use crate::borsh::{anchor_disc, Eof, Reader, EVENT_IX_TAG};
use crate::consts::*;
use crate::ixs;
use crate::pda;

pub const LAUNCHLAB_PROGRAM: Pubkey = pubkey!("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");

pub fn ix_disc(name: &str) -> [u8; 8] {
    anchor_disc("global", name)
}
pub fn event_disc(name: &str) -> [u8; 8] {
    anchor_disc("event", name)
}

fn pda_of(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &LAUNCHLAB_PROGRAM).0
}
pub fn authority() -> Pubkey {
    static V: std::sync::OnceLock<Pubkey> = std::sync::OnceLock::new();
    *V.get_or_init(|| pda_of(&[b"vault_auth_seed"]))
}
pub fn event_authority() -> Pubkey {
    static V: std::sync::OnceLock<Pubkey> = std::sync::OnceLock::new();
    *V.get_or_init(|| pda_of(&[b"__event_authority"]))
}
/// SDK `getPdaPlatformVault(programId, platformId, mintB)`.
pub fn platform_fee_vault(platform_config: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    pda_of(&[platform_config.as_ref(), quote_mint.as_ref()])
}
/// SDK `getPdaCreatorVault(programId, creator, mintB)`.
pub fn creator_fee_vault(creator: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    pda_of(&[creator.as_ref(), quote_mint.as_ref()])
}

// ------------------------------------------------------------------ events

#[derive(Debug, Clone, PartialEq)]
pub struct TradeEvt {
    pub pool: Pubkey,
    pub virtual_base: u64,
    pub virtual_quote: u64,
    pub real_base_after: u64,
    pub real_quote_after: u64,
    pub amount_in: u64,
    pub amount_out: u64,
    pub total_fee: u64,
    pub is_buy: bool,
    /// 0 Fund (on curve), 1 Migrate, 2 Trade (migrated).
    pub pool_status: u8,
}

impl TradeEvt {
    pub fn decode(data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(data);
        let pool = r.pubkey()?;
        r.skip(8)?; // total_base_sell
        let virtual_base = r.u64()?;
        let virtual_quote = r.u64()?;
        r.skip(16)?; // real_base_before, real_quote_before
        let real_base_after = r.u64()?;
        let real_quote_after = r.u64()?;
        let amount_in = r.u64()?;
        let amount_out = r.u64()?;
        let total_fee = r.u64()? + r.u64()? + r.u64()? + r.u64()?;
        let dir = r.u8()?;
        let pool_status = r.u8()?;
        Ok(Self {
            pool,
            virtual_base,
            virtual_quote,
            real_base_after,
            real_quote_after,
            amount_in,
            amount_out,
            total_fee,
            is_buy: dir == 0,
            pool_status,
        })
    }

    /// Constant-product spot price after the trade, raw quote units per raw base unit
    /// (SDK `LaunchConstantProductCurve.getPoolPrice`).
    pub fn price_raw(&self) -> f64 {
        let base = self.virtual_base.saturating_sub(self.real_base_after);
        if base == 0 {
            return 0.0;
        }
        (self.virtual_quote as f64 + self.real_quote_after as f64) / base as f64
    }

    pub fn fee_bps_estimate(&self) -> u64 {
        let base = if self.is_buy {
            self.amount_in
        } else {
            self.amount_out + self.total_fee
        };
        if base == 0 {
            return 200;
        }
        let bps = (self.total_fee as u128 * 10_000 / base as u128) as u64;
        if bps == 0 || bps > 1_000 {
            200
        } else {
            bps
        }
    }
}

pub fn decode_event(body: &[u8]) -> Option<TradeEvt> {
    let rest = body.strip_prefix(&event_disc("TradeEvent")[..])?;
    TradeEvt::decode(rest).ok()
}

pub fn decode_event_ix(data: &[u8]) -> Option<TradeEvt> {
    decode_event(data.strip_prefix(&EVENT_IX_TAG[..])?)
}

// ------------------------------------------------------------------ execution

#[derive(Debug, Clone, PartialEq)]
pub struct LaunchCoin {
    pub pool: Pubkey,
    pub global_config: Pubkey,
    pub platform_config: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    pub platform_fee_vault: Pubkey,
    pub creator_fee_vault: Pubkey,
}

const FIXED: usize = 15;

impl LaunchCoin {
    /// Rebuild from any observed buy/sell instruction on this pool.
    pub fn from_trade_ix(accounts: &[Pubkey], data: &[u8]) -> Option<Self> {
        let known = [
            "buy_exact_in",
            "buy_exact_out",
            "sell_exact_in",
            "sell_exact_out",
        ];
        if accounts.len() < FIXED + 3 || !known.iter().any(|n| data.starts_with(&ix_disc(n))) {
            return None;
        }
        let sys = accounts[FIXED..]
            .iter()
            .position(|a| *a == SYSTEM_PROGRAM)?
            + FIXED;
        Some(Self {
            global_config: accounts[2],
            platform_config: accounts[3],
            pool: accounts[4],
            base_vault: accounts[7],
            quote_vault: accounts[8],
            base_mint: accounts[9],
            quote_mint: accounts[10],
            base_token_program: accounts[11],
            quote_token_program: accounts[12],
            platform_fee_vault: *accounts.get(sys + 1)?,
            creator_fee_vault: *accounts.get(sys + 2)?,
        })
    }

    pub fn payer_of(accounts: &[Pubkey]) -> Option<Pubkey> {
        accounts.first().copied()
    }
}

fn trade_ix(
    c: &LaunchCoin,
    user: &Pubkey,
    name: &str,
    amount_in: u64,
    min_out: u64,
) -> Instruction {
    let mut data = ix_disc(name).to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // share_fee_rate
    Instruction {
        program_id: LAUNCHLAB_PROGRAM,
        accounts: vec![
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(authority(), false),
            AccountMeta::new_readonly(c.global_config, false),
            AccountMeta::new_readonly(c.platform_config, false),
            AccountMeta::new(c.pool, false),
            AccountMeta::new(pda::ata(user, &c.base_mint, &c.base_token_program), false),
            AccountMeta::new(pda::ata(user, &c.quote_mint, &c.quote_token_program), false),
            AccountMeta::new(c.base_vault, false),
            AccountMeta::new(c.quote_vault, false),
            AccountMeta::new_readonly(c.base_mint, false),
            AccountMeta::new_readonly(c.quote_mint, false),
            AccountMeta::new_readonly(c.base_token_program, false),
            AccountMeta::new_readonly(c.quote_token_program, false),
            AccountMeta::new_readonly(event_authority(), false),
            AccountMeta::new_readonly(LAUNCHLAB_PROGRAM, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new(c.platform_fee_vault, false),
            AccountMeta::new(c.creator_fee_vault, false),
        ],
        data,
    }
}

pub fn buy_instructions(
    c: &LaunchCoin,
    user: &Pubkey,
    quote_in: u64,
    min_tokens_out: u64,
) -> Vec<Instruction> {
    let wsol = c.quote_mint == WSOL_MINT;
    let wsol_ata = pda::ata(user, &WSOL_MINT, &TOKEN_PROGRAM);
    let mut out = vec![
        ixs::create_ata_idempotent(user, user, &c.base_mint, &c.base_token_program),
        ixs::create_ata_idempotent(user, user, &c.quote_mint, &c.quote_token_program),
    ];
    if wsol {
        out.push(ixs::system_transfer(user, &wsol_ata, quote_in));
        out.push(ixs::sync_native(&wsol_ata));
    }
    out.push(trade_ix(c, user, "buy_exact_in", quote_in, min_tokens_out));
    if wsol {
        out.push(ixs::close_token_account(
            &wsol_ata,
            user,
            user,
            &TOKEN_PROGRAM,
        ));
    }
    out
}

pub fn sell_instructions(
    c: &LaunchCoin,
    user: &Pubkey,
    tokens: u64,
    min_quote_out: u64,
) -> Vec<Instruction> {
    let wsol = c.quote_mint == WSOL_MINT;
    let wsol_ata = pda::ata(user, &WSOL_MINT, &TOKEN_PROGRAM);
    let mut out = vec![
        ixs::create_ata_idempotent(user, user, &c.quote_mint, &c.quote_token_program),
        trade_ix(c, user, "sell_exact_in", tokens, min_quote_out),
    ];
    if wsol {
        out.push(ixs::close_token_account(
            &wsol_ata,
            user,
            user,
            &TOKEN_PROGRAM,
        ));
    }
    out
}
