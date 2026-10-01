# Runbook: getting copybot live, step by step

This is the order to follow. **Do not skip the shadow and simulation steps.** They are how we prove the bot works on the real chain before any money moves.

## What you need to buy or set up (one time)

| What | Why | Example providers |
|---|---|---|
| **A server** near the Solana validators: 4+ CPU cores, 8 GB RAM, Ubuntu | Speed. The bot has to sit physically close to the network. | Any dedicated or VPS host in **Frankfurt** or **Amsterdam** (or New York) |
| **A Solana data plan with Yellowstone gRPC, plus an RPC URL** | The live feed of every trade, and basic chain access | Helius (Business or Professional plan includes LaserStream gRPC + RPC + Sender), Triton, Shyft, QuickNode |
| **Transaction senders** | Getting our trades into blocks fast | Jito (free, tip-based), Helius Sender (comes with Helius). Optional: Nozomi, 0slot, Astralane |
| **A Telegram bot** | Alerts and remote control from your phone | Message **@BotFather** → `/newbot` gives you the token. Message **@userinfobot** to get your chat id. |
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
- `/etc/copybot.env`: your RPC URL, gRPC token, a long keystore passphrase, and the Telegram token.
- `/opt/copybot/config/copybot.toml`:
  - `[infra.geyser] endpoint`: your gRPC URL.
  - `[[infra.senders]]`: your sender URLs (use the region closest to your server).
  - `[infra.telegram] chat_id`: your chat id.
  - `[[leaders]]`: the wallets to copy (step 4).
  - Leave `mode = "shadow"` for now.

## 3. Create the trading wallet
```bash
sudo -u copybot bash -c 'set -a; . /etc/copybot.env; cd /opt/copybot && ./copybot wallet new --out keys/hot-1.json'
```
It prints the wallet address. The private key is stored **encrypted** and is never shown. Back up `keys/hot-1.json` **and** your passphrase. Without both, the funds are gone.

## 4. Pick leaders
For each wallet you're considering:
```bash
./copybot leader-report <WALLET_ADDRESS>
```
It reads the wallet's history and reports:
- profit;
- win rate;
- median hold time;
- active hours per day.

It also flags wallets that are bots, or that flip too fast to copy (you'd be their exit liquidity). Add good candidates to `[[leaders]]` in the config.

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
`SIMULATION OK` means the live Pump program accepted the exact buy the bot would send. Do this for one coin still on the bonding curve and one that has graduated to PumpSwap.

## 7. Shadow mode (1–2 weeks)
```bash
sudo systemctl enable --now copybot
journalctl -u copybot -f          # live log
```
The bot follows the leaders in real time and decides every trade, but **sends nothing**. On Telegram:
- `/status`: signals, copies, skips, latency, simulated PnL.
- `/positions`: open simulated positions.

Everything is logged to `/opt/copybot/data/journal/*.jsonl`. Review it before moving on. That includes skipped trades and their reasons, and how every alternative exit strategy would have done.

## 8. Go live, small
1. Send a **small** amount of SOL (e.g. 1–2 SOL) to the wallet address from step 3.
2. In the config, set `mode = "live"`. Keep `max_buy_sol` small, e.g. 0.1.
3. Run `sudo systemctl restart copybot`.
4. Watch the first trades on Telegram. Check them on Solscan.

Compare live results with the shadow results. Raise sizes only when they match.

## Day-to-day
| Need | Do |
|---|---|
| Health and PnL | Telegram `/status` |
| Stop new buys (exits keep running) | `/pause`, later `/resume` |
| Emergency: sell everything | `/flatten` |
| Hard stop for the day | `/kill` |
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
