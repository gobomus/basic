# 11. What decides a trade: the data, what it says so far, and what we test

Written 2026-10-09 on the first 1,934 recorded launches (two windows: 2026-10-08 22:10–22:27 and 2026-10-09 18:30–18:57 UTC) and the question "can we narrow selection to a 50/50 chance of a runner to $100k with entry under $20k, and ride the retracements?". Numbers here come from `data/census` on that day unless a source is named; SOL was $109–111. The sample is tiny and the recorder had outages, so every number is a first look, not a result. What is firm is which data is needed and whether we collect it; that is what this document fixes.

## 0. The three things the first data says

1. **Every coin that reached $100k was pre-funded.** 19 of 19. The creator bought the whole curve (85 SOL, about $9.3k) in the create transaction, held 79.3% of the supply, and the coin landed on PumpSwap at the $45k graduation cap in its first second. 2,000–3,000 holders appeared within 5 minutes (distribution, not buying). None of them was ever under $20k. Of 1,742 ordinary launches, none reached $100k in the time we followed them (most under 15 minutes, 345 for 3 hours or more). So, in this sample, **"entry under $20k and a runner to $100k" is an empty set**, and the $100k population is a factory product with its own rules.
2. **Round numbers show no support or resistance on the curve.** Local peaks are no denser within 3% of $5k, $10k, $15k, $20k, $25k, $30k than 8% above or below them (ratios 0.7–1.3, no pattern), and the chance that the first touch of a level is followed by a 20% fall rather than a 20% rise is the same at round and non-round levels (30–46%). The one level that works is **just below graduation, around $40k: 77% of first touches reversed** (10 of 13): early buyers sell before migration. After graduation the question is open: we had no post-graduation price path until today.
3. **The early features point the way you expected, but the sample is 337 coins.** Holders per $1k of market cap at 60 s: under 1 → 2.6% ran, 4 or more → 13–15% (3× the base rate). 25+ buys in the first minute: 11% (2.5×). Six or more new buyers in the last 5 s of the first minute: 18% (4×). "Dev sold" makes no difference (3.9% vs 5.4%), as in the DeepSeek study. Sniper count makes no difference. These are 10–20-coin buckets; the shape is worth testing, the numbers are not worth believing yet.

Seen in the first ten minutes of the PumpSwap stream (7 graduations): two of them were sold to nothing within a minute of landing ("HC": $1.43 of liquidity and a $1 market cap by the time DexScreener listed it; "STEVECOIN": created 22:19:41, graduated 22:21:18, $4 by 22:27). **Graduation is not a health signal; it is the moment the creator can sell into a pool.** The 1 h outcomes will give the share of graduations that are rugs within the first minutes, by population.

## 0b. One day later: the first verdicts (2026-10-10, 11 hours of Actions recording)

- **The curve tier has no edge at our speed and size.** 16,908 coins, 1.41 M trades, 480 entry rules × 90 exits: not one of the 43,200 pairs is profitable at 95% confidence even on the data it was picked from; walk-forward, 276 trades, −3.8% per trade (95% low −8.4%), profit factor 0.77, and the result is the same at 0.1 or 5 SOL and at a 1 s or 8 s delay. The holders@15 s lift is 2.8–3.7× (the gate asked 5×); HHI ≥ 0.8 cuts the rate to 0.4× (the gate asked ≈ 0). Per the plan, **the launch tier is dropped** as a buy-side strategy; its data stays, because it is where the wallet and post-graduation features come from.
- **The first hour after graduation is a dump zone.** 190 pools with a full hour (run 2): organic graduates peak within 30 s (the migration spike) and 85% are at −95% an hour later; "pre-funded" coins that are not insider-held (36) peak at 1.6× and 92% are below half their landing price at the hour. Only 15% of graduations are alive at the hour; whatever edge exists is in telling those apart at landing, and the EV needs ≥ 25–30% survival to break even.
- **Half of the pre-funded graduations are fabricated market caps.** 43 of 79: in the graduation slot the creator buys another 10–50% of the supply with a median 161 SOL (one with 2,985 SOL), which leaves almost no tokens in the pool and prints a market cap in the hundreds of millions (Jupiter showed $194M on $611k of liquidity, 2,940 airdropped holders). They "peak" at 16× by construction and 72% are at −96% an hour later when the creator takes his SOL back. **Any market-cap target has to be liquidity-backed and float-adjusted**; the recorder now records the landing price, the pool's SOL after every trade, the insider share bought in the graduation slot, and sells by wallets that never bought (airdrop and insider distribution), and the report splits these coins out.

- **The trending tier, first pass (12 hours of 5-minute captures, 1,455 coins with a path, 833 first entries into the 1 h top 100):** entering the 1 h top 100 is followed by a later peak ≥ 1.5× in 15% of cases and ≥ 2× in 9%; 45% hold their entry price to the end of their path; the median coin enters at $246k market cap, $30k liquidity, 1,748 holders. The replay (`copybot replay-trending`, 720 entry rules × 72 exits, fills moved by size against the pool, 0.5% fee a side) finds no pair profitable at 95% confidence in sample, and the walk-forward loses (−16% a trade on 44 trades, 26 of them stop-losses at 5-minute granularity). Twelve hours is two usable blocks; the number to watch is the walk-forward line on each run's summary page as days accumulate. Two things were fixed from this pass: paths now keep the listed point when a checkpoint falls in the same minute, and the recorder keeps pricing a coin for a day after it leaves the lists, so the fall after a listing is on tape, not a haircut.

What this changes in the plan: the money is not in buying new coins early, nor in buying graduations. The two tiers left are the ones the plan sized for in the first place: **established and trending coins** (liquidity-backed, hours old, 5-minute data already recorded) and **following verified wallets** through our own tape (their trades are on it, so the copy can be replayed with the real delay). Both replays are the next builds.

## 1. The target, in numbers

- $100k from $45k (graduation) is 2.2×; from $20k it is 5×. The curve itself caps at 14.7× from the very start and at 2.3× from $20k, so **a $100k runner is a PumpSwap event**: nothing on the curve can produce it, and the curve data only describes the coin's first minutes.
- 50/50 is not the number to aim at. What matters is expectancy. Breakeven hit rate p* = L / (W − 1 + L), for a win of W× and a loss of L on the trades that fail:

| win | loss 30% | loss 50% | loss 80% |
|---|---|---|---|
| 2× | 23% | 33% | 44% |
| 3× | 13% | 20% | 29% |
| 5× | 7% | 11% | 17% |
| 10× | 3% | 5% | 8% |

  A 5× target with losses held to 50% breaks even at 11% hit rate and earns +48% per trade at 20%. **The realistic ask of the data is a 10× lift on a 1–2% base rate, not 50%.** DeepSeek's holders@15 s gave 10.6× on "doubles again"; our first look gives 3–4× on a cruder label. That is the gap to close with more features and more data.
- "Layer 2 and 3" (the 100× and 1000× among the winners) are an exit problem, not a selection problem: the selection cannot tell a 5× from a 100× at entry. Section 6 is about that.

## 2. What the first 1,934 launches say

### 2.1 Two populations

| | pre-funded (curve bought in the create tx) | ordinary |
|---|---|---|
| share of launches | 28 of 2,019 (1.4%) → roughly 700–900 a day | 98.6% |
| creator's share at 5 s | median 79.3% | median 0.1% |
| holders at 5 s | 1 (the creator) | 1 |
| reached $100k (Jupiter checkpoints) | **20 of 26 (77%)** | **0 of 1,742** |
| reached $250k | 15 (58%) | 0 |
| reached $1M | 3 (12%) | 0 |
| holders at 5 min | 500–3,000 in 62% of them | a few |
| creator's wallet | fresh (1 prior mint) in all but two | varies |

The pre-funded ones are a production line: fresh wallet, 85 SOL into the curve, trend-jacked name (TESLA, Claude AI, Robinhood, MrBeast, Haaland), tokens spread to thousands of wallets within minutes, 300–800 traders in the first 5 minutes, organic score 0. The economics: selling 79% of the supply into the pool returns ~63 of the 85 SOL even with no outside buyers, so the launch costs about 22 SOL plus marketing and pays whenever outsiders put in more than that before the creator sells. **For a trader the only question is the window between landing and the creator's exit**: how long it is, how high it goes, and whether it is visible in advance (buyer arrival rate, the creator's first sell). That is a PumpSwap question, answered by the candles we started recording today, not by the curve.

Caveat on the zero: ordinary coins were followed for 15 minutes at most in both windows (the recorder died), 3 hours or more for 345 of them. A slow organic runner takes hours. The zero is a lower bound on our information, not a rate. The Actions census follows every coin for 24 h; the rate will be known within days.

### 2.2 Ordinary launches: base rates on the 15-minute books (337 coins)

| within 15 min | share |
|---|---|
| no trade after the first 5 s | 22.8% |
| peak market cap ≥ $5k | 20.8% |
| ≥ $10k | 10.1% |
| ≥ $20k | 5.3% |
| ≥ $30k | 3.3% |
| graduated | 2.1% (2.4% at ≥ $44k) |
| "ran" (doubled after 60 s, or graduated) | 4.5% |

Feature tables against "ran" (lift = share in bucket / 4.5%); n per bucket in brackets:

| feature at 60 s | low bucket | high bucket |
|---|---|---|
| holders per $1k market cap | < 1: 2.6% (195) | ≥ 4: 13.6–15.4% (35) → **3×** |
| buys | 3–5: 0% (43) | 25+: 11.0% (82) → 2.5× |
| new buyers in the last 5 s of the minute | 0: 4.3% (305) | 6+: 18.2% (11) → 4× |
| net SOL in | < 0.5: 2.8% (249) | 15+: 38.9% (18) → 8.7× (pre-funded coins) |
| dev holding at 5 s | 0%: 1.3% (152) | < 2%: 7.5% (107) → 1.7×; 30%+: 67% (6, pre-funded) |
| dev sold by 60 s | kept 3.9% (207) | sold 5.4% (130) → no signal |
| snipers in the create slot (+1) | 0: 6.0% (184) | 8+: 4.5% (22) → no signal |
| HHI of holdings | < 0.1: 1.7% (121) | 0.4–0.8: 10.2% (59) → 2.3× (non-monotone) |
| top-10 share | < 20%: 3.0% (297) | 60%+: 83% (6, pre-funded) |

Reading: breadth per dollar (holders per $1k) and momentum into the end of the first minute are the two ordinary-coin signals worth a real test; "the dev bought nothing" is a mild negative; holder concentration only matters because the pre-funded coins sit in the top bucket. Everything with n under 30 is a hint.

### 2.3 Round numbers

Method: every coin's trade-by-trade market cap in dollars (161,092 trades, 1,496 coins); local peaks = higher than the 5 trades before and after and 15% above the preceding trough (4,595 peaks); density within ±3% of a level against the same windows 8% above and below; and, separately, what happened after the first touch of each level from below (20% fall first vs 20% rise first).

| level | peaks at level / nearby | first touch: reversed |
|---|---|---|
| $5k | 1.00 | — |
| $7.5k | 1.13 | — |
| $10k | 0.72 | 37% (n = 146) |
| $15k | 0.82 | 40% (88) |
| $20k | 0.75 | 35% (58) |
| $22k (control) | — | 41% (49) |
| $25k | 1.15 | 46% (35) |
| $28k (control) | — | 34% (29) |
| $30k | 0.76 | 30% (27) |
| **$40k** | 1.29 | **77% (13)** |
| graduation (~$45k) | 1.5 (6 vs 4) | — |

No round-number effect on the curve. The ~$40k reversal is structural: the curve is 85–90% full, the early buyers' exit before migration. Post-graduation levels ($50k, $100k, $159k…) are a different market (constant-product pool, charts on DexScreener/Axiom, round numbers in dollars on every screen) and are now testable with the candles; the test is the same two columns. Until then the honest statement is: on the curve, the levels that matter are the creator's cost basis, the snipers' cost basis and graduation.

### 2.4 Holders per market cap, transactions, dev, supply

- Holders per $1k is the one ratio with a visible gradient (above). A tipping point cannot be placed yet; it needs 10,000+ coins and a proper calibration curve, which the daily report will produce once the data is there.
- Transactions: buys in the first minute lift 2.5× at 25+; buys per buyer (one wallet buying many times) is a wash-trade sign: 2.5+ buys per buyer ran 8.9% — because that bucket is bundles, not organic interest. The report needs both.
- Dev: the supply share at 5 s and whether it is 0 matter; selling in the first minute does not.
- Supply: fixed at 1 B; what matters is who holds it (top-10, snipers, dev) and, after graduation, how fast the big holder moves.

## 3. The feature catalogue, by layer

Status: **R** recorded since 2026-10-08, **A** added 2026-10-09, **–** not yet.

| layer | feature | definition | row | status |
|---|---|---|---|---|
| L0 market regime | `launches_10m`, `grads_1h` | launches in the last 10 min, graduations in the last hour, at the coin's creation | `micro` (t=5) | A |
| | `sol_usd` | SOL price at the time, so every market cap reads in dollars | every row; `sol_price` each minute | A |
| | time of day, day of week | from `created_ts` | derived | R |
| L1 the launch | `create_buy_sol`, `create_slot_buys`, `create_slot_sol` | what the creator bought in the create transaction, how many others bought in that slot and how much (bundle) | `micro` (t=5) | A |
| | `instant_grad` | the curve was filled in the create transaction (pre-funded) | `micro` (t=5) | A |
| | `dev_pct`, `dev_sold` | creator's holding, whether he sold | `micro` | R |
| | `dev_mints` | creator's earlier launches (Jupiter) | `launches.first_seen`, `checkpoints` | R |
| | creator history from our own tape | prior launches by this wallet and how they ended | – | – (next) |
| | creator funding source | where the creator's SOL came from (a known factory?) | – | – (costly: one RPC read per creator) |
| | metadata | name, symbol, image/socials present in the URI's JSON | `launches` has the URI; the JSON is not fetched | – (cheap) |
| | rail | `pump_suffix`, `mayhem`, launchpad | `micro`, `launches` | R |
| L2 first minutes (curve) | holders, buyers, sellers, buys, sells, buy/sell SOL, net SOL | from every trade, at 5/15/30/60/120/300/900 s | `micro` | R |
| | `hhi`, `hhi_buys`, `top1_pct`, `top10_pct` | concentration of holdings and of buying | `micro` | R |
| | `snipers`, `snipers_pct`, `snipers_out` | create-slot(+1) buyers, their share, how many sold out | `micro` | R |
| | `new_buyers_5s` | buyers whose first buy was in the last 5 s before the snapshot | `micro` | R |
| | `inflow_1s` | SOL in/out per second for the first minute | `micro` (t=60) | R |
| | `progress`, `curve_sol`, `mcap_sol`, `mcap_usd` | where on the curve | `micro` | R / A |
| | holders per $1k, buys per buyer, SOL per buyer | ratios | derived | R |
| L3 after graduation (PumpSwap) | landing market cap, creator, pool | `graduations` | A |
| | 1-minute candles: o/h/l/c, buy/sell SOL, buys, sells, buyers, sellers, creator's buys and sells | `candles` | A |
| | peak multiple, time to peak, deepest drawdown, trough after peak, creator sold, buyers | `amm_outcomes` at 1 h, 6 h, 24 h | A |
| | holder count after graduation | Jupiter checkpoints at 15 m … 24 h (`holders`, `top_holders_pct`, `organic_score`) | `checkpoints` | R |
| | every pool trade | `trades-*.jsonl.gz`, `ev: "amm"` | A |
| | liquidity, volume, organic flow | Jupiter checkpoints | `checkpoints` | R |
| L4 who is in it | known profitable wallets buying by t | needs the wallet table from the tape | – | – (task: wallet discovery) |
| | attention: pump.fun live, DexScreener boosts/profiles, trending ranks | lists every 5 min; join by mint | `trending` | R |
| | X mentions of the contract address: posts per 5 min, distinct authors, engagement, keyword polarity, KOL authors | search by mint address for coins in the trending universe only (~100–200 a day); TwitterAPI.io ≈ $0.15 per 1,000 posts, so ≈ $1–3 a day; X's own API is pay-per-read | – (gated: after the trending baseline, and only with a paid key) |

## 4. Hypotheses and their tests

Each test runs in `census-report` (daily, on all days) once the data exists; the decision column is what the result changes in the engine.

| # | hypothesis | data | test | decides |
|---|---|---|---|---|
| H1 | holders per $1k market cap at 60 s has a tipping point above which coins run | `micro` | calibration curve P(ran) by ratio, chronological halves, ≥ 10k coins | the breadth gate of `CURVE_EARLY` |
| H2 | momentum into the end of the minute (`new_buyers_5s` ≥ 6) predicts the next doubling | `micro` | same | an entry trigger |
| H3 | pre-funded launches have a usable window: landing → peak takes minutes, the creator's first sell marks the top | `graduations`, `candles`, `amm_outcomes` | distribution of time to peak, peak multiple, and the age of the first creator-sell candle; P(peak ≥ 2.2× = $100k) | whether the engine trades the factory coins at all, and the exit |
| H4 | buyer arrival in the first 1–3 minutes after landing separates the pre-funded coins that go 5×+ from the ones that go 2× | `candles` (buyers per minute) | lift table on minute-1/2/3 buyers | the entry gate of `MIGRATED_FRESH` |
| H5 | round numbers act as support/resistance after graduation | `candles` with `sol_usd` | the two columns of §2.3 on 1-minute highs/lows at $50k, $100k, $150k, $250k, $500k, $1M vs controls | whether exits ladder at levels or at multiples |
| H6 | the creator's earlier launches predict this one (serial ruggers rug, serial winners win) | our tape's creator index | P(ran) by prior-launch outcome | a creator blacklist / whitelist |
| H7 | bundles (many buys in the create slot) hurt ordinary coins and are neutral for pre-funded ones | `create_slot_buys`, `snipers_pct` | lift tables per population | a filter |
| H8 | the market regime moves every base rate | `launches_10m`, `grads_1h`, SOL trend | base rate by regime bucket | position size by regime, or pausing |
| H9 | known profitable wallets buying in the first minutes raise the odds | wallet table + `trades` | lift of "≥ 1 tracked wallet by t" | the W-source feature of the plan |
| H10 | runners retrace 50–80% before their peak, so a fixed stop loses most of them | `candles` | for coins with peak ≥ 2× (and ≥ 5×): deepest drawdown between landing and the peak, as a distribution | the trailing width and the ladder (§6) |
| H12 | X mentions of a coin's address rise before it enters the trending list, and the rise (not the level) separates the ones that go 1.5×+ from the ones that fade; author diversity tells bot farms from real attention | X mentions per CA (above) joined to the 5-minute trending path | lift tables of mentions and authors in the 30 min before first appearance vs peak ≥ 1.5×, chronological halves; marginal lift over the trending baseline without social data | whether the trending entry gets a social gate, and whether ~$1–3/day of post reads is worth paying |
| H11 | the ~$40k pre-graduation sell wall is tradable (sell into it, or buy the dip after it) | `trades` | P(reversal) at 85–90% progress and the depth of the dip, by population | an exit rule on the curve |

The report never tunes thresholds on the data it scores: buckets are fixed in advance (the ones in this document), the two chronological halves are shown side by side, and anything with fewer than 30 coins in a bucket is shown greyed.

### On pairing an X-sentiment tool (asked 2026-10-10: brainstormity/Jev-X-Sentiment-Analysis)

What it is: an on-demand web terminal for majors (BTC, SOL, ETH). Per search it pulls Kraken spot and futures data (price, RSI-14 on 48 hourly candles, funding rate, open interest), 50–1,000 posts for `$SYM OR Name` from TwitterAPI.io (≈ $0.15 per 1,000), computes keyword polarity (18 fear / 17 greed words), author diversity and engagement, picks 50 posts, and asks a hosted LLM four typed questions, printing a Buy/Sell/Hold card with fixed levels (stop −3.8%, targets +4.5% / +8.5%). No backtest, no outcome tracking; without keys it runs on simulated posts and a hand-written rule table.

Why the code does not pair: it is built for assets with a ticker, a perpetuals market and a news flow, none of which a 20-minute-old pump.fun coin has; `$FOMO OR FOMO` is noise, Kraken lists none of our universe, funding rates do not exist, and a 10-minute cache and a per-symbol request model cannot serve 50,000 launches a day. The LLM decision card is an untested signal; in this project every feature gets a lift table and a walk-forward replay before it touches a trade.

What is worth taking: the *question*. For the trending/established tier, attention is the driver, and X mentions of the **contract address** (not the symbol) are a plausible leading indicator we do not record: posts per 5 minutes, distinct authors (bot farms post the same CA from many accounts), engagement, and whether known KOL accounts posted. The pieces of that repo that transfer are concepts (engagement velocity, author diversity, stratified sampling), a few dozen lines when written against our tape. It is H12 above, gated on two things: the trending replay must exist first, so the social feature is measured as *marginal* lift over what the free lists already give (DexScreener boosts, pump.fun live, organic score, holder growth), and it costs money (≈ $1–3 a day for the trending universe), which under the $0 rule waits for a proven baseline or an explicit decision.

## 5. Collection gaps

Closed today (the recorder now writes them; the Actions census picks them up on its next run):

- **PumpSwap stream**: every graduated coin's pool followed for 24 h, from the free public endpoint (a second `logsSubscribe`, two connections, de-duplicated); `graduations`, `candles`, `amm_outcomes`, `ev: "amm"` trades. A curve priced in another token (pump.fun `buy_v2` with a quote mint; "Amanda" was priced in NFLX) migrates into a pool quoted in that token, whose prices are not SOL: those are recorded (`followed: false`, `quote`) and not followed.
- **SOL/USD** every minute (`sol_price`), and `sol_usd` + `mcap_usd` on every book row and candle.
- **The create transaction**: `create_buy_sol`, `create_slot_buys`, `create_slot_sol`, `instant_grad`.
- **Regime**: `launches_10m`, `grads_1h` on every book.

Still open, in the order they are worth doing:

1. **Creator index from our own tape** (prior launches and outcomes by creator wallet, carried between runs in the state artifact). Cheap; H6.
2. **Wallet table from the tape** (every wallet's realized results across coins; task "wallet discovery"). Medium; H9.
3. **Metadata JSON** (socials present, image present, description length) from the launch URI: one small HTTP fetch per launch, ~50k a day; cheap but a lot of requests; H1-era studies found it weak. Later.
4. **Holder count after graduation at minute resolution**: not derivable from swaps (airdrops are transfers). Jupiter checkpoints give it at 15 m / 30 m / 1 h; enough for H3/H4 at first.
5. **Creator funding source**: one RPC read per pre-funded creator (~900/day) is affordable on the public endpoint; would turn "fresh wallet" into "factory X's 14th wallet". After H3 says the population is worth trading.
6. **Post-graduation replay**: the replay engine reads curve ticks only; it needs the AMM path (constant-product fills from the candles' trades) to test H3/H4/H10 strategies. Next build.

## 6. Riding the runners: what to test, once the candles exist

The selection layer cannot tell a 5× from a 100× at entry, so the gap between them is made by the exit. The exit design to test against the recorded paths:

- **Ladder**: sell a third at 2× (the position is now free), a third at 4–5×, and let the rest run on a trailing stop measured from the peak, with **no fixed stop once the cost is out**.
- **The trail's width comes from H10**, not from taste: if the $100k coins typically retrace 50–80% on the way, a 30% trail sells every one of them at the first dip; a 70% trail keeps them and gives back most of the last leg. The distribution of "deepest drawdown before the peak" among the winners, by peak multiple, is the number that sets it. It is in `amm_outcomes` (`max_drawdown`, `peak_after_s`) and the candles.
- **Re-entry on the retracement** (buy the dip to the level that held) is only worth testing if H5 finds levels that hold; otherwise it is adding to a position that is being distributed.
- **The creator's sell as a hard exit** for pre-funded coins: the first candle with `creator_sold_sol > 0` is the signal; the test is how much is left to lose after it.

All of this runs in the replay (`copybot replay`) once it reads the AMM path, with the same walk-forward and holdout rules as the curve rules.

## 7. How much data, and when

- Ordinary coins: ~50k a day. The 15-minute books cover all of them; 10k coins (the WO-3 gate) are five hours of recording. Fine-grained buckets (holders per $1k in 10 steps × two halves) need ~30k.
- Pre-funded coins: ~700–900 a day; a week gives 5k, enough for H3/H4 with chronological halves.
- $100k coins: in this sample ~20 per 45 minutes, i.e. hundreds a day, nearly all pre-funded. H5 and H10 need a few hundred winners with full paths: a few days.
- The Actions census runs every 6 hours for 5 h 20 min each; pools followed for 24 h lose their later candles when a run ends (the next run starts a fresh follow for coins graduating in it). So 1 h outcomes are complete for ~85% of graduations and 24 h outcomes for none on Actions; the Jupiter checkpoints (15 m … 24 h) fill the 24 h labels. A small server would close that gap.

## 8. Decisions

1. **Treat pre-funded launches as their own tier.** They are where the $100k coins are, and the trade is a post-graduation timing trade against a known seller. The plan's `MIGRATED_FRESH` state becomes two: factory coins and organic graduations.
2. **Keep "entry under $20k" as a goal for ordinary coins only, with the honest base rates**, and measure the organic-runner rate over 24 h before deciding whether that tier can carry size.
3. **No tuning to a target.** Buckets are fixed here; the report shows both halves; the replay keeps its holdout. If the data says the edge is 15% hit rate at 5×, that is what the sizing uses.
4. **Next builds, in order**: creator index; post-graduation replay; wallet table. Each is a day or two and each unlocks a hypothesis above.
