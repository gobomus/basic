# Solana memecoin copy-trading engine → self-calibrating trading engine

A high-frequency copy-trading engine for Solana memecoins, built in three steps:

1. **Copy selected leader wallets.** Enter with a percentage of each leader's size, at their entry.
2. **Exit on our own, data-tuned logic.** We don't wait for the leader to sell.
3. **Record everything** that terminals like GMGN, Axiom and Padre show (volume, transactions per second, holders, snipers, bundlers, dev history, socials, …) plus our own execution data. The resulting trade log tunes exits and sizing first, and later a **leader-free, self-adjusting engine**.

> Status: **Phase 0 (foundation)**.
> - Done: design, research, schemas, and the tested strategy core.
> - Not built yet: live feeds, decoders, and execution.
> - See [docs/07-roadmap.md](docs/07-roadmap.md).

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

## Layout
```
engine/              Rust workspace — crates/core = pure strategy logic (sizing, exits, replay, wallet scoring)
config/              engine.example.toml (validated by the test suite)
schema/clickhouse/   market firehose + point-in-time token snapshots
schema/postgres/     journal: leaders, signals (incl. skips), positions, orders, shadow exits, wallets
docs/                research + design
tools/dashboards/    cross-asset meme basket dashboard (GeckoTerminal OHLCV)
archive/             unrelated legacy code (RFO BASIC! Android app), kept for history
```

## Develop
```sh
cd engine && cargo test        # engine-core unit tests (also validates config/engine.example.toml)
```

Real configs (`config/engine.toml`), keys and `.env` files are git-ignored. Never commit key material.

## Disclaimer
This is experimental trading software. Memecoins are extremely risky, and published research finds that most copy-trading edge is lost to latency and costs. Nothing here is financial advice. Trade only money you can afford to lose.
