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

// ---------------------------------------------------------------------------
// Quotes must reproduce what the live programs really did. These tests would
// have caught a fee model that silently overstated costs (the buyback share
// of the protocol fee had been added on top of it).

use chain::pump::{self, CurveState, PumpEvent};
use chain::pump_amm;

fn amm_events(tx: &ChainTx) -> Vec<pump_amm::SwapEventData> {
    tx.all_ixs()
        .filter(|ix| ix.program == chain::consts::PUMP_AMM_PROGRAM)
        .filter_map(|ix| pump_amm::decode_event_ix(&ix.data))
        .collect()
}

#[test]
fn curve_quotes_reproduce_real_trades() {
    for f in ["pump_curve_0.json", "pump_curve_1.json"] {
        let tx = load(f);
        let mut checked = 0;
        for ev in detect::pump_events(&tx) {
            let PumpEvent::Trade(e) = ev else { continue };
            let post = e.curve_state();
            let net = e.sol_amount; // lamports the curve's reserves moved by
            let fees = e.fee + e.creator_fee;
            assert_eq!(
                e.total_fee_bps(),
                125,
                "{f}: protocol 0.95% + creator 0.30%"
            );
            assert_eq!(fees * 10_000 / net, 125, "{f}: fees really are 1.25%");
            // the swap amount we report is what the trader really paid / received
            let swap = detect::all_swaps(&tx)
                .into_iter()
                .find(|s| s.wallet == e.user)
                .expect("swap");
            assert_eq!(
                swap.sol_amount,
                if e.is_buy { net + fees } else { net - fees },
                "{f}"
            );
            let i = tx.key_index(&e.user).unwrap();
            let delta = tx.post_balances[i] as i128 - tx.pre_balances[i] as i128;
            let real = if e.is_buy {
                -delta
            } else {
                delta + tx.fee as i128
            };
            // within rent / tips of the balance change (a new token account costs 0.002 SOL)
            assert!(
                (real - swap.sol_amount as i128).abs() < 12_000_000,
                "{f}: reported {} vs balance change {real}",
                swap.sol_amount
            );
            if e.is_buy {
                let pre = CurveState {
                    virtual_quote_reserves: post.virtual_quote_reserves - net,
                    virtual_token_reserves: post.virtual_token_reserves + e.token_amount,
                    real_quote_reserves: post.real_quote_reserves - net,
                    real_token_reserves: post.real_token_reserves + e.token_amount,
                };
                let got = pump::buy_tokens_for_quote(&pre, net + fees, e.total_fee_bps());
                assert!(
                    got.abs_diff(e.token_amount) <= e.token_amount / 1_000_000 + 2,
                    "{f}: buy quote {got} vs real {}",
                    e.token_amount
                );
            } else {
                let pre = CurveState {
                    virtual_quote_reserves: post.virtual_quote_reserves + net,
                    virtual_token_reserves: post.virtual_token_reserves - e.token_amount,
                    real_quote_reserves: post.real_quote_reserves + net,
                    real_token_reserves: post.real_token_reserves - e.token_amount,
                };
                let got = pump::sell_quote_for_tokens(&pre, e.token_amount, e.total_fee_bps());
                assert!(
                    got.abs_diff(net - fees) <= 2,
                    "{f}: sell quote {got} vs real {}",
                    net - fees
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "{f}");
    }
}

#[test]
fn pumpswap_quotes_and_reserves_reproduce_real_trades() {
    // a buy on a canonical pool, with buyback_fee_basis_points = 5000 in the event
    let tx = load("pumpswap_0.json");
    let e = amm_events(&tx)
        .into_iter()
        .find(|e| e.is_buy)
        .expect("a BuyEvent");
    assert_eq!(e.buyback_fee_basis_points, 5000);
    assert_eq!(
        e.total_fee_bps(),
        20 + 5 + 95,
        "lp + protocol + creator; buyback is inside protocol"
    );
    let got = pump_amm::buy_base_for_quote(
        e.pool_base_token_reserves,
        e.effective_quote_reserves(),
        e.quote_amount,
        e.total_fee_bps(),
    );
    assert!(
        got.abs_diff(e.base_amount) <= e.base_amount / 1_000_000 + 2,
        "buy quote {got} vs real {}",
        e.base_amount
    );
    // the user's real SOL outflow is quote_amount_in (every fee included)
    let i = tx.key_index(&tx.fee_payer().copied().unwrap()).unwrap();
    let outflow = tx.pre_balances[i] - tx.post_balances[i] - tx.fee;
    assert!(
        outflow.abs_diff(e.user_flow()) < 10_000,
        "outflow {outflow} vs event {}",
        e.user_flow()
    );
    // reserves after the swap equal the pool vaults after the transaction
    let (base_after, quote_after) = e.post_reserves();
    let vault = |mint: &chain::solana_sdk::pubkey::Pubkey| {
        tx.post_tokens
            .iter()
            .find(|b| b.owner == e.pool && b.mint == *mint)
            .map(|b| b.amount)
            .expect("pool vault")
    };
    let swaps = detect::all_swaps(&tx);
    let mint = swaps
        .iter()
        .find(|s| s.venue == Venue::PumpSwap)
        .unwrap()
        .mint;
    assert_eq!(base_after, vault(&mint));
    assert_eq!(
        quote_after as i128 - e.virtual_quote_reserves,
        vault(&chain::consts::WSOL_MINT) as i128
    );

    // a sell (non-canonical pool, protocol fee split 50/50 with buyback)
    let tx = load("pumpswap_reversed_pool.json");
    let e = amm_events(&tx)
        .into_iter()
        .find(|e| !e.is_buy)
        .expect("a SellEvent");
    let got = pump_amm::sell_quote_for_base(
        e.pool_base_token_reserves,
        e.effective_quote_reserves(),
        e.base_amount,
        e.total_fee_bps(),
    );
    assert!(
        got.abs_diff(e.user_quote_amount) <= 2,
        "sell quote {got} vs real {}",
        e.user_quote_amount
    );
}

/// Curves priced in another token (pump.fun's `buy_v2` with a non-SOL quote mint).
/// The trade event's amounts are in the quote token: reading them as SOL turned a
/// 1.54 SOL buy into a 188 "SOL" one in wallet audits. The SOL leg must come from the
/// buyer's real balance change, and the swap must not claim a SOL-paired curve
/// template we could execute. (Real transactions from 2026-10-08.)
#[test]
fn curve_priced_in_another_token_uses_the_real_sol_leg() {
    for (f, sol_paid) in [
        ("pump_curve_nonsol_buy.json", 1.541_495_72),
        ("pump_curve_nonsol_buy2.json", 3.020_088_039),
    ] {
        let tx = load(f);
        let buyer: chain::solana_sdk::pubkey::Pubkey =
            "AXgzGEvbgaFeb1xhdXkQc5D5CehZUa89ci2F9ugQPLaA"
                .parse()
                .unwrap();
        let swaps = detect::swaps_by(&tx, &buyer);
        // the first leg (buying the quote token with SOL) is routing, not a position
        assert_eq!(
            swaps.len(),
            1,
            "{f}: one buy, not the routing leg too: {:?}",
            swaps
                .iter()
                .map(|s| (s.venue, s.sol_amount))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            detect::all_swaps(&tx)
                .iter()
                .filter(|s| s.wallet == buyer)
                .count(),
            1,
            "{f}: the market view agrees"
        );
        for s in &swaps {
            assert_eq!(s.venue, Venue::PumpFunCurve, "{f}");
            assert_eq!(s.side, Side::Buy, "{f}");
            assert_eq!(kind(&s.template), "generic", "{f}: not a SOL-paired curve");
            assert!(!s.exact, "{f}: amounts come from balances, not the event");
            let paid = s.sol_amount as f64 / 1e9;
            // the balance leg excludes the network fee; ATA rent stays in (it is real cost)
            assert!(
                (paid - sol_paid).abs() < 0.01,
                "{f}: recorded {paid} SOL, the wallet paid {sol_paid}"
            );
        }
    }
}
