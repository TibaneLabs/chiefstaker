//! Unstake tokens instruction

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
    events::{emit_reward_payout, RewardPayoutType},
    math::{calculate_user_weighted_stake, wad_div, wad_mul, U256, WAD},
    state::{is_valid_token_program, update_pool_member_count, StakingPool, UserStake, POOL_SEED},
};

/// Accounting-only portion of an unstake, shared by direct `Unstake`
/// (`execute_unstake`) and the cooldown `RequestUnstake` flow.
///
/// Settles pending rewards, redistributes the unstaked portion's forfeited
/// immature SOL, removes the unstaked weight/stake from the pool, recomputes the
/// remaining position's reward debt, and pre-updates `last_synced_lamports`.
///
/// Mutates `pool` and `user_stake` in memory and reallocs the stake account if it
/// is a legacy size, but does NOT serialize accounts, transfer tokens, or move
/// reward lamports — the caller does those. Returns the lamports the caller must
/// transfer pool -> user as the reward payout.
///
/// Assumes all account validation has been done by the caller.
pub fn settle_unstake_accounting<'a>(
    pool: &mut StakingPool,
    user_stake: &mut UserStake,
    pool_info: &AccountInfo<'a>,
    user_stake_info: &AccountInfo<'a>,
    user_info: &AccountInfo<'a>,
    amount: u64,
    current_time: i64,
    system_program_info: Option<&AccountInfo<'a>>,
) -> Result<u64, ProgramError> {
    // EX-1: fold any unsynced lamports that arrived at the pool PDA since the
    // last sync into the accumulator BEFORE settling, exactly like Stake /
    // StakeOnBehalf / CancelUnstake do. Otherwise the exiting position settles
    // against a stale accumulator and (on a full exit) permanently loses its
    // share of those rewards. Done here so every exit path (Unstake,
    // RequestUnstake, legacy CompleteUnstake) gets it; all callers serialize
    // the pool afterwards. The reward payout computed below is subtracted from
    // last_synced_lamports and then leaves the pool, so
    // last_synced_lamports <= (pool lamports - rent minimum) keeps holding.
    {
        let rent_exempt_minimum = solana_program::rent::Rent::get()?
            .minimum_balance(pool_info.data_len());
        pool.sync_pending_rewards(pool_info.lamports(), rent_exempt_minimum, current_time)?;
    }

    // Capture old reward_debt for total_reward_debt bookkeeping
    let old_reward_debt = user_stake.reward_debt;

    // Calculate pending rewards (the caller defers the SOL transfer until after
    // any token CPI, because the Solana runtime verifies CPI account balances and
    // user_info is not a CPI account).
    let mut reward_transfer_amount: u64 = 0;

    let user_weighted = calculate_user_weighted_stake(
        user_stake.amount,
        user_stake.exp_start_factor,
        current_time,
        pool.base_time,
        pool.tau_seconds,
    )?;

    // Track unpaid rewards (WAD-scaled) — nonzero only when the payout below is
    // capped by the pool balance. On a full unstake it is redistributed via
    // last_synced_lamports; on a partial unstake the surviving position keeps
    // its (A-X)/A share pending and the unstaked X/A share is redistributed.
    let mut unpaid_rewards_wad: u128 = 0;

    // Compute delta_rps for the position being settled.  Needed for both
    // reward payout, forfeited-immature redistribution, and (on a partial
    // unstake) the proportional scaling of reward_debt / claimed_rewards_wad
    // for the remaining position.
    let amount_wad = (user_stake.amount as u128)
        .checked_mul(WAD)
        .ok_or(StakingError::MathOverflow)?;
    let delta_rps = if pool.acc_reward_per_weighted_share > 0 && amount_wad > 0 {
        let snapshot = wad_div(user_stake.reward_debt, amount_wad)?;
        pool.acc_reward_per_weighted_share.saturating_sub(snapshot)
    } else {
        0u128
    };

    // full_entitlement = what the FULL pre-unstake position has earned up to
    // now in weighted terms. Computed unconditionally so the partial-unstake
    // branch can scale it by (A-X)/A to preserve the remaining position's
    // claimed_rewards_wad tracker.
    let full_entitlement = if user_weighted > 0 && delta_rps > 0 {
        wad_mul(user_weighted, delta_rps)?
    } else {
        0u128
    };

    if full_entitlement > 0 {
        // Subtract already-claimed amount (frequency-independent)
        let pending = full_entitlement.saturating_sub(user_stake.claimed_rewards_wad);

        if pending > 0 {
            let pending_lamports = pending / WAD;

            if pending_lamports > 0 {
                let pool_lamports = pool_info.lamports();
                let rent_exempt_minimum = solana_program::rent::Rent::get()?
                    .minimum_balance(pool_info.data_len());

                let available_rewards = pool_lamports.saturating_sub(rent_exempt_minimum);
                reward_transfer_amount = pending_lamports.min(available_rewards as u128) as u64;

                // Track unpaid portion (only redistributed on the full-unstake path)
                let paid_wad = (reward_transfer_amount as u128)
                    .checked_mul(WAD)
                    .ok_or(StakingError::MathOverflow)?;
                unpaid_rewards_wad = pending.saturating_sub(paid_wad);

                // Pre-update last_synced_lamports (actual SOL transfer deferred to caller)
                if reward_transfer_amount > 0 {
                    pool.last_synced_lamports = pool.last_synced_lamports.saturating_sub(reward_transfer_amount);
                }
            }
        }
    }

    // Redistribute forfeited immature SOL for the unstaked portion only.
    // When a not-fully-mature staker unstakes, the gap between their max-weight
    // allocation and their time-weighted allocation is SOL that stays in the pool
    // but was already counted in last_synced_lamports.  By reducing
    // last_synced_lamports the next sync_rewards will detect this SOL as new
    // rewards and distribute it to remaining stakers.
    //
    // For partial unstakes we scale the forfeiture to the unstaked fraction so
    // the remaining position keeps its future maturity benefit.
    if delta_rps > 0 {
        let max_entitlement_wad = wad_mul(amount_wad, delta_rps)?;
        let weighted_entitlement_wad = if user_weighted > 0 {
            wad_mul(user_weighted, delta_rps)?
        } else {
            0u128
        };
        let forfeited_full_wad = max_entitlement_wad.saturating_sub(weighted_entitlement_wad);
        // Scale to the unstaked portion: forfeited * (unstake_amount / total_amount)
        let forfeited_wad = if (amount as u128) >= (user_stake.amount as u128) {
            forfeited_full_wad
        } else {
            // Use U256 to avoid overflow in the multiplication
            let wide = U256::from_u128(forfeited_full_wad) * U256::from_u128(amount as u128);
            (wide / U256::from_u128(user_stake.amount as u128))
                .to_u128()
                .ok_or(StakingError::MathOverflow)?
        };
        let forfeited_lamports = (forfeited_wad / WAD) as u64;
        if forfeited_lamports > 0 {
            pool.last_synced_lamports = pool.last_synced_lamports.saturating_sub(forfeited_lamports);
            msg!("Redistributing {} lamports of forfeited immature rewards", forfeited_lamports);
        }
    }

    // Calculate the unstaked portion's contribution to sum_stake_exp
    // contribution = amount * exp_start_factor
    let unstake_contribution = wad_mul(
        (amount as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?,
        user_stake.exp_start_factor,
    )?;

    // Update pool sum_stake_exp (saturating to handle rounding drift)
    let new_sum = pool
        .get_sum_stake_exp()
        .saturating_sub(U256::from_u128(unstake_contribution));
    pool.set_sum_stake_exp(new_sum);

    // Update pool total staked
    pool.total_staked = pool
        .total_staked
        .checked_sub(amount as u128)
        .ok_or(StakingError::MathUnderflow)?;

    // Update user stake
    user_stake.amount = user_stake
        .amount
        .checked_sub(amount)
        .ok_or(StakingError::MathUnderflow)?;

    // Recalculate reward debt for remaining stake
    if user_stake.amount > 0 {
        // Partial unstake: scale reward_debt and claimed_rewards_wad by the
        // remaining fraction (A-X)/A so the per-token snapshot stays at D/A.
        // This mirrors the claim path ("snapshot stays fixed so weight
        // maturation isn't forfeited") and preserves the remaining position's
        // claim on the immature gap of the pre-unstake delta period — the
        // forfeiture above already accounts for the unstaked fraction's share.
        // Resetting reward_debt to remaining * acc_rps (the prior behavior)
        // zeroed the user's immature-balance display from ~full_gap to 0 on
        // any partial unstake instead of dropping it by the unstaked fraction.
        let new_amount_u128 = user_stake.amount as u128;            // A - X
        let old_amount_u128 = new_amount_u128 + (amount as u128);   // A

        // reward_debt_new = old_reward_debt * (A-X) / A. U256 to avoid overflow.
        let wide_debt = U256::from_u128(old_reward_debt) * U256::from_u128(new_amount_u128);
        user_stake.reward_debt = (wide_debt / U256::from_u128(old_amount_u128))
            .to_u128()
            .ok_or(StakingError::MathOverflow)?;

        // claimed_rewards_wad_new = settled_entitlement * (A-X) / A, where
        // settled_entitlement = old claimed + what was ACTUALLY paid now.
        // In the normal (fully funded) case the payout is not capped, so
        // unpaid_rewards_wad == 0 and settled_entitlement == full_entitlement.
        // When the payout IS capped by the pool balance, marking the full
        // entitlement as claimed would silently destroy the unpaid portion;
        // using the settled amount keeps the surviving position's (A-X)/A
        // share of it pending (the per-token snapshot D/A is unchanged, so the
        // entitlement math recovers it once the pool is funded again).
        let settled_entitlement = full_entitlement.saturating_sub(unpaid_rewards_wad);
        let wide_claimed = U256::from_u128(settled_entitlement) * U256::from_u128(new_amount_u128);
        user_stake.claimed_rewards_wad = (wide_claimed / U256::from_u128(old_amount_u128))
            .to_u128()
            .ok_or(StakingError::MathOverflow)?;

        // The unstaked fraction X/A of the unpaid remainder can no longer be
        // claimed by the surviving position (its future entitlement scales by
        // (A-X)/A), so redistribute that share to the remaining stakers via
        // last_synced_lamports — exactly like the full-unstake branch does for
        // the whole remainder. No-op when the payout was not capped.
        if unpaid_rewards_wad > 0 {
            let wide_unpaid = U256::from_u128(unpaid_rewards_wad) * U256::from_u128(amount as u128);
            let unstaked_unpaid_wad = (wide_unpaid / U256::from_u128(old_amount_u128))
                .to_u128()
                .ok_or(StakingError::MathOverflow)?;
            let remainder_lamports = (unstaked_unpaid_wad / WAD) as u64;
            if remainder_lamports > 0 {
                pool.last_synced_lamports = pool.last_synced_lamports.saturating_sub(remainder_lamports);
            }
        }

        // Update pool-level aggregate: subtract old, add new (saturating for bootstrapping)
        pool.total_reward_debt = pool
            .total_reward_debt
            .saturating_sub(old_reward_debt)
            .checked_add(user_stake.reward_debt)
            .ok_or(StakingError::MathOverflow)?;
    } else {
        // Full unstake: the position is fully reset and the account will be closed.
        // The user's pending rewards were paid above (capped by the pool balance).
        // Any unpayable remainder should not occur — the pool holds every staker's
        // unclaimed rewards — but if it does, we redistribute it to the remaining
        // stakers via last_synced_lamports rather than stranding it on a closed
        // account or owing it to a user who is gone.
        let remainder_lamports = (unpaid_rewards_wad / WAD) as u64;
        if remainder_lamports > 0 {
            pool.last_synced_lamports = pool.last_synced_lamports.saturating_sub(remainder_lamports);
        }

        user_stake.reward_debt = 0;
        user_stake.claimed_rewards_wad = 0;

        // Remove the user's old debt from the pool aggregate; nothing is owed.
        pool.total_reward_debt = pool
            .total_reward_debt
            .saturating_sub(old_reward_debt);
    }

    // Increment cumulative rewards counter
    if reward_transfer_amount > 0 {
        user_stake.total_rewards_claimed = user_stake.total_rewards_claimed.saturating_add(reward_transfer_amount);
    }

    // Realloc legacy accounts to current size (payer = user)
    UserStake::maybe_realloc(user_stake_info, user_info, system_program_info)?;

    Ok(reward_transfer_amount)
}

/// Close a fully-unstaked user stake account: drain its rent lamports to the
/// user, zero its data, shrink it to zero length and reassign it to the system
/// program, and decrement the pool member_count if a metadata account is
/// supplied.
///
/// Mirrors `process_close_stake_account`. The caller must ensure the position is
/// fully unstaked before calling.
pub fn close_user_stake_account<'a>(
    program_id: &Pubkey,
    pool_info: &AccountInfo<'a>,
    user_stake_info: &AccountInfo<'a>,
    user_info: &AccountInfo<'a>,
    metadata_info: Option<&AccountInfo<'a>>,
) -> ProgramResult {
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
    // to the system program. Leaving a zeroed program-owned account around would
    // let a same-transaction re-fund keep it alive, bricking this owner's stake
    // PDA forever (Stake / Close would both fail with NotInitialized).
    user_stake_info.realloc(0, false)?;
    user_stake_info.assign(&solana_program::system_program::id());

    // Decrement member_count on close (metadata PDA is a required account on the
    // close paths; an uninitialized account is tolerated for metadata-less pools).
    if let Some(metadata_info) = metadata_info {
        update_pool_member_count(program_id, pool_info, metadata_info, -1)?;
    }

    msg!("Closed user stake account, returned {} lamports", stake_lamports);
    Ok(())
}

/// Shared unstake logic for the direct `Unstake` path (cooldown == 0).
///
/// Settles rewards and pool math via `settle_unstake_accounting`, transfers the
/// staked tokens back to the user, pays out the reward SOL, and — when the
/// position is fully unstaked with nothing owed — closes the stake account.
///
/// Assumes all account validation has been done by the caller.
#[allow(clippy::too_many_arguments)]
pub fn execute_unstake<'a>(
    program_id: &Pubkey,
    pool: &mut StakingPool,
    user_stake: &mut UserStake,
    pool_info: &AccountInfo<'a>,
    user_stake_info: &AccountInfo<'a>,
    token_vault_info: &AccountInfo<'a>,
    user_token_info: &AccountInfo<'a>,
    mint_info: &AccountInfo<'a>,
    user_info: &AccountInfo<'a>,
    amount: u64,
    current_time: i64,
    system_program_info: Option<&AccountInfo<'a>>,
    metadata_info: Option<&AccountInfo<'a>>,
) -> ProgramResult {
    let reward_transfer_amount = settle_unstake_accounting(
        pool,
        user_stake,
        pool_info,
        user_stake_info,
        user_info,
        amount,
        current_time,
        system_program_info,
    )?;

    // A full unstake fully resets the position; the account is always closed
    // afterward (the user's rewards were settled and paid in full above).
    let fully_unstaked = user_stake.amount == 0;

    // Save states (before CPI — pool data includes pre-updated last_synced_lamports)
    {
        let mut pool_data = pool_info.try_borrow_mut_data()?;
        pool.serialize(&mut &mut pool_data[..])?;
    }
    {
        let mut stake_data = user_stake_info.try_borrow_mut_data()?;
        user_stake.serialize(&mut &mut stake_data[..])?;
    }

    // Transfer tokens from vault to user (CPI)
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
            amount,
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

    // Transfer SOL rewards AFTER token CPI to avoid CPI balance check failure
    // (pool_info is a CPI account but user_info is not)
    if reward_transfer_amount > 0 {
        **pool_info.try_borrow_mut_lamports()? -= reward_transfer_amount;
        **user_info.try_borrow_mut_lamports()? += reward_transfer_amount;
        msg!("Claimed {} lamports in rewards", reward_transfer_amount);
        emit_reward_payout(pool_info.key, user_info.key, reward_transfer_amount, RewardPayoutType::Unstake);
    }

    msg!("Unstaked {} tokens", amount);

    // Full unstake closes the account to reclaim its rent.
    if fully_unstaked {
        close_user_stake_account(program_id, pool_info, user_stake_info, user_info, metadata_info)?;
    }

    Ok(())
}

/// Unstake tokens from the pool (direct unstake when cooldown is 0)
///
/// Accounts:
/// 0. `[writable]` Pool account
/// 1. `[writable]` User stake account
/// 2. `[writable]` Token vault
/// 3. `[writable]` User token account
/// 4. `[]` Token mint
/// 5. `[writable, signer]` User/owner
/// 6. `[]` Token 2022 program
/// 7. `[]` System program (optional, for legacy account reallocation)
/// 8. `[writable]` Metadata PDA (optional, to decrement member_count on full-unstake close)
pub fn process_unstake(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
) -> ProgramResult {
    if amount == 0 {
        return Err(StakingError::ZeroAmount.into());
    }

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

    // If pool has a cooldown, reject direct unstake
    if pool.unstake_cooldown_seconds > 0 {
        return Err(StakingError::CooldownRequired.into());
    }

    // Verify mint matches pool
    if pool.mint != *mint_info.key {
        return Err(StakingError::InvalidPoolMint.into());
    }

    // Verify token vault
    if pool.token_vault != *token_vault_info.key {
        return Err(StakingError::InvalidTokenVault.into());
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

    // Check sufficient balance
    if user_stake.amount < amount {
        return Err(StakingError::InsufficientStakeBalance.into());
    }

    // A partial unstake must leave at least min_stake_amount staked. Withdrawing
    // the whole position is always allowed (the minimum is a floor on what a
    // member must keep staked, not a per-transaction amount).
    let remaining = user_stake.amount - amount;
    if remaining > 0 && pool.min_stake_amount > 0 && remaining < pool.min_stake_amount {
        return Err(StakingError::RemainingStakeBelowMinimum.into());
    }

    // Block if pending unstake request
    if user_stake.has_pending_unstake_request() {
        return Err(StakingError::PendingUnstakeRequestExists.into());
    }

    // Lazily adjust exp_start_factor if pool has been rebased
    user_stake.sync_to_pool(&pool)?;

    let clock = Clock::get()?;
    let current_time = clock.unix_timestamp;

    // Check lock duration
    if pool.lock_duration_seconds > 0 {
        let last_stake = user_stake.effective_last_stake_time();
        let elapsed = current_time.saturating_sub(last_stake).max(0) as u64;
        if elapsed < pool.lock_duration_seconds {
            return Err(StakingError::StakeLocked.into());
        }
    }

    // System program (for legacy account realloc) and the metadata PDA (for the
    // member_count decrement on a full-unstake close) are both required accounts.
    let system_program_info = next_account_info(account_info_iter)?;
    let metadata_info = next_account_info(account_info_iter)?;

    // Execute the shared unstake logic
    execute_unstake(
        program_id,
        &mut pool,
        &mut user_stake,
        pool_info,
        user_stake_info,
        token_vault_info,
        user_token_info,
        mint_info,
        user_info,
        amount,
        current_time,
        Some(system_program_info),
        Some(metadata_info),
    )
}
