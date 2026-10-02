# 04 — Leader selection: finding wallets worth copying

**Core rule: score a wallet on what a copier would have earned, not on what the wallet earned.**

On a bonding curve the copier always buys after the leader, at a worse price. Published 2026 research measured smart-money trades at about +14% per trade but realistic copier returns at about +3%. Most top-ranked wallets are bots whose edge depends on being first, so their copier return is negative.

## Pipeline

```
discover → backfill → reconstruct round trips → simulate copies → score + gates → probation (shadow) → active → monitor → retire
```

> **Free starting workflow (no backfill vendor, no paid feed).** The pipeline below is the full version. To start for free: take candidates from a public list (GMGN, Axiom, KOLscan; their daily boards are survivorship-biased, so treat them as candidates only), screen them for activity, vet each with `copybot leader-report` (step 3 from the chain's own history, free), then let **paper mode** do step 4 live: it enters 1.2 s after each leader buy at the price the pool really shows then and exits with our policy, and `copybot report` prints the imitation penalty's end result (copier PnL after costs, per leader). See [RUNBOOK](../RUNBOOK.md) Path A.

### 1. Discover candidates
- **Our own firehose:** wallets with high realised profit on universe tokens over 30 and 90 days.
- **Public leaderboards and tags:** GMGN smart money and KOL lists, Axiom pro traders, KOL trackers.
  - Treat these as *candidates only*. Public labels attract copiers, and some wallets are built to look smart.
- **Manual adds:** wallets you already know. Add them as `candidate`, never straight to `active`.

### 2. Backfill
- Pull 90 days of swaps for each candidate from Bitquery, Birdeye or our archival RPC.
- Also pull **every pool trade around each of their trades** (from a few seconds before their entry to well after their exit). Copy simulation needs the pool's trade sequence, not just the leader's fills.

### 3. Reconstruct round trips
- Group by (wallet, mint). Match buys to sells FIFO and handle partial sells.
- Record cost, proceeds, hold time, and `entry_slots_after_creation`.
- Follow tokens transferred out to other wallets. A transfer is a hidden exit or a position split, not a hold. The funding graph (see [03](03-data-model.md#wallet-tags-the-hard-valuable-part)) catches multi-wallet operators.

### 4. Simulate copies (the key step)
For each round trip, compute `copy_ret`:
1. Enter `d` slots or `k` transactions after the leader's buy, at the price the pool actually showed then, with our size added to price impact. Use `d` = our measured p50 and p90 `slots_after_leader`; start with 1 and 2 before we have live data.
2. Exit with the **policy we would actually run** (`engine-core::replay` over the recorded path). Also record the "mirror the leader's exit with delay `d`" baseline.
3. Apply the `CostModel`: venue fees per side, tip and priority fee per transaction, slippage, and ATA rent net of reclaim.

`imitation_penalty = leader median return − copier median return`. This is a per-wallet number and one of the best selection signals: small for slow swing traders, enormous for snipers.

### 5. Score and gates (`engine-core::wallet_score`)
Hard gates (defaults to tune):

| Gate | Default | Why |
|---|---|---|
| `min_trades` | 30 round trips / 30 d | Sample size |
| `min_median_hold_secs` | 120 s | Seconds-long flips can't be copied |
| `max_sniper_share` | 0.3 | Entries mostly within 2 slots of creation suggest insider or bundle activity |
| `max_active_hours_per_day` | 16 | Bot filter (2026 study: 93 of the top 100 traders are active >18 h/day) |
| `min_copy_median_ret` | > 0 after costs | The only thing that matters |

**Score** = `copy_median_ret × n / (n + prior)`. This shrinks thin samples toward zero. Report but don't gate on:
- win rate, profit factor and drawdown;
- **consistency** across weekly buckets (one lucky 100x shouldn't carry a wallet);
- **style**: median hold, market-cap band, venues and active hours. Style drives per-leader exit and sizing overrides.

### 6. Probation → active
- **Probation:** shadow mode only (`mode = shadow` or `enabled = false` for that leader). Collect at least 30 live signals.
- **Promote to active** when the *live* simulated copy return (real detection latency, real pool state) is consistent with the backfill estimate. Within the bootstrap CI is good enough.
- **Size ramp:** start at 25–50% of target `copy_pct`, then raise with evidence. Later phase: fractional Kelly on the posterior of copy return, hard-capped.

### 7. Monitor and retire (automatic)
Re-score nightly over a rolling window, adding `live_copy_median_ret` from our own fills. Pause or retire a leader when:
- **its rolling realised copy return falls below a threshold** (e.g. 30 trades). This is a per-leader circuit breaker;
- **its style drifts**, e.g. median hold collapses or it starts sniping;
- **it shows signs of exploiting copiers:**
  - it sells right after our buys land, repeatedly;
  - its buys are immediately followed by a cluster of copier buys and then its exit;
  - its funding links to tokens' dev clusters;
- **it goes dormant**, or a new wallet funded from it appears. That is a likely rotation, so add the new wallet as a candidate.

## Portfolio of leaders
- **Diversify by style:** swing traders, migration traders, narrative/KOL followers. Correlated leaders all buying the same token is concentration risk. Merge simultaneous signals on the same mint into one position with a combined size cap.
- **Leader agreement is a feature.** Several independent tracked wallets buying the same token within minutes is a stronger signal than any single one. It becomes an entry-model input and, eventually, a leader-free signal.
