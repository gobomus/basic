//! Detection round trips: a leader trade is built with our own (IDL-verified)
//! builders, wrapped in a transaction exactly as Geyser delivers it, and the
//! detector must recover the swap and an identical execution template.

use chain::borsh::EVENT_IX_TAG;
use chain::consts::*;
use chain::detect::{self, Template};
use chain::model::{ChainTx, Ix, TokenBal};
use chain::pda;
use chain::pump::{self, CurveCoin};
use chain::pump_amm::{self, AmmCoin};
use engine_core::types::{FeedSource, Side, Venue};
use solana_sdk::pubkey::Pubkey;

fn to_ix(i: &solana_sdk::instruction::Instruction) -> Ix {
    Ix {
        program: i.program_id,
        accounts: i.accounts.iter().map(|m| m.pubkey).collect(),
        data: i.data.clone(),
    }
}

fn base_tx(leader: Pubkey) -> ChainTx {
    ChainTx {
        signature: "sig".into(),
        slot: 100,
        tx_index: Some(5),
        block_time_ms: None,
        observed_at_ns: 0,
        source: FeedSource::Geyser,
        failed: false,
        fee: 5000,
        keys: vec![leader],
        num_signers: 1,
        top: vec![],
        inner: vec![],
        pre_balances: vec![10_000_000_000],
        post_balances: vec![10_000_000_000],
        pre_tokens: vec![],
        post_tokens: vec![],
        logs: vec![],
        has_meta: true,
    }
}

fn trade_event_bytes(
    mint: Pubkey,
    user: Pubkey,
    creator: Pubkey,
    is_buy: bool,
    sol: u64,
    tokens: u64,
) -> Vec<u8> {
    let mut d = EVENT_IX_TAG.to_vec();
    d.extend(pump::event_disc("TradeEvent"));
    d.extend(mint.to_bytes());
    d.extend(sol.to_le_bytes());
    d.extend(tokens.to_le_bytes());
    d.push(is_buy as u8);
    d.extend(user.to_bytes());
    d.extend(1_700_000_000i64.to_le_bytes());
    for v in [
        31_000_000_000u64,
        1_040_000_000_000_000,
        1_000_000_000,
        760_000_000_000_000,
    ] {
        d.extend(v.to_le_bytes());
    }
    d.extend(PUMP_FEE_RECIPIENTS[0].to_bytes());
    d.extend(95u64.to_le_bytes());
    d.extend(0u64.to_le_bytes());
    d.extend(creator.to_bytes());
    d.extend(30u64.to_le_bytes());
    d.extend(0u64.to_le_bytes());
    d
}

#[test]
fn pump_curve_buy_is_detected_exactly() {
    let leader = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let creator = Pubkey::new_unique();
    let coin = CurveCoin::sol_paired(mint, creator, TOKEN_2022_PROGRAM, false);
    let ix = pump::buy_exact_quote_in_v2(&coin, &leader, 2_000_000_000, 1, 3);
    let mut tx = base_tx(leader);
    tx.top.push(to_ix(&ix));
    tx.inner.push((
        0,
        vec![Ix {
            program: PUMP_PROGRAM,
            accounts: vec![pda::pump_event_authority()],
            data: trade_event_bytes(
                mint,
                leader,
                creator,
                true,
                1_975_000_000,
                60_000_000_000_000,
            ),
        }],
    ));
    tx.post_tokens.push(TokenBal {
        account: pda::ata(&leader, &mint, &TOKEN_2022_PROGRAM),
        mint,
        owner: leader,
        program: TOKEN_2022_PROGRAM,
        amount: 60_000_000_000_000,
        decimals: 6,
    });

    let swaps = detect::swaps_by(&tx, &leader);
    assert_eq!(swaps.len(), 1);
    let s = &swaps[0];
    assert_eq!(
        (s.side, s.venue, s.mint, s.sol_amount, s.exact),
        (Side::Buy, Venue::PumpFunCurve, mint, 1_975_000_000, true)
    );
    assert_eq!(s.token_program, TOKEN_2022_PROGRAM);
    match &s.template {
        Template::Curve {
            coin: c, fee_bps, ..
        } => {
            assert_eq!(c, &coin);
            assert_eq!(*fee_bps, 125);
        }
        t => panic!("{t:?}"),
    }
    // same tx seen pre-execution
    let pre = detect::pre_exec_pump_buys(&tx, &leader);
    assert_eq!(pre[0].mint, mint);
    assert_eq!(pre[0].sol_amount, 2_000_000_000);
    // someone else's wallet: nothing
    assert!(detect::swaps_by(&tx, &Pubkey::new_unique()).is_empty());
}

#[test]
fn pump_curve_sell_reports_fraction() {
    let leader = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let creator = Pubkey::new_unique();
    let mut tx = base_tx(leader);
    tx.inner.push((0, vec![]));
    tx.top.push(Ix {
        program: PUMP_PROGRAM,
        accounts: vec![],
        data: vec![],
    });
    tx.inner[0].1.push(Ix {
        program: PUMP_PROGRAM,
        accounts: vec![],
        data: trade_event_bytes(mint, leader, creator, false, 500_000_000, 25_000_000_000),
    });
    tx.pre_tokens.push(TokenBal {
        account: Pubkey::new_unique(),
        mint,
        owner: leader,
        program: TOKEN_PROGRAM,
        amount: 100_000_000_000,
        decimals: 6,
    });
    let s = &detect::swaps_by(&tx, &leader)[0];
    assert_eq!(s.side, Side::Sell);
    assert!((s.fraction_sold.unwrap() - 0.25).abs() < 1e-9);
}

fn amm_event_bytes(pool: Pubkey, user: Pubkey, coin_creator: Pubkey, fee_rcpt: Pubkey) -> Vec<u8> {
    let mut d = EVENT_IX_TAG.to_vec();
    d.extend(pump_amm::event_disc("BuyEvent"));
    d.extend(1_700_000_000i64.to_le_bytes()); // timestamp
    for v in [
        5_000_000_000u64,
        0,
        0,
        0,
        900_000_000_000_000,
        120_000_000_000,
        1_000_000_000,
        20,
        2_000_000,
        93,
        9_300_000,
        1_002_000_000,
        1_012_000_000,
    ] {
        d.extend(v.to_le_bytes());
    }
    d.extend(pool.to_bytes());
    d.extend(user.to_bytes());
    d.extend([0u8; 64]);
    d.extend(fee_rcpt.to_bytes());
    d.extend([0u8; 32]);
    d.extend(coin_creator.to_bytes());
    d.extend(30u64.to_le_bytes());
    d.extend(300_000u64.to_le_bytes());
    d
}

#[test]
fn pumpswap_buy_rebuilds_identical_template() {
    let leader = Pubkey::new_unique();
    for cashback in [false, true] {
        let coin = AmmCoin {
            pool: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            quote_mint: WSOL_MINT,
            pool_base_token_account: Pubkey::new_unique(),
            pool_quote_token_account: Pubkey::new_unique(),
            base_token_program: TOKEN_2022_PROGRAM,
            quote_token_program: TOKEN_PROGRAM,
            coin_creator: Pubkey::new_unique(),
            is_mayhem_mode: false,
            is_cashback_coin: cashback,
            protocol_fee_recipient: Pubkey::new_unique(),
            buyback_fee_recipient: PUMP_BUYBACK_FEE_RECIPIENTS[3],
        };
        let mut tx = base_tx(leader);
        for i in pump_amm::buy_instructions(&coin, &leader, 5_000_000_000, 1_100_000_000) {
            tx.top.push(to_ix(&i));
        }
        let swap_idx = tx
            .top
            .iter()
            .position(|i| i.program == PUMP_AMM_PROGRAM)
            .unwrap();
        tx.inner.push((
            swap_idx,
            vec![Ix {
                program: PUMP_AMM_PROGRAM,
                accounts: vec![],
                data: amm_event_bytes(
                    coin.pool,
                    leader,
                    coin.coin_creator,
                    coin.protocol_fee_recipient,
                ),
            }],
        ));
        let s = &detect::swaps_by(&tx, &leader)[0];
        assert_eq!(
            (s.venue, s.side, s.mint, s.sol_amount),
            // a buy's flow is `quote_amount_in`: everything the trader paid
            (Venue::PumpSwap, Side::Buy, coin.base_mint, 1_000_000_000)
        );
        match &s.template {
            Template::Amm {
                coin: c,
                base_reserve,
                fee_bps,
                ..
            } => {
                assert_eq!(c, &coin, "cashback={cashback}");
                // the event carries pre-trade reserves; the template is post-trade
                assert_eq!(*base_reserve, 900_000_000_000_000 - 5_000_000_000);
                assert_eq!(*fee_bps, 20 + 93 + 30);
            }
            t => panic!("{t:?}"),
        }
    }
}

#[test]
fn generic_venue_detected_from_balances() {
    let leader = Pubkey::new_unique();
    let mint = Pubkey::new_unique();
    let mut tx = base_tx(leader);
    tx.post_balances = vec![10_000_000_000 - 750_000_000 - 5000];
    tx.post_tokens.push(TokenBal {
        account: Pubkey::new_unique(),
        mint,
        owner: leader,
        program: TOKEN_PROGRAM,
        amount: 3_000_000_000,
        decimals: 6,
    });
    let s = &detect::swaps_by(&tx, &leader)[0];
    assert_eq!(
        (s.side, s.sol_amount, s.token_amount, s.exact),
        (Side::Buy, 750_000_000, 3_000_000_000, false)
    );
    assert!((s.price_sol - 0.00025).abs() < 1e-12); // 0.75 SOL for 3,000 tokens
    assert_eq!(s.template, Template::Generic);

    // failed transactions are ignored
    tx.failed = true;
    assert!(detect::swaps_by(&tx, &leader).is_empty());
}

#[test]
fn meteora_dbc_buy_detected_with_template() {
    use chain::meteora_dbc::{self as dbc, DbcCoin};
    let leader = Pubkey::new_unique();
    let coin = DbcCoin {
        pool: Pubkey::new_unique(),
        config: Pubkey::new_unique(),
        base_mint: Pubkey::new_unique(),
        quote_mint: WSOL_MINT,
        base_vault: Pubkey::new_unique(),
        quote_vault: Pubkey::new_unique(),
        base_token_program: TOKEN_PROGRAM,
        quote_token_program: TOKEN_PROGRAM,
        needs_ix_sysvar: true,
    };
    let mut tx = base_tx(leader);
    for i in dbc::buy_instructions(&coin, &leader, 500_000_000, 1) {
        tx.top.push(to_ix(&i));
    }
    let idx = tx
        .top
        .iter()
        .position(|i| i.program == dbc::DBC_PROGRAM)
        .unwrap();
    // EvtSwap2 with next_sqrt_price for P = 3e-5 lamports per raw unit
    let sqrt = (3e-5f64.sqrt() * 18_446_744_073_709_551_616.0) as u128;
    let mut ev = EVENT_IX_TAG.to_vec();
    ev.extend(dbc::event_disc("EvtSwap2"));
    ev.extend(coin.pool.to_bytes());
    ev.extend(coin.config.to_bytes());
    ev.push(1); // QuoteToBase
    ev.push(0);
    ev.extend(500_000_000u64.to_le_bytes());
    ev.extend(1u64.to_le_bytes());
    ev.push(0);
    for v in [500_000_000u64, 495_000_000, 0, 16_000_000_000_000] {
        ev.extend(v.to_le_bytes());
    }
    ev.extend(sqrt.to_le_bytes());
    for v in [5_000_000u64, 0, 0, 42_000_000_000, 85_000_000_000, 0] {
        ev.extend(v.to_le_bytes());
    }
    tx.inner.push((
        idx,
        vec![Ix {
            program: dbc::DBC_PROGRAM,
            accounts: vec![],
            data: ev,
        }],
    ));
    tx.post_balances = vec![10_000_000_000 - 500_000_000 - 5000];
    tx.post_tokens.push(TokenBal {
        account: Pubkey::new_unique(),
        mint: coin.base_mint,
        owner: leader,
        program: TOKEN_PROGRAM,
        amount: 16_000_000_000_000,
        decimals: 6,
    });

    let s = &detect::swaps_by(&tx, &leader)[0];
    assert_eq!(
        (s.venue, s.side, s.mint),
        (Venue::MeteoraDbc, Side::Buy, coin.base_mint)
    );
    assert_eq!(s.pool_sol, Some(42_000_000_000));
    assert!((s.price_sol - 3e-5 * 1e6 / 1e9).abs() < 1e-15);
    match &s.template {
        Template::Dbc {
            coin: c, fee_bps, ..
        } => {
            assert_eq!(c, &coin);
            assert_eq!(*fee_bps, 100); // 5_000_000 / 500_000_000
        }
        t => panic!("{t:?}"),
    }
}

#[test]
fn log_events_are_attributed_to_the_emitting_program() {
    use base64::Engine;
    let other = Pubkey::new_unique();
    let mut tx = base_tx(Pubkey::new_unique());
    let payload = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
    tx.logs = vec![
        format!("Program {} invoke [1]", other),
        format!("Program data: {payload}"),
        format!("Program {PUMP_PROGRAM} invoke [2]"),
        "Program data: BAUG".into(),
        format!("Program {PUMP_PROGRAM} success"),
        format!("Program data: {payload}"),
        format!("Program {} success", other),
    ];
    assert_eq!(
        detect::program_data_logs(&tx, &PUMP_PROGRAM),
        vec![vec![4u8, 5, 6]]
    );
    assert_eq!(detect::program_data_logs(&tx, &other).len(), 2);
}

#[test]
fn raydium_launchlab_buy_detected_with_template() {
    use chain::raydium_launchlab as ll;
    let leader = Pubkey::new_unique();
    let platform = Pubkey::new_unique();
    let coin = ll::LaunchCoin {
        pool: Pubkey::new_unique(),
        global_config: Pubkey::new_unique(),
        platform_config: platform,
        base_mint: Pubkey::new_unique(),
        quote_mint: WSOL_MINT,
        base_vault: Pubkey::new_unique(),
        quote_vault: Pubkey::new_unique(),
        base_token_program: TOKEN_PROGRAM,
        quote_token_program: TOKEN_PROGRAM,
        platform_fee_vault: ll::platform_fee_vault(&platform, &WSOL_MINT),
        creator_fee_vault: ll::creator_fee_vault(&Pubkey::new_unique(), &WSOL_MINT),
    };
    let mut tx = base_tx(leader);
    for i in ll::buy_instructions(&coin, &leader, 1_000_000_000, 1) {
        tx.top.push(to_ix(&i));
    }
    let idx = tx
        .top
        .iter()
        .position(|i| i.program == ll::LAUNCHLAB_PROGRAM)
        .unwrap();
    let mut ev = EVENT_IX_TAG.to_vec();
    ev.extend(ll::event_disc("TradeEvent"));
    ev.extend(coin.pool.to_bytes());
    // total_base_sell, virtual_base, virtual_quote, real_base_before, real_quote_before, real_base_after, real_quote_after
    for v in [
        793_100_000_000_000u64,
        1_073_025_605_596_382,
        30_000_852_951,
        0,
        0,
        33_000_000_000_000,
        990_000_000,
    ] {
        ev.extend(v.to_le_bytes());
    }
    for v in [
        1_000_000_000u64,
        33_000_000_000_000,
        2_500_000,
        5_000_000,
        0,
        0,
    ] {
        ev.extend(v.to_le_bytes());
    }
    ev.extend([0u8, 0u8, 1u8]); // Buy, Fund, exact_in
    tx.inner.push((
        idx,
        vec![Ix {
            program: ll::LAUNCHLAB_PROGRAM,
            accounts: vec![],
            data: ev,
        }],
    ));
    tx.post_balances = vec![10_000_000_000 - 1_000_000_000 - 5000];
    tx.post_tokens.push(TokenBal {
        account: Pubkey::new_unique(),
        mint: coin.base_mint,
        owner: leader,
        program: TOKEN_PROGRAM,
        amount: 33_000_000_000_000,
        decimals: 6,
    });

    let s = &detect::swaps_by(&tx, &leader)[0];
    assert_eq!(
        (s.venue, s.side, s.mint),
        (Venue::RaydiumLaunchLab, Side::Buy, coin.base_mint)
    );
    assert_eq!(s.pool_sol, Some(990_000_000));
    let expect_raw =
        (30_000_852_951f64 + 990_000_000.0) / (1_073_025_605_596_382f64 - 33_000_000_000_000.0);
    match &s.template {
        Template::LaunchLab {
            coin: c,
            price_raw,
            fee_bps,
        } => {
            assert_eq!(c, &coin);
            assert!((price_raw - expect_raw).abs() / expect_raw < 1e-12);
            assert_eq!(*fee_bps, 75); // 7.5M fees on 1 SOL in
        }
        t => panic!("{t:?}"),
    }
}
