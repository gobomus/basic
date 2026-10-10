# Solana memecoin copy-trading engine → self-calibrating trading engine

A high-frequency copy-trading engine for Solana memecoins, built in three steps:

1. **Copy selected leader wallets.** Enter with a percentage of each leader's size, at their entry.
2. **Exit on our own, data-tuned logic.** We don't wait for the leader to sell.
3. **Record everything** that terminals like GMGN, Axiom and Padre show (volume, transactions per second, holders, snipers, bundlers, dev history, socials, …) plus our own execution data. The resulting trade log tunes exits and sizing first, and later a **leader-free, self-adjusting engine**.

> **Status: the free proof-of-concept path is operational; the paid live path is built and verified against mainnet.**
> - **Path A, free (start here).** Paper mode on a standard RPC WebSocket feed: it follows your chosen leaders live and simulates every copy **as it would really have landed** (priced from the pool 1.2 s after detection, with the same slippage limit a real transaction has, every fee charged). `copybot report` then shows PnL after costs, by leader and by exit style, with a go / no-go checklist. Runs from GitHub Actions (nothing to install) or any computer: see [RUNBOOK.md](RUNBOOK.md).
> - **Path B, paid, only after Path A shows an edge.** Yellowstone gRPC feed, multi-sender delivery, live trading, all checked against mainnet (the live programs accepted our Pump and PumpSwap buys and sells, using 75-125k compute units).
> - Regression tests run on real mainnet transactions and reproduce the programs' exact quotes, fees and pool balances. Live validation found and fixed a fee-model error (a buyback share had been added on top of the protocol fee), which is why every number is now checked against chain data.
> - What is and isn't built: [docs/08-feature-parity.md](docs/08-feature-parity.md).

### Quick start (free, paper mode)
```bash
cd engine && cargo build --release -p bot && cd ..
export RPC_URL=https://api.mainnet-beta.solana.com          # public and free
cp config/poc.example.toml config/copybot.toml              # then add wallets under [[leaders]]
./engine/target/release/copybot leader-report <WALLET> ...  # vet them on-chain first
./engine/target/release/copybot check && ./engine/target/release/copybot run
./engine/target/release/copybot report                      # PnL after costs + go / no-go checklist
```

## Read first
| Doc | What's in it |
|---|---|
| [01 — Research landscape (Oct 2026)](docs/01-research-landscape.md) | GMGN, Axiom, Padre/Terminal, BasedBot and Proxima; 2026 infra (ShredStream, gRPC, senders); launchpads; evidence on copy-trading returns |
| [02 — Architecture](docs/02-architecture.md) | Components, latency budget, tech choices |
| [03 — Data model](docs/03-data-model.md) | Every dimension we record, wallet tags, labels |
| [04 — Leader selection](docs/04-wallet-selection.md) | Scoring wallets on *copier* return, gates, lifecycle |
| [05 — Exit engine](docs/05-exit-engine.md) | Rule policies → data-tuned → contextual → dynamic; champion/challenger |
| [06 — Wallets & risk](docs/06-wallets-and-risk.md) | Treasury/hot-wallet fleet, custody, kill switches, MEV |
| [07 — Roadmap](docs/07-roadmap.md) | Phases, go/no-go gates, unit economics, decisions needed |
| [08 — Feature parity](docs/08-feature-parity.md) | What GMGN / Axiom / BasedBot / Proxima do vs what copybot has today |
| [09 — Proxima & BasedBot deep dive](docs/09-proxima-basedbot-deep-dive.md) | Their full documented feature sets, mapped to copybot; what we won't build and why |
| [10 — Engine v2 plan](docs/10-engine-v2-plan.md) | Audit of attempt 1 and of the wallet list, the coin lifecycle state machine (wallets + launches + trending as features), data/census plan, work orders with gates and costs, decisions needed |
| [11 — What decides a trade](docs/11-trading-decisions-data.md) | What the first recorded launches say (pre-funded launches are the $100k coins; no round-number levels on the curve; early breadth), the feature catalogue by layer, the hypotheses and the data each needs, collection gaps, how runners are ridden |
| [12 — Audit of the replays](docs/12-replay-audit.md) | What the replays test and cannot test, where the method was biased toward "no", the hold thesis measured directly; §5 works backwards from the winners: the conventional practices discarded, the earliest signal (net SOL in the curve at 5 s, seven minutes before the lists), and what entering at it pays. The full `copybot winners` report: [reports/winners-2026-10-10.md](docs/reports/winners-2026-10-10.md) |
| [RUNBOOK](RUNBOOK.md) | **Start here.** Path A: free proof of concept. Path B: server, providers, install, check, simulate, shadow, live |

## Layout
```
engine/                     Rust workspace
  crates/core/              pure strategy logic: sizing, exit policies, replay, wallet scoring
  crates/chain/             Solana I/O: Pump.fun, PumpSwap, Meteora DBC, LaunchLab builders/decoders,
                            swap detection for any venue, pool-state reader, durable nonces,
                            free WebSocket feed (wsfeed) and Yellowstone gRPC feed (geyser),
                            Yellowstone gRPC feed, RPC, multi-sender
  crates/bot/               `copybot` binary: engine, execution, wallet keystore, control socket,
                            journal, report, check / simulate / audit / leader-report / discover / bench / wallet tools
  idl/                      official program IDLs (tests verify our code against them)
config/                     poc.example.toml (free paper run), copybot.example.toml (production), engine.example.toml
scripts/                    make_poc_config.py (config from a list of wallets)
deploy/                     setup.sh, systemd unit, docker-compose (Postgres + ClickHouse)
.github/workflows/          ci.yml, paper-run.yml (run the free proof of concept from the Actions tab)
schema/                     ClickHouse market firehose + Postgres journal
docs/                       research and design
tools/dashboards/           cross-asset meme basket dashboard
archive/                    unrelated legacy code (RFO BASIC! Android app)
```

## Develop
```sh
cd engine
cargo test --workspace                 # 107 tests: IDL conformance, real mainnet txs, end-to-end engine
cargo build --release -p bot           # → target/release/copybot
./target/release/copybot bench         # in-process reaction time (~0.17 ms median)
```

Real configs (`config/copybot.toml`), keys and `.env` files are git-ignored. Never commit key material.

## Disclaimer
This is experimental trading software. Memecoins are extremely risky, and published research finds that most copy-trading edge is lost to latency and costs. Nothing here is financial advice. Trade only money you can afford to lose.
