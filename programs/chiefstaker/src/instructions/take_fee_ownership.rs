//! TakeFeeOwnership — permissionless crank
//!
//! Sets the pool PDA as the sole fee recipient (100% / 10000 bps) via
//! `update_fee_shares` on the pfee program, then revokes the fee sharing
//! authority via `revoke_fee_sharing_authority` (irreversible).
//!
//! Prerequisite: the fee sharing authority must already have been transferred
//! to the pool PDA off-chain via `transfer_fee_sharing_authority`.

use borsh::BorshDeserialize;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
};

use crate::{
    error::StakingError,
    state::{StakingPool, PFEE_PROGRAM_ID, POOL_SEED},
};

/// update_fee_shares discriminator
const UPDATE_FEE_SHARES_DISC: [u8; 8] = [189, 13, 136, 99, 187, 164, 237, 35];

/// revoke_fee_sharing_authority discriminator
const REVOKE_FEE_SHARING_AUTHORITY_DISC: [u8; 8] = [18, 233, 158, 39, 185, 207, 58, 104];

/// Take fee ownership: set pool as sole fee recipient and revoke authority.
///
/// None of the passed-through accounts (1..=17) may be the pool's token vault:
/// the pool PDA signs the CPI and is also the vault's authority, so the vault
/// must never be handed to the external pfee program.
///
/// Accounts (18):
///  0. `[W]` pool — Pool PDA ["pool", mint], CPI signer. Must be writable: it
///     is passed to pfee `update_fee_shares` as the (writable) payer.
///  1. `[]`  mint — must match pool.mint
///  2. `[]`  pfee_program
///  3. `[]`  pfee_event_authority — PDA on pfee: ["__event_authority"]
///  4. `[]`  pump_global — PDA on pump: ["global"]
///  5. `[W]` sharing_config — PDA on pfee: ["sharing-config", mint]
///  6. `[]`  bonding_curve — PDA on pump: ["bonding-curve", mint]
///  7. `[W]` pump_creator_vault — PDA on pump: ["creator-vault", sharing_config]
///  8. `[]`  system_program
///  9. `[]`  pump_program
/// 10. `[]`  pump_event_authority — PDA on pump: ["__event_authority"]
/// 11. `[]`  pump_amm_program
/// 12. `[]`  amm_event_authority — PDA on AMM: ["__event_authority"]
/// 13. `[]`  wsol_mint
/// 14. `[]`  token_program
/// 15. `[]`  associated_token_program
/// 16. `[W]` coin_creator_vault_auth — PDA on AMM: ["creator_vault", sharing_config]
/// 17. `[W]` coin_creator_vault_ata — ATA of wSOL for #16
pub fn process_take_fee_ownership(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();

    let pool_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let pfee_program_info = next_account_info(account_info_iter)?;
    let pfee_event_authority_info = next_account_info(account_info_iter)?;
    let pump_global_info = next_account_info(account_info_iter)?;
    let sharing_config_info = next_account_info(account_info_iter)?;
    let bonding_curve_info = next_account_info(account_info_iter)?;
    let pump_creator_vault_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;
    let pump_program_info = next_account_info(account_info_iter)?;
    let pump_event_authority_info = next_account_info(account_info_iter)?;
    let pump_amm_program_info = next_account_info(account_info_iter)?;
    let amm_event_authority_info = next_account_info(account_info_iter)?;
    let wsol_mint_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    let associated_token_program_info = next_account_info(account_info_iter)?;
    let coin_creator_vault_auth_info = next_account_info(account_info_iter)?;
    let coin_creator_vault_ata_info = next_account_info(account_info_iter)?;

    // ── Validate pool ───────────────────────────────────────────────────────
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

    // Defence in depth: the pool PDA signs the CPIs below into the external
    // (upgradeable) pfee program, and that same PDA is the authority of the
    // staked-token vault. Never let the vault be passed through to it.
    for passed_through in accounts.iter().take(18).skip(1) {
        if *passed_through.key == pool.token_vault {
            return Err(StakingError::InvalidTokenVault.into());
        }
    }

    // Verify pfee program address
    if *pfee_program_info.key != PFEE_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    // Pool PDA signing seeds
    let pool_seeds: &[&[u8]] = &[POOL_SEED, pool.mint.as_ref(), &[pool.bump]];

    // ── CPI 1: update_fee_shares ────────────────────────────────────────────
    // Data: 8-byte discriminator + Vec<Shareholder> with 1 entry {pool_pda, 10000u16}
    // Vec encoding: 4-byte length + 1 * (32 + 2) = 46 bytes total
    let mut update_data = Vec::with_capacity(8 + 4 + 34);
    update_data.extend_from_slice(&UPDATE_FEE_SHARES_DISC);
    update_data.extend_from_slice(&1u32.to_le_bytes()); // vec length = 1
    update_data.extend_from_slice(pool_info.key.as_ref()); // shareholder address = pool PDA
    update_data.extend_from_slice(&10000u16.to_le_bytes()); // 10000 bps = 100%

    // 19 account metas matching the pfee update_fee_shares IDL order
    let update_accounts = vec![
        AccountMeta::new_readonly(*pfee_event_authority_info.key, false),   // 0  event_authority
        AccountMeta::new_readonly(*pfee_program_info.key, false),           // 1  program (self)
        AccountMeta::new_readonly(*pool_info.key, true),                    // 2  authority (signer)
        AccountMeta::new_readonly(*pump_global_info.key, false),            // 3  global
        AccountMeta::new_readonly(*mint_info.key, false),                   // 4  mint
        AccountMeta::new(*sharing_config_info.key, false),                  // 5  sharing_config
        AccountMeta::new_readonly(*bonding_curve_info.key, false),          // 6  bonding_curve
        AccountMeta::new(*pump_creator_vault_info.key, false),              // 7  pump_creator_vault
        AccountMeta::new_readonly(*system_program_info.key, false),         // 8  system_program
        AccountMeta::new_readonly(*pump_program_info.key, false),           // 9  pump_program
        AccountMeta::new_readonly(*pump_event_authority_info.key, false),   // 10 pump_event_authority
        AccountMeta::new_readonly(*pump_amm_program_info.key, false),       // 11 pump_amm_program
        AccountMeta::new_readonly(*amm_event_authority_info.key, false),    // 12 amm_event_authority
        AccountMeta::new_readonly(*wsol_mint_info.key, false),              // 13 wsol_mint
        AccountMeta::new_readonly(*token_program_info.key, false),          // 14 token_program
        AccountMeta::new_readonly(*associated_token_program_info.key, false), // 15 ata_program
        AccountMeta::new(*coin_creator_vault_auth_info.key, false),         // 16 coin_creator_vault_auth
        AccountMeta::new(*coin_creator_vault_ata_info.key, false),          // 17 coin_creator_vault_ata
        AccountMeta::new(*pool_info.key, false),                            // 18 payer (pool PDA)
    ];

    let update_ix = Instruction {
        program_id: PFEE_PROGRAM_ID,
        accounts: update_accounts,
        data: update_data,
    };

    invoke_signed(
        &update_ix,
        &[
            pfee_event_authority_info.clone(),
            pfee_program_info.clone(),
            pool_info.clone(),
            pump_global_info.clone(),
            mint_info.clone(),
            sharing_config_info.clone(),
            bonding_curve_info.clone(),
            pump_creator_vault_info.clone(),
            system_program_info.clone(),
            pump_program_info.clone(),
            pump_event_authority_info.clone(),
            pump_amm_program_info.clone(),
            amm_event_authority_info.clone(),
            wsol_mint_info.clone(),
            token_program_info.clone(),
            associated_token_program_info.clone(),
            coin_creator_vault_auth_info.clone(),
            coin_creator_vault_ata_info.clone(),
        ],
        &[pool_seeds],
    )?;

    msg!("Fee shares updated: pool PDA = 100%");

    // ── CPI 2: revoke_fee_sharing_authority ─────────────────────────────────
    let revoke_data = REVOKE_FEE_SHARING_AUTHORITY_DISC.to_vec();

    let revoke_accounts = vec![
        AccountMeta::new_readonly(*pool_info.key, true),                    // 0  authority (signer)
        AccountMeta::new_readonly(*pump_global_info.key, false),            // 1  global
        AccountMeta::new_readonly(*mint_info.key, false),                   // 2  mint
        AccountMeta::new(*sharing_config_info.key, false),                  // 3  sharing_config
        AccountMeta::new_readonly(*pfee_event_authority_info.key, false),   // 4  event_authority
        AccountMeta::new_readonly(*pfee_program_info.key, false),           // 5  program (self)
    ];

    let revoke_ix = Instruction {
        program_id: PFEE_PROGRAM_ID,
        accounts: revoke_accounts,
        data: revoke_data,
    };

    invoke_signed(
        &revoke_ix,
        &[
            pool_info.clone(),
            pump_global_info.clone(),
            mint_info.clone(),
            sharing_config_info.clone(),
            pfee_event_authority_info.clone(),
            pfee_program_info.clone(),
        ],
        &[pool_seeds],
    )?;

    msg!("Fee sharing authority revoked — pool {} owns fees for mint {}", pool_info.key, mint_info.key);

    Ok(())
}
