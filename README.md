# ChiefStaker

A Solana program for staking Token 2022 tokens with time-weighted SOL reward distribution. Permissionless pool creation with creator-configurable settings.

## How It Works

Staking weight grows over time using an exponential curve:

```
weight = stake_amount * (1 - e^(-age / tau))
```

- New stakers start near **0% weight**
- Weight asymptotically approaches **100%** over time
- At 1 tau: ~63% weight
- At 3 tau: ~95% weight
- At 5 tau: ~99% weight

This prevents flash-stake attacks -- you can't just deposit right before a reward distribution and steal rewards from long-term stakers.

SOL rewards are distributed using a **snapshot-delta** formula. When rewards arrive, they are divided by `total_staked * WAD` (max weight) to produce an accumulator increment. Each staker's pending rewards are computed as:

```
pending = user_weighted * (acc_reward_per_weighted_share - snapshot) - claimed_rewards_wad
```

where `snapshot` is encoded in `reward_debt` and `claimed_rewards_wad` tracks cumulative payouts for frequency-independent claiming (claiming once or ten times yields the same total).

**Immature rewards** are the gap between max-weight entitlement and time-weighted entitlement — SOL the staker has earned "on paper" but can't claim until their weight matures further. These stay in the pool and are eventually redistributed to all stakers.

### Additional Stakes (Restaking)

When a staker adds more tokens to an existing position:

1. **Maturity preserved**: `exp_start_factor` is unchanged — maturity depends only on when the user first staked, not on amount
2. **Pending rewards preserved**: existing pending rewards (both vested and immature) stay exactly the same
3. **New tokens start fresh**: `reward_debt += new_amount × acc_reward_per_weighted_share` so new tokens don't earn rewards deposited before the add-stake

Adding tokens increases the staker's max-weight entitlement but does not change their maturity percentage. For example, a staker at 50% maturity who doubles their stake goes from earning 50% of 1M to 50% of 2M — the maturity percentage is unchanged, and pending rewards carry over seamlessly.

Rewards can be deposited directly via instruction or sent to the pool PDA (e.g., from pump.fun fee revenue) and synced.

## Features

- **Permissionless pool creation** -- anyone can create a staking pool for any Token 2022 mint
- **Time-weighted rewards** -- configurable tau (time constant) per pool
- **Creator-configurable settings** -- minimum stake amounts, lock durations, unstake cooldown periods
- **Authority management** -- transfer or renounce pool authority
- **Cooldown unstake flow** -- optional request/wait/complete unstake for pools that want it
- **O(1) operations** -- all instructions run in constant time regardless of staker count
- **Sybil resistant** -- splitting stake across accounts gives no advantage
- **Direct SOL rewards** -- SOL sent directly to the pool PDA is auto-detected via `SyncRewards`

## Program ID

```
3Ecf8gyRURyrBtGHS1XAVXyQik5PqgDch4VkxrH4ECcr
```

## Instructions

| # | Instruction | Description |
|---|-------------|-------------|
| 0 | `InitializePool` | Create a new staking pool for a Token 2022 mint |
| 1 | `Stake` | Stake tokens into the pool |
| 2 | `Unstake` | Unstake tokens (direct, when no cooldown); full unstake closes the account |
| 3 | `ClaimRewards` | Claim accumulated SOL rewards |
| 4 | `DepositRewards` | Deposit SOL rewards into the pool |
| 5 | `SyncPool` | Rebase pool math to prevent overflow |
| 6 | `SyncRewards` | Sync SOL sent directly to the pool PDA |
| 7 | `UpdatePoolSettings` | Set min stake, lock duration, cooldown (authority only) |
| 8 | `TransferAuthority` | Transfer or renounce pool authority |
| 9 | `RequestUnstake` | Start unstake cooldown (requested coins stop earning; earned rewards settled) |
| 10 | `CompleteUnstake` | Finish unstake after cooldown; full unstake closes the account |
| 11 | `CancelUnstakeRequest` | Cancel a pending request, restoring coins to the active position |
| 12 | `CloseStakeAccount` | Close zero-balance stake account to reclaim rent |
| 13 | ~~`FixTotalRewardDebt`~~ | Deprecated (no-op, returns error) |
| 14 | `SetPoolMetadata` | Set pool name, tags, and URL (permissionless) |
| 15 | `TakeFeeOwnership` | Claim pump.fun creator fee revenue for the pool |
| 16 | `StakeOnBehalf` | Stake tokens on behalf of another user (beneficiary) |
| 17 | `FixStakeAccount` | Fix a user's corrupted stake account (program upgrade authority only) |
| 18 | `MigrateStake` | Migrate a stake position to a new owner (1:1; destination must be empty; time/maturity/reward state preserved) |

## Pool Settings

Pool creators can configure these settings at any time (until authority is renounced):

| Setting | Default | Max | Description |
|---------|---------|-----|-------------|
| `min_stake_amount` | 0 (none) | 10^15 base units | Minimum position size a member must keep staked. Top-ups of any size are fine once at/above it; a partial unstake must leave at least this much (full withdrawal always allowed) |
| `lock_duration_seconds` | 0 (none) | 365 days | Time staker must wait after last deposit before unstaking |
| `unstake_cooldown_seconds` | 0 (none) | 30 days | Required cooldown period via request/complete flow |

The tau value (`tau_seconds`) is set at pool creation and is **immutable**.

## Building

```bash
# Build for Solana
./scripts/build-sbf.sh

# Run unit tests
cargo test
```

## Testing

```bash
# Build, start a local test validator, deploy to it and run the E2E tests
./scripts/run-e2e-tests.sh
```

`run-e2e-tests.sh` starts its own `solana-test-validator` on `127.0.0.1:8899`, deploys to it and stops it on exit. It passes the local URL and `~/.config/solana/id.json` explicitly to every CLI call and never modifies your global Solana CLI config. (`scripts/setup-cli.sh` is the one script that does point the global CLI config at the local validator; it prints the previous URL so you can restore it.)

CI runs the full E2E suite against a local test validator on every push.

## Deploying (maintainers)

`scripts/deploy-program.sh` upgrades the **production program** on whatever cluster the Solana CLI is configured for; it is not a test helper.

```bash
./scripts/deploy-program.sh                    # artifact of the latest CI push run on master
./scripts/deploy-program.sh --run-id <id>      # artifact of a specific CI run
./scripts/deploy-program.sh --so <path>        # a local .so (skips the CI download)
```

- CI artifacts are only accepted from successful `push` runs of this repository's `master` branch (pull-request runs do not upload the artifact and are refused). Without `--run-id`, the run's commit must also equal the current `origin/master`. `--allow-unverified-run` turns these checks into warnings.
- The script prints the sha256 of the `.so`, checks that the local keypair is the upgrade authority, and asks you to type `deploy` before sending anything. `--yes` skips the prompt (automation).
- `--priority-fee <N>` sets the compute unit price (micro-lamports / CU).

`scripts/publish-idl.sh [--rpc <url>] [--keypair <path>] [--idl <path>] [--yes]` publishes `idl.json` on-chain using a pinned version of `@solana-program/program-metadata`; it asks you to type `publish` first unless `--yes` is given.

## Verification

The deployed program is verified on the OtterSec registry. To verify locally:

```bash
./scripts/verify-deploy.sh
```

This runs `solana-verify verify-from-repo --remote` against the deployed program ID.

## Changelog

### v4 (current)

- **Add-stake maturity preservation**: on additional stake, `exp_start_factor` is unchanged (maturity depends only on start time, not amount). Pending rewards (vested and immature) carry over seamlessly. New tokens get a fresh `reward_debt` snapshot so they don't earn prior rewards. This prevents the dust-stake exploit while preserving all earned SOL across add-stake operations.
- **StakeOnBehalf**: new instruction allowing any signer to stake tokens on behalf of a beneficiary. The staker pays rent and provides tokens; the beneficiary owns the position.
- **TakeFeeOwnership**: new instruction to claim pump.fun creator fee revenue for the pool, setting the pool PDA as sole fee recipient and revoking the authority.
- **FixStakeAccount**: program upgrade authority instruction to fix a user's `exp_start_factor` and `reward_debt` corrupted by the old add-stake blending bug. After the fix, the user can claim corrected rewards via normal `ClaimRewards`.
- **FixTotalRewardDebt** (deprecated): was a one-time admin instruction to correct `total_reward_debt`. Slot 13 retained as no-op for ABI compatibility.
- **solana-security-txt**: embedded security contact info readable by explorers and auditors.

### v3

- **Legacy account realloc fix**: `maybe_realloc` uses system program CPI (`system_instruction::transfer`) instead of direct lamport manipulation, fixing "instruction spent from the balance of an account it does not own" for legacy accounts.
- **System program as trailing account**: instructions that call `maybe_realloc` (claim, unstake, request unstake, complete unstake, cancel unstake) accept an optional trailing system program account for legacy account resizing.
- **Frequency-independent claims**: `claimed_rewards_wad` field tracks cumulative payouts so claiming once or many times yields the same total. Prevents repeated-claim exploits.
- **`total_rewards_claimed` accounting**: per-user cumulative lamport counter for reward tracking.

## Project Structure

```
programs/chiefstaker/src/
  lib.rs                          # Entrypoint, instruction enum, dispatch
  state.rs                        # Account state (StakingPool, UserStake)
  error.rs                        # Error types
  math.rs                         # Fixed-point exponential math (WAD-scaled)
  instructions/
    initialize.rs                 # InitializePool
    stake.rs                      # Stake (with min stake + lock guards)
    unstake.rs                    # Unstake + shared execute_unstake helper
    claim.rs                      # ClaimRewards
    deposit.rs                    # DepositRewards
    sync.rs                       # SyncPool (rebase)
    sync_rewards.rs               # SyncRewards (detect direct SOL transfers)
    update_settings.rs            # UpdatePoolSettings
    transfer_authority.rs         # TransferAuthority
    request_unstake.rs            # RequestUnstake
    complete_unstake.rs           # CompleteUnstake
    cancel_unstake.rs             # CancelUnstakeRequest
    close_stake.rs                # CloseStakeAccount
    set_metadata.rs               # SetPoolMetadata
    take_fee_ownership.rs         # TakeFeeOwnership
    stake_on_behalf.rs            # StakeOnBehalf
    distribute_surplus.rs         # FixStakeAccount
tests/typescript/
  test_staking.ts                 # E2E tests
```

## License

[MIT](LICENSE)
