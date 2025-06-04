use anyhow::{Result, Context, anyhow};
use log::{info, error, warn}; // Removed unused debug
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer}, // Removed unused Signature
    signer::keypair::read_keypair_file,
    transaction::VersionedTransaction,
    // instruction::{Instruction, AccountMeta}, // Will add AccountMeta separately
    instruction::Instruction, // Keep Instruction here
    message::{v0::Message as MessageV0, VersionedMessage}, // Removed unused Message
    compute_budget::ComputeBudgetInstruction,
    // commitment_config::{CommitmentLevel}, // Removed unused CommitmentLevel and CommitmentConfig
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    hash::Hash,
    // Removed unused system_program
    // Removed unused sysvar
    // Removed unused native_token::LAMPORTS_PER_SOL
};
use solana_sdk::account::Account; // <-- Add import for Account struct
// use solana_sdk::instruction::AccountMeta; // Add AccountMeta explicitly and separately - Unused
use solana_program::native_token::sol_to_lamports;
use solana_program::system_instruction;
use borsh::BorshDeserialize;
use std::str::FromStr;
use std::sync::Arc;
use std::env;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use tokio::time::{timeout, Duration};
use tokio::sync::mpsc::UnboundedSender; // <-- Add sender import
use crate::models::LaunchStatus; // <-- Import from new location
use bincode; // Added for transaction size logging

use crate::config::{get_commitment_config}; // Removed unused get_rpc_url
use crate::errors::PumpfunError;
// use crate::wallet::{load_zombie_wallets_from_file}; // Removed unused import
use crate::models::wallet::WalletInfo; // <-- Add import for WalletInfo
use crate::models::settings::AppSettings; // <-- ADDED for parent wallet
use crate::commands::utils::load_keypair_from_string; // <-- ADDED for parent wallet
use crate::utils::transaction::{send_jito_bundle_via_json_rpc, simulate_and_check};
use crate::pump_instruction_builders::{
    build_pump_create_instruction, build_pump_buy_instruction, build_pump_extend_account_instruction, // Added extend
    find_bonding_curve_pda, find_metadata_pda, GlobalState, calculate_tokens_out,
    apply_slippage_to_tokens_out, apply_slippage_to_sol_cost, GLOBAL_STATE_PUBKEY,
    BondingCurveAccount, predict_next_curve_state, EXAMPLE_CREATOR_VAULT_PUBKEY, find_creator_vault_pda, FEE_RECIPIENT_PUBKEY, // Added FEE_RECIPIENT_PUBKEY
};
use crate::api::upload_metadata_to_ipfs;
use crate::models::token::PumpfunMetadata;
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent, // Use idempotent version
    // Removed unused ID as ASSOCIATED_TOKEN_PROGRAM_ID
};
use spl_token;

// Constants for the target "create + buy" transaction profile based on the example
const TARGET_CREATE_BUY_COMPUTE_LIMIT: u32 = 220_000; // Increased from 196_971
const TARGET_CREATE_BUY_COMPUTE_PRICE_MICRO_LAMPORTS: u64 = 590_909;
const TIP_TX_COMPUTE_PRICE_MICRO_LAMPORTS: u64 = 100_000; // Price for the separate tip transaction

// Original constants, can be used for other scenarios or kept for reference
const _ORIGINAL_CREATE_DEV_TX_PRIORITY_FEE_LAMPORTS: u64 = 750_000; // Increased for congestion
const ZOMBIE_TX_PRIORITY_FEE_LAMPORTS: u64 = 500_000; // Added for congestion
const _ORIGINAL_CREATE_DEV_TX_COMPUTE_LIMIT: u32 = 700_000;
const ZOMBIE_BUY_COMPUTE_LIMIT: u32 = 250_000;
const ATA_CREATE_COMPUTE_LIMIT: u32 = 50_000;
const TIP_TX_COMPUTE_LIMIT: u32 = 10_000;
const MAX_ZOMBIES_PER_TX: usize = 7; // Original, kept for reference or other potential uses
const ZOMBIE_BUY_BATCH_SIZE: usize = 5; // New constant for zombie buy batching
const MAX_ZOMBIE_BUY_TX_INSTRUCTIONS: usize = 7;
const ZOMBIE_CHUNK_COMPUTE_LIMIT: u32 = 1_400_000; // This might need adjustment if batch size changes CU needs significantly

// Helper function to predict state after a buy - Removed redundant simulation constant
// [Function predict_next_curve_state deleted here]

#[allow(clippy::too_many_arguments)]
pub async fn launch_buy(
    name: String,
    symbol: String,
    description: String,
    image_url: String,
    dev_buy_sol: f64,
    zombie_buy_sol: f64,
    slippage_bps: u64,
    // priority_fee: u64, // Removed, using constants now
    lookup_table_str: String,
    mint_keypair_path: Option<String>,
    minter_private_key_str: String, // <-- Add minter key param
    zombie_wallet_data: Vec<WalletInfo>, // <-- Replace keys_path with loaded data
    simulate_only: bool,
    sender: UnboundedSender<LaunchStatus>, // <-- Add sender parameter
    jito_block_engine_url: String, // <-- ADDED Jito URL
    main_tx_priority_fee_micro_lamports: u64, // New parameter for Tx1 and Zombie Tx priority fee
    jito_actual_tip_sol: f64, // New parameter for the actual Jito tip in SOL
) -> Result<()> {
    let _ = sender.send(LaunchStatus::Starting);
    info!("🚀 Starting Launch & Buy Jito Bundle process with Main Prio Fee: {} microLamports, Jito Tip: {} SOL...", main_tx_priority_fee_micro_lamports, jito_actual_tip_sol);
    info!("  Token Name: {}", name);
    info!("  Token Symbol: {}", symbol);
    info!("  Description: {}", description);
    info!("  Image URL: {}", image_url);
    info!("  Dev Buy (SOL): {}", dev_buy_sol);
    info!("  Zombie Buy (SOL per wallet): {}", zombie_buy_sol); // Re-enabled
    info!("  Slippage (bps): {}", slippage_bps);
    info!("  Lookup Table: {}", lookup_table_str);

    // --- Global Setup ---
    info!("---------------------------------");
    info!("1. Global Setup");
    info!("---------------------------------");
    
    let payer_keypair = {
        let bytes = bs58::decode(&minter_private_key_str)
            .into_vec()
            .map_err(|e| PumpfunError::Wallet(format!("Failed to decode base58 minter private key: {}", e)))?;
        Keypair::from_bytes(&bytes)
            .map_err(|e| PumpfunError::Wallet(format!("Failed to create minter keypair from bytes: {}", e)))?
    };
    let payer_pubkey = payer_keypair.pubkey();
    info!("🔑 Loaded Payer/Minter Keypair (from GUI settings): {}", payer_pubkey);

    // Determine the actual creator vault once, based on the payer (creator of this token).
    // This vault will receive the creator's share of SOL from all buys of this token.
    let example_creator_pubkey_for_logic_str = "6NryqHZxhy7HxUfVvQtnDg1Xh55tYr29VbvyRbP9TLZW"; // Pubkey from the example transaction, assumed to be a known special creator.
    let example_creator_pk_for_logic = Pubkey::from_str(example_creator_pubkey_for_logic_str)
        .expect("Failed to parse example_creator_pubkey_for_logic_str"); // This should be infallible if the string is correct.

    let actual_creator_vault_pubkey = if payer_pubkey == example_creator_pk_for_logic {
        info!("  Creator (payer) {} matches the example creator. Using specific EXAMPLE_CREATOR_VAULT_PUBKEY {} for all buys.", payer_pubkey, EXAMPLE_CREATOR_VAULT_PUBKEY);
        EXAMPLE_CREATOR_VAULT_PUBKEY // This is imported from pump_instruction_builders
    } else {
        info!("  Creator (payer) {} does not match the example creator. Deriving creator_vault PDA using payer's pubkey.", payer_pubkey);
        // Derive the creator_vault PDA using the payer's (creator's) pubkey
        let (derived_vault_pda, _bump) = find_creator_vault_pda(&payer_pubkey)
            .context("Failed to derive creator_vault_pda for the current payer/creator")?;
        info!("  Derived Creator Vault PDA for this launch: {}", derived_vault_pda);
        derived_vault_pda
    };
    info!("  Using Actual Creator Vault Pubkey for this launch: {}", actual_creator_vault_pubkey);

    let zombie_keypairs: Vec<Keypair> = zombie_wallet_data.iter().map(|wallet_info| {
        let bytes = bs58::decode(&wallet_info.private_key)
            .into_vec()
            .map_err(|e| PumpfunError::Wallet(format!(
                "Failed to decode base58 zombie private key for pubkey {}: {}",
                wallet_info.public_key, e
            )))?;
        Keypair::from_bytes(&bytes)
             .map_err(|e| PumpfunError::Wallet(format!(
                "Failed to create zombie keypair from bytes for pubkey {}: {}",
                wallet_info.public_key, e
            )))
            .map_err(anyhow::Error::from) 
    }).collect::<Result<Vec<Keypair>, anyhow::Error>>()
      .context("Failed to convert zombie wallet data to keypairs")?;

    info!("🧟 Loaded {} initial Zombie Keypairs before filtering.", zombie_keypairs.len());

    // --- Load Parent Wallet for exclusion ---
    let settings = AppSettings::load();
    let parent_keypair = load_keypair_from_string(&settings.parent_wallet_private_key, "Parent (for exclusion)")
        .context("Failed to load parent keypair for exclusion")?;
    let parent_pubkey_for_exclusion = parent_keypair.pubkey();
    info!("  Parent wallet for exclusion: {}", parent_pubkey_for_exclusion);
    info!("  Minter/Payer wallet for exclusion: {}", payer_pubkey);

    // --- Filter zombie_keypairs to exclude minter (payer) and parent ---
    let original_zombie_count = zombie_keypairs.len();
    let zombie_keypairs: Vec<Keypair> = zombie_keypairs.into_iter().filter(|kp| {
        let kp_pubkey = kp.pubkey();
        let is_minter = kp_pubkey == payer_pubkey;
        let is_parent = kp_pubkey == parent_pubkey_for_exclusion;
        if is_minter {
            info!("  Excluding minter wallet {} from zombie list.", kp_pubkey);
        }
        if is_parent {
            info!("  Excluding parent wallet {} from zombie list.", kp_pubkey);
        }
        !is_minter && !is_parent
    }).collect();
    
    info!("🧟 Filtered Zombie Keypairs: {} remaining out of {} ({} excluded).", zombie_keypairs.len(), original_zombie_count, original_zombie_count - zombie_keypairs.len());

    if zombie_keypairs.is_empty() && zombie_buy_sol > 0.0 { // Only error if zombie buys were intended
        warn!("No zombie wallets remaining after filtering for bundle, but zombie buy amount > 0. This might be intended if all zombies were the minter/parent.");
        // Depending on desired behavior, you might not want to error out here if filtering is expected.
        // For now, keeping the original error logic but with a warning.
        // return Err(PumpfunError::Wallet("No zombie wallets remaining after filtering for bundle, but zombie buy amount > 0.".to_string()).into());
    }
    

    let rpc_url = "https://api.mainnet-beta.solana.com".to_string();
    let commitment_config = get_commitment_config();
    let rpc_client = Arc::new(RpcClient::new_with_commitment(rpc_url.clone(), commitment_config));
    info!("⚡ RPC Client initialized for URL: {} with commitment: {:?}", rpc_url, commitment_config.commitment);
    
    let alt_lookup: Vec<AddressLookupTableAccount> = if lookup_table_str.is_empty() {
        info!("  No ALT address provided. Transactions will be compiled without ALT.");
        Vec::new()
    } else {
        let lookup_table_pubkey = Pubkey::from_str(&lookup_table_str)
            .map_err(|e| PumpfunError::InvalidInput(format!("Invalid lookup table address '{}': {}", lookup_table_str, e)))
            .context("Failed to parse lookup table address")?;
        info!("📖 Fetching ALT Account: {}", lookup_table_pubkey);
        let alt_account_data = rpc_client.get_account_data(&lookup_table_pubkey).await
            .context(format!("Failed to fetch ALT account data for {}", lookup_table_pubkey))?;
        let alt_state = AddressLookupTable::deserialize(&mut &alt_account_data[..])
             .map_err(|e| PumpfunError::Deserialization(format!("Failed to deserialize ALT account {}: {}", lookup_table_pubkey, e)))?;
        let lookup_table_account = AddressLookupTableAccount {
            key: lookup_table_pubkey,
            addresses: alt_state.addresses.to_vec(),
        };
        info!("  ✅ ALT fetched and deserialized successfully ({} addresses).", lookup_table_account.addresses.len());
        vec![lookup_table_account] 
    };

    // 1d. Load or Generate Mint Keypair
    let mint_keypair = match mint_keypair_path {
        Some(path) => {
            info!("🔑 Loading mint keypair from file: {}", path);
            read_keypair_file(&path).map_err(|e|
                PumpfunError::Wallet(format!("Failed to read mint keypair file '{}': {}", path, e))
            )?
        }
        None => {
            info!("💎 Generating new random mint keypair...");
            Keypair::new()
        }
    };
    let mint_pubkey = mint_keypair.pubkey();
    info!("  Using Token Mint Keypair with Pubkey: {}", mint_pubkey);

    let (bonding_curve_pubkey, _) = find_bonding_curve_pda(&mint_pubkey)
        .context("Failed to calculate bonding curve PDA")?;
    let (metadata_pubkey, _) = find_metadata_pda(&mint_pubkey)
        .context("Failed to calculate metadata PDA")?;
    info!("  Bonding Curve PDA: {}", bonding_curve_pubkey);
    info!("  Metadata PDA: {}", metadata_pubkey);

    let payer_ata_for_tx1 = get_associated_token_address(&payer_pubkey, &mint_pubkey); // Renamed for clarity
    info!("  Payer ATA (for Tx1 dev buy): {}", payer_ata_for_tx1);

    const JITO_TIP_SOL: f64 = 0.01;
    const JITO_TIP_ACCOUNT_STR: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"; // Keep hardcoded tip account
    let mut tip_account_pubkey: Option<Pubkey> = None;
    let jito_tip_lamports: u64 = sol_to_lamports(jito_actual_tip_sol); // Use new parameter

    match Pubkey::from_str(JITO_TIP_ACCOUNT_STR) {
        Ok(pk) => {
            tip_account_pubkey = Some(pk);
            info!("  💸 Jito Tip Prepared: Account={}, Amount={} lamports ({:.9} SOL)", pk, jito_tip_lamports, jito_actual_tip_sol);
        }
        Err(_) => {
            warn!("Invalid hardcoded JITO_TIP_ACCOUNT_STR: '{}'. No tip will be added. THIS SHOULD NOT HAPPEN WITH A VALID HARDCODED KEY.", JITO_TIP_ACCOUNT_STR);
            // Consider returning an error here if a hardcoded key is invalid, as it's a compile-time issue.
            // For now, it will just skip the tip.
        }
    }

    info!("Fetching Global State...");
    let global_state_data: Vec<u8> = rpc_client.get_account_data(&GLOBAL_STATE_PUBKEY).await
        .context("Failed to fetch Global State account data")?;
    info!("  Raw Global State Account Data Length: {} bytes", global_state_data.len());
    const DISCRIMINATOR_LEN: usize = 8;
    if global_state_data.len() <= DISCRIMINATOR_LEN {
        return Err(anyhow!("Global state account data too short to contain discriminator and data ({} bytes)", global_state_data.len()));
    }
    let global_state = GlobalState::deserialize(&mut &global_state_data[DISCRIMINATOR_LEN..])
        .context("Failed to deserialize Global State")?;
    info!("  DEBUG: Global State Initial Reserves - SOL: {}, Tokens: {}",
          global_state.initial_virtual_sol_reserves,
          global_state.initial_virtual_token_reserves);
    if global_state.initial_virtual_sol_reserves == 0 || global_state.initial_virtual_token_reserves == 0 {
        warn!("  DEBUG: WARNING - Initial reserves fetched from Global State are zero!");
    }
    info!("  Deserialized Global State: {:?}", global_state); 
    info!("  ✅ Global State fetched. Initial Virtual SOL: {}, Initial Virtual Tokens: {}, Fee Basis Points: {}",
        global_state.initial_virtual_sol_reserves, global_state.initial_virtual_token_reserves, global_state.fee_basis_points);

    info!("\n☁️ Uploading Metadata...");
    let _ = sender.send(LaunchStatus::UploadingMetadata); 
    let metadata_uri = {
        let twitter = env::var("TOKEN_TWITTER").ok();
        let telegram = env::var("TOKEN_TELEGRAM").ok();
        let website = env::var("TOKEN_WEBSITE").ok();
        let metadata = PumpfunMetadata {
            name: name.clone(), 
            symbol: symbol.clone(),
            description: description.clone(), 
            image: Some(image_url.clone()), 
            twitter, telegram, website, show_name: true, 
        };
        upload_metadata_to_ipfs(&metadata).await?
    };
    info!("  ✅ Metadata uploaded: {}", metadata_uri);

    let mut bundle_transactions: Vec<VersionedTransaction> = Vec::new();
    let latest_blockhash: Hash = rpc_client.get_latest_blockhash().await
        .context("Failed to get latest blockhash for bundle")?;
    info!("🧱 Using blockhash {} for all bundle transactions.", latest_blockhash);

    let mut bonding_curve_state_after_tx1: Option<BondingCurveAccount> = None;
    let mut _mint_account_data_after_tx1_sim: Option<Vec<u8>> = None;  // Prefixed as unused
    let mut _mint_account_lamports_after_tx1_sim: Option<u64> = None; // Prefixed as unused

    info!("-----------------------------------------");
    info!("2. Build/Sign/Simulate Tx 1: Create + Dev Buy");
    info!("-----------------------------------------");

    fn ensure_in_alt(alt: &mut AddressLookupTableAccount, pk: &Pubkey, pk_name: &str) {
        if !alt.addresses.contains(pk) {
            alt.addresses.push(*pk);
            info!("    (ALT Patch) Added {} ({}) to ALT addresses.", pk_name, pk);
        }
    }

    info!("--- Pre-Tx1 Pubkey Verification ---");
    info!("  Payer/Minter (payer_pubkey): {}", payer_pubkey);
    info!("  Token Mint (mint_pubkey): {}", mint_pubkey);
    info!("  Payer ATA for Tx1 (payer_ata_for_tx1): {}", payer_ata_for_tx1);
    info!("  Bonding Curve PDA (bonding_curve_pubkey): {}", bonding_curve_pubkey);
    info!("  Metadata PDA (metadata_pubkey): {}", metadata_pubkey);
    // The actual_creator_vault_pubkey is logged when it's defined above.
    info!("  (Reminder) Actual Creator Vault for all buys related to this launch: {}", actual_creator_vault_pubkey);
    info!("------------------------------------");


    let mut instructions_tx1: Vec<Instruction> = vec![];
    let mut tx1_includes_jito_tip = false;
    let mut tx1_cu_limit = TARGET_CREATE_BUY_COMPUTE_LIMIT; // Base CU limit for Tx1

    // Determine if Jito tip will be included in Tx1 and adjust CU limit
    if let (Some(_tip_pk), tip_amount) = (tip_account_pubkey, jito_tip_lamports) {
        if tip_amount > 0 {
            const JITO_TIP_INSTRUCTION_CU_COST_ESTIMATE: u32 = 2_000; // Estimated CU for a system transfer
            tx1_cu_limit = tx1_cu_limit.saturating_add(JITO_TIP_INSTRUCTION_CU_COST_ESTIMATE);
            tx1_includes_jito_tip = true;
            info!("  Jito tip will be included in Tx1. Adjusted Tx1 CU Limit to: {}", tx1_cu_limit);
        }
    }

    let tx1_signed = {
        info!("  Targeting Tx1 CU Limit: {}", tx1_cu_limit);
        
        let mut create_payer_ata_ix_for_tx1: Option<Instruction> = None; // Renamed for clarity
        if rpc_client.get_account(&payer_ata_for_tx1).await.is_err() {
            info!("   Payer ATA {} for Tx1 dev buy does not exist. Will add creation instruction using spl_associated_token_account helper.", payer_ata_for_tx1);
            create_payer_ata_ix_for_tx1 = Some(
                create_associated_token_account_idempotent(
                    &payer_pubkey, // Payer (funder)
                    &payer_pubkey, // Owner of the ATA
                    &mint_pubkey,  // Mint
                    &spl_token::ID // Token program ID
                )
            );
        } else {
            info!("  Payer ATA {} for Tx1 dev buy already exists.", payer_ata_for_tx1);
        }

        instructions_tx1.push(ComputeBudgetInstruction::set_compute_unit_limit(tx1_cu_limit));
        instructions_tx1.push(ComputeBudgetInstruction::set_compute_unit_price(main_tx_priority_fee_micro_lamports)); // Use new parameter
        info!("  Set Tx1 CU Limit: {}, CU Price (microLamports): {}", tx1_cu_limit, main_tx_priority_fee_micro_lamports);

        let create_instruction = {
            info!("  🏗️ Building create instruction..."); 
            build_pump_create_instruction(&payer_pubkey, &mint_pubkey, &bonding_curve_pubkey, &metadata_pubkey, &metadata_uri, &name, &symbol).context("Failed to build create token instruction")?
        };
        instructions_tx1.push(create_instruction);

        info!("  🛠️ Building extend_account instruction...");
        let extend_account_instruction = build_pump_extend_account_instruction(
            &bonding_curve_pubkey, 
            &payer_pubkey,         
        )
        .context("Failed to build extend_account instruction")?;
        instructions_tx1.push(extend_account_instruction);

        if let Some(ix) = create_payer_ata_ix_for_tx1 {
            info!("  Adding Payer ATA creation instruction for Tx1 dev buy.");
            instructions_tx1.push(ix);
        }

        let dev_buy_instruction = { 
            info!("  🛒 Building dev buy instruction..."); 
            let dev_buy_lamports = sol_to_lamports(dev_buy_sol); 
            if dev_buy_lamports == 0 {
                warn!("Dev buy amount is 0 SOL, skipping instruction build.");
                None
            } else {
                let initial_expected_tokens_dev = calculate_tokens_out(
                    dev_buy_lamports,
                    global_state.initial_virtual_sol_reserves,
                    global_state.initial_virtual_token_reserves,
                    global_state.fee_basis_points, 
                ).context("Failed initial token calculation for dev buy")?;
                let max_sol_cost_dev = apply_slippage_to_sol_cost(dev_buy_lamports, slippage_bps);
                let mut min_tokens_out_dev = apply_slippage_to_tokens_out(
                    initial_expected_tokens_dev,
                    slippage_bps
                );
                if min_tokens_out_dev == 0 {
                    warn!("Calculated min_tokens_out_dev was 0 after slippage for dev buy ({} lamports, initial expected {}), setting to 1.", dev_buy_lamports, initial_expected_tokens_dev);
                    min_tokens_out_dev = 1;
                }
                info!("    Dev Buy: Min Tokens={} (initial state - slippage), Max SOL Cost={} lamports (input + slippage)", min_tokens_out_dev, max_sol_cost_dev);
                info!("    Dev Buy: Min Tokens={} (initial state - slippage), Max SOL Cost={} lamports (input + slippage)", min_tokens_out_dev, max_sol_cost_dev);
                // Use the `actual_creator_vault_pubkey` determined earlier for the dev buy.
                Some(build_pump_buy_instruction(
                    &payer_pubkey,
                    &mint_pubkey,
                    &bonding_curve_pubkey,
                    &actual_creator_vault_pubkey, // Use the globally determined creator vault
                    min_tokens_out_dev,
                    max_sol_cost_dev
                ).context("Failed to build dev buy instruction")?)
            }
        };

        if let Some(mut instruction) = dev_buy_instruction { // Make instruction mutable
            // Ensure mint_pubkey in dev_buy_instruction is consistent with its role in Tx1
            // (signer due to mint_keypair signing Tx1, writable due to create_instruction)
            for acc_meta in instruction.accounts.iter_mut() {
                if acc_meta.pubkey == mint_pubkey {
                    if !acc_meta.is_signer || !acc_meta.is_writable {
                        info!("    (Consistency Fix) Updating dev_buy_instruction's AccountMeta for mint_pubkey {} to signer=true, writable=true", mint_pubkey);
                        acc_meta.is_signer = true;
                        acc_meta.is_writable = true;
                    }
                    break;
                }
            }
            instructions_tx1.push(instruction);
        }

        // Conditionally add Jito tip instruction to Tx1
        if tx1_includes_jito_tip {
            if let (Some(tip_pk), tip_amount) = (tip_account_pubkey, jito_tip_lamports) {
                // This inner check for tip_amount > 0 is redundant due to the outer one, but safe.
                if tip_amount > 0 {
                    info!("  💸 Adding Jito Tip instruction to Tx1: {} lamports to {}", tip_amount, tip_pk);
                    let tip_ix = system_instruction::transfer(&payer_pubkey, &tip_pk, tip_amount);
                    instructions_tx1.push(tip_ix);
                }
            }
        }

        let mut alt_lookup_mut = alt_lookup.clone(); // Clone for potential modification if ALT is used
        let alt_for_tx1_compile: &[AddressLookupTableAccount];

        if tx1_includes_jito_tip {
            info!("  Tx1 includes Jito tip, compiling WITHOUT ALT.");
            alt_for_tx1_compile = &[];
        } else {
            info!("  Tx1 does not include Jito tip, attempting to use provided ALT.");
            if !alt_lookup_mut.is_empty() {
                 if let Some(alt) = alt_lookup_mut.get_mut(0) {
                    ensure_in_alt(alt, &payer_pubkey, "payer_pubkey");
                    ensure_in_alt(alt, &mint_pubkey, "mint_pubkey");
                    ensure_in_alt(alt, &payer_ata_for_tx1, "payer_ata_for_tx1");
                    ensure_in_alt(alt, &bonding_curve_pubkey, "bonding_curve_pubkey");
                    ensure_in_alt(alt, &metadata_pubkey, "metadata_pubkey");
                    info!("(DEBUG) Tx1 ALT (mutated) before try_compile contains {} addresses: {:#?}",
                          alt.addresses.len(),
                          &alt.addresses);
                } else {
                    // This case should not be reached if !alt_lookup_mut.is_empty()
                    warn!("ALT was provided but alt_lookup_mut.get_mut(0) failed unexpectedly.");
                }
                alt_for_tx1_compile = alt_lookup_mut.as_slice();
            } else {
                info!("  No ALT provided or ALT list is empty.");
                alt_for_tx1_compile = &[];
            }
        }

        let message_v0_tx1 = MessageV0::try_compile(&payer_pubkey, &instructions_tx1, alt_for_tx1_compile, latest_blockhash)
            .map_err(|e| PumpfunError::Build(format!("Failed to compile Tx1 message: {:?} using ALT: {} ({} addresses)", e, !alt_for_tx1_compile.is_empty(), alt_for_tx1_compile.len())))?;
        let versioned_message = VersionedMessage::V0(message_v0_tx1);
        let message_bytes = versioned_message.serialize();
        let payer_signature = payer_keypair.sign_message(&message_bytes);
        let mint_signature = mint_keypair.sign_message(&message_bytes);
        
        Result::<VersionedTransaction, PumpfunError>::Ok(VersionedTransaction {
            signatures: vec![payer_signature, mint_signature],
            message: versioned_message
        })
    }?;
    info!("  ✅ Tx1 (Create + Dev Buy) built and signed.");
    match bincode::serialize(&tx1_signed) {
        Ok(tx_bytes) => info!("  Tx1 Serialized Size: {} bytes.", tx_bytes.len()),
        Err(e) => warn!("  Failed to serialize Tx1 for size logging: {}", e),
    }

    let tx1_for_simulation = {
        info!("  Re-compiling Tx1 for simulation with fresh blockhash and potentially modified ALT...");
        let sim_blockhash = rpc_client.get_latest_blockhash().await
            .context("Failed to get latest blockhash for Tx1 simulation")?;
        info!("  Using simulation blockhash: {}", sim_blockhash);
        let instructions_tx1_sim = instructions_tx1.clone();
        let mut alt_lookup_sim_mut = alt_lookup.clone(); // Clone for potential modification if ALT is used for sim
        let alt_for_tx1_sim_compile: &[AddressLookupTableAccount];

        if tx1_includes_jito_tip { // Use the same flag as for the actual transaction
            info!("  Tx1 (sim) includes Jito tip, compiling WITHOUT ALT.");
            alt_for_tx1_sim_compile = &[];
        } else {
            info!("  Tx1 (sim) does not include Jito tip, attempting to use provided ALT.");
            if !alt_lookup_sim_mut.is_empty() {
                if let Some(alt) = alt_lookup_sim_mut.get_mut(0) {
                    ensure_in_alt(alt, &payer_pubkey, "payer_pubkey (sim)");
                    ensure_in_alt(alt, &mint_pubkey, "mint_pubkey (sim)");
                    ensure_in_alt(alt, &payer_ata_for_tx1, "payer_ata_for_tx1 (sim)");
                    ensure_in_alt(alt, &bonding_curve_pubkey, "bonding_curve_pubkey (sim)");
                    ensure_in_alt(alt, &metadata_pubkey, "metadata_pubkey (sim)");
                    info!("(DEBUG) Tx1 Sim ALT (mutated) before try_compile contains {} addresses: {:#?}",
                          alt.addresses.len(),
                          &alt.addresses);
                } else {
                     warn!("ALT was provided for sim but alt_lookup_sim_mut.get_mut(0) failed unexpectedly.");
                }
                alt_for_tx1_sim_compile = alt_lookup_sim_mut.as_slice();
            } else {
                info!("  No ALT provided for sim or ALT list is empty.");
                alt_for_tx1_sim_compile = &[];
            }
        }
        
        // Debug logging for simulation instructions
        for (i, ix) in instructions_tx1_sim.iter().enumerate() {
            info!("(DEBUG) Tx1 Sim Instruction {}: program_id={}, accounts={:?}", i, ix.program_id, ix.accounts.iter().map(|am| (am.pubkey, am.is_signer, am.is_writable)).collect::<Vec<_>>());
        }
        // Debug logging for account metas consistency (optional, can be verbose)
        // let mut seen_accounts_sim = std::collections::HashMap::new();
        // for (ix_idx, ix) in instructions_tx1_sim.iter().enumerate() {
        //     for (ac_idx, acct) in ix.accounts.iter().enumerate() {
        //         if let Some(prev_flags) = seen_accounts_sim.insert(acct.pubkey, (acct.is_signer, acct.is_writable)) {
        //             if prev_flags != (acct.is_signer, acct.is_writable) {
        //                 warn!("⚠️ (DEBUG) Tx1 Sim Account {} in Ix {}/Acc {} has conflicting metas: was {:?}, now {:?}",
        //                       acct.pubkey, ix_idx, ac_idx, prev_flags, (acct.is_signer, acct.is_writable));
        //             }
        //         }
        //     }
        // }

        let sim_message_v0_tx1 = MessageV0::try_compile(&payer_pubkey, &instructions_tx1_sim, alt_for_tx1_sim_compile, sim_blockhash)
            .map_err(|e| PumpfunError::Build(format!("Failed to compile Tx1 message for simulation: {:?} using ALT: {} ({} addresses)", e, !alt_for_tx1_sim_compile.is_empty(), alt_for_tx1_sim_compile.len())))?;
        let versioned_message_sim = VersionedMessage::V0(sim_message_v0_tx1);
        let message_bytes_sim = versioned_message_sim.serialize();
        let payer_signature_sim = payer_keypair.sign_message(&message_bytes_sim);
        let mint_signature_sim = mint_keypair.sign_message(&message_bytes_sim);
        Result::<VersionedTransaction, PumpfunError>::Ok(VersionedTransaction {
            signatures: vec![payer_signature_sim, mint_signature_sim],
            message: versioned_message_sim
        })
    }?;
    
    // For Tx1 simulation, also fetch the state of payer_ata_for_tx1 if it's created
    let accounts_to_fetch_for_tx1_sim = Some(vec![
        bonding_curve_pubkey,
        mint_pubkey,
        payer_ata_for_tx1 // Add payer's ATA
    ]);

    let simulation_result_outer: Result<solana_client::rpc_response::RpcSimulateTransactionResult, anyhow::Error> = match simulate_and_check(
        &rpc_client,
        &tx1_for_simulation,
        "Tx1 (Create+DevBuy)",
        accounts_to_fetch_for_tx1_sim
        // No 5th argument, as simulate_and_check now takes 4.
    ).await {
        Ok(sim_data) => {
            info!("    ✅ Simulation successful for Tx1.");
            let _ = sender.send(LaunchStatus::SimulationSuccess("Tx1 (Create+DevBuy)".to_string()));
            Ok(sim_data)
        }
        Err(e) => {
            let err_msg = format!("Simulation failed for Tx1 (Create+DevBuy): {}", e);
            error!("{}", err_msg);
            let _ = sender.send(LaunchStatus::SimulationFailed("Tx1 (Create+DevBuy)".to_string(), err_msg.clone()));
            let err_context = anyhow::Error::new(e).context(err_msg);
            Err(err_context)
        }
    };

    bundle_transactions.push(tx1_signed);

    // Store simulated account states from Tx1 to use in subsequent simulations
    let mut override_accounts_for_simulation: Vec<(Pubkey, Account)> = Vec::new();

    if let Ok(ref sim_data_for_extraction) = simulation_result_outer {
        if let Some(ref accounts_data) = sim_data_for_extraction.accounts {
            // The accounts_to_fetch_for_tx1_sim was Some(vec![bonding_curve_pubkey, mint_pubkey, payer_ata_for_tx1])
            // So, accounts_data should correspond to these in order.
            let pubkeys_fetched_in_tx1_sim = vec![bonding_curve_pubkey, mint_pubkey, payer_ata_for_tx1];

            for (idx, pk_ref) in pubkeys_fetched_in_tx1_sim.iter().enumerate() {
                if let Some(Some(ui_account)) = accounts_data.get(idx) {
                    if ui_account.lamports > 0 { // Only consider accounts that have lamports (exist)
                        if let UiAccountData::Binary(data_str, UiAccountEncoding::Base64) = &ui_account.data {
                            match BASE64_STANDARD.decode(data_str) {
                                Ok(data_bytes) => {
                                    let owner_pubkey = ui_account.owner.parse().unwrap_or_else(|_| {
                                        warn!("Failed to parse owner PK string '{}' for account {}, defaulting to system_program.", ui_account.owner, pk_ref);
                                        solana_sdk::system_program::id()
                                    });
                                    let account_override = Account {
                                        lamports: ui_account.lamports,
                                        data: data_bytes,
                                        owner: owner_pubkey,
                                        executable: ui_account.executable,
                                        rent_epoch: ui_account.rent_epoch,
                                    };
                                    override_accounts_for_simulation.push((*pk_ref, account_override));
                                    info!("  Stored account state for {} from Tx1 sim for override (Owner: {}).", pk_ref, owner_pubkey);
                                }
                                Err(e) => {
                                    warn!("  Failed to decode base64 account data for {} from Tx1 sim: {}. Not adding to overrides.", pk_ref, e);
                                }
                            }
                        } else {
                            warn!("Account data for {} from Tx1 sim is not in Base64 format. Not adding to overrides. Data: {:?}", pk_ref, ui_account.data);
                        }
                    } else {
                        info!("Account {} from Tx1 sim has 0 lamports. Not adding to overrides.", pk_ref);
                    }
                } else {
                    warn!("Account data for {} (index {}) is None or missing in Tx1 simulation result accounts array.", pk_ref, idx);
                }
            }
        } else {
            warn!("Tx1 Simulation result missing account data, cannot extract states for subsequent simulations.");
        }
    }

    info!("Attempting to extract bonding curve state from Tx1 simulation result...");
    bonding_curve_state_after_tx1 = { // Assign to the outer variable
        match simulation_result_outer {
            Ok(ref sim_data) => { 
                if let Some(ref accounts) = sim_data.accounts {
                    if let Some(Some(bonding_curve_account_info)) = accounts.get(0) {
                        let account_data_base64 = match &bonding_curve_account_info.data {
                            UiAccountData::Binary(data_str, UiAccountEncoding::Base64) => {
                                info!("  Bonding curve account data found in simulation (Base64 string length: {}).", data_str.len());
                                Some(data_str.clone())
                            },
                            _ => {
                                warn!("  Bonding curve account data from simulation is not in Base64 format. Data: {:?}", bonding_curve_account_info.data);
                                None
                            }
                        };
                        if let Some(data_str) = account_data_base64 {
                            match BASE64_STANDARD.decode(data_str) {
                                Ok(bonding_curve_account_data) => {
                                    info!("  Decoded bonding curve data length: {} bytes.", bonding_curve_account_data.len());
                                    const BONDING_CURVE_DISCRIMINATOR_LEN: usize = 8;
                                    if bonding_curve_account_data.len() > BONDING_CURVE_DISCRIMINATOR_LEN {
                                        match BondingCurveAccount::deserialize(&mut &bonding_curve_account_data[BONDING_CURVE_DISCRIMINATOR_LEN..]) {
                                            Ok(state) => {
                                                info!("  ✅ Successfully deserialized BondingCurveAccount from Tx1 simulation: Virtual SOL={}, Virtual Tokens={}, Real SOL={}, Real Tokens={}", 
                                                    state.virtual_sol_reserves, state.virtual_token_reserves, state.real_sol_reserves, state.real_token_reserves);
                                                Some(state)
                                            }
                                            Err(e) => {
                                                error!("  ❌ Failed to deserialize BondingCurveAccount from Tx1 simulation: {:?}. Data (hex): {}", e, hex::encode(&bonding_curve_account_data));
                                                None
                                            }
                                        }
                                    } else {
                                        error!("  Bonding curve account data from simulation is too short after base64 decode ({} bytes) to contain discriminator and data.", bonding_curve_account_data.len());
                                        None
                                    }
                                }
                                Err(e) => {
                                    error!("  Failed to decode base64 bonding curve account data from Tx1 simulation: {}", e);
                                    None
                                }
                            }
                        } else {
                            None 
                        }
                    } else {
                        warn!("  Bonding curve account data not found at expected index 0 in Tx1 simulation result accounts array.");
                        None
                    }
                } else {
                    warn!("  Tx1 Simulation result missing account data for bonding curve extraction.");
                    None
                }
            }
            Err(ref e) => { 
                error!("  Tx1 simulation failed, cannot extract bonding curve state. Error: {:?}", e);
                None
            }
        }
    }; 

    if bonding_curve_state_after_tx1.is_none() {
        let error_message = "Tx1 Simulation result missing account data for bonding curve extraction or deserialization failed".to_string();
        error!("{}", error_message);
        let _ = sender.send(LaunchStatus::Error(error_message.clone()));
        return Err(PumpfunError::TransactionSimulation(error_message).into());
    }
    let current_simulated_curve_state = bonding_curve_state_after_tx1.clone().unwrap(); // Safe due to check above, cloned to avoid move
    
    info!("  Initial current_simulated_curve_state (after Tx1): Virtual SOL={}, Virtual Tokens={}, Real SOL={}, Real Tokens={}",
        current_simulated_curve_state.virtual_sol_reserves, current_simulated_curve_state.virtual_token_reserves,
        current_simulated_curve_state.real_sol_reserves, current_simulated_curve_state.real_token_reserves);

    info!("-----------------------------------------");
    info!("3. Build/Sign Zombie Buy Transactions (Atomic Style with Sequential Prediction)");
    info!("-----------------------------------------");

    // Initialize the mutable state for sequential prediction
    let mut current_simulated_curve_state = match bonding_curve_state_after_tx1.as_ref() { // Use .as_ref() to borrow
        Some(state_ref) => { // state_ref is now &BondingCurveAccount
            info!("  Using bonding curve state from Tx1 simulation as starting point for sequential prediction.");
            state_ref.clone() // Clone the inner value to get an owned BondingCurveAccount
        }
        None => {
            error!("Cannot proceed with zombie buys, failed to get bonding curve state after Tx1 simulation.");
            return Err(PumpfunError::Build("Failed to get bonding curve state after Tx1 simulation.".to_string()).into());
        }
    };

    let zombie_lamports_per_wallet = sol_to_lamports(zombie_buy_sol);

    // Use fixed chunk size like atomic_buy.rs (e.g., 4 transactions max for buys)
    // --- Set Max Zombies Per Chunk --- 
    // Override calculation and set directly to 4 as requested.
    // WARNING: This might exceed CU or transaction size limits.
    // Override calculation and set directly to 4 as requested.
    // WARNING: This might exceed CU or transaction size limits.
    let max_zombies_per_chunk = ZOMBIE_BUY_BATCH_SIZE; // Use the new batch size
    info!("Max zombies per chunk set to: {} (using ZOMBIE_BUY_BATCH_SIZE)", max_zombies_per_chunk);
    // ---------------------------------

    for (chunk_index, zombie_chunk) in zombie_keypairs.chunks(max_zombies_per_chunk).enumerate() {
        let tx_num = chunk_index + 2; // Start zombie Txs from #2
        let tx_label = format!("Tx{} (Zombies {}-{})", tx_num, chunk_index * max_zombies_per_chunk + 1, (chunk_index + 1) * max_zombies_per_chunk);
        info!("  🧱 Building {} for {} zombie wallets...", tx_label, zombie_chunk.len());
        let _ = sender.send(LaunchStatus::PreparingTx(tx_label.clone())); // <-- Send status

        info!("    Awaiting result of async block for Tx #{}...", tx_num); // <-- Log before await
        // Updated return type: (SignedTx, BuyInstructionCount)
        let build_result: Result<Option<(VersionedTransaction, usize)>, anyhow::Error> = async {
            info!("      Entering async block to build Tx #{}...", tx_num); // <-- Log entry
            if zombie_chunk.is_empty() {
                warn!("      Skipping Tx #{} because zombie chunk is empty.", tx_num);
                return Ok(None);
            }

            let mut instructions: Vec<Instruction> = Vec::with_capacity(max_zombies_per_chunk);

            // --- Designate Fee Payer for this Chunk ---
            // Use the first zombie in the chunk as the fee payer
            let fee_payer_keypair = &zombie_chunk[0]; // Borrow the keypair reference
            let fee_payer_pubkey = fee_payer_keypair.pubkey();
            info!("        Designating fee payer for Tx #{}...", tx_num); // <-- Log fee payer step
            info!("          Fee Payer Pubkey: {}", fee_payer_pubkey);

            // --- Set Compute Budget ---
            instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(ZOMBIE_CHUNK_COMPUTE_LIMIT));
            // Use the new constant for zombie tx priority fee
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(ZOMBIE_TX_PRIORITY_FEE_LAMPORTS));
            // Removed the check for priority_fee > 0 and the warning, as we always set it now.

            let mut buy_instructions_added = 0;
            // Signers now start with just the fee payer for this chunk
            let mut signers_for_tx: Vec<&Keypair> = vec![fee_payer_keypair];

            // --- Process zombies in this chunk ---
            for zombie_keypair in zombie_chunk {
                let zombie_pk = zombie_keypair.pubkey();
                info!("        Processing Zombie (Pubkey: {})...", zombie_pk); // <-- Log zombie start

                // Check and potentially add ATA creation instruction
                info!("          Checking Zombie ATA existence..."); // <-- Log ATA check step
                let zombie_ata = get_associated_token_address(&zombie_pk, &mint_pubkey);
                // Check ATA existence using the main RPC client (doesn't cost the zombie)
                info!("          Calling rpc_client.get_account for ATA: {}", zombie_ata); // <-- Log before RPC
                let get_account_future = rpc_client.get_account(&zombie_ata);
                let ata_exists = match timeout(Duration::from_secs(15), get_account_future).await {
                    Ok(Ok(_)) => { // Timeout succeeded, get_account succeeded
                        info!("          RPC get_account for ATA {} successful (Exists=true).", zombie_ata);
                        true
                    }
                    Ok(Err(_)) => { // Timeout succeeded, get_account failed (doesn't exist)
                        info!("          RPC get_account for ATA {} failed (Exists=false).", zombie_ata);
                        false
                    }
                    Err(_) => { // Timeout elapsed
                        warn!("          RPC get_account for ATA {} timed out after 15 seconds. Assuming it doesn't exist.", zombie_ata);
                        // Decide how to handle timeout: assume false or return error?
                        // Assuming false for now to potentially proceed with ATA creation.
                        false
                        // Alternatively, return an error:
                        // return Err(anyhow!("RPC timeout checking ATA existence for {}", zombie_ata));
                    }
                };
                // info!("          RPC get_account result for ATA {}: Exists={}", zombie_ata, ata_exists); // <-- Log after RPC (replaced by match logging)

                if !ata_exists {
                    info!("        ATA {} does not exist. Adding creation instruction funded by zombie.", zombie_ata);
                    // Use the standard spl_token::ID constant directly
                    instructions.push(create_associated_token_account_idempotent( // Use idempotent version
                        &zombie_pk,     // Zombie FUNDS its own ATA
                        &zombie_pk,     // Owner is zombie
                        &mint_pubkey,
                        &spl_token::ID, // Use the constant directly
                    ));
                    // Zombie needs to sign because it's the funder
                    signers_for_tx.push(zombie_keypair);
                } else {
                     info!("          ATA {} exists.", zombie_ata);
                }

                // --- Calculate Buy Params using *current* simulated state ---
                info!("          Calculating expected tokens using state: SOL={}, Tokens={}", current_simulated_curve_state.virtual_sol_reserves, current_simulated_curve_state.virtual_token_reserves); // <-- Log state before calc
                info!("        Calculating buy for {} using simulated state: SOL={}, Tokens={}",
                    zombie_pk, current_simulated_curve_state.virtual_sol_reserves, current_simulated_curve_state.virtual_token_reserves);

                let expected_tokens_zombie = calculate_tokens_out(
                     zombie_lamports_per_wallet,
                     current_simulated_curve_state.virtual_sol_reserves, // Use current state
                     current_simulated_curve_state.virtual_token_reserves, // Use current state
                     global_state.fee_basis_points,
                 ).context(format!("Failed token calculation for zombie {}", zombie_pk))?;

                info!("          Expected tokens (before slippage): {}", expected_tokens_zombie); // <-- Log calc result

                // Apply slippage only to SOL cost for safety against overpaying
                let max_sol_cost_zombie = apply_slippage_to_sol_cost(zombie_lamports_per_wallet, slippage_bps);

                // Calculate MINIMUM tokens expected based on calculated expected tokens and downward slippage
                 let min_tokens_after_slippage = apply_slippage_to_tokens_out(
                     expected_tokens_zombie,
                     slippage_bps
                 );

                info!("          Min Tokens Out (after slippage): {}, Max SOL Cost (after slippage): {}", min_tokens_after_slippage, max_sol_cost_zombie); // <-- Log slippage results

                let mut min_tokens_out_zombie = min_tokens_after_slippage; // Start with slippage-adjusted value

                // Ensure minimum is at least 1 if SOL is being spent
                if zombie_lamports_per_wallet > 0 && min_tokens_out_zombie == 0 {
                    warn!("        Zombie {}: Calculated min_tokens_out_zombie was 0 after slippage, forcing to 1.", zombie_pk);
                    min_tokens_out_zombie = 1;
                }

                info!(
                    "        Zombie {}: FINAL PARAMS -> Min tokens={} , Max SOL cost={} lamports",
                    zombie_pk, min_tokens_out_zombie, max_sol_cost_zombie // Final values passed to build_pump_buy_instruction
                );
                info!("           (Min tokens calculated from current state)"); // Add context for min tokens source

                // Add Buy Instruction if valid (i.e., if min_tokens is > 0)
                if min_tokens_out_zombie > 0 {
                     info!("          Adding buy instruction for zombie {}", zombie_pk); // <-- Log adding instruction
                    let buy_ix = build_pump_buy_instruction(
                        &zombie_pk,
                        &mint_pubkey,
                        &bonding_curve_pubkey,
                        &actual_creator_vault_pubkey, // Use the globally determined creator vault
                        min_tokens_out_zombie, // Use the conservative minimum
                        max_sol_cost_zombie,
                    )?;
                    instructions.push(buy_ix);
                    signers_for_tx.push(zombie_keypair);
                    buy_instructions_added += 1;

                    // --- Predict and Update State for the *next* iteration ---
                    info!("          Predicting next curve state after this buy..."); // <-- Log before prediction
                    match predict_next_curve_state(
                        &current_simulated_curve_state, // Pass the current state
                        zombie_lamports_per_wallet,      // SOL amount input for *this* buy
                        global_state.fee_basis_points,  // Global fee setting
                    ) {
                        Ok(next_state) => {
                            info!("          Predicted next state: SOL={}, Tokens={}",
                                next_state.virtual_sol_reserves, next_state.virtual_token_reserves); // <-- Log prediction result
                            current_simulated_curve_state = next_state; // Update the state for the next zombie
                        }
                        Err(e) => {
                            // If prediction fails, we probably shouldn't continue building the bundle
                            // as subsequent calculations will be wrong.
                            error!("Failed to predict next curve state after buy for {}: {}", zombie_pk, e);
                            return Err(e).context(format!("State prediction failed for zombie {}", zombie_pk));
                        }
                    }

                } else {
                     warn!("Skipping buy instruction for zombie {} as min tokens is 0.", zombie_pk);
                }

            } // End loop through zombies in chunk
            info!("      Finished processing zombies for Tx #{}.", tx_num); // <-- Log end of zombie loop

            if buy_instructions_added == 0 {
                 warn!("Skipping Tx #{} because no valid buy instructions were generated.", tx_num);
                 return Ok(None); // Skip this chunk
            }

            // --- Compile the V0 message for this chunk ---
            info!("        Compiling message for Tx #{}...", tx_num); // <-- Log before compile

            // --- Workaround for simulation error ---
            // Temporarily add mint_pubkey to a local copy of the ALT Vec
            // to ensure it's resolvable during simulation, even if readonly.
            // --- Workaround for simulation error ---
            // Temporarily add mint_pubkey to a local copy of the ALT Vec
            // to ensure it's resolvable during simulation, even if readonly.
            let mut local_alt_lookup = alt_lookup.clone(); // Clone the original Vec<AddressLookupTableAccount>
            if let Some(alt) = local_alt_lookup.get_mut(0) { // Get mutable ref to the first ALT in the Vec
                if !alt.addresses.contains(&mint_pubkey) {
                    // This check is technically redundant if Tx1 succeeded,
                    // but good practice.
                    alt.addresses.push(mint_pubkey);
                    info!("        (Sim Fix) Added mint_pubkey to local ALT copy for Tx #{} compilation.", tx_num);
                }
            } else {
                // This case should ideally not happen if alt_lookup was initialized correctly
                warn!("        (Sim Fix) Could not get mutable reference to ALT in local copy for Tx #{}.", tx_num);
            }
            // --- End Workaround ---

            let message = MessageV0::try_compile(
                &fee_payer_pubkey, // Use the designated zombie fee payer for this chunk
                &instructions,
                local_alt_lookup.as_slice(), // Pass &[AddressLookupTableAccount] directly
                latest_blockhash,
            ).map_err(|e| PumpfunError::Build(format!("Failed to compile V0 message for tx #{}: {}", tx_num, e)))?;
            info!("        Message compiled for Tx #{}.", tx_num); // <-- Log after compile

            // --- Collect signers for THIS transaction ---
            // Ensure signers are unique (zombie might be fee payer AND funder/buyer)
            signers_for_tx.sort_by_key(|k| k.pubkey());
            signers_for_tx.dedup_by_key(|k| k.pubkey());
            info!("    Total unique signers for Tx #{}: {}", tx_num, signers_for_tx.len());

            // --- Create and sign the transaction for this chunk ---
             info!("        Signing Tx #{}...", tx_num); // <-- Log before sign
             let signers_refs: Vec<&dyn Signer> = signers_for_tx.iter().map(|kp| *kp as &dyn Signer).collect();
            let transaction = VersionedTransaction::try_new(
                VersionedMessage::V0(message),
                &signers_refs,
            ).map_err(|e| PumpfunError::Signing(format!("Failed to sign Tx #{}: {}", tx_num, e)))?;
            info!("        Tx #{} created and signed.", tx_num); // <-- Log after sign
            match bincode::serialize(&transaction) {
                Ok(tx_bytes) => info!("        Tx #{} Serialized Size: {} bytes.", tx_num, tx_bytes.len()),
                Err(e) => warn!("        Failed to serialize Tx #{} for size logging: {}", tx_num, e),
            }
            info!("      Exiting async block for Tx #{}.", tx_num); // <-- Log exit

            // Return the signed transaction and the count of buy instructions
            Ok(Some((transaction, buy_instructions_added)))

        }.await; // End async block definition and await

        // Process build_result: Add to bundle_transactions if Ok(Some), handle errors/skips
        match build_result {
            Ok(Some((tx_chunk_signed, buy_count))) => {
                info!("  ✅ {} ({} zombie buys) built and signed using sequential prediction.", tx_label, buy_count);

                // Simulate this zombie chunk if simulate_only is true, or for logging if not.
                // This simulation will be against the latest chain state + our local predictions for the curve.
                // It does not use override_accounts to chain simulations directly via RPC.
                let mut accounts_to_fetch_this_chunk_sim = vec![
                    bonding_curve_pubkey,
                    mint_pubkey,
                    actual_creator_vault_pubkey,
                    FEE_RECIPIENT_PUBKEY,
                ];
                for kp_in_chunk in zombie_chunk { // Iterate over the keypairs in the current chunk
                    let pk_in_chunk = kp_in_chunk.pubkey();
                    accounts_to_fetch_this_chunk_sim.push(pk_in_chunk);
                    accounts_to_fetch_this_chunk_sim.push(get_associated_token_address(&pk_in_chunk, &mint_pubkey));
                }
                accounts_to_fetch_this_chunk_sim.sort();
                accounts_to_fetch_this_chunk_sim.dedup();

                info!("    Simulating {} (primarily for logging and error checking)...", tx_label);
                match simulate_and_check(
                    &rpc_client,
                    &tx_chunk_signed,
                    &tx_label,
                    Some(accounts_to_fetch_this_chunk_sim.clone())
                    // No 5th argument for override_accounts
                ).await {
                    Ok(_sim_data_chunk) => { // sim_data_chunk is RpcSimulateTransactionResult
                        info!("    ✅ Simulation successful for {}.", tx_label);
                        let _ = sender.send(LaunchStatus::SimulationSuccess(tx_label.clone()));
                        // We don't use the result to update override_accounts_for_simulation here
                        // as simulate_and_check doesn't support pre-states for true sequential sim.
                        // The `current_simulated_curve_state` (local prediction) is already updated.
                    }
                    Err(e) => {
                        let err_msg = format!("Simulation failed for {}: {}", tx_label, e);
                        // error!("{}", err_msg); // Suppress console error for zombie buy simulation as it's deemed irrelevant
                        let _ = sender.send(LaunchStatus::SimulationFailed(tx_label.clone(), err_msg.clone()));
                        // If in simulate_only mode, we might want to continue to see other errors.
                        // If not in simulate_only, an actual send would likely fail, so consider halting.
                        if !simulate_only {
                            // Optionally, return an error here to stop the bundle sending
                            // return Err(anyhow::Error::new(e).context(err_msg));
                            warn!("Continuing bundle creation despite simulation failure for {} because not in simulate_only mode (actual send might fail).", tx_label);
                        }
                    }
                }
                // Add to bundle regardless of simulation outcome if not in simulate_only mode,
                // as the primary check is build success. Simulation is best-effort.
                if !simulate_only {
                    bundle_transactions.push(tx_chunk_signed);
                }
            }
            Ok(None) => {
                info!("{} skipped as planned (no buy instructions generated).", tx_label);
            }
            Err(e) => {
                let err_msg = format!("Failed to build/sign {}: {}", tx_label, e);
                error!("{}. Halting bundle creation.", err_msg);
                let _ = sender.send(LaunchStatus::BuildFailed(tx_label.clone(), err_msg.clone()));
                return Err(e).context(err_msg);
            }
        }

    } // End outer loop through zombie chunks

    // Jito Tip is now included in Tx1 if applicable.
    // The separate tip transaction logic has been removed.
    info!("-----------------------------------------");
    info!("4. Jito Tip (now part of Tx1 if applicable)");
    info!("-----------------------------------------");
    if tx1_includes_jito_tip {
        info!("  Jito tip was included in Tx1. No separate tip transaction needed.");
    } else {
        info!("  No Jito tip was included in Tx1 (tip amount was 0 or tip account not set).");
    }

    // --- Conditionally Send Bundle or Finish Simulation ---
    info!("  DEBUG: Checking simulate_only flag. Value = {}", simulate_only); // <-- Add logging
    if simulate_only {
        info!("---------------------------------");
        info!("🏁 Simulation Only Mode Enabled - EXECUTING SIMULATION BLOCK"); // <-- Add logging
        info!("---------------------------------");
        info!("✅ Bundle preparation logic completed successfully.");
        info!("   Total transactions built: {}", bundle_transactions.len());
        if bundle_transactions.len() <= 1 && !bundle_transactions.is_empty() {
             warn!("   Warning: Only Tx1 was built. Ensure zombie buys were intended and parameters/funding allow them.");
        } else if bundle_transactions.is_empty() {
             warn!("   Warning: No transactions were successfully built (e.g., Tx1 simulation failed). Check logs above.");
        }
        info!("   Bundle NOT sent to Jito.");
        println!("\n✅ Thorough simulation completed. Bundle would contain {} transactions.", bundle_transactions.len());

    } else {
        info!("---------------------------------");
        info!("🚀 Simulate Only Mode DISABLED - EXECUTING SEND BLOCK"); // <-- Add logging
        // --- Send Bundle via Jito (Original Logic) ---
        info!("---------------------------------");
        info!("5. Send Bundle via Jito");
        info!("---------------------------------");

        if bundle_transactions.is_empty() {
            error!("No valid transactions were built (e.g., Tx1 failed to build or simulate successfully). Cannot send bundle.");
            let _ = sender.send(LaunchStatus::Error("No transactions built for bundle.".to_string()));
            return Err(anyhow::Error::msg("No successful transactions to form a bundle"));
        }
        // If bundle_transactions.len() == 1, it means only Tx1 (which now includes the tip if applicable) was successful.
        // This is a valid scenario to send to Jito, assuming Jito can handle a "bundle" of one.
        // The request was to put create, devbuy, and tip in the *same transaction*.
        info!("  Bundle contains {} transaction(s). Tx1 includes create, dev buy, and potentially the Jito tip.", bundle_transactions.len());

        info!("📦 Sending bundle with {} transactions to Jito...", bundle_transactions.len());
        let _ = sender.send(LaunchStatus::SubmittingBundle(bundle_transactions.len())); // <-- Send status
        let bundle_id = send_jito_bundle_via_json_rpc(bundle_transactions, &jito_block_engine_url).await
            .context("Failed to send Jito bundle")?;

        info!("  ✅ Jito bundle submitted successfully! Bundle ID: {}", bundle_id);
        let _ = sender.send(LaunchStatus::BundleSubmitted(bundle_id.clone())); // <-- Send status
        info!("Monitor Jito Bundle status separately using the Bundle ID and the 'check-bundle' command.");
        println!("\n✅ Launch & Buy Jito bundle submission initiated successfully.");
        // Pump.fun link will be sent via LaunchStatus.
    }

    let final_link = format!("https://pump.fun/{}", mint_pubkey.to_string());
    info!("🚀 Launch process finished. Sending final status with link: {}", final_link);
    let _ = sender.send(LaunchStatus::LaunchCompletedWithLink(final_link.clone()));
    
    // Keep a console print for CLI users or direct runs, but the GUI will use the status.
    println!("\n\n======================================================================");
    println!("🎉 TOKEN CREATION AND INITIAL BUYS COMPLETED! 🎉");
    println!("======================================================================");
    println!("📈 Pump.fun Link: {}", final_link);
    println!("======================================================================\n");

    Ok(())
}
