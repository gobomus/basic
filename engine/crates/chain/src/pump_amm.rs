//! PumpSwap (pump AMM): events, Pool / GlobalConfig decoding, quoting, and
//! buy / sell instructions including the remaining accounts the official
//! `@pump-fun/pump-swap-sdk` appends (cashback ATA, pool-v2 PDA, buyback
//! fee recipient + its ATA) and the wrapped-SOL wrap/unwrap around them.

use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

use crate::borsh::{anchor_disc, Eof, Reader, EVENT_IX_TAG};
use crate::consts::*;
use crate::ixs;
use crate::pda;

pub fn ix_disc(name: &str) -> [u8; 8] {
    anchor_disc("global", name)
}
pub fn event_disc(name: &str) -> [u8; 8] {
    anchor_disc("event", name)
}

// ------------------------------------------------------------------ events

/// Fields shared by `BuyEvent` and `SellEvent` that we use.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapEventData {
    pub is_buy: bool,
    pub timestamp: i64,
    /// base_amount_out (buy) / base_amount_in (sell)
    pub base_amount: u64,
    /// `user_quote_amount_in` (buy: the amount net of protocol and creator fees)
    /// / `user_quote_amount_out` (sell: what the user received).
    pub user_quote_amount: u64,
    /// `quote_amount_in` (buy: everything the user paid, every fee included)
    /// / `quote_amount_out` (sell: the gross amount before fees).
    pub quote_amount: u64,
    /// `quote_amount_in_with_lp_fee` (buy: what the pool's quote reserve grew by)
    /// / `quote_amount_out_without_lp_fee` (sell: what it shrank by).
    pub pool_quote_delta: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub lp_fee_basis_points: u64,
    pub protocol_fee_basis_points: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
    pub protocol_fee_recipient: Pubkey,
    pub coin_creator: Pubkey,
    pub coin_creator_fee_basis_points: u64,
    pub buyback_fee_basis_points: u64,
    pub virtual_quote_reserves: i128,
}

impl SwapEventData {
    fn decode(data: &[u8], is_buy: bool) -> Result<Self, Eof> {
        let mut r = Reader::new(data);
        let timestamp = r.i64()?;
        let base_amount = r.u64()?;
        r.skip(8)?; // max_quote_amount_in / min_quote_amount_out
        r.skip(16)?; // user base/quote reserves
        let pool_base_token_reserves = r.u64()?;
        let pool_quote_token_reserves = r.u64()?;
        let quote_amount = r.u64()?; // quote_amount_in / quote_amount_out
        let lp_fee_basis_points = r.u64()?;
        r.skip(8)?; // lp_fee
        let protocol_fee_basis_points = r.u64()?;
        r.skip(8)?; // protocol_fee
        let pool_quote_delta = r.u64()?; // quote_amount_in_with_lp_fee / quote_amount_out_without_lp_fee
        let user_quote_amount = r.u64()?;
        let pool = r.pubkey()?;
        let user = r.pubkey()?;
        r.skip(64)?; // user base/quote token accounts
        let protocol_fee_recipient = r.pubkey()?;
        r.skip(32)?; // protocol fee recipient token account
        let coin_creator = r.pubkey()?;
        let coin_creator_fee_basis_points = r.u64()?;
        r.skip(8)?; // coin_creator_fee
        let tail = r.opt(|r| {
            if is_buy {
                r.skip(1 + 8 + 8 + 8 + 8 + 8)?; // track_volume .. min_base_amount_out
                r.string()?; // ix_name
            }
            r.skip(16)?; // cashback bps, cashback
            let buyback = r.u64()?;
            r.skip(8)?;
            let vqr = r.opt(|r| r.i128()).unwrap_or(0);
            Ok((buyback, vqr))
        });
        let (buyback_fee_basis_points, virtual_quote_reserves) = tail.unwrap_or((0, 0));
        Ok(Self {
            is_buy,
            timestamp,
            base_amount,
            user_quote_amount,
            quote_amount,
            pool_quote_delta,
            pool_base_token_reserves,
            pool_quote_token_reserves,
            lp_fee_basis_points,
            protocol_fee_basis_points,
            pool,
            user,
            protocol_fee_recipient,
            coin_creator,
            coin_creator_fee_basis_points,
            buyback_fee_basis_points,
            virtual_quote_reserves,
        })
    }

    /// Pool quote reserves before the swap, virtual reserves included.
    pub fn effective_quote_reserves(&self) -> u128 {
        (self.pool_quote_token_reserves as i128 + self.virtual_quote_reserves).max(0) as u128
    }

    /// Quote units the trader actually paid (buy, all fees included) or received (sell).
    pub fn user_flow(&self) -> u64 {
        if self.is_buy {
            self.quote_amount
        } else {
            self.user_quote_amount
        }
    }

    /// Pool reserves *after* this swap: (base, effective quote). The event
    /// carries the reserves from before it.
    pub fn post_reserves(&self) -> (u64, u128) {
        let q = self.effective_quote_reserves();
        if self.is_buy {
            (
                self.pool_base_token_reserves
                    .saturating_sub(self.base_amount),
                q + self.pool_quote_delta as u128,
            )
        } else {
            (
                self.pool_base_token_reserves
                    .saturating_add(self.base_amount),
                q.saturating_sub(self.pool_quote_delta as u128),
            )
        }
    }

    /// Spot price after the swap, quote per whole base token.
    pub fn post_price(&self, base_decimals: u8, quote_decimals: u8) -> f64 {
        let (b, q) = self.post_reserves();
        if b == 0 {
            return 0.0;
        }
        (q as f64 / 10f64.powi(quote_decimals as i32))
            / (b as f64 / 10f64.powi(base_decimals as i32))
    }

    /// Spot price after the swap (quote per whole base token), assuming 6 base decimals.
    pub fn price(&self, base_decimals: u8, quote_decimals: u8) -> f64 {
        if self.pool_base_token_reserves == 0 {
            return 0.0;
        }
        let q = self.effective_quote_reserves() as f64 / 10f64.powi(quote_decimals as i32);
        let b = self.pool_base_token_reserves as f64 / 10f64.powi(base_decimals as i32);
        q / b
    }

    pub fn total_fee_bps(&self) -> u64 {
        let creator = if self.coin_creator == Pubkey::default() {
            0
        } else {
            self.coin_creator_fee_basis_points
        };
        // `buyback_fee_basis_points` is the share of the protocol fee routed to
        // buybacks (5000 = half of it), not an additional fee.
        self.lp_fee_basis_points + self.protocol_fee_basis_points + creator
    }
}

pub fn decode_event_ix(data: &[u8]) -> Option<SwapEventData> {
    decode_event(data.strip_prefix(&EVENT_IX_TAG[..])?)
}

pub fn decode_event(body: &[u8]) -> Option<SwapEventData> {
    if body.len() < 8 {
        return None;
    }
    let (disc, rest) = body.split_at(8);
    if disc == event_disc("BuyEvent") {
        SwapEventData::decode(rest, true).ok()
    } else if disc == event_disc("SellEvent") {
        SwapEventData::decode(rest, false).ok()
    } else {
        None
    }
}

// ------------------------------------------------------------------ accounts

#[derive(Debug, Clone, PartialEq)]
pub struct Pool {
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    pub virtual_quote_reserves: i128,
}

impl Pool {
    pub fn decode(account_data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(account_data);
        r.skip(8)?;
        r.skip(1)?; // bump
        let index = r.u16()?;
        let creator = r.pubkey()?;
        let base_mint = r.pubkey()?;
        let quote_mint = r.pubkey()?;
        let lp_mint = r.pubkey()?;
        let pool_base_token_account = r.pubkey()?;
        let pool_quote_token_account = r.pubkey()?;
        r.skip(8)?; // lp_supply
        let coin_creator = r.opt(|r| r.pubkey()).unwrap_or_default();
        let is_mayhem_mode = r.opt(|r| r.bool()).unwrap_or(false);
        let is_cashback_coin = r.opt(|r| r.bool()).unwrap_or(false);
        let virtual_quote_reserves = r.opt(|r| r.i128()).unwrap_or(0);
        Ok(Self {
            index,
            creator,
            base_mint,
            quote_mint,
            lp_mint,
            pool_base_token_account,
            pool_quote_token_account,
            coin_creator,
            is_mayhem_mode,
            is_cashback_coin,
            virtual_quote_reserves,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalConfig {
    pub lp_fee_basis_points: u64,
    pub protocol_fee_basis_points: u64,
    pub protocol_fee_recipients: [Pubkey; 8],
    pub coin_creator_fee_basis_points: u64,
    pub reserved_fee_recipient: Pubkey,
    pub reserved_fee_recipients: [Pubkey; 7],
    pub buyback_fee_recipients: [Pubkey; 8],
}

impl GlobalConfig {
    pub fn decode(account_data: &[u8]) -> Result<Self, Eof> {
        let mut r = Reader::new(account_data);
        r.skip(8)?;
        r.skip(32)?; // admin
        let lp_fee_basis_points = r.u64()?;
        let protocol_fee_basis_points = r.u64()?;
        r.skip(1)?; // disable_flags
        let mut protocol_fee_recipients = [Pubkey::default(); 8];
        for p in &mut protocol_fee_recipients {
            *p = r.pubkey()?;
        }
        let coin_creator_fee_basis_points = r.u64()?;
        r.skip(32 + 32)?; // admin_set_coin_creator_authority, whitelist_pda
        let reserved_fee_recipient = r.pubkey()?;
        r.skip(1)?; // mayhem_mode_enabled
        let mut reserved_fee_recipients = [Pubkey::default(); 7];
        for p in &mut reserved_fee_recipients {
            *p = r.pubkey()?;
        }
        r.skip(1)?; // is_cashback_enabled
        let mut buyback_fee_recipients = [Pubkey::default(); 8];
        for p in &mut buyback_fee_recipients {
            *p = r.pubkey()?;
        }
        Ok(Self {
            lp_fee_basis_points,
            protocol_fee_basis_points,
            protocol_fee_recipients,
            coin_creator_fee_basis_points,
            reserved_fee_recipient,
            reserved_fee_recipients,
            buyback_fee_recipients,
        })
    }

    /// SDK `getFeeRecipient` / `getBuybackFeeRecipient`, with a caller-supplied salt.
    pub fn pick_recipients(&self, is_mayhem: bool, salt: u64) -> (Pubkey, Pubkey) {
        let fee = if is_mayhem {
            let i = (salt % 8) as usize;
            if i == 0 {
                self.reserved_fee_recipient
            } else {
                self.reserved_fee_recipients[i - 1]
            }
        } else {
            self.protocol_fee_recipients[(salt % 8) as usize]
        };
        (fee, self.buyback_fee_recipients[((salt / 8) % 8) as usize])
    }
}

// ------------------------------------------------------------------ quoting

/// Base tokens out for spending exactly `quote` (fees included). Port of SDK `buyQuoteInput`.
pub fn buy_base_for_quote(
    base_reserve: u64,
    effective_quote_reserve: u128,
    quote: u64,
    total_fee_bps: u64,
) -> u64 {
    if base_reserve == 0 || effective_quote_reserve == 0 || quote < 2 {
        return 0;
    }
    let quote = quote as u128;
    let mut effective = quote * 10_000 / (10_000 + total_fee_bps as u128);
    let fees = (effective * total_fee_bps as u128).div_ceil(10_000);
    if effective + fees > quote {
        effective -= effective + fees - quote;
    }
    let input = effective.saturating_sub(1);
    (base_reserve as u128 * input / (effective_quote_reserve + input)) as u64
}

/// Quote received for selling `base` after fees. Port of SDK `sellBaseInput`.
pub fn sell_quote_for_base(
    base_reserve: u64,
    effective_quote_reserve: u128,
    base: u64,
    total_fee_bps: u64,
) -> u64 {
    if base_reserve == 0 || base == 0 {
        return 0;
    }
    let gross = effective_quote_reserve * base as u128 / (base_reserve as u128 + base as u128);
    let fees = (gross * total_fee_bps as u128).div_ceil(10_000);
    gross.saturating_sub(fees) as u64
}

// ------------------------------------------------------------------ instructions

#[derive(Debug, Clone, PartialEq)]
pub struct AmmCoin {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    pub coin_creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    pub protocol_fee_recipient: Pubkey,
    pub buyback_fee_recipient: Pubkey,
}

impl AmmCoin {
    pub fn from_pool(
        pool_key: Pubkey,
        p: &Pool,
        base_token_program: Pubkey,
        quote_token_program: Pubkey,
        g: &GlobalConfig,
        salt: u64,
    ) -> Self {
        let (protocol_fee_recipient, buyback_fee_recipient) =
            g.pick_recipients(p.is_mayhem_mode, salt);
        Self {
            pool: pool_key,
            base_mint: p.base_mint,
            quote_mint: p.quote_mint,
            pool_base_token_account: p.pool_base_token_account,
            pool_quote_token_account: p.pool_quote_token_account,
            base_token_program,
            quote_token_program,
            coin_creator: p.coin_creator,
            is_mayhem_mode: p.is_mayhem_mode,
            is_cashback_coin: p.is_cashback_coin,
            protocol_fee_recipient,
            buyback_fee_recipient,
        }
    }
}

fn swap_accounts(c: &AmmCoin, user: &Pubkey, is_buy: bool) -> Vec<AccountMeta> {
    let ccva = pda::amm_coin_creator_vault_authority(&c.coin_creator);
    let mut a = vec![
        AccountMeta::new(c.pool, false),
        AccountMeta::new(*user, true),
        AccountMeta::new_readonly(pda::amm_global_config(), false),
        AccountMeta::new_readonly(c.base_mint, false),
        AccountMeta::new_readonly(c.quote_mint, false),
        AccountMeta::new(pda::ata(user, &c.base_mint, &c.base_token_program), false),
        AccountMeta::new(pda::ata(user, &c.quote_mint, &c.quote_token_program), false),
        AccountMeta::new(c.pool_base_token_account, false),
        AccountMeta::new(c.pool_quote_token_account, false),
        AccountMeta::new_readonly(c.protocol_fee_recipient, false),
        AccountMeta::new(
            pda::ata(
                &c.protocol_fee_recipient,
                &c.quote_mint,
                &c.quote_token_program,
            ),
            false,
        ),
        AccountMeta::new_readonly(c.base_token_program, false),
        AccountMeta::new_readonly(c.quote_token_program, false),
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(ATA_PROGRAM, false),
        AccountMeta::new_readonly(pda::amm_event_authority(), false),
        AccountMeta::new_readonly(PUMP_AMM_PROGRAM, false),
        AccountMeta::new(
            pda::ata(&ccva, &c.quote_mint, &c.quote_token_program),
            false,
        ),
        AccountMeta::new_readonly(ccva, false),
    ];
    if is_buy {
        a.push(AccountMeta::new_readonly(
            pda::amm_global_volume_accumulator(),
            false,
        ));
        a.push(AccountMeta::new(
            pda::amm_user_volume_accumulator(user),
            false,
        ));
    }
    a.push(AccountMeta::new_readonly(
        pda::fee_config_for(&PUMP_AMM_PROGRAM),
        false,
    ));
    a.push(AccountMeta::new_readonly(PUMP_FEE_PROGRAM, false));

    // Remaining accounts, in SDK order.
    let uva = pda::amm_user_volume_accumulator(user);
    if c.is_cashback_coin {
        a.push(AccountMeta::new(
            pda::ata(&uva, &c.quote_mint, &c.quote_token_program),
            false,
        ));
        if !is_buy {
            a.push(AccountMeta::new(uva, false));
        }
    }
    if c.coin_creator != Pubkey::default() {
        a.push(AccountMeta::new_readonly(
            pda::amm_pool_v2(&c.base_mint),
            false,
        ));
    }
    a.push(AccountMeta::new_readonly(c.buyback_fee_recipient, false));
    a.push(AccountMeta::new(
        pda::ata(
            &c.buyback_fee_recipient,
            &c.quote_mint,
            &c.quote_token_program,
        ),
        false,
    ));
    a
}

/// Full instruction list for a buy: base ATA, WSOL wrap, `buy`, WSOL close.
pub fn buy_instructions(
    c: &AmmCoin,
    user: &Pubkey,
    base_amount_out: u64,
    max_quote_amount_in: u64,
) -> Vec<Instruction> {
    let mut data = ix_disc("buy").to_vec();
    data.extend_from_slice(&base_amount_out.to_le_bytes());
    data.extend_from_slice(&max_quote_amount_in.to_le_bytes());
    data.push(1); // OptionBool(track_volume = true), as the SDK passes { 0: true }
    let swap = Instruction {
        program_id: PUMP_AMM_PROGRAM,
        accounts: swap_accounts(c, user, true),
        data,
    };

    let mut out = vec![ixs::create_ata_idempotent(
        user,
        user,
        &c.base_mint,
        &c.base_token_program,
    )];
    wrap_around(&mut out, c, user, max_quote_amount_in, swap);
    out
}

/// Full instruction list for a sell: WSOL ATA, `sell`, WSOL close.
pub fn sell_instructions(
    c: &AmmCoin,
    user: &Pubkey,
    base_amount_in: u64,
    min_quote_amount_out: u64,
) -> Vec<Instruction> {
    let mut data = ix_disc("sell").to_vec();
    data.extend_from_slice(&base_amount_in.to_le_bytes());
    data.extend_from_slice(&min_quote_amount_out.to_le_bytes());
    let swap = Instruction {
        program_id: PUMP_AMM_PROGRAM,
        accounts: swap_accounts(c, user, false),
        data,
    };
    let mut out = Vec::new();
    wrap_around(&mut out, c, user, 0, swap);
    out
}

fn wrap_around(
    out: &mut Vec<Instruction>,
    c: &AmmCoin,
    user: &Pubkey,
    wrap_lamports: u64,
    swap: Instruction,
) {
    let wsol = c.quote_mint == WSOL_MINT;
    let wsol_ata = pda::ata(user, &WSOL_MINT, &TOKEN_PROGRAM);
    if wsol {
        out.push(ixs::create_ata_idempotent(
            user,
            user,
            &WSOL_MINT,
            &TOKEN_PROGRAM,
        ));
        if wrap_lamports > 0 {
            out.push(ixs::system_transfer(user, &wsol_ata, wrap_lamports));
            out.push(ixs::sync_native(&wsol_ata));
        }
    } else {
        out.push(ixs::create_ata_idempotent(
            user,
            user,
            &c.quote_mint,
            &c.quote_token_program,
        ));
    }
    out.push(swap);
    if wsol {
        out.push(ixs::close_token_account(
            &wsol_ata,
            user,
            user,
            &TOKEN_PROGRAM,
        ));
    }
}
