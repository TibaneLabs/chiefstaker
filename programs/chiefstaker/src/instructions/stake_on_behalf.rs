//! Stake tokens on behalf of another user (beneficiary)

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
use spl_token_2022::extension::StateWithExtensions;

use crate::{
    error::StakingError,
    math::{exp_time_ratio, wad_mul, MAX_STAKE_EXP_INPUT, U256, WAD},
    state::{
        create_pda_account, is_valid_token_program, update_pool_member_count, StakingPool,
        UserStake, STAKE_SEED,
    },
};

/// Stake tokens on behalf of another user (beneficiary)
///
/// Accounts:
/// 0. `[writable]` Pool account
/// 1. `[writable]` Beneficiary stake account (PDA: ["stake", pool, beneficiary])
/// 2. `[writable]` Token vault
/// 3. `[writable]` Staker's token account (A's tokens)
/// 4. `[]` Token mint
/// 5. `[writable, signer]` Staker (A) — signs, pays rent, provides tokens
/// 6. `[writable]` Beneficiary (B) — NOT a signer, receives position
/// 7. `[]` System program
/// 8. `[]` Token 2022 program
pub fn process_stake_on_behalf(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
) -> ProgramResult {
    if amount == 0 {
        return Err(StakingError::ZeroAmount.into());
    }

    let account_info_iter = &mut accounts.iter();

    let pool_info = next_account_info(account_info_iter)?;
    let beneficiary_stake_info = next_account_info(account_info_iter)?;
    let token_vault_info = next_account_info(account_info_iter)?;
    let staker_token_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let staker_info = next_account_info(account_info_iter)?;
    let beneficiary_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    // Metadata PDA (required). See update_pool_member_count for the metadata-less
    // pool (uninitialized account) tolerance.
    let metadata_info = next_account_info(account_info_iter)?;

    // Validate token program (SPL Token or Token 2022)
    if !is_valid_token_program(token_program_info.key) {
        return Err(StakingError::InvalidTokenProgram.into());
    }

    // Validate staker is signer (beneficiary does NOT need to sign)
    if !staker_info.is_signer {
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

    // Verify mint matches pool
    if pool.mint != *mint_info.key {
        return Err(StakingError::InvalidPoolMint.into());
    }

    // Verify token vault
    if pool.token_vault != *token_vault_info.key {
        return Err(StakingError::InvalidTokenVault.into());
    }

    // Verify beneficiary stake PDA (derived from beneficiary, not staker)
    let (expected_stake, stake_bump) =
        UserStake::derive_pda(pool_info.key, beneficiary_info.key, program_id);
    if *beneficiary_stake_info.key != expected_stake {
        return Err(StakingError::InvalidPDA.into());
    }

    let clock = Clock::get()?;
    let current_time = clock.unix_timestamp;
    let rent = Rent::get()?;

    // Fold any unsynced direct-donation lamports into the accumulator BEFORE
    // snapshotting reward_debt or growing total_staked. Otherwise a just-in-time
    // staker could dilute pending rewards (stake against the stale accumulator,
    // then run the permissionless SyncRewards over the inflated total_staked)
    // and siphon SOL that belongs to the pre-existing stakers.
    let rent_exempt_minimum = rent.minimum_balance(pool_info.data_len());
    pool.sync_pending_rewards(pool_info.lamports(), rent_exempt_minimum, current_time)?;

    // Check if pool needs rebasing (sum_stake_exp near overflow)
    if pool.get_sum_stake_exp().needs_rebase() {
        return Err(StakingError::PoolRequiresSync.into());
    }

    // Calculate exp_start_factor for this stake
    let time_since_base = current_time.saturating_sub(pool.base_time);

    // Check that time_since_base / tau is small enough for a new stake to be
    // weighted precisely (and, a fortiori, for exp_wad not to overflow).
    // Require SyncPool first if the ratio exceeds MAX_STAKE_EXP_INPUT (see
    // its doc comment: precision collapses well before MAX_EXP_INPUT).
    let ratio_wad = (time_since_base as u128)
        .checked_mul(WAD)
        .ok_or(StakingError::MathOverflow)?
        / (pool.tau_seconds as u128);
    if ratio_wad > MAX_STAKE_EXP_INPUT {
        return Err(StakingError::PoolRequiresSync.into());
    }

    let exp_start_factor = exp_time_ratio(time_since_base, pool.tau_seconds)?;

    // Create or update beneficiary stake account
    let is_new_stake = beneficiary_stake_info.data_is_empty();

    if is_new_stake {
        // Check minimum stake amount
        if pool.min_stake_amount > 0 && amount < pool.min_stake_amount {
            return Err(StakingError::BelowMinimumStake.into());
        }

        // Create new beneficiary stake account (staker pays rent). Uses the
        // pre-fund-tolerant helper so a griefer cannot brick first-time staking
        // by sending the deterministic stake PDA a lamport beforehand (plain
        // create_account requires a zero balance at the destination).
        let stake_seeds = &[
            STAKE_SEED,
            pool_info.key.as_ref(),
            beneficiary_info.key.as_ref(),
            &[stake_bump],
        ];

        create_pda_account(
            staker_info, // staker pays rent
            beneficiary_stake_info,
            system_program_info,
            program_id,
            UserStake::LEN,
            &rent,
            stake_seeds,
        )?;

        // Initialize user stake with beneficiary as owner
        let mut user_stake = UserStake::new(
            *beneficiary_info.key,  // beneficiary owns the position
            *pool_info.key,
            amount,
            current_time,
            exp_start_factor,
            stake_bump,
            pool.base_time,
        );

        // Set reward_debt using max weight (amount * WAD) to prevent accessing prior rewards
        user_stake.reward_debt = wad_mul(
            (amount as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?,
            pool.acc_reward_per_weighted_share,
        )?;

        // Track in pool-level aggregate
        pool.total_reward_debt = pool
            .total_reward_debt
            .checked_add(user_stake.reward_debt)
            .ok_or(StakingError::MathOverflow)?;

        let mut stake_data = beneficiary_stake_info.try_borrow_mut_data()?;
        user_stake.serialize(&mut &mut stake_data[..])?;

        // Update pool sum_stake_exp
        // sum_stake_exp += amount * exp_start_factor
        let stake_contribution = wad_mul(
            (amount as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?,
            exp_start_factor,
        )?;
        let new_sum = pool
            .get_sum_stake_exp()
            .checked_add(U256::from_u128(stake_contribution))
            .ok_or(StakingError::MathOverflow)?;
        pool.set_sum_stake_exp(new_sum);
    } else {
        // Realloc legacy accounts to current size (staker pays)
        UserStake::maybe_realloc(beneficiary_stake_info, staker_info, Some(system_program_info))?;

        // Load existing stake
        if beneficiary_stake_info.owner != program_id {
            return Err(StakingError::InvalidAccountOwner.into());
        }
        let mut user_stake = UserStake::try_from_slice(&beneficiary_stake_info.try_borrow_data()?)?;
        if !user_stake.is_initialized() {
            return Err(StakingError::NotInitialized.into());
        }

        // Verify ownership — must belong to the beneficiary
        if user_stake.owner != *beneficiary_info.key {
            return Err(StakingError::InvalidOwner.into());
        }
        if user_stake.pool != *pool_info.key {
            return Err(StakingError::InvalidPool.into());
        }

        // Block staking while unstake request is pending
        if user_stake.has_pending_unstake_request() {
            return Err(StakingError::PendingUnstakeRequestExists.into());
        }

        // A legacy `amount == 0` account stores unpaid residual rewards in
        // reward_debt (paid by ClaimRewards against pool.total_residual_unpaid,
        // never part of pool.total_reward_debt). Adding stake on top would
        // reinterpret that residual as snapshot debt: the residual claim is
        // lost, future rewards are suppressed, total_reward_debt is debited
        // for a value it never contained and the reservation in
        // total_residual_unpaid is stranded. The beneficiary does not sign
        // StakeOnBehalf, so without this guard anyone could inflict that on a
        // victim with a dust stake. Require the residual to be claimed first.
        // Same threshold as ClaimRewards / CloseStakeAccount / CancelUnstake:
        // sub-lamport dust (< 1 WAD) is not payable and does not block.
        if user_stake.amount == 0 && user_stake.reward_debt / WAD > 0 {
            return Err(StakingError::ResidualRewardsUnclaimed.into());
        }

        // Check minimum stake amount on new total
        let new_total = user_stake
            .amount
            .checked_add(amount)
            .ok_or(StakingError::MathOverflow)?;
        if pool.min_stake_amount > 0 && new_total < pool.min_stake_amount {
            return Err(StakingError::BelowMinimumStake.into());
        }

        // Lazily adjust exp_start_factor if pool has been rebased
        user_stake.sync_to_pool(&pool)?;

        // Maturity percentage is preserved — it depends only on when the
        // beneficiary first staked, not on amount. exp_start_factor and
        // claimed_rewards_wad are NOT changed.
        let old_reward_debt = user_stake.reward_debt;

        // sum_stake_exp: new tokens use the SAME exp_start_factor (same maturity)
        let new_contribution = wad_mul(
            (amount as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?,
            user_stake.exp_start_factor,
        )?;
        let new_sum = pool
            .get_sum_stake_exp()
            .checked_add(U256::from_u128(new_contribution))
            .ok_or(StakingError::MathOverflow)?;
        pool.set_sum_stake_exp(new_sum);

        // reward_debt += fresh snapshot for new tokens only
        let new_token_debt = wad_mul(
            (amount as u128).checked_mul(WAD).ok_or(StakingError::MathOverflow)?,
            pool.acc_reward_per_weighted_share,
        )?;
        user_stake.reward_debt = user_stake.reward_debt
            .checked_add(new_token_debt)
            .ok_or(StakingError::MathOverflow)?;

        user_stake.amount = new_total;
        // Only refresh the lock clock when the signing staker IS the position
        // owner. The beneficiary does not sign StakeOnBehalf, so a non-owner
        // must not be able to extend the owner's lock: otherwise anyone could
        // add 1 token to a victim's position to reset last_stake_time forever,
        // permanently blocking unstake in pools with lock_duration_seconds > 0.
        if staker_info.key == beneficiary_info.key {
            user_stake.last_stake_time = current_time;
        }
        // exp_start_factor: UNCHANGED — maturity depends only on start time
        // claimed_rewards_wad: UNCHANGED — pending rewards stay exactly the same

        // Update pool-level aggregate
        pool.total_reward_debt = pool
            .total_reward_debt
            .saturating_sub(old_reward_debt)
            .checked_add(user_stake.reward_debt)
            .ok_or(StakingError::MathOverflow)?;

        let mut stake_data = beneficiary_stake_info.try_borrow_mut_data()?;
        user_stake.serialize(&mut &mut stake_data[..])?;
    }

    // Update pool total staked
    pool.total_staked = pool
        .total_staked
        .checked_add(amount as u128)
        .ok_or(StakingError::MathOverflow)?;

    // Save pool state
    let mut pool_data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut pool_data[..])?;

    // Transfer tokens from staker to vault (staker signs the transfer)
    let mint_data = mint_info.try_borrow_data()?;
    let mint = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&mint_data)?;
    let decimals = mint.base.decimals;
    drop(mint_data);

    invoke(
        &spl_token_2022::instruction::transfer_checked(
            token_program_info.key,
            staker_token_info.key,
            mint_info.key,
            token_vault_info.key,
            staker_info.key,  // staker is the authority for the token transfer
            &[],
            amount,
            decimals,
        )?,
        &[
            staker_token_info.clone(),
            mint_info.clone(),
            token_vault_info.clone(),
            staker_info.clone(),
        ],
    )?;

    // Increment member_count on a new stake (metadata PDA is a required account).
    if is_new_stake {
        update_pool_member_count(program_id, pool_info, metadata_info, 1)?;
    }

    msg!("Staked {} tokens on behalf of beneficiary", amount);

    Ok(())
}
