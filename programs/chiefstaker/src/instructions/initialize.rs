//! Initialize a staking pool for a Token 2022 mint

use borsh::BorshSerialize;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};
use solana_program::program_option::COption;
use solana_program::program_pack::Pack;
use spl_token_2022::{
    extension::{
        confidential_transfer::ConfidentialTransferMint,
        default_account_state::DefaultAccountState,
        mint_close_authority::MintCloseAuthority,
        non_transferable::NonTransferable,
        permanent_delegate::PermanentDelegate,
        transfer_fee::TransferFeeConfig,
        transfer_hook::TransferHook,
        BaseStateWithExtensions, PodStateWithExtensions, StateWithExtensions,
    },
    pod::PodMint,
    state::Mint,
};
use spl_token_metadata_interface::state::TokenMetadata;

use crate::{
    error::StakingError,
    state::{
        is_valid_token_program, is_whitelisted_pool_creator, StakingPool, METAPLEX_PROGRAM_ID,
        METEORA_DBC_CREATOR_DISC,
        METEORA_DBC_CREATOR_PROGRAM_ID, PFEE_PROGRAM_ID, PFEE_SHARING_CONFIG_DISC, POOL_SEED,
        PUMP_AMM_POOL_DISC, PUMP_AMM_PROGRAM_ID, PUMP_PROGRAM_ID, TOKEN_VAULT_SEED,
    },
};

/// Initialize a new staking pool
///
/// Accounts:
/// 0. `[writable]` Pool account (PDA: ["pool", mint])
/// 1. `[]` Token mint (Token 2022)
/// 2. `[writable]` Token vault (PDA: ["token_vault", pool])
/// 3. `[writable, signer]` Authority/payer
/// 4. `[]` System program
/// 5. `[]` Token 2022 program
/// 6. `[]` Rent sysvar
pub fn process_initialize_pool(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    tau_seconds: u64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();

    let pool_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let token_vault_info = next_account_info(account_info_iter)?;
    let authority_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    let rent_sysvar_info = next_account_info(account_info_iter)?;

    // Validate token program (SPL Token or Token 2022)
    if !is_valid_token_program(token_program_info.key) {
        return Err(StakingError::InvalidTokenProgram.into());
    }

    // Validate authority is signer
    if !authority_info.is_signer {
        return Err(StakingError::MissingRequiredSigner.into());
    }

    // Validate tau_seconds (min 60s to prevent near-instant maturation,
    // max ~10 years to ensure weights eventually mature)
    const MIN_TAU_SECONDS: u64 = 60;
    const MAX_TAU_SECONDS: u64 = 10 * 365 * 24 * 60 * 60; // ~10 years
    if tau_seconds < MIN_TAU_SECONDS || tau_seconds > MAX_TAU_SECONDS {
        return Err(StakingError::InvalidTau.into());
    }

    // Verify mint is owned by the provided token program
    if *mint_info.owner != *token_program_info.key {
        return Err(StakingError::InvalidMintProgram.into());
    }

    // Verify mint is valid by trying to unpack it
    let mint_data = mint_info.try_borrow_data()?;
    let mint_state = StateWithExtensions::<Mint>::unpack(&mint_data)?;

    // Reject Token 2022 mints with dangerous extensions
    // (SPL Token mints have no extensions, so these checks are skipped naturally)
    if *token_program_info.key == spl_token_2022::id() {
        // Reject mints with transfer fee extension — fee-on-transfer tokens
        // would cause total_staked to diverge from actual vault balance,
        // eventually bricking unstakes for later users.
        if mint_state.get_extension::<TransferFeeConfig>().is_ok() {
            msg!("Token 2022 mints with TransferFee extension are not supported");
            return Err(StakingError::InvalidPoolMint.into());
        }

        // Reject mints with PermanentDelegate — the delegate can transfer tokens
        // out of the vault at any time, breaking the total_staked invariant and
        // enabling theft of all staked tokens.
        if mint_state.get_extension::<PermanentDelegate>().is_ok() {
            msg!("Token 2022 mints with PermanentDelegate extension are not supported");
            return Err(StakingError::UnsupportedMintExtension.into());
        }

        // Reject mints with TransferHook — allows arbitrary program execution
        // during every transfer CPI (stake/unstake), which could manipulate
        // state or extract MEV.
        if mint_state.get_extension::<TransferHook>().is_ok() {
            msg!("Token 2022 mints with TransferHook extension are not supported");
            return Err(StakingError::UnsupportedMintExtension.into());
        }

        // Reject mints with NonTransferable — tokens from such a mint can
        // never be transferred, so staking into (and unstaking out of) the
        // vault is impossible; the pool would be bricked from creation.
        if mint_state.get_extension::<NonTransferable>().is_ok() {
            msg!("Token 2022 mints with NonTransferable extension are not supported");
            return Err(StakingError::UnsupportedMintExtension.into());
        }

        // Reject mints with DefaultAccountState — a default state of Frozen
        // would leave the vault token account frozen on creation, making the
        // pool permanently unusable. Rejected outright (regardless of the
        // current default) — simpler and safer than special-casing Frozen.
        if mint_state.get_extension::<DefaultAccountState>().is_ok() {
            msg!("Token 2022 mints with DefaultAccountState extension are not supported");
            return Err(StakingError::UnsupportedMintExtension.into());
        }

        // Reject mints with ConfidentialTransferMint — confidential balances
        // would undermine the vault-balance == total_staked invariant the
        // program relies on for solvency.
        if mint_state.get_extension::<ConfidentialTransferMint>().is_ok() {
            msg!("Token 2022 mints with ConfidentialTransfer extension are not supported");
            return Err(StakingError::UnsupportedMintExtension.into());
        }

        // Reject mints with a LIVE MintCloseAuthority. Token-2022 lets the close
        // authority close the mint whenever supply is 0; the mint address can
        // then be re-created with extensions this screen rejects
        // (PermanentDelegate, TransferHook, ...) while the pool and vault PDAs
        // still reference it. The screen only runs once, at init, so a mint
        // that can be swapped out afterwards defeats it. An extension whose
        // authority has been revoked (None) is harmless — Token-2022 does not
        // allow a None authority to be set again — and stays allowed.
        if let Ok(close_ext) = mint_state.get_extension::<MintCloseAuthority>() {
            if Option::<Pubkey>::from(close_ext.close_authority).is_some() {
                msg!("Token 2022 mints with an active MintCloseAuthority are not supported");
                return Err(StakingError::UnsupportedMintExtension.into());
            }
        }

        // Reject mints carrying any extension type this build does not know
        // about. The pinned spl-token-2022 5.0 crate predates extensions such
        // as Pausable/PausableConfig — a pausable mint lets its creator halt
        // all transfers after users stake, locking principal indefinitely.
        // `get_extension_types()` fails on unknown extension discriminants
        // (and on malformed TLV data), so any error here means the mint
        // carries an extension we cannot vet — reject it.
        if mint_state.get_extension_types().is_err() {
            msg!("Token 2022 mint carries an unknown or malformed extension; not supported");
            return Err(StakingError::UnsupportedMintExtension.into());
        }
    }

    // === Authority check: signer must match a known authority for this mint ===
    let mut authority_verified = false;

    // 0. Whitelisted operators may create a pool for any mint, regardless of
    //    whether they are one of the mint's recognized authorities.
    if is_whitelisted_pool_creator(authority_info.key) {
        authority_verified = true;
    }

    // 1. Token 2022: check metadata update_authority
    if !authority_verified && *token_program_info.key == spl_token_2022::id() {
        let pod_mint = PodStateWithExtensions::<PodMint>::unpack(&mint_data)?;
        if let Ok(token_metadata) = pod_mint.get_variable_len_extension::<TokenMetadata>() {
            let update_auth_option: Option<Pubkey> = token_metadata.update_authority.into();
            let update_auth = update_auth_option.unwrap_or_default();
            if update_auth != Pubkey::default() && update_auth == *authority_info.key {
                authority_verified = true;
            }
        }
    }

    // 2. Check mint_authority (works for both SPL Token and Token 2022)
    if !authority_verified {
        if let COption::Some(mint_auth) = mint_state.base.mint_authority {
            if mint_auth == *authority_info.key {
                authority_verified = true;
            }
        }
    }

    // 3–7. Check remaining accounts for authority proof.
    // Collect all valid authorities, then apply priority:
    //   pfee SharingConfig admin > PumpSwap coin_creator > bonding curve creator
    //     > Meteora DBC creator > pfee SharingConfig creator > Metaplex update_authority
    if !authority_verified {
        let mut metaplex_update_auth: Option<Pubkey> = None;
        let mut pfee_admin: Option<Pubkey> = None;
        let mut pfee_creator: Option<Pubkey> = None;
        let mut pump_amm_coin_creator: Option<Pubkey> = None;
        let mut pump_bonding_creator: Option<Pubkey> = None;
        let mut meteora_dbc_creator: Option<Pubkey> = None;

        while let Ok(proof_info) = next_account_info(account_info_iter) {
            // 3. Metaplex metadata: owner=metaqbxx, PDA=["metadata", program, mint]
            if *proof_info.owner == METAPLEX_PROGRAM_ID && metaplex_update_auth.is_none() {
                let (expected_pda, _) = Pubkey::find_program_address(
                    &[
                        b"metadata",
                        METAPLEX_PROGRAM_ID.as_ref(),
                        mint_info.key.as_ref(),
                    ],
                    &METAPLEX_PROGRAM_ID,
                );
                if *proof_info.key == expected_pda {
                    let data = proof_info.try_borrow_data()?;
                    if data.len() >= 33 {
                        let update_auth = Pubkey::try_from(&data[1..33]).unwrap();
                        if update_auth != Pubkey::default() {
                            metaplex_update_auth = Some(update_auth);
                        }
                    }
                }
            }

            // 4. pfee SharingConfig: owner=pfee, PDA=["sharing-config", mint]
            //    Reads admin (offset 43) and creator (offset 80, v2+).
            //    Both are valid regardless of admin_revoked / creator_revoked flags.
            if *proof_info.owner == PFEE_PROGRAM_ID && pfee_admin.is_none() {
                let (expected_pda, _) = Pubkey::find_program_address(
                    &[b"sharing-config", mint_info.key.as_ref()],
                    &PFEE_PROGRAM_ID,
                );
                if *proof_info.key == expected_pda {
                    let data = proof_info.try_borrow_data()?;
                    if data.len() >= 75 && data[..8] == PFEE_SHARING_CONFIG_DISC {
                        pfee_admin = Some(Pubkey::try_from(&data[43..75]).unwrap());
                        // v2 SharingConfig: creator at offset 80
                        if data.len() >= 112 {
                            let creator = Pubkey::try_from(&data[80..112]).unwrap();
                            if creator != Pubkey::default() {
                                pfee_creator = Some(creator);
                            }
                        }
                    }
                }
            }

            // 5. PumpSwap AMM pool: owner=pAMM, verify discriminator + base_mint
            if *proof_info.owner == PUMP_AMM_PROGRAM_ID && pump_amm_coin_creator.is_none() {
                let data = proof_info.try_borrow_data()?;
                if data.len() >= 243
                    && data[..8] == PUMP_AMM_POOL_DISC
                    && Pubkey::try_from(&data[43..75]).unwrap() == *mint_info.key
                {
                    pump_amm_coin_creator = Some(Pubkey::try_from(&data[211..243]).unwrap());
                }
            }

            // 6. PumpFun bonding curve: owner=pump, PDA=["bonding-curve", mint]
            if *proof_info.owner == PUMP_PROGRAM_ID && pump_bonding_creator.is_none() {
                let (expected_pda, _) = Pubkey::find_program_address(
                    &[b"bonding-curve", mint_info.key.as_ref()],
                    &PUMP_PROGRAM_ID,
                );
                if *proof_info.key == expected_pda {
                    let data = proof_info.try_borrow_data()?;
                    if data.len() >= 81 {
                        pump_bonding_creator = Some(Pubkey::try_from(&data[49..81]).unwrap());
                    }
                }
            }

            // 7. Meteora DBC creator: owner=GL6k, verify discriminator + mint
            if *proof_info.owner == METEORA_DBC_CREATOR_PROGRAM_ID
                && meteora_dbc_creator.is_none()
            {
                let data = proof_info.try_borrow_data()?;
                if data.len() >= 72
                    && data[..8] == METEORA_DBC_CREATOR_DISC
                    && Pubkey::try_from(&data[40..72]).unwrap() == *mint_info.key
                {
                    meteora_dbc_creator = Some(Pubkey::try_from(&data[8..40]).unwrap());
                }
            }
        }

        // Apply priority: pfee admin > PumpSwap coin_creator > bonding curve creator > Meteora DBC creator
        let canonical = pfee_admin
            .or(pump_amm_coin_creator)
            .or(pump_bonding_creator)
            .or(meteora_dbc_creator);

        if let Some(auth) = canonical {
            if auth == *authority_info.key {
                authority_verified = true;
            }
        }

        // pfee SharingConfig creator (independent — admin and creator are different roles)
        if !authority_verified {
            if let Some(auth) = pfee_creator {
                if auth == *authority_info.key {
                    authority_verified = true;
                }
            }
        }

        // Metaplex update_authority (independent of PumpFun chain)
        if !authority_verified {
            if let Some(auth) = metaplex_update_auth {
                if auth == *authority_info.key {
                    authority_verified = true;
                }
            }
        }
    }

    if !authority_verified {
        msg!(
            "Signer does not match any known authority for mint {}",
            mint_info.key
        );
        return Err(StakingError::InvalidAuthority.into());
    }

    // Derive and verify pool PDA
    let (expected_pool, pool_bump) =
        Pubkey::find_program_address(&[POOL_SEED, mint_info.key.as_ref()], program_id);
    if *pool_info.key != expected_pool {
        return Err(StakingError::InvalidPDA.into());
    }

    // Derive and verify token vault PDA
    let (expected_vault, vault_bump) =
        Pubkey::find_program_address(&[TOKEN_VAULT_SEED, pool_info.key.as_ref()], program_id);
    if *token_vault_info.key != expected_vault {
        return Err(StakingError::InvalidPDA.into());
    }

    // Reject re-initialization. The pool/vault PDAs are deterministic and could
    // have been pre-funded with lamports by a third party (a griefing vector that
    // would make plain `create_account` fail), but a third party cannot allocate
    // or assign them. So an empty (zero-length) data buffer means "not yet
    // initialized" regardless of any stray lamports; non-empty means already done.
    if !pool_info.data_is_empty() {
        return Err(StakingError::AlreadyInitialized.into());
    }
    if !token_vault_info.data_is_empty() {
        return Err(StakingError::AlreadyInitialized.into());
    }

    let rent = Rent::from_account_info(rent_sysvar_info)?;
    let clock = Clock::get()?;

    // Create pool account (tolerates a pre-funded PDA)
    let pool_seeds = &[POOL_SEED, mint_info.key.as_ref(), &[pool_bump]];
    create_pda_account(
        authority_info,
        pool_info,
        system_program_info,
        program_id,
        StakingPool::LEN,
        &rent,
        pool_seeds,
    )?;

    // Create token vault account (tolerates a pre-funded PDA)
    let vault_seeds = &[TOKEN_VAULT_SEED, pool_info.key.as_ref(), &[vault_bump]];

    // Get the size needed for a token account
    let vault_size = if *token_program_info.key == spl_token_2022::id() {
        spl_token_2022::extension::ExtensionType::try_calculate_account_len::<
            spl_token_2022::state::Account,
        >(&[])?
    } else {
        spl_token_2022::state::Account::LEN
    };

    create_pda_account(
        authority_info,
        token_vault_info,
        system_program_info,
        token_program_info.key,
        vault_size,
        &rent,
        vault_seeds,
    )?;

    // Initialize token vault as token account
    invoke_signed(
        &spl_token_2022::instruction::initialize_account3(
            token_program_info.key,
            token_vault_info.key,
            mint_info.key,
            pool_info.key, // Pool PDA is the owner of the vault
        )?,
        &[token_vault_info.clone(), mint_info.clone()],
        &[vault_seeds],
    )?;

    // Initialize pool state
    let pool = StakingPool::new(
        *mint_info.key,
        *token_vault_info.key,
        *pool_info.key, // Reward vault is the pool itself (stores SOL as lamports)
        *authority_info.key,
        tau_seconds,
        clock.unix_timestamp,
        pool_bump,
    );

    // Serialize pool state
    let mut pool_data = pool_info.try_borrow_mut_data()?;
    pool.serialize(&mut &mut pool_data[..])?;

    msg!("Initialized staking pool for mint {}", mint_info.key);
    msg!("Tau: {} seconds", tau_seconds);

    Ok(())
}

/// Create a program-derived account at `target`, tolerating the case where the
/// target PDA has already been funded with lamports by a third party.
///
/// `system_instruction::create_account` requires the destination to hold zero
/// lamports, so anyone can permanently block creation of a deterministic PDA by
/// sending it 1 lamport first. To avoid that DoS we, when the account is already
/// funded, top it up to rent-exemption and then `allocate` + `assign` it under
/// its own seeds — operations that require the PDA's signature and therefore
/// cannot be performed by a griefer who only transferred lamports.
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
