# 08 — Feature parity: GMGN · Axiom · Padre/Terminal · BasedBot · Proxima vs copybot

An honest status of every feature category those platforms offer, as of this commit.

**Legend**
- ✅ built and covered by tests
- 🟢 verified on Solana mainnet (accepted by the live program, or decoded correctly on real transactions)
- 🔶 built, but needs the live-chain check on your own infrastructure (`copybot check` / shadow mode)
- 📦 raw data is recorded; the analytics on top are not built yet
- ⬜ not built

## Copy trading (GMGN, BasedBot, Axiom, Padre)
| Feature | Status | Where |
|---|---|---|
| Follow N wallets in real time (gRPC, sub-second) | 🔶 needs a gRPC provider | `chain::geyser`, filters hot-update without reconnect |
| Pre-execution (shred/deshred) leader detection | 🔶 | `geyser::run_deshred`, `detect::pre_exec_pump_buys` (if your provider supports deshred) |
| Copy a % of the leader's size (per-leader override) | ✅ | `sizing.copy_pct`, `[[leaders]] copy_pct` |
| Size by the share of their balance the leader spent (BasedBot "Buy %") / fixed size | ✅ | `sizing.mode = balance_fraction \| fixed`, `copy_amount_sol` |
| Market-cap range, max liquidity | ✅ | `filters.min/max_market_cap_sol`, `max_pool_sol` |
| Token / dev blacklist (live-editable) | ✅ | `filters.blacklist_mints/devs`, `copybot ctl blacklist <address>` |
| One entry per token across leaders (BasedBot "Trade Once Per Token") | ✅ | `filters.one_entry_per_token` |
| Min/max leader trade size filter (BasedBot) | ✅ | `filters.min/max_leader_buy_sol` |
| Min / max liquidity filter (BasedBot) | ✅ | `filters.min_pool_sol`, `max_pool_sol` |
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
| Pump.fun curve buy/sell (`buy_exact_quote_in_v2`, `sell_v2`) | 🟢 | `chain::pump`: official IDL + mainnet `simulateTransaction` accepted (buy 88.8k CU, sell 74.5k CU) |
| PumpSwap buy/sell (incl. cashback, pool-v2, buyback accounts) | 🟢 | `chain::pump_amm`: official SDK + mainnet simulation accepted (buy 91.3k CU, sell 74.3k CU). Reversed (SOL-base) pools are detected and routed via Jupiter |
| Meteora DBC (Bags, Jupiter Studio, Believe…) direct | 🟢 detect / 🔶 send | `chain::meteora_dbc`: IDL from Meteora's SDK; real mainnet trades decoded into a direct template |
| Raydium LaunchLab (LetsBONK) direct | 🟢 detect / 🔶 send | `chain::raydium_launchlab`: IDL + Raydium SDK v2; all 18 accounts match live mainnet instructions |
| Other venues (Raydium AMM/CPMM/CLMM, Meteora DAMM/DLMM, Orca) | 🟢 | detected for any venue from balance changes; executed through Jupiter (`/swap/v1`, v0 tx simulated on mainnet) |
| Multi-sender fan-out (Jito, Helius Sender, Nozomi, Astralane, RPC) | 🔶 | `chain::sender` groups services by tip family; one variant per family on a shared durable nonce (`chain::nonce`, byte-checked against `solana-system-interface` / `solana-nonce`), so only one can land; expired orders are cancelled by advancing the nonce. Helius tip accounts taken verbatim from `helius-sdk` 3.2.0 |
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
| Multi-wallet fleet, folders, funders, bulk funding with delays (Proxima) | ⬜ | next item; see [09](09-proxima-basedbot-deep-dive.md) |
| Action timeline | 📦 | the JSONL journal is the timeline; no UI yet |

## Token intelligence (GMGN, Axiom Pulse)
| Feature | Status | Where |
|---|---|---|
| Live price, liquidity, buys/sells, volume, curve progress | 📦 | every Pump/PumpSwap trade recorded (`mkt.swaps`) |
| Lifecycle: creation, graduation | 📦 | `CreateEvent` / `CompleteEvent` decoded |
| Holders, top-10 %, dev %, snipers, bundlers, insiders, fresh wallets | 🔶 | GMGN OpenAPI (`token/info` + `token/security`) logged with every copy decision; optional entry gate; `copybot token-intel` |
| Dev history (prior launches / rugs) | 🔶 | GMGN `creator_open_count`, creator ATH, CTO flag, `created_tokens` |
| Socials (X, Telegram, website), DexScreener ad/boost, X renames | 🔶 | GMGN `token/info` link + dev fields |
| Smart money / KOL tags and live trades | 🔶 | `copybot discover` (GMGN `smartmoney` / `kol` feeds + wallet stats) |

## Wallet analytics (GMGN wallet pages)
| Feature | Status | Where |
|---|---|---|
| PnL, win rate, median hold, bot detection per wallet | 🔶 | `copybot leader-report`: on-chain history + GMGN track-record / copy-tradeability score (ported from GMGN's own scoring) |
| Copier-return simulation (the imitation penalty) | ✅ logic / 📦 data | `wallet_score` + shadow mode measures it live |

## Operations
| Feature | Status | Where |
|---|---|---|
| Operator control: `copybot ctl status / positions / leaders / pause / resume / kill / flatten / blacklist` | ✅ | `control.rs` (local Unix socket) |
| Kill switch: daily loss, stale feed, manual | ✅ | engine |
| Preflight check with latency | 🟢 | `copybot check` (mainnet RPC 47 ms from the build sandbox; Jito tip accounts fetched live) |
| Live-chain dry run without funds | 🟢 | `copybot simulate [--sell]` |
| Decoder audit on live chain | 🟢 | `copybot audit --program pump\|pumpswap\|dbc\|launchlab`: decodes recent real trades and cross-checks amounts against balance changes (Pump 10/10, PumpSwap 20/20 agree) |
| Version-1 transactions (new mainnet format, compute budget in `transactionConfig`) | 🟢 | `rpc` requests `maxSupportedTransactionVersion: 1`; real v1 tx in the fixtures |
| Journal (JSONL always; Postgres and ClickHouse optional) | ✅ JSONL / 🔶 DBs | `journal.rs`, `schema/` |
| systemd service, setup script, CI | ✅ | `deploy/`, `.github/workflows/ci.yml` |
| Web dashboard / UI | ⬜ | `copybot ctl` + Grafana on the databases for now |

## Sources of truth
- **GMGN:** official `gmgn-cli` (npm 1.6.6) and the `GMGNAI/gmgn-skills` repo define the OpenAPI routes, auth and fields the client uses.
- **BasedBot:** no public repo or SDK. Its full GitBook docs (`docs.basedbot.app/llms-full.txt`) define the copy-trading controls; see [09](09-proxima-basedbot-deep-dive.md).
- **Proxima:** no public code. Its complete docs (`docs.proxima.tools/llms-full.txt`) define its wallet and execution features; see [09](09-proxima-basedbot-deep-dive.md).
- **Pump.fun / PumpSwap:** official IDLs + `@pump-fun/pump-sdk` / `pump-swap-sdk` source.
- **Meteora DBC:** IDL embedded in `@meteora-ag/dynamic-bonding-curve-sdk` 1.5.13 + its `swap()` builder.
- **Helius Sender:** endpoints, tip accounts and minimum tips from the official `helius-sdk` 3.2.0.
- **Raydium LaunchLab:** `raydium-io/raydium-idl` + `@raydium-io/raydium-sdk-v2` 0.2.73 (`launchpad/instrument.ts`, `pda.ts`, curve math).

## Why some items are still 🔶
Mainnet verification (Oct 2026) covered everything that needs only RPC: instruction acceptance, decoding real trades, Jupiter routing, preflight. What remains needs your own paid infrastructure:
- the live gRPC feed (Yellowstone / LaserStream) and deshred stream;
- landing through the senders (Jito, Helius Sender, …) with real tips;
- the Postgres and ClickHouse sinks.

Those are steps 5–8 of [RUNBOOK.md](../RUNBOOK.md): `check`, then shadow mode, then small live size.
