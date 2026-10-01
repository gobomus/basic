//! copybot — Solana memecoin copy-trading engine.
//!
//!   copybot check            preflight: RPC, wallet, senders, gRPC feed, latency
//!   copybot run              start the engine (mode from config: shadow | paper | live)
//!   copybot simulate         dry-run our real buy against mainnet (no keys, no funds)
//!   copybot leader-report    score a wallet from its on-chain history
//!   copybot discover         leader candidates from GMGN smart-money / KOL feeds
//!   copybot wallet …         create / import / balance / sweep / close-empty
//!   copybot ctl <cmd>        talk to the running engine: status | positions | leaders | pause | resume | kill | flatten

mod cfg;
mod control;
mod engine;
mod exec;
mod gmgn;
mod journal;
mod keystore;
mod tools;

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
    Run,
    /// Simulate our buy for a token against mainnet on behalf of any funded address
    Simulate {
        #[arg(long)]
        mint: String,
        #[arg(long, default_value_t = 0.1)]
        sol: f64,
        /// Any address holding enough SOL (no keys needed: signature checks are skipped)
        #[arg(long = "as")]
        as_wallet: String,
    },
    /// Analyse a wallet's on-chain trading history
    LeaderReport {
        address: String,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// Control the running engine: status | positions | leaders | pause | resume | kill | flatten
    Ctl { command: String },
    /// Find leader candidates from GMGN's live smart-money / KOL feeds (needs GMGN_API_KEY)
    Discover {
        #[arg(long, default_value_t = 200)]
        limit: u32,
        #[arg(long, default_value_t = 25)]
        top: usize,
    },
    /// GMGN token panel: holders, dev, snipers, bundlers, insiders, smart money, socials, gate verdict
    TokenIntel { mint: String },
    /// Measure in-process reaction time (decode → size → build → sign)
    Bench {
        #[arg(long, default_value_t = 10000)]
        iterations: u32,
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
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
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
                } => {
                    let ok =
                        tools::simulate_buy(&cfg, mint.parse()?, sol, as_wallet.parse()?).await?;
                    std::process::exit(if ok { 0 } else { 1 });
                }
                Cmd::LeaderReport { address, limit } => {
                    tools::leader_report(&cfg, address.parse()?, limit).await?;
                    tools::gmgn_wallet(&cfg, &address).await
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
                    cmd: WalletCmd::CloseEmpty,
                } => {
                    let kp = keystore::load(
                        &cfg.infra.wallet.keystore,
                        &cfg.infra.wallet.passphrase_env,
                    )?;
                    tools::close_empty(&cfg, &kp).await
                }
                Cmd::Run => run(cfg).await,
                Cmd::Discover { limit, top } => tools::discover(&cfg, limit, top).await,
                Cmd::TokenIntel { mint } => tools::token_intel(&cfg, &mint).await,
                Cmd::Ctl { command } => {
                    print!(
                        "{}",
                        control::send(&cfg.infra.control_socket, &command).await?
                    );
                    Ok(())
                }
                Cmd::Wallet { .. } | Cmd::Bench { .. } => unreachable!(),
            }
        }
    }
}

async fn run(cfg: cfg::BotConfig) -> anyhow::Result<()> {
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
    control::serve(&cfg.infra.control_socket, cmd_tx)?;
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
    let (filters_tx, filters_rx) = watch::channel(chain::geyser::Filters::default());
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

    tokio::spawn(chain::geyser::run(
        cfg.infra.geyser.clone(),
        filters_rx.clone(),
        feed_tx.clone(),
    ));
    if cfg.infra.geyser.deshred {
        tokio::spawn(chain::geyser::run_deshred(
            cfg.infra.geyser.clone(),
            filters_rx,
            feed_tx.clone(),
        ));
    }
    drop(feed_tx);

    tokio::select! {
        _ = engine.run(feed_rx, res_rx, cmd_rx) => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("ctrl-c: stopping. Open positions are NOT sold automatically: run `copybot ctl flatten` first, or they are re-adopted on the next start.");
        }
    }
    Ok(())
}
