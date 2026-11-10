//! ChiefStaker: Token 2022 Staking with Time-Weighted Rewards
//!
//! A Solana program allowing Token 2022 holders to stake tokens and receive
//! SOL rewards distributed proportionally based on staking duration.
//!
//! Weight formula: `weight = stake_amount × (1 - e^(-age/τ))`
//! - New stakers start near 0% weight
//! - Weight asymptotically approaches 100% over time
//! - At τ: weight ≈ 63% of max
//! - At 3τ: weight ≈ 95% of max

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::AccountInfo, entrypoint, entrypoint::ProgramResult, msg,
    program_error::ProgramError, pubkey::Pubkey,
};

pub mod error;
pub mod events;
pub mod instructions;
pub mod math;
pub mod state;

use instructions::*;

// Declare program ID - matches target/deploy/chiefstaker-keypair.json
solana_program::declare_id!("3Ecf8gyRURyrBtGHS1XAVXyQik5PqgDch4VkxrH4ECcr");

/// Program instructions
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub enum StakingInstruction {
    /// Initialize a new staking pool for a Token 2022 mint
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account (PDA: ["pool", mint])
    /// 1. `[]` Token mint (Token 2022)
    /// 2. `[writable]` Token vault (PDA: ["token_vault", pool])
    /// 3. `[writable, signer]` Authority/payer
    /// 4. `[]` System program
    /// 5. `[]` Token 2022 program
    /// 6. `[]` Rent sysvar
    InitializePool {
        /// Time constant in seconds (e.g., 2592000 for 30 days)
        tau_seconds: u64,
    },

    /// Stake tokens into the pool
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[writable]` User stake account (PDA: ["stake", pool, owner])
    /// 2. `[writable]` Token vault
    /// 3. `[writable]` User token account
    /// 4. `[]` Token mint
    /// 5. `[writable, signer]` User/owner
    /// 6. `[]` System program
    /// 7. `[]` Token 2022 program
    /// 8. `[writable]` Metadata PDA (required; member_count, uninitialized tolerated)
    Stake {
        /// Amount of tokens to stake
        amount: u64,
    },

    /// Unstake tokens from the pool.
    /// A partial unstake must leave at least `min_stake_amount` staked (if set);
    /// unstaking the full position is always allowed.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[writable]` User stake account
    /// 2. `[writable]` Token vault
    /// 3. `[writable]` User token account
    /// 4. `[]` Token mint
    /// 5. `[writable, signer]` User/owner
    /// 6. `[]` Token 2022 program
    /// 7. `[]` System program (required; for legacy account reallocation)
    /// 8. `[writable]` Metadata PDA (required; member_count, uninitialized tolerated)
    Unstake {
        /// Amount of tokens to unstake
        amount: u64,
    },

    /// Claim accumulated SOL rewards
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account (holds SOL rewards)
    /// 1. `[writable]` User stake account
    /// 2. `[writable, signer]` User/owner
    ClaimRewards,

    /// Deposit SOL rewards into the pool (permissionless)
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account (receives SOL)
    /// 1. `[writable, signer]` Depositor
    /// 2. `[]` System program
    DepositRewards {
        /// Amount of lamports to deposit
        amount: u64,
    },

    /// Sync/rebase the pool to prevent overflow (permissionless crank)
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    SyncPool,

    /// Sync rewards sent directly to the pool (permissionless crank)
    /// Use this when SOL is sent directly to the pool PDA (e.g., from pump.fun)
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    SyncRewards,

    /// Update pool settings (authority only)
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[signer]` Authority
    UpdatePoolSettings {
        min_stake_amount: Option<u64>,
        lock_duration_seconds: Option<u64>,
        unstake_cooldown_seconds: Option<u64>,
    },

    /// Transfer pool authority to a new address
    /// Setting to Pubkey::default() renounces authority (irreversible)
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[signer]` Current authority
    TransferAuthority {
        new_authority: Pubkey,
    },

    /// Request unstake - starts cooldown period.
    /// The requested coins stop earning immediately (removed from pool weight) and
    /// their already-earned rewards are settled/paid out. Only the token transfer
    /// is deferred to CompleteUnstake. A partial request must leave at least
    /// `min_stake_amount` staked (if set); requesting the full position is always allowed.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[writable]` User stake account
    /// 2. `[writable, signer]` User/owner (receives settled reward SOL)
    /// 3. `[]` System program (optional, for legacy account reallocation)
    RequestUnstake {
        amount: u64,
    },

    /// Complete unstake after cooldown elapsed.
    /// Delivers the frozen tokens and closes the account on a full unstake.
    ///
    /// Accounts (same as Unstake):
    /// 0. `[writable]` Pool account
    /// 1. `[writable]` User stake account
    /// 2. `[writable]` Token vault
    /// 3. `[writable]` User token account
    /// 4. `[]` Token mint
    /// 5. `[writable, signer]` User/owner
    /// 6. `[]` Token 2022 program
    /// 7. `[]` System program (required)
    /// 8. `[writable]` Metadata PDA (required; member_count, uninitialized tolerated)
    CompleteUnstake,

    /// Cancel a pending unstake request, restoring the frozen coins to the
    /// active staking position.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[writable]` User stake account
    /// 2. `[signer]` User/owner
    /// 3. `[]` System program (optional, for legacy account reallocation)
    CancelUnstakeRequest,

    /// Close a zero-balance user stake account to reclaim rent
    ///
    /// Accounts:
    /// 0. `[]` Pool account
    /// 1. `[writable]` User stake account
    /// 2. `[writable, signer]` User/owner
    /// 3. `[writable]` Metadata PDA (required; member_count, uninitialized tolerated)
    CloseStakeAccount,

    /// DEPRECATED: Slot 13 reserved for ABI compatibility (was FixTotalRewardDebt).
    /// Always returns InvalidInstruction.
    DeprecatedFixTotalRewardDebt {
        _unused: u128,
    },

    /// Set (create or update) pool metadata for explorer display (permissionless)
    ///
    /// Derives name from Token 2022 mint metadata extension, tags are fixed.
    /// Creates the metadata PDA if it doesn't exist.
    ///
    /// Accounts:
    /// 0. `[]` Pool account
    /// 1. `[writable]` Metadata PDA (["metadata", pool])
    /// 2. `[]` Token mint
    /// 3. `[writable, signer]` Payer
    /// 4. `[]` System program
    SetPoolMetadata,

    /// Take fee ownership: set pool as sole fee recipient and revoke authority (permissionless)
    ///
    /// Prerequisite: fee sharing authority must already be transferred to the pool PDA.
    /// This instruction sets the pool PDA as the sole shareholder (100% / 10000 bps)
    /// and then revokes the fee sharing authority, making it irreversible.
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account (PDA: ["pool", mint]; CPI signer and payer)
    /// 1. `[]` Token mint
    /// 2. `[]` pfee program
    /// 3. `[]` pfee event authority
    /// 4. `[]` pump global
    /// 5. `[writable]` sharing config
    /// 6. `[]` bonding curve
    /// 7. `[writable]` pump creator vault
    /// 8. `[]` system program
    /// 9. `[]` pump program
    /// 10. `[]` pump event authority
    /// 11. `[]` pump AMM program
    /// 12. `[]` AMM event authority
    /// 13. `[]` wSOL mint
    /// 14. `[]` token program
    /// 15. `[]` associated token program
    /// 16. `[writable]` coin creator vault authority
    /// 17. `[writable]` coin creator vault ATA
    TakeFeeOwnership,

    /// Stake tokens on behalf of another user (beneficiary)
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[writable]` Beneficiary stake account (PDA: ["stake", pool, beneficiary])
    /// 2. `[writable]` Token vault
    /// 3. `[writable]` Staker's token account
    /// 4. `[]` Token mint
    /// 5. `[writable, signer]` Staker (payer + token source)
    /// 6. `[writable]` Beneficiary (receives position)
    /// 7. `[]` System program
    /// 8. `[]` Token 2022 program
    /// 9. `[writable]` Metadata PDA (required; member_count, uninitialized tolerated)
    StakeOnBehalf {
        amount: u64,
    },

    /// Fix a user's stake account corrupted by the add-stake blending bug.
    /// Sets exp_start_factor and reward_debt to correct values.
    /// (program upgrade authority only)
    ///
    /// Accounts:
    /// 0. `[writable]` Pool account
    /// 1. `[writable]` User stake account
    /// 2. `[signer]`   Program upgrade authority
    /// 3. `[]`         ProgramData account
    FixStakeAccount {
        new_exp_start_factor: u128,
        new_reward_debt: u128,
    },

    /// Migrate a stake position from one owner to another (clean 1:1 transfer).
    /// Destination must be empty; all time/maturity/reward state is preserved.
    /// Pool aggregates and `member_count` are unchanged.
    ///
    /// Accounts:
    /// 0. `[]` Pool account
    /// 1. `[writable]` Source user stake (PDA: ["stake", pool, source_owner]) — closed
    /// 2. `[writable, signer]` Source owner (pays dest rent, receives source rent)
    /// 3. `[writable]` Destination user stake (PDA: ["stake", pool, new_owner]) — must be empty
    /// 4. `[]` New owner (pubkey only; no signer required)
    /// 5. `[]` System program
    MigrateStake,
}

#[cfg(not(feature = "no-entrypoint"))]
entrypoint!(process_instruction);

#[cfg(not(feature = "no-entrypoint"))]
use solana_security_txt::security_txt;

#[cfg(not(feature = "no-entrypoint"))]
security_txt! {
    name: "ChiefStaker",
    project_url: "https://github.com/TibaneLabs/chiefstaker",
    contacts: "link:https://github.com/TibaneLabs/chiefstaker/security/advisories",
    policy: "https://github.com/TibaneLabs/chiefstaker/security/policy",
    source_code: "https://github.com/TibaneLabs/chiefstaker"
}

/// Program entrypoint
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    // Verify this is the correct program
    if program_id != &crate::id() {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Deserialize instruction
    let instruction = StakingInstruction::try_from_slice(instruction_data)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    // Dispatch to appropriate handler
    match instruction {
        StakingInstruction::InitializePool { tau_seconds } => {
            msg!("Instruction: InitializePool (tau={}s)", tau_seconds);
            process_initialize_pool(program_id, accounts, tau_seconds)
        }
        StakingInstruction::Stake { amount } => {
            msg!("Instruction: Stake (amount={})", amount);
            process_stake(program_id, accounts, amount)
        }
        StakingInstruction::Unstake { amount } => {
            msg!("Instruction: Unstake (amount={})", amount);
            process_unstake(program_id, accounts, amount)
        }
        StakingInstruction::ClaimRewards => {
            msg!("Instruction: ClaimRewards");
            process_claim_rewards(program_id, accounts)
        }
        StakingInstruction::DepositRewards { amount } => {
            msg!("Instruction: DepositRewards (amount={})", amount);
            process_deposit_rewards(program_id, accounts, amount)
        }
        StakingInstruction::SyncPool => {
            msg!("Instruction: SyncPool");
            process_sync_pool(program_id, accounts)
        }
        StakingInstruction::SyncRewards => {
            msg!("Instruction: SyncRewards");
            process_sync_rewards(program_id, accounts)
        }
        StakingInstruction::UpdatePoolSettings {
            min_stake_amount,
            lock_duration_seconds,
            unstake_cooldown_seconds,
        } => {
            msg!("Instruction: UpdatePoolSettings");
            process_update_pool_settings(
                program_id,
                accounts,
                min_stake_amount,
                lock_duration_seconds,
                unstake_cooldown_seconds,
            )
        }
        StakingInstruction::TransferAuthority { new_authority } => {
            msg!("Instruction: TransferAuthority");
            process_transfer_authority(program_id, accounts, new_authority)
        }
        StakingInstruction::RequestUnstake { amount } => {
            msg!("Instruction: RequestUnstake (amount={})", amount);
            process_request_unstake(program_id, accounts, amount)
        }
        StakingInstruction::CompleteUnstake => {
            msg!("Instruction: CompleteUnstake");
            process_complete_unstake(program_id, accounts)
        }
        StakingInstruction::CancelUnstakeRequest => {
            msg!("Instruction: CancelUnstakeRequest");
            process_cancel_unstake_request(program_id, accounts)
        }
        StakingInstruction::CloseStakeAccount => {
            msg!("Instruction: CloseStakeAccount");
            process_close_stake_account(program_id, accounts)
        }
        StakingInstruction::DeprecatedFixTotalRewardDebt { .. } => {
            msg!("Instruction: FixTotalRewardDebt (deprecated, no-op)");
            Err(ProgramError::InvalidInstructionData)
        }
        StakingInstruction::SetPoolMetadata => {
            msg!("Instruction: SetPoolMetadata");
            process_set_pool_metadata(program_id, accounts)
        }
        StakingInstruction::TakeFeeOwnership => {
            msg!("Instruction: TakeFeeOwnership");
            process_take_fee_ownership(program_id, accounts)
        }
        StakingInstruction::StakeOnBehalf { amount } => {
            msg!("Instruction: StakeOnBehalf (amount={})", amount);
            process_stake_on_behalf(program_id, accounts, amount)
        }
        StakingInstruction::FixStakeAccount {
            new_exp_start_factor,
            new_reward_debt,
        } => {
            msg!("Instruction: FixStakeAccount");
            process_fix_stake_account(
                program_id,
                accounts,
                new_exp_start_factor,
                new_reward_debt,
            )
        }
        StakingInstruction::MigrateStake => {
            msg!("Instruction: MigrateStake");
            process_migrate_stake(program_id, accounts)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_instruction_serialization() {
        let instruction = StakingInstruction::InitializePool {
            tau_seconds: 2592000,
        };
        let serialized = borsh::to_vec(&instruction).unwrap();
        let deserialized: StakingInstruction =
            BorshDeserialize::try_from_slice(&serialized).unwrap();

        match deserialized {
            StakingInstruction::InitializePool { tau_seconds } => {
                assert_eq!(tau_seconds, 2592000);
            }
            _ => panic!("Wrong instruction type"),
        }
    }

    #[test]
    fn test_stake_instruction() {
        let instruction = StakingInstruction::Stake { amount: 1_000_000 };
        let serialized = borsh::to_vec(&instruction).unwrap();
        let deserialized: StakingInstruction =
            BorshDeserialize::try_from_slice(&serialized).unwrap();

        match deserialized {
            StakingInstruction::Stake { amount } => {
                assert_eq!(amount, 1_000_000);
            }
            _ => panic!("Wrong instruction type"),
        }
    }
}
