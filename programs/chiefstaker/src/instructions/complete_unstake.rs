//! Complete unstake instruction (after cooldown period has elapsed)

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::Sysvar,
};
use spl_token_2022::extension::StateWithExtensions;

use crate::{
    error::StakingError,
    state::{is_valid_token_program, StakingPool, UserStake, POOL_SEED},
};

use super::unstake::{close_user_stake_account, execute_unstake};

/// Complete unstake after the cooldown has elapsed.
///
/// For requests created under the current flow (`unstake_request_settled == 1`)
/// the reward settlement and pool accounting already happened at RequestUnstake,
/// so this only delivers the frozen tokens and closes the account on a full
/// unstake.
///
/// For a legacy in-flight request created before this upgrade
/// (`unstake_request_settled == 0`), the coins are still counted in the pool, so
/// this runs the full unstake (settle + remove from pool + transfer + close),
/// exactly as the pre-upgrade complete path did.
///
/// Accounts (same as Unstake):
/// 0. `[writable]` Pool account
/// 1. `[writable]` User stake account
/// 2. `[writable]` Token vault
/// 3. `[writable]` User token account
/// 4. `[]` Token mint
/// 5. `[writable, signer]` User/owner
/// 6. `[]` Token 2022 program
/// 7. `[]` System program (optional)
/// 8. `[writable]` Metadata PDA (optional, to decrement member_count on close)
pub fn process_complete_unstake(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();

    let pool_info = next_account_info(account_info_iter)?;
    let user_stake_info = next_account_info(account_info_iter)?;
    let token_vault_info = next_account_info(account_info_iter)?;
    let user_token_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let user_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;

    // Validate token program (SPL Token or Token 2022)
    if !is_valid_token_program(token_program_info.key) {
        return Err(StakingError::InvalidTokenProgram.into());
    }

    // Validate user is signer
    if !user_info.is_signer {
        return Err(StakingError::MissingRequiredSigner.into());
    }

    // Load and validate pool
    if pool_info.owner != program_id {
        return Err(StakingError::InvalidAccountOwner.into());
    }
    let mut pool = StakingPool::try_from_slice(&pool_info.try_borrow_data()?)?;
    if !pool.is_initialized() {
        return Err(StakingError::NotInitialized.into());
    }

    // Verify pool PDA
    let (expected_pool, _) = StakingPool::derive_pda(&pool.mint, program_id);
    if *pool_info.key != expected_pool {
        return Err(StakingError::InvalidPDA.into());
    }

    // Check if pool needs rebasing
    if pool.get_sum_stake_exp().needs_rebase() {
        return Err(StakingError::PoolRequiresSync.into());
    }

    // Verify mint matches pool
    if pool.mint != *mint_info.key {
        return Err(StakingError::InvalidPoolMint.into());
    }

    // Verify token vault
    if pool.token_vault != *token_vault_info.key {
        return Err(StakingError::InvalidTokenVault.into());
    }

    // EX-6: the destination must not be the vault itself. A vault -> vault
    // self-transfer would debit the position while the tokens stay in the
    // vault with no way to recover them.
    if user_token_info.key == token_vault_info.key {
        return Err(ProgramError::InvalidArgument);
    }

    // Load and validate user stake
    if user_stake_info.owner != program_id {
        return Err(StakingError::InvalidAccountOwner.into());
    }
    let mut user_stake = UserStake::try_from_slice(&user_stake_info.try_borrow_data()?)?;
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

    // Check there is a pending request
    if !user_stake.has_pending_unstake_request() {
        return Err(StakingError::NoPendingUnstakeRequest.into());
    }

    // Check cooldown has elapsed
    let clock = Clock::get()?;
    let current_time = clock.unix_timestamp;
    let elapsed = current_time.saturating_sub(user_stake.unstake_request_time).max(0) as u64;
    if elapsed < pool.unstake_cooldown_seconds {
        return Err(StakingError::CooldownNotElapsed.into());
    }

    // Lazily adjust exp_start_factor if pool has been rebased (needed by the
    // legacy path's accounting; harmless for the new path).
    user_stake.sync_to_pool(&pool)?;

    let withdraw_amount = user_stake.unstake_request_amount;

    // System program (for legacy account realloc) and the metadata PDA (for the
    // member_count decrement on close) are both required accounts.
    let system_program_info = next_account_info(account_info_iter)?;
    let metadata_info = next_account_info(account_info_iter)?;

    if user_stake.unstake_request_settled == 0 {
        // ── Legacy in-flight request ──────────────────────────────────────
        // The frozen coins were never removed from the pool. Run the full unstake
        // accounting + transfer + close, exactly like the pre-upgrade flow.
        user_stake.unstake_request_amount = 0;
        user_stake.unstake_request_time = 0;

        return execute_unstake(
            program_id,
            &mut pool,
            &mut user_stake,
            pool_info,
            user_stake_info,
            token_vault_info,
            user_token_info,
            mint_info,
            user_info,
            withdraw_amount,
            current_time,
            Some(system_program_info),
            Some(metadata_info),
        );
    }

    // ── New-style request ─────────────────────────────────────────────────
    // Accounting already happened at RequestUnstake. Just deliver the frozen
    // tokens and close the account on a full unstake.
    user_stake.unstake_request_amount = 0;
    user_stake.unstake_request_time = 0;
    user_stake.unstake_request_settled = 0;

    // A full unstake (no active stake remaining) closes the account; a partial
    // completion leaves the still-active position open and earning.
    let should_close = user_stake.amount == 0;

    // Persist the cleared request fields before the CPI
    {
        let mut stake_data = user_stake_info.try_borrow_mut_data()?;
        user_stake.serialize(&mut &mut stake_data[..])?;
    }

    // Transfer the frozen tokens from vault to user (CPI, pool PDA signs)
    let mint_data = mint_info.try_borrow_data()?;
    let mint = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&mint_data)?;
    let decimals = mint.base.decimals;
    drop(mint_data);

    let pool_seeds = &[POOL_SEED, pool.mint.as_ref(), &[pool.bump]];

    invoke_signed(
        &spl_token_2022::instruction::transfer_checked(
            mint_info.owner,
            token_vault_info.key,
            mint_info.key,
            user_token_info.key,
            pool_info.key,
            &[],
            withdraw_amount,
            decimals,
        )?,
        &[
            token_vault_info.clone(),
            mint_info.clone(),
            user_token_info.clone(),
            pool_info.clone(),
        ],
        &[pool_seeds],
    )?;

    msg!("Completed unstake of {} tokens", withdraw_amount);

    // A full unstake fully resets the position; close the account to reclaim rent.
    if should_close {
        close_user_stake_account(program_id, pool_info, user_stake_info, user_info, Some(metadata_info))?;
    }

    Ok(())
}
