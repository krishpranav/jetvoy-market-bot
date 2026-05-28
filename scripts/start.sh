#!/usr/bin/env bash
# Quick start/stop/status/logs helper
set -euo pipefail

SERVICE="jetvoy-market-bot"
CMD="${1:-status}"

case "$CMD" in
  start)
    systemctl start "$SERVICE"
    echo "Bot started. Watching logs (Ctrl+C to exit logs, bot keeps running)..."
    sleep 1
    journalctl -u "$SERVICE" -f --no-pager
    ;;
  stop)
    systemctl stop "$SERVICE"
    echo "Bot stopped."
    ;;
  restart)
    systemctl restart "$SERVICE"
    echo "Bot restarted."
    sleep 1
    journalctl -u "$SERVICE" -f --no-pager
    ;;
  status)
    systemctl status "$SERVICE" --no-pager
    ;;
  logs)
    journalctl -u "$SERVICE" -f --no-pager
    ;;
  trades)
    REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    if [ -f "$REPO_DIR/trades.csv" ]; then
      echo "Last 20 trades:"
      tail -20 "$REPO_DIR/trades.csv"
    else
      echo "No trades logged yet."
    fi
    ;;
  wallets)
    REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    if [ -f "$REPO_DIR/.env" ]; then
      echo "Wallet addresses:"
      export PATH="$HOME/.foundry/bin:$PATH"
      grep "^WALLET_KEY_" "$REPO_DIR/.env" | while IFS='=' read -r key val; do
        addr=$(cast wallet address "$val" 2>/dev/null || echo "unknown")
        echo "  $key → $addr"
      done
    fi
    ;;
  *)
    echo "Usage: $0 {start|stop|restart|status|logs|trades|wallets}"
    exit 1
    ;;
esac
