//! Close an empty user stake account to reclaim rent

use borsh::BorshDeserialize;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    pubkey::Pubkey,
};

use crate::{
    error::StakingError,
    math::WAD,
    state::{update_pool_member_count, StakingPool, UserStake},
};

/// Close a zero-balance user stake account, returning rent to the user.
///
/// Accounts:
/// 0. `[]` Pool account
/// 1. `[writable]` User stake account (PDA: ["stake", pool, owner])
/// 2. `[writable, signer]` User/owner (receives rent)
pub fn process_close_stake_account(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();

    let pool_info = next_account_info(account_info_iter)?;
    let user_stake_info = next_account_info(account_info_iter)?;
    let user_info = next_account_info(account_info_iter)?;
    // Metadata PDA (required). Tolerates an uninitialized account for pools that
    // never created metadata (see update_pool_member_count).
    let metadata_info = next_account_info(account_info_iter)?;

    // Validate user is signer
    if !user_info.is_signer {
        return Err(StakingError::MissingRequiredSigner.into());
    }

    // Load and validate pool
    if pool_info.owner != program_id {
        return Err(StakingError::InvalidAccountOwner.into());
    }
    let pool = StakingPool::try_from_slice(&pool_info.try_borrow_data()?)?;
    if !pool.is_initialized() {
        return Err(StakingError::NotInitialized.into());
    }

    // Verify pool PDA
    let (expected_pool, _) = StakingPool::derive_pda(&pool.mint, program_id);
    if *pool_info.key != expected_pool {
        return Err(StakingError::InvalidPDA.into());
    }

    // Load and validate user stake
    if user_stake_info.owner != program_id {
        return Err(StakingError::InvalidAccountOwner.into());
    }
    let user_stake = UserStake::try_from_slice(&user_stake_info.try_borrow_data()?)?;
    if !user_stake.is_initialized() {
        return Err(StakingError::NotInitialized.into());
    }

    // Verify ownership
    if user_stake.owner != *user_info.key {
        return Err(StakingError::InvalidOwner.into());
    }
    if user_stake.pool != *pool_info.key {
        return Err(StakingError::InvalidPool.into());
    }

    // Verify user stake PDA
    let (expected_stake, _) =
        UserStake::derive_pda(pool_info.key, user_info.key, program_id);
    if *user_stake_info.key != expected_stake {
        return Err(StakingError::InvalidPDA.into());
    }

    // Account must be empty: no staked tokens, no pending unstake request,
    // and no residual unclaimed rewards worth >= 1 lamport.
    // reward_debt stores unclaimed WAD-scaled rewards after a full unstake
    // when the pool lacked SOL. Sub-WAD dust (< 1 lamport) is forgiven
    // to prevent permanent lock of the account.
    if user_stake.amount > 0
        || user_stake.has_pending_unstake_request()
        || user_stake.reward_debt / WAD > 0
    {
        return Err(StakingError::AccountNotEmpty.into());
    }

    // Transfer all lamports from stake account to user (closes the account)
    let stake_lamports = user_stake_info.lamports();
    **user_stake_info.try_borrow_mut_lamports()? = 0;
    **user_info.try_borrow_mut_lamports()? += stake_lamports;

    // Zero out the account data so it can't be re-read as a valid stake
    {
        let mut stake_data = user_stake_info.try_borrow_mut_data()?;
        stake_data.fill(0);
    }

    // EX-2: fully close the account — shrink it to zero length and hand it back
    // to the system program, so a same-transaction re-fund cannot leave a
    // zeroed program-owned account that bricks this owner's stake PDA.
    user_stake_info.realloc(0, false)?;
    user_stake_info.assign(&solana_program::system_program::id());

    // Decrement member_count on close (metadata PDA is a required account).
    update_pool_member_count(program_id, pool_info, metadata_info, -1)?;

    msg!("Closed user stake account, returned {} lamports", stake_lamports);

    Ok(())
}
