//! Program-derived addresses, exactly as derived by the official Pump SDKs.

use solana_sdk::pubkey::Pubkey;

use crate::consts::*;

fn pda(seeds: &[&[u8]], program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

pub fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    pda(&[owner.as_ref(), token_program.as_ref(), mint.as_ref()], &ATA_PROGRAM)
}

// ---- pump (bonding curve)
pub fn pump_global() -> Pubkey {
    pda(&[b"global"], &PUMP_PROGRAM)
}
pub fn pump_event_authority() -> Pubkey {
    pda(&[b"__event_authority"], &PUMP_PROGRAM)
}
pub fn bonding_curve(mint: &Pubkey) -> Pubkey {
    pda(&[b"bonding-curve", mint.as_ref()], &PUMP_PROGRAM)
}
pub fn bonding_curve_v2(mint: &Pubkey) -> Pubkey {
    pda(&[b"bonding-curve-v2", mint.as_ref()], &PUMP_PROGRAM)
}
pub fn creator_vault(creator: &Pubkey) -> Pubkey {
    pda(&[b"creator-vault", creator.as_ref()], &PUMP_PROGRAM)
}
pub fn pump_global_volume_accumulator() -> Pubkey {
    pda(&[b"global_volume_accumulator"], &PUMP_PROGRAM)
}
pub fn pump_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    pda(&[b"user_volume_accumulator", user.as_ref()], &PUMP_PROGRAM)
}
pub fn pump_pool_authority(mint: &Pubkey) -> Pubkey {
    pda(&[b"pool-authority", mint.as_ref()], &PUMP_PROGRAM)
}

// ---- pump fees program
pub fn sharing_config(mint: &Pubkey) -> Pubkey {
    pda(&[b"sharing-config", mint.as_ref()], &PUMP_FEE_PROGRAM)
}
pub fn fee_config_for(program: &Pubkey) -> Pubkey {
    pda(&[b"fee_config", program.as_ref()], &PUMP_FEE_PROGRAM)
}

// ---- pump amm (PumpSwap)
pub fn amm_global_config() -> Pubkey {
    pda(&[b"global_config"], &PUMP_AMM_PROGRAM)
}
pub fn amm_event_authority() -> Pubkey {
    pda(&[b"__event_authority"], &PUMP_AMM_PROGRAM)
}
pub fn amm_global_volume_accumulator() -> Pubkey {
    pda(&[b"global_volume_accumulator"], &PUMP_AMM_PROGRAM)
}
pub fn amm_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    pda(&[b"user_volume_accumulator", user.as_ref()], &PUMP_AMM_PROGRAM)
}
pub fn amm_coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    pda(&[b"creator_vault", coin_creator.as_ref()], &PUMP_AMM_PROGRAM)
}
pub fn amm_pool_v2(base_mint: &Pubkey) -> Pubkey {
    pda(&[b"pool-v2", base_mint.as_ref()], &PUMP_AMM_PROGRAM)
}
pub fn amm_pool(index: u16, owner: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey) -> Pubkey {
    pda(
        &[b"pool", &index.to_le_bytes(), owner.as_ref(), base_mint.as_ref(), quote_mint.as_ref()],
        &PUMP_AMM_PROGRAM,
    )
}
/// Canonical PumpSwap pool a graduated Pump coin migrates into.
pub fn canonical_pump_pool(mint: &Pubkey) -> Pubkey {
    amm_pool(0, &pump_pool_authority(mint), mint, &WSOL_MINT)
}
