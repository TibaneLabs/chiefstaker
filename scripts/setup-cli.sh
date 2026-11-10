#!/bin/bash
# Configure Solana CLI to use local test validator
#
# Run this after starting the test validator to configure your CLI.
#
# NOTE: this changes the RPC URL in your GLOBAL Solana CLI config
# (~/.config/solana/cli/config.yml). The previous URL is printed so you can
# restore it afterwards with `solana config set --url <previous-url>`.

set -euo pipefail

AGAVE_LOCAL="$HOME/.local/share/solana/install/active_release/bin"
if [ -x "/pkg/main/net-p2p.agave.core/bin/solana" ]; then
    SOLANA_BIN="/pkg/main/net-p2p.agave.core/bin"
elif [ -x "$AGAVE_LOCAL/solana" ]; then
    SOLANA_BIN="$AGAVE_LOCAL"
else
    SOLANA_BIN=""
fi
SOLANA_CLI="${SOLANA_BIN:+$SOLANA_BIN/}solana"

echo "=== Configuring Solana CLI ==="

# Set to localhost (remember the previous URL so the change is visible)
LOCAL_URL="http://127.0.0.1:8899"
PREVIOUS_URL=$("$SOLANA_CLI" config get | awk '/^RPC URL:/ {print $3}')
"$SOLANA_CLI" config set --url "$LOCAL_URL"
echo ""
if [ "$PREVIOUS_URL" != "$LOCAL_URL" ]; then
    echo "Changed global Solana CLI RPC URL: ${PREVIOUS_URL:-<unset>} -> $LOCAL_URL"
    echo "Restore it with: $SOLANA_CLI config set --url ${PREVIOUS_URL:-<previous-url>}"
else
    echo "Global Solana CLI RPC URL was already $LOCAL_URL (unchanged)."
fi

# Generate a keypair if none exists
KEYPAIR_PATH="${HOME}/.config/solana/id.json"
if [ ! -f "$KEYPAIR_PATH" ]; then
    echo "Generating new keypair..."
    "${SOLANA_BIN:+$SOLANA_BIN/}solana-keygen" new --no-bip39-passphrase -o "$KEYPAIR_PATH"
fi

# Show config
echo ""
echo "Current configuration:"
"$SOLANA_CLI" config get

# Request airdrop
echo ""
echo "Requesting airdrop..."
"$SOLANA_CLI" airdrop 100

# Show balance
echo ""
echo "Balance:"
"$SOLANA_CLI" balance
