//! Regression test for audit finding F-5 (defence in depth): TakeFeeOwnership
//! CPIs into the external, upgradeable pfee program with the pool PDA as
//! signer, and that PDA is also the authority of the staked-token vault. The
//! handler must reject any transaction that passes the pool's token vault in
//! one of the caller-supplied pass-through account slots.

use chiefstaker::{
    error::StakingError,
    state::{StakingPool, PFEE_PROGRAM_ID, POOL_SEED, TOKEN_VAULT_SEED},
    StakingInstruction,
};
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction, InstructionError},
    pubkey::Pubkey,
    system_program,
};
use solana_program_test::*;
use solana_sdk::{
    account::Account,
    rent::Rent,
    signature::Signer,
    transaction::{Transaction, TransactionError},
};

/// Writability of each of the 18 TakeFeeOwnership accounts (pool is writable:
/// it is the payer of the pfee update_fee_shares CPI).
const WRITABLE: [bool; 18] = [
    true, false, false, false, false, true, false, true, false, false, false, false, false, false,
    false, false, true, true,
];

/// Stand-in for the external pfee program: accepts any instruction, so the
/// handler's CPIs succeed when (and only when) the handler lets them through.
fn pfee_stub(_program_id: &Pubkey, _accounts: &[AccountInfo], _data: &[u8]) -> ProgramResult {
    Ok(())
}

/// Build a TakeFeeOwnership transaction with the pool's token vault placed in
/// account slot `vault_slot` (None = no vault anywhere) and return the result.
async fn take_fee_ownership_with_vault_at(
    vault_slot: Option<usize>,
) -> Result<(), BanksClientError> {
    let program_id = chiefstaker::id();
    let mint = Pubkey::new_unique();
    let (pool_pda, pool_bump) =
        Pubkey::find_program_address(&[POOL_SEED, mint.as_ref()], &program_id);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[TOKEN_VAULT_SEED, pool_pda.as_ref()], &program_id);

    let pool = StakingPool::new(
        mint,
        vault_pda,
        Pubkey::default(),
        Pubkey::new_unique(),
        100_000,
        0,
        pool_bump,
    );

    let mut pt = ProgramTest::new(
        "chiefstaker",
        program_id,
        processor!(chiefstaker::process_instruction),
    );
    pt.add_program("pfee_stub", PFEE_PROGRAM_ID, processor!(pfee_stub));
    pt.add_account(
        pool_pda,
        Account {
            lamports: Rent::default().minimum_balance(StakingPool::LEN),
            data: borsh::to_vec(&pool).unwrap(),
            owner: program_id,
            executable: false,
            rent_epoch: 0,
        },
    );

    let (mut banks, payer, blockhash) = pt.start().await;

    let mut keys: Vec<Pubkey> = (0..18).map(|_| Pubkey::new_unique()).collect();
    keys[0] = pool_pda;
    keys[1] = mint;
    keys[2] = PFEE_PROGRAM_ID;
    keys[8] = system_program::id();
    if let Some(slot) = vault_slot {
        keys[slot] = vault_pda;
    }

    let accounts = keys
        .iter()
        .zip(WRITABLE.iter())
        .map(|(k, w)| {
            if *w {
                AccountMeta::new(*k, false)
            } else {
                AccountMeta::new_readonly(*k, false)
            }
        })
        .collect();

    let ix = Instruction {
        program_id,
        accounts,
        data: borsh::to_vec(&StakingInstruction::TakeFeeOwnership).unwrap(),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);
    banks.process_transaction(tx).await
}

fn custom_code(err: BanksClientError) -> Option<u32> {
    match err.unwrap() {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => Some(code),
        _ => None,
    }
}

/// The vault is rejected in every pass-through slot that can hold an arbitrary
/// caller-supplied key (slots 1 and 2 are already pinned to the pool mint and
/// the pfee program id, so the vault can never pass those checks either).
#[tokio::test]
async fn token_vault_rejected_in_every_pass_through_slot() {
    for slot in 3..18 {
        let err = take_fee_ownership_with_vault_at(Some(slot))
            .await
            .expect_err("token vault must not be accepted as a pass-through account");
        assert_eq!(
            custom_code(err),
            Some(StakingError::InvalidTokenVault as u32),
            "slot {slot}: expected InvalidTokenVault"
        );
    }
}

/// Slots 1 (mint) and 2 (pfee program) also reject the vault (by their own
/// pinning checks or the new guard — either way the transaction fails).
#[tokio::test]
async fn token_vault_rejected_in_pinned_slots() {
    for slot in [1usize, 2] {
        take_fee_ownership_with_vault_at(Some(slot))
            .await
            .expect_err("token vault must not be accepted in a pinned slot");
    }
}

/// Without the vault in the account list the guard does not fire: both CPIs
/// reach the (stubbed) pfee program and the instruction succeeds. This also
/// exercises the documented account flags — the pool must be writable in the
/// outer transaction because it is the writable payer of update_fee_shares.
#[tokio::test]
async fn guard_does_not_fire_without_vault() {
    take_fee_ownership_with_vault_at(None)
        .await
        .expect("TakeFeeOwnership must succeed when the vault is not passed");
}
