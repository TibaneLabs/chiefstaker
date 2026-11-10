//! Regression test for audit finding A-2: SetPoolMetadata creation could be
//! blocked permanently by pre-funding the metadata PDA.
//!
//! The metadata PDA (`["metadata", pool]`) is deterministic, so anyone could
//! send it lamports before the first SetPoolMetadata. The previous plain
//! `system_instruction::create_account` fails on a non-zero destination
//! balance, so the pool could never get metadata. SetPoolMetadata now uses the
//! shared pre-fund-tolerant creation helper (fund-to-rent + allocate + assign
//! under the PDA seeds), like InitializePool (tests/init_griefing.rs) and
//! Stake (tests/stake_prefund_griefing.rs).

use borsh::BorshDeserialize;
use chiefstaker::{
    state::{PoolMetadata, StakingPool, METADATA_SEED, POOL_SEED, TOKEN_VAULT_SEED},
    StakingInstruction,
};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    system_program,
};
use solana_program_test::*;
use solana_sdk::{
    account::Account, rent::Rent, signature::Signer, transaction::Transaction,
};
use spl_token_2022::state::Mint;

fn rent_for(len: usize) -> u64 {
    Rent::default().minimum_balance(len)
}

fn borsh_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// A Token-2022 mint carrying a TokenMetadata extension, crafted at the raw
/// TLV level:
///   [0..82]  base Mint  [82..165] padding  [165] AccountType::Mint
///   [166..]  TLV: type u16=19 (TokenMetadata), length u16, borsh value
fn pack_mint_with_token_metadata(mint: Pubkey, name: &str, symbol: &str) -> Vec<u8> {
    const BASE_ACCOUNT_LENGTH: usize = spl_token_2022::state::Account::LEN; // 165
    const TOKEN_METADATA_EXT_TYPE: u16 = 19;

    let mut value = Vec::new();
    value.extend_from_slice(&[0u8; 32]); // update_authority: none
    value.extend_from_slice(mint.as_ref()); // mint
    borsh_str(&mut value, name);
    borsh_str(&mut value, symbol);
    borsh_str(&mut value, "https://example.invalid/meta.json");
    value.extend_from_slice(&0u32.to_le_bytes()); // additional_metadata: empty

    let mut data = vec![0u8; BASE_ACCOUNT_LENGTH + 1];
    Mint {
        mint_authority: COption::None,
        supply: 1_000_000,
        decimals: 6,
        is_initialized: true,
        freeze_authority: COption::None,
    }
    .pack_into_slice(&mut data[..Mint::LEN]);
    data[BASE_ACCOUNT_LENGTH] = 1; // AccountType::Mint
    data.extend_from_slice(&TOKEN_METADATA_EXT_TYPE.to_le_bytes());
    data.extend_from_slice(&(value.len() as u16).to_le_bytes());
    data.extend_from_slice(&value);
    data
}

/// Run SetPoolMetadata against a pool whose metadata PDA was pre-funded with
/// `prefund_lamports` (0 = untouched), and return the resulting metadata
/// account.
async fn set_metadata_with_prefund(prefund_lamports: u64) -> (Account, Pubkey) {
    let program_id = chiefstaker::id();
    let mint = Pubkey::new_unique();
    let (pool_pda, pool_bump) =
        Pubkey::find_program_address(&[POOL_SEED, mint.as_ref()], &program_id);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[TOKEN_VAULT_SEED, pool_pda.as_ref()], &program_id);
    let (metadata_pda, _) =
        Pubkey::find_program_address(&[METADATA_SEED, pool_pda.as_ref()], &program_id);

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
    let mint_data = pack_mint_with_token_metadata(mint, "Chief Token", "CHIEF");
    pt.add_account(
        mint,
        Account {
            lamports: rent_for(mint_data.len()),
            data: mint_data,
            owner: spl_token_2022::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    if prefund_lamports > 0 {
        // GRIEF: pre-fund the deterministic metadata PDA; system-owned, empty.
        pt.add_account(
            metadata_pda,
            Account {
                lamports: prefund_lamports,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
    }

    let (mut banks, payer, blockhash) = pt.start().await;

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new_readonly(pool_pda, false),
            AccountMeta::new(metadata_pda, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: borsh::to_vec(&StakingInstruction::SetPoolMetadata).unwrap(),
    };
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[&payer], blockhash);
    banks
        .process_transaction(tx)
        .await
        .expect("SetPoolMetadata must succeed even when the metadata PDA was pre-funded");

    let acc = banks.get_account(metadata_pda).await.unwrap().unwrap();
    (acc, pool_pda)
}

fn assert_metadata_created(acc: &Account, pool_pda: Pubkey) {
    assert_eq!(acc.owner, chiefstaker::id(), "metadata PDA must be owned by the program");
    assert_eq!(acc.data.len(), PoolMetadata::LEN);
    assert!(acc.lamports >= rent_for(PoolMetadata::LEN), "must be rent-exempt");
    let md = PoolMetadata::try_from_slice(&acc.data).unwrap();
    assert!(md.is_initialized());
    assert_eq!(md.pool, pool_pda);
    assert_eq!(
        std::str::from_utf8(&md.name[..md.name_len as usize]).unwrap(),
        "Chief Token Staking Pool"
    );
    assert_eq!(md.num_tags, 3);
    assert_eq!(&md.tags[2][..md.tag_lengths[2] as usize], b"#chief");
    assert_eq!(md.member_count, 0);
}

/// Baseline: the unfunded fast path still works.
#[tokio::test]
async fn set_metadata_succeeds_on_unfunded_pda() {
    let (acc, pool_pda) = set_metadata_with_prefund(0).await;
    assert_metadata_created(&acc, pool_pda);
    assert_eq!(acc.lamports, rent_for(PoolMetadata::LEN));
}

/// Griefer sent a single lamport: payer tops up to rent-exemption.
#[tokio::test]
async fn set_metadata_succeeds_when_pda_pre_funded_with_one_lamport() {
    let (acc, pool_pda) = set_metadata_with_prefund(1).await;
    assert_metadata_created(&acc, pool_pda);
    assert_eq!(acc.lamports, rent_for(PoolMetadata::LEN));
}

/// Griefer sent a rent-exempt amount (the ~0.00089 SOL case from the audit).
#[tokio::test]
async fn set_metadata_succeeds_when_pda_pre_funded_rent_exempt() {
    let prefund = rent_for(0);
    let (acc, pool_pda) = set_metadata_with_prefund(prefund).await;
    assert_metadata_created(&acc, pool_pda);
    assert_eq!(acc.lamports, rent_for(PoolMetadata::LEN));
}

/// Griefer over-funded the PDA: no top-up needed, creation still succeeds.
#[tokio::test]
async fn set_metadata_succeeds_when_pda_over_funded() {
    let prefund = rent_for(PoolMetadata::LEN) + 12_345;
    let (acc, pool_pda) = set_metadata_with_prefund(prefund).await;
    assert_metadata_created(&acc, pool_pda);
    assert_eq!(acc.lamports, prefund);
}
