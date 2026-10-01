# 03 — Data model: what we record and why

The trade log is the asset. Every later stage depends on it: exit tuning, leader re-scoring, entry models, and the leader-free engine. These are the principles behind the schema in [`schema/`](../schema).

## Principles
1. **Record the world, not just our trades.**
   - Store every swap on every token in our universe (`mkt.swaps`).
   - Our positions are a tiny, biased sample. The firehose is what lets us replay exits, simulate copies of leaders we didn't copy, and later trade without leaders.
2. **Skips are data.**
   - Every leader buy we saw goes into `copy_signals` with `decision = buy|skip` and the reason.
   - Without skips, filter tuning only ever sees survivors.
3. **Decision-time snapshots.**
   - At each decision (leader buy seen, our entry, each exit), write a full feature snapshot (`mkt.token_snapshots`, `trigger = …`). The journal row points to it.
   - Features must be computed *only* from data available at that instant. Lookahead is the single most common way trading models lie.
4. **Full post-entry paths.**
   - Keep recording each token's swaps for at least 24 h after our last exit, or until activity dies.
   - This is what makes counterfactual exit replay (`engine-core::replay`) and MFE/MAE possible.
5. **Execution is a first-class dataset.**
   - Store every order with its senders, tip, priority fee, CU, expected vs fill price, `slots_after_leader` and `txs_between_leader`.
   - The imitation penalty is measured here, and the replay cost model is calibrated from here.
6. **Version everything that decides.**
   - Exit policy hash, model version and config version go on every row.
   - Otherwise results can't be attributed after a change.

## Feature catalogue (token snapshot dimensions)

The panels GMGN, Axiom and Padre show, plus what we need for modelling. Column names match `mkt.token_snapshots`.

| Group | Features | How computed |
|---|---|---|
| Identity / lifecycle | `age_secs`, `launchpad`, `venue`, `migrated`, `curve_progress` | Token create event; curve account reserves vs graduation threshold |
| Price / size | `price_sol`, `mcap_usd`, `liquidity_sol`, `sol_usd`, `ath_price_sol`, `drawdown_from_ath` | Pool reserves; supply; SOL/USD feed |
| Flow (5 s, 1 m, 5 m, 1 h) | `buys_*`, `sells_*`, `vol_sol_*`, `net_flow_sol_*`, `tps_1m`, `unique_traders_5m`, `new_wallets_5m` | Rolling windows over `mkt.swaps` |
| Returns / volatility | `ret_1m`, `ret_5m`, `ret_1h`, `realized_vol_5m` | From swap prices |
| Distribution | `holders`, `top10_pct`, `dev_holding_pct`, `sniper_pct`, `bundler_pct`, `insider_pct`, `fresh_wallet_pct` | Holder balances × wallet tags (below) |
| Wallet intelligence | `smart_money_holders`, `kol_holders`, `tracked_leader_holders` | Holder set ∩ our tagged wallet sets |
| Dev history | `dev_prior_launches`, `dev_prior_migrations`, `dev_prior_rug_rate`, `dev_sold` | Creator address (and its funding cluster) across `mkt.token_events` |
| Safety | `mint_authority_revoked`, `freeze_authority_revoked`, `lp_burned_pct`, `token_2022_extensions`, `creator_fee_mode` | Mint account, LP mint, Token-2022 extension parsing, launchpad config |
| Social / attention | `has_twitter`, `has_telegram`, `has_website`, `twitter_handle_reused`, `twitter_followers`, `mentions_5m`, `mentions_1h`, `kol_mentions_1h`, `dex_paid`, `dex_boosts` | Metadata URI JSON; X API (pay-per-read, so enrich selectively); DexScreener |
| Regime context | `sol_ret_1h`, `launches_1h`, `migrations_1h`, `meme_volume_1h_sol` | Aggregates over the whole firehose |

Planned additions, in priority order:
1. Leader-specific context at signal time: the leader's recent copy-return streak, and whether the leader already holds the token (an add vs a first buy).
2. Order-flow toxicity: share of volume from tagged bots and snipers.
3. Concentration change rates: Δtop10 over 5 m, smart-money net flow.
4. Meme-beta regime from the [cross-asset basket dashboard](../tools/dashboards/basket-dashboard.html). Its 8-factor trend score on USELESS, Fartcoin and WIF is a cheap "is meme risk-on" gauge.

## Wallet tags (the hard, valuable part)

Tags drive the distribution features. They are computed by the research pipeline and stored per wallet with a confidence score and the time they were assigned. A tag never uses information from after the snapshot it feeds.

| Tag | Heuristic (v1) |
|---|---|
| `dev` | Token creator, plus wallets in the creator's funding cluster |
| `sniper` | Bought within ≤ 2 slots of token creation |
| `bundler` | Bought in the creation slot via the same bundle or the same funder as other buyers |
| `insider` | Funded by the dev or the dev's funder within X hours before launch, or received tokens by transfer from the dev |
| `fresh` | First funded < 72 h ago and fewer than N prior transactions |
| `bot` | Active in more than 16–18 distinct UTC hours per day, or median hold under 10 s |
| `kol` | Curated list of public KOL wallets, refreshed periodically |
| `smart` | Our own score ([04](04-wallet-selection.md)): positive *simulated copier* return over a rolling window |
| `cex` | Known exchange hot wallets (funding-source labelling) |

**Funding graph.** For every wallet that trades a universe token, record its first SOL funder and later large inflows. Clusters built from this graph are the backbone of the insider, bundler and dev-cluster tags. They also catch leaders who split positions across wallets.

## Execution metrics (per order)
- `detect_latency_ms`, `detect_slot_lag`, `detect_source`
- `senders[]`, `landed_by`, `priority_fee`, `tip_lamports`, `cu_limit`, `cu_used`
- `expected_price`, `fill_price`, `slippage_bps_real`
- `sent_slot`, `landed_slot`, `slots_after_leader`, `txs_between_leader`
- `status`, `error`

These feed four things:
- the tip and fee controller;
- per-sender routing weights;
- the replay `CostModel` calibration;
- the leader-level imitation penalty.

## Outcome labels (derived, for modelling)

Computed by research jobs from `mkt.swaps` relative to any snapshot time `t`:
- forward returns at 30 s, 1 m, 5 m, 15 m, 1 h and 4 h;
- forward max gain and max drawdown (MFE/MAE) over the same horizons;
- triple-barrier label `(+X%, −Y%, T)`: which barrier is hit first;
- time to death (volume < ε) and whether the token migrated.

Labels live in derived tables or views, never in the snapshots themselves, so snapshots stay free of lookahead.

## Volumes and retention (rough planning numbers; measure early)
- **Swaps:** the Solana meme universe produces millions of swaps per day. Expect order-of a few GB/day compressed in ClickHouse for the firehose (to be measured). Raw swaps are kept 400 days (TTL in the schema). Older data can be downsampled to 1 s bars.
- **Snapshots** are the largest table if the universe cadence is aggressive. Start at 60 s for the universe and 5 s for held tokens.
- **Postgres journal** is small (thousands of rows per day).
