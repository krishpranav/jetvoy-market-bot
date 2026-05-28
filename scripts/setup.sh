#!/usr/bin/env bash
# =============================================================================
# JetVoy Market Bot — Full Server Setup
# Run once on a fresh Digital Ocean Ubuntu 22.04 droplet:
#   curl -fsSL https://raw.githubusercontent.com/YOUR_REPO/main/scripts/setup.sh | bash
# =============================================================================
set -euo pipefail

REPO_DIR="/opt/jetvoy-market-bot"
SERVICE_NAME="jetvoy-market-bot"
BOT_USER="botrunner"

echo ""
echo "╔══════════════════════════════════════════╗"
echo "║   JetVoy Market Bot — Server Setup       ║"
echo "╚══════════════════════════════════════════╝"
echo ""

# ── 1. System dependencies ───────────────────────────────────────────────────
echo "[1/7] Installing system dependencies..."
apt-get update -qq
apt-get install -y -qq \
  build-essential \
  curl \
  git \
  pkg-config \
  libssl-dev \
  screen \
  jq \
  ufw

# ── 2. Install Rust ──────────────────────────────────────────────────────────
echo "[2/7] Installing Rust..."
if ! command -v cargo &>/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  source "$HOME/.cargo/env"
else
  echo "  Rust already installed: $(rustc --version)"
fi
source "$HOME/.cargo/env"

# ── 3. Install Foundry (for cast wallet) ─────────────────────────────────────
echo "[3/7] Installing Foundry..."
if ! command -v cast &>/dev/null; then
  curl -L https://foundry.paradigm.xyz | bash
  source "$HOME/.bashrc" 2>/dev/null || true
  "$HOME/.foundry/bin/foundryup"
  export PATH="$HOME/.foundry/bin:$PATH"
else
  echo "  Foundry already installed: $(cast --version)"
fi
export PATH="$HOME/.foundry/bin:$PATH"

# ── 4. Copy project files ────────────────────────────────────────────────────
echo "[4/7] Setting up project directory..."
mkdir -p "$REPO_DIR"
cp -r . "$REPO_DIR/"
cd "$REPO_DIR"

# ── 5. Build release binary ──────────────────────────────────────────────────
echo "[5/7] Building release binary (this takes ~5 mins first time)..."
source "$HOME/.cargo/env"
cargo build --release
echo "  Binary built: $REPO_DIR/target/release/market-bot"

# ── 6. Generate wallets ──────────────────────────────────────────────────────
echo "[6/7] Generating wallets..."
bash "$REPO_DIR/scripts/generate-wallets.sh"

# ── 7. Install systemd service ───────────────────────────────────────────────
echo "[7/7] Installing systemd service..."
cp "$REPO_DIR/scripts/jetvoy-market-bot.service" /etc/systemd/system/
sed -i "s|REPO_DIR|$REPO_DIR|g" /etc/systemd/system/jetvoy-market-bot.service
systemctl daemon-reload
systemctl enable "$SERVICE_NAME"

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  Setup complete!                                             ║"
echo "╠══════════════════════════════════════════════════════════════╣"
echo "║  NEXT STEPS:                                                 ║"
echo "║                                                              ║"
echo "║  1. Send funds (ETH + USDC) to the wallet addresses above   ║"
echo "║  2. Edit config if needed: $REPO_DIR/config/default.toml    ║"
echo "║  3. Start the bot:                                           ║"
echo "║     systemctl start jetvoy-market-bot                       ║"
echo "║  4. Watch logs:                                              ║"
echo "║     journalctl -u jetvoy-market-bot -f                      ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""
