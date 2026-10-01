//! Our hand-written nonce instructions/parsing vs Solana's official crates.

use chain::nonce;
use solana_sdk::pubkey::Pubkey;

fn same(ours: &solana_sdk::instruction::Instruction, theirs: &solana_instruction_3::Instruction) {
    assert_eq!(ours.program_id.to_bytes(), theirs.program_id.to_bytes());
    assert_eq!(ours.data, theirs.data);
    let a: Vec<([u8; 32], bool, bool)> = ours
        .accounts
        .iter()
        .map(|m| (m.pubkey.to_bytes(), m.is_signer, m.is_writable))
        .collect();
    let b: Vec<([u8; 32], bool, bool)> = theirs
        .accounts
        .iter()
        .map(|m| (m.pubkey.to_bytes(), m.is_signer, m.is_writable))
        .collect();
    assert_eq!(a, b);
}

fn addr(p: &Pubkey) -> solana_address::Address {
    solana_address::Address::new_from_array(p.to_bytes())
}

#[test]
fn instructions_match_solana_system_interface() {
    use solana_system_interface::instruction as si;
    let (payer, n, auth, to) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let ours = nonce::create_nonce_account(&payer, &n, &auth, 1_500_000);
    let theirs = si::create_nonce_account(&addr(&payer), &addr(&n), &addr(&auth), 1_500_000);
    assert_eq!(ours.len(), theirs.len());
    for (o, t) in ours.iter().zip(theirs.iter()) {
        same(o, t);
    }
    same(
        &nonce::advance_nonce(&n, &auth),
        &si::advance_nonce_account(&addr(&n), &addr(&auth)),
    );
    same(
        &nonce::withdraw_nonce(&n, &auth, &to, 42),
        &si::withdraw_nonce_account(&addr(&n), &addr(&auth), &addr(&to), 42),
    );
}

#[test]
fn account_parsing_matches_solana_nonce() {
    use solana_nonce::state::{Data, DurableNonce, State};
    use solana_nonce::versions::Versions;
    let auth = Pubkey::new_unique();
    let blockhash = solana_sdk::hash::Hash::new_from_array([9; 32]);
    let dn =
        DurableNonce::from_blockhash(&solana_hash_4::Hash::new_from_array(blockhash.to_bytes()));
    let data = Data::new(
        solana_pubkey_4::Pubkey::new_from_array(auth.to_bytes()),
        dn,
        5000,
    );
    let v = Versions::new(State::Initialized(data.clone()));
    let bytes = bincode::serialize(&v).unwrap();
    assert_eq!(bytes.len() as u64, nonce::NONCE_ACCOUNT_SIZE);
    let (a, h) = nonce::parse_nonce_account(&bytes).unwrap();
    assert_eq!(a, auth);
    assert_eq!(h.to_bytes(), dn.as_hash().to_bytes());
    let uninit = bincode::serialize(&Versions::new(State::Uninitialized)).unwrap();
    assert!(nonce::parse_nonce_account(&uninit).is_none());
}

#[test]
fn variants_share_the_nonce_and_differ_only_in_tip() {
    use chain::tx::{build_with_nonce, FeePlan};
    use solana_sdk::signature::{Keypair, Signer};
    let kp = Keypair::new();
    let nonce_acc = Pubkey::new_unique();
    let nonce_val = solana_sdk::hash::Hash::new_from_array([3; 32]);
    let body = vec![chain::ixs::system_transfer(
        &kp.pubkey(),
        &Pubkey::new_unique(),
        1,
    )];
    let fp = FeePlan {
        cu_limit: 100_000,
        cu_price_micro_lamports: 1000,
        tip_lamports: 1_000_000,
    };
    let (tip_a, tip_b) = (Pubkey::new_unique(), Pubkey::new_unique());
    let a = build_with_nonce(&kp, body.clone(), fp, Some(&tip_a), &nonce_acc, nonce_val).unwrap();
    let b = build_with_nonce(&kp, body, fp, Some(&tip_b), &nonce_acc, nonce_val).unwrap();
    assert_ne!(a.signature, b.signature);
    for t in [&a.tx, &b.tx] {
        assert_eq!(*t.message.recent_blockhash(), nonce_val);
        let m = &t.message;
        let keys = m.static_account_keys();
        let first = &m.instructions()[0];
        assert_eq!(
            keys[first.program_id_index as usize],
            chain::consts::SYSTEM_PROGRAM
        );
        assert_eq!(
            first.data,
            4u32.to_le_bytes().to_vec(),
            "AdvanceNonceAccount must be first"
        );
        assert_eq!(keys[first.accounts[0] as usize], nonce_acc);
    }
    let has = |t: &solana_sdk::transaction::VersionedTransaction, p: &Pubkey| {
        t.message.static_account_keys().contains(p)
    };
    assert!(has(&a.tx, &tip_a) && !has(&a.tx, &tip_b));
    assert!(has(&b.tx, &tip_b) && !has(&b.tx, &tip_a));
}
