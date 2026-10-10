//! copybot — Solana memecoin copy-trading engine.
//!
//!   copybot check            preflight: RPC, wallet, senders, gRPC feed, latency
//!   copybot run              start the engine (mode from config: shadow | paper | live)
//!   copybot report           profit proof from the journal: PnL, leaders, exits, execution cost
//!   copybot simulate         dry-run our real buy against mainnet (no keys, no funds)
//!   copybot leader-report    score a wallet from its on-chain history
//!   copybot wallet-audit     classify many wallets (bot / sniper / trader / holder) and write the leader list
//!   copybot census           record every launch, its checkpoints and the trending lists (free)
//!   copybot discover         leader candidates from GMGN smart-money / KOL feeds
//!   copybot wallet …         create / import / balance / sweep / close-empty
//!   copybot ctl <cmd>        talk to the running engine: status | positions | leaders | pause | resume | kill | flatten

mod audit;
mod census;
mod census_report;
mod cfg;
mod control;
mod engine;
mod exec;
mod gmgn;
mod journal;
mod keystore;
mod micro;
mod replay;
mod replay_trend;
mod report;
mod tools;
mod winners;

use std::sync::Arc;

use chain::rpc::Rpc;
use chain::sender::{fetch_jito_tip_accounts, Fanout};
use chain::solana_sdk::pubkey::Pubkey;
use chain::solana_sdk::signature::{Keypair, Signer};
use clap::{Parser, Subcommand};
use engine_core::config::RunMode;
use tokio::sync::{mpsc, watch};

#[derive(Parser)]
#[command(
    name = "copybot",
    version,
    about = "Solana memecoin copy-trading engine"
)]
struct Cli {
    /// Config file (see config/copybot.example.toml)
    #[arg(
        short,
        long,
        global = true,
        default_value = "config/copybot.toml",
        env = "COPYBOT_CONFIG"
    )]
    config: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Preflight: verify every dependency and measure latency
    Check,
    /// Run the engine
    Run {
        /// Stop gracefully after this many minutes (scripted runs); Ctrl-C also stops gracefully
        #[arg(long)]
        minutes: Option<u64>,
    },
    /// Simulate our buy for a token against mainnet on behalf of any funded address
    Simulate {
        #[arg(long)]
        mint: String,
        #[arg(long, default_value_t = 0.1)]
        sol: f64,
        /// Any address holding enough SOL (no keys needed: signature checks are skipped)
        #[arg(long = "as")]
        as_wallet: String,
        /// Simulate selling the wallet's whole balance of the mint instead
        #[arg(long)]
        sell: bool,
    },
    /// Analyse one or more wallets' on-chain trading history (free; compares several side by side)
    LeaderReport {
        #[arg(required = true, num_args = 1..)]
        addresses: Vec<String>,
        /// Transactions to read per wallet (more = steadier verdict, slower on a free RPC)
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// Classify wallets as bot / sniper / trader / holder from their own on-chain history and
    /// write the leaders the engine may follow (free; re-run weekly)
    WalletAudit {
        /// Wallet addresses (or use --file)
        addresses: Vec<String>,
        /// File with one wallet per line: `name: address`, `name address` or `address`
        #[arg(long)]
        file: Option<String>,
        /// Own transactions to read per wallet (more = a longer window and a steadier
        /// verdict; ~5 min per active wallet on a free RPC)
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Folder for the full results (JSON + table)
        #[arg(long, default_value = "data/wallet-audit")]
        out: String,
        /// Write the wallets that qualify as `[[leaders]]` here (load it with `leaders_file = "…"`)
        #[arg(long)]
        leaders_out: Option<String>,
    },
    /// Record every launch, each coin's state at checkpoints from 15 s to 24 h, and the
    /// trending lists, from free sources (RPC_URL optional: adds exact Pump.fun curve state)
    Census {
        /// Folder for the tape (one subfolder per UTC day; state.json for resuming)
        #[arg(long, default_value = "data/census")]
        out: String,
        /// Stop after this many minutes (pending checkpoints are saved for the next run)
        #[arg(long)]
        minutes: Option<u64>,
        /// Seconds between captures of the trending and attention lists
        #[arg(long, default_value_t = 300)]
        trending_secs: u64,
        /// Response bodies kept in the raw archive
        #[arg(long, value_enum, default_value_t = census::RawMode::Lists)]
        raw: census::RawMode,
        /// WebSocket endpoint(s) streaming the pump program's logs (every create and
        /// trade; the public one works), comma-separated; `off` to record without it
        #[arg(long, default_value = "wss://api.mainnet-beta.solana.com")]
        trades_ws: String,
        /// Connections kept to each endpoint at once (a recycled one leaves no hole)
        #[arg(long, default_value_t = 2)]
        trades_ws_conns: usize,
        /// Decoded events kept in trades-<hour>.jsonl.gz
        #[arg(long, value_enum, default_value_t = micro::TradesMode::FirstHour)]
        trades: micro::TradesMode,
    },
    /// Labels and daily tables from the census tape: coverage, base rates, early-signal
    /// table, top 10 launches and top 20 trending (writes <dir>/<day>/daily.md)
    CensusReport {
        #[arg(long, default_value = "data/census")]
        dir: String,
        /// UTC day (YYYY-MM-DD); default today
        #[arg(long)]
        day: Option<String>,
    },
    /// Test entry and exit rules on the recorded trade tape: exact curve fills, fees and
    /// our delay; pairs picked walk-forward; newest data locked until --final
    Replay {
        #[arg(long, default_value = "data/census")]
        dir: String,
        /// First UTC day of coins to use (YYYY-MM-DD)
        #[arg(long)]
        from: Option<String>,
        /// Last UTC day of coins to use
        #[arg(long)]
        to: Option<String>,
        /// SOL per trade
        #[arg(long, default_value_t = 0.25)]
        size: f64,
        /// Seconds from a decision (or an exit trigger) until our transaction lands
        #[arg(long, default_value_t = 4.0)]
        delay: f64,
        /// Network + priority fee per transaction, SOL
        #[arg(long, default_value_t = 0.001)]
        tx_cost: f64,
        /// Walk-forward block length
        #[arg(long, default_value_t = 6)]
        block_hours: i64,
        /// Share of the newest coins locked away until --final
        #[arg(long, default_value_t = 0.2)]
        holdout: f64,
        /// Include the locked holdout: the one-time final test
        #[arg(long = "final")]
        final_run: bool,
        /// Starting bankroll for the bankroll run, SOL
        #[arg(long, default_value_t = 1.0)]
        bankroll: f64,
        /// Share of the bankroll per trade
        #[arg(long, default_value_t = 0.1)]
        bet: f64,
        /// Most positions open at once
        #[arg(long, default_value_t = 10)]
        max_open: usize,
    },
    /// Work backwards from the winners: what singles out the coins that reach $30k and
    /// $100k at 5-300 s, how early, and what entering at that signal pays on the tape
    Winners {
        #[arg(long, default_value = "data/census")]
        dir: String,
        /// Where the trade tape (trades-*.jsonl.gz) is, if not under --dir
        #[arg(long)]
        trades: Option<String>,
        /// Only coins created on this UTC day (YYYY-MM-DD)
        #[arg(long)]
        day: Option<String>,
        /// SOL per trade in the signal-as-entry replay
        #[arg(long, default_value_t = 0.5)]
        size: f64,
        /// Seconds from the signal until our transaction lands
        #[arg(long, default_value_t = 4.0)]
        delay: f64,
        /// Network + priority fee per transaction, SOL
        #[arg(long, default_value_t = 0.001)]
        tx_cost: f64,
        /// A leaders file (`[[leaders]] address = ...`): "a leader bought within 60 s" is
        /// then tested as a signal too
        #[arg(long)]
        leaders: Option<String>,
    },
    /// Test entry and exit rules for the trending tier on the census's 5-minute captures
    /// (fills moved by size against the pool's liquidity; walk-forward; newest coins locked)
    ReplayTrending {
        #[arg(long, default_value = "data/census")]
        dir: String,
        /// SOL per trade
        #[arg(long, default_value_t = 5.0)]
        size: f64,
        /// SOL price in dollars (default: the census's own sol_price rows)
        #[arg(long)]
        sol_usd: Option<f64>,
        /// Swap fee per side (0.005 = 0.5%)
        #[arg(long, default_value_t = 0.005)]
        fee: f64,
        /// Network + priority fee per transaction, SOL
        #[arg(long, default_value_t = 0.001)]
        tx_cost: f64,
        /// Loss assumed on a coin that leaves every list and never prices again
        #[arg(long, default_value_t = 0.3)]
        delist_haircut: f64,
        #[arg(long, default_value_t = 6)]
        block_hours: i64,
        #[arg(long, default_value_t = 0.2)]
        holdout: f64,
        #[arg(long = "final")]
        final_run: bool,
        /// Trades a pair needs before it can be picked
        #[arg(long, default_value_t = 30)]
        min_trades: usize,
        #[arg(long, default_value_t = 1.0)]
        bankroll: f64,
        #[arg(long, default_value_t = 0.1)]
        bet: f64,
        #[arg(long, default_value_t = 5)]
        max_open: usize,
    },
    /// Control the running engine: status | positions | leaders | pause | resume | kill | flatten | stop | blacklist [<address>] | unblacklist <address>
    Ctl {
        #[arg(trailing_var_arg = true, num_args = 1.., required = true)]
        command: Vec<String>,
    },
    /// Find leader candidates from GMGN's live smart-money / KOL feeds (needs GMGN_API_KEY)
    Discover {
        #[arg(long, default_value_t = 200)]
        limit: u32,
        #[arg(long, default_value_t = 25)]
        top: usize,
    },
    /// GMGN token panel: holders, dev, snipers, bundlers, insiders, smart money, socials, gate verdict
    TokenIntel { mint: String },
    /// Decode recent real transactions of a program (pump | pumpswap | dbc | launchlab | <id>) and cross-check them
    Audit {
        program: String,
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Measure in-process reaction time (decode → size → build → sign)
    Bench {
        #[arg(long, default_value_t = 10000)]
        iterations: u32,
    },
    /// Profit proof: results, leaders, exits and execution costs from the journal
    Report {
        /// Journal directory (default: [infra.storage] journal_dir from the config)
        #[arg(long)]
        dir: Option<String>,
        /// Only the last N hours
        #[arg(long)]
        hours: Option<f64>,
    },
    /// Hot-wallet management
    Wallet {
        #[command(subcommand)]
        cmd: WalletCmd,
    },
}

#[derive(Subcommand)]
enum WalletCmd {
    /// Create a new encrypted hot wallet
    New {
        #[arg(long)]
        out: String,
        #[arg(long, default_value = "hot")]
        label: String,
    },
    /// Import an existing key (base58 secret or a solana-keygen JSON file)
    Import {
        #[arg(long)]
        out: String,
        /// base58 secret key, or path to a JSON keypair file
        #[arg(long)]
        secret: String,
        #[arg(long, default_value = "imported")]
        label: String,
    },
    /// Show SOL and token balances of the configured hot wallet
    Balance,
    /// Send SOL from the hot wallet to another address, keeping a reserve
    Sweep {
        #[arg(long)]
        to: String,
        #[arg(long, default_value_t = 0.05)]
        keep: f64,
    },
    /// Close empty token accounts to reclaim rent
    CloseEmpty,
    /// Create durable nonce accounts for multi-sender fan-out (~0.0015 SOL rent each, refundable)
    NonceCreate {
        #[arg(long, default_value_t = 4)]
        count: u32,
    },
    /// Close the configured nonce accounts and return their rent
    NonceClose,
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        // colours only on a terminal: logs redirected to a file or CI stay readable
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Wallet {
            cmd: WalletCmd::New { out, label },
        } => {
            let kp = Keypair::new();
            let pass = keystore::passphrase("KEYSTORE_PASSPHRASE", true)?;
            keystore::write(&out, &keystore::encrypt(&kp, &pass, &label)?)?;
            println!(
                "created {out}\naddress {}\nfund this address with SOL before running in live mode",
                kp.pubkey()
            );
            Ok(())
        }
        Cmd::Wallet {
            cmd: WalletCmd::Import { out, secret, label },
        } => {
            let kp = if std::path::Path::new(&secret).exists() {
                let bytes: Vec<u8> = serde_json::from_str(&std::fs::read_to_string(&secret)?)?;
                Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                Keypair::try_from(bs58::decode(secret.trim()).into_vec()?.as_slice())
                    .map_err(|e| anyhow::anyhow!("{e}"))?
            };
            let pass = keystore::passphrase("KEYSTORE_PASSPHRASE", true)?;
            keystore::write(&out, &keystore::encrypt(&kp, &pass, &label)?)?;
            println!("imported {} → {out}", kp.pubkey());
            Ok(())
        }
        Cmd::Bench { iterations } => {
            tools::bench(iterations);
            Ok(())
        }
        Cmd::WalletAudit {
            addresses,
            file,
            limit,
            out,
            leaders_out,
        } => {
            // needs only an RPC: RPC_URL, else the config's
            let url = match cfg::env("RPC_URL") {
                Ok(u) => u,
                Err(_) => cfg::BotConfig::load(&cli.config)?.rpc_url()?,
            };
            wallet_audit(&url, addresses, file, limit, &out, leaders_out.as_deref()).await
        }
        Cmd::Census {
            out,
            minutes,
            trending_secs,
            raw,
            trades_ws,
            trades_ws_conns,
            trades,
        } => {
            census::run(census::CensusArgs {
                out: out.into(),
                minutes,
                rpc_url: cfg::env("RPC_URL").ok(),
                trending_secs,
                raw,
                trades_ws: trades_ws
                    .split(',')
                    .map(str::trim)
                    .filter(|u| !u.is_empty() && *u != "off")
                    .map(String::from)
                    .collect(),
                trades_ws_conns,
                trades,
            })
            .await
        }
        Cmd::Replay {
            dir,
            from,
            to,
            size,
            delay,
            tx_cost,
            block_hours,
            holdout,
            final_run,
            bankroll,
            bet,
            max_open,
        } => {
            print!(
                "{}",
                replay::write(&replay::ReplayArgs {
                    dir: dir.into(),
                    from,
                    to,
                    size_sol: size,
                    delay_s: delay,
                    tx_cost_sol: tx_cost,
                    block_hours,
                    holdout,
                    final_run,
                    bankroll_sol: bankroll,
                    bet_share: bet,
                    max_open,
                })?
            );
            Ok(())
        }
        Cmd::Winners {
            dir,
            trades,
            day,
            size,
            delay,
            tx_cost,
            leaders,
        } => {
            print!(
                "{}",
                winners::write(&winners::Args {
                    dir: dir.into(),
                    trades: trades.map(Into::into),
                    day,
                    size_sol: size,
                    delay_s: delay,
                    tx_cost_sol: tx_cost,
                    leaders_file: leaders.map(Into::into),
                })?
            );
            Ok(())
        }
        Cmd::ReplayTrending {
            dir,
            size,
            sol_usd,
            fee,
            tx_cost,
            delist_haircut,
            block_hours,
            holdout,
            final_run,
            min_trades,
            bankroll,
            bet,
            max_open,
        } => {
            print!(
                "{}",
                replay_trend::write(&replay_trend::Args {
                    dir: dir.into(),
                    size_sol: size,
                    sol_usd,
                    fee,
                    tx_sol: tx_cost,
                    delist_haircut,
                    block_hours,
                    holdout,
                    final_run,
                    min_trades,
                    bankroll_sol: bankroll,
                    bet_share: bet,
                    max_open,
                })?
            );
            Ok(())
        }
        Cmd::CensusReport { dir, day } => {
            let day = day.unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
            print!(
                "{}",
                census_report::write(std::path::Path::new(&dir), &day)?
            );
            Ok(())
        }
        Cmd::Report { dir, hours } => {
            let dir = match dir {
                Some(d) => d,
                None => cfg::BotConfig::load(&cli.config)?.infra.storage.journal_dir,
            };
            print!("{}", report::run(&dir, hours)?);
            Ok(())
        }
        cmd => {
            let cfg = cfg::BotConfig::load(&cli.config)?;
            match cmd {
                Cmd::Check => {
                    let ok = tools::check(&cfg).await?;
                    std::process::exit(if ok { 0 } else { 1 });
                }
                Cmd::Simulate {
                    mint,
                    sol,
                    as_wallet,
                    sell,
                } => {
                    let ok =
                        tools::simulate_buy(&cfg, mint.parse()?, sol, as_wallet.parse()?, sell)
                            .await?;
                    std::process::exit(if ok { 0 } else { 1 });
                }
                Cmd::LeaderReport { addresses, limit } => {
                    let mut rows = vec![];
                    for (i, address) in addresses.iter().enumerate() {
                        if i > 0 {
                            // a free RPC counts calls over a window: give it a breather between wallets
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        }
                        match tools::leader_report(&cfg, address.parse()?, limit).await {
                            Ok(row) => rows.push(row),
                            Err(e) => eprintln!("wallet {address}: could not be vetted: {e}"),
                        }
                        let _ = tools::gmgn_wallet(&cfg, address).await;
                    }
                    if rows.len() > 1 {
                        tools::print_wallet_table(&rows);
                    }
                    Ok(())
                }
                Cmd::Wallet {
                    cmd: WalletCmd::Balance,
                } => {
                    let pk: Pubkey = keystore::read(&cfg.infra.wallet.keystore)?.pubkey.parse()?;
                    tools::wallet_balance(&cfg, &pk).await
                }
                Cmd::Wallet {
                    cmd: WalletCmd::Sweep { to, keep },
                } => {
                    let kp = keystore::load(
                        &cfg.infra.wallet.keystore,
                        &cfg.infra.wallet.passphrase_env,
                    )?;
                    tools::sweep(&cfg, &kp, &to.parse()?, keep).await
                }
                Cmd::Wallet {
                    cmd: WalletCmd::NonceCreate { count },
                } => {
                    let kp = keystore::load(
                        &cfg.infra.wallet.keystore,
                        &cfg.infra.wallet.passphrase_env,
                    )?;
                    tools::nonce_create(&cfg, &kp, count).await
                }
                Cmd::Wallet {
                    cmd: WalletCmd::NonceClose,
                } => {
                    let kp = keystore::load(
                        &cfg.infra.wallet.keystore,
                        &cfg.infra.wallet.passphrase_env,
                    )?;
                    tools::nonce_close(&cfg, &kp).await
                }
                Cmd::Wallet {
                    cmd: WalletCmd::CloseEmpty,
                } => {
                    let kp = keystore::load(
                        &cfg.infra.wallet.keystore,
                        &cfg.infra.wallet.passphrase_env,
                    )?;
                    tools::close_empty(&cfg, &kp).await
                }
                Cmd::Run { minutes } => run(cfg, minutes).await,
                Cmd::Discover { limit, top } => tools::discover(&cfg, limit, top).await,
                Cmd::TokenIntel { mint } => tools::token_intel(&cfg, &mint).await,
                Cmd::Audit { program, limit } => {
                    let ok = tools::audit(&cfg, &program, limit).await?;
                    std::process::exit(if ok { 0 } else { 1 });
                }
                Cmd::Ctl { command } => {
                    print!(
                        "{}",
                        control::send(&cfg.infra.control_socket, &command.join(" ")).await?
                    );
                    Ok(())
                }
                Cmd::Wallet { .. }
                | Cmd::Bench { .. }
                | Cmd::Report { .. }
                | Cmd::WalletAudit { .. }
                | Cmd::Census { .. }
                | Cmd::CensusReport { .. }
                | Cmd::Replay { .. }
                | Cmd::ReplayTrending { .. }
                | Cmd::Winners { .. } => unreachable!(),
            }
        }
    }
}

async fn run(cfg: cfg::BotConfig, minutes: Option<u64>) -> anyhow::Result<()> {
    let valid_leaders = cfg
        .engine
        .leaders
        .iter()
        .filter(|l| l.enabled && l.address.parse::<Pubkey>().is_ok())
        .count();
    anyhow::ensure!(
        valid_leaders > 0,
        "no leader wallets to follow: put real Solana addresses under [[leaders]] in the config \
         (find them on GMGN / Axiom / KOLscan, vet them with `copybot leader-report <WALLET>`)"
    );
    let live = cfg.engine.mode == RunMode::Live;
    let kp = match keystore::load(&cfg.infra.wallet.keystore, &cfg.infra.wallet.passphrase_env) {
        Ok(k) => Arc::new(k),
        Err(e) if !live => {
            tracing::warn!(
                "no keystore ({e}); {:?} mode runs without a wallet",
                cfg.engine.mode
            );
            Arc::new(Keypair::new())
        }
        Err(e) => return Err(e),
    };
    let me = kp.pubkey();
    let rpc = Rpc::new(cfg.rpc_url()?);

    let exec = if live {
        let tips: Vec<Pubkey> = if !cfg.infra.tip_accounts.is_empty() {
            cfg.infra
                .tip_accounts
                .iter()
                .map(|s| s.parse())
                .collect::<Result<_, _>>()?
        } else if cfg.infra.fees.tip_lamports_buy > 0 || cfg.infra.fees.tip_lamports_sell > 0 {
            fetch_jito_tip_accounts(&cfg.infra.jito_block_engine)
                .await
                .map_err(|e| anyhow::anyhow!("tip accounts: {e}"))?
        } else {
            vec![]
        };
        let ex = Arc::new(exec::Exec::new(
            rpc.clone(),
            Fanout::new(cfg.infra.senders.clone()),
            kp.clone(),
            cfg.infra.fees.clone(),
            tips,
            cfg.infra.jupiter.clone(),
            cfg.infra
                .nonce_accounts
                .iter()
                .map(|s| s.parse())
                .collect::<Result<Vec<Pubkey>, _>>()
                .map_err(|e| anyhow::anyhow!("infra.nonce_accounts: {e}"))?,
        ));
        ex.spawn_refreshers();
        Some(ex)
    } else {
        None
    };

    let pg = cfg
        .infra
        .storage
        .postgres_url_env
        .as_ref()
        .and_then(|e| std::env::var(e).ok())
        .filter(|s| !s.is_empty());
    let journal = journal::Journal::start(
        &cfg.infra.storage.journal_dir,
        pg,
        cfg.infra.storage.clickhouse_url.clone(),
    )
    .await?;

    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    control::serve(&cfg.infra.control_socket, cmd_tx.clone())?;
    tracing::info!(
        "control socket: {} (use `copybot ctl status`)",
        cfg.infra.control_socket
    );

    let gmgn_client = cfg.infra.gmgn.as_ref().and_then(|g| {
        let c = gmgn::Gmgn::from_config(g);
        if c.is_none() {
            tracing::warn!(
                "[infra.gmgn] configured but {} is not set: token intelligence disabled",
                g.api_key_env
            );
        }
        c.map(Arc::new)
    });

    let (feed_tx, feed_rx) = mpsc::channel(200_000);
    let (filters_tx, filters_rx) = watch::channel(chain::feed::Filters::default());
    let (watch_tx, watch_rx) = watch::channel(Vec::new());
    let (res_tx, res_rx) = mpsc::channel(10_000);
    let mut engine = engine::Engine::new(
        cfg.clone(),
        me,
        exec,
        gmgn_client,
        journal,
        filters_tx,
        res_tx,
    );
    if live {
        // Recover: anything already held is put back under exit management.
        for prog in [
            chain::consts::TOKEN_PROGRAM,
            chain::consts::TOKEN_2022_PROGRAM,
        ] {
            match rpc.token_accounts(&me, &prog).await {
                Ok(accs) => {
                    for (_, mint, amount) in accs {
                        engine.adopt(mint, amount, prog, 6);
                    }
                }
                Err(e) => tracing::warn!("startup token scan: {e}"),
            }
        }
    }

    if !live {
        engine.set_rpc(rpc.clone());
    }
    match (&cfg.infra.geyser, &cfg.infra.ws) {
        (Some(g), _) => {
            tracing::info!("feed: Yellowstone gRPC {}", g.endpoint);
            tokio::spawn(chain::geyser::run(
                g.clone(),
                filters_rx.clone(),
                feed_tx.clone(),
            ));
            if g.deshred {
                tokio::spawn(chain::geyser::run_deshred(
                    g.clone(),
                    filters_rx,
                    feed_tx.clone(),
                ));
            }
        }
        (None, Some(w)) => {
            tracing::info!("feed: free WebSocket + RPC (no paid streaming plan)");
            engine.set_free_feed(watch_tx);
            chain::wsfeed::spawn(
                w.clone(),
                rpc.clone(),
                filters_rx,
                watch_rx,
                feed_tx.clone(),
            );
        }
        (None, None) => anyhow::bail!("no data feed configured"),
    }
    drop(feed_tx);

    if let Some(m) = minutes {
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(m * 60)).await;
            let (r, _rx) = tokio::sync::oneshot::channel();
            let _ = tx.send((control::Command::Stop, r)).await;
        });
    }

    let run = engine.run(feed_rx, res_rx, cmd_rx);
    tokio::pin!(run);
    tokio::select! {
        _ = &mut run => {}
        _ = shutdown_signal() => {
            tracing::warn!("stopping gracefully (signal again to force)");
            let (r, _rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send((control::Command::Stop, r)).await;
            tokio::select! {
                _ = &mut run => {}
                _ = shutdown_signal() => {}
            }
        }
    }
    // let the journal writer drain
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    Ok(())
}

/// Ctrl-C, or SIGTERM from systemd / a container runtime.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn wallet_audit(
    rpc_url: &str,
    addresses: Vec<String>,
    file: Option<String>,
    limit: usize,
    out: &str,
    leaders_out: Option<&str>,
) -> anyhow::Result<()> {
    let mut text = addresses.join("\n");
    if let Some(f) = &file {
        text.push('\n');
        text.push_str(&std::fs::read_to_string(f).map_err(|e| anyhow::anyhow!("{f}: {e}"))?);
    }
    let list = audit::parse_list(&text);
    anyhow::ensure!(
        !list.is_empty(),
        "no wallet addresses given (arguments or --file)"
    );
    let rpc = Rpc::new(rpc_url.to_string());
    let mut rows = vec![];
    for (i, c) in list.iter().enumerate() {
        if i > 0 {
            // a free RPC counts calls over a window: give it a breather between wallets
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        eprintln!("[{}/{}] {} {}", i + 1, list.len(), c.name, c.wallet);
        match audit::audit_one(&rpc, c, limit).await {
            Ok(r) => {
                eprintln!(
                    "    {:?} / {:?}: {} trips, {:+.2} SOL, read {}/{} ({} sent by others)",
                    r.assessment.verdict,
                    r.assessment.class,
                    r.trading.round_trips,
                    r.trading.pnl_sol,
                    r.fetched,
                    r.fetched + r.fetch_failed,
                    r.foreign_txs
                );
                rows.push(r)
            }
            Err(e) => eprintln!("    could not be audited: {e}"),
        }
    }
    let table = audit::table(&rows);
    println!(
        "\nWALLET AUDIT ({} wallets, last {limit} own transactions each)\n{table}",
        rows.len()
    );
    let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
    std::fs::create_dir_all(out)?;
    let json_path = format!("{out}/{day}.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(&rows)?)?;
    std::fs::write(format!("{out}/{day}.txt"), &table)?;
    println!("full results: {json_path}");
    if let Some(p) = leaders_out {
        std::fs::write(p, audit::leaders_toml(&rows, &day))?;
        let n = rows
            .iter()
            .filter(|r| r.assessment.verdict == audit::Verdict::Leader)
            .count();
        println!("{n} leader(s) written to {p}");
    }
    Ok(())
}
