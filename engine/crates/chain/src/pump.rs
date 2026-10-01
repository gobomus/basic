//! Pump.fun bonding curve: events, account decoding, quoting, and the
//! `buy_exact_quote_in_v2` / `sell_v2` instructions.
//!
//! Account order is taken from the official IDL (engine/idl/pump.json) and the
//! test suite asserts it still matches, so an IDL update that changes the
//! interface fails CI instead of failing on-chain.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

use crate::borsh::{anchor_disc, Eof, Reader, EVENT_IX_TAG};
use crate::consts::*;
use crate::pda;

pub fn ix_disc(name: &str) -> [u8; 8] {
    anchor_disc("global", name)
}
pub fn event_disc(name: &str) -> [u8; 8] {
    anchor_disc("event", name)
}
pub fn account_disc(name: &str) -> [u8; 8] {
    anchor_disc("account", name)
}

// ------------------------------------------------------------------ events

#[derive(Debug, Clone, PartialEq)]
pub struct TradeEvent {
    pub mint: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: Pubkey,
    pub timestamp: i64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub fee_recipient: Pubkey,
    pub fee_basis_points: u64,
    pub fee: u64,
    pub creator: Pubkey,
    pub creator_fee_basis_points: u64,
    pub creator_fee: u64,
    pub ix_name: String,
    pub mayhem_mode: bool,
    pub buyback_fee_basis_points: u64,
    pub quote_mint: Option<Pubkey>,
}

impl TradeEvent {
    pub fn decode(data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(data);
        let mint = r.pubkey()?;
        let sol_amount = r.u64()?;
        let token_amount = r.u64()?;
        let is_buy = r.bool()?;
        let user = r.pubkey()?;
        let timestamp = r.i64()?;
        let virtual_sol_reserves = r.u64()?;
        let virtual_token_reserves = r.u64()?;
        let real_sol_reserves = r.u64()?;
        let real_token_reserves = r.u64()?;
        let fee_recipient = r.pubkey()?;
        let fee_basis_points = r.u64()?;
        let fee = r.u64()?;
        let creator = r.pubkey()?;
        let creator_fee_basis_points = r.u64()?;
        let creator_fee = r.u64()?;
        // track_volume, total_unclaimed_tokens, total_claimed_tokens, current_sol_volume, last_update_timestamp
        let tail = r.opt(|r| {
            r.skip(1 + 8 + 8 + 8 + 8)?;
            let ix_name = r.string()?;
            let mayhem = r.bool()?;
            r.skip(8 + 8)?; // cashback bps, cashback
            let buyback_bps = r.u64()?;
            r.skip(8)?; // buyback_fee
            let n = r.u32()? as usize; // shareholders: Vec<{pubkey,u16}>
            r.skip(n * 34)?;
            let quote_mint = r.opt(|r| r.pubkey());
            Ok((ix_name, mayhem, buyback_bps, quote_mint))
        });
        let (ix_name, mayhem_mode, buyback_fee_basis_points, quote_mint) =
            tail.unwrap_or((String::new(), false, 0, None));
        Ok(Self {
            mint,
            sol_amount,
            token_amount,
            is_buy,
            user,
            timestamp,
            virtual_sol_reserves,
            virtual_token_reserves,
            real_sol_reserves,
            real_token_reserves,
            fee_recipient,
            fee_basis_points,
            fee,
            creator,
            creator_fee_basis_points,
            creator_fee,
            ix_name,
            mayhem_mode,
            buyback_fee_basis_points,
            quote_mint,
        })
    }

    /// Spot price after this trade, SOL per whole token.
    pub fn price_sol(&self) -> f64 {
        spot_price(self.virtual_sol_reserves, self.virtual_token_reserves)
    }

    pub fn total_fee_bps(&self) -> u64 {
        self.fee_basis_points + self.creator_fee_basis_points + self.buyback_fee_basis_points
    }

    pub fn curve_state(&self) -> CurveState {
        CurveState {
            virtual_token_reserves: self.virtual_token_reserves,
            virtual_quote_reserves: self.virtual_sol_reserves,
            real_token_reserves: self.real_token_reserves,
            real_quote_reserves: self.real_sol_reserves,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateEvent {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub user: Pubkey,
    pub creator: Pubkey,
    pub timestamp: i64,
    pub token_program: Option<Pubkey>,
    pub is_mayhem_mode: bool,
}

impl CreateEvent {
    pub fn decode(data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(data);
        let name = r.string()?;
        let symbol = r.string()?;
        let uri = r.string()?;
        let mint = r.pubkey()?;
        let bonding_curve = r.pubkey()?;
        let user = r.pubkey()?;
        let creator = r.pubkey()?;
        let timestamp = r.i64()?;
        let tail = r.opt(|r| {
            r.skip(8 * 4)?; // virtual_token, virtual_sol, real_token, total_supply
            let tp = r.pubkey()?;
            let mayhem = r.bool()?;
            Ok((tp, mayhem))
        });
        Ok(Self {
            name,
            symbol,
            uri,
            mint,
            bonding_curve,
            user,
            creator,
            timestamp,
            token_program: tail.map(|t| t.0),
            is_mayhem_mode: tail.is_some_and(|t| t.1),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompleteEvent {
    pub user: Pubkey,
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub timestamp: i64,
}

impl CompleteEvent {
    pub fn decode(data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(data);
        Ok(Self {
            user: r.pubkey()?,
            mint: r.pubkey()?,
            bonding_curve: r.pubkey()?,
            timestamp: r.i64()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PumpEvent {
    Trade(TradeEvent),
    Create(CreateEvent),
    Complete(CompleteEvent),
}

/// Decode an `emit_cpi!` self-invocation's instruction data
/// (`EVENT_IX_TAG ‖ event discriminator ‖ borsh`).
pub fn decode_event_ix(data: &[u8]) -> Option<PumpEvent> {
    let body = data.strip_prefix(&EVENT_IX_TAG[..])?;
    decode_event(body)
}

/// Decode `discriminator ‖ borsh` (also the format of `Program data:` logs).
pub fn decode_event(body: &[u8]) -> Option<PumpEvent> {
    if body.len() < 8 {
        return None;
    }
    let (disc, rest) = body.split_at(8);
    if disc == event_disc("TradeEvent") {
        TradeEvent::decode(rest).ok().map(PumpEvent::Trade)
    } else if disc == event_disc("CreateEvent") {
        CreateEvent::decode(rest).ok().map(PumpEvent::Create)
    } else if disc == event_disc("CompleteEvent") {
        CompleteEvent::decode(rest).ok().map(PumpEvent::Complete)
    } else {
        None
    }
}

// ------------------------------------------------------------------ accounts

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveState {
    pub virtual_token_reserves: u64,
    pub virtual_quote_reserves: u64,
    pub real_token_reserves: u64,
    pub real_quote_reserves: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BondingCurve {
    pub state: CurveState,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    /// `Pubkey::default()` for SOL-paired coins.
    pub quote_mint: Pubkey,
}

impl BondingCurve {
    pub fn decode(account_data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(account_data);
        r.skip(8)?;
        let state = CurveState {
            virtual_token_reserves: r.u64()?,
            virtual_quote_reserves: r.u64()?,
            real_token_reserves: r.u64()?,
            real_quote_reserves: r.u64()?,
        };
        let token_total_supply = r.u64()?;
        let complete = r.bool()?;
        let creator = r.pubkey()?;
        let is_mayhem_mode = r.opt(|r| r.bool()).unwrap_or(false);
        let is_cashback_coin = r.opt(|r| r.bool()).unwrap_or(false);
        let quote_mint = r.opt(|r| r.pubkey()).unwrap_or_default();
        Ok(Self {
            state,
            token_total_supply,
            complete,
            creator,
            is_mayhem_mode,
            is_cashback_coin,
            quote_mint,
        })
    }
}

// ------------------------------------------------------------------ quoting

pub fn spot_price(virtual_quote: u64, virtual_token: u64) -> f64 {
    if virtual_token == 0 {
        return 0.0;
    }
    let tokens = virtual_token as f64 / 10f64.powi(PUMP_TOKEN_DECIMALS as i32);
    (virtual_quote as f64 / 1e9) / tokens
}

/// Tokens received for spending `quote_in` lamports (fees included), as in
/// the official SDK's `getBuyTokenAmountFromSolAmount`.
pub fn buy_tokens_for_quote(s: &CurveState, quote_in: u64, total_fee_bps: u64) -> u64 {
    if quote_in == 0 || s.virtual_token_reserves == 0 {
        return 0;
    }
    let input = (quote_in as u128 - 1) * 10_000 / (total_fee_bps as u128 + 10_000);
    let out = input * s.virtual_token_reserves as u128 / (s.virtual_quote_reserves as u128 + input);
    (out as u64).min(s.real_token_reserves)
}

/// Lamports received for selling `tokens`, after fees (`getSellSolAmountFromTokenAmount`).
pub fn sell_quote_for_tokens(s: &CurveState, tokens: u64, total_fee_bps: u64) -> u64 {
    if tokens == 0 || s.virtual_token_reserves == 0 {
        return 0;
    }
    let gross = tokens as u128 * s.virtual_quote_reserves as u128
        / (s.virtual_token_reserves as u128 + tokens as u128);
    let fee = (gross * total_fee_bps as u128).div_ceil(10_000);
    gross.saturating_sub(fee) as u64
}

pub fn apply_slippage_down(amount: u64, bps: u32) -> u64 {
    (amount as u128 * (10_000 - bps.min(10_000) as u128) / 10_000) as u64
}

// ------------------------------------------------------------------ instructions

/// Everything needed to trade one coin on the curve. Can be built entirely
/// from a leader's `TradeEvent` + the mint's token program (no RPC call).
#[derive(Debug, Clone, PartialEq)]
pub struct CurveCoin {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub base_token_program: Pubkey,
    pub is_mayhem_mode: bool,
    /// Quote mint as passed to v2 instructions (wrapped SOL for SOL-paired coins).
    pub quote_mint: Pubkey,
    pub quote_token_program: Pubkey,
}

impl CurveCoin {
    pub fn sol_paired(
        mint: Pubkey,
        creator: Pubkey,
        base_token_program: Pubkey,
        is_mayhem_mode: bool,
    ) -> Self {
        Self {
            mint,
            creator,
            base_token_program,
            is_mayhem_mode,
            quote_mint: WSOL_MINT,
            quote_token_program: TOKEN_PROGRAM,
        }
    }
}

/// Pick fee recipients the way the SDK does (uniformly at random per tx).
pub fn pick_fee_recipients(is_mayhem: bool, salt: u64) -> (Pubkey, Pubkey) {
    let i = (salt % 8) as usize;
    let j = ((salt / 8) % 8) as usize;
    let fee = if is_mayhem {
        PUMP_RESERVED_FEE_RECIPIENTS[i]
    } else {
        PUMP_FEE_RECIPIENTS[i]
    };
    (fee, PUMP_BUYBACK_FEE_RECIPIENTS[j])
}

fn v2_accounts(
    c: &CurveCoin,
    user: &Pubkey,
    fee_recipient: &Pubkey,
    buyback: &Pubkey,
    is_buy: bool,
) -> Vec<AccountMeta> {
    let bc = pda::bonding_curve(&c.mint);
    let cv = pda::creator_vault(&c.creator);
    let uva = pda::pump_user_volume_accumulator(user);
    let q = |owner: &Pubkey| pda::ata(owner, &c.quote_mint, &c.quote_token_program);
    let mut a = vec![
        AccountMeta::new_readonly(pda::pump_global(), false),
        AccountMeta::new_readonly(c.mint, false),
        AccountMeta::new_readonly(c.quote_mint, false),
        AccountMeta::new_readonly(c.base_token_program, false),
        AccountMeta::new_readonly(c.quote_token_program, false),
        AccountMeta::new_readonly(ATA_PROGRAM, false),
        AccountMeta::new(*fee_recipient, false),
        AccountMeta::new(q(fee_recipient), false),
        AccountMeta::new(*buyback, false),
        AccountMeta::new(q(buyback), false),
        AccountMeta::new(bc, false),
        AccountMeta::new(pda::ata(&bc, &c.mint, &c.base_token_program), false),
        AccountMeta::new(q(&bc), false),
        AccountMeta::new(*user, true),
        AccountMeta::new(pda::ata(user, &c.mint, &c.base_token_program), false),
        AccountMeta::new(q(user), false),
        AccountMeta::new(cv, false),
        AccountMeta::new(q(&cv), false),
        AccountMeta::new_readonly(pda::sharing_config(&c.mint), false),
    ];
    if is_buy {
        a.push(AccountMeta::new_readonly(
            pda::pump_global_volume_accumulator(),
            false,
        ));
    }
    a.extend([
        AccountMeta::new(uva, false),
        AccountMeta::new(q(&uva), false),
        AccountMeta::new_readonly(pda::fee_config_for(&PUMP_PROGRAM), false),
        AccountMeta::new_readonly(PUMP_FEE_PROGRAM, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(pda::pump_event_authority(), false),
        AccountMeta::new_readonly(PUMP_PROGRAM, false),
    ]);
    a
}

/// `buy_exact_quote_in_v2`: spend exactly `spendable_quote_in` lamports
/// (fees included), receive at least `min_tokens_out`.
pub fn buy_exact_quote_in_v2(
    c: &CurveCoin,
    user: &Pubkey,
    spendable_quote_in: u64,
    min_tokens_out: u64,
    salt: u64,
) -> Instruction {
    let (fee, buyback) = pick_fee_recipients(c.is_mayhem_mode, salt);
    let mut data = ix_disc("buy_exact_quote_in_v2").to_vec();
    data.extend_from_slice(&spendable_quote_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    Instruction {
        program_id: PUMP_PROGRAM,
        accounts: v2_accounts(c, user, &fee, &buyback, true),
        data,
    }
}

/// `sell_v2`: sell `amount` tokens for at least `min_sol_output` lamports.
pub fn sell_v2(
    c: &CurveCoin,
    user: &Pubkey,
    amount: u64,
    min_sol_output: u64,
    salt: u64,
) -> Instruction {
    let (fee, buyback) = pick_fee_recipients(c.is_mayhem_mode, salt);
    let mut data = ix_disc("sell_v2").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());
    Instruction {
        program_id: PUMP_PROGRAM,
        accounts: v2_accounts(c, user, &fee, &buyback, false),
        data,
    }
}
