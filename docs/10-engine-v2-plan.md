# 10 — Engine v2 plan: wallets, launches and trending in one state machine

Written 2026-10-08 after reading the four parallel engine studies in Notion (GLM 5.3 Flash, Kimi K3, GLM 5.3, DeepSeek), vetting the "PROFITABLE WALLETS" list on-chain, and probing which data sources work for free. Numbers in this document were measured on that day unless a source is named. SOL was $111.6.

**In one paragraph.** Attempt 1 (`copybot`) is a working execution engine with verified decoders, honest paper fills, exit replay and a report, but it has no eyes: it only sees the wallets it is told to follow, 1–6 s late, on a free feed. The four Notion studies are the opposite: research and census pilots with real findings and no execution. v2 joins the two. The engine keeps a lifecycle state for every coin it sees (new → curve → migrated → trending → dead) and treats the three signal sources, tracked wallets, launch microstructure and trending flow, as *features of that state*, not as triggers. Every snapshot and every decision is recorded with outcome labels computed from our own tape, so the engine's rules can be tuned on our own data and later replaced by learned models. Capital follows pool depth: the curve tier takes 0.1–1 SOL positions, the trending tier takes $1k–10k positions, and that is where a $10–100k book actually fits.

The plan is staged so that each step can prove itself cheaply before the next costs money. The decisions needed from you are at the end.

---

## 1. Audit of attempt 1 against the v2 goal

### 1.1 What exists and is verified

| Capability | Status | Evidence |
|---|---|---|
| Decoders for Pump.fun curve, PumpSwap, Meteora DBC, Raydium LaunchLab, generic swaps | ✅ | 107 tests; quotes, fees and pool balances reproduced on real mainnet transactions (`engine/crates/chain/tests`) |
| Copy engine: sizing, filters, risk, exits, kill switches, control socket | ✅ | `engine/crates/bot`, `copybot ctl` |
| Honest paper fills (land 1.2 s later at the then-current pool, slippage floor, fees charged) | ✅ | live-validated 2026-10-02; found and fixed a fee bug and a stablecoin-copy bug |
| Exit replay: every exit policy scored on every recorded price path | ✅ | `engine-core::replay`, "replay check" line in the report |
| Journal + `copybot report` with a go/no-go checklist | ✅ | bootstrap CI, per-leader table, alternative exits |
| Free data feed (RPC WebSocket + account polling) | ✅ but slow | 1–6 s behind the leader; public endpoint recycles connections every 10–20 min |
| Wallet vetting (`copybot leader-report`) | ✅ | realized PnL, hold time, bot check from the last N transactions |
| GitHub Actions runner (no server) | ✅ | one 15-minute run succeeded on 2026-10-02 |
| Live order path (senders, Jito tips, keystore) | ⚠️ written, never used with money | mainnet simulation only |
| gRPC (Yellowstone) feed | ⚠️ written, never run | no provider account |
| ClickHouse/Postgres schemas | ⚠️ designed, no writer | `schema/` |

### 1.2 What the v2 goal needs that attempt 1 does not have

| Need | Attempt 1 | Gap |
|---|---|---|
| See every launch, not just tracked wallets' trades | No | a launch census (free: PumpPortal + Jupiter; slot-level: gRPC) |
| See what is trending and why | No | a trending census (free: Jupiter trending/organic, DexScreener boosts) |
| Feature snapshot per coin at fixed checkpoints (15 s, 60 s, 5 m, 1 h, 24 h) | Designed in [03](03-data-model.md), not computed | snapshot writer + Parquet store |
| Outcome labels from our own tape (peak multiple, graduated, dead) | No | nightly labeler |
| Classify wallets as bots / traders / holders before trusting a leaderboard | Partly (`leader-report` bot check) | automated classifier; see §2 |
| Daily "top 10 launches / top 20 trending" tables with their early snapshots | No | the training table for "what did winners look like early" |
| Coin lifecycle state machine with per-state gates and sizes | No (one filter set for everything) | §4 |
| Position sizing by pool depth | Partly (`max_pool_impact_pct`) | per-state size bands |
| Sub-second detection | No | paid feed (§6) |
| Learned entry models | No | after ≥ 2 weeks of labelled tape |

### 1.3 How attempt 1 compares with the four Notion engines

| | copybot (this repo) | GLM 5.3 Flash | Kimi K3 | GLM 5.3 | DeepSeek V4.1 |
|---|---|---|---|---|---|
| Execution engine (orders, exits, risk) | **yes** | no | no | no | no |
| Decoders verified on real transactions | **yes** | partly | IDL-verified, "never compiled" | pilot decode | 35% of states failed its own invariant |
| Launch census recorder | no | **yes** (PumpPortal + RPC, hash-verified archive) | scaffold | no | **yes** (40 min, 1,656 launches) |
| Wallet classification | hold-time bot check | no | no | **yes** (11 of 20 are bots) | no |
| Measured findings | fee model, fills, latency | PumpPortal 310 ms ahead of public RPC; 430–530 pump events/s | 312 tx/s, 65.7% failed on the pump program | public WSS 10.6 s late; 403s from datacenter IPs | curve ceiling 14.7x; holders@15 s is the strongest early signal |
| Learned model results | none | AUC 0.637 on vendor fields (build 2) | — | — | — |

None of the four has an engine; this repo has no census. The findings below are adopted as inputs; the engine stays the runtime.

**Findings adopted (with their source):**
- The Pump.fun curve starts at 28 SOL market cap ($3.1k), fills at exactly 85.005 SOL of real reserves (411 SOL market cap, $45.9k) and therefore **caps at 14.7x**. 100x only exists after graduation, on the AMM. (DeepSeek study, 88,335 curve states.)
- **Holder count at 15 s is monotone in outcome**: 2.8% of coins with one holder double again, 21% with ≥ 30 holders (10.6x lift). **Buyer concentration (HHI ≥ 0.8) at 15 s → 0% double again.** The "dev sold" rule does not work (2.95% vs 2.02%). Requiring 5 wallets in the first 5 s throws away 8 of the 18 ten-x coins. (DeepSeek.)
- Junk peaks inside the create slot (245 of 405 coins); 10x coins peak at a median of 72 s with 65% of the move still buyable after the create slot. "There is time for the big ones. There is none for the small ones." (DeepSeek.)
- 37% of "pump.fun" creates come from third-party rails using the pump program (no `pump` suffix). (DeepSeek.)
- The pump program runs 312–530 events/s with ~2/3 failed transactions; polling it over RPC is impossible; a server-side-filtered stream (gRPC) is the only ingest shape that scales. (Kimi, GLM Flash; confirmed by our own 141 tx/s single-wallet bursts.)
- PumpPortal's free `subscribeNewToken`/`subscribeMigration` beats the public RPC by ~310 ms median and is the strongest free launch feed. (GLM Flash; confirmed today: 17 creations in 45 s ≈ 32,600/day.)
- Vendor features bought almost nothing: 119 GMGN fields → AUC 0.637 vs 0.637 for the original 18. Sequence features alone 0.519. Outcome coverage was selection-biased (1.9% candle coverage in the top liquidity decile vs 87% in the bottom). **Labels must come from our own tape.** (Build 2.)
- GMGN REST is 5–9 s per call: enrichment only, never in a decision path. (Build 2, GMGN study.)
- You cannot front-run on Solana (no mempool); the achievable thing is first-following at slot N+1 with shreds/gRPC, or being earlier *in the coin's life* on the same setup the wallet will buy later ("pre-arrival"). Slots are going from 400 ms to 200 ms. (DeepSeek, GLM 5.3.)
- Evidence discipline: raw bytes archived before parsing, hashes, receipt timestamps, no backfilling of later knowledge into earlier decisions, chronological splits, thresholds frozen before scoring. (All four; we keep it.)

---

## 2. Audit of the wallet list (the "$100k/day" claim)

### 2.1 Method
For each wallet: the last 1,000 transactions from the public RPC (time span, transactions per day, share that failed, peak per second), SOL balance, and `copybot leader-report` on the last 300–400 transactions (decoded swaps, round trips, realized PnL after fees, median hold). Vendor numbers come from the GMGN `wallet_profits` fields saved in `solana_wallet_watchlist.json` (2026-10-03) and are marked as such.

### 2.2 The 16 "PROFITABLE WALLETS" (Notion)

Final classification from `copybot wallet-audit` (§2.4), 2026-10-08 evening, last 1,000 own transactions per wallet.

| Wallet | Class | Evidence | Copyable? |
|---|---|---|---|
| decu, truonest, theo, beanzol, megga, dv, daumen | bot | 1,000 transactions in seconds, 93–99% failed, 116–825 per second | no |
| orange | bot | 85% failed, bursts of 103 per second; 5 decodable round trips | no |
| bennytradez | sniper | +46.7 SOL over 168 trips, median hold **5 s** | no |
| LuckBlueParsley | sniper | +89.4 SOL over 96 trips, 97% win, median hold **3 s** | no |
| FoulMidiPumper | sniper | +52.6 SOL over 70 trips, 99% win, median hold **4 s** | no |
| slyorca69467 | sniper | +40.7 SOL over 72 trips, 97% win, median hold **6 s** | no |
| cupsey | unclear | 6 decodable round trips in 20 h, −12.3 SOL | no |
| **mofo chad** | **trader** | **+87.2 SOL over 230 trips in 20 h, 74% win, profit factor 4.1, median hold 42 s** | **leader (fast)** |
| skarraOG | trader | +24.7 SOL over 221 trips in 20 h, 39% win, profit factor 1.40, hold 176 s | watch (just under the 1.5 profit-factor bar) |
| early biddy | trader | −33.1 SOL over 116 trips in 11 h (it was +133 SOL in the 32 h before) | reject this week |

Nine of sixteen are machine-gun bots: they fire 100+ transactions per second at every new coin and 93–99% fail. Four more are snipers that are very profitable *because* they hold for 3–6 seconds: they buy in the creation block and sell to the copy herd seconds later (97–99% win rates, +40 to +89 SOL a day). Nobody can copy that; following them means being the herd they sell to. GLM 5.3's pilot reached the same split two days earlier. **One wallet from the Notion list qualifies as a leader this week: mofo chad.**

### 2.3 The 8 GMGN-sourced watchlist wallets (the six-figure days)

| Wallet | tx/day | failed | peak tx/s | SOL balance | GMGN realized 1 d / 7 d / 30 d (vendor) | Class |
|---|---:|---:|---:|---:|---|---|
| MfDuWeq… | 477,000 | 34% | 75 | 97 | −$47k / $779k / **$6.03M** | MEV/arbitrage bot (349k buys per week) |
| EnaatNf… | 8 | 1% | 3 | 2.6 | $521k / $510k / $3.44M | holder distributing a position (0 buys in 7 d) |
| 3XrqaGd… | 7 | 0% | 2 | 0.8 | $0 / $398k / $1.23M | holder distributing (0 buys in 7 d) |
| EC2f5Dn… | 118 | 0% | 2 | 0.0 (sweeps out) | **$62k / $93k / $875k** | decision trader, hours-scale holds |
| 8deJ9xe… | 272 | 17% | 28 | 274 | **$53k / $135k / $565k** | active trader, 1,747 buys per week |
| 9qgh1ed… | 216 | 50% | 1 | 0.1 | $18 / $119k / $123k | trader, half of sends fail |
| 9WhiKDT… | 370 | 4% | 3 | 15 | $824 / $1.4k / $5.5k | small trader |
| 8yspHpQ… | 34 | 11% | 2 | 11 | $391 / $4.8k / $7.9k | small trader |

**What this says about "$100k/day".** The headline numbers come from three kinds of wallet: (1) an arbitrage bot doing half a million transactions a day, (2) large holders selling into strength, and (3) two or three real traders (EC2f5, 8deJ9) with $50–60k realized days and ~$0.5–0.9M months, **as reported by the vendor**, holding for hours on migrated coins with liquidity. Only group (3) is a template for an engine, and their numbers are unverified vendor figures from one window (our own file says so: "net PnL and copying unverified"). The on-chain check of those two is in §2.4.

### 2.4 On-chain realized PnL (our decoder, fees included)

`copybot wallet-audit --limit 1000` on PublicNode, 2026-10-08 ~21:00 UTC: each wallet's last 1,000 **own** transactions (transfers and fee payouts sent to it by others are skipped), every one read (0 refused after retries). For active traders that is about **20 hours**, so these are one-day slices, not monthly rates. SOL at $111.6.

| Wallet | Source | Verdict | Round trips | Realized PnL | Hours | Win | Profit factor | Median hold |
|---|---|---|---:|---:|---:|---:|---:|---:|
| **mofo chad** | Notion | **LEADER** | 230 | **+87.2 SOL ($9.7k)** | 20.4 | 74% | 4.1 | 42 s |
| **kol-CkPFG** | starter | **LEADER** | 57 | **+35.1 SOL ($3.9k)** | 17.7 | 56% | 3.0 | 84 s |
| **kol-4Ddrf** | starter | **LEADER** | 41 | **+16.2 SOL ($1.8k)** | 20.0 | 63% | 16.9 | 31 s |
| **9WhiKDT…** | watchlist | **LEADER** | 88 | **+5.4 SOL** | 20.3 | 52% | 1.7 | 172 s |
| skarraOG | Notion | watch | 221 | +24.7 SOL | 20.3 | 39% | 1.40 | 176 s |
| kol-CCUcj | starter | watch | 15 | +10.3 SOL | 8.2 | 87% | — | 384 s |
| kol-AEeJU | starter | watch | 11 | +4.1 SOL | 20.4 | 55% | — | 35 min |
| early biddy | Notion | reject | 116 | −33.1 SOL | 10.8 | 53% | <1 | 120 s |
| LuckBlueParsley | Notion | reject (sniper) | 96 | +89.4 SOL | 19.8 | 97% | 276 | 3 s |
| FoulMidiPumper | Notion | reject (sniper) | 70 | +52.6 SOL | 9.1 | 99% | 193 | 4 s |
| bennytradez | Notion | reject (sniper) | 168 | +46.7 SOL | 20.2 | 58% | — | 5 s |
| slyorca69467 | Notion | reject (sniper) | 72 | +40.7 SOL | 18.2 | 97% | — | 6 s |
| 8deJ9xe… | watchlist | reject (holder) | 5 | −29.9 SOL | 19.0 | — | — | — |
| EC2f5Dn…, 9qgh1ed…, 3XrqaGd… | watchlist | watch (unclear) | 0 | — | — | — | — | 85–100% of their transactions are sent to them by others; no decodable trades of their own in the window |
| EnaatNf… | watchlist | watch (unclear) | — | — | — | — | — | the RPC returned no history |
| MfDuWeq… | watchlist | reject (bot) | — | — | — | — | — | 207,000 transactions/day |

**Corrections to the first version of this section.** The numbers first published here (early biddy +133 SOL, mofo chad +74, skarraOG +26, LuckBlueParsley −100% median trip, FoulMidiPumper −20.8 SOL, bennytradez −14.9) came from `leader-report` before two decoder bugs were found and fixed while building WO-1: (1) curves priced in another token were read in that token's units as if they were SOL, and (2) wallets' histories were full of transactions sent *to* them, which pushed their own trades out of the window. Both are fixed and covered by tests on the real transactions. Separately, early biddy's swing from +133 SOL (32 h) to −33 SOL (11 h) is real: single-day slices of one trader vary that much, which is why the audit re-runs weekly and why a leader needs 30+ round trips.

**Reading.** The verifiable pace of the best copyable trader on the list is about **+87 SOL/day ($9.7k)**, earned with 230 round trips at a 42 s median hold. The most profitable wallets overall (+40 to +89 SOL/day) are 3–6 s snipers that no follower can copy. The six-figure vendor numbers remain unverified: the wallets behind them are a bot, a holder, or show no trades of their own in the window. **Nothing we can verify trades its way to $100k/day; one copyable trader does about $10k/day on a good day.**

### 2.5 Consequences
- Drop the bots and snipers from any leader list. Following them is paying their exit. This week's leaders, written by the audit to `config/leaders.local.toml`: **mofo chad, kol-CkPFG, kol-4Ddrf and 9WhiK**; skarraOG, kol-CCUcj and kol-AEeJU on watch.
- Even the leaders are **fast**: 31 s–3 min median holds. On a 1–6 s feed we land inside their hold window; a 35 s hold with a 3 s delay is already 10% of the trade gone. Copying them needs the paid feed (WO-3/WO-5), and the pre-arrival framing (learn their setups, be there earlier) matters more than the copy itself.
- A leaderboard (kolscan, GMGN) is a *candidate source*, never a leader list: every wallet goes through the classifier and the on-chain PnL check first, and again every week (strategies decay). Vendor PnL figures are not evidence until our decoder reproduces them.
- The "$100k/day" target is not supported by anything we can verify; **about $10k/day by one trader with 230 fast round trips is.** An engine that reproduces *that* on several wallets' worth of setups, with size scaled by pool depth, is the realistic version of the goal; the rest is compounding.
- The capital question answers itself from the tier the real earners trade in: curve coins and freshly migrated coins for the fast traders (small positions, many trades), and migrated coins with $100k–$5M liquidity for the hours-scale holders ($5–50k positions). The curve tier is where the bots live and where $10k cannot even be deployed (the whole curve is $9.5k of SOL).

---

## 3. Reality constraints that shape the design

| Constraint | Number | Consequence |
|---|---|---|
| Launch rate | 32,600/day measured today (PumpPortal); 42–60k/day in the studies; 263k SPL tokens on the record day | the census must be streaming, not polled |
| Pump curve economics | start $3.1k mcap, full at 85 SOL ($9.5k) = $45.9k mcap, ceiling 14.7x | curve positions are 0.1–1 SOL; 100x only after graduation |
| Graduation rate | 0.2–1.4% (studies); 5.5% bonded within 20 min in the 40-min sample | graduation is a base rate, not a filter |
| Early signal | holders@15 s: 2.8% → 21% double-again across 1 → 30 holders; HHI ≥ 0.8 → 0% | needs trade-level data in the first minute, not 5-minute polling. *Update 2026-10-08:* the public RPC's `logsSubscribe` on the pump program delivers every trade for free (WO-3 status below); gRPC is only needed for sub-second speed |
| Speed ladder | shreds → gRPC +33 ms → confirmed APIs +1–3 s → terminals +2–10 s → Telegram alerts +10 s–min; our free feed 1–6 s | free feed = "confirmed-data crowd"; to be a first follower the engine needs gRPC on a nearby server |
| Slot time | 400 ms → 200 ms (Agave 4.2) | budgets written for 200 ms |
| Cost floor | pump curve 1.25% per side, PumpSwap 0.25–0.30%, priority fee + tip 0.001–0.005 SOL | a 0.1 SOL copy needs ≈ 4.5% to break even; a 1 SOL copy ≈ 2.7% |
| Copier penalty | academic result: copier returns negative on average even with accurate wallet identification | copying is a signal source, not the business |
| Trader base rate | 6.25% of 304k active memecoin wallets profitable over 90 days; 88% of those made < $100 | leader lists must be re-vetted continuously |
| Trending-tier depth | top-20 trending today: median liquidity $408k (min $14k, max $30M) | a 1%-of-pool position is ~$4k on a median trending coin; this is the tier for a $10–100k book |
| Free RPC behaviour | PublicNode: 403 from datacenter IPs, −32005 limits; public endpoint ~4 req/s per method; WebSocket recycled every 10–20 min | free data is for census and research; a Helius free key is the minimum for anything else |

---

## 4. Target architecture: one state machine, three signal sources

### 4.1 Coin lifecycle (the state machine)

Every coin the engine sees gets a state, and the state decides which features are computed, which gates apply, how big a position may be and which exit policy runs.

| State | Enter when | Leave when | Position band | Primary features |
|---|---|---|---|---|
| `SEEN` | create event (PumpPortal / gRPC / RPC logs) | first trade or 60 s idle | none | creator history, launchpad/rail, metadata, initial buy |
| `CURVE_EARLY` (0–120 s) | first trade | 120 s | 0.1–0.5 SOL | holders@15/30/60 s, unique-buyer slope, HHI, net inflow per 1 s bucket, sniper/dev exits |
| `CURVE_RUN` | ≥ 30 holders and rising | progress ≥ 70% or decay | 0.3–1 SOL | buyer breadth, two-sided turnover, tracked-wallet buys, curve progress |
| `NEAR_BOND` (≥ 70% of 85 SOL) | progress threshold | migration or stall | 0.3–1 SOL | time-to-bond estimate, bonding flow, pre-migration selling |
| `MIGRATED_FRESH` (0–30 min on the AMM) | `Complete`/`migrate` event | 30 min | 0.5–3 SOL | opening-candle multiple, liquidity, holder growth, tracked-wallet entries |
| `ESTABLISHED` (≥ 30 min, liquidity ≥ $50k) | age + depth | trending rank or fade | 1–20 SOL, ≤ 1% of pool | organic buy/sell volume, net buyers, holder change, liquidity change, age, top-10 concentration |
| `TRENDING` (ranked top-N by organic flow) | rank entry | rank exit | 5–50 SOL, ≤ 1% of pool | same + rank momentum, attention (boosts, live stream), KOL/tracked-wallet presence |
| `FADING` | net sellers, liquidity falling | 24 h idle | exits only | — |
| `DEAD` | volume < ε for 24 h | — | — | labels only |

Positions are never opened outside the band of the coin's current state. Exits stay decoupled from the entry source (the existing exit engine, with per-state default policies and shadow policies as now).

### 4.2 Signal sources become features

| Source | Feed (free) | Feed (paid) | Features |
|---|---|---|---|
| **W** tracked wallets | RPC WebSocket (1–6 s) | gRPC (30–300 ms) | `tracked_buys_5m`, `tracked_net_flow_5m`, `tracked_first_buyer_age`, per-wallet quality score, "wallet arrived after us" (pre-arrival telemetry) |
| **L** launch microstructure | PumpPortal creations + migrations; RPC `logsSubscribe` on the pump program (every trade, 1–3 s behind) | gRPC pump program stream (every trade) | holders@t, HHI@t, inflow buckets, sniper/dev behaviour, rail (vanity suffix), creator history from our own archive |
| **T** trending flow | Jupiter `toptrending`/`toporganicscore`/`recent` + DexScreener boosts/profiles + pump.fun live, every 5 min | Birdeye/Solana Tracker streams (optional) | organic volume, net buyers, holder change, liquidity change, rank momentum, age |
| **X** attention | DexScreener boosts, pump.fun livestream flags | — | `dex_boosts`, `is_live`, profile present |

A rule in v2 looks like: *state = ESTABLISHED, age 1–72 h, organic net buyers rising over 3 snapshots, liquidity up, top-10 ≤ 40%, ≥ 1 tracked wallet holding → enter 0.5% of pool with trailing exit.* Rules are small and readable; the learned model comes later, trained on our own labels, and only replaces a rule when it beats it out-of-sample.

### 4.3 Layers (what runs where)

```
S0 ingest      PumpPortal ws · RPC ws/accounts · Jupiter/DexScreener/pump.fun census · (gRPC)
S1 normalize   decode to typed events (existing chain crate) · archive raw bytes + hashes
S2 state       lifecycle map in RAM · rolling features per coin · wallet profiles
S3 gates+score per-state rules (versioned) · size by pool depth · abstain on missing data
S4 execute     existing engine: paper / shadow / live · exits · risk · journal
Store          JSONL journal (today) → Parquet partitioned by day, queried with DuckDB
Nightly        labels (peak multiple at 1 h/24 h, graduated, dead) · daily top-10 launches / top-20 trending · wallet re-vetting
```

Hard rules carried over from the studies: no third-party HTTP call in a decision path; raw bytes archived before parsing; features use only what was known at decision time; thresholds frozen before scoring; missing data → no trade.

---

## 5. The data plan ("record every dimension, tune on our own data")

### 5.1 Snapshots
One row per coin per checkpoint — `t+15 s, 30 s, 60 s, 5 m, 30 m, 1 h, 6 h, 24 h` after creation, plus every 5 min while `ESTABLISHED`/`TRENDING` — with the feature catalogue from [03](03-data-model.md) as far as the current feed allows (free feed: everything the census APIs return + curve/pool state; gRPC: the first-minute microstructure too).

### 5.2 Labels (nightly, from our own tape)
Peak multiple from each checkpoint at 1 h / 24 h; drawdown; graduated (and time to graduate); dead; "a tracked wallet bought after this checkpoint" (pre-arrival label); realized return of the engine's own shadow/paper entries.

### 5.3 Daily tables (the ones you asked for)
- **Top 10 launches of the day** ranked by peak market cap within 24 h (also by holders and by graduation), each with its `t+15 s / 60 s / 5 m` snapshot so "what did the winners look like early" is a query, not an opinion.
- **Top 20 trending of the day** ranked by 24 h organic buy volume and net buyers, with first-seen time, rank history and the snapshot at the moment it first entered the top 20 (the earliest point a rule could have caught it).
- Both written as CSV/Parquet and a short markdown summary; the first weeks' tables are the training set for the state-machine rules.

### 5.4 Where it lives
Free POC: JSONL/Parquet files produced by the recorder, kept as GitHub Actions artifacts or on a small VPS, queried with DuckDB. Proper: the ClickHouse/Postgres schemas already in `schema/` on a server. The journal format does not change; the store does.

---

## 6. What is free, what costs money

| Item | Free tier | Paid | Needed for |
|---|---|---|---|
| Launch events | PumpPortal `subscribeNewToken` + `subscribeMigration` (no key) | PumpPortal trade stream (metered) | census (free) |
| Every pump trade | public RPC `logsSubscribe` on the pump program (no key; 1–3 s behind, p99 ≤ ~9 s; connections recycled every minute or so, so two are kept open) | gRPC (30–300 ms) | first-minute features (free is enough for research and for decisions at ≥ 15 s) |
| Trending/discovery | Jupiter tokens v2 (trending, organic, recent, search), DexScreener (boosts, profiles, pairs; 60–300 rpm) | Birdeye / Solana Tracker ($) | trending tier (free is enough) |
| RPC | public endpoint (4 rps/method), Helius free (1M credits, 10 rps) | Helius Developer $49 | history, account polling, paper runs |
| Streaming | — | Yellowstone gRPC: Triton from ~$49/mo (1 stream); Solana Tracker/Chainstack mid tiers; Helius LaserStream $499+ | sub-second detection, first-minute microstructure |
| Server | GitHub Actions (5.5 h runs, cron ≥ 5 min, timing not guaranteed) | VPS $5–15 (census) · bare metal Frankfurt/Ashburn $50–150 (execution) | 24/7 census; live execution near leaders |
| Execution | Helius Sender / Jito tips (pay per tx) | — | live phase |

Two budget tiers are realistic: **Tier B ≈ $60–120/month** (Helius Developer or a Triton gRPC stream + a small VPS) gets slot-level launch data and a 24/7 census; **Tier C ≈ $600–1,500/month** (LaserStream-class feed + bare metal + second provider) is the latency-competitive execution setup from [07](07-roadmap.md). Tier A ($0) runs the census and the trending tier at 5-minute resolution and wallet copies at 1–6 s.

---

## 7. Work orders

Each has an output, a gate and a cost. Nothing after WO-2 starts until WO-1/WO-2 produce data, because every later design choice should be made on our own numbers.

| # | Work order | Output | Gate to pass | Effort | Cost |
|---|---|---|---|---|---|
| **WO-1** | **Wallet classifier + continuous re-vetting.** `copybot wallet-audit <list>`: bot/trader/holder class from tx rate, failure share and burstiness; realized PnL, hold time, venues, position sizes from chain; copyability verdict; weekly re-run; writes the leader list the engine uses. | table + auto-maintained `leaders` config | ≥ 5 wallets classed *trader* with positive 30-day on-chain PnL and median hold ≥ 60 s | 1–2 days | free (Helius free key for speed) |
| **WO-2** | **Census recorder.** `copybot census`: PumpPortal creations/migrations continuously; Jupiter recent/trending/organic, DexScreener boosts/profiles, pump.fun live every 5 min; curve/pool state of every seen coin at the checkpoints; raw bytes + hashes; Parquet by day; nightly labels; daily top-10 launches / top-20 trending tables. | the tape + daily tables | 14 days recorded with ≥ 95% checkpoint completeness; launch count and graduation rate stated from our own data | 3–5 days | free on Actions; better on a $5–15 VPS |
| **WO-3** | **First-minute microstructure.** Trade stream on the pump program (planned: gRPC or metered PumpPortal; built: the free RPC log stream); holders@t, HHI@t, inflow buckets, sniper/dev exits per coin; reproduce the DeepSeek thresholds on ≥ 10k of our own launches with chronological splits. | validated early-signal table | holders@15 s lift ≥ 5x and HHI ≥ 0.8 → ≈ 0% reproduce out-of-sample | 1 week | free (was: gRPC $49–$499/mo) |
| **WO-4** | **State machine in the engine + trending policy, shadow.** Lifecycle states, per-state gates and size bands, trending entry rules on census features, copy signals demoted to features, shadow fills with the honest cost model, all exits replayed; pre-arrival telemetry. | shadow journal with ≥ 200 trending entries and ≥ 100 copy entries | expectancy after costs > 0 with CI on untouched future data; pre-arrival rate measured | 1–2 weeks | free |
| **WO-5** | **Live at minimum size.** 0.1–1 SOL on the trending tier and the best wallets, Helius Sender/Jito tips, server near the leaders, landing-rate and realized-vs-shadow reconciliation. | 200 live round trips | landed ≥ 90%; realized within tolerance of shadow; positive after all costs including infrastructure | 1 week + run time | Tier B/C |
| **WO-6** | **Learned models and scaling.** Per-state entry models trained on our labels (meta-labels on the rules), champion/challenger in shadow, size scaled by pool depth and posterior, wallet rotation automated. | model registry + promotion log | out-of-sample lift over the rules; 8 weeks of leader-free shadow ≥ copy returns (roadmap graduation gate) | ongoing | — |

Work orders run **one at a time on the $0 tier** (decided 2026-10-08). WO-1 and WO-2 need no money. WO-3 was planned as the first step needing a paid feed; it turned out not to (status below). It is the one that decides whether the launch tier is worth entering at all; if its gate fails, the engine lives in the trending and wallet tiers only.

### WO-1 result (closed 2026-10-08)

Delivered: `copybot wallet-audit` (classifier, own-transaction read with retries, verdicts with reasons and the best and worst trips as evidence, JSON + table), `leaders_file` in the engine config, the weekly `wallet-audit` workflow (list from the `AUDIT_WALLETS` secret), RUNBOOK section. Found and fixed on the way: two decoder bugs (coin-priced curves; histories flooded by other people's transactions), both pinned by tests on real transactions.

Gate: "≥ 5 wallets classed trader with positive on-chain PnL and median hold ≥ 60 s".

| Check | Result |
|---|---|
| Traders with positive PnL and hold ≥ 60 s | 5 (kol-CkPFG, 9WhiK, skarraOG, kol-CCUcj, kol-AEeJU) → **passes on its literal terms** |
| Of those, full leaders (30+ trips, profit factor ≥ 1.5) | 2 (kol-CkPFG, 9WhiK) |
| Leaders in total | 4 (adds mofo chad and kol-4Ddrf, both under 60 s holds) |
| Window | ~20 h per wallet (1,000 own transactions on a free RPC), not 30 days |

Honest reading: the gate passes, narrowly. The window is a day, not a month, so the leader list is provisional and the weekly re-run is what makes it trustworthy. Activating that weekly run needs the workflow file on the default branch (same one-file step as paper-run). The leader count is the limiting factor for signal volume; adding candidates (KOLscan, GMGN lists) and auditing them is a cheap repeatable step.

### WO-2 status (built 2026-10-08; recording, gate open)

Delivered: `copybot census` (the recorder), `copybot census-report` (labels and the daily tables), the `census` workflow (5 h 45 min every 6 hours, checkpoints carried from run to run, 90-day retention, the day's report on each run's summary page), RUNBOOK section A5.

| WO-2 item | Built as |
|---|---|
| PumpPortal creations and migrations | WebSocket, reconnects by itself |
| Jupiter recent / trending / organic, DexScreener boosts / profiles, pump.fun live | recent every 10 s (all launchpads); 10 lists every 5 min |
| state of every seen coin at the checkpoints | Jupiter batch stats (100 coins per call) at 15 s … 24 h, plus the exact curve SOL from the chain when `RPC_URL` is set; dead coins stop at 5 min |
| raw bytes + hashes | gzip archive per hour, SHA-256 on every row |
| Parquet by day | **JSON lines by day instead.** No columnar dependency is needed at this volume, and the report reads it directly; conversion is one step when WO-5's training set needs it |
| nightly labels, daily top-10 launches / top-20 trending | `census-report` → `daily.md` + `labels.jsonl` (forward outcomes from later checkpoints only) |

Found in the first hour of data and fixed: PumpPortal reports some existing tokens as creates (the PUMP token itself, at $2.6B and 320k holders "at 15 s"); a launch whose token Jupiter dates more than 10 minutes earlier is now flagged `not_new` and left out. Meteora DBC coins can be quoted at millions on a few hundred dollars of liquidity; the report counts a market cap only when liquidity of at least 1% of it backs it (real pools hold far more).

Gate (unchanged): 14 days recorded with ≥ 95% checkpoint completeness, launch count and graduation rate stated from our own data. The 14 days start when the workflow is on the default branch (or a server runs it). On Actions the gaps between runs (startup and build, a few minutes every 6 hours, plus any late schedule) cost about 2–4% of the day, so the completeness gate is reachable there but with little margin; a small server has no gaps.

### WO-3 status (built 2026-10-08 on the free tier; recording, gate open)

Found while building: the public RPC endpoint streams every transaction that touches the pump program over `logsSubscribe` (no key). A 70 s probe: 237 transactions/s, 57% of them failed (bots), 62 trades/s and 48 creates, i.e. ~59k launches a day, matching PumpPortal. The `Program data:` lines carry the full create, trade and graduation events, so no transaction has to be fetched. Delay behind the block time: p50 1.1 s / p99 1.7 s on a quiet minute, p50 2.4–2.8 s / p99 7.5–8.8 s under load (the same from an independent client, so it is the endpoint, not us). The endpoint closes each connection every minute or so; two connections with de-duplication by signature gave **0 gaps across 11 recycles** in an 8-minute test.

Delivered, inside `copybot census` (no new command):
- per-coin books from the stream, written at 5, 15, 30, 60, 120, 300 and 900 s after creation (`micro.jsonl`). Each snapshot is rebuilt from the trades stamped at or before its moment, sorted in chain order, 10 s after that moment; trades that arrive later still are counted as `late`. Features: holders, buyers, sellers, flow, market cap, curve progress, HHI of holdings and of buy volume, top-1/top-10 share, dev holding and dev sold, snipers (create slot + 1) with their share and how many sold out, new buyers in the last 5 s, and the per-second SOL flow of the first minute.
- outcome per coin one hour after creation (`curve_outcomes.jsonl`): market cap at each snapshot and the highest after it (labels with no lookahead), graduation and its time.
- feed health per minute (`feed.jsonl`) and `gap_ms` on every row, so coins seen with a hole are left out.
- the decoded trade tape (`trades-<hour>.jsonl.gz`), for re-deriving features under new definitions and, later, for finding wallets ourselves from every trade.
- `census-report`: holders@15 s, HHI@15 s and dev/sniper tables against "market cap doubles within the hour", for the whole day and for its earlier and later half, and the WO-3 verdict judged on the later half only (needs ≥ 10,000 usable coins and ≥ 30 in each tested bucket).

What the free stream does not give: sub-second speed (gRPC's job, only relevant once the launch tier is proven), and PumpSwap trades after graduation (a second subscription on the AMM program when MIGRATED_FRESH is built). Holdings are from curve trades only; plain token transfers are not seen.

### What the data says about the target (2026-10-09)

The first look at 1,934 launches is in [11-trading-decisions-data.md](11-trading-decisions-data.md): every coin that reached $100k was pre-funded (the curve filled in the create transaction, creator holding 79%, landing at $45k), so the $100k population is a PumpSwap timing trade against a known seller, not a curve pick; round numbers show no support or resistance on the curve; the early-breadth features point the right way on a tiny sample. The recorder now follows every graduated coin's pool for a day (candles, outcomes, the creator's sells), records the SOL price, the create transaction and the market regime, and the daily report has an after-graduation table. The hypotheses and the data each needs are listed there; `MIGRATED_FRESH` in §4.1 splits into factory coins and organic graduations.

### WO-3 status (2026-10-10, corrected 2026-10-10 evening)

On 16,908 coins (two Actions runs, 11 h): holders ≥ 30 at 15 s makes a coin 3.1–3.7× as likely to double within the hour (34–38% of them do, in both chronological halves); HHI ≥ 0.8 cuts the rate to 0.4×. The first replay found no profitable ten-minute scalp of new coins with tight stops (out of sample −3.8% a trade), and I wrote "the launch tier is dropped" on that. That was wrong: the replay's exits held for at most ten minutes on a tape that ends at one hour, so the hold-for-hours thesis was never tested; see [12](12-replay-audit.md). What stands: scalping new coins with 2.5% round-trip fees loses; the early-breadth signal is real; held through the first hour without a stop, the holders ≥ 30 cohort is at −7.5% ± 16% (unresolved). The test of the thesis is the full-life path replay (curve → pool → checkpoints, ladder exits), next.

### WO-4 progress (2026-10-09): the replay engine

*(Removed on 2026-10-10, late evening, with the trending-tier grid: the entry × exit search, the walk-forward and the holdout. What stays of it is the tape model that `copybot winners` uses: exact curve fills for our size, the recorded fee, our delay, the signal's first-crossing second. See [12 §5.4](12-replay-audit.md).)*

The first piece of WO-4 was built: `copybot replay` tests entry rules and exits on the trade tape before any money is at risk (RUNBOOK A6). Exact curve fills for our size, the recorded fee, our transaction landing `delay` after each decision and exit trigger, our own buy kept in the curve until we sell. 480 entry rules × 90 exits are scored; one is chosen walk-forward (picked on earlier 6 h blocks, scored on the next block only), the newest 20% of coins stay locked until a single `--final` run, and the report states how many pairs were tried and refuses per-day figures under 6 h of data.

First run (25 minutes of tape, 1,174 coins): the best in-sample pairs show +7 to +9% per trade, but none is positive at 95% confidence, and the walk-forward trades swung from -13% to +10% per trade as four trades were added. That is the expected picture for this little data; it is why the protocol exists. The answer needs days of recording, which the Actions census now provides.

Data problems found and fixed on the way: a checkpoint taken late (after a pause) was recorded under its original moment, so a "5 min" row could describe a coin 20 hours later; it is now recorded as missed. Each trade on the tape now carries the curve's fee rate and real token reserves.

### WO-4 progress (2026-10-10, evening): working backwards from the winners

The order above was the conventional one and it was wrong for this market: it searched exits before defining winners. `copybot winners` (RUNBOOK A6) now runs in every census: the day's winners by liquidity-backed peak, their fingerprint at 5–300 s, fixed rules with recall, precision and lift, the lead over the trending lists, who bought them first, and the signal-as-entry replay on the trade tape. On 33,378 coins (16.4 h): the winners are visible at **5 s** and the thing that shows them is SOL in the curve (net SOL ≥ 30 at 5 s → 45% of fires reach $30k, lift 38×; the rule first holds at a median 2 s after creation, the trending lists show the coin at 7 min). Entering at that second and exiting anywhere inside the hour loses 11–19% a trade with intervals that exclude zero (the signal is priced: a 20–30 SOL curve is a $9–13k coin, $30k is 3× away, and 55–70% of fires are at −70% an hour later). The $100k class is not separable on the first-minute book (best precision 1 in 20; a third of them show nothing in the first minute). What stays open is the multi-hour hold from the signal, whose 6 h and 24 h columns now accumulate every run with their intervals. Full audit: [12 §5](12-replay-audit.md); report: [reports/winners-2026-10-10.md](reports/winners-2026-10-10.md).

Consequences for the work orders: the launch-tier entry in WO-4's state machine is the SOL-in-curve signal at seconds, not a snapshot rule; no shadow or live trade is taken on it inside the first hour; the launch tier's remaining question is the hours-long hold, answered by the census as the 6 h/24 h checkpoints come in; the trending tier's entry becomes the 5–30 min stage before the lists (holders and net SOL rising while the market cap is still small), where the slow-burning $100k coins first show.

---

## 8. KPIs

- **Census completeness**: launches seen / PumpPortal creations; checkpoint rows present / expected.
- **Detection latency**: create → seen, leader trade → decision (p50/p90), by feed.
- **Selection quality**: per rule and per state, lift of P(2x within 1 h) over the base rate, chronological out-of-sample only.
- **Pre-arrival rate**: share of our entries where a tracked wallet bought the same coin after us at a worse price (the GLM 5.3 definition; it is the test of "before the manual traders").
- **Expectancy after costs** with a bootstrap CI, per source (W/L/T) and per state; profit factor; max drawdown.
- **Execution**: landed rate, realized vs modelled slippage, tip spend per SOL traded.
- **Capacity**: SOL deployable per day at ≤ 1% pool impact per position, by state (this is the "$10–100k" question measured rather than assumed).

---

## 9. Honest expectations

- Nothing recorded so far shows an edge: attempt 1 has two paper trades (+0.23 SOL, noise). The published copy-trading study found copier returns negative on average; our first live validation showed why (we land 1–6 s after the leader on the free feed).
- The "$100k/day" evidence is vendor-reported PnL for wallets we either identified as bots and holders or could not decode. What we *can* verify is one trader at about $10k/day and three more at $0.6–4k/day in one-day slices, with 31 s–3 min holds. The trending tier is the only one where $10–100k of capital fits, so the plan puts the first shadow money there, keeps the launch tier small and fast, and uses copies as a signal.
- **What $10k/day takes** (asked 2026-10-09: "1 SOL to $10k/day soon?"). $10k/day is about 90 SOL of profit a day at $111/SOL; on a 1 SOL bankroll that is 90× a day. The best trader we verified did +87 SOL in 20 h, with 1.5–10 SOL positions and most of the profit from a few trades after graduation (one went 1.5 → 9 SOL); his bankroll is tens of SOL. So the route is: an edge proven out of sample at small size, the bankroll grown to ~20–50 SOL at a bet size that survives losing streaks (weeks to months if the edge holds), and only then his position sizes, in the tiers where they fit (post-graduation and trending, not the curve). Results are never tuned to reach a target: a backtest fitted to show $10k/day says nothing about tomorrow and loses the bankroll.
- Each work order has a gate designed to kill it cheaply. (The WO-3 gate was withdrawn on 2026-10-10: it compared lifts across different labels. The launch tier is now judged by `copybot winners`: the first-hour trade from the early signal is a measured loser; the hours-long hold is measured as the data comes in.) If WO-4's shadow expectancy is not positive on future data, no live phase starts.
- The durable asset is the tape: every launch, every snapshot, every decision and its outcome, timestamped at receipt. Vendors sell the same feeds to everyone; nobody sells this.

---

## 10. Decisions needed

1. **Budget tier for the next 4–6 weeks.** A ($0: census + trending at 5-min resolution + wallet copies 1–6 s late), **B (≈ $60–120/mo: slot-level launch data and a 24/7 census; recommended)**, or C (≈ $600–1,500/mo: latency-competitive execution, only after WO-4 passes). *Decided 2026-10-08: A, until something proves profitable. Since then the free log stream covers B's launch data at 1–3 s instead of sub-second.*
2. **Where the census runs 24/7.** GitHub Actions (free, 5-minute granularity, interruptions) or a small VPS (recommended, $5–15/mo; I set it up from `deploy/`). Your Windows machine works for the console and research, not for the recorder.
3. **Leader list.** Settled by WO-1: the weekly `wallet-audit` writes it (this week mofo chad, kol-CkPFG, kol-4Ddrf, 9WhiK).
4. **Capital framing.** Agree that the engine's size lives in the trending/migrated tier (positions ≤ 1% of pool, $1–10k each) and that the curve tier is for 0.1–1 SOL probes. This changes which gates get built first (trending rules before launch sniping).

---

## Appendix A — How the vetting was run

- Activity and failure rates: `getSignaturesForAddress` (last 1,000) and `getBalance` on the public endpoint, 2026-10-08 13:46–13:53 UTC.
- Decoded trading: `copybot leader-report --limit 400` on PublicNode for the 16 Notion wallets (all 400 fetched), and `--limit 300` on the public endpoint for five watchlist wallets (32–80% of fetches refused by the rate limit; marked partial above). Realized PnL counts what the wallet really paid and received, fees included, per round trip; open positions are excluded.
- Vendor figures are GMGN `wallet_profits` windows saved on 2026-10-03 in `solana_wallet_watchlist.json`, reported here as vendor numbers only.
- Re-reads with a keyed RPC (Helius free tier) are part of WO-1.
