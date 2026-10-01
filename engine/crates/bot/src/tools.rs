//! Operator tools: preflight check, dry-run simulation, leader report, wallet ops.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chain::consts::*;
use chain::detect;
use chain::geyser::{self, FeedEvent, Filters};
use chain::model::ChainTx;
use chain::pda;
use chain::pump::{self, BondingCurve, CurveCoin};
use chain::pump_amm::{self, AmmCoin, GlobalConfig, Pool};
use chain::rpc::Rpc;
use chain::sender::{fetch_jito_tip_accounts, Fanout};
use chain::solana_sdk::pubkey::Pubkey;
use chain::solana_sdk::signature::{Keypair, Signer};
use chain::tx::{self, FeePlan};
use engine_core::types::{lamports_to_sol, sol_to_lamports, Side};
use engine_core::wallet_score::{score_wallet, RoundTrip, WalletScoreConfig};
use tokio::sync::{mpsc, watch};

use crate::cfg::BotConfig;
use crate::keystore;

fn line(ok: bool, what: &str, detail: impl std::fmt::Display) {
    println!(
        "  [{}] {what:<28} {detail}",
        if ok { " OK " } else { "FAIL" }
    );
}

// ------------------------------------------------------------------ check

pub async fn check(cfg: &BotConfig) -> anyhow::Result<bool> {
    let mut all_ok = true;
    println!("copybot preflight ({:?} mode)\n", cfg.engine.mode);
    let enabled: Vec<_> = cfg.engine.leaders.iter().filter(|l| l.enabled).collect();
    let bad: Vec<String> = enabled
        .iter()
        .filter(|l| l.address.parse::<Pubkey>().is_err())
        .map(|l| l.address.clone())
        .collect();
    line(
        !enabled.is_empty() && bad.is_empty(),
        "leaders",
        if bad.is_empty() {
            format!("{} enabled", enabled.len())
        } else {
            format!("invalid address(es): {}", bad.join(", "))
        },
    );
    all_ok &= !enabled.is_empty() && bad.is_empty();

    let rpc = match cfg.rpc_url() {
        Ok(u) => Rpc::new(u),
        Err(e) => {
            line(false, "RPC url", e);
            return Ok(false);
        }
    };
    let mut lat = vec![];
    for _ in 0..5 {
        let t = Instant::now();
        match rpc.get_slot("processed").await {
            Ok(_) => lat.push(t.elapsed().as_millis()),
            Err(e) => {
                line(false, "RPC getSlot", e);
                all_ok = false;
                break;
            }
        }
    }
    if !lat.is_empty() {
        lat.sort();
        line(
            true,
            "RPC round trip",
            format!("min {} ms · median {} ms", lat[0], lat[lat.len() / 2]),
        );
    }
    match rpc.latest_blockhash("processed").await {
        Ok((h, _)) => line(true, "blockhash", h),
        Err(e) => {
            line(false, "blockhash", e);
            all_ok = false;
        }
    }

    match keystore::read(&cfg.infra.wallet.keystore) {
        Ok(f) => {
            let pk: Pubkey = f.pubkey.parse()?;
            match rpc.balance(&pk).await {
                Ok(b) => {
                    let need = sol_to_lamports(
                        cfg.engine.risk.min_sol_reserve + cfg.engine.sizing.min_buy_sol,
                    );
                    line(
                        b >= need || cfg.engine.mode != engine_core::config::RunMode::Live,
                        "hot wallet",
                        format!("{} · {:.4} SOL", pk, lamports_to_sol(b)),
                    );
                }
                Err(e) => line(false, "hot wallet balance", e),
            }
        }
        Err(e) => {
            line(false, "keystore", e);
            all_ok = false;
        }
    }

    if cfg.infra.tip_accounts.is_empty() && cfg.infra.fees.tip_lamports_buy > 0 {
        match fetch_jito_tip_accounts(&cfg.infra.jito_block_engine).await {
            Ok(v) => line(true, "Jito tip accounts", format!("{} fetched", v.len())),
            Err(e) => {
                line(false, "Jito tip accounts", e);
                all_ok = false;
            }
        }
    }

    // Sender reachability: an empty/invalid sendTransaction must come back with a
    // JSON-RPC error, which proves the endpoint is reachable and measures RTT.
    let fan = Fanout::new(cfg.infra.senders.clone());
    for r in fan.send("AA==").await {
        // Reachable = the endpoint itself answered with JSON-RPC (a rejection of our dummy tx is expected).
        let reachable = r.ok || r.detail.starts_with('{');
        line(
            reachable,
            &format!("sender {}", r.sender),
            if reachable {
                format!("{} ms", r.ms)
            } else {
                format!("{} ms · {}", r.ms, r.detail)
            },
        );
        all_ok &= reachable;
    }

    // Geyser: connect, then measure how many leader/firehose txs arrive in 10 s.
    let (ftx, frx) = watch::channel(Filters {
        leaders: cfg
            .engine
            .leaders
            .iter()
            .map(|l| l.address.clone())
            .collect(),
        mints: vec![],
    });
    let (tx, mut rx) = mpsc::channel(10_000);
    let g = cfg.infra.geyser.clone();
    let h = tokio::spawn(geyser::run(g, frx, tx));
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut txs, mut slots, mut connected, mut swaps) = (0u64, 0u64, false, 0u64);
    let mut slot_seen_first: Option<Instant> = None;
    let mut geyser_err: Option<String> = None;
    while Instant::now() < deadline {
        match tokio::time::timeout(deadline - Instant::now(), rx.recv()).await {
            Ok(Some(FeedEvent::Status {
                connected: c,
                detail,
                ..
            })) => {
                if !c && !connected && geyser_err.is_none() {
                    geyser_err = Some(detail);
                }
                connected |= c;
            }
            Ok(Some(FeedEvent::Slot { .. })) => {
                slots += 1;
                slot_seen_first.get_or_insert_with(Instant::now);
            }
            Ok(Some(FeedEvent::Tx(t))) => {
                txs += 1;
                swaps += detect::all_swaps(&t).len() as u64;
            }
            _ => break,
        }
    }
    h.abort();
    drop(ftx);
    let ok = connected && slots > 0;
    if let (false, Some(e)) = (connected, &geyser_err) {
        line(false, "geyser connect", e);
    }
    line(
        ok,
        "geyser stream",
        format!("{slots} slot updates · {txs} txs · {swaps} decoded swaps in 10 s"),
    );
    all_ok &= ok;

    println!(
        "\n{}",
        if all_ok {
            "READY"
        } else {
            "NOT READY — fix the FAIL lines above"
        }
    );
    Ok(all_ok)
}

// ------------------------------------------------------------------ simulate

/// Build our real buy for `mint` on behalf of `as_wallet` (any funded address,
/// no keys needed) and run it through `simulateTransaction` against mainnet.
pub async fn simulate_buy(
    cfg: &BotConfig,
    mint: Pubkey,
    sol: f64,
    as_wallet: Pubkey,
) -> anyhow::Result<bool> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let lamports = sol_to_lamports(sol);
    let mint_acc = rpc
        .account(&mint)
        .await?
        .ok_or_else(|| anyhow::anyhow!("mint {mint} not found"))?;
    let tp = mint_acc.owner;
    let bc_key = pda::bonding_curve(&mint);
    let (body, route) = match rpc.account(&bc_key).await? {
        Some(a) if !BondingCurve::decode(&a.data)?.complete => {
            let bc = BondingCurve::decode(&a.data)?;
            let coin = CurveCoin::sol_paired(mint, bc.creator, tp, bc.is_mayhem_mode);
            let expected = pump::buy_tokens_for_quote(&bc.state, lamports, 200);
            let ixs = vec![
                chain::ixs::create_ata_idempotent(&as_wallet, &as_wallet, &mint, &tp),
                pump::buy_exact_quote_in_v2(
                    &coin,
                    &as_wallet,
                    lamports,
                    pump::apply_slippage_down(expected, 3000),
                    rand::random(),
                ),
            ];
            (ixs, format!("pump curve · expected {} tokens", expected))
        }
        _ => {
            let pool_key = pda::canonical_pump_pool(&mint);
            let pool = Pool::decode(
                &rpc.account(&pool_key)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no curve and no canonical PumpSwap pool"))?
                    .data,
            )?;
            let g = GlobalConfig::decode(
                &rpc.account(&pda::amm_global_config())
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no GlobalConfig"))?
                    .data,
            )?;
            let coin = AmmCoin::from_pool(pool_key, &pool, tp, TOKEN_PROGRAM, &g, rand::random());
            let accs = rpc
                .accounts(&[pool.pool_base_token_account, pool.pool_quote_token_account])
                .await?;
            let amt = |a: &Option<chain::rpc::AccountData>| {
                a.as_ref()
                    .and_then(|a| a.data.get(64..72))
                    .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                    .unwrap_or(0)
            };
            let qr = (amt(&accs[1]) as i128 + pool.virtual_quote_reserves).max(0) as u128;
            let fee = g.lp_fee_basis_points
                + g.protocol_fee_basis_points
                + g.coin_creator_fee_basis_points;
            let expected = pump_amm::buy_base_for_quote(amt(&accs[0]), qr, lamports, fee);
            (
                pump_amm::buy_instructions(
                    &coin,
                    &as_wallet,
                    pump::apply_slippage_down(expected, 3000),
                    lamports,
                ),
                format!("PumpSwap pool {pool_key} · expected {expected} tokens"),
            )
        }
    };
    let (bh, _) = rpc.latest_blockhash("processed").await?;
    let wire = tx::build_unsigned_b64(&as_wallet, body, cfg.infra.fees.cu_limit_buy, bh)?;
    let sim = rpc.simulate(&wire, false).await?;
    println!("route: {route}");
    for l in &sim.logs {
        println!("  {l}");
    }
    println!("compute units: {:?}", sim.units_consumed);
    match &sim.err {
        None => println!("\nSIMULATION OK — the live program accepted our buy instruction"),
        Some(e) => println!("\nSIMULATION FAILED: {e}"),
    }
    Ok(sim.err.is_none())
}

// ------------------------------------------------------------------ leader report

pub async fn leader_report(cfg: &BotConfig, wallet: Pubkey, limit: usize) -> anyhow::Result<()> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let mut sigs = vec![];
    let mut before: Option<String> = None;
    while sigs.len() < limit {
        let page = rpc
            .signatures_for_address(&wallet, 1000.min(limit - sigs.len()), before.as_deref())
            .await?;
        if page.is_empty() {
            break;
        }
        before = page
            .last()
            .and_then(|v| v["signature"].as_str().map(String::from));
        sigs.extend(
            page.into_iter()
                .filter(|v| v["err"].is_null())
                .filter_map(|v| v["signature"].as_str().map(String::from)),
        );
    }
    eprintln!("fetching {} transactions…", sigs.len());
    let mut swaps = vec![];
    for chunk in sigs.chunks(8) {
        let futs = chunk.iter().map(|s| rpc.transaction_json(s));
        for v in futures::future::join_all(futs).await.into_iter().flatten() {
            if v.is_null() {
                continue;
            }
            if let Ok(t) = ChainTx::from_rpc_json(&v) {
                for s in detect::swaps_by(&t, &wallet) {
                    swaps.push((t.block_time_ms.unwrap_or(0), s));
                }
            }
        }
    }
    swaps.sort_by_key(|(t, _)| *t);
    // FIFO round trips per mint
    let mut open: HashMap<Pubkey, (i64, f64, u64)> = HashMap::new(); // first entry ts, cost, tokens
    let mut trips = vec![];
    let mut venues: HashMap<String, u32> = HashMap::new();
    for (t, s) in &swaps {
        *venues.entry(format!("{:?}", s.venue)).or_default() += 1;
        match s.side {
            Side::Buy => {
                let e = open.entry(s.mint).or_insert((*t, 0.0, 0));
                e.1 += lamports_to_sol(s.sol_amount);
                e.2 += s.token_amount;
            }
            Side::Sell => {
                if let Some(e) = open.get_mut(&s.mint) {
                    if e.2 == 0 {
                        continue;
                    }
                    let frac = (s.token_amount as f64 / e.2 as f64).min(1.0);
                    let cost = e.1 * frac;
                    trips.push(RoundTrip {
                        entry_t_ms: e.0,
                        exit_t_ms: *t,
                        cost_sol: cost,
                        proceeds_sol: lamports_to_sol(s.sol_amount),
                        entry_slots_after_creation: None,
                        copy_ret: None,
                    });
                    e.1 -= cost;
                    e.2 = e.2.saturating_sub(s.token_amount);
                    if e.2 == 0 {
                        open.remove(&s.mint);
                    }
                }
            }
        }
    }
    let sc = WalletScoreConfig {
        min_trades: 30,
        min_median_hold_secs: 120.0,
        sniper_slot_window: 2,
        max_sniper_share: 0.3,
        max_active_hours_per_day: 16.0,
        min_copy_median_ret: 0.0,
        prior_trades: 20.0,
    };
    let r = score_wallet(&sc, &trips);
    let pnl: f64 = trips.iter().map(|t| t.proceeds_sol - t.cost_sol).sum();
    println!("wallet {wallet}");
    println!(
        "  swaps decoded        {}  (venues: {:?})",
        swaps.len(),
        venues
    );
    println!("  round trips          {}", r.n);
    println!("  realised PnL         {pnl:+.3} SOL");
    println!("  win rate             {:.1}%", r.win_rate * 100.0);
    println!("  median return        {:+.1}%", r.median_ret * 100.0);
    println!(
        "  profit factor        {:?}",
        r.profit_factor.map(|x| (x * 100.0).round() / 100.0)
    );
    println!("  median hold          {:.0} s", r.median_hold_secs);
    println!("  active hours/day     {:.1}", r.active_hours_per_day);
    println!("  open positions       {}", open.len());
    let mut flags = vec![];
    if r.n < sc.min_trades {
        flags.push("too few round trips for a verdict".to_string());
    }
    if r.median_hold_secs < sc.min_median_hold_secs {
        flags.push(format!(
            "median hold {:.0}s < {}s: likely uncopyable (you'd be exit liquidity)",
            r.median_hold_secs, sc.min_median_hold_secs
        ));
    }
    if r.active_hours_per_day > sc.max_active_hours_per_day {
        flags.push("active > 16 h/day: likely a bot".into());
    }
    if pnl <= 0.0 {
        flags.push("not profitable over this window".into());
    }
    println!(
        "  verdict              {}",
        if flags.is_empty() {
            "CANDIDATE → add with enabled=false, run shadow mode to measure copier returns"
                .to_string()
        } else {
            format!("REJECT/WATCH: {}", flags.join("; "))
        }
    );
    Ok(())
}

// ------------------------------------------------------------------ wallet ops

pub async fn wallet_balance(cfg: &BotConfig, pk: &Pubkey) -> anyhow::Result<()> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    println!("{pk}\n  SOL {:.6}", lamports_to_sol(rpc.balance(pk).await?));
    for prog in [TOKEN_PROGRAM, TOKEN_2022_PROGRAM] {
        for (acc, mint, amt) in rpc.token_accounts(pk, &prog).await? {
            println!(
                "  {mint}  amount {amt}  (account {acc}{})",
                if amt == 0 {
                    ", empty → reclaimable rent"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}

async fn send_simple(
    cfg: &BotConfig,
    kp: &Keypair,
    ixs: Vec<chain::solana_sdk::instruction::Instruction>,
) -> anyhow::Result<String> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let (bh, _) = rpc.latest_blockhash("confirmed").await?;
    let signed = tx::build(
        kp,
        ixs,
        FeePlan {
            cu_limit: 200_000,
            cu_price_micro_lamports: cfg.infra.fees.cu_price_micro_lamports,
            tip_lamports: 0,
        },
        None,
        bh,
    )?;
    let sig = rpc.send(&signed.wire_b64).await?;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(st) = rpc.signature_statuses(std::slice::from_ref(&sig)).await {
            if let Some(Some((slot, err))) = st.first() {
                anyhow::ensure!(err.is_none(), "transaction failed: {err:?}");
                println!("confirmed in slot {slot}");
                return Ok(sig);
            }
        }
    }
    anyhow::bail!("not confirmed after 30 s: {sig}")
}

/// Close empty token accounts to reclaim rent (~0.002 SOL each).
pub async fn close_empty(cfg: &BotConfig, kp: &Keypair) -> anyhow::Result<()> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let me = kp.pubkey();
    let mut ixs = vec![];
    for prog in [TOKEN_PROGRAM, TOKEN_2022_PROGRAM] {
        for (acc, _mint, amt) in rpc.token_accounts(&me, &prog).await? {
            if amt == 0 {
                ixs.push(chain::ixs::close_token_account(&acc, &me, &me, &prog));
            }
        }
    }
    if ixs.is_empty() {
        println!("no empty token accounts");
        return Ok(());
    }
    let n = ixs.len();
    for chunk in ixs.chunks(20) {
        let sig = send_simple(cfg, kp, chunk.to_vec()).await?;
        println!("closed {} accounts: {sig}", chunk.len());
    }
    println!("reclaimed rent from {n} accounts");
    Ok(())
}

/// Move SOL from the hot wallet, keeping `keep_sol` behind.
pub async fn sweep(
    cfg: &BotConfig,
    kp: &Keypair,
    to: &Pubkey,
    keep_sol: f64,
) -> anyhow::Result<()> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let bal = rpc.balance(&kp.pubkey()).await?;
    let keep = sol_to_lamports(keep_sol) + 10_000;
    anyhow::ensure!(
        bal > keep,
        "balance {:.4} SOL ≤ keep {:.4}",
        lamports_to_sol(bal),
        keep_sol
    );
    let amt = bal - keep;
    println!("sweeping {:.6} SOL → {to}", lamports_to_sol(amt));
    let sig = send_simple(
        cfg,
        kp,
        vec![chain::ixs::system_transfer(&kp.pubkey(), to, amt)],
    )
    .await?;
    println!("{sig}");
    Ok(())
}

// ------------------------------------------------------------------ bench

/// Measure the in-process hot path: decode the leader's transaction → filters
/// + sizing → build the Pump buy → sign the transaction. Network time excluded.
pub fn bench(iterations: u32) {
    use chain::borsh::EVENT_IX_TAG;
    use chain::model::{Ix, TokenBal};
    use engine_core::sizing::{pre_trade_filters, size_buy, BookContext, EntryContext};

    let cfg = BotConfig::from_toml(include_str!("../../../../config/copybot.example.toml"))
        .expect("example config");
    let (leader, mint, creator) = (
        Pubkey::new_unique(),
        Pubkey::new_unique(),
        Pubkey::new_unique(),
    );
    let mut d = EVENT_IX_TAG.to_vec();
    d.extend(pump::event_disc("TradeEvent"));
    d.extend(mint.to_bytes());
    d.extend(1_000_000_000u64.to_le_bytes());
    d.extend(30_000_000_000_000u64.to_le_bytes());
    d.push(1);
    d.extend(leader.to_bytes());
    d.extend(0i64.to_le_bytes());
    for v in [
        31_000_000_000u64,
        1_040_000_000_000_000,
        31_000_000_000,
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
    let tx = ChainTx {
        signature: "s".into(),
        slot: 1,
        tx_index: Some(1),
        block_time_ms: None,
        observed_at_ns: 0,
        source: engine_core::types::FeedSource::Geyser,
        failed: false,
        fee: 5000,
        keys: vec![leader],
        num_signers: 1,
        top: vec![chain::model::Ix {
            program: PUMP_PROGRAM,
            accounts: vec![],
            data: vec![],
        }],
        inner: vec![(
            0,
            vec![Ix {
                program: PUMP_PROGRAM,
                accounts: vec![],
                data: d,
            }],
        )],
        pre_balances: vec![],
        post_balances: vec![],
        pre_tokens: vec![],
        post_tokens: vec![TokenBal {
            account: Pubkey::new_unique(),
            mint,
            owner: leader,
            program: TOKEN_2022_PROGRAM,
            amount: 1,
            decimals: 6,
        }],
        logs: vec![],
        has_meta: true,
    };
    let kp = Keypair::new();
    let bh = chain::solana_sdk::hash::Hash::new_from_array([7u8; 32]);
    let mut times = Vec::with_capacity(iterations as usize);
    for i in 0..iterations {
        let t = Instant::now();
        let s = &detect::swaps_by(&tx, &leader)[0];
        let entry = EntryContext {
            venue: s.venue,
            leader_buy: s.sol_amount,
            leader_price: s.price_sol,
            current_price: s.price_sol,
            pool_sol: s.pool_sol,
            token_age_secs: None,
            detection_slot_lag: 0,
        };
        pre_trade_filters(&cfg.engine.filters, &entry).expect("filters");
        let engine_core::sizing::SizeDecision::Buy { lamports, .. } = size_buy(
            &cfg.engine.sizing,
            &cfg.engine.risk,
            &entry,
            &BookContext {
                free_balance: 10_000_000_000,
                ..Default::default()
            },
        ) else {
            panic!("skip")
        };
        let chain::detect::Template::Curve {
            coin,
            state,
            fee_bps,
        } = &s.template
        else {
            panic!()
        };
        let mut coin = coin.clone();
        coin.mint = mint;
        let exp = pump::buy_tokens_for_quote(state, lamports, *fee_bps);
        let ixs = vec![
            chain::ixs::create_ata_idempotent(
                &kp.pubkey(),
                &kp.pubkey(),
                &mint,
                &coin.base_token_program,
            ),
            pump::buy_exact_quote_in_v2(
                &coin,
                &kp.pubkey(),
                lamports,
                pump::apply_slippage_down(exp, 1500),
                i as u64,
            ),
        ];
        let signed = tx::build(
            &kp,
            ixs,
            FeePlan {
                cu_limit: 260_000,
                cu_price_micro_lamports: 300_000,
                tip_lamports: 1_000_000,
            },
            Some(&Pubkey::new_unique()),
            bh,
        )
        .expect("build");
        std::hint::black_box(signed.wire_b64.len());
        times.push(t.elapsed().as_micros() as u64);
    }
    times.sort_unstable();
    let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    println!("hot path (decode → size → build → sign), {iterations} runs:");
    println!(
        "  p50 {} µs · p90 {} µs · p99 {} µs · max {} µs",
        p(0.5),
        p(0.9),
        p(0.99),
        times[times.len() - 1]
    );
    println!("  = {:.3} ms median of in-process work; the rest of the reaction time is network (feed + send).", p(0.5) as f64 / 1000.0);
}
