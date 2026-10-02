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
            all_ok &= check_nonces(cfg, &rpc, &pk).await;
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
        Err(e) if cfg.engine.mode != engine_core::config::RunMode::Live => {
            let _ = e;
            line(true, "keystore", "not needed in paper / shadow mode");
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

    all_ok &= check_feed(cfg).await;

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

/// Connect the configured feed (paid gRPC, else the free WebSocket one) and
/// count what arrives in 10 s.
async fn check_feed(cfg: &BotConfig) -> bool {
    let leaders: Vec<String> = cfg
        .engine
        .leaders
        .iter()
        .filter(|l| l.enabled)
        .map(|l| l.address.clone())
        .collect();
    let (ftx, frx) = watch::channel(Filters {
        leaders: leaders.clone(),
        mints: vec![],
    });
    let (tx, mut rx) = mpsc::channel(10_000);
    let (name, handles) = match (&cfg.infra.geyser, &cfg.infra.ws) {
        (Some(g), _) => (
            "geyser",
            vec![tokio::spawn(geyser::run(g.clone(), frx, tx))],
        ),
        (None, Some(w)) => {
            let rpc = match cfg.rpc_url() {
                Ok(u) => Rpc::new(u),
                Err(e) => {
                    line(false, "free feed", e);
                    return false;
                }
            };
            let (_wtx, wrx) = watch::channel(Vec::new());
            ("ws", chain::wsfeed::spawn(w.clone(), rpc, frx, wrx, tx))
        }
        _ => return false,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut txs, mut slots, mut connected, mut swaps, mut subs) =
        (0u64, 0u64, false, 0u64, 0usize);
    let mut err: Option<String> = None;
    while Instant::now() < deadline {
        match tokio::time::timeout(deadline - Instant::now(), rx.recv()).await {
            Ok(Some(FeedEvent::Status {
                connected: c,
                detail,
                ..
            })) => {
                if !c && !connected && err.is_none() {
                    err = Some(detail.clone());
                }
                if c && detail.starts_with("subscribed") {
                    subs += 1;
                }
                connected |= c;
            }
            Ok(Some(FeedEvent::Slot { .. })) => slots += 1,
            Ok(Some(FeedEvent::Tx(t))) => {
                txs += 1;
                swaps += detect::all_swaps(&t).len() as u64;
            }
            Ok(Some(FeedEvent::State(_))) => {}
            _ => break,
        }
    }
    handles.iter().for_each(|h| h.abort());
    drop(ftx);
    let mut ok = connected && slots > 0;
    if let (false, Some(e)) = (connected, &err) {
        line(false, &format!("{name} connect"), e);
    }
    line(
        ok,
        &format!("{name} stream"),
        format!("{slots} slot updates · {txs} txs · {swaps} decoded swaps in 10 s"),
    );
    if name == "ws" {
        let sub_ok = subs >= leaders.len();
        line(
            sub_ok,
            "leader subscriptions",
            format!("{subs}/{} accepted by the RPC", leaders.len()),
        );
        ok &= sub_ok;
    }
    ok
}

// ------------------------------------------------------------------ simulate

/// Build our real buy for `mint` on behalf of `as_wallet` (any funded address,
/// no keys needed) and run it through `simulateTransaction` against mainnet.
pub async fn simulate_buy(
    cfg: &BotConfig,
    mint: Pubkey,
    sol: f64,
    as_wallet: Pubkey,
    sell: bool,
) -> anyhow::Result<bool> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let lamports = sol_to_lamports(sol);
    let mint_acc = rpc
        .account(&mint)
        .await?
        .ok_or_else(|| anyhow::anyhow!("mint {mint} not found"))?;
    let tp = mint_acc.owner;
    // sell mode: sell everything the wallet holds of this mint
    let held: u64 = if sell {
        rpc.token_accounts(&as_wallet, &tp)
            .await?
            .iter()
            .filter(|(_, m, _)| *m == mint)
            .map(|(_, _, a)| *a)
            .sum()
    } else {
        0
    };
    anyhow::ensure!(!sell || held > 0, "{as_wallet} holds none of {mint}");
    let bc_key = pda::bonding_curve(&mint);
    let (body, route) = match rpc.account(&bc_key).await? {
        Some(a) if !BondingCurve::decode(&a.data)?.complete => {
            let bc = BondingCurve::decode(&a.data)?;
            let coin = CurveCoin::sol_paired(mint, bc.creator, tp, bc.is_mayhem_mode);
            if sell {
                let out = pump::sell_quote_for_tokens(&bc.state, held, 200);
                (
                    vec![pump::sell_v2(&coin, &as_wallet, held, 1, rand::random())],
                    format!("pump curve SELL {held} tokens · expected {out} lamports"),
                )
            } else {
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
            if sell {
                let out = pump_amm::sell_quote_for_base(amt(&accs[0]), qr, held, fee);
                (
                    pump_amm::sell_instructions(&coin, &as_wallet, held, 1),
                    format!(
                        "PumpSwap pool {pool_key} SELL {held} tokens · expected {out} lamports"
                    ),
                )
            } else {
                let expected = pump_amm::buy_base_for_quote(amt(&accs[0]), qr, lamports, fee);
                (
                    pump_amm::buy_instructions(
                        &coin,
                        &as_wallet,
                        lamports,
                        pump::apply_slippage_down(expected, 3000),
                    ),
                    format!("PumpSwap pool {pool_key} · expected {expected} tokens"),
                )
            }
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
        None => println!(
            "\nSIMULATION OK — the live program accepted our {} instruction",
            if sell { "sell" } else { "buy" }
        ),
        Some(e) => println!("\nSIMULATION FAILED: {e}"),
    }
    Ok(sim.err.is_none())
}

// ------------------------------------------------------------------ leader report

/// One wallet's headline numbers, for the side-by-side table.
pub struct WalletSummary {
    pub wallet: Pubkey,
    pub swaps: usize,
    pub round_trips: usize,
    pub pnl_sol: f64,
    pub win_rate: f64,
    pub median_hold_secs: f64,
    pub candidate: bool,
}

/// Side-by-side comparison of several vetted wallets.
pub fn print_wallet_table(rows: &[WalletSummary]) {
    println!("\nCOMPARISON");
    println!(
        "  {:<46} {:>6} {:>6} {:>10} {:>6} {:>9}  verdict",
        "wallet", "swaps", "trips", "PnL SOL", "win%", "hold s"
    );
    for r in rows {
        println!(
            "  {:<46} {:>6} {:>6} {:>+10.3} {:>5.0}% {:>9.0}  {}",
            r.wallet.to_string(),
            r.swaps,
            r.round_trips,
            r.pnl_sol,
            r.win_rate * 100.0,
            r.median_hold_secs,
            if r.candidate {
                "candidate"
            } else {
                "reject / watch"
            }
        );
    }
}

pub async fn leader_report(
    cfg: &BotConfig,
    wallet: Pubkey,
    limit: usize,
) -> anyhow::Result<WalletSummary> {
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
    let mut failed = 0usize;
    // ~4 transactions per second: what a free RPC tolerates per method
    for chunk in sigs.chunks(4) {
        let futs = chunk.iter().map(|s| rpc.transaction_json(s));
        for res in futures::future::join_all(futs).await {
            match res {
                Ok(v) if !v.is_null() => {
                    if let Ok(t) = ChainTx::from_rpc_json(&v) {
                        for s in detect::swaps_by(&t, &wallet) {
                            // parking SOL in USDC or staking it says nothing about coin picking
                            if !chain::base_assets::is_base_asset(&s.mint) {
                                swaps.push((t.block_time_ms.unwrap_or(0), s));
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => failed += 1,
            }
        }
        tokio::time::sleep(Duration::from_millis(900)).await;
    }
    if failed > 0 {
        eprintln!("warning: {failed} of {} transactions could not be fetched (RPC rate limit); the numbers below miss them", sigs.len());
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
    let mut notes = vec![];
    if r.median_hold_secs < 20.0 {
        flags.push(format!(
            "median hold {:.0}s: cannot be copied at any feed speed (you'd be exit liquidity)",
            r.median_hold_secs
        ));
    } else if r.median_hold_secs < sc.min_median_hold_secs {
        notes.push(format!(
            "fast flipper (median hold {:.0}s): only a fast feed copies it well; paper mode measures what survives",
            r.median_hold_secs
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
            let mut v = "CANDIDATE → add it under [[leaders]] and run paper mode to measure what copying it earns"
                .to_string();
            for n in &notes {
                v.push_str(&format!("; note: {n}"));
            }
            v
        } else {
            format!("REJECT/WATCH: {}", flags.join("; "))
        }
    );
    Ok(WalletSummary {
        wallet,
        swaps: swaps.len(),
        round_trips: r.n,
        pnl_sol: pnl,
        win_rate: r.win_rate,
        median_hold_secs: r.median_hold_secs,
        candidate: flags.is_empty(),
    })
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
        fetched_at_ns: 0,
        fetch_tries: 0,
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
            leader_sol_before: None,
            market_cap_sol: None,
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

// ------------------------------------------------------------------ discover (GMGN)

/// Scores ported from GMGN's official `gmgn-wallet-score` skill (stats-only
/// factors; entry-mcap / fast-flip factors need activity sampling and are
/// covered by `leader-report` on-chain + shadow mode).
#[derive(Debug, Clone, serde::Serialize)]
pub struct WalletCard {
    pub address: String,
    pub name: String,
    pub tags: Vec<String>,
    pub seen_buys: u32,
    pub seen_sells: u32,
    pub trades_7d: u64,
    pub tokens_7d: u64,
    pub realized_profit_usd: f64,
    pub roi: f64,
    pub winrate: f64,
    pub avg_hold_s: f64,
    pub created_tokens: u64,
    pub track_score: u32,
    pub copy_score_partial: u32,
    pub flags: Vec<String>,
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

pub fn score_stats(address: &str, s: &serde_json::Value) -> WalletCard {
    use serde_json::Value;
    let f = |v: &Value| match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(x) => x.parse().unwrap_or(0.0),
        _ => 0.0,
    };
    let pnl = &s["pnl_stat"];
    let common = &s["common"];
    let buy = f(if s["buy"].is_null() {
        &s["buy_count"]
    } else {
        &s["buy"]
    }) as u64;
    let sell = f(if s["sell"].is_null() {
        &s["sell_count"]
    } else {
        &s["sell"]
    }) as u64;
    let trades = buy + sell;
    let token_num = f(&pnl["token_num"]) as u64;
    let (gt5, x2_5, x0_2, lt_n50) = (
        f(&pnl["pnl_gt_5x_num"]),
        f(&pnl["pnl_2x_5x_num"]),
        f(&pnl["pnl_0x_2x_num"]),
        f(&pnl["pnl_lt_nd5_num"]),
    );
    let realized = f(&s["realized_profit"]);
    let roi = if s["realized_profit_pnl"].is_null() {
        f(&s["pnl"])
    } else {
        f(&s["realized_profit_pnl"])
    };
    let winrate = if pnl["winrate"].is_null() {
        f(&s["winrate"])
    } else {
        f(&pnl["winrate"])
    };
    let avg_hold_s = f(&pnl["avg_holding_period"]);
    let created = f(&common["created_token_count"]) as u64;
    let tn = token_num.max(1) as f64;

    // track-record score (GMGN weights)
    let tail = 1.0 - lt_n50 / tn;
    let upside = (gt5 + x2_5 + x0_2) / tn;
    let roi_f = clamp01((roi + 0.05) / 0.35);
    let win_f = clamp01(winrate / 0.5);
    let size_f = clamp01((tn - 20.0) / 300.0);
    let track =
        0.34 * clamp01(tail) + 0.28 * clamp01(upside) + 0.16 * roi_f + 0.10 * win_f + 0.12 * size_f;
    // copy-tradeability, stats-only factors (GMGN weights renormalised over profit/hold/feasible)
    let avg_trade_usd = if sell > 0 {
        realized / sell as f64
    } else {
        0.0
    };
    let profit_f = clamp01(avg_trade_usd / 80.0);
    let hold_f = clamp01(avg_hold_s / 172_800.0 + 0.15);
    let feasible_f = clamp01(1.0 - trades as f64 / 2500.0);
    let copy = (0.22 * profit_f + 0.20 * hold_f + 0.18 * feasible_f) / 0.60;
    let is_dev = created > 0 && created as f64 > 0.5 * tn;
    let discount = if is_dev { 0.45 } else { 1.0 };

    let mut flags = vec![];
    if trades >= 2000 {
        flags.push(format!("bot-tier frequency ({trades} trades/7d)"));
    }
    if avg_hold_s > 0.0 && avg_hold_s < 60.0 {
        flags.push(format!(
            "avg hold {avg_hold_s:.0}s: you'd be exit liquidity"
        ));
    }
    if is_dev {
        flags.push(format!(
            "mostly a token launcher ({created} created): self-dealing"
        ));
    }
    if realized <= 0.0 {
        flags.push("not profitable over 7d".into());
    }
    if token_num < 10 {
        flags.push("small sample".into());
    }
    WalletCard {
        address: address.to_string(),
        name: common["twitter_name"]
            .as_str()
            .or(common["name"].as_str())
            .unwrap_or("")
            .to_string(),
        tags: common["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        seen_buys: 0,
        seen_sells: 0,
        trades_7d: trades,
        tokens_7d: token_num,
        realized_profit_usd: realized,
        roi,
        winrate,
        avg_hold_s,
        created_tokens: created,
        track_score: (100.0 * track * discount).round() as u32,
        copy_score_partial: (100.0 * copy * discount).round() as u32,
        flags,
    }
}

pub async fn discover(cfg: &BotConfig, limit: u32, top: usize) -> anyhow::Result<()> {
    let gc = cfg.infra.gmgn.clone().unwrap_or(crate::gmgn::GmgnConfig {
        api_key_env: "GMGN_API_KEY".into(),
        requests_per_sec: 4.0,
        enrich: true,
        gate: None,
    });
    let gm = crate::gmgn::Gmgn::from_config(&gc)
        .ok_or_else(|| anyhow::anyhow!("set {} (GMGN OpenAPI key)", gc.api_key_env))?;
    let mut seen: HashMap<String, (u32, u32, Vec<String>)> = HashMap::new();
    for (src, res) in [
        ("smartmoney", gm.smartmoney_trades(limit).await),
        ("kol", gm.kol_trades(limit).await),
    ] {
        let v = res.map_err(|e| anyhow::anyhow!("{src}: {e}"))?;
        for t in v["list"].as_array().into_iter().flatten() {
            let Some(maker) = t["maker"].as_str() else {
                continue;
            };
            let e = seen.entry(maker.to_string()).or_default();
            if t["side"] == "buy" {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
            for tag in t["maker_info"]["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str())
            {
                if !e.2.iter().any(|x| x == tag) {
                    e.2.push(tag.to_string());
                }
            }
        }
    }
    eprintln!(
        "{} active smart-money / KOL wallets in the latest feed; scoring…",
        seen.len()
    );
    let wallets: Vec<String> = seen.keys().cloned().collect();
    let mut cards = vec![];
    for chunk in wallets.chunks(20) {
        let v = gm.wallet_stats(chunk, "7d").await?;
        let rows: Vec<serde_json::Value> = match &v {
            serde_json::Value::Array(a) => a.clone(),
            o => vec![o.clone()],
        };
        for (i, row) in rows.iter().enumerate() {
            let addr = row["wallet_address"]
                .as_str()
                .or(row["address"].as_str())
                .map(String::from)
                .or_else(|| chunk.get(i).cloned())
                .unwrap_or_default();
            let mut c = score_stats(&addr, row);
            if let Some((b, s, tags)) = seen.get(&addr) {
                c.seen_buys = *b;
                c.seen_sells = *s;
                for t in tags {
                    if !c.tags.contains(t) {
                        c.tags.push(t.clone());
                    }
                }
            }
            cards.push(c);
        }
    }
    cards.sort_by_key(|c| {
        std::cmp::Reverse(
            c.flags.is_empty() as u32 * 1000 + c.copy_score_partial * 2 + c.track_score,
        )
    });
    println!(
        "{:<44} {:>5} {:>5} {:>7} {:>10} {:>6} {:>8}  name / tags / flags",
        "wallet", "track", "copy", "trades", "pnl_usd", "win", "hold"
    );
    for c in cards.iter().take(top) {
        println!(
            "{:<44} {:>5} {:>5} {:>7} {:>10.0} {:>5.0}% {:>7.0}m  {} [{}] {}",
            c.address,
            c.track_score,
            c.copy_score_partial,
            c.trades_7d,
            c.realized_profit_usd,
            c.winrate * 100.0,
            c.avg_hold_s / 60.0,
            c.name,
            c.tags.join(","),
            if c.flags.is_empty() {
                String::new()
            } else {
                format!("⚠ {}", c.flags.join("; "))
            }
        );
    }
    println!("\n# Paste candidates into config as probation leaders, verify with `copybot leader-report <addr>`, then shadow mode:");
    for c in cards
        .iter()
        .filter(|c| c.flags.is_empty())
        .take(top.min(10))
    {
        println!("[[leaders]]\naddress = \"{}\"\nlabel = \"{}\"\nenabled = false   # track {} / copy {} (GMGN 7d)\n", c.address, if c.name.is_empty() { "gmgn" } else { &c.name }, c.track_score, c.copy_score_partial);
    }
    Ok(())
}

/// GMGN token panel for one mint: intel + tagged top holders.
pub async fn token_intel(cfg: &BotConfig, mint: &str) -> anyhow::Result<()> {
    let gc = cfg
        .infra
        .gmgn
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configure [infra.gmgn]"))?;
    let gm = crate::gmgn::Gmgn::from_config(&gc)
        .ok_or_else(|| anyhow::anyhow!("set {}", gc.api_key_env))?;
    let (intel, _) = gm.token_intel(mint).await?;
    println!("{}", serde_json::to_string_pretty(&intel)?);
    if let Some(g) = &gc.gate {
        println!(
            "gate: {}",
            match g.check(&intel) {
                Ok(()) => "PASS".to_string(),
                Err(e) => format!("BLOCK ({e})"),
            }
        );
    }
    for tag in [
        "sniper",
        "bundler",
        "rat_trader",
        "smart_degen",
        "renowned",
        "dev",
    ] {
        let v = gm.top_holders(mint, Some(tag), 20).await?;
        let list = v["list"]
            .as_array()
            .or(v.as_array())
            .cloned()
            .unwrap_or_default();
        let pct: f64 = list
            .iter()
            .filter_map(|h| {
                h["amount_percentage"]
                    .as_f64()
                    .or_else(|| h["amount_percentage"].as_str()?.parse().ok())
            })
            .sum();
        println!(
            "{tag:<12} {:>3} wallets holding {:.2}% of supply",
            list.len(),
            pct * 100.0
        );
    }
    Ok(())
}

/// GMGN view of a wallet: 7d/30d stats score + launched tokens.
pub async fn gmgn_wallet(cfg: &BotConfig, wallet: &str) -> anyhow::Result<()> {
    let Some(gc) = cfg.infra.gmgn.clone() else {
        return Ok(());
    };
    let Some(gm) = crate::gmgn::Gmgn::from_config(&gc) else {
        return Ok(());
    };
    println!("\nGMGN view");
    for period in ["7d", "30d"] {
        match gm.wallet_stats(&[wallet.to_string()], period).await {
            Ok(v) => {
                let row = v.as_array().and_then(|a| a.first().cloned()).unwrap_or(v);
                let c = score_stats(wallet, &row);
                println!(
                    "  {period}: track {} · copy {} (stats-only) · trades {} · pnl ${:.0} · win {:.0}% · avg hold {:.0} m {}",
                    c.track_score, c.copy_score_partial, c.trades_7d, c.realized_profit_usd, c.winrate * 100.0, c.avg_hold_s / 60.0,
                    if c.flags.is_empty() { String::new() } else { format!("⚠ {}", c.flags.join("; ")) }
                );
            }
            Err(e) => println!("  {period}: {e}"),
        }
    }
    if let Ok(ct) = gm.created_tokens(wallet).await {
        let n = ct["tokens"].as_array().map(|a| a.len()).unwrap_or(0);
        if n > 0 {
            println!(
                "  launched {n} tokens (open {} / still on curve {})",
                ct["open_count"], ct["inner_count"]
            );
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ durable nonces

pub async fn nonce_create(cfg: &BotConfig, kp: &Keypair, count: u32) -> anyhow::Result<()> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let rent: u64 = rpc
        .call(
            "getMinimumBalanceForRentExemption",
            serde_json::json!([chain::nonce::NONCE_ACCOUNT_SIZE]),
        )
        .await?
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("bad rent response"))?;
    let mut created = vec![];
    for _ in 0..count {
        let nonce_kp = Keypair::new();
        let ixs = chain::nonce::create_nonce_account(
            &kp.pubkey(),
            &nonce_kp.pubkey(),
            &kp.pubkey(),
            rent,
        );
        let (bh, _) = rpc.latest_blockhash("confirmed").await?;
        let signed = tx::build_multi(
            &[kp, &nonce_kp],
            ixs,
            50_000,
            cfg.infra.fees.cu_price_micro_lamports,
            bh,
        )?;
        let sig = rpc.send(&signed.wire_b64).await?;
        let mut ok = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(st) = rpc.signature_statuses(std::slice::from_ref(&sig)).await {
                if let Some(Some((_, err))) = st.first() {
                    anyhow::ensure!(err.is_none(), "nonce creation failed: {err:?}");
                    ok = true;
                    break;
                }
            }
        }
        anyhow::ensure!(ok, "nonce creation not confirmed: {sig}");
        println!("created nonce account {}", nonce_kp.pubkey());
        created.push(nonce_kp.pubkey().to_string());
    }
    let mut all = cfg.infra.nonce_accounts.clone();
    all.extend(created);
    println!(
        "\nadd to [infra] in your config:\nnonce_accounts = {:?}",
        all
    );
    Ok(())
}

pub async fn nonce_close(cfg: &BotConfig, kp: &Keypair) -> anyhow::Result<()> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    for a in &cfg.infra.nonce_accounts {
        let acc: Pubkey = a.parse()?;
        let Some(info) = rpc.account(&acc).await? else {
            println!("{acc}: not found");
            continue;
        };
        let ix = chain::nonce::withdraw_nonce(&acc, &kp.pubkey(), &kp.pubkey(), info.lamports);
        let sig = send_simple(cfg, kp, vec![ix]).await?;
        println!("closed {acc}: {sig}");
    }
    println!("remove nonce_accounts from the config");
    Ok(())
}

/// Preflight for nonce accounts: exist, initialised, owned by the hot wallet.
async fn check_nonces(cfg: &BotConfig, rpc: &Rpc, wallet: &Pubkey) -> bool {
    if cfg.infra.nonce_accounts.is_empty() {
        let families = Fanout::new(cfg.infra.senders.clone())
            .groups(&[Pubkey::new_unique()])
            .len();
        line(
            families < 2,
            "durable nonces",
            if families < 2 {
                "not needed (one tip family)".to_string()
            } else {
                format!("{families} tip families but no nonce_accounts: only the first family is used. Run `copybot wallet nonce-create`")
            },
        );
        return true;
    }
    let mut ok = true;
    for a in &cfg.infra.nonce_accounts {
        let res = match a.parse::<Pubkey>() {
            Err(e) => Err(e.to_string()),
            Ok(pk) => match rpc.account(&pk).await {
                Ok(Some(acc)) => match chain::nonce::parse_nonce_account(&acc.data) {
                    Some((auth, _)) if auth == *wallet => Ok(()),
                    Some((auth, _)) => Err(format!("authority {auth} ≠ hot wallet")),
                    None => Err("not an initialised nonce account".into()),
                },
                Ok(None) => Err("not found".into()),
                Err(e) => Err(e.to_string()),
            },
        };
        ok &= res.is_ok();
        line(
            res.is_ok(),
            &format!("nonce {}", &a[..8.min(a.len())]),
            res.err().unwrap_or_else(|| "ok".into()),
        );
    }
    ok
}

// ------------------------------------------------------------------ live decoder audit

/// Decode recent real transactions of a program and cross-check every decoded
/// swap against the trader's actual SOL balance change.
pub async fn audit(cfg: &BotConfig, program: &str, limit: usize) -> anyhow::Result<bool> {
    let rpc = Rpc::new(cfg.rpc_url()?);
    let prog: Pubkey = match program {
        "pump" => PUMP_PROGRAM,
        "pumpswap" => PUMP_AMM_PROGRAM,
        "dbc" => chain::meteora_dbc::DBC_PROGRAM,
        "launchlab" => chain::raydium_launchlab::LAUNCHLAB_PROGRAM,
        other => other.parse()?,
    };
    let sigs: Vec<String> = rpc
        .signatures_for_address(&prog, limit, None)
        .await?
        .into_iter()
        .filter(|v| v["err"].is_null())
        .filter_map(|v| v["signature"].as_str().map(String::from))
        .collect();
    let (mut txs, mut swaps, mut templated, mut agree, mut checked) = (0, 0, 0, 0, 0);
    for s in &sigs {
        let v = match rpc.transaction_json(s).await {
            Ok(v) if !v.is_null() => v,
            _ => continue,
        };
        let Ok(tx) = ChainTx::from_rpc_json(&v) else {
            continue;
        };
        txs += 1;
        for sw in detect::all_swaps(&tx) {
            swaps += 1;
            let tpl = match &sw.template {
                chain::detect::Template::Curve { .. } => "curve",
                chain::detect::Template::Amm { .. } => "pumpswap",
                chain::detect::Template::Dbc { .. } => "dbc",
                chain::detect::Template::LaunchLab { .. } => "launchlab",
                chain::detect::Template::Generic => "generic",
            };
            if tpl != "generic" {
                templated += 1;
            } else if sw.venue != engine_core::types::Venue::Other {
                eprintln!("NO-TEMPLATE {s}");
            }
            // cross-check exact (event) amounts with the wallet's balance delta
            let bal = detect::balance_swaps(&tx, &sw.wallet)
                .into_iter()
                .find(|b| b.mint == sw.mint);
            let verdict = match &bal {
                Some(b) if sw.exact => {
                    checked += 1;
                    let rel = (b.sol_amount as f64 - sw.sol_amount as f64).abs()
                        / (sw.sol_amount.max(1) as f64);
                    let side_ok = b.side == sw.side;
                    // balance delta includes fees, tips and ATA rent: allow 15% or 0.01 SOL
                    let ok = side_ok
                        && (rel < 0.15 || b.sol_amount.abs_diff(sw.sol_amount) < 10_000_000);
                    if ok {
                        agree += 1;
                    }
                    if ok {
                        "✓".to_string()
                    } else {
                        format!("✗ balance says {:?} {} lamports", b.side, b.sol_amount)
                    }
                }
                _ => "-".into(),
            };
            println!(
                "{} {:<4} {:>12.6} SOL {:>18} tok  px {:.3e}  {:<10} {:<16?} wallet {} mint {} {}",
                &s[..8],
                format!("{:?}", sw.side),
                engine_core::types::lamports_to_sol(sw.sol_amount),
                sw.token_amount,
                sw.price_sol,
                tpl,
                sw.venue,
                &sw.wallet.to_string()[..6],
                &sw.mint.to_string()[..6],
                verdict
            );
        }
    }
    println!("\n{txs} transactions · {swaps} swaps decoded · {templated} with an execution template · {agree}/{checked} event amounts agree with balance changes");
    Ok(checked == 0 || agree == checked)
}

#[cfg(test)]
mod discover_tests {
    use super::*;

    #[test]
    fn gmgn_scoring_port() {
        let good = serde_json::json!({
            "buy": 40, "sell": 35, "realized_profit": "12000", "pnl": 0.4,
            "pnl_stat": {"token_num": 60, "winrate": 0.55, "avg_holding_period": 5400, "pnl_gt_5x_num": 3, "pnl_2x_5x_num": 8, "pnl_0x_2x_num": 22, "pnl_lt_nd5_num": 2},
            "common": {"name": "alpha", "tags": ["smart_degen"], "created_token_count": 0}
        });
        let c = score_stats("A", &good);
        assert!(c.flags.is_empty(), "{:?}", c.flags);
        assert!(c.track_score > 60, "{}", c.track_score);
        let bot = serde_json::json!({
            "buy": 3000, "sell": 2900, "realized_profit": 900, "pnl": 0.02,
            "pnl_stat": {"token_num": 800, "winrate": 0.6, "avg_holding_period": 8, "pnl_lt_nd5_num": 50},
            "common": {}
        });
        let b = score_stats("B", &bot);
        assert!(b.flags.iter().any(|f| f.contains("bot-tier")));
        assert!(b.flags.iter().any(|f| f.contains("exit liquidity")));
        assert!(b.copy_score_partial < c.copy_score_partial);
        let dev = serde_json::json!({"buy": 30, "sell": 30, "realized_profit": 5000, "pnl_stat": {"token_num": 20}, "common": {"created_token_count": 15}});
        assert!(score_stats("D", &dev)
            .flags
            .iter()
            .any(|f| f.contains("launcher")));
    }
}
