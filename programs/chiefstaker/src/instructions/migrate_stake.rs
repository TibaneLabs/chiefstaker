//! Migrate a stake position to a new owner (clean 1:1 transfer).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};

use crate::{
    error::StakingError,
    state::{StakingPool, UserStake, STAKE_SEED},
};

/// Migrate a stake position to a new owner without unstaking.
///
/// Creates a new `UserStake` at the destination PDA, copies every field of the
/// source position (only `owner` and `bump` are rewritten), and closes the
/// source. Pool aggregates (`total_staked`, `sum_stake_exp`, `total_reward_debt`)
/// are unchanged; `member_count` is net unchanged so the metadata PDA is not
/// required.
///
/// Reject conditions: source `amount == 0`, pending unstake request on source,
/// self-migrate (`new_owner == source_owner`), destination already initialized,
/// wrong destination PDA, pool needs rebase.
///
/// Accounts:
/// 0. `[]` Pool account
/// 1. `[writable]` Source user stake (PDA: ["stake", pool, source_owner]) — closed
/// 2. `[writable, signer]` Source owner (pays dest rent, receives source rent)
/// 3. `[writable]` Destination user stake (PDA: ["stake", pool, new_owner]) — must be empty
/// 4. `[]` New owner (pubkey only; no signer required)
/// 5. `[]` System program
pub fn process_migrate_stake(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();

    let pool_info = next_account_info(account_info_iter)?;
    let source_stake_info = next_account_info(account_info_iter)?;
    let source_owner_info = next_account_info(account_info_iter)?;
    let dest_stake_info = next_account_info(account_info_iter)?;
    let new_owner_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;

    if !source_owner_info.is_signer {
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
    let (expected_pool, _) = StakingPool::derive_pda(&pool.mint, program_id);
    if *pool_info.key != expected_pool {
        return Err(StakingError::InvalidPDA.into());
    }
    if pool.get_sum_stake_exp().needs_rebase() {
        return Err(StakingError::PoolRequiresSync.into());
    }

    // Load and validate source UserStake
    if source_stake_info.owner != program_id {
        return Err(StakingError::InvalidAccountOwner.into());
    }
    let mut user_stake =
        UserStake::try_from_slice(&source_stake_info.try_borrow_data()?)?;
    if !user_stake.is_initialized() {
        return Err(StakingError::NotInitialized.into());
    }
    if user_stake.owner != *source_owner_info.key {
        return Err(StakingError::InvalidOwner.into());
    }
    if user_stake.pool != *pool_info.key {
        return Err(StakingError::InvalidPool.into());
    }
    let (expected_source, _) =
        UserStake::derive_pda(pool_info.key, source_owner_info.key, program_id);
    if *source_stake_info.key != expected_source {
        return Err(StakingError::InvalidPDA.into());
    }

    if user_stake.amount == 0 {
        return Err(StakingError::InsufficientStakeBalance.into());
    }
    if user_stake.has_pending_unstake_request() {
        return Err(StakingError::PendingUnstakeRequestExists.into());
    }
    if *new_owner_info.key == *source_owner_info.key {
        return Err(StakingError::SelfMigrateNotAllowed.into());
    }

    // Verify destination PDA and that the account has not been created yet.
    let (expected_dest, dest_bump) =
        UserStake::derive_pda(pool_info.key, new_owner_info.key, program_id);
    if *dest_stake_info.key != expected_dest {
        return Err(StakingError::InvalidPDA.into());
    }
    if !dest_stake_info.data_is_empty() {
        return Err(StakingError::AlreadyInitialized.into());
    }

    // Lazily adjust to the current pool epoch so the copied exp_start_factor
    // and base_time_snapshot match what a fresh stake would carry.
    user_stake.sync_to_pool(&pool)?;

    let migrated_amount = user_stake.amount;

    // Create the destination account at full LEN, signed by its own PDA seeds.
    // Pre-fund tolerant: a griefer may have sent stray lamports to the
    // deterministic destination PDA to block plain create_account.
    let rent = Rent::get()?;
    let dest_seeds = &[
        STAKE_SEED,
        pool_info.key.as_ref(),
        new_owner_info.key.as_ref(),
        &[dest_bump],
    ];
    create_pda_account(
        source_owner_info,
        dest_stake_info,
        system_program_info,
        program_id,
        UserStake::LEN,
        &rent,
        dest_seeds,
    )?;

    // Rewrite only the identity fields; everything else (amount, stake_time,
    // last_stake_time, exp_start_factor, reward_debt, claimed_rewards_wad,
    // total_rewards_claimed, base_time_snapshot, request fields) carries over.
    user_stake.owner = *new_owner_info.key;
    user_stake.bump = dest_bump;

    {
        let mut dest_data = dest_stake_info.try_borrow_mut_data()?;
        user_stake.serialize(&mut &mut dest_data[..])?;
    }

    // Close the source: drain lamports to source owner, zero the buffer so it
    // can't be re-read as a valid stake.
    let source_lamports = source_stake_info.lamports();
    **source_stake_info.try_borrow_mut_lamports()? = 0;
    **source_owner_info.try_borrow_mut_lamports()? += source_lamports;
    {
        let mut source_data = source_stake_info.try_borrow_mut_data()?;
        source_data.fill(0);
    }
    // EX-2: fully close the source — shrink it to zero length and hand it back
    // to the system program, so a same-transaction re-fund cannot leave a
    // zeroed program-owned account that bricks the source owner's stake PDA.
    source_stake_info.realloc(0, false)?;
    source_stake_info.assign(&solana_program::system_program::id());

    msg!(
        "Migrated {} tokens from {} to {}",
        migrated_amount,
        source_owner_info.key,
        new_owner_info.key
    );

    Ok(())
}

/// Create a program-derived account at `target`, tolerating the case where the
/// target PDA has already been funded with lamports by a third party.
///
/// `system_instruction::create_account` requires the destination to hold zero
/// lamports, so anyone could permanently block migration to a given recipient
/// by sending 1 lamport to the deterministic destination stake PDA first. To
/// avoid that DoS we, when the account is already funded, top it up to
/// rent-exemption and then `allocate` + `assign` it under its own seeds —
/// operations that require the PDA's signature and therefore cannot be
/// performed by a griefer who only transferred lamports. Lamports are only
/// ever transferred FROM the payer TO the PDA, and only the shortfall to
/// rent-exemption (nothing if the PDA is already at or above it).
///
/// Local mirror of the helper hardening `initialize.rs` against the same
/// vector (see `tests/init_griefing.rs`).
fn create_pda_account<'a>(
    payer: &AccountInfo<'a>,
    target: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    owner: &Pubkey,
    space: usize,
    rent: &Rent,
    seeds: &[&[u8]],
) -> ProgramResult {
    let required = rent.minimum_balance(space);

    if target.lamports() == 0 {
        // Fast path: empty + unfunded, a single create_account does it all.
        return invoke_signed(
            &system_instruction::create_account(
                payer.key,
                target.key,
                required,
                space as u64,
                owner,
            ),
            &[payer.clone(), target.clone(), system_program.clone()],
            &[seeds],
        );
    }

    // Pre-funded path: fund to rent-exemption, then allocate + assign under seeds.
    let current = target.lamports();
    if current < required {
        invoke(
            &system_instruction::transfer(payer.key, target.key, required - current),
            &[payer.clone(), target.clone(), system_program.clone()],
        )?;
    }
    invoke_signed(
        &system_instruction::allocate(target.key, space as u64),
        &[target.clone(), system_program.clone()],
        &[seeds],
    )?;
    invoke_signed(
        &system_instruction::assign(target.key, owner),
        &[target.clone(), system_program.clone()],
        &[seeds],
    )
}
