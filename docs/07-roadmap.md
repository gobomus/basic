# 07 — Roadmap, gates and unit economics

Each phase ends in a **go/no-go gate** decided from data in the journal. Real money only goes in after the gates say the edge survives costs.

## Unit economics

Fixed cost per transaction (tip + priority fee) is assumed at ~0.001 SOL; the Helius Sender Max minimum tip alone is 0.001 SOL. Venue fee example: Pump bonding curve at 1.25% per side. Migrated pools are cheaper; use real per-venue fees from data. ATA rent is reclaimed on close, so it is excluded.

| Copy size | Fixed cost, 1 buy + 1 sell | Venue fees, 2 × 1.25% | **Hurdle before slippage and imitation penalty** |
|---|---|---|---|
| 0.05 SOL | 4.0% | 2.5% | **6.5%** |
| 0.10 SOL | 2.0% | 2.5% | **4.5%** |
| 0.25 SOL | 0.8% | 2.5% | **3.3%** |
| 0.50 SOL | 0.4% | 2.5% | **2.9%** |
| 1.00 SOL | 0.2% | 2.5% | **2.7%** |

- Every extra take-profit leg adds one more fixed cost.
- **Implication:** below ~0.1 SOL per copy, the leader's edge has to be very large to survive. `min_buy_sol` exists for this reason.
- If the bankroll is small, copy **fewer leaders at a larger percentage** rather than many leaders at dust sizes.

**Infrastructure budget (monthly, rough; verify current pricing):**

| Item | Estimate |
|---|---|
| Geyser gRPC with shred access (e.g. Helius Business $499 / Professional $999 for LaserStream mainnet, or an equivalent Yellowstone provider) | $500–1,000 |
| Second feed provider (redundancy) | $200–1,000 |
| Dedicated server in the chosen region | $150–600 |
| X API pay-per-use, selective enrichment | $50–300 |
| Backfill data (Bitquery / Birdeye), mostly one-off | plan-dependent |

**Total: roughly $1–3k/month.** The copy engine has to clear this before it "builds the account". Phases 0–1 can run on a single provider at the lower tier.

## Phase 0 — Foundation (weeks 0–2)
- [x] Repo reorganised; legacy archived.
- [x] `engine-core`: types, config, sizing, exit policies, replay, wallet scoring, with tests.
- [x] Schemas: ClickHouse firehose and snapshots; Postgres journal.
- [ ] Provider accounts: gRPC plus shred feed, Sender / Jito / one more landing service.
- [ ] `ingest` + `decoders` crates in this order:
  1. Pump curve and PumpSwap;
  2. Raydium LaunchLab and CPMM;
  3. Meteora DBC and DAMM v2;
  4. Raydium AMM v4 and CLMM, Meteora DLMM, Orca;
  5. Jupiter inner routes.
- [ ] `recorder` into ClickHouse; Grafana with the basic latency and coverage panels.
- [ ] `research/` Python package: backfill, round-trip reconstruction, copy simulation (reuses replay logic via a small CLI or PyO3 binding), wallet scoring.

**Gate 0:**
- Decode coverage ≥ 99% of candidate leaders' swaps over 7 days.
- Measured `detect_latency_ms` p50/p90 per source.
- At least 5 candidate leaders pass the wallet-score gates on simulated copier return.

## Phase 1 — Shadow (weeks 2–4)
- Engine runs live with `mode = "shadow"`: real signals, real decisions, real snapshots; no orders.
- Shadow exits active. Feature service writing snapshots at decision points.
- Telegram alerts; kill-switch plumbing tested.

**Gate 1:**
- At least 300 shadow signals.
- Simulated copier return after costs (using the measured detection latency and an assumed +1 slot to land) is positive, with a bootstrap CI above 0, on the leader set as a whole.
- At least 3 leaders are individually positive.

## Phase 2 — Live, small (weeks 4–8)
- `mode = "live"` with minimum viable size (`min_buy_sol` to ~0.1 SOL) on the leaders that passed Gate 1.
- Calibrate the `CostModel` from real orders: slippage by venue and size, landing rate by sender, `slots_after_leader`.
- Tune tips and senders against the measured landing curve.

**Gate 2:**
- At least 200 live round trips.
- Landed rate ≥ 90%.
- Realised returns within tolerance of shadow-simulated returns. If the gap is large, fix execution before scaling.
- Positive expectancy after all costs, including infrastructure.

## Phase 3 — Scale and tune (months 2–4)
- Ramp size per leader with evidence (fractional Kelly on the posterior, hard caps).
- Stage B exit tuning (walk-forward) live; challengers in shadow; first manual promotions.
- Leader lifecycle automation: nightly re-score, probation, retire, rotation detection.
- Hot-wallet rotation and sweeping automated.

**Gate 3:**
- Exit policy chosen by data beats the `mirror_leader` baseline out-of-sample (paired bootstrap).
- Account growing net of infrastructure cost.

## Phase 4 — Learned models (months 4–8)
- Entry meta-label model: veto and scale copy signals.
- Stage C context-aware exit selection; Stage D hazard-based dynamic exits behind the hard guards.
- Champion/challenger loop automated, with manual approval until it has a clean track record.
- Regime detection and fallback policy.

## Phase 5 — Leader-free engine
- Train entry models on the **whole firehose**: every token at every snapshot, labelled with forward outcomes. Leader activity ("k tracked wallets bought in the last 5 m") becomes a *feature*, not a trigger.
- Run leader-free entries in shadow next to copy entries. Same exit engine, same journal, same gates.

**Graduation gate:** over at least 8 weeks, leader-free shadow returns after costs match or beat copy returns with lower drawdown. Then shift capital gradually; copying stays on as a signal source.

## Decisions needed from you
1. **Budget:** monthly infrastructure budget, and starting bankroll for Phase 2.
2. **Region / hosting:** dedicated server provider, or start on a cloud VM and measure.
3. **Custody:** hardware wallet vs Turnkey/Privy for the treasury.
4. **Seed leaders:** any wallets you already follow (they go in as candidates).
5. **Benchmark:** run a small manual copy of the same leaders on a commercial bot (GMGN / BasedBot / Axiom) during Phase 2 as an execution benchmark.
6. **Jurisdiction and tax** constraints that affect how we account for or structure the account.
