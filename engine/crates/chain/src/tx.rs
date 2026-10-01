//! Transaction assembly and signing (local keys, microseconds).

use base64::Engine;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::VersionedTransaction;

use crate::ixs;

/// Fee settings attached to every transaction we send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeePlan {
    pub cu_limit: u32,
    pub cu_price_micro_lamports: u64,
    /// Tip transfer appended as the last instruction (0 = none).
    pub tip_lamports: u64,
}

pub struct SignedTx {
    pub tx: VersionedTransaction,
    pub signature: Signature,
    pub wire_b64: String,
}

pub fn build(
    payer: &Keypair,
    body: Vec<Instruction>,
    fees: FeePlan,
    tip_account: Option<&Pubkey>,
    blockhash: Hash,
) -> anyhow::Result<SignedTx> {
    let mut ixs = Vec::with_capacity(body.len() + 3);
    ixs.push(ixs::set_compute_unit_limit(fees.cu_limit));
    if fees.cu_price_micro_lamports > 0 {
        ixs.push(ixs::set_compute_unit_price(fees.cu_price_micro_lamports));
    }
    ixs.extend(body);
    if fees.tip_lamports > 0 {
        let to =
            tip_account.ok_or_else(|| anyhow::anyhow!("tip requested but no tip account known"))?;
        ixs.push(ixs::system_transfer(&payer.pubkey(), to, fees.tip_lamports));
    }
    let msg = v0::Message::try_compile(&payer.pubkey(), &ixs, &[], blockhash)?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer])?;
    let bytes = bincode::serialize(&tx)?;
    anyhow::ensure!(
        bytes.len() <= 1232,
        "transaction too large: {} bytes",
        bytes.len()
    );
    Ok(SignedTx {
        signature: tx.signatures[0],
        wire_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        tx,
    })
}

/// Same as [`build`] but on a durable nonce: `AdvanceNonceAccount` first and
/// the nonce value as the recent blockhash. Variants sharing a nonce are
/// mutually exclusive on-chain.
pub fn build_with_nonce(
    payer: &Keypair,
    body: Vec<Instruction>,
    fees: FeePlan,
    tip_account: Option<&Pubkey>,
    nonce_account: &Pubkey,
    nonce_value: Hash,
) -> anyhow::Result<SignedTx> {
    let mut with_nonce = vec![crate::nonce::advance_nonce(nonce_account, &payer.pubkey())];
    with_nonce.extend(body);
    // `build` prepends compute-budget ixs; the nonce advance must stay first.
    let mut ixs = Vec::with_capacity(with_nonce.len() + 3);
    ixs.push(with_nonce.remove(0));
    ixs.push(ixs::set_compute_unit_limit(fees.cu_limit));
    if fees.cu_price_micro_lamports > 0 {
        ixs.push(ixs::set_compute_unit_price(fees.cu_price_micro_lamports));
    }
    ixs.extend(with_nonce);
    if fees.tip_lamports > 0 {
        let to =
            tip_account.ok_or_else(|| anyhow::anyhow!("tip requested but no tip account known"))?;
        ixs.push(ixs::system_transfer(&payer.pubkey(), to, fees.tip_lamports));
    }
    sign(&[payer], ixs, nonce_value)
}

/// Sign with several keypairs (first = fee payer). Used for account creation.
pub fn build_multi(
    signers: &[&Keypair],
    body: Vec<Instruction>,
    cu_limit: u32,
    cu_price: u64,
    blockhash: Hash,
) -> anyhow::Result<SignedTx> {
    let mut ixs = vec![ixs::set_compute_unit_limit(cu_limit)];
    if cu_price > 0 {
        ixs.push(ixs::set_compute_unit_price(cu_price));
    }
    ixs.extend(body);
    sign(signers, ixs, blockhash)
}

fn sign(signers: &[&Keypair], ixs: Vec<Instruction>, blockhash: Hash) -> anyhow::Result<SignedTx> {
    let msg = v0::Message::try_compile(&signers[0].pubkey(), &ixs, &[], blockhash)?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), signers)?;
    let bytes = bincode::serialize(&tx)?;
    anyhow::ensure!(
        bytes.len() <= 1232,
        "transaction too large: {} bytes",
        bytes.len()
    );
    Ok(SignedTx {
        signature: tx.signatures[0],
        wire_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        tx,
    })
}

/// Unsigned-equivalent message for `simulateTransaction` with `sigVerify=false`
/// on behalf of any address (used by the `simulate` command — no keys needed).
pub fn build_unsigned_b64(
    payer: &Pubkey,
    body: Vec<Instruction>,
    cu_limit: u32,
    blockhash: Hash,
) -> anyhow::Result<String> {
    let mut ixs = vec![ixs::set_compute_unit_limit(cu_limit)];
    ixs.extend(body);
    let msg = v0::Message::try_compile(payer, &ixs, &[], blockhash)?;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); msg.header.num_required_signatures as usize],
        message: VersionedMessage::V0(msg),
    };
    Ok(base64::engine::general_purpose::STANDARD.encode(bincode::serialize(&tx)?))
}
