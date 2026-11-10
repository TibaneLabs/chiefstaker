#!/bin/bash
# Publish or update the on-chain IDL for Solscan account decoding
#
# Usage: ./scripts/publish-idl.sh [--rpc <url>] [--keypair <path>] [--idl <path>] [--yes]
#
# Defaults:
#   RPC:     https://api.mainnet-beta.solana.com
#   Keypair: ~/.config/solana/id.json
#   IDL:     ./idl.json
#
# The IDL is minified before upload to reduce on-chain size and transaction
# count.
#
# A confirmation prompt is shown before anything is sent; pass --yes to skip
# it (for automation).

set -euo pipefail

# Exact version of the @solana-program/program-metadata CLI to run.
#
# This is pinned on purpose: the CLI is handed the program's upgrade-authority
# keypair and talks to mainnet, so we must not execute whatever happens to be
# tagged "latest" on npm at the time (a compromised or simply breaking release
# would run with that key). To upgrade, review the new release, bump this
# value, and commit the change.
PROGRAM_METADATA_VERSION="0.11.0"

PROGRAM_ID="3Ecf8gyRURyrBtGHS1XAVXyQik5PqgDch4VkxrH4ECcr"
IDL_FILE="$(cd "$(dirname "$0")/.." && pwd)/idl.json"
RPC_URL="https://api.mainnet-beta.solana.com"
KEYPAIR="$HOME/.config/solana/id.json"
ASSUME_YES=0
AGAVE_LOCAL="$HOME/.local/share/solana/install/active_release/bin"
if [ -x "/pkg/main/net-p2p.agave.core/bin/solana" ]; then
    SOLANA_CLI="/pkg/main/net-p2p.agave.core/bin/solana"
elif [ -x "$AGAVE_LOCAL/solana" ]; then
    SOLANA_CLI="$AGAVE_LOCAL/solana"
else
    SOLANA_CLI="solana"
fi

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --rpc)     RPC_URL="${2:?--rpc requires a URL}";      shift 2 ;;
        --keypair) KEYPAIR="${2:?--keypair requires a path}"; shift 2 ;;
        --idl)     IDL_FILE="${2:?--idl requires a path}";    shift 2 ;;
        --yes)     ASSUME_YES=1; shift ;;
        -h|--help)
            echo "Usage: $0 [--rpc <url>] [--keypair <path>] [--idl <path>] [--yes]"
            exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ ! -f "$IDL_FILE" ]; then
    echo "Error: IDL file not found: $IDL_FILE" >&2
    exit 1
fi

if [ ! -f "$KEYPAIR" ]; then
    echo "Error: Keypair not found: $KEYPAIR" >&2
    exit 1
fi

# Private scratch directory (mktemp -d is portable across GNU and BSD/macOS,
# unlike a template with a suffix after the Xs). Holds the minified IDL and a
# clean install of the pinned CLI.
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/chiefstaker-idl.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT

# Minify IDL to reduce on-chain size and transaction count. The path is passed
# as an argument, never interpolated into the JS source.
MINIFIED="$WORK_DIR/idl.min.json"
node -e 'process.stdout.write(JSON.stringify(JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))))' \
    "$IDL_FILE" > "$MINIFIED"

ORIG_SIZE=$(wc -c < "$IDL_FILE" | tr -d ' ')
MIN_SIZE=$(wc -c < "$MINIFIED" | tr -d ' ')

echo "=== Publish IDL ==="
echo "Program:  $PROGRAM_ID"
echo "IDL file: $IDL_FILE ($ORIG_SIZE bytes, minified to $MIN_SIZE bytes)"
echo "RPC:      $RPC_URL"
echo "Keypair:  $KEYPAIR"
echo "CLI:      @solana-program/program-metadata@$PROGRAM_METADATA_VERSION"

# Show authority balance
if command -v "$SOLANA_CLI" >/dev/null 2>&1; then
    BALANCE=$("$SOLANA_CLI" balance --keypair "$KEYPAIR" --url "$RPC_URL" 2>/dev/null || echo "unknown")
    echo "Balance:  $BALANCE"
fi
echo ""

# Confirm before sending anything with the authority keypair.
if [ "$ASSUME_YES" -eq 1 ]; then
    echo "--yes given: skipping confirmation."
else
    if [ ! -t 0 ]; then
        echo "Error: stdin is not a terminal; re-run interactively or pass --yes." >&2
        exit 1
    fi
    CONFIRM=""
    read -r -p "Type 'publish' to write this IDL for $PROGRAM_ID via $RPC_URL: " CONFIRM || true
    if [ "$CONFIRM" != "publish" ]; then
        echo "Aborted: nothing was published." >&2
        exit 1
    fi
fi

# Install the pinned CLI into the scratch directory and run it from there.
# A clean install (rather than `npx` from the repo root) avoids the stale npx
# cache / peer-dependency conflicts that have broken this step before, and
# --ignore-scripts keeps dependency install scripts from running at all.
echo "Installing @solana-program/program-metadata@$PROGRAM_METADATA_VERSION..."
npm install --prefix "$WORK_DIR/cli" --no-save --ignore-scripts --no-audit --no-fund \
    "@solana-program/program-metadata@$PROGRAM_METADATA_VERSION"

"$WORK_DIR/cli/node_modules/.bin/program-metadata" write idl \
    "$PROGRAM_ID" \
    "$MINIFIED" \
    --keypair "$KEYPAIR" \
    --rpc "$RPC_URL"

echo ""
echo "IDL published successfully!"
echo "View on Solscan: https://solscan.io/account/$PROGRAM_ID"
