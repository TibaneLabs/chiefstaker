//! Regression tests for the Token-2022 mint-extension screen in
//! `process_initialize_pool`.
//!
//! The screen originally rejected only TransferFeeConfig, PermanentDelegate
//! and TransferHook. That left several extensions through that brick or
//! compromise a pool:
//!   - NonTransferable: stake/unstake transfers are impossible; bricked pool.
//!   - DefaultAccountState (Frozen): the vault token account is frozen on
//!     creation; pool permanently unusable.
//!   - ConfidentialTransferMint: undermines the
//!     vault-balance == total_staked invariant.
//!   - Pausable (unknown to the pinned spl-token-2022 5.0 crate): the mint
//!     creator can pause all transfers after users stake, locking principal.
//!     Caught by the unknown-extension screen (`get_extension_types()` fails
//!     on discriminants this crate version does not know).
//!
//! These tests assert pool init is rejected for such mints with
//! `StakingError::UnsupportedMintExtension`, and still succeeds for a plain
//! mint and for a pump.fun-style mint (MetadataPointer, the benign extension
//! pump.fun mints carry alongside TokenMetadata).

use borsh::BorshDeserialize;
use chiefstaker::{
    error::StakingError,
    state::{StakingPool, POOL_SEED, TOKEN_VAULT_SEED},
    StakingInstruction,
};
use solana_program::{
    instruction::{AccountMeta, Instruction, InstructionError},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    system_program, sysvar,
};
use solana_program_test::*;
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use spl_token_2022::{
    extension::{
        default_account_state::DefaultAccountState, metadata_pointer::MetadataPointer,
        mint_close_authority::MintCloseAuthority, non_transferable::NonTransferable, BaseStateWithExtensionsMut, ExtensionType,
        StateWithExtensionsMut,
    },
    state::{AccountState, Mint},
};

fn base_mint(mint_authority: Pubkey, decimals: u8) -> Mint {
    Mint {
        mint_authority: COption::Some(mint_authority),
        supply: 0,
        decimals,
        is_initialized: true,
        freeze_authority: COption::None,
    }
}

/// A plain Token-2022 mint with no extensions (82-byte legacy layout).
fn pack_plain_mint(mint_authority: Pubkey, decimals: u8) -> Vec<u8> {
    let mut data = vec![0u8; Mint::LEN];
    base_mint(mint_authority, decimals).pack_into_slice(&mut data);
    data
}

/// Allocate a mint buffer sized for `extensions` and return it with the base
/// state packed and the account type byte set; the caller's closure
/// initializes the extension TLV entries.
fn pack_mint_with_extensions(
    mint_authority: Pubkey,
    decimals: u8,
    extensions: &[ExtensionType],
    init: impl FnOnce(&mut StateWithExtensionsMut<Mint>),
) -> Vec<u8> {
    let space = ExtensionType::try_calculate_account_len::<Mint>(extensions).unwrap();
    let mut data = vec![0u8; space];
    let mut state = StateWithExtensionsMut::<Mint>::unpack_uninitialized(&mut data).unwrap();
    init(&mut state);
    state.base = base_mint(mint_authority, decimals);
    state.pack_base();
    state.init_account_type().unwrap();
    data
}

/// A Token-2022 mint carrying an extension type the pinned spl-token-2022 5.0
/// crate does not know about, mimicking Pausable/PausableConfig (discriminant
/// 26, introduced in later Token-2022 releases). Crafted at the raw TLV level
/// since the crate cannot express it:
///   [0..82]  base Mint  [82..165] padding  [165] AccountType::Mint
///   [166..]  TLV: type u16=26, length u16=33, value (authority + paused)
fn pack_mint_with_unknown_extension(mint_authority: Pubkey, decimals: u8) -> Vec<u8> {
    const BASE_ACCOUNT_LENGTH: usize = spl_token_2022::state::Account::LEN; // 165
    const UNKNOWN_EXT_TYPE: u16 = 26; // Pausable in token-2022 >= 6.x
    const UNKNOWN_EXT_LEN: u16 = 33; // PausableConfig: authority (32) + paused (1)

    let mut data = vec![0u8; BASE_ACCOUNT_LENGTH + 1 + 4 + UNKNOWN_EXT_LEN as usize];
    base_mint(mint_authority, decimals).pack_into_slice(&mut data[..Mint::LEN]);
    data[BASE_ACCOUNT_LENGTH] = 1; // AccountType::Mint
    let tlv = BASE_ACCOUNT_LENGTH + 1;
    data[tlv..tlv + 2].copy_from_slice(&UNKNOWN_EXT_TYPE.to_le_bytes());
    data[tlv + 2..tlv + 4].copy_from_slice(&UNKNOWN_EXT_LEN.to_le_bytes());
    // value bytes left zeroed (authority = none, paused = false) — presence
    // alone must be enough to reject.
    data
}

/// Spin up a ProgramTest with the given mint data and try InitializePool,
/// returning the transaction result and the pool PDA.
async fn try_initialize_pool(
    mint_data: Vec<u8>,
    authority: &Keypair,
) -> (Result<(), BanksClientError>, BanksClient, Pubkey, Pubkey) {
    let program_id = chiefstaker::id();
    let mint = Pubkey::new_unique();
    let (pool_pda, _) = Pubkey::find_program_address(&[POOL_SEED, mint.as_ref()], &program_id);
    let (vault_pda, _) =
        Pubkey::find_program_address(&[TOKEN_VAULT_SEED, pool_pda.as_ref()], &program_id);

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
        mint,
        Account {
            lamports: 10_000_000,
            data: mint_data,
            owner: spl_token_2022::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    pt.add_account(
        authority.pubkey(),
        Account {
            lamports: 1_000_000_000,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    let (mut banks, payer, blockhash) = pt.start().await;

    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(pool_pda, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(vault_pda, false),
            AccountMeta::new(authority.pubkey(), true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_token_2022::id(), false),
            AccountMeta::new_readonly(sysvar::rent::id(), false),
        ],
        data: borsh::to_vec(&StakingInstruction::InitializePool { tau_seconds: 60 }).unwrap(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer, authority],
        blockhash,
    );
    let result = banks.process_transaction(tx).await;
    (result, banks, pool_pda, mint)
}

fn assert_unsupported_extension(result: Result<(), BanksClientError>) {
    let err = result.expect_err("InitializePool must be rejected for this mint");
    let BanksClientError::TransactionError(te) = err else {
        panic!("expected TransactionError, got {:?}", err)
    };
    let solana_sdk::transaction::TransactionError::InstructionError(_, ie) = te else {
        panic!("expected InstructionError, got {:?}", te)
    };
    assert_eq!(
        ie,
        InstructionError::Custom(StakingError::UnsupportedMintExtension as u32),
        "expected UnsupportedMintExtension"
    );
}

#[tokio::test]
async fn initialize_rejects_non_transferable_mint() {
    let authority = Keypair::new();
    let mint_data = pack_mint_with_extensions(
        authority.pubkey(),
        9,
        &[ExtensionType::NonTransferable],
        |state| {
            state.init_extension::<NonTransferable>(true).unwrap();
        },
    );
    let (result, mut banks, pool_pda, _) = try_initialize_pool(mint_data, &authority).await;
    assert_unsupported_extension(result);
    assert!(
        banks.get_account(pool_pda).await.unwrap().is_none(),
        "pool must not have been created"
    );
}

#[tokio::test]
async fn initialize_rejects_default_account_state_mint() {
    let authority = Keypair::new();
    let mint_data = pack_mint_with_extensions(
        authority.pubkey(),
        9,
        &[ExtensionType::DefaultAccountState],
        |state| {
            let ext = state.init_extension::<DefaultAccountState>(true).unwrap();
            ext.state = AccountState::Frozen as u8;
        },
    );
    let (result, _, _, _) = try_initialize_pool(mint_data, &authority).await;
    assert_unsupported_extension(result);
}

#[tokio::test]
async fn initialize_rejects_unknown_extension_mint() {
    // Mimics a Pausable mint, which the pinned spl-token-2022 5.0 crate cannot
    // name — the unknown-extension screen must reject it.
    let authority = Keypair::new();
    let mint_data = pack_mint_with_unknown_extension(authority.pubkey(), 9);
    let (result, _, _, _) = try_initialize_pool(mint_data, &authority).await;
    assert_unsupported_extension(result);
}

#[tokio::test]
async fn initialize_succeeds_for_plain_mint() {
    let authority = Keypair::new();
    let mint_data = pack_plain_mint(authority.pubkey(), 9);
    let (result, mut banks, pool_pda, mint) = try_initialize_pool(mint_data, &authority).await;
    result.expect("InitializePool must succeed for a plain mint");

    let pool_acc = banks.get_account(pool_pda).await.unwrap().unwrap();
    let pool = StakingPool::try_from_slice(&pool_acc.data).unwrap();
    assert!(pool.is_initialized());
    assert_eq!(pool.mint, mint);
}

#[tokio::test]
async fn initialize_succeeds_for_metadata_pointer_mint() {
    // pump.fun mints carry MetadataPointer (+ TokenMetadata); the benign
    // metadata extensions must stay allowed.
    let authority = Keypair::new();
    let auth_pk = authority.pubkey();
    let mint_data = pack_mint_with_extensions(
        auth_pk,
        9,
        &[ExtensionType::MetadataPointer],
        |state| {
            let ext = state.init_extension::<MetadataPointer>(true).unwrap();
            ext.authority = Some(auth_pk).try_into().unwrap();
        },
    );
    let (result, mut banks, pool_pda, mint) = try_initialize_pool(mint_data, &authority).await;
    result.expect("InitializePool must succeed for a MetadataPointer mint");

    let pool_acc = banks.get_account(pool_pda).await.unwrap().unwrap();
    let pool = StakingPool::try_from_slice(&pool_acc.data).unwrap();
    assert!(pool.is_initialized());
    assert_eq!(pool.mint, mint);
}

#[tokio::test]
async fn initialize_rejects_live_mint_close_authority() {
    // F2: a set close authority can close the mint at supply 0 and re-create
    // it at the same address with PermanentDelegate / TransferHook, bypassing
    // the one-time init screen.
    let authority = Keypair::new();
    let close_authority = Pubkey::new_unique();
    let mint_data = pack_mint_with_extensions(
        authority.pubkey(),
        9,
        &[ExtensionType::MintCloseAuthority],
        |state| {
            let ext = state.init_extension::<MintCloseAuthority>(true).unwrap();
            ext.close_authority = Some(close_authority).try_into().unwrap();
        },
    );
    let (result, mut banks, pool_pda, _) = try_initialize_pool(mint_data, &authority).await;
    assert_unsupported_extension(result);
    assert!(
        banks.get_account(pool_pda).await.unwrap().is_none(),
        "pool must not have been created"
    );
}

#[tokio::test]
async fn initialize_succeeds_for_revoked_mint_close_authority() {
    // The extension is present but its authority was revoked (None): the mint
    // can never be closed, so it must stay allowed.
    let authority = Keypair::new();
    let mint_data = pack_mint_with_extensions(
        authority.pubkey(),
        9,
        &[ExtensionType::MintCloseAuthority],
        |state| {
            let ext = state.init_extension::<MintCloseAuthority>(true).unwrap();
            ext.close_authority = None.try_into().unwrap();
        },
    );
    let (result, mut banks, pool_pda, mint) = try_initialize_pool(mint_data, &authority).await;
    result.expect("InitializePool must succeed when the close authority is None");

    let pool_acc = banks.get_account(pool_pda).await.unwrap().unwrap();
    let pool = StakingPool::try_from_slice(&pool_acc.data).unwrap();
    assert!(pool.is_initialized());
    assert_eq!(pool.mint, mint);
}
