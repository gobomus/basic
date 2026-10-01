//! Meteora Dynamic Bonding Curve (DBC) — the launch curve behind Bags,
//! Jupiter Studio, Believe and many other launchpads.
//!
//! Source of truth: the IDL embedded in `@meteora-ag/dynamic-bonding-curve-sdk`
//! (vendored as engine/idl/meteora_dbc.json) and that SDK's `swap()` builder:
//! 15 fixed accounts, optional referral (program id when absent), plus
//! `SYSVAR_INSTRUCTIONS` as a remaining account while the rate limiter or the
//! first-swap-min-fee rule applies. We mirror the leader's own instruction for
//! that remaining account, since we trade right behind the leader.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

use crate::borsh::{anchor_disc, Eof, Reader, EVENT_IX_TAG};
use crate::consts::*;
use crate::ixs;
use crate::pda;

pub const DBC_PROGRAM: Pubkey = pubkey!("dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN");
pub const DBC_POOL_AUTHORITY: Pubkey = pubkey!("FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM");
pub const SYSVAR_INSTRUCTIONS: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");

pub fn ix_disc(name: &str) -> [u8; 8] {
    anchor_disc("global", name)
}
pub fn event_disc(name: &str) -> [u8; 8] {
    anchor_disc("event", name)
}

pub fn event_authority() -> Pubkey {
    static V: std::sync::OnceLock<Pubkey> = std::sync::OnceLock::new();
    *V.get_or_init(|| Pubkey::find_program_address(&[b"__event_authority"], &DBC_PROGRAM).0)
}

// ------------------------------------------------------------------ events

/// `EvtSwap` / `EvtSwap2` (fields common to both that we use).
#[derive(Debug, Clone, PartialEq)]
pub struct SwapEvt {
    pub pool: Pubkey,
    pub config: Pubkey,
    /// 1 = QuoteToBase (buy), 0 = BaseToQuote (sell).
    pub is_buy: bool,
    pub input_amount: u64,
    pub output_amount: u64,
    pub next_sqrt_price: u128,
    pub total_fee: u64,
    /// EvtSwap2 only: quote reserve after the swap and the migration threshold.
    pub quote_reserve: Option<u64>,
    pub migration_threshold: Option<u64>,
}

impl SwapEvt {
    fn decode_v1(data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(data);
        let pool = r.pubkey()?;
        let config = r.pubkey()?;
        let dir = r.u8()?;
        r.skip(1)?; // has_referral
        r.skip(16)?; // params: amount_in, minimum_amount_out
        let actual_input = r.u64()?;
        let output = r.u64()?;
        let next_sqrt_price = r.u128()?;
        let fee = r.u64()? + r.u64()? + r.u64()?;
        Ok(Self {
            pool,
            config,
            is_buy: dir == 1,
            input_amount: actual_input,
            output_amount: output,
            next_sqrt_price,
            total_fee: fee,
            quote_reserve: None,
            migration_threshold: None,
        })
    }

    fn decode_v2(data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(data);
        let pool = r.pubkey()?;
        let config = r.pubkey()?;
        let dir = r.u8()?;
        r.skip(1)?; // has_referral
        r.skip(17)?; // SwapParameters2: amount_0, amount_1, swap_mode
        let included_fee_input = r.u64()?;
        r.skip(16)?; // excluded_fee_input_amount, amount_left
        let output = r.u64()?;
        let next_sqrt_price = r.u128()?;
        let fee = r.u64()? + r.u64()? + r.u64()?;
        let quote_reserve = r.opt(|r| r.u64());
        let migration_threshold = r.opt(|r| r.u64());
        Ok(Self {
            pool,
            config,
            is_buy: dir == 1,
            input_amount: included_fee_input,
            output_amount: output,
            next_sqrt_price,
            total_fee: fee,
            quote_reserve,
            migration_threshold,
        })
    }

    /// Fee as bps of the trade, bounded to a sane range (fee mint varies by config).
    pub fn fee_bps_estimate(&self) -> u64 {
        let base = if self.is_buy {
            self.input_amount
        } else {
            self.output_amount + self.total_fee
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

    pub fn curve_progress(&self) -> Option<f64> {
        match (self.quote_reserve, self.migration_threshold) {
            (Some(q), Some(t)) if t > 0 => Some(q as f64 / t as f64),
            _ => None,
        }
    }
}

pub fn decode_event_ix(data: &[u8]) -> Option<SwapEvt> {
    let body = data.strip_prefix(&EVENT_IX_TAG[..])?;
    if body.len() < 8 {
        return None;
    }
    let (disc, rest) = body.split_at(8);
    if disc == event_disc("EvtSwap") {
        SwapEvt::decode_v1(rest).ok()
    } else if disc == event_disc("EvtSwap2") || disc == event_disc("EvtSwap2WithTransferHook") {
        SwapEvt::decode_v2(rest).ok()
    } else {
        None
    }
}

/// Raw price (quote base-units per base base-unit) from a Q64.64 sqrt price.
pub fn price_raw_from_sqrt(sqrt_q64: u128) -> f64 {
    let s = sqrt_q64 as f64 / 18_446_744_073_709_551_616.0;
    s * s
}

/// SOL per whole token.
pub fn price_sol(sqrt_q64: u128, base_decimals: u8) -> f64 {
    price_raw_from_sqrt(sqrt_q64) * 10f64.powi(base_decimals as i32) / 1e9
}

// ------------------------------------------------------------------ execution

#[derive(Debug, Clone, PartialEq)]
pub struct DbcCoin {
    pub pool: Pubkey,
    pub config: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    /// Pass SYSVAR_INSTRUCTIONS as a remaining account (rate limiter / first-swap rule).
    pub needs_ix_sysvar: bool,
}

const FIXED: usize = 15;

impl DbcCoin {
    /// Rebuild from any observed `swap` / `swap2` instruction on this pool.
    pub fn from_swap_ix(accounts: &[Pubkey], data: &[u8]) -> Option<Self> {
        if accounts.len() < FIXED
            || !(data.starts_with(&ix_disc("swap")) || data.starts_with(&ix_disc("swap2")))
        {
            return None;
        }
        Some(Self {
            config: accounts[1],
            pool: accounts[2],
            base_vault: accounts[5],
            quote_vault: accounts[6],
            base_mint: accounts[7],
            quote_mint: accounts[8],
            base_token_program: accounts[10],
            quote_token_program: accounts[11],
            needs_ix_sysvar: accounts[FIXED..].contains(&SYSVAR_INSTRUCTIONS),
        })
    }

    pub fn payer_of(accounts: &[Pubkey]) -> Option<Pubkey> {
        accounts.get(9).copied()
    }
}

fn swap_ix(c: &DbcCoin, user: &Pubkey, is_buy: bool, amount_in: u64, min_out: u64) -> Instruction {
    let base_ata = pda::ata(user, &c.base_mint, &c.base_token_program);
    let quote_ata = pda::ata(user, &c.quote_mint, &c.quote_token_program);
    let (input, output) = if is_buy {
        (quote_ata, base_ata)
    } else {
        (base_ata, quote_ata)
    };
    let mut accounts = vec![
        AccountMeta::new_readonly(DBC_POOL_AUTHORITY, false),
        AccountMeta::new_readonly(c.config, false),
        AccountMeta::new(c.pool, false),
        AccountMeta::new(input, false),
        AccountMeta::new(output, false),
        AccountMeta::new(c.base_vault, false),
        AccountMeta::new(c.quote_vault, false),
        AccountMeta::new_readonly(c.base_mint, false),
        AccountMeta::new_readonly(c.quote_mint, false),
        AccountMeta::new_readonly(*user, true),
        AccountMeta::new_readonly(c.base_token_program, false),
        AccountMeta::new_readonly(c.quote_token_program, false),
        // optional referral_token_account = None → program id (Anchor convention)
        AccountMeta::new_readonly(DBC_PROGRAM, false),
        AccountMeta::new_readonly(event_authority(), false),
        AccountMeta::new_readonly(DBC_PROGRAM, false),
    ];
    if c.needs_ix_sysvar {
        accounts.push(AccountMeta::new_readonly(SYSVAR_INSTRUCTIONS, false));
    }
    let mut data = ix_disc("swap").to_vec();
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    Instruction {
        program_id: DBC_PROGRAM,
        accounts,
        data,
    }
}

/// Buy with exactly `quote_in` (SOL), receiving at least `min_tokens_out`.
pub fn buy_instructions(
    c: &DbcCoin,
    user: &Pubkey,
    quote_in: u64,
    min_tokens_out: u64,
) -> Vec<Instruction> {
    let mut out = vec![ixs::create_ata_idempotent(
        user,
        user,
        &c.base_mint,
        &c.base_token_program,
    )];
    let wsol = c.quote_mint == WSOL_MINT;
    let wsol_ata = pda::ata(user, &WSOL_MINT, &TOKEN_PROGRAM);
    out.push(ixs::create_ata_idempotent(
        user,
        user,
        &c.quote_mint,
        &c.quote_token_program,
    ));
    if wsol {
        out.push(ixs::system_transfer(user, &wsol_ata, quote_in));
        out.push(ixs::sync_native(&wsol_ata));
    }
    out.push(swap_ix(c, user, true, quote_in, min_tokens_out));
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

/// Sell `tokens`, receiving at least `min_quote_out`.
pub fn sell_instructions(
    c: &DbcCoin,
    user: &Pubkey,
    tokens: u64,
    min_quote_out: u64,
) -> Vec<Instruction> {
    let wsol = c.quote_mint == WSOL_MINT;
    let wsol_ata = pda::ata(user, &WSOL_MINT, &TOKEN_PROGRAM);
    let mut out = vec![
        ixs::create_ata_idempotent(user, user, &c.quote_mint, &c.quote_token_program),
        swap_ix(c, user, false, tokens, min_quote_out),
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

/// Expected tokens for `quote_in` right after the observed swap (spot × fee buffer).
pub fn estimate_buy(price_raw: f64, quote_in: u64, fee_bps: u64) -> u64 {
    if price_raw <= 0.0 {
        return 0;
    }
    (quote_in as f64 * (1.0 - fee_bps as f64 / 10_000.0) / price_raw) as u64
}

pub fn estimate_sell(price_raw: f64, tokens: u64, fee_bps: u64) -> u64 {
    (tokens as f64 * price_raw * (1.0 - fee_bps as f64 / 10_000.0)) as u64
}
