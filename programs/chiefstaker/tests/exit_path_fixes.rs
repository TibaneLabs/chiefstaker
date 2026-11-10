//! Regression tests for the exit-path audit findings.
//!
//! EX-1: Unstake / RequestUnstake settled against a stale reward accumulator —
//!       lamports sent directly to the pool PDA since the last sync were not
//!       credited to the exiting user. The exit paths now fold them first.
//! EX-2: the close routines left a zeroed, program-owned 178-byte account alive
//!       for the rest of the transaction; re-funded in the same transaction it
//!       bricked the owner's stake PDA. Closing now shrinks the account to zero
//!       length and reassigns it to the system program.
//! EX-6: Unstake accepted the vault itself as the destination token account.

use borsh::BorshDeserialize;
use chiefstaker::{
    math::{U256, WAD},
    state::{StakingPool, UserStake, METADATA_SEED, POOL_SEED, STAKE_SEED, TOKEN_VAULT_SEED},
    StakingInstruction,
};
use solana_program::{
    instruction::{AccountMeta, Instruction, InstructionError},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction, system_program,
};
use solana_program_test::*;
use solana_sdk::{
    account::Account,
    rent::Rent,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};

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

const USER_START_LAMPORTS: u64 = 10_000_000_000;

struct Env {
    program_id: Pubkey,
    mint: Pubkey,
    pool: Pubkey,
    vault: Pubkey,
    metadata: Pubkey,
    user: Keypair,
    user_token: Pubkey,
    user_stake: Pubkey,
}

impl Env {
    fn unstake_ix(&self, amount: u64, destination: Pubkey) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new(self.user_stake, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.user.pubkey(), true),
                AccountMeta::new_readonly(spl_token_2022::id(), false),
                AccountMeta::new_readonly(system_program::id(), false),
                AccountMeta::new(self.metadata, false),
            ],
            data: borsh::to_vec(&StakingInstruction::Unstake { amount }).unwrap(),
        }
    }

    fn request_unstake_ix(&self, amount: u64) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new(self.user_stake, false),
                AccountMeta::new(self.user.pubkey(), true),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
            data: borsh::to_vec(&StakingInstruction::RequestUnstake { amount }).unwrap(),
        }
    }

    fn stake_ix(&self, amount: u64) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.pool, false),
                AccountMeta::new(self.user_stake, false),
                AccountMeta::new(self.vault, false),
                AccountMeta::new(self.user_token, false),
                AccountMeta::new_readonly(self.mint, false),
                AccountMeta::new(self.user.pubkey(), true),
                AccountMeta::new_readonly(system_program::id(), false),
                AccountMeta::new_readonly(spl_token_2022::id(), false),
                AccountMeta::new(self.metadata, false),
            ],
            data: borsh::to_vec(&StakingInstruction::Stake { amount }).unwrap(),
        }
    }

    fn close_ix(&self) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new_readonly(self.pool, false),
                AccountMeta::new(self.user_stake, false),
                AccountMeta::new(self.user.pubkey(), true),
                AccountMeta::new(self.metadata, false),
            ],
            data: borsh::to_vec(&StakingInstruction::CloseStakeAccount).unwrap(),
        }
    }
}

/// Build a pool with one on-chain staker (`user`, staking `user_amount` at
/// base_time, i.e. exp_start_factor = WAD) plus `other_staked` tokens belonging
/// to other stakers that only exist in the pool totals. `donation` lamports sit
/// on the pool PDA on top of its rent minimum and are NOT synced.
async fn setup(
    tau_seconds: u64,
    base_time: i64,
    cooldown: u64,
    user_amount: u64,
    other_staked: u64,
    donation: u64,
) -> (BanksClient, Keypair, Env) {
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
    let (stake_pda, stake_bump) = Pubkey::find_program_address(
        &[STAKE_SEED, pool_pda.as_ref(), user.pubkey().as_ref()],
        &program_id,
    );

    let total = user_amount + other_staked;
    let mut pool = StakingPool::new(
        mint,
        vault_pda,
        Pubkey::default(),
        Pubkey::new_unique(),
        tau_seconds,
        base_time,
        pool_bump,
    );
    pool.total_staked = total as u128;
    pool.set_sum_stake_exp(U256::from_u128(total as u128 * WAD));
    pool.unstake_cooldown_seconds = cooldown;
    pool.last_synced_lamports = 0; // donation not synced

    let stake = UserStake::new(
        user.pubkey(),
        pool_pda,
        user_amount,
        base_time,
        WAD,
        stake_bump,
        base_time,
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
    let mut add = |key: Pubkey, lamports: u64, data: Vec<u8>, owner: Pubkey| {
        pt.add_account(
            key,
            Account {
                lamports,
                data,
                owner,
                executable: false,
                rent_epoch: 0,
            },
        );
    };
    add(
        pool_pda,
        rent_for(StakingPool::LEN) + donation,
        borsh::to_vec(&pool).unwrap(),
        program_id,
    );
    add(
        stake_pda,
        rent_for(UserStake::LEN),
        borsh::to_vec(&stake).unwrap(),
        program_id,
    );
    add(
        mint,
        rent_for(spl_token_2022::state::Mint::LEN),
        pack_mint(9),
        spl_token_2022::id(),
    );
    add(
        vault_pda,
        rent_for(spl_token_2022::state::Account::LEN),
        pack_token_account(mint, pool_pda, total),
        spl_token_2022::id(),
    );
    add(
        user_token,
        rent_for(spl_token_2022::state::Account::LEN),
        pack_token_account(mint, user.pubkey(), 0),
        spl_token_2022::id(),
    );
    add(user.pubkey(), USER_START_LAMPORTS, vec![], system_program::id());

    let (banks, payer, _) = pt.start().await;
    let env = Env {
        program_id,
        mint,
        pool: pool_pda,
        vault: vault_pda,
        metadata: metadata_pda,
        user,
        user_token,
        user_stake: stake_pda,
    };
    (banks, payer, env)
}

async fn send(
    banks: &mut BanksClient,
    payer: &Keypair,
    ixs: &[Instruction],
    extra_signer: &Keypair,
) -> Result<(), BanksClientError> {
    let blockhash = banks.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        ixs,
        Some(&payer.pubkey()),
        &[payer, extra_signer],
        blockhash,
    );
    banks.process_transaction(tx).await
}

async fn lamports(banks: &mut BanksClient, key: Pubkey) -> u64 {
    banks
        .get_account(key)
        .await
        .unwrap()
        .map(|a| a.lamports)
        .unwrap_or(0)
}

// ── EX-1 ────────────────────────────────────────────────────────────────────

/// Direct lamport donation to the pool PDA, then a full Unstake WITHOUT a
/// SyncRewards crank in between: the exiting (fully mature) staker must still
/// be paid their pro-rata share of the donation.
#[tokio::test]
async fn ex1_unstake_credits_unsynced_rewards() {
    let user_amount = 1_000u64;
    let other = 3_000u64;
    let donation = 1_000_000u64;
    // tau = 60s, base_time = 0: the position is fully mature at wall-clock time.
    let (mut banks, payer, env) = setup(60, 0, 0, user_amount, other, donation).await;

    send(
        &mut banks,
        &payer,
        &[env.unstake_ix(user_amount, env.user_token)],
        &env.user,
    )
    .await
    .expect("unstake should succeed");

    let expected_share = donation * user_amount / (user_amount + other); // 250_000
    let user_after = lamports(&mut banks, env.user.pubkey()).await;
    let reward = user_after - USER_START_LAMPORTS - rent_for(UserStake::LEN);
    assert!(
        reward <= expected_share && reward >= expected_share - 10,
        "exiting staker must receive their share of the unsynced donation \
         (got {reward}, expected ~{expected_share}; pre-fix: 0)",
    );

    let pool_acc = banks.get_account(env.pool).await.unwrap().unwrap();
    let pool = StakingPool::try_from_slice(&pool_acc.data).unwrap();
    assert_eq!(
        pool.acc_reward_per_weighted_share,
        (donation as u128) * WAD / ((user_amount + other) as u128),
        "donation must be folded over the pre-unstake total_staked",
    );
    assert_eq!(pool.total_staked, other as u128);

    // Invariant: last_synced_lamports never exceeds what is actually on the
    // pool, and (bar a few lamports of forfeited-immature rounding) tracks it.
    let available = pool_acc.lamports - rent_for(StakingPool::LEN);
    assert_eq!(available, donation - reward);
    assert!(pool.last_synced_lamports <= available);
    assert!(pool.last_synced_lamports >= available - 10);

    // Tokens delivered, stake account closed.
    let token_acc = banks.get_account(env.user_token).await.unwrap().unwrap();
    let token = spl_token_2022::state::Account::unpack(&token_acc.data).unwrap();
    assert_eq!(token.amount, user_amount);
    assert!(banks.get_account(env.user_stake).await.unwrap().is_none());
}

/// Same as above through the cooldown flow: RequestUnstake settles rewards at
/// request time and must fold the unsynced donation first.
#[tokio::test]
async fn ex1_request_unstake_credits_unsynced_rewards() {
    let user_amount = 1_000u64;
    let other = 3_000u64;
    let donation = 1_000_000u64;
    let (mut banks, payer, env) = setup(60, 0, 3_600, user_amount, other, donation).await;

    send(
        &mut banks,
        &payer,
        &[env.request_unstake_ix(user_amount)],
        &env.user,
    )
    .await
    .expect("request unstake should succeed");

    let expected_share = donation * user_amount / (user_amount + other);
    let user_after = lamports(&mut banks, env.user.pubkey()).await;
    let reward = user_after - USER_START_LAMPORTS;
    assert!(
        reward <= expected_share && reward >= expected_share - 10,
        "requesting staker must receive their share of the unsynced donation \
         (got {reward}, expected ~{expected_share}; pre-fix: 0)",
    );

    let pool_acc = banks.get_account(env.pool).await.unwrap().unwrap();
    let pool = StakingPool::try_from_slice(&pool_acc.data).unwrap();
    assert_eq!(
        pool.acc_reward_per_weighted_share,
        (donation as u128) * WAD / ((user_amount + other) as u128),
    );
    let available = pool_acc.lamports - rent_for(StakingPool::LEN);
    assert_eq!(available, donation - reward);
    assert!(pool.last_synced_lamports <= available);
    assert!(pool.last_synced_lamports >= available - 10);

    let stake_acc = banks.get_account(env.user_stake).await.unwrap().unwrap();
    let stake = UserStake::try_from_slice(&stake_acc.data).unwrap();
    assert_eq!(stake.amount, 0);
    assert_eq!(stake.unstake_request_amount, user_amount);
}

// ── EX-2 ────────────────────────────────────────────────────────────────────

/// Full Unstake (closes the stake account) followed, in the SAME transaction,
/// by a re-fund of the stake PDA. The PDA must end up as a plain system-owned,
/// zero-length account — and the owner must be able to stake again.
#[tokio::test]
async fn ex2_unstake_close_then_refund_leaves_system_account() {
    let user_amount = 1_000u64;
    // Young pool (base_time just in the past, long tau) so the re-Stake below
    // passes the time-ratio check.
    let (mut banks, payer, env) =
        setup(100_000, now_unix() - 60, 0, user_amount, 0, 0).await;

    let refund = rent_for(UserStake::LEN);
    send(
        &mut banks,
        &payer,
        &[
            env.unstake_ix(user_amount, env.user_token),
            system_instruction::transfer(&payer.pubkey(), &env.user_stake, refund),
        ],
        &env.user,
    )
    .await
    .expect("unstake + refund should succeed");

    let acc = banks.get_account(env.user_stake).await.unwrap().unwrap();
    assert_eq!(
        acc.owner,
        system_program::id(),
        "closed stake account must be handed back to the system program",
    );
    assert!(acc.data.is_empty(), "closed stake account must have no data");
    assert_eq!(acc.lamports, refund);

    // The PDA is not bricked: the owner can stake again (pre-funded create path).
    let restake = 500u64;
    send(&mut banks, &payer, &[env.stake_ix(restake)], &env.user)
        .await
        .expect("re-stake into the re-funded PDA should succeed");

    let acc = banks.get_account(env.user_stake).await.unwrap().unwrap();
    assert_eq!(acc.owner, env.program_id);
    assert_eq!(acc.data.len(), UserStake::LEN);
    let stake = UserStake::try_from_slice(&acc.data).unwrap();
    assert!(stake.is_initialized());
    assert_eq!(stake.owner, env.user.pubkey());
    assert_eq!(stake.amount, restake);
}

/// Same for the explicit CloseStakeAccount instruction on an empty position.
#[tokio::test]
async fn ex2_close_stake_account_then_refund_leaves_system_account() {
    let (mut banks, payer, env) = setup(100_000, now_unix() - 60, 0, 0, 0, 0).await;

    let refund = rent_for(UserStake::LEN);
    send(
        &mut banks,
        &payer,
        &[
            env.close_ix(),
            system_instruction::transfer(&payer.pubkey(), &env.user_stake, refund),
        ],
        &env.user,
    )
    .await
    .expect("close + refund should succeed");

    let acc = banks.get_account(env.user_stake).await.unwrap().unwrap();
    assert_eq!(acc.owner, system_program::id());
    assert!(acc.data.is_empty());
    assert_eq!(acc.lamports, refund);
    assert_eq!(
        lamports(&mut banks, env.user.pubkey()).await,
        USER_START_LAMPORTS + rent_for(UserStake::LEN),
        "rent must still be returned to the owner",
    );
}

// ── EX-6 ────────────────────────────────────────────────────────────────────

/// Unstake with the vault itself as the destination token account is rejected
/// and leaves the position untouched.
#[tokio::test]
async fn ex6_unstake_rejects_vault_as_destination() {
    let user_amount = 1_000u64;
    let (mut banks, payer, env) = setup(60, 0, 0, user_amount, 0, 0).await;

    let err = send(
        &mut banks,
        &payer,
        &[env.unstake_ix(user_amount, env.vault)],
        &env.user,
    )
    .await
    .expect_err("unstake to the vault must fail")
    .unwrap();
    assert_eq!(
        err,
        TransactionError::InstructionError(0, InstructionError::InvalidArgument),
    );

    let acc = banks.get_account(env.user_stake).await.unwrap().unwrap();
    let stake = UserStake::try_from_slice(&acc.data).unwrap();
    assert_eq!(stake.amount, user_amount);
}
