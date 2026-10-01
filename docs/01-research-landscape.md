# 01 — Research: the Solana memecoin trading landscape (as of 1 Oct 2026)

What the leading tools do, what the infrastructure looks like in 2026, what the evidence says about copy trading, and what this means for our design. Sources are listed at the bottom. Vendor claims are marked as claims.

> Method note: the research sandbox's egress proxy blocked direct page fetches to most vendor sites (gmgn.ai, proxima.tools, docs.basedbot.app, chainstack, …). The findings below come from web search, which returned excerpts of those pages plus third-party reviews. Before relying on any vendor detail, check it on the vendor's own docs.

---

## 1. Terminals and bots: what they track and automate

### GMGN.ai
- **Position:** Memecoin terminal on Solana, BSC, Base and Ethereum. It covers new-token monitoring, security audits, holder analysis, smart-money copy trading, wallet P&L analytics and automated execution.
- **Smart-money tracking:** It runs AI-assisted wallet filtering and tags wallets (smart money, KOL, sniper, insider, fresh, …). It shows which tracked wallets are accumulating, how holder clusters connect, and whether buying is fresh money or the same insiders recycling. A premium private-node option claims to track up to 500 wallets.
- **Copy-trade rules:** These filter on token age, liquidity, holder characteristics, the leader's trade size, platform/launchpad, slippage, TP and SL.
- **API:** A **GMGN OpenAPI** (REST) plus an **Agent API / skills** repo (`GMGNAI/gmgn-skills`), with token, market, portfolio, wallet-style, wallet-review, track and swap commands. Swap supports market, limit and TP/SL strategy orders.
  → Usable as a *secondary* data source and as a benchmark, not as our hot path.

### Axiom (axiom.trade)
- **Position:** Described in 2026 comparisons as the volume leader among Solana terminals. One source claims ~73% share; treat that as unverified.
- **Pulse:** A three-column lifecycle view (New Pairs → Final Stretch → Migrated) with about 14 filters: token age, top-10 holder %, dev holding %, sniper count/%, insider %, bundle %, holder count, pro-trader count, liquidity, volume, market cap and transaction count.
- **Other features:** Wallet tracker, a native X/Twitter monitor, migration auto-buy, auto TP/SL, MEV settings, limit orders, and Hyperliquid perps.
- **Takeaway:** Pulse's filter set is the minimum feature set our token snapshots must cover ([03-data-model.md](03-data-model.md)).

### Padre → "Terminal" (by Pump.fun)
- Pump.fun acquired Padre in Oct 2025 and rebranded it **Terminal**. It is multi-chain, with copy trading, limit orders, auto-exit strategies and multi-wallet support.
- One 2026 comparison measured **127 ms** execution for Padre vs **310 ms** for Axiom. This is a single third-party test, so treat it as indicative only.
- The launchpad operator now owns a terminal, so expect venue-specific advantages (order flow, early data) for Pump-native tools.

### BasedBot (basedbot.app)
- **Position:** A non-custodial, multi-chain (19 networks) Telegram bot with a web terminal and Chrome extension. Live since June 2024. It markets 30% fee cashback.
- **Automations:** Market/limit orders, sniping, DCA, **copy trading**, **social copy trading** (copy from X accounts), migration sniping, TP, SL, trailing SL and **dev-sell protection**.
- **Copy-trade settings (from its docs):**
  - Buy/sell enabled per network.
  - Copy amount.
  - Buy and sell slippage.
  - **Min/max pool liquidity (USD)**.
  - **Min/max leader trade size**, which applies to the *leader's* trade, not ours.
  - **Max buy** as a hard USD cap on our size.
- **Takeaway:** This is the standard filter vocabulary, and our `[filters]` and `[sizing]` config covers it ([config/engine.example.toml](../config/engine.example.toml)). What BasedBot lacks is a decoupled, data-tuned exit.

### Proxima (proxima.tools)
- **Position:** A closed-beta, multi-chain "token operations" platform aimed at **token-launch teams**. It launches on about 29 launchpads (Pump.fun, Bonk.fun, Uniswap, …) and handles supply distribution, wallet orchestration, execution sequencing and post-launch automation.
- **Wallet management** is the part relevant to us:
  - Wallet sourcing and a wallet marketplace.
  - **CEX funding**.
  - **Bulk funding**.
  - Live balance monitoring.
  - A searchable **timeline** of past and scheduled actions across wallets and strategies.
- **Automations:** AutoBuy, AutoSell, **Volume Bot**.
- **Takeaways:**
  - Copy the **operational patterns**: a wallet fleet, funding rails, balance monitoring, a unified action timeline. These are spelled out in [06-wallets-and-risk.md](06-wallets-and-risk.md).
  - Do **not** copy the launch-side tooling (volume bots, coordinated multi-wallet buys). That is wash trading or manipulation. For us it is a signal to **detect** in the tokens we trade, because it is exactly what makes "smart money" and volume metrics lie.

### Summary: what every serious tool tracks
| Category | Fields |
|---|---|
| Token safety | Mint/freeze authority, LP burned/locked, Token-2022 extensions, honeypot / sell simulation |
| Distribution | Holders, top-10 %, dev holding %, snipers %, bundlers %, insiders %, fresh-wallet % |
| Flow | Buys/sells and volume per window (5 s … 24 h), transactions per second, unique traders, net flow |
| Lifecycle | Launchpad, bonding-curve progress, migration status and venue, token age |
| Wallet intelligence | Smart money / KOL / pro-trader counts among holders; dev history (prior launches, rugs) |
| Social | X / Telegram / website presence, X handle reuse, KOL mentions, DexScreener paid / boosts |
| Execution | Speed, MEV protection, multi-sender, TP/SL/trailing, dev-sell auto-exit, migration snipe |

---

## 2. Market structure in 2026 (what we have to decode)

### Launchpad share swings fast
- Pump.fun held ~99% in mid-April 2026.
- By July 2026, LetsBONK reportedly flipped it: about **47% LetsBONK**, **41% Pump.fun**, ~6% Bags, ~2% Jupiter Studio, ~1.5% Believe.
- Other reports in the same year show Pump.fun reclaiming ~90%.
- → **Decoders must be multi-venue from day one:**
  - Pump.fun curve and PumpSwap
  - Raydium LaunchLab, CPMM, AMM v4 and CLMM
  - Meteora DBC (program `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN`), DAMM v2 and DLMM. Bags and Jupiter Studio launch on Meteora DBC.
  - Orca
- Also track **migrations** across all of them.

### Pump.fun economics changed several times
- **PumpSwap** is the migration destination (since Mar 2025), with no ~6 SOL Raydium migration fee.
- The bonding-curve fee is quoted at **1.25%** (0.95% protocol + 0.30% creator).
- A dynamic fee model was introduced later.
- **Jan 2026:** creator fee sharing across up to 10 wallets. Coins now choose **Creator Fees vs Trader Cashback** at launch.
- → Fee mode is a token feature (`creator_fee_mode`), and venue fees must be **data, not constants**, in the cost model.

### Jupiter
- As of Mar 2026, Ultra and Metis are unified into **Swap API v2** (`api.jup.ag/swap/v2`).
- Ultra v3 added bonding-curve routing with ~10 ms quotes.
- → Use it as a fallback route for venues we have no direct builder for yet, and for emergency exits. The direct venue builder is always faster.

### Chain
- **SIMD-0286 (100M CU blocks)** went live on 29 Jul 2026, giving more block space.
- **Alpenglow** (~150 ms finality, vote transactions removed from blocks) is **not live**. Activation rumours for 28 Sep 2026 were denied, and the 9 Nov window also omits it.
- → Design for it now: do not hard-code slot or confirmation timing assumptions.

---

## 3. Low-latency infrastructure (the copy-trading race)

| Layer | Option | Notes |
|---|---|---|
| Earliest signal | **Jito ShredStream** / shred-decoding services (e.g. RPC Fast Aperture TxStream) | Sees transactions *before execution* (no logs or balances). RPC Fast reports **~120 ms average and ~270 ms p99** earlier arrival vs plain gRPC. |
| Confirmed signal | **Yellowstone gRPC** (Triton, many providers), **Helius LaserStream** | The production standard: **10–40 ms vs 300+ ms** for polling. Raw Yellowstone is a few ms faster; LaserStream is more fault-tolerant. LaserStream mainnet needs the Helius **Business ($499/mo)** or **Professional ($999/mo)** plan. |
| Decoding | **Carbon** (SevenLabs, Rust, ~40 decoders, Yellowstone and ShredStream sources), **Yellowstone Vixen** | Saves writing every IDL decoder by hand. |
| Landing | **Jito** bundles/tips, **Helius Sender** (parallel Jito + Helius, 7 regions, tip-only billing; Sender Max minimum tip **0.001 SOL**), **Nozomi**, **0slot**, **Astralane** (private relay, MEV-protected), **bloXroute** | Jito-Solana reportedly runs under >95% of stake by mid-2026, and Jito tips are >60% of priority-fee volume. Best practice in 2026 is **proximity-aware, multi-sender fan-out**. |
| Signing / custody | Local keypair (µs), **Turnkey** (enclave and policy engine, claims sub-100 ms), **Privy** server wallets (policy engine, Solana) | Local signing for hot wallets. Enclave or policy wallets for the treasury. |

**Detection-latency math.** A copier that sees the leader 300 ms late on a fast token is buying a price the leader already moved, and sometimes buying into the leader's exit. The target is to land within **1–2 slots** of the leader.

---

## 4. Evidence on copy trading (read this before investing)

1. **The imitation penalty is real and large.**
   - An arXiv study (2601.08641, 2026) covered 6,000+ meme coins. Smart-money trades averaged about **+14%** per trade. The **estimated copier return under realistic frictions was ~+3%**, and that was *with* their best filtering.
   - The gap comes from trade ordering on bonding curves: the copier always buys after the leader, at a higher price.
2. **Most "top traders" are bots.**
   - One 2026 analysis found **93 of the top 100** Pump.fun/PumpSwap traders active more than 18 h/day.
   - It also found about **800 non-bot accounts** that each traded more than $10M. Those humans or slower bots are the copyable population.
3. **Adversarial leaders exist.**
   - Some wallets know they are copied. They use followers' buys as exit liquidity, front-run their copiers, hide positions across wallets, or fabricate sentiment.
   - Public "smart money" labels are visible to everyone, so some wallets are *engineered* to look smart.
4. **Short-hold wallets can't be copied.** If a wallet's median hold is seconds, the copier is the leader's exit liquidity.
5. **Fixed costs dominate small trades.**
   - Pump curve fees are about 1.25% per side.
   - Each transaction pays a tip plus priority fee (e.g. ≥0.001 SOL for Sender Max).
   - ATA rent (~0.002 SOL) is locked per new token until the account is closed.
   - On a 0.1 SOL copy that is roughly **4–5% round-trip** before slippage. See the table in [07-roadmap.md](07-roadmap.md#unit-economics).

**What this means for the design:**
- Select leaders on **simulated copier return**, not on the leader's P&L.
- Prefer **slower, swing-style leaders** (holds of minutes to hours) and skip snipers.
- Decouple exits from the leader.
- Log every skipped signal and every post-entry price path so the gap can be measured and attacked.
- Treat the copy phase as a **data-collection business that pays for itself**, with the leader-free engine as the real goal. That is exactly the stated plan.

---

## 5. Data sources for enrichment and backfill

| Need | Options (2026) |
|---|---|
| Historical swaps / backfill for wallet discovery | **Bitquery** (decoded trades across 300+ DEXs; Pump.fun create/curve/graduation/PumpSwap; gRPC/Kafka/WebSocket), **Birdeye** (prices, trades, OHLCV, wallet portfolios, security signals, new listings), **Solana Tracker**, **Moralis**, **Codex**, **Helius DAS**, own archival RPC |
| Social: X | X API moved to **pay-per-use in Feb 2026**: about **$0.005 per post read**, capped at 2M reads/month, no free tier. Third-party account streams also exist (e.g. ~$250/mo for 100 accounts over WebSocket). → Enrich only tokens in our universe. |
| Token pages / paid profiles | DexScreener (paid profile, boosts), GeckoTerminal (OHLCV; the archived basket dashboard already uses it) |
| Smart-money cross-check | GMGN OpenAPI, Axiom / GMGN UI for spot checks |

---

## Sources
- GMGN: [review 2026 (airdropalert)](https://airdropalert.com/blogs/gmgn-review/) · [bot settings & copy trading (coincodecap)](https://coincodecap.com/best-settings-for-gmgn-bot) · [smart-money guide (gmgn.ai blog)](https://gmgn.ai/blog/how-to-track-copy-solana-smart-money/) · [GMGN vs Axiom](https://gmgn.ai/blog/gmgn-vs-axiom-for-beginners/) · [GMGNAI/gmgn-skills](https://github.com/GMGNAI/gmgn-skills) · [GMGN API overview](https://medium.com/@gemQueenx/gmgn-api-gmgn-solana-trading-bot-openapi-and-ai-agent-api-51f30074d22e)
- Axiom: [Pulse docs](https://docs.axiom.trade/axiom/finding-tokens/pulse) · [Pulse filters guide](https://axiompedia.com/guides/trading/axiom-pulse-explained) · [Coin Bureau review](https://coinbureau.com/review/axiom-trade-review) · [terminal comparison 2026 (athenaalpha)](https://athenaalpha.xyz/blog/best-solana-trading-terminal-2026-comparison) · [Padre vs Axiom vs GMGN](https://degenspaced.vercel.app/)
- Padre/Terminal: [Padre guide](https://medium.com/@geggonen/padre-gg-complete-guide-f5b4ab97e47b) · [Pump.fun acquires Padre](https://www.valuethemarkets.com/cryptocurrency/news/pumpfun-expands-horizons-with-acquisition-of-padre-trading-terminal)
- BasedBot: [docs](https://docs.basedbot.app/) · [copy trading](https://docs.basedbot.app/quick-setup-guide/trading-automations/copy-trading) · [managing wallets](https://docs.basedbot.app/quick-setup-guide/based-bot-set-up/managing-wallets) · [Solana Compass profile](https://solanacompass.com/projects/basedbot)
- Proxima: [proxima.tools](https://proxima.tools/) · [docs](https://docs.proxima.tools/)
- Infra: [Solana trading infra 2026 (Chainstack)](https://chainstack.com/solana-trading-infrastructure-2026/) · [ShredStream vs Geyser vs RPC (RPC Fast)](https://rpcfast.com/blog/shredstream-vs-geyser-vs-standard-rpc) · [copy-trading bot playbook (RPC Fast)](https://rpcfast.com/blog/how-to-build-a-solana-copy-trading-bot) · [Aperture TxStream](https://rpcfast.com/blog/aperture-txstream-real-time-simulation) · [Yellowstone providers compared](https://nolimitnodes.com/blog/yellowstone-grpc-providers-compared) · [Yellowstone vs LaserStream](https://nolimitnodes.com/compare/yellowstone-grpc-vs-laserstream) · [LaserStream](https://www.helius.dev/laserstream) · [Helius plans](https://helius.dev/docs/billing/plans) · [Helius Sender Max](https://www.helius.dev/docs/sending-transactions/sender-max) · [landing txs in 2026 (dev.to)](https://dev.to/techmystique_/the-fastest-way-to-land-solana-transactions-in-2026-5gg9) · [MEV protection 2026 (dev.to)](https://dev.to/gerus_team/mev-protection-on-solana-in-2026-jito-bundles-astralane-and-what-actually-works-3gbc) · [Jito explained 2026 (RPC Fast)](https://rpcfast.com/blog/jito-explained-bundles-tips-mev-solana) · [Carbon + gRPC (QuickNode)](https://www.quicknode.com/guides/solana-development/tooling/solana-grpc/solana-grpc-carbon) · [Carbon talk (Breakpoint 25)](https://solanacompass.com/learn/breakpoint-25/tech-talk-sevenlabs-carbon-data-pipeline)
- Chain: [100M CU blocks](https://solana.com/zh/upgrades/100m-cu-blocks) · [SIMD-0286 activation](https://solanacompass.com/news/solana-raises-mainnet-block-compute-limit-66-to-100m-cus-with-simd-0286-at) · [Alpenglow not on 28 Sep](https://www.kucoin.com/news/flash/solana-developers-deny-alpenglow-mainnet-activation-on-september-28) · [Alpenglow community testing](https://coinmarketcap.com/academy/article/solana-alpenglow-upgrade-enters-community-validator-testing)
- Launchpads / Pump.fun: [launchpad share (CoinMarketCap)](https://coinmarketcap.com/academy/article/pumpfun-reclaims-90percent-market-share-in-solana-launchpad-war) · [LetsBONK flips Pump (OKX)](https://www.okx.com/en-us/orbit/news/letsbonk-flips-pumpfun-as-solana-s-top-memecoin-launchpad-49066129410084) · [Pump.fun fees](https://pump.fun/docs/fees) · [dynamic fees (Blockworks)](https://blockworks.com/news/pumpdotfun-fee-model) · [creator fees vs cashback](https://crypto.news/pump-fun-flips-creator-fees-launches-trader-cashback/) · [Meteora DBC](https://docs.meteora.ag/developer-guides/dbc) · [Raydium LaunchLab](https://docs.raydium.io/products/launchlab/platforms)
- Jupiter: [Ultra v3](https://developers.jup.ag/blog/ultra-v3) · [Mar 2026 changelog](https://developers.jup.ag/changelog/2026-03)
- Evidence: [Resisting Manipulative Bots in Meme Coin Copy Trading (arXiv 2601.08641)](https://arxiv.org/abs/2601.08641v3) · [90% of top Pump.fun traders are bots (BeInCrypto)](https://beincrypto.com/pump-fun-bot-activity-may-be-epidemic/) · [copy-trading alpha wallets & bots (Medium)](https://medium.com/@nathan.baldwin_31153/copy-trading-on-solana-how-to-find-alpha-wallets-and-avoid-bots-26182d750bb2)
- Data/social: [Bitquery Solana DEX APIs](https://bitquery.io/blog/best-solana-dex-trade-data-api) · [Solana APIs 2026 (CoinStats)](https://coinstats.app/blog/best-solana-api/) · [X API pricing 2026](https://postproxy.dev/blog/x-api-pricing-2026) · [Twitter API pricing (1322)](https://1322.io/blog/twitter-api-pricing)
- Custody: [Turnkey vs Privy](https://www.turnkey.com/vs/privy) · [Privy policy engine](https://privy.io/blog/turning-wallets-programmable-with-privy-policy-engine) · [Turnkey agentic wallets](https://docs.turnkey.com/products/embedded-wallets/features/agentic-wallets)
