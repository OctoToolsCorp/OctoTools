use anyhow::{Result, Context};
use solana_sdk::{
    pubkey::Pubkey,
    signature::Signer,
    signer::keypair::read_keypair_file,
};
use spl_associated_token_account::get_associated_token_address;
use std::collections::HashSet;
use std::str::FromStr; // Import the FromStr trait
use log::info;

use crate::wallet::{load_zombie_wallets_from_file}; // Removed unused get_wallet_keypair
use crate::errors::PumpfunError;
use crate::pump_instruction_builders::{
    // Programs & Sysvars
    PUMPFUN_PROGRAM_ID,
    // Pump.fun Static Accounts
    GLOBAL_STATE_PUBKEY,
    PUMPFUN_MINT_AUTHORITY,
    FEE_RECIPIENT_PUBKEY, // Corrected import
    EVENT_AUTHORITY_PUBKEY,
    // PDA Functions
    find_bonding_curve_pda,
    find_metadata_pda,
};

// Add direct imports for programs and sysvars
use solana_program::{
    system_program::ID as SYSTEM_PROGRAM_ID,
    sysvar::rent::ID as RENT_SYSVAR_ID,
};
use spl_token::ID as TOKEN_PROGRAM_ID;
use spl_associated_token_account::ID as ASSOCIATED_TOKEN_PROGRAM_ID;
use solana_sdk::compute_budget::ID as COMPUTE_BUDGET_PROGRAM_ID;
// Removed unused crate::pump_instruction_builders::METADATA_PROGRAM_ID;

pub async fn precalc_alt_addresses(
    mint_keypair_path: &str,
    keys_path_arg: Option<&str>,
) -> Result<Vec<Pubkey>> {
    info!("Pre-calculating addresses for ALT...");
    info!(" Using mint keypair path: {}", mint_keypair_path);
    let keys_path = keys_path_arg.unwrap_or("./keys.json");
    info!(" Using main keys path: {}", keys_path);

    // 1. Load Mint Keypair & Get Pubkey
    let mint_keypair = read_keypair_file(mint_keypair_path)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to read mint keypair file '{}': {}", mint_keypair_path, e)))?;
    let mint_pubkey = mint_keypair.pubkey();
    info!(" Mint Pubkey: {}", mint_pubkey);

    // 2. Load Payer & Zombie Wallets
    // Assuming payer is defined by env or first in keys.json if structure supports it
    let payer_keypair = crate::wallet::get_wallet_keypair()
        .context("Failed to load payer keypair")?;
    let payer_pubkey = payer_keypair.pubkey();
    info!(" Payer Pubkey: {}", payer_pubkey);
    let zombie_keypairs = load_zombie_wallets_from_file(Some(keys_path))
        .context("Failed to load zombie wallets")?;
    info!(" Loaded {} zombie wallets.", zombie_keypairs.len());

    // 3. Calculate Dynamic PDAs
    let (bonding_curve_pubkey, _) = find_bonding_curve_pda(&mint_pubkey)?;
    info!(" Bonding Curve PDA: {}", bonding_curve_pubkey);
    let (metadata_pubkey, _) = find_metadata_pda(&mint_pubkey)?;
    info!(" Metadata PDA: {}", metadata_pubkey);

    // 4. Calculate Dynamic ATAs
    let bonding_curve_vault_ata = get_associated_token_address(&bonding_curve_pubkey, &mint_pubkey);
    info!(" Bonding Curve Vault ATA: {}", bonding_curve_vault_ata);
    let payer_ata = get_associated_token_address(&payer_pubkey, &mint_pubkey);
    info!(" Payer ATA: {}", payer_ata);

    let mut zombie_atas = Vec::new();
    for zombie in &zombie_keypairs {
        let zombie_pk = zombie.pubkey();
        let zombie_ata = get_associated_token_address(&zombie_pk, &mint_pubkey);
        info!("  Zombie {} ATA: {}", zombie_pk, zombie_ata);
        zombie_atas.push(zombie_ata);
    }

    // 5. Collect All Addresses
    let mut all_addresses = HashSet::new();

    // Static Programs & Sysvars
    all_addresses.insert(PUMPFUN_PROGRAM_ID);
    all_addresses.insert(TOKEN_PROGRAM_ID);
    all_addresses.insert(ASSOCIATED_TOKEN_PROGRAM_ID);
    all_addresses.insert(SYSTEM_PROGRAM_ID);
    all_addresses.insert(RENT_SYSVAR_ID);
    all_addresses.insert(COMPUTE_BUDGET_PROGRAM_ID);

    // Static Pump.fun Accounts
    all_addresses.insert(GLOBAL_STATE_PUBKEY);
    all_addresses.insert(PUMPFUN_MINT_AUTHORITY);
    all_addresses.insert(FEE_RECIPIENT_PUBKEY); // Corrected usage
    all_addresses.insert(EVENT_AUTHORITY_PUBKEY);

    // Your Wallets
    all_addresses.insert(payer_pubkey);
    for zombie in &zombie_keypairs {
        all_addresses.insert(zombie.pubkey());
    }

    // Add Jito Tip Account if valid
    dotenvy::dotenv().ok(); // Ensure .env is loaded
    if let Ok(tip_acc_str) = std::env::var("JITO_TIP_ACCOUNT") {
        if let Ok(tip_acc_pk) = Pubkey::from_str(&tip_acc_str) {
            info!(" Adding Jito Tip Account {} to precalculated addresses", tip_acc_pk);
            all_addresses.insert(tip_acc_pk);
        } else {
            log::warn!(" JITO_TIP_ACCOUNT env var contains invalid pubkey '{}', not adding to ALT.", tip_acc_str);
        }
    } else {
        info!(" JITO_TIP_ACCOUNT env var not set, not adding to ALT.");
    }

    // Calculated Dynamic Addresses
    all_addresses.insert(mint_pubkey);
    all_addresses.insert(bonding_curve_pubkey);
    all_addresses.insert(metadata_pubkey);
    all_addresses.insert(bonding_curve_vault_ata);
    all_addresses.insert(payer_ata);
    for ata in zombie_atas {
        all_addresses.insert(ata);
    }

    // Convert HashSet to Vec and sort
    let mut sorted_addresses: Vec<Pubkey> = all_addresses.into_iter().collect();
    sorted_addresses.sort_by_key(|a| a.to_string());

    info!("Total unique addresses collected: {}", sorted_addresses.len());

    Ok(sorted_addresses)
} 