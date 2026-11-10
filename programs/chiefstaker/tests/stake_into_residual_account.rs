//! Regression tests for F6: staking into a legacy `amount == 0` UserStake that
//! still holds an unpaid residual.
//!
//! For such accounts `reward_debt` is NOT snapshot debt — it is the unpaid
//! WAD-scaled residual reward, paid by ClaimRewards against
//! `pool.total_residual_unpaid`. Before the fix the add-stake branch of Stake /
//! StakeOnBehalf treated it as snapshot debt: the residual claim was lost,
//! future rewards were suppressed, `total_reward_debt` was debited for a value
//! it never contained and the `total_residual_unpaid` reservation was stranded.
//! StakeOnBehalf needs no beneficiary signature, so anyone could do this to a
//! victim with a dust stake.
//!
//! After the fix both handlers reject with `ResidualRewardsUnclaimed` until the
//! residual has been claimed; zero-residual (and sub-lamport dust) accounts
//! behave exactly as before.

use borsh::BorshDeserialize;
use chiefstaker::{
    error::StakingError,
    math::WAD,
    state::{StakingPool, UserStake, METADATA_SEED, POOL_SEED, STAKE_SEED, TOKEN_VAULT_SEED},
    StakingInstruction,
};
use solana_program::{
    instruction::{AccountMeta, Instruction, InstructionError},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    system_program,
};
use solana_program_test::*;
use solana_sdk::{
    account::Account,
    rent::Rent,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};

const TAU: u64 = 100_000;
const POOL_REWARD_LAMPORTS: u64 = 10_000_000;
const EXISTING_TOTAL_REWARD_DEBT: u128 = 123_456_789 * WAD;

fn rent_for(len: usize) -> u64 {
    Rent::default().minimum_balance(len)
}

fn pack_mint(decimals: u8) -> Vec<u8> {
    let mut data = vec![0u8; spl_token_2022::state::Mint::LEN];
    spl_token_2022::state::Mint {
        mint_authority: COption::Some(Pubkey::new_unique()),
        supply: 1_000_000,
        decimals,
        is_initialized: true,
        freeze_authority: COption::None,
    }
    .pack_into_slice(&mut data);
    data
}

fn pack_token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; spl_token_2022::state::Account::LEN];
    spl_token_2022::state::Account {
        mint,
        owner,
        amount,
        delegate: COption::None,
        state: spl_token_2022::state::AccountState::Initialized,
        is_native: COption::None,
        delegated_amount: 0,
        close_authority: COption::None,
    }
    .pack_into_slice(&mut data);
    data
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Setup {
    program_id: Pubkey,
    pool_pda: Pubkey,
    vault_pda: Pubkey,
    metadata_pda: Pubkey,
    mint: Pubkey,
    /// Owner of the legacy `amount == 0` position.
    owner: Keypair,
    owner_token: Pubkey,
    owner_stake_pda: Pubkey,
    /// Unrelated third party (the griefer in the StakeOnBehalf case).
    attacker: Keypair,
    attacker_token: Pubkey,
}

/// Pool with `total_residual_unpaid = reservation` plus a legacy UserStake with
/// `amount == 0` and `reward_debt = residual_wad`.
fn build(reservation: u64, residual_wad: u128) -> (ProgramTest, Setup) {
    let program_id = chiefstaker::id();
    let mint = Pubkey::new_unique();
    let (pool_pda, pool_bump) =
        Pubkey::find_program_address(&[POOL_SEED, mint.as_ref()], &program_id);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[TOKEN_VAULT_SEED, pool_pda.as_ref()], &program_id);
    let (metadata_pda, _) =
        Pubkey::find_program_address(&[METADATA_SEED, pool_pda.as_ref()], &program_id);

    let owner = Keypair::new();
    let owner_token = Pubkey::new_unique();
    let attacker = Keypair::new();
    let attacker_token = Pubkey::new_unique();
    let (owner_stake_pda, stake_bump) = Pubkey::find_program_address(
        &[STAKE_SEED, pool_pda.as_ref(), owner.pubkey().as_ref()],
        &program_id,
    );

    let base_time = now_unix() - 60;
    let mut pool = StakingPool::new(
        mint,
        vault_pda,
        Pubkey::default(),
        Pubkey::new_unique(),
        TAU,
        base_time,
        pool_bump,
    );
    pool.total_residual_unpaid = reservation;
    pool.total_reward_debt = EXISTING_TOTAL_REWARD_DEBT;
    pool.last_synced_lamports = POOL_REWARD_LAMPORTS;

    let mut us = UserStake::new(
        owner.pubkey(),
        pool_pda,
        0,
        base_time,
        WAD,
        stake_bump,
        base_time,
    );
    us.reward_debt = residual_wad;

    let mut pt = ProgramTest::new(
        "chiefstaker",
        program_id,
        processor!(chiefstaker::process_instruction),
    );
    pt.add_program(
        "spl_token_2022",
        spl_token_2022::id(),
        processor!(spl_token_2022::processor::Processor::process),
    );
    pt.add_account(
        pool_pda,
        Account {
            lamports: rent_for(StakingPool::LEN) + POOL_REWARD_LAMPORTS,
            data: borsh::to_vec(&pool).unwrap(),
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(
        mint,
        Account {
            lamports: rent_for(spl_token_2022::state::Mint::LEN),
            data: pack_mint(9),
            owner: spl_token_2022::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(
        vault_pda,
        Account {
            lamports: rent_for(spl_token_2022::state::Account::LEN),
            data: pack_token_account(mint, pool_pda, 0),
            owner: spl_token_2022::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(
        owner_stake_pda,
        Account {
            lamports: rent_for(UserStake::LEN),
            data: borsh::to_vec(&us).unwrap(),
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    );
    for (wallet, token) in [
        (owner.pubkey(), owner_token),
        (attacker.pubkey(), attacker_token),
    ] {
        pt.add_account(
            token,
            Account {
                lamports: rent_for(spl_token_2022::state::Account::LEN),
                data: pack_token_account(mint, wallet, 1_000),
                owner: spl_token_2022::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        pt.add_account(
            wallet,
            Account {
                lamports: 10_000_000_000,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    (
        pt,
        Setup {
            program_id,
            pool_pda,
            vault_pda,
            metadata_pda,
            mint,
            owner,
            owner_token,
            owner_stake_pda,
            attacker,
            attacker_token,
        },
    )
}

fn stake_ix(s: &Setup, amount: u64) -> Instruction {
    Instruction {
        program_id: s.program_id,
        accounts: vec![
            AccountMeta::new(s.pool_pda, false),
            AccountMeta::new(s.owner_stake_pda, false),
            AccountMeta::new(s.vault_pda, false),
            AccountMeta::new(s.owner_token, false),
            AccountMeta::new_readonly(s.mint, false),
            AccountMeta::new(s.owner.pubkey(), true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
            AccountMeta::new(s.metadata_pda, false),
        ],
        data: borsh::to_vec(&StakingInstruction::Stake { amount }).unwrap(),
    }
}

fn attacker_stake_on_behalf_ix(s: &Setup, amount: u64) -> Instruction {
    Instruction {
        program_id: s.program_id,
        accounts: vec![
            AccountMeta::new(s.pool_pda, false),
            AccountMeta::new(s.owner_stake_pda, false),
            AccountMeta::new(s.vault_pda, false),
            AccountMeta::new(s.attacker_token, false),
            AccountMeta::new_readonly(s.mint, false),
            AccountMeta::new(s.attacker.pubkey(), true),
            AccountMeta::new(s.owner.pubkey(), false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
            AccountMeta::new(s.metadata_pda, false),
        ],
        data: borsh::to_vec(&StakingInstruction::StakeOnBehalf { amount }).unwrap(),
    }
}

fn claim_ix(s: &Setup) -> Instruction {
    Instruction {
        program_id: s.program_id,
        accounts: vec![
            AccountMeta::new(s.pool_pda, false),
            AccountMeta::new(s.owner_stake_pda, false),
            AccountMeta::new(s.owner.pubkey(), true),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: borsh::to_vec(&StakingInstruction::ClaimRewards).unwrap(),
    }
}

async fn send(
    banks: &mut BanksClient,
    payer: &Keypair,
    signer: &Keypair,
    ix: Instruction,
) -> Result<(), BanksClientError> {
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, signer],
        blockhash,
    );
    banks.process_transaction(tx).await
}

fn assert_residual_unclaimed(result: Result<(), BanksClientError>) {
    let err = result.expect_err("staking into a residual account must be rejected");
    let BanksClientError::TransactionError(TransactionError::InstructionError(_, ie)) = err else {
        panic!("expected InstructionError, got {:?}", err)
    };
    assert_eq!(
        ie,
        InstructionError::Custom(StakingError::ResidualRewardsUnclaimed as u32),
        "expected ResidualRewardsUnclaimed"
    );
}

async fn load(banks: &mut BanksClient, s: &Setup) -> (StakingPool, UserStake) {
    let pool_acc = banks.get_account(s.pool_pda).await.unwrap().unwrap();
    let stake_acc = banks.get_account(s.owner_stake_pda).await.unwrap().unwrap();
    (
        StakingPool::try_from_slice(&pool_acc.data).unwrap(),
        UserStake::try_from_slice(&stake_acc.data).unwrap(),
    )
}

async fn lamports(banks: &mut BanksClient, key: Pubkey) -> u64 {
    banks.get_account(key).await.unwrap().unwrap().lamports
}

/// The griefing vector: a third party dust-stakes on behalf of a victim whose
/// account holds an unpaid residual. Must be rejected, leaving everything
/// untouched.
#[tokio::test]
async fn stake_on_behalf_into_residual_account_is_rejected() {
    let residual: u64 = 2_000_000;
    let (pt, s) = build(residual, residual as u128 * WAD);
    let (mut banks, payer, _) = pt.start().await;

    let result = send(
        &mut banks,
        &payer,
        &s.attacker,
        attacker_stake_on_behalf_ix(&s, 1),
    )
    .await;
    assert_residual_unclaimed(result);

    let (pool, us) = load(&mut banks, &s).await;
    assert_eq!(us.amount, 0);
    assert_eq!(
        us.reward_debt,
        residual as u128 * WAD,
        "residual must be intact"
    );
    assert_eq!(pool.total_residual_unpaid, residual);
    assert_eq!(pool.total_reward_debt, EXISTING_TOTAL_REWARD_DEBT);
    assert_eq!(pool.total_staked, 0);
}

/// The owner's own Stake is rejected too — and works once the residual has
/// been claimed, with the residual paid in full and clean accounting.
#[tokio::test]
async fn stake_into_residual_account_requires_claim_first() {
    let residual: u64 = 2_000_000;
    let (pt, s) = build(residual, residual as u128 * WAD);
    let (mut banks, payer, _) = pt.start().await;

    // (Different amount from the post-claim stake below so the two
    // transactions don't share a signature.)
    let result = send(&mut banks, &payer, &s.owner, stake_ix(&s, 499)).await;
    assert_residual_unclaimed(result);

    let owner_before = lamports(&mut banks, s.owner.pubkey()).await;
    send(&mut banks, &payer, &s.owner, claim_ix(&s))
        .await
        .expect("residual claim should succeed");
    let owner_after = lamports(&mut banks, s.owner.pubkey()).await;
    assert_eq!(
        owner_after - owner_before,
        residual,
        "residual paid in full"
    );

    send(&mut banks, &payer, &s.owner, stake_ix(&s, 500))
        .await
        .expect("stake must succeed once the residual is claimed");

    let (pool, us) = load(&mut banks, &s).await;
    assert_eq!(us.amount, 500);
    assert_eq!(pool.total_staked, 500);
    assert_eq!(pool.total_residual_unpaid, 0);
    // acc_reward_per_weighted_share is 0, so the fresh snapshot debt is 0 and
    // the pool aggregate is exactly what it was.
    assert_eq!(us.reward_debt, 0);
    assert_eq!(pool.total_reward_debt, EXISTING_TOTAL_REWARD_DEBT);
}

/// An existing `amount == 0` account with NO residual is not affected by the
/// guard: Stake and StakeOnBehalf add to it exactly as before.
#[tokio::test]
async fn stake_into_zero_residual_account_still_works() {
    let (pt, s) = build(0, 0);
    let (mut banks, payer, _) = pt.start().await;

    send(&mut banks, &payer, &s.owner, stake_ix(&s, 500))
        .await
        .expect("Stake into an empty existing account must succeed");
    send(
        &mut banks,
        &payer,
        &s.attacker,
        attacker_stake_on_behalf_ix(&s, 7),
    )
    .await
    .expect("StakeOnBehalf into the same account must succeed");

    let (pool, us) = load(&mut banks, &s).await;
    assert_eq!(us.amount, 507);
    assert_eq!(us.reward_debt, 0);
    assert_eq!(pool.total_staked, 507);
    assert_eq!(pool.total_reward_debt, EXISTING_TOTAL_REWARD_DEBT);
}

/// Sub-lamport dust (< 1 WAD) is not a payable residual — ClaimRewards cannot
/// clear it and CloseStakeAccount / CancelUnstake already forgive it — so it
/// must not lock the account out of staking.
#[tokio::test]
async fn sub_lamport_residual_dust_does_not_block_staking() {
    let dust = WAD - 1;
    let (pt, s) = build(0, dust);
    let (mut banks, payer, _) = pt.start().await;

    send(&mut banks, &payer, &s.owner, stake_ix(&s, 500))
        .await
        .expect("sub-lamport dust must not block staking");

    let (pool, us) = load(&mut banks, &s).await;
    assert_eq!(us.amount, 500);
    assert_eq!(us.reward_debt, dust);
    assert_eq!(pool.total_staked, 500);
    assert_eq!(pool.total_reward_debt, EXISTING_TOTAL_REWARD_DEBT);
}
