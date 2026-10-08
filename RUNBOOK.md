# Runbook

Two paths. **Start with Path A: it is free and risks nothing.** Move to Path B only when Path A's report shows an edge.

| | Path A: free proof of concept | Path B: live trading |
|---|---|---|
| Money | none (paper mode: simulated fills) | real SOL, small to start |
| Data feed | standard RPC WebSocket (free) | paid Yellowstone gRPC (faster) |
| Cost | $0 | gRPC plan + server + tips |
| Speed | sees a leader trade ~1-3 s after it lands | sub-second |
| Purpose | find out **whether copying these wallets makes money after costs** | execute it fast |

---

# Path A: free proof of concept

Paper mode follows your chosen wallets on live Solana and simulates every copy **as it would really have landed**: priced from the pool 1.2 s after detection (configurable), with the same slippage limit a real transaction has, so late or run-away fills are missed or worse, exactly as on-chain. All fees are charged. Then `copybot report` tells you what it earned.

## A1. Pick leader wallets
**Quick start:** `config/starter-leaders.example.toml` holds seven candidates already vetted this way (with the numbers next to each), ready to paste under `[[leaders]]`. Treat them as a starting point, not a recommendation.

To pick your own: GMGN smart-money / KOL lists, Axiom, KOLscan. A leaderboard is only a list of candidates: most "profitable wallets" on them are sniper bots whose edge is their position in the block, and vendor PnL figures are not checked. **Audit the whole list at once, free:**
```bash
# wallets.txt: one per line, `name: address` (the Notion / GMGN format works as is)
RPC_URL=https://solana-rpc.publicnode.com \
copybot wallet-audit --file wallets.txt --leaders-out config/leaders.local.toml
```
For every wallet it reads the transactions the wallet signed itself (transfers and fee payouts sent to it by others are counted but skipped), decodes its round trips, and sorts it into one of:

| Class | Means | Verdict |
|---|---|---|
| bot | fires many transactions per second, most of them fail, or 1,000 transactions in minutes | reject |
| sniper | profitable or not, its median hold is under 20 s: nobody can copy that | reject |
| holder | selling a position it already had (no buys in the window) | reject |
| trader, losing | 30+ round trips and down over the window | reject |
| trader | decision-speed trading | **LEADER** with 30+ round trips, positive realised PnL, profit factor ≥ 1.5, median hold ≥ 30 s and ≥ 90% of its transactions read; otherwise watch |
| unclear | under 10 decoded round trips (or mostly other people's transactions) | watch |

The table goes to the screen and to `data/wallet-audit/<date>.txt` (full numbers in `<date>.json`); the leaders go to `config/leaders.local.toml`. Load them in the engine config with one line near the top (before any `[section]`):
```toml
leaders_file = "config/leaders.local.toml"
```
Leaders written directly in the config stay too (a config entry wins over the same address in the file). Re-run the audit weekly: wallets that stop qualifying drop out by themselves. The `wallet-audit` workflow does this every Monday once it is on the default branch; put your list in the repository secret `AUDIT_WALLETS` so it stays private.

To look at one wallet in more detail:
```bash
copybot leader-report <WALLET> [<WALLET> ...]    # profit, win rate, hold time, bot check; several wallets get a comparison table
```
It reads the wallet's last `--limit` transactions (default 1000; `--limit 300` is a good first look) at about 4 per second, with a pause between wallets so a free RPC does not cut you off. History reads work best on PublicNode's free endpoint (`RPC_URL=https://solana-rpc.publicnode.com copybot leader-report ...`); for following wallets live, use the public Solana endpoint or a free keyed one (PublicNode's live stream arrives too late).
Rules of thumb for the free feed:
- **1 to 60 swaps per hour is ideal.** A wallet doing hundreds of swaps an hour is a bot: it cannot be copied (you would only be its exit liquidity) and it floods a free RPC.
- Skip wallets whose median hold is under 20 s (the report rejects them: nothing can copy that). Holds of 20-120 s are "fast flippers": the report notes them as candidates, and paper mode measures what survives your feed's delay.
- Prefer wallets that trade Pump.fun / PumpSwap coins; those are the venues the free feed can price.

## A2. Run it: three ways

**Way 1: GitHub Actions (nothing to install).** The workflow (`.github/workflows/paper-run.yml`) shows up in the Actions tab once it is on the repository's default branch (`master`). Then, in the repository on GitHub: **Actions → paper-run → Run workflow**, paste the wallet addresses, choose hours (max 5.5), leave "Branch with the code" as it is, Run. When it finishes, open the run: the **summary page shows two reports**, one for this run and one for **all earlier runs added together** (each run attaches its journal; the next one collects them), so the evidence builds up across days without downloading anything. Free and unlimited for public repositories; a private one has 2,000 free minutes a month (about six 5.5-hour runs). Optional: add a repository secret `RPC_URL` with a free key (below), otherwise the public Solana endpoint is used; it can be slow or rate-limited from GitHub's shared machines, so a free keyed endpoint is the better choice.

*Privacy:* in a public repository the wallet list you type in and the reports are visible to anyone. That is fine for the starter wallets (public KOL wallets). If your own wallet list is your edge, make the repository private first (Settings → General → bottom of the page → Change visibility).

**Way 2: your own computer** (Linux or macOS; on Windows use WSL / Ubuntu). One-time install of Rust (<https://rustup.rs>), then:
```bash
git clone https://github.com/gobomus/basic.git && cd basic
git checkout claude/solana-copy-trading-bot-xlkuzm
cd engine && cargo build --release -p bot && cd ..
cp config/poc.example.toml config/copybot.toml      # then put your wallets under [[leaders]]
export RPC_URL=https://api.mainnet-beta.solana.com    # public and free; see "RPC options"
./engine/target/release/copybot check                 # every line OK
./engine/target/release/copybot run                   # leave it running; Ctrl-C stops it cleanly
```
Or generate the config from a list of wallets: `python3 scripts/make_poc_config.py config/copybot.toml "WALLET1, WALLET2"`.

**Way 3: a free always-on server** (for multi-day runs), e.g. Oracle Cloud "Always Free" or any small VPS: same commands as Way 2, run under `tmux` or the systemd unit in `deploy/`.

## A3. Watch it
```bash
copybot ctl status          # signals, copies, skips, open positions, simulated PnL
copybot ctl positions       # what is open right now
copybot report              # the proof: see below
copybot ctl stop            # graceful: open paper positions are marked out, alternative exits scored
```
Everything is also written to `data/journal/*.jsonl` (one JSON per line).

## A4. Read the report
`copybot report` prints, after every modelled cost:
- the **signal funnel**: what leaders did, what was copied, why trades were skipped, how many entries missed because the price ran past slippage;
- **results**: trades, win rate, total PnL, expectancy per trade, profit factor, and a **95% confidence interval** for the mean return per trade;
- **by leader** and **by exit reason**: who earns it, who loses it, which rule closes the winners and the losers;
- **alternative exits**: the same entries and price paths replayed under every exit policy, so one run compares exit styles;
- a **go / no-go checklist** and a verdict. It refuses to conclude anything below 30 closed trades.

**Go / no-go for Path B:** all four boxes ticked: 300+ leader buys seen, 100+ closed trades, mean return per trade positive with 95% confidence, and at least 3 leaders individually in profit. Several days of running, not one afternoon. If the verdict is "losing" or "inconclusive": change leaders or exits and run again. That is what Path A is for.

## RPC options (all free)
| | Limits | Notes |
|---|---|---|
| Public `https://api.mainnet-beta.solana.com` | rate-limited per IP, no SLA | works with a handful of leaders; nothing to sign up for |
| Helius free | 1M credits/month, 10 requests/s | set `poll_ms = 3000` in `[infra.ws]` to stay under the monthly budget |
| Alchemy free | 30M compute units/month, 25 requests/s | |
| dRPC free | large monthly quota, 100 requests/s | |

Cost model of the feed: every leader swap costs about 2 calls; while positions are open, one batched call per `poll_ms` prices all of them. WebSocket traffic is not metered per call. The free feed prices **Pump.fun curve and PumpSwap** coins; other venues need the paid feed.

---

# Path B: live trading with paid infrastructure

Do not start here. Everything below needs a Path A result that justifies it.

## What you need to buy or set up (one time)

| What | Why | Example providers |
|---|---|---|
| **A server** near the Solana validators: 4+ CPU cores, 8 GB RAM, Ubuntu | Speed. The bot has to sit physically close to the network. | Any dedicated or VPS host in **Frankfurt** or **Amsterdam** (or New York) |
| **A Solana data plan with Yellowstone gRPC, plus an RPC URL** | The live feed of every trade, and basic chain access | Helius (Business or Professional plan includes LaserStream gRPC + RPC + Sender), Triton, Shyft, QuickNode |
| **Transaction senders** | Getting our trades into blocks fast | Jito (free, tip-based), Helius Sender (comes with Helius). Optional: Nozomi, 0slot, Astralane |
| **SOL** | Trading capital + fees | Your exchange account |

Postgres and ClickHouse (the analytics databases) are optional on day one. The bot always writes a complete daily log file regardless.

## 1. Install on the server
```bash
git clone https://github.com/gobomus/basic.git && cd basic
git checkout claude/solana-copy-trading-bot-xlkuzm
sudo bash deploy/setup.sh
```
This builds the bot, runs the full test suite, and installs the background service.

## 2. Fill in your keys and settings
- `/etc/copybot.env`: your RPC URL, gRPC token, and a long keystore passphrase.
- `/opt/copybot/config/copybot.toml`:
  - `[infra.geyser] endpoint`: your gRPC URL.
  - `[[infra.senders]]`: your sender URLs (use the region closest to your server).
  - `[[leaders]]`: the wallets to copy (step 4).
  - Leave `mode = "shadow"` for now.

## 3. Create the trading wallet
```bash
sudo -u copybot bash -c 'set -a; . /etc/copybot.env; cd /opt/copybot && ./copybot wallet new --out keys/hot-1.json'
```
It prints the wallet address. The private key is stored **encrypted** and is never shown. Back up `keys/hot-1.json` **and** your passphrase. Without both, the funds are gone.

## 3b. (Recommended) Durable nonces for multi-sender sending
Jito, Helius Sender, Nozomi and others each require tips to **their own** accounts. To use several at once, the bot signs one copy of each order per service on a shared *durable nonce*, so only one copy can ever execute:
```bash
./copybot wallet nonce-create --count 4      # ~0.0015 SOL rent each, refundable with `wallet nonce-close`
```
Paste the printed `nonce_accounts = [...]` line into `[infra]`. Without nonces, the bot uses only the first sender group (e.g. Jito) plus any plain RPC endpoints.

## 4. Pick leaders
Find candidates from GMGN's live smart-money and KOL feeds (needs `GMGN_API_KEY` in `/etc/copybot.env`):
```bash
./copybot discover
```
It scores each active wallet with GMGN's own track-record and copy-tradeability method and prints ready-to-paste `[[leaders]]` entries.

For each wallet you're considering:
```bash
./copybot leader-report <WALLET_ADDRESS>
```
It reads the wallet's history and reports:
- profit;
- win rate;
- median hold time;
- active hours per day.

It also flags wallets that are bots, or that flip too fast to copy (you'd be their exit liquidity). With a GMGN key it adds GMGN's 7-day and 30-day scores. Add good candidates to `[[leaders]]` in the config.

To check a specific token (holders, dev, snipers, bundlers, insiders, smart money, socials):
```bash
./copybot token-intel --mint <TOKEN_MINT>
```

## 5. Preflight check
```bash
./copybot check
```
Every line must say `OK` and it must end with **READY**. It measures:
- your RPC speed;
- each sender;
- the gRPC feed (how many trades it decodes in 10 seconds).

## 6. Prove the trade instructions on the real chain (no money needed)
Pick any live Pump.fun token and any wallet address that holds some SOL (it doesn't have to be yours):
```bash
./copybot simulate --mint <TOKEN_MINT> --sol 0.1 --as <ANY_FUNDED_ADDRESS>
```
`SIMULATION OK` means the live Pump program accepted the exact buy the bot would send. Do this for one coin still on the bonding curve and one that has graduated to PumpSwap. Add `--sell` with an address that holds the token to prove the sell as well.

To check the trade decoder against what is happening on-chain right now:
```bash
./copybot audit --program pump --limit 20       # also: pumpswap, dbc, launchlab
```
It decodes recent real trades and compares the amounts with each trader's actual balance changes. It exits with an error if any disagree.

## 7. Shadow mode (1–2 weeks)
```bash
sudo systemctl enable --now copybot
journalctl -u copybot -f          # live log
```
The bot follows the leaders in real time and decides every trade, but **sends nothing**. On the server:
- `copybot ctl status`: signals, copies, skips, latency, simulated PnL.
- `copybot ctl positions`: open simulated positions.

Everything is logged to `/opt/copybot/data/journal/*.jsonl`. Review it before moving on. That includes skipped trades and their reasons, and how every alternative exit strategy would have done.

## 8. Go live, small
1. Send a **small** amount of SOL (e.g. 1–2 SOL) to the wallet address from step 3.
2. In the config, set `mode = "live"`. Keep `max_buy_sol` small, e.g. 0.1.
3. Run `sudo systemctl restart copybot`.
4. Watch the first trades with `journalctl -u copybot -f` and `copybot ctl positions`. Check them on Solscan.

Compare live results with the shadow results. Raise sizes only when they match.

## Day-to-day
| Need | Do |
|---|---|
| Health and PnL | `copybot ctl status` |
| Stop new buys (exits keep running) | `copybot ctl pause`, later `copybot ctl resume` |
| Emergency: sell everything | `copybot ctl flatten` |
| Hard stop for the day | `copybot ctl kill` |
| Never buy a token or a dev's tokens again | `copybot ctl blacklist <mint or dev wallet>` (`unblacklist` to undo; survives restarts) |
| Take profits out | `./copybot wallet sweep --to <YOUR_SAFE_WALLET> --keep 1.0` |
| Reclaim rent from empty token accounts (~0.002 SOL each) | `./copybot wallet close-empty` |
| Wallet contents | `./copybot wallet balance` |
| Stop the service | `sudo systemctl stop copybot`. Tokens still held are re-adopted and managed on the next start. |

## Built-in safety rails
- A **daily loss limit** trips the kill switch automatically. It resets the next UTC day.
- If the data feed goes quiet for 5 seconds, **new entries pause** until it recovers.
- Hard caps apply per trade, per token, on total exposure and on open positions. A SOL reserve is always kept.
- **Sells are retried** up to 5 times with a rising tip, and you're alerted if one is stuck.
- Each order is **one signed transaction** sent through all senders at once, so it can never execute twice.
