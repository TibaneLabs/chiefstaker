#!/bin/bash
# Verify the deployed program against the repo on OtterSec
#
# Usage: ./verify-deploy.sh [commit-hash]
# If no commit hash is provided, uses the current HEAD.

set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

PROGRAM_ID="3Ecf8gyRURyrBtGHS1XAVXyQik5PqgDch4VkxrH4ECcr"
REPO_URL="https://github.com/TibaneLabs/chiefstaker"
LIBRARY_NAME="chiefstaker"

COMMIT="${1:-$(git rev-parse HEAD)}"

echo "Verifying program $PROGRAM_ID"
echo "Repo: $REPO_URL"
echo "Commit: $COMMIT"
echo ""

solana-verify verify-from-repo --remote -y \
  --program-id "$PROGRAM_ID" \
  --commit-hash "$COMMIT" \
  --library-name "$LIBRARY_NAME" \
  "$REPO_URL"
