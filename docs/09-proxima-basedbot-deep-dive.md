# 09 — Proxima and BasedBot: deep dive (Oct 2026)

**Sources:** each platform's complete published documentation, read in full on 2026-10-02:
- Proxima: `docs.proxima.tools/llms-full.txt`, the export of every docs page (~950 KB, English plus translations).
- BasedBot: `docs.basedbot.app/llms-full.txt` (GitBook export) and its page index.

Neither platform publishes source code, an SDK or its on-chain program IDs. BasedBot's site and docs block non-browser clients, but its GitBook export is open. Their backends are closed, and an account would show only their web UI, not their code. Their *documented behaviour* is the most precise public description of what they do.

## What each platform is
- **BasedBot**: a Telegram bot plus web terminal for retail trading on 8+ chains. Copy trading, TP/SL/trailing, dev-sell, limit orders, DCA, migration sniping, social (X) copy trading, wallet tracker.
- **Proxima**: a token *launch and market-operations* console for launchers on 29 launchpads. Bundled launches (Block-0), wallet fleets, funders, CEX-routed funding, auto buy/sell, volume bot and "generate activity", a wallet marketplace, and "Shield", a per-account custom router.

## Execution mechanics they document
| Mechanic | Platform | copybot |
|---|---|---|
| Send each tx through several providers in parallel ("Provider Racing") | Proxima | ✅ multi-sender fan-out. We go further: one variant per tip family on a shared **durable nonce**, so at most one lands |
| Per-account custom on-chain router ("Shield", needs a 7 SOL funder to deploy) | Proxima | ⬜ candidate, see below |
| Skip pre-send checks for speed ("Pro Mode", "Turbo") | BasedBot | ✅ by design: instruction templates are prebuilt from the leader's own tx, with no quote, simulation or RPC round-trip on the hot path (~0.2 ms in-process) |
| MEV protection | BasedBot (EVM only; not offered on Solana) | ✅ Jito / Sender bundles with tips |
| Parallel multi-wallet buys in separate bundles, or staggered | Proxima | ⬜ needs the wallet fleet (next item) |

## Copy-trading controls (BasedBot `/copytrading`) vs copybot
| BasedBot control | copybot |
|---|---|
| Copy amount; **Buy %** = your amount × (leader spend ÷ leader balance) | ✅ **new:** `sizing.mode = "balance_fraction"` + `copy_amount_sol`. The leader's pre-trade balance is read from the same transaction (no extra RPC) |
| **Buy Exact** (mirror the leader's SOL amount) | ✅ `mode = "leader_pct"`, `copy_pct = 1.0` |
| Fixed amount per copy | ✅ **new:** `mode = "fixed"` |
| **Sell %** (sell the share the leader sold) | ✅ `on_leader_sell = "mirror"` |
| Min/Max liquidity | ✅ `min_pool_sol` + **new** `max_pool_sol` |
| Min/Max trade amount (leader's trade) | ✅ `min/max_leader_buy_sol` |
| Max Buy (cap, still executes) | ✅ `max_buy_sol` (plus position, exposure, pool-impact and balance caps) |
| Market-cap check | ✅ **new:** `min/max_market_cap_sol` (Pump.fun / PumpSwap, where supply is fixed) |
| Trade Once Per Token | ✅ **new:** `one_entry_per_token` |
| Trade Once (auto-unfollow) | ⬜ low value for a curated leader set |
| Blacklisted tokens / devs | ✅ **new:** `blacklist_mints` / `blacklist_devs` + live `copybot ctl blacklist <address>`, persisted across restarts |
| Skip filters on sells | ✅ always: exits never pass through entry filters |
| Delay (ms) before copying | ⬜ on purpose: it costs entry price. Shadow exits measure the effect of waiting instead |
| Auto-Order Template (TP/SL/trailing/dev-sell on every buy) | ✅ exit policies per leader; several shadow policies scored on every trade |
| Migration buy/sell | ✅ `on_migrated` handling; migration *sniping* is outside copy scope |

## Wallet management (Proxima) vs copybot
| Proxima | copybot |
|---|---|
| Create up to 100 wallets, folders, labels, archive | ⬜ **next:** wallet fleet (one keystore holding N wallets, groups, per-group leaders/limits) |
| Funders: reusable funding wallets, set default, withdraw, export key | 🔶 `wallet sweep` (single wallet). Fleet funding comes with the fleet |
| Fund wallets: fixed or range amounts, delay between fundings | ⬜ with the fleet |
| Withdraw (Max, Leave Dust) | ✅ `wallet sweep --keep` |
| Clean Dust / Recover residual funds | ✅ `wallet close-empty` (rent). ⬜ selling residual token dust before closing |
| Nuke (sell all from all wallets, optionally consolidate first) | ✅ `ctl flatten` (one wallet) |
| Tracker: labelled addresses; import/export (CSV, JSON, Axiom/GMGN formats) | 🔶 `[[leaders]]` with labels. ⬜ import/export |
| Dashboard: cumulative and daily P&L, win rate, P&L calendar | 📦 every fill and close is in the journal. ⬜ report command |
| Auto Buy/Sell: DCA, market-cap limit orders, mcap range, end conditions | ⬜ outside copy scope for now; exits cover the sell side |

## What we deliberately do not build
Several Proxima features exist to make activity *look* organic to other traders and to on-chain analytics:
- **Volume Bot** and **Generate Activity**: buy-and-sell loops, including same-block buy+sell bundles.
- The aged-wallet **Marketplace**, filtered so wallets avoid "time-linked" badges and Bubble Maps clustering.
- **Counter Trading**, which is pitched as making selling "less obvious".
- CEX-hop funding chosen to break wallet links.

That is wash trading and concealment aimed at other market participants. It is market manipulation, so it is not part of this engine. The legitimate parts (fleet management, funding, dust recovery, multi-provider sending) are on the list above.

## Candidate: a private on-chain router (Proxima "Shield")
A small program of our own that wraps each swap could:
- abort cheaply when the pool has already moved past our limit (checked on-chain against live reserves, not a stale quote);
- enforce a max slot, so a late landing fails instead of buying the top;
- make the tip conditional on the swap succeeding.

**Costs:**
- program deployment rent (several SOL);
- an audit-grade test suite;
- extra compute per swap.

**Decision:** build it only if shadow and live data show losses from late or moved-price fills that slippage limits don't already stop. The journal records everything needed to measure that (`slot_lag`, `leader_price` vs fill price).
