//! Regression tests on REAL mainnet transactions (fetched 2026-10-02 via
//! getTransaction, maxSupportedTransactionVersion = 1). They pin the decoders
//! to what the live programs actually emit, independent of our own encoders.

use chain::detect::{self, Template};
use chain::model::ChainTx;
use engine_core::types::{Side, Venue};

fn load(name: &str) -> ChainTx {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    ChainTx::from_rpc_json(&v).unwrap()
}

fn kind(t: &Template) -> &'static str {
    match t {
        Template::Curve { .. } => "curve",
        Template::Amm { .. } => "pumpswap",
        Template::Dbc { .. } => "dbc",
        Template::LaunchLab { .. } => "launchlab",
        Template::Generic => "generic",
    }
}

/// Every exact (event-decoded) swap must agree with the trader's real SOL
/// balance change (which also includes fees, tips and ATA rent).
fn assert_exact_matches_balances(tx: &ChainTx) -> usize {
    let mut n = 0;
    for s in detect::all_swaps(tx).iter().filter(|s| s.exact) {
        let b = detect::balance_swaps(tx, &s.wallet)
            .into_iter()
            .find(|b| b.mint == s.mint)
            .expect("balance delta for exact swap");
        assert_eq!(b.side, s.side, "{}: side", tx.signature);
        let diff = b.sol_amount.abs_diff(s.sol_amount);
        assert!(
            diff < 10_000_000 || (diff as f64) < 0.15 * s.sol_amount as f64,
            "{}: event {} vs balance {}",
            tx.signature,
            s.sol_amount,
            b.sol_amount
        );
        let tb = b.token_amount.abs_diff(s.token_amount);
        assert!(
            tb == 0 || (tb as f64) < 0.01 * s.token_amount as f64,
            "{}: tokens event {} vs balance {}",
            tx.signature,
            s.token_amount,
            b.token_amount
        );
        n += 1;
    }
    n
}

#[test]
fn real_pump_curve_trades() {
    for f in ["pump_curve_0.json", "pump_curve_1.json"] {
        let tx = load(f);
        let swaps = detect::all_swaps(&tx);
        assert!(!swaps.is_empty(), "{f}");
        for s in &swaps {
            assert_eq!(s.venue, Venue::PumpFunCurve, "{f}");
            assert_eq!(kind(&s.template), "curve", "{f}");
            assert!(s.price_sol > 0.0 && s.price_sol < 1.0);
            assert!(s.pool_sol.is_some());
        }
        assert!(assert_exact_matches_balances(&tx) > 0, "{f}");
        assert!(
            tx.tx_index.is_some(),
            "getTransaction now returns transactionIndex"
        );
    }
}

#[test]
fn real_pumpswap_trades() {
    let tx = load("pumpswap_0.json");
    let swaps = detect::all_swaps(&tx);
    let direct: Vec<_> = swaps
        .iter()
        .filter(|s| kind(&s.template) == "pumpswap")
        .collect();
    assert!(
        !direct.is_empty(),
        "canonical token/WSOL pool should yield a direct template"
    );
    for s in direct {
        assert_eq!(s.venue, Venue::PumpSwap);
        assert!(s.migrated);
    }
    assert!(assert_exact_matches_balances(&tx) > 0);
}

#[test]
fn real_reversed_pumpswap_pool_falls_back_correctly() {
    // Non-canonical pool with base = WSOL, quote = the token: the program logs
    // "Sell" (of WSOL) but economically the trader BUYS the token. The direct
    // builder must decline it; the balance path must still read it as a buy.
    let tx = load("pumpswap_reversed_pool.json");
    let swaps = detect::all_swaps(&tx);
    assert_eq!(swaps.len(), 1);
    let s = &swaps[0];
    assert_eq!(s.venue, Venue::PumpSwap);
    assert_eq!(s.side, Side::Buy);
    assert_eq!(kind(&s.template), "generic");
    assert_eq!(
        s.mint.to_string(),
        "DPdmSZEJxibFRqsjmQPaBYTvLFx4ZDKSNTfZBiM5pump"
    );
    assert_eq!(s.sol_amount, 1_576_402);
    assert_eq!(s.token_amount, 3_810_306_867);
}

#[test]
fn real_v1_transaction_parses() {
    // version-1 transaction (compute budget in transactionConfig, no ALTs):
    // an aggregator routing PUMP against another token, with no SOL leg.
    let tx = load("v1_router.json");
    assert!(tx.has_meta);
    assert_eq!(tx.keys.len(), 63);
    assert!(tx.tx_index.is_some());
    assert!(
        detect::all_swaps(&tx).is_empty(),
        "token-to-token route has no SOL leg to copy"
    );
}

#[test]
fn real_meteora_dbc_trade() {
    let tx = load("meteora_dbc_0.json");
    let swaps = detect::all_swaps(&tx);
    assert!(!swaps.is_empty());
    let s = swaps
        .iter()
        .find(|s| s.venue == Venue::MeteoraDbc)
        .expect("DBC venue");
    assert_eq!(
        kind(&s.template),
        "dbc",
        "SOL-quoted DBC swap should yield a direct template"
    );
    assert!(s.price_sol > 0.0);
    assert!(matches!(s.side, Side::Buy | Side::Sell));
}

#[test]
fn real_launchlab_trade_is_recognised() {
    let tx = load("launchlab_0.json");
    let swaps = detect::all_swaps(&tx);
    assert!(!swaps.is_empty());
    assert!(swaps.iter().any(|s| s.venue == Venue::RaydiumLaunchLab));
}

#[test]
fn real_jupiter_v0_transaction_deserializes() {
    let raw = std::fs::read(format!(
        "{}/tests/fixtures/jupiter_v0_swap.bin",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let vtx: solana_sdk::transaction::VersionedTransaction = bincode::deserialize(&raw).unwrap();
    assert_eq!(
        vtx.message.static_account_keys()[0].to_string(),
        "7xhYhQm48yfuhGJmiZ9Xqm7KzwgWMUQiriERqMysz5Ja"
    );
    assert_eq!(
        bincode::serialize(&vtx).unwrap(),
        raw,
        "round-trips byte for byte"
    );
}
