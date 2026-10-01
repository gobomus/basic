#!/usr/bin/env bash
# One-shot install on a fresh Ubuntu 22.04/24.04 server (run as root).
#   curl/scp this repo to the server, then:  sudo bash deploy/setup.sh
set -euo pipefail
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"

apt-get update -y
apt-get install -y build-essential pkg-config curl git chrony
systemctl enable --now chrony   # accurate clock matters for latency stats

if ! command -v cargo >/dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi
source "$HOME/.cargo/env"

cd "$REPO_DIR/engine"
cargo test --workspace --release
cargo build --release -p bot

id copybot >/dev/null 2>&1 || useradd --system --create-home --home-dir /opt/copybot copybot
install -d -o copybot -g copybot /opt/copybot/config /opt/copybot/keys /opt/copybot/data
install -m 0755 target/release/copybot /opt/copybot/copybot
[ -f /opt/copybot/config/copybot.toml ] || install -o copybot -m 0640 "$REPO_DIR/config/copybot.example.toml" /opt/copybot/config/copybot.toml
[ -f /etc/copybot.env ] || install -m 0600 "$REPO_DIR/.env.example" /etc/copybot.env
install -m 0644 "$REPO_DIR/deploy/copybot.service" /etc/systemd/system/copybot.service
systemctl daemon-reload

cat <<MSG

Installed. Next (see RUNBOOK.md):
  1. edit /etc/copybot.env                 (RPC_URL, GEYSER_X_TOKEN, KEYSTORE_PASSPHRASE, TELEGRAM_BOT_TOKEN)
  2. edit /opt/copybot/config/copybot.toml (endpoints, leaders, chat_id)
  3. sudo -u copybot bash -c 'set -a; . /etc/copybot.env; cd /opt/copybot && ./copybot wallet new --out keys/hot-1.json'
  4. sudo -u copybot bash -c 'set -a; . /etc/copybot.env; cd /opt/copybot && ./copybot check'
  5. systemctl enable --now copybot   &&   journalctl -u copybot -f
MSG
