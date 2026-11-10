//! Set (create or update) pool metadata account — permissionless, no args

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
use spl_token_2022::extension::{BaseStateWithExtensions, PodStateWithExtensions};
use spl_token_2022::pod::PodMint;
use spl_token_metadata_interface::state::TokenMetadata;

use crate::{
    error::StakingError,
    state::{
        create_pda_account, PoolMetadata, StakingPool, METADATA_DISCRIMINATOR, METADATA_SEED,
    },
};

const NAME_SUFFIX: &str = " Staking Pool";
const TAG_STAKING_POOL: &str = "#stakingpool";
const TAG_CHIEFSTAKER: &str = "#chiefstaker";
const URL_PREFIX: &str = "https://labs.chiefpussy.com/staking/";

/// Set pool metadata. Permissionless, no instruction args.
///
/// Derives name from the Token 2022 mint's metadata extension:
///   name = "<token name> Staking Pool"
/// Tags are fixed: #stakingpool, #chiefstaker, #<symbol lowercase>
/// member_count is preserved across updates (starts at 0 on create).
///
/// Accounts:
/// 0. `[]` Pool account
/// 1. `[writable]` Metadata PDA (["metadata", pool])
/// 2. `[]` Token mint (must have TokenMetadata extension)
/// 3. `[writable, signer]` Payer
/// 4. `[]` System program
pub fn process_set_pool_metadata(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();

    let pool_info = next_account_info(account_info_iter)?;
    let metadata_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let payer_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;

    // Validate payer is signer
    if !payer_info.is_signer {
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

    // Verify pool PDA
    let (expected_pool, _) = StakingPool::derive_pda(&pool.mint, program_id);
    if *pool_info.key != expected_pool {
        return Err(StakingError::InvalidPDA.into());
    }

    // Verify mint matches pool
    if pool.mint != *mint_info.key {
        return Err(StakingError::InvalidPoolMint.into());
    }

    // Read token name and symbol from Token 2022 metadata extension
    let mint_data = mint_info.try_borrow_data()?;
    let mint_state = PodStateWithExtensions::<PodMint>::unpack(&mint_data)?;
    let token_metadata = mint_state.get_variable_len_extension::<TokenMetadata>()?;

    let token_name = token_metadata.name.trim();
    let token_symbol = token_metadata.symbol.trim();

    // Build display name: "<token name> Staking Pool", truncated to 64 bytes
    let full_name = format!("{}{}", token_name, NAME_SUFFIX);
    let name_bytes = if full_name.len() > 64 {
        // Truncate token name to fit, keeping suffix
        let max_prefix = 64 - NAME_SUFFIX.len();
        let truncated = &token_name.as_bytes()[..max_prefix];
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(truncated);
        buf.extend_from_slice(NAME_SUFFIX.as_bytes());
        buf
    } else {
        full_name.into_bytes()
    };

    // Build symbol tag: "#<symbol lowercase>", capped to 32 bytes
    let symbol_lower = token_symbol.to_lowercase();
    let symbol_tag = format!("#{}", symbol_lower);

    // Derive and verify metadata PDA
    let (expected_metadata, metadata_bump) =
        PoolMetadata::derive_pda(pool_info.key, program_id);
    if *metadata_info.key != expected_metadata {
        return Err(StakingError::InvalidPDA.into());
    }

    // Preserve existing member_count when updating
    let existing_member_count = if !metadata_info.data_is_empty() {
        if metadata_info.owner != program_id {
            return Err(StakingError::InvalidAccountOwner.into());
        }
        let existing = PoolMetadata::try_from_slice(&metadata_info.try_borrow_data()?)?;
        if !existing.is_initialized() {
            return Err(StakingError::NotInitialized.into());
        }
        if existing.pool != *pool_info.key {
            return Err(StakingError::InvalidPool.into());
        }
        existing.member_count
    } else {
        // Account doesn't exist — create it. Uses the pre-fund-tolerant helper so
        // a griefer cannot permanently block metadata creation by sending the
        // deterministic metadata PDA lamports first (plain create_account
        // requires a zero balance at the destination).
        let rent = Rent::get()?;
        let metadata_seeds = &[
            METADATA_SEED,
            pool_info.key.as_ref(),
            &[metadata_bump],
        ];

        create_pda_account(
            payer_info,
            metadata_info,
            system_program_info,
            program_id,
            PoolMetadata::LEN,
            &rent,
            metadata_seeds,
        )?;
        0
    };

    // Build URL: https://labs.chiefpussy.com/staking/<mint_base58>
    let url_str = format!("{}{}", URL_PREFIX, mint_info.key);
    let url_bytes = url_str.as_bytes();
    let url_len = url_bytes.len().min(128);
    let mut url_buf = [0u8; 128];
    url_buf[..url_len].copy_from_slice(&url_bytes[..url_len]);

    // Fill name buffer
    let mut name_buf = [0u8; 64];
    let name_len = name_bytes.len().min(64);
    name_buf[..name_len].copy_from_slice(&name_bytes[..name_len]);

    // Fill tags: #stakingpool, #chiefstaker, #<symbol>
    let tags_list: [&[u8]; 3] = [
        TAG_STAKING_POOL.as_bytes(),
        TAG_CHIEFSTAKER.as_bytes(),
        &symbol_tag.as_bytes()[..symbol_tag.len().min(32)],
    ];
    let mut tag_lengths = [0u8; 8];
    let mut tags_buf = [[0u8; 32]; 8];
    for (i, tag_bytes) in tags_list.iter().enumerate() {
        let len = tag_bytes.len().min(32);
        tag_lengths[i] = len as u8;
        tags_buf[i][..len].copy_from_slice(&tag_bytes[..len]);
    }

    let metadata = PoolMetadata {
        discriminator: METADATA_DISCRIMINATOR,
        pool: *pool_info.key,
        name_len: name_len as u8,
        name: name_buf,
        num_tags: 3,
        tag_lengths,
        tags: tags_buf,
        url_len: url_len as u8,
        url: url_buf,
        member_count: existing_member_count,
        bump: metadata_bump,
    };

    let mut metadata_data = metadata_info.try_borrow_mut_data()?;
    metadata.serialize(&mut &mut metadata_data[..])?;

    msg!("Set pool metadata for {}", pool_info.key);

    Ok(())
}
