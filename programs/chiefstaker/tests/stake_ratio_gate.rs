//! Regression tests for C-2: the stake-time ratio gate.
//!
//! Time-weighting precision collapses as (now - base_time) / tau approaches
//! MAX_EXP_INPUT (42): a brand-new stake made at ratio 40.5 is instantly ~22%
//! mature, at >= 41.5 fully mature. Stake / StakeOnBehalf therefore require a
//! SyncPool rebase once the ratio exceeds MAX_STAKE_EXP_INPUT (20).
//!
//! These tests check that:
//!   - both handlers reject with PoolRequiresSync just above ratio 20,
//!   - the permissionless SyncPool crank clears the condition (staking is not
//!     bricked) and the resulting stake starts with zero weight,
//!   - staking just below ratio 20 still works without a sync.

use borsh::BorshDeserialize;
use chiefstaker::{
    error::StakingError,
    math::{calculate_user_weighted_stake, WAD},
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

/// Large enough that the gap between the wall clock and the test bank's clock
/// is negligible against one unit of ratio.
const TAU: u64 = 100_000;

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
    user: Keypair,
    user_token: Pubkey,
    user_stake_pda: Pubkey,
    /// A second wallet with no stake account, used as StakeOnBehalf beneficiary.
    beneficiary: Pubkey,
    beneficiary_stake_pda: Pubkey,
}

/// An empty pool whose base_time lies `ratio_x100 / 100` taus in the past.
fn build(ratio_x100: u64) -> (ProgramTest, Setup) {
    let program_id = chiefstaker::id();
    let mint = Pubkey::new_unique();
    let (pool_pda, pool_bump) =
        Pubkey::find_program_address(&[POOL_SEED, mint.as_ref()], &program_id);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[TOKEN_VAULT_SEED, pool_pda.as_ref()], &program_id);
    let (metadata_pda, _) =
        Pubkey::find_program_address(&[METADATA_SEED, pool_pda.as_ref()], &program_id);

    let user = Keypair::new();
    let user_token = Pubkey::new_unique();
    let (user_stake_pda, _) = Pubkey::find_program_address(
        &[STAKE_SEED, pool_pda.as_ref(), user.pubkey().as_ref()],
        &program_id,
    );
    let beneficiary = Pubkey::new_unique();
    let (beneficiary_stake_pda, _) = Pubkey::find_program_address(
        &[STAKE_SEED, pool_pda.as_ref(), beneficiary.as_ref()],
        &program_id,
    );

    let base_time = now_unix() - (TAU * ratio_x100 / 100) as i64;
    let pool = StakingPool::new(
        mint,
        vault_pda,
        Pubkey::default(),
        Pubkey::new_unique(),
        TAU,
        base_time,
        pool_bump,
    );

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
            lamports: rent_for(StakingPool::LEN),
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
        user_token,
        Account {
            lamports: rent_for(spl_token_2022::state::Account::LEN),
            data: pack_token_account(mint, user.pubkey(), 2_000_000),
            owner: spl_token_2022::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(
        user.pubkey(),
        Account {
            lamports: 10_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    (
        pt,
        Setup {
            program_id,
            pool_pda,
            vault_pda,
            metadata_pda,
            mint,
            user,
            user_token,
            user_stake_pda,
            beneficiary,
            beneficiary_stake_pda,
        },
    )
}

fn stake_ix(s: &Setup, amount: u64) -> Instruction {
    Instruction {
        program_id: s.program_id,
        accounts: vec![
            AccountMeta::new(s.pool_pda, false),
            AccountMeta::new(s.user_stake_pda, false),
            AccountMeta::new(s.vault_pda, false),
            AccountMeta::new(s.user_token, false),
            AccountMeta::new_readonly(s.mint, false),
            AccountMeta::new(s.user.pubkey(), true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
            AccountMeta::new(s.metadata_pda, false),
        ],
        data: borsh::to_vec(&StakingInstruction::Stake { amount }).unwrap(),
    }
}

fn stake_on_behalf_ix(s: &Setup, amount: u64) -> Instruction {
    Instruction {
        program_id: s.program_id,
        accounts: vec![
            AccountMeta::new(s.pool_pda, false),
            AccountMeta::new(s.beneficiary_stake_pda, false),
            AccountMeta::new(s.vault_pda, false),
            AccountMeta::new(s.user_token, false),
            AccountMeta::new_readonly(s.mint, false),
            AccountMeta::new(s.user.pubkey(), true),
            AccountMeta::new(s.beneficiary, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
            AccountMeta::new(s.metadata_pda, false),
        ],
        data: borsh::to_vec(&StakingInstruction::StakeOnBehalf { amount }).unwrap(),
    }
}

fn sync_pool_ix(s: &Setup) -> Instruction {
    Instruction {
        program_id: s.program_id,
        accounts: vec![AccountMeta::new(s.pool_pda, false)],
        data: borsh::to_vec(&StakingInstruction::SyncPool).unwrap(),
    }
}

async fn send(
    banks: &mut BanksClient,
    payer: &Keypair,
    signers: &[&Keypair],
    ix: Instruction,
) -> Result<(), BanksClientError> {
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let mut all: Vec<&Keypair> = vec![payer];
    all.extend_from_slice(signers);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &all, blockhash);
    banks.process_transaction(tx).await
}

fn assert_requires_sync(result: Result<(), BanksClientError>) {
    let err = result.expect_err("staking above the ratio gate must be rejected");
    let BanksClientError::TransactionError(TransactionError::InstructionError(_, ie)) = err else {
        panic!("expected InstructionError, got {:?}", err)
    };
    assert_eq!(
        ie,
        InstructionError::Custom(StakingError::PoolRequiresSync as u32),
        "expected PoolRequiresSync"
    );
}

/// Just above ratio 20 both stake handlers demand a sync; SyncPool (no signer
/// beyond the fee payer) clears it, and the stake made afterwards starts with
/// zero weight.
#[tokio::test]
async fn stake_above_ratio_20_requires_sync_and_sync_pool_clears_it() {
    let (pt, s) = build(2_100); // ratio 21 — far below the old gate of 42
    let (mut banks, payer, _) = pt.start().await;

    assert_requires_sync(send(&mut banks, &payer, &[&s.user], stake_ix(&s, 1_000)).await);
    assert_requires_sync(send(&mut banks, &payer, &[&s.user], stake_on_behalf_ix(&s, 1_000)).await);
    assert!(
        banks.get_account(s.user_stake_pda).await.unwrap().is_none(),
        "no stake account may be created by a rejected stake"
    );

    send(&mut banks, &payer, &[], sync_pool_ix(&s))
        .await
        .expect("permissionless SyncPool must succeed at ratio 21");

    // Different amounts from the rejected attempts so signatures differ.
    send(&mut banks, &payer, &[&s.user], stake_ix(&s, 2_000))
        .await
        .expect("Stake must succeed after SyncPool");
    send(&mut banks, &payer, &[&s.user], stake_on_behalf_ix(&s, 3_000))
        .await
        .expect("StakeOnBehalf must succeed after SyncPool");

    let pool_acc = banks.get_account(s.pool_pda).await.unwrap().unwrap();
    let pool = StakingPool::try_from_slice(&pool_acc.data).unwrap();
    assert_eq!(pool.total_staked, 5_000);

    let stake_acc = banks.get_account(s.user_stake_pda).await.unwrap().unwrap();
    let us = UserStake::try_from_slice(&stake_acc.data).unwrap();
    assert_eq!(us.amount, 2_000);
    assert_eq!(us.base_time_snapshot, pool.base_time);

    // Weight at the moment of staking is (essentially) zero.
    let w = calculate_user_weighted_stake(
        us.amount,
        us.exp_start_factor,
        us.stake_time,
        pool.base_time,
        pool.tau_seconds,
    )
    .unwrap();
    assert!(
        w <= us.amount as u128 * WAD / 1_000_000_000,
        "fresh stake must start with ~zero weight, got {}",
        w
    );
}

/// Just below ratio 20, staking works without a sync and the new stake's
/// instant weight is negligible (the precision the gate is there to protect).
#[tokio::test]
async fn stake_below_ratio_20_needs_no_sync() {
    let (pt, s) = build(1_900); // ratio 19
    let (mut banks, payer, _) = pt.start().await;

    send(&mut banks, &payer, &[&s.user], stake_ix(&s, 1_000_000))
        .await
        .expect("Stake below the gate must succeed without SyncPool");
    send(&mut banks, &payer, &[&s.user], stake_on_behalf_ix(&s, 1))
        .await
        .expect("StakeOnBehalf below the gate must succeed without SyncPool");

    let pool_acc = banks.get_account(s.pool_pda).await.unwrap().unwrap();
    let pool = StakingPool::try_from_slice(&pool_acc.data).unwrap();
    let stake_acc = banks.get_account(s.user_stake_pda).await.unwrap().unwrap();
    let us = UserStake::try_from_slice(&stake_acc.data).unwrap();
    assert_eq!(us.amount, 1_000_000);

    let w = calculate_user_weighted_stake(
        us.amount,
        us.exp_start_factor,
        us.stake_time,
        pool.base_time,
        pool.tau_seconds,
    )
    .unwrap();
    assert!(
        w <= us.amount as u128 * WAD / 100_000_000,
        "instant weight must be < 1e-8 of the stake at ratio 19, got {}",
        w
    );
}
