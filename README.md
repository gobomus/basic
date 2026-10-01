# Solana memecoin copy-trading engine → self-calibrating trading engine

A high-frequency copy-trading engine for Solana memecoins, built in three steps:

1. **Copy selected leader wallets.** Enter with a percentage of each leader's size, at their entry.
2. **Exit on our own, data-tuned logic.** We don't wait for the leader to sell.
3. **Record everything** that terminals like GMGN, Axiom and Padre show (volume, transactions per second, holders, snipers, bundlers, dev history, socials, …) plus our own execution data. The resulting trade log tunes exits and sizing first, and later a **leader-free, self-adjusting engine**.

> **Status: engine built; next step is the live-chain verification on your server.**
> - Built and tested offline (61 tests):
>   - the live engine, real-time feed, Pump.fun and PumpSwap execution, multi-sender delivery, exits, risk controls, wallet tools, local control (`copybot ctl`) and journal;
>   - all of it checked against Pump's official program definitions and SDK.
> - Not yet run against mainnet: this build environment can't reach Solana.
> - **Start with [RUNBOOK.md](RUNBOOK.md)** and see what is and isn't built in [docs/08-feature-parity.md](docs/08-feature-parity.md).

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
| [RUNBOOK](RUNBOOK.md) | Step-by-step: server, providers, install, check, simulate, shadow, live |

## Layout
```
engine/                     Rust workspace
  crates/core/              pure strategy logic: sizing, exit policies, replay, wallet scoring
  crates/chain/             Solana I/O: Pump.fun + PumpSwap builders/decoders, swap detection,
                            Yellowstone gRPC feed, RPC, multi-sender
  crates/bot/               `copybot` binary: engine, execution, wallet keystore, control socket,
                            journal, check / simulate / leader-report / bench / wallet tools
  idl/                      official Pump IDLs (tests verify our code against them)
config/                     copybot.example.toml (production template) + engine.example.toml
deploy/                     setup.sh, systemd unit, docker-compose (Postgres + ClickHouse)
schema/                     ClickHouse market firehose + Postgres journal
docs/                       research and design
tools/dashboards/           cross-asset meme basket dashboard
archive/                    unrelated legacy code (RFO BASIC! Android app)
```

## Develop
```sh
cd engine
cargo test --workspace                 # 61 tests, incl. IDL conformance + end-to-end engine
cargo build --release -p bot           # → target/release/copybot
./target/release/copybot bench         # in-process reaction time (~0.17 ms median)
```

Real configs (`config/copybot.toml`), keys and `.env` files are git-ignored. Never commit key material.

## Disclaimer
This is experimental trading software. Memecoins are extremely risky, and published research finds that most copy-trading edge is lost to latency and costs. Nothing here is financial advice. Trade only money you can afford to lose.
