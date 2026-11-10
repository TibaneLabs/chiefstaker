#!/bin/bash
# Upgrade the chiefstaker program on the currently-configured Solana cluster.
#
# By default this pulls the verifiable .so artifact from the latest successful
# CI run on master and deploys it as an upgrade to the production program ID
# (--program-id is passed explicitly so a missing flag can't accidentally
# create a brand-new program at a random address).
#
# A local .so can be supplied with --so to skip the CI download (useful when
# resuming a partially-failed deploy with the same artifact).
#
# Provenance checks on the CI artifact (audit finding B-1):
#   * default path: the run must be a `push` run of this repository's master
#     branch whose head commit equals the current origin/master;
#   * --run-id: the run must be a successful `push` run of this repository's
#     master branch (it may be an older commit than origin/master).
#   Pull-request runs (including fork PRs from a branch named "master") are
#   refused. --allow-unverified-run downgrades these checks to warnings.
#
# The sha256 of the .so is always printed and a typed confirmation is required
# before anything is sent to the cluster (--yes skips the prompt).
#
# Usage:
#   ./scripts/deploy-program.sh                       # latest master artifact
#   ./scripts/deploy-program.sh --run-id 26644748323  # specific CI run
#   ./scripts/deploy-program.sh --so /tmp/chiefstaker.so
#   ./scripts/deploy-program.sh --priority-fee 100000
#   ./scripts/deploy-program.sh --yes                 # no confirmation prompt

set -euo pipefail

PROGRAM_ID="3Ecf8gyRURyrBtGHS1XAVXyQik5PqgDch4VkxrH4ECcr"
REPO="TibaneLabs/chiefstaker"
WORKFLOW="CI"
WORKFLOW_PATH=".github/workflows/verifiable-build.yml"
RELEASE_BRANCH="master"
ARTIFACT_NAME="chiefstaker-verifiable"
ARTIFACT_FILE="chiefstaker.so"
DEFAULT_PRIORITY_FEE=50000

AGAVE_LOCAL="$HOME/.local/share/solana/install/active_release/bin"
if [ -x "/pkg/main/net-p2p.agave.core/bin/solana" ]; then
    SOLANA_CLI="/pkg/main/net-p2p.agave.core/bin/solana"
elif [ -x "$AGAVE_LOCAL/solana" ]; then
    SOLANA_CLI="$AGAVE_LOCAL/solana"
else
    SOLANA_CLI="solana"
fi

LOCAL_SO=""
RUN_ID=""
EXPECTED_SHA=""
DL_DIR=""
PRIORITY_FEE="$DEFAULT_PRIORITY_FEE"
ASSUME_YES=0
ALLOW_UNVERIFIED_RUN=0
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
    cat <<EOF
Usage: $0 [options]
  --so <path>           Deploy from a local .so file (skips CI download)
  --run-id <id>         Use a specific GitHub Actions run ID for the artifact
  --priority-fee <N>    Compute unit price, micro-lamports / CU (default: $DEFAULT_PRIORITY_FEE)
  --yes                 Skip the typed confirmation prompt (for automation)
  --allow-unverified-run
                        Deploy a CI artifact even if the run is not a successful
                        push run of $REPO $RELEASE_BRANCH at the expected commit
                        (checks become warnings; the confirmation still applies)
  -h, --help            Show this help

With no options: downloads the artifact from the latest successful $WORKFLOW
push run on $RELEASE_BRANCH (which must match origin/$RELEASE_BRANCH) and
upgrades program $PROGRAM_ID on the configured cluster.
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --so)             LOCAL_SO="${2:?--so requires a path}";            shift 2 ;;
        --run-id)         RUN_ID="${2:?--run-id requires a run ID}";       shift 2 ;;
        --priority-fee)   PRIORITY_FEE="${2:?--priority-fee requires a value}"; shift 2 ;;
        --yes)            ASSUME_YES=1;           shift ;;
        --allow-unverified-run) ALLOW_UNVERIFIED_RUN=1; shift ;;
        -h|--help)        usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

# --- Resolve the .so file ------------------------------------------------------
if [ -n "$LOCAL_SO" ]; then
    PROGRAM_SO="$LOCAL_SO"
    SOURCE_DESC="local file"
else
    if ! command -v gh >/dev/null 2>&1; then
        echo "Error: 'gh' CLI not found. Install it, or pass --so <path>." >&2
        exit 1
    fi

    if [ -z "$RUN_ID" ]; then
        # Default path: the artifact must come from a push run whose head
        # commit is the current tip of origin/$RELEASE_BRANCH.
        if ! git -C "$REPO_ROOT" fetch --quiet origin "$RELEASE_BRANCH"; then
            echo "Error: 'git fetch origin $RELEASE_BRANCH' failed; cannot determine the expected commit." >&2
            exit 1
        fi
        EXPECTED_SHA=$(git -C "$REPO_ROOT" rev-parse --verify "refs/remotes/origin/$RELEASE_BRANCH^{commit}")

        RUN_ID=$(gh run list -R "$REPO" --workflow "$WORKFLOW" --branch "$RELEASE_BRANCH" \
                    --event push --status success --limit 1 \
                    --json databaseId --jq '.[0].databaseId // empty')
        if [ -z "$RUN_ID" ]; then
            echo "Error: no successful $WORKFLOW push run found on $RELEASE_BRANCH." >&2
            exit 1
        fi
    fi

    case "$RUN_ID" in
        ''|*[!0-9]*) echo "Error: run ID must be numeric: $RUN_ID" >&2; exit 1 ;;
    esac

    # Look the run up and check where it came from. Pull-request runs (fork
    # or not) build unreviewed code and must never be deployed.
    RUN_META=$(gh api "repos/$REPO/actions/runs/$RUN_ID" --jq \
        '[.event, .head_sha, .head_branch, .head_repository.full_name, .repository.full_name, .conclusion, .path] | map(if . == null or . == "" then "-" else tostring end) | join(" ")')
    read -r RUN_EVENT RUN_SHA RUN_BRANCH RUN_HEAD_REPO RUN_REPO RUN_CONCLUSION RUN_PATH <<<"$RUN_META"

    echo "CI run $RUN_ID: event=$RUN_EVENT branch=$RUN_BRANCH repo=$RUN_HEAD_REPO"
    echo "             commit=$RUN_SHA conclusion=$RUN_CONCLUSION"

    PROBLEMS=()
    [ "$RUN_EVENT" = "push" ] || PROBLEMS+=("event is '$RUN_EVENT', expected 'push'")
    [ "$RUN_HEAD_REPO" = "$REPO" ] || PROBLEMS+=("head repository is '$RUN_HEAD_REPO', expected '$REPO'")
    [ "$RUN_REPO" = "$REPO" ] || PROBLEMS+=("run belongs to '$RUN_REPO', expected '$REPO'")
    [ "$RUN_BRANCH" = "$RELEASE_BRANCH" ] || PROBLEMS+=("branch is '$RUN_BRANCH', expected '$RELEASE_BRANCH'")
    [ "$RUN_CONCLUSION" = "success" ] || PROBLEMS+=("conclusion is '$RUN_CONCLUSION', expected 'success'")
    [ "$RUN_PATH" = "$WORKFLOW_PATH" ] || PROBLEMS+=("workflow is '$RUN_PATH', expected '$WORKFLOW_PATH'")
    if [ -n "$EXPECTED_SHA" ] && [ "$RUN_SHA" != "$EXPECTED_SHA" ]; then
        PROBLEMS+=("head commit $RUN_SHA is not origin/$RELEASE_BRANCH ($EXPECTED_SHA); CI may not have finished for the latest commit - wait for it, or pass --run-id <id> to deploy an older master build deliberately")
    fi

    if [ "${#PROBLEMS[@]}" -gt 0 ]; then
        for p in "${PROBLEMS[@]}"; do
            echo "  - $p" >&2
        done
        if [ "$ALLOW_UNVERIFIED_RUN" -eq 1 ]; then
            echo "WARNING: continuing despite the above because --allow-unverified-run was given." >&2
        else
            echo "Error: refusing to deploy the artifact of CI run $RUN_ID (see above)." >&2
            echo "       Pass --allow-unverified-run to override." >&2
            exit 1
        fi
    fi

    DL_DIR=$(mktemp -d -t chiefstaker-deploy-XXXXXX)
    trap 'rm -rf "$DL_DIR"' EXIT
    echo "Downloading $ARTIFACT_NAME from $REPO run $RUN_ID..."
    gh run download "$RUN_ID" -R "$REPO" -n "$ARTIFACT_NAME" -D "$DL_DIR"
    PROGRAM_SO="$DL_DIR/$ARTIFACT_FILE"
    if [ -n "$RUN_SHA" ]; then
        SOURCE_DESC="CI run $RUN_ID ($RUN_EVENT, $RUN_BRANCH @ $RUN_SHA)"
    else
        SOURCE_DESC="CI run $RUN_ID"
    fi
fi

if [ ! -f "$PROGRAM_SO" ]; then
    echo "Error: program file not found: $PROGRAM_SO" >&2
    exit 1
fi

# --- Pre-flight: confirm upgrade authority matches the configured keypair -----
LOCAL_AUTH=$("$SOLANA_CLI" address)
CHAIN_AUTH=$("$SOLANA_CLI" program show "$PROGRAM_ID" | awk '/^Authority:/ {print $2}')
RPC_URL=$("$SOLANA_CLI" config get | awk '/^RPC URL:/ {print $3}')
SO_SIZE=$(wc -c < "$PROGRAM_SO" | tr -d ' ')
if command -v sha256sum >/dev/null 2>&1; then
    SO_SHA256=$(sha256sum "$PROGRAM_SO" | awk '{print $1}')
else
    SO_SHA256=$(shasum -a 256 "$PROGRAM_SO" | awk '{print $1}')
fi
if [ "${#SO_SHA256}" -ne 64 ]; then
    echo "Error: could not compute the sha256 of $PROGRAM_SO" >&2
    exit 1
fi

echo ""
echo "=== Deploy plan ==="
echo "Program ID:    $PROGRAM_ID"
echo "Source:        $SOURCE_DESC"
echo "Binary:        $PROGRAM_SO ($SO_SIZE bytes)"
echo "sha256:        $SO_SHA256"
echo "Cluster:       $RPC_URL"
echo "Local key:     $LOCAL_AUTH"
echo "On-chain auth: $CHAIN_AUTH"
echo "Priority fee:  $PRIORITY_FEE micro-lamports / CU"
echo ""

if [ "$LOCAL_AUTH" != "$CHAIN_AUTH" ]; then
    echo "Error: local keypair is NOT the upgrade authority. Aborting." >&2
    exit 1
fi

# --- Confirmation --------------------------------------------------------------
# The sha256 above is of the raw .so file. Compare it with the artifact you
# expect (e.g. `shasum -a 256` of a local verifiable build of the same commit).
if [ "$ASSUME_YES" -eq 1 ]; then
    echo "--yes given: skipping confirmation."
else
    if [ ! -t 0 ]; then
        echo "Error: stdin is not a terminal; re-run interactively or pass --yes." >&2
        exit 1
    fi
    CONFIRM=""
    read -r -p "Type 'deploy' to upgrade $PROGRAM_ID on $RPC_URL: " CONFIRM || true
    if [ "$CONFIRM" != "deploy" ]; then
        echo "Aborted: nothing was deployed." >&2
        exit 1
    fi
fi

# --- Deploy --------------------------------------------------------------------
"$SOLANA_CLI" program deploy \
    --program-id "$PROGRAM_ID" \
    --with-compute-unit-price "$PRIORITY_FEE" \
    "$PROGRAM_SO"

echo ""
echo "Deployment complete!"
