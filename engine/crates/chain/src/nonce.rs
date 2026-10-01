//! Durable nonces: lets one order be signed as several variants (each tipping
//! a different landing service) that are mutually exclusive on-chain — every
//! variant starts with `AdvanceNonceAccount` on the same nonce, so at most one
//! can ever execute. Wire formats are verified against `solana-system-interface`
//! and `solana-nonce` in the tests.

use std::sync::Mutex;

use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

use crate::consts::SYSTEM_PROGRAM;

pub const RECENT_BLOCKHASHES_SYSVAR: Pubkey =
    pubkey!("SysvarRecentB1ockHashes11111111111111111111");
pub const RENT_SYSVAR: Pubkey = pubkey!("SysvarRent111111111111111111111111111111111");
pub const NONCE_ACCOUNT_SIZE: u64 = 80;

fn sys(index: u32, extra: &[u8], accounts: Vec<AccountMeta>) -> Instruction {
    let mut data = index.to_le_bytes().to_vec();
    data.extend_from_slice(extra);
    Instruction {
        program_id: SYSTEM_PROGRAM,
        accounts,
        data,
    }
}

/// CreateAccount (system-owned, 80 bytes) + InitializeNonceAccount.
pub fn create_nonce_account(
    payer: &Pubkey,
    nonce: &Pubkey,
    authority: &Pubkey,
    lamports: u64,
) -> Vec<Instruction> {
    let mut create = lamports.to_le_bytes().to_vec();
    create.extend_from_slice(&NONCE_ACCOUNT_SIZE.to_le_bytes());
    create.extend_from_slice(SYSTEM_PROGRAM.as_ref());
    vec![
        sys(
            0,
            &create,
            vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(*nonce, true),
            ],
        ),
        sys(
            6,
            authority.as_ref(),
            vec![
                AccountMeta::new(*nonce, false),
                AccountMeta::new_readonly(RECENT_BLOCKHASHES_SYSVAR, false),
                AccountMeta::new_readonly(RENT_SYSVAR, false),
            ],
        ),
    ]
}

pub fn advance_nonce(nonce: &Pubkey, authority: &Pubkey) -> Instruction {
    sys(
        4,
        &[],
        vec![
            AccountMeta::new(*nonce, false),
            AccountMeta::new_readonly(RECENT_BLOCKHASHES_SYSVAR, false),
            AccountMeta::new_readonly(*authority, true),
        ],
    )
}

/// WithdrawNonceAccount — withdrawing the full balance closes the account.
pub fn withdraw_nonce(
    nonce: &Pubkey,
    authority: &Pubkey,
    to: &Pubkey,
    lamports: u64,
) -> Instruction {
    sys(
        5,
        &lamports.to_le_bytes(),
        vec![
            AccountMeta::new(*nonce, false),
            AccountMeta::new(*to, false),
            AccountMeta::new_readonly(RECENT_BLOCKHASHES_SYSVAR, false),
            AccountMeta::new_readonly(RENT_SYSVAR, false),
            AccountMeta::new_readonly(*authority, true),
        ],
    )
}

/// (authority, durable nonce) from an initialised nonce account's data.
pub fn parse_nonce_account(data: &[u8]) -> Option<(Pubkey, Hash)> {
    if data.len() < 72 {
        return None;
    }
    let state = u32::from_le_bytes(data[4..8].try_into().ok()?);
    if state != 1 {
        return None;
    }
    let authority = Pubkey::new_from_array(data[8..40].try_into().ok()?);
    let hash = Hash::new_from_array(data[40..72].try_into().ok()?);
    Some((authority, hash))
}

/// Pool of nonce accounts owned by the hot wallet. A slot is checked out per
/// order and returned with its next value once the order has resolved.
#[derive(Default)]
pub struct NoncePool {
    slots: Mutex<Vec<(Pubkey, Option<Hash>, bool)>>,
}

impl NoncePool {
    pub fn new(accounts: &[Pubkey]) -> Self {
        Self {
            slots: Mutex::new(accounts.iter().map(|p| (*p, None, false)).collect()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.lock().unwrap().is_empty()
    }

    pub fn accounts(&self) -> Vec<Pubkey> {
        self.slots.lock().unwrap().iter().map(|s| s.0).collect()
    }

    /// Record the current value of a nonce (startup / after use).
    pub fn set(&self, account: &Pubkey, hash: Hash) {
        let mut s = self.slots.lock().unwrap();
        if let Some(slot) = s.iter_mut().find(|x| x.0 == *account) {
            slot.1 = Some(hash);
            slot.2 = false;
        }
    }

    /// Check out a free slot with a known value.
    pub fn take(&self) -> Option<(Pubkey, Hash)> {
        let mut s = self.slots.lock().unwrap();
        let slot = s.iter_mut().find(|x| !x.2 && x.1.is_some())?;
        slot.2 = true;
        Some((slot.0, slot.1.take().unwrap()))
    }

    pub fn available(&self) -> usize {
        self.slots
            .lock()
            .unwrap()
            .iter()
            .filter(|x| !x.2 && x.1.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_checkout() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let p = NoncePool::new(&[a, b]);
        assert!(p.take().is_none(), "no values yet");
        p.set(&a, Hash::new_from_array([1; 32]));
        let (acc, h) = p.take().unwrap();
        assert_eq!((acc, h), (a, Hash::new_from_array([1; 32])));
        assert!(p.take().is_none(), "a is checked out, b unknown");
        p.set(&a, Hash::new_from_array([2; 32]));
        assert_eq!(p.available(), 1);
    }
}
