# 06 — Wallet operations, custody and risk controls

These are the operational patterns that managed platforms (Proxima's wallet fleet, CEX funding, bulk funding, live monitoring and action timeline; BasedBot's non-custodial multi-wallet setup) offer, adapted for a single-operator copy engine.

## Wallet hierarchy

```
CEX account ──(withdraw)──► Treasury ──(top-up, policy-limited)──► Hot wallets (N) ──► trades
                               ▲                                        │
                               └────────────(auto-sweep profits)────────┘
```

| Role | Holds | Custody | Rules |
|---|---|---|---|
| **Treasury** | Most of the capital | Hardware wallet, or Turnkey/Privy with a policy engine | Can only send to whitelisted hot wallets and the CEX deposit address. Daily outflow cap. |
| **Hot (trading)** | Working capital only (`max_balance_sol`) | Encrypted local keystore loaded into the engine process | Auto-swept above the cap; topped up below a floor. |
| **Fee payer** (optional) | Small SOL float | Local | Pays fees and tips so hot-wallet balances stay clean for accounting. |
| **Burner** | Throwaway | Local | For testing new venues and builders with dust amounts. |

**Why the hot path uses local keys.**
- Signing in-process takes microseconds.
- A remote signer adds a network round trip; Turnkey markets sub-100 ms, which is still 10–100× slower than local. That is a significant share of a one-slot latency budget.
- The trade-off is accepted by **keeping hot balances small** and sweeping often. A compromised host then loses at most `Σ max_balance_sol`.

**Keystore:**
- Keys are encrypted at rest (age or sops), with the passphrase or KMS unlock entered at start.
- Never put keys in the repo or in config: `.gitignore` blocks `*.keypair.json`, `keys/` and `.env`.
- Never log key material.
- Keep separate hosts, or at least separate users, for the trading engine and research.

## Fleet management (Proxima-style, built in)
- **Funding:**
  - CEX withdrawal to the treasury; treasury top-ups to hot wallets with automatic thresholds.
  - Every transfer is recorded in `wallet_transfers` with a reason: `fund`, `sweep`, `rotate`, `cex_withdraw`, `cex_deposit`.
- **Monitoring:**
  - Balance per wallet, open positions per wallet, pending orders and token-account count. Unclosed ATAs mean rent is locked.
  - Alert on any balance change not explained by our own journal. Treat that as a possible compromise and trip the kill switch.
- **Rotation:**
  - Our own hot wallets become visible over time. Others may copy us, which adds competition for our fills, or target us for sandwiches.
  - Rotate hot wallets on a schedule or when we detect followers: wallets that consistently buy the same tokens right after us.
  - Retire a wallet by setting it to `draining`: no new entries, positions exit normally, then sweep, close ATAs and retire.
- **Timeline:** one view (Grafana / Telegram `/timeline`) merging signals, orders, transfers, config changes and risk events in time order. This is the "what happened at 03:12?" tool.
- **ATA hygiene:** close empty token accounts after full exits to reclaim ~0.002 SOL each. At 100 tokens a day that is ~0.2 SOL a day.

## Pre-trade token safety checks (fast, cached per mint)
- **Mint authority and freeze authority.** A non-revoked freeze authority on a non-launchpad token is a skip, or a very small size.
- **Token-2022 extensions:** transfer hooks, transfer fees, permanent delegate and non-transferable are skips unless explicitly allowed.
- **Sell-path check.** For venues and tokens without a known-safe template, simulate a sell of a tiny amount once per mint. This is cached and runs off the hot path for the first leader buy, then on the hot path only if the token isn't cached.
- **Launchpad-native tokens** (Pump curve, LaunchLab, DBC) are standardised. Their template is pre-verified, so checks are near-free.

## Risk limits (in `config/engine.example.toml` → `[risk]`)

| Limit | Purpose |
|---|---|
| `max_position_sol` | Per-token exposure (cost basis) |
| `max_total_exposure_sol` | Sum of open cost basis |
| `max_open_positions` | Concentration and attention |
| `min_sol_reserve` | Always able to pay fees and emergency sells |
| `daily_loss_limit_sol` | Trips the kill switch for the UTC day |
| `max_buy_slippage_bps` / `max_sell_slippage_bps` | Ceiling on dynamic slippage |

**Kill switch triggers** (logged to `risk_events`):
- daily loss limit hit;
- feed stale (no slots from any provider for more than N seconds) or providers disagree;
- landing rate below X% over the last K orders, or every sender degraded;
- unexplained balance change;
- several orders with realised slippage far above expected (sign of being sandwiched or of a broken builder);
- manual `/kill`.

**Kill switch behaviour:**
- Stop new entries.
- Keep managing exits, because stop losses still need to fire.
- Optionally `/flatten` to market-sell everything.

**Per-leader breaker:** pause a leader after N consecutive losing copies, or when its rolling copy return falls below a threshold ([04](04-wallet-selection.md#7-monitor-and-retire-automatic)).

## MEV and adversarial considerations
- Buys with wide slippage over plain RPC are sandwich bait. Use MEV-protected paths (Jito bundles, private relays such as Astralane) and slippage derived from expected impact.
- Leaders who know they are copied can bait copiers. That is why the leader-behaviour checks in [04](04-wallet-selection.md) exist and why exits are independent.
- **Do not** run volume bots, wash trades or coordinated multi-wallet buys to move prices. That is market manipulation. We detect it; we don't do it.

## Accounting, tax, legal
- **Cost basis.** The journal records cost basis, proceeds and fees per position, in SOL, with `sol_usd` captured in snapshots. That is enough for per-trade gain/loss reports.
- **Rules vary by jurisdiction.** Check with a tax professional where you live before scaling.
- **Your own capital only.** Running this for other people's money (copy-trading as a service) brings licensing questions in most jurisdictions.
