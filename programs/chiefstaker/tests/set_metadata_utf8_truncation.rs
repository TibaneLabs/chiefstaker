//! Regression test for audit finding A-5: SetPoolMetadata truncated the pool
//! name and the symbol tag by byte, so a multibyte token name / symbol could be
//! cut mid-codepoint and stored as invalid UTF-8. Truncation now walks back to
//! a char boundary; stored layout and max lengths (64 / 32 bytes) are unchanged.

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

async fn set_metadata(name: &str, symbol: &str) -> PoolMetadata {
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
    let mint_data = pack_mint_with_token_metadata(mint, name, symbol);
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
    banks.process_transaction(tx).await.expect("SetPoolMetadata");

    let acc = banks.get_account(metadata_pda).await.unwrap().unwrap();
    PoolMetadata::try_from_slice(&acc.data).unwrap()
}

const SUFFIX: &str = " Staking Pool";

/// 3-byte codepoints: the 51-byte prefix budget (64 - suffix) is an exact
/// multiple of 3 only by luck, so offset by one ASCII byte to force the byte
/// cut to land mid-codepoint.
#[tokio::test]
async fn name_truncation_does_not_split_multibyte_codepoint() {
    // 1 + 30 * 3 = 91 bytes; byte 51 falls inside a codepoint (1 + 16*3 = 49,
    // next boundary 52), so the prefix must be cut back to 49 bytes.
    let token_name = format!("A{}", "\u{732b}".repeat(30));
    let md = set_metadata(&token_name, "CAT").await;

    let stored = &md.name[..md.name_len as usize];
    let name = std::str::from_utf8(stored).expect("stored name must be valid UTF-8");
    let expected = format!("A{}{}", "\u{732b}".repeat(16), SUFFIX);
    assert_eq!(name, expected);
    assert_eq!(md.name_len as usize, 49 + SUFFIX.len());
    // Remainder of the fixed buffer stays zero-padded.
    assert!(md.name[md.name_len as usize..].iter().all(|b| *b == 0));
}

/// A long ASCII name still truncates to exactly the 64-byte maximum.
#[tokio::test]
async fn ascii_name_truncation_unchanged() {
    let token_name = "x".repeat(80);
    let md = set_metadata(&token_name, "X").await;
    assert_eq!(md.name_len, 64);
    let name = std::str::from_utf8(&md.name[..64]).unwrap();
    assert_eq!(name, format!("{}{}", "x".repeat(51), SUFFIX));
}

/// A short multibyte name that fits is stored untouched.
#[tokio::test]
async fn short_multibyte_name_is_not_truncated() {
    let md = set_metadata("\u{732b}\u{732b}", "CAT").await;
    let name = std::str::from_utf8(&md.name[..md.name_len as usize]).unwrap();
    assert_eq!(name, format!("\u{732b}\u{732b}{}", SUFFIX));
}

/// Symbol tag "#<symbol>" is capped to 32 bytes on a char boundary.
#[tokio::test]
async fn symbol_tag_truncation_does_not_split_multibyte_codepoint() {
    // "#" + 20 * 3 bytes = 61 bytes; byte 32 falls inside a codepoint
    // (1 + 10*3 = 31, next boundary 34), so the tag is cut back to 31 bytes.
    let symbol = "\u{732b}".repeat(20);
    let md = set_metadata("Cat", &symbol).await;

    assert_eq!(md.num_tags, 3);
    let len = md.tag_lengths[2] as usize;
    let tag = std::str::from_utf8(&md.tags[2][..len]).expect("stored tag must be valid UTF-8");
    assert_eq!(tag, format!("#{}", "\u{732b}".repeat(10)));
    assert_eq!(len, 31);
    assert!(md.tags[2][len..].iter().all(|b| *b == 0));

    // Fixed tags unaffected.
    assert_eq!(&md.tags[0][..md.tag_lengths[0] as usize], b"#stakingpool");
    assert_eq!(&md.tags[1][..md.tag_lengths[1] as usize], b"#chiefstaker");
}

/// A long ASCII symbol still caps at exactly 32 bytes.
#[tokio::test]
async fn ascii_symbol_tag_truncation_unchanged() {
    let symbol = "S".repeat(40);
    let md = set_metadata("Cat", &symbol).await;
    assert_eq!(md.tag_lengths[2], 32);
    assert_eq!(&md.tags[2][..32], format!("#{}", "s".repeat(31)).as_bytes());
}
