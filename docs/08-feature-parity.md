# 08 — Feature parity: GMGN · Axiom · Padre/Terminal · BasedBot · Proxima vs copybot

An honest status of every feature category those platforms offer, as of this commit.

**Legend**
- ✅ built and covered by tests
- 🔶 built, but needs the live-chain check (`copybot check` / `simulate` / shadow mode)
- 📦 raw data is recorded; the analytics on top are not built yet
- ⬜ not built

## Copy trading (GMGN, BasedBot, Axiom, Padre)
| Feature | Status | Where |
|---|---|---|
| Follow N wallets in real time (gRPC, sub-second) | 🔶 | `chain::geyser`, filters hot-update without reconnect |
| Pre-execution (shred/deshred) leader detection | 🔶 | `geyser::run_deshred`, `detect::pre_exec_pump_buys` (if your provider supports deshred) |
| Copy a % of the leader's size (per-leader override) | ✅ | `sizing.copy_pct`, `[[leaders]] copy_pct` |
| Min/max leader trade size filter (BasedBot) | ✅ | `filters.min/max_leader_buy_sol` |
| Min liquidity filter (BasedBot) | ✅ | `filters.min_pool_sol` |
| Max buy cap (BasedBot "Max Buy") | ✅ | `sizing.max_buy_sol` |
| Token age filter (GMGN) | ✅ | `filters.min/max_token_age_secs` (age known for coins created while running) |
| Platform/venue filter (GMGN) | ✅ | `filters.venues` |
| Slippage settings (buy/sell) | ✅ | `risk.max_buy/sell_slippage_bps` |
| Skip if price already ran / detection too late | ✅ | `max_price_drift_pct`, `max_detection_slot_lag` |
| Copy follow-up buys (adds) | ✅ | `follow_adds`, `max_adds` |
| Copy sells: mirror / sell-all / ignore / tighten | ✅ | `on_leader_sell` per exit policy |
| Dedupe (same leader tx seen on 2 feeds) | ✅ | engine `seen` set |
| Social copy trading (copy X accounts) | ⬜ | needs an X/Twitter feed |

## Exits / automation (all platforms)
| Feature | Status | Where |
|---|---|---|
| Take-profit ladder | ✅ | `take_profit` |
| Stop loss | ✅ | `stop_loss_pct` |
| Trailing stop (with activation level) | ✅ | `trailing` |
| Time stop / stale stop | ✅ | `max_hold_secs`, `stale_secs` |
| Dev-sell protection (BasedBot) | ✅ | `exit_on_dev_sell` |
| Liquidity-pull exit | ✅ | `liquidity_drop_pct` |
| Migration handling (curve → PumpSwap mid-position) | 🔶 | `on_migrated` → canonical pool template |
| Several exit strategies scored side by side on every trade | ✅ | `shadow_exits` + replay at close (beyond what the platforms offer) |
| Limit orders / DCA / sniping new launches | ⬜ | not part of copy-first scope |

## Execution (speed)
| Feature | Status | Where |
|---|---|---|
| Pump.fun curve buy/sell (`buy_exact_quote_in_v2`, `sell_v2`) | 🔶 | `chain::pump`, verified against the official IDL |
| PumpSwap buy/sell (incl. cashback, pool-v2, buyback accounts) | 🔶 | `chain::pump_amm`, matches the official SDK |
| Raydium, Meteora, Orca and other venues | 🔶 | detected for any venue; executed through the Jupiter fallback |
| Multi-sender fan-out (Jito, Helius Sender, Nozomi, 0slot, Astralane, RPC) | 🔶 | `chain::sender`, one signature so it can't double-fill |
| Priority fee + Jito tip, urgent-exit tip boost, retry escalation | ✅ | `[infra.fees]`, `sell()` |
| In-process reaction time | ✅ measured | `copybot bench`: **~0.2 ms** median (decode → size → build → sign) |
| MEV-protected routing | 🔶 | via Jito / Astralane-type senders |

## Wallet management (Proxima, BasedBot)
| Feature | Status | Where |
|---|---|---|
| Encrypted hot wallet (Argon2id + XChaCha20) | ✅ | `copybot wallet new / import` |
| Balances + token accounts | 🔶 | `copybot wallet balance` |
| Sweep profits to a safe wallet | 🔶 | `copybot wallet sweep` |
| Reclaim rent (close empty token accounts) | 🔶 | `copybot wallet close-empty`; also auto-close on full exit |
| Recover open positions after restart | ✅ | engine `adopt` at startup |
| Multi-wallet fleet, rotation, CEX funding, bulk funding | ⬜ | planned ([06](06-wallets-and-risk.md)) |
| Action timeline | 📦 | the JSONL journal is the timeline; no UI yet |

## Token intelligence (GMGN, Axiom Pulse)
| Feature | Status | Where |
|---|---|---|
| Live price, liquidity, buys/sells, volume, curve progress | 📦 | every Pump/PumpSwap trade recorded (`mkt.swaps`) |
| Lifecycle: creation, graduation | 📦 | `CreateEvent` / `CompleteEvent` decoded |
| Holders, top-10 %, dev %, snipers, bundlers, insiders, fresh wallets | ⬜ | feature service planned ([03](03-data-model.md)) |
| Dev history (prior launches / rugs) | 📦 | creator of every coin recorded; aggregation planned |
| Socials (X, Telegram, website), KOL mentions | ⬜ | needs metadata fetch + X API |
| Smart money / KOL tags | ⬜ | `leader-report` covers single wallets; bulk tagging planned |

## Wallet analytics (GMGN wallet pages)
| Feature | Status | Where |
|---|---|---|
| PnL, win rate, median hold, bot detection per wallet | 🔶 | `copybot leader-report` (reads on-chain history) |
| Copier-return simulation (the imitation penalty) | ✅ logic / 📦 data | `wallet_score` + shadow mode measures it live |

## Operations
| Feature | Status | Where |
|---|---|---|
| Operator control: `copybot ctl status / positions / leaders / pause / resume / kill / flatten` | ✅ | `control.rs` (local Unix socket) |
| Kill switch: daily loss, stale feed, manual | ✅ | engine |
| Preflight check with latency | 🔶 | `copybot check` |
| Live-chain dry run without funds | 🔶 | `copybot simulate` |
| Journal (JSONL always; Postgres and ClickHouse optional) | ✅ JSONL / 🔶 DBs | `journal.rs`, `schema/` |
| systemd service, setup script, CI | ✅ | `deploy/`, `.github/workflows/ci.yml` |
| Web dashboard / UI | ⬜ | `copybot ctl` + Grafana on the databases for now |

## Why some items are 🔶
The build sandbox's network policy blocks Solana RPC, gRPC and sender hosts. Everything was therefore verified offline:
- against Pump's **official IDLs and SDK source** (account order, read/write flags, every PDA, every byte layout);
- by end-to-end engine tests on synthetic transactions.

The live-chain checks are the first three steps of [RUNBOOK.md](../RUNBOOK.md): `check`, `simulate`, then shadow mode. They take minutes once the server and endpoints exist.
