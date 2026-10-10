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
