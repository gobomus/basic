# 12. Audit of the replays (2026-10-10)

Asked for after the sentence "not one of the 43,200 pairs is profitable at 95% confidence". This document says what the replays test, what they cannot test, where the method was biased toward "no", what the data actually supports, and what changes. It is written against the code as of commit `6248424`.

## 1. What was wrong

### 1.1 The statistics were framed as verdicts the sample cannot give

The curve replay's first run (2026-10-09, 25 minutes of tape) reported a best in-sample pair with 56 trades, a mean of **+9.3% per trade**, a profit factor of 1.88 and a 95% lower bound of −2.5%. I reported that as "not profitable at 95% confidence". With 56 trades and a per-trade spread of ~54%, the smallest edge that test can see is about ±12% per trade: a strategy earning +9% a trade (a very large edge) **cannot pass that bar** at this sample size, whatever its real merit. The honest sentence was "+9% a trade, confidence interval −2.5% to +21%, unproven, needs about four times the trades". The same mistake was made on the trending replay.

A lower bound is a reasonable way to *pick* among many candidates (it penalises small, noisy samples). It is not a reasonable way to *announce* that nothing works. Fixed: both replays now state the mean, the interval and the minimum detectable effect at the current sample, and never print "profitable" or "not profitable".

### 1.2 The curve replay never tested the strategy you described

Your thesis: enter early, hold runners for hours, sit through 50–80% retracements, ladder out of the winners. The curve replay's exit grid was: take-profit 30–200%, stop-loss 15–30%, trailing 25%, **maximum hold 60 / 180 / 600 seconds**, on a tape that ends **one hour** after each coin's creation. It tested ten-minute scalps with tight stops. 187 of its 276 out-of-sample trades ended on the time limit. That grid cannot hold a runner, cannot survive a 50% retracement, and cannot see a coin that does 10× over three hours. The result "−3.8% per trade" is a statement about scalping new coins with 2.5% round-trip fees. It is not a statement about your strategy, and I presented it as one ("the launch tier is dropped").

### 1.3 The trending replay bought the pump

Its entry triggers were "enters the 1 h trending top 20/100", "enters the organic top 20/100", "in the 5-minute top 20". A coin enters the 1 h trending list because it already moved in the last hour. That is the opposite of catching a coin before the manual traders. Its exits were the same tight stops on 5-minute data where single candles move 20–30%: 26 of 44 out-of-sample trades were stop-losses. The result (−16% a trade) is what buying tops with tight stops does; it says nothing about an early-breadth entry.

### 1.4 Fifty thousand pairs on two blocks of data

Picking the best of 43,200 (curve) or 51,840 (trending) pairs on a few hundred coins, then scoring it on the next six-hour block, produces noise in both directions. With 12 hours of data the walk-forward has two usable blocks; its line was reported as a result. It was not one. The search is still useful (it finds the shape of what works) but its out-of-sample line needs days of blocks before it means anything, and the report now says so in numbers (the minimum detectable effect) rather than in adjectives.

### 1.5 A gate compared across different labels

The WO-3 gate asked for a holders@15 s lift of ≥ 5× because a published study found 10.6× on the label "doubles again". Our label is "doubles within the hour after 15 s", with a base rate of 10.8%. On it, holders ≥ 30 gives 3.1–3.7× lift, i.e. **34–38% of those coins double within the hour**, in both chronological halves. That is the strongest early-signal number in the data and it was filed as a failed gate.

## 2. What the data supports, measured the direct way

No rule search. One cohort, one entry, two exit policies, the whole first hour of every coin (5,566 coins with a 15 s book and a full-hour outcome, Actions runs 1–2, 1.25% fee a side, fill 4 s after the 15 s book):

| cohort at 15 s | coins | peak ≥ 2× | ≥ 3× | ≥ 5× | at 1 h: ≥ entry | at 1 h: ≤ −50% | hold the hour, no stop: mean | ladder (½ at 2×, ¼ at 4×, rest at 1 h), no stop: mean (95% CI) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| all coins | 5,566 | 11% | 5% | 3% | 31% | 24% | −13.2% | −12.2% (±1.7%) |
| holders 10–19 | 284 | 13% | 7% | 3% | 6% | 13% | −21.0% | −18.2% (±6.2%) |
| holders 20–29 | 116 | 25% | 17% | 9% | 7% | 32% | −6.9% | −10.3% (±15.6%) |
| **holders ≥ 30** | 128 | **34%** | **17%** | **8%** | 16% | 55% | −6.0% | −7.5% (±16.0%) |
| holders ≥ 50 | 41 | 39% | 20% | 10% | 22% | 71% | +4.0% | −2.5% (±30.9%) |
| net SOL in ≥ 5 | 516 | 24% | 13% | 6% | 11% | 32% | −14.2% | −13.5% (±6.8%) |

Reading it straight:

- The early-breadth signal is real: a coin with 30+ holders at 15 s is three times as likely as the average coin to double, and one in twelve goes 5× **within the hour**.
- Held through the hour, the same cohort is at −50% or worse 55% of the time, and the ladder averages −7.5% ± 16%. Inside the first hour, with no stop, the losers cost about what the winners pay. The interval is wide: the true figure is somewhere between −24% and +9% per trade. Not proven either way; not "nothing works".
- Nothing here looks at hour two onward. The runners you describe are made after the first hour, on the pool. The tape only started following pools yesterday, so this is the part that is genuinely untested, not refuted.

## 3. What is right and stays

- The recording: every curve trade, every pool trade of graduated coins, 5-minute paths of listed coins (now continued for a day after they leave the lists), SOL price, insider share, sells by wallets that never bought. No vendor labels; nothing is read from the future.
- The fill model: exact Pump.fun curve formula with our size in the curve, the recorded fee, our measured delay; constant-product impact against the pool's liquidity on the trending tier.
- Walk-forward and the holdout: the right protocol, once there are enough blocks to run it on.
- The findings that did not depend on the replays: pre-funded launches are the $100k population; half of them are insider-held fabricated caps; graduations are sold into; round numbers do nothing on the curve.

## 4. Changes

Done in this commit:

1. Both replays print the mean, the interval, and **the minimum detectable effect at the current sample** ("with n trades and this spread, the smallest edge this test can see is ±x% per trade"); the binary sentence is gone.
2. The daily report gains **the hold thesis, first hour**: the table above, recomputed every run on all the data, by cohort, with intervals. A direct test, no search, no gate.
3. The plan's "launch tier dropped" is withdrawn and replaced by what the data supports: ten-minute scalps of new coins with tight stops lose; the hold-for-hours thesis is untested until the full-life replay below exists.

Next, in order:

4. **Full-life paths.** One price path per coin: curve trades (first hour) → pool candles (24 h, started 2026-10-09) → checkpoints (3 h, 6 h, 24 h). The curve replay then holds for 1 / 6 / 24 h, with ladder exits (⅓ at 2×, ⅓ at 4–5×, trail the rest), trails of 50–70% measured from the peak, and the choice of no stop. This is the test of the thesis; everything before it was not.
5. **Early triggers on the trending tier**: organic buyers and holder growth rising while market cap and liquidity are still small, with the follow rows supplying the fall as well as the rise; "enters the top 100" stays as the control that it is.
6. **Fewer, named hypotheses** instead of a 50,000-pair grid: each hypothesis in [11](11-trading-decisions-data.md) §4 gets its cohort table with intervals, and the grid search is kept for shape-finding only, with its multiple-comparison count printed next to every number it produces.

What I will not change: the fees, the delay, the impact model and the no-lookahead rule. They are the reason a result here will survive contact with real money.

## 5. Working backwards from the winners (2026-10-10, evening)

Asked for after: *"The whole point of identifying trending and top runners each day is to find the metrics and data that singles them out at as early stage as possible, and THAT IS our entry. We should be in the coins within seconds of that signal. Now audit all the conventional junk we seem to have been contaminated with."*

The order was wrong. The replays asked "which entry × exit pair makes money" before asking "who are the winners and what did they look like in their first seconds". That is the standard backtest template and it is the wrong order for this market, where a coin's future is decided in its first seconds by who shows up. `copybot winners` now does it in the right order, on the whole tape, every run: winners → fingerprint → earliest signal → what entering at that signal pays. The full report for the three Actions runs so far (33,378 coins, 16.4 h) is in [reports/winners-2026-10-10.md](reports/winners-2026-10-10.md).

### 5.1 The conventional practices, named, and what replaces them

| what was done | why it is wrong here | replaced by |
|---|---|---|
| Discovery by exit grid: 43,200 (curve) and 51,840 (trending) entry × exit pairs, the best lower bound wins | The exit grid cannot find a winner it never defined; it finds the exit that best fits noise | Winners defined first (liquidity-backed peak ≥ $30k, ≥ $100k), then fixed rules at 5–300 s with recall, precision (Wilson), lift and payoff. 17 rules, stated |
| Decisions at fixed moments (15/30/60 s), fills 4 s later, holds ≤ 10 min | A signal does not wait for a snapshot; a runner is not held for ten minutes | The exact second a signal first holds on the trade tape, our fill `delay` later with our size in the curve, exits that hold the hour, ladder out, trail from the peak, no stop |
| "Enters the trending top 20/100" as an entry | The list is where the crowd sees the coin; buying there is buying the pump | The lead is measured: the signal first holds at a median **2 s** after creation; the lists show the coin at a median **7 min** |
| Verdicts at 95% confidence (§1) | A sample of 56 cannot see a 9% edge | Mean, interval, smallest visible edge, Wilson intervals on every proportion |
| The label "doubles within the hour from the 15 s price" | A relative label on a collapsing price: 1,740 of the first pass's "winners" were Mayhem-mode coins whose market cap had fallen to 3 SOL by 60 s | Absolute, liquidity-backed labels; Mayhem-mode coins (28% of launches) segmented; pre-funded and insider-held coins left out |
| Gates copied from papers ("lift ≥ 5× or the launch tier is dropped") | A gate on another study's label decides nothing about ours | No gates. A table per moment; the reader sees recall and precision and decides |
| The wallet list taken as a signal | Never tested as one | Tested on the tape: the leaders buy a new coin within 60 s 19 times in 16 h, precision 5%. They are scalpers (31 s–3 min holds), not early buyers |
| The best in-sample pair reported as a result | It is the best of 50,000 | Every table prints how many rules it chose from; the entry replay chooses nothing: every fire is a trade |

### 5.2 What singles the winners out, and how early

Standard coins (not Mayhem-mode, not pre-funded, not priced in another token): 21,592 with complete 5 s and 15 s books. 259 reached $30k (1.20%), 27 reached $100k (0.13%), 13 reached $250k, 1 reached $1M.

The fingerprint at **5 s** (median, winners ≥ $30k against all coins): holders **24 vs 1**, buyers 30 vs 1, net SOL in the curve **21.8 vs 0.1**, new buyers in the last 5 s 18 vs 0, HHI 0.10 vs 0.38, market cap 83 SOL vs 28. The winners are visible in the first five seconds, and the thing that shows them is money: SOL in the curve.

| moment | rule | fires/day | recall | precision ≥ $30k (95%) | lift | winners' median peak from there |
|---|---|---:|---:|---|---:|---:|
| 5 s | net SOL ≥ 20 | 718 | 54% | 28.7% [24.8–32.8] | 24× | 3.2× |
| 5 s | net SOL ≥ 30 | 274 | 33% | **45.2%** [38.3–52.4] | 38× | 2.6× |
| 15 s | net SOL ≥ 30 | 369 | 47% | 48.2% [42.1–54.4] | 40× | 2.6× |
| 60 s | net SOL ≥ 30 | 384 | 54% | 53.6% [47.6–59.5] | 45× | 1.9× |
| 300 s | net SOL ≥ 30 | 366 | 58% | 60.2% [54.0–66.0] | 50× | 1.6× |

For 257 of the 259 winners the rule "net SOL ≥ 20" first holds at a median of **2 s** after creation. The first checkpoint reading ≥ $30k is at a median of 5 min; the first appearance on any trending list at a median of 7 min (and 41% of the winners never list at all). The engine's lead over the crowd is about seven minutes. That part of the thesis holds.

Where it does not: the **$100k class**. The best first-minute rule reaches 4.8–5.7% precision for ≥ $100k (lift 38–45×, but 1 in 20). The ≥ $100k coins' fingerprint at 5 s is *weaker* than the $30k–$100k coins' (16 holders vs 24, 11 new buyers vs 18): the biggest winners are not the most explosive first seconds. A third of them show nothing in the first minute at all (3–6 holders at 15 s; net SOL ≥ 20 only at 2–30 min) and are first visible at the 5–30 min stage. No rule on the first-minute book gets near a 50/50 chance of $100k; the book alone does not give that target.

### 5.3 What entering at the signal pays

The entry replay: the exact second each signal first holds, our 0.5 SOL buy landing 4 s later, our size in the curve, fees as recorded, every fire a trade. 9 signals × 5 exits, fixed before looking.

| signal | fires/day | entry | hold to 1 h, no stop | ladder ½ at 2×, ¼ at 4× | trail 50% from peak | stop −50% |
|---|---:|---:|---|---|---|---|
| net SOL ≥ 20 within 60 s (1,199 fires) | 1,750 | $9k | **−18.8%** (−25.3 to −12.3) | −15.7% (−20.5 to −10.8) | −14.3% (−19.5 to −9.2) | −16.7% |
| net SOL ≥ 30 within 60 s (610 fires) | 890 | $13k | **−16.6%** (−25.3 to −8.0) | −14.7% (−21.8 to −7.6) | −14.4% (−21.2 to −7.7) | −15.9% |
| net SOL ≥ 20 & holders ≥ 30 within 60 s (955) | 1,394 | $10k | −15.8% (−23.4 to −8.1) | −13.8% (−19.5 to −8.1) | **−11.4%** (−17.5 to −5.3) | −13.0% |

All 60 pairs are negative, every interval excludes zero, both chronological halves agree, and a bankroll run from 1 SOL at 10% a trade goes to zero. (That exit comparison was the last grid in the code and is gone with the rest, §5.4; the report now prints what the position is worth after the signal, at the end of the hour, at its peak on the way, and at 6 h and 24 h. For net SOL ≥ 20 within 60 s: at 1 h mean −18.8% (±6%), median −63%, 11% at or above 2×; peak within the hour median +31%, 27% at or above 2×, 2% at or above 5×; at 6 h on the 162 fires old enough: mean +11% (±108%), median −65%, 5% at or above 2×.) The reason is in the detector table: the signal is already priced. Twenty to thirty SOL in the curve is a $9–13k coin; $30k is 2.6–3.2× away, and that is the winners' *median peak*, not their exit. The other 55–70% of fires sit at −70% an hour later (losers' median 0.2–0.3×). A 29–45% hit rate on a 2–3× peak does not pay for a 70% loss on the rest, with any exit inside the hour.

**Beyond the hour**, which is where the thesis lives: the 6 h checkpoint column. For net SOL ≥ 20 at 5 s, the 67 fires old enough to have one average **+95% (±259%)**, median −72%, 9% at or above 2×. For net SOL ≥ 30: +240% (±540%) on 32. One coin (TM: $18k at 15 s, $1.2M peak) is most of each mean. The sample cannot tell +95% from −50%. What it says is that the hold-for-hours thesis stands or falls on how often a 50–100× coin comes, which 16 hours cannot measure and days can: the Actions census takes 6 h checkpoints across runs (the resume works) and 24 h ones from tomorrow, and the table recomputes every run with its interval.

**Wallets**: early buyers of the first-half winners that are *selective* (≥ 2 winners, ≥ 25% of their early buys winners; the bots that buy every launch are excluded) give, on the second half, 9.5% [7.6–11.7] precision for ≥ $30k (lift 12×, recall 85%) and 1.0% for ≥ $100k. Weaker than SOL-in-curve alone, and negative in the entry replay (−10 to −17%). The leaders file: 19 fires, 5%. Who is behind the bundle is still the open discriminator, and the first-minute wallet overlap does not capture it; the creator index (prior launches per creator wallet) is the next test.

### 5.4 What changes

1. **The entry is a signal, not a list.** "Net SOL ≥ 20–30 within 60 s" is the launch-tier trigger the state machine watches: it holds at a median 2 s after creation, seven minutes before the trending lists, and 29–45% of its fires become $30k coins. Nothing else on the first-minute book adds to it.
2. **The first-hour trade from that signal is a measured loser at every exit and is not traded**, shadow or live. Not "unproven": −11 to −19% per trade with intervals that exclude zero.
3. **What stays open, free, and now measured every run:** (a) the multi-hour hold from the signal (the 6 h and 24 h columns, with intervals; it is tradeable the day the interval excludes zero on the upside, not before); (b) the slow-burners' stage at 5–30 min, before the lists (holders and net SOL rising while the market cap is still under $10k) — the trending tier's early trigger, change 5 of §4; (c) the creator index, task #17.
4. `copybot winners` runs in every Actions census. The grid replays (`copybot replay`, `copybot replay-trending`), the walk-forward and holdout machinery around them, and the daily report's lift tables and WO-3 gate are **removed** from the code (2026-10-10, late evening: "fuck grids and conventional junk"). What remains of the replay code is the tape model: exact curve fills for our size, the recorded fee, our delay, and the signal's first-crossing time. There is no exit grid anywhere: the report shows what the coins do after the signal, at 1 h, 6 h and 24 h, and the riding policy is designed from that, not searched.
