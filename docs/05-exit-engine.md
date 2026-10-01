# 05 — Exit engine: from rules to a self-calibrating policy

The leader decides *what* and *when* to buy (at first). **We** decide when to sell. This doc describes how the exit gets dialled in from data and then keeps re-calibrating as conditions change.

## Stage A — composable rule policies (implemented)
`engine-core::exit::ExitPolicy` combines these rules:

| Rule | Parameter(s) |
|---|---|
| Stop loss | `stop_loss_pct` |
| Take-profit ladder | `take_profit = [{at_multiple, sell_fraction}, …]` (fractions of the initial position) |
| Trailing stop | `trailing = {activate_at_multiple, drawdown_pct}` |
| Time stop | `max_hold_secs` |
| Stale stop | `stale_secs` (no new high for N seconds) |
| Leader sold | `on_leader_sell = ignore \| mirror \| sell_all \| tighten{drawdown_pct}` |
| Dev sold | `exit_on_dev_sell` |
| Liquidity pulled | `liquidity_drop_pct` (vs pool SOL at entry) |

**Shadow policies.** The live position runs one policy. Every policy listed in `shadow_exits` runs on the same event stream and logs what it *would* have done. `mirror_leader` is always a shadow baseline: it answers "is our exit actually better than just following the leader?".

**Counterfactual replay** (`engine-core::replay`). Because every pool trade after entry is recorded, any new policy can be scored on every historical position, including leader signals we skipped. Exit research is therefore **full-information**: we observe the outcome of every policy on every position, not just the one we traded. That makes exit tuning a supervised-learning and optimisation problem rather than a slow exploration problem.

Replay caveats to keep honest:
- **Our own sells don't move the recorded price.** Keep sizes small relative to the pool (`max_pool_impact_pct`) and add an impact term to the cost model.
- **Fill prices.** Replay fills at the observed price with a slippage haircut. Calibrate `CostModel` from the `orders` table (expected vs fill, per venue and size bucket) and re-calibrate weekly.
- **Latency on exits.** Replay can apply a sell delay, e.g. our measured p50 time to land a sell. Without it, tight trailing stops look better than they are.

## Stage B — data-tuned parameters (first research milestone)
1. **Dataset:** all positions plus all skipped and probation signals, each with its post-entry event path.
2. **Search:** Bayesian optimisation (optuna) over policy parameters. Objective: median or trimmed-mean return after costs, with a drawdown penalty, and constraints such as max hold or minimum win rate if desired.
3. **Walk-forward validation:** tune on weeks `t-4…t-1`, test on week `t`, roll forward. Only out-of-sample results count. Report the distribution of OOS results, not the best in-sample run.
4. **Segment where the data supports it:** separate parameter sets by leader style, venue (curve vs migrated), market-cap band, token age and regime. Use hierarchical shrinkage (segment parameters pulled toward the global set) so thin segments don't overfit.
5. **Output:** a new named policy in `[exits]`. It enters `shadow_exits` first, never straight to live.

## Stage C — context-aware policy selection
At entry, pick *which* policy (from a menu of ~5–20 diverse policies) to run, given the decision-time snapshot.
- Train a model that predicts each policy's return from snapshot features. With full information this is plain multi-output regression or classification (LightGBM). Choose the argmax, or use a risk-adjusted choice.
- Optionally keep a small exploration rate (Thompson sampling) to cover drift in fill quality that replay can't see.

## Stage D — dynamic, state-based exits
Replace fixed thresholds with a model evaluated continuously while the position is open:
- **Hazard / survival view:** at each tick, estimate P(drawdown ≥ X before gain ≥ Y within T) from rolling features:
  - flow imbalance, transactions-per-second decay, unique-trader decay;
  - smart-money and leader net flow, dev or insider selling;
  - holder concentration changes, social velocity.
- **Decision rule:** sell a fraction when expected forward return after costs turns negative, or when the hazard crosses a calibrated threshold. Keep the Stage A rules as **hard guards** (stop loss, liquidity pull, dev dump) that the model can't override.
- **Training data:** the entire firehose. Every token at every snapshot is a training example, not just tokens we held. This is the same model family the leader-free engine needs for entries.

## Self-calibration loop (champion / challenger)

```
nightly:
  recompute labels + replay all challengers on new positions
  update cost model from orders
  walk-forward re-tune (Stage B), retrain (C/D) on a rolling window
  drift checks: feature PSI, live-vs-replay gap, per-segment performance
weekly (or when evidence threshold met):
  challenger vs champion on the last N positions (OOS, paired):
    promote if bootstrap 95% CI of the paired return difference > 0
       AND max drawdown not worse by > X
       AND sample ≥ N_min (e.g. 200 positions)
  promotion = config change with new policy hash, logged; auto-rollback if live
  performance falls below the challenger's lower CI band for K days
```

**Guardrails:**
- The loop may change **exit and entry-model parameters only**. Risk limits, bankroll caps, leader activation and custody are human-controlled.
- Parameter search spaces are bounded, e.g. stop loss in [10%, 60%].
- Every promotion is a versioned, reversible config change. At first, promotions require a manual approve flag from the control plane. Remove the flag only after the loop has a track record.
- **Regime awareness:**
  - Regime features (launch rate, migration rate, meme volume, SOL trend, meme-basket trend score) are model inputs.
  - Rolling windows are short enough (e.g. 2–6 weeks) to adapt, but sample sizes come first.
  - If a regime shift is detected (feature PSI spike, or performance outside its band), fall back to the conservative baseline policy until enough new-regime data accumulates.

## Entry gets the same treatment (meta-labelling)
The copy signal is the "primary model". A secondary model predicts P(copy is profitable after costs | snapshot, leader context) and:
1. **vetoes** low-probability signals;
2. **scales** size (bounded) for high-probability ones.

Train it on all copy signals, including skips and probation leaders, with labels from replay. Its score and version are logged on every signal. That is the first step to [leader-free trading](07-roadmap.md#phase-5--leader-free-engine).
