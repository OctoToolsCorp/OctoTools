// use crate::api::pumpfun::{create_token as api_create_token}; // unused
use crate::config::{get_commitment_config, get_rpc_url};
use crate::errors::{PumpfunError, Result};
use crate::models::token::{TokenConfig, PumpfunMetadata};
use crate::utils::transaction::{simulate_and_check};
use crate::wallet::{get_wallet_keypair, load_zombie_wallets_from_file};
// use anyhow::Context; // unused
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use borsh::BorshDeserialize;
// use console::Style; // unused
use console::style;
use log::{info, error, debug, warn};
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero, CheckedSub};
// use rand::Rng; // unused
use reqwest::Client as ReqwestClient;
use serde_json::{json, Value as JsonValue};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    client_error::{ClientError, ClientErrorKind},
    // rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig}, // unused
    // rpc_response::RpcResult, // unused
};
use solana_sdk::{
    instruction::Instruction, // Added for bundle_instructions type
    message::{v0::Message as TransactionMessageV0, VersionedMessage},
    // native_token::{self, LAMPORTS_PER_SOL}, // LAMPORTS_PER_SOL unused
    native_token,
    pubkey::Pubkey,
    signature::{Keypair, Signer, Signature},
    system_instruction,
    // sysvar, // unused
    transaction::VersionedTransaction,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
};
use spl_associated_token_account::get_associated_token_address;
// use spl_token::instruction as token_instruction; // unused
// use std::path::PathBuf; // unused
// use std::str::FromStr; // unused
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use crate::pump_instruction_builders::{
    build_pump_create_instruction,
    build_pump_buy_instruction,
    find_bonding_curve_pda,
    find_metadata_pda,
    // BondingCurveAccount, // unused
    GlobalState,
    GLOBAL_STATE_PUBKEY,
    // PUMPFUN_PROGRAM_ID, // unused
    // METADATA_PROGRAM_ID, // unused
    // FEE_RECIPIENT_PUBKEY, // unused
};
// use crate::utils::transaction::sign_and_send_versioned_transaction; // unused

// Add imports for simulation config
// use solana_client::rpc_config::{RpcSimulateTransactionAccountsConfig}; // unused
// use solana_transaction_status::UiTransactionEncoding; // unused

// Required for JSON-RPC approach
use bincode;
use spl_token;

// Import ATA program ID and instruction builder
use spl_associated_token_account::{
    instruction::create_associated_token_account,
};

// Import env
use std::env;

// Correct IPFS uploader path to pumpfun module
use crate::api::pumpfun::upload_metadata_to_ipfs;

// --- Calculation Helpers (Restored Outside create_token) ---

/// Calculates the amount of tokens received for a given SOL input amount, considering fees.
fn calculate_tokens_out(
    sol_in: u64,
    virtual_sol: u64,
    virtual_token: u64,
    fee_bps: u64,
) -> Result<u64> {
    if virtual_sol == 0 || virtual_token == 0 {
        return Err(PumpfunError::Calculation("Initial reserves cannot be zero".to_string()));
    }
    let k_sol = BigUint::from(virtual_sol);
    let k_token = BigUint::from(virtual_token);
    let k = k_sol * k_token;
    let fee = (sol_in as u128 * fee_bps as u128) / 10000u128;
    let sol_to_curve = sol_in.saturating_sub(fee as u64);
    if sol_to_curve == 0 { return Ok(0); }
    let new_virtual_sol = BigUint::from(virtual_sol.saturating_add(sol_to_curve));
    if new_virtual_sol.is_zero() { return Err(PumpfunError::Calculation("New virtual SOL reserve is zero".to_string())); }
    let new_virtual_token = &k / new_virtual_sol;
    let tokens_out = BigUint::from(virtual_token)
        .checked_sub(&new_virtual_token)
        .ok_or_else(|| PumpfunError::Calculation("Calculation resulted in negative tokens out".to_string()))?;
    tokens_out.to_u64().ok_or_else(|| PumpfunError::Calculation("Tokens out exceeds u64::MAX".to_string()))
}

/// Calculates the SOL cost required to receive a given amount of tokens, including fees.
fn calculate_sol_cost(
    tokens_in: u64,
    virtual_sol: u64,
    virtual_token: u64,
    fee_bps: u64,
) -> Result<u64> {
    if virtual_sol == 0 || virtual_token == 0 { return Err(PumpfunError::Calculation("Initial reserves cannot be zero".to_string())); }
    if tokens_in == 0 { return Ok(0); }
    if tokens_in >= virtual_token { return Err(PumpfunError::Calculation(format!("Cannot buy {} tokens, only {} virtual tokens available", tokens_in, virtual_token))); }
    let k_sol = BigUint::from(virtual_sol);
    let k_token = BigUint::from(virtual_token);
    let k = k_sol * k_token;
    let new_virtual_token = BigUint::from(virtual_token.saturating_sub(tokens_in));
    if new_virtual_token.is_zero() { return Err(PumpfunError::Calculation("New virtual token reserve is zero after buying".to_string())); }
    let new_virtual_sol = &k / new_virtual_token;
    let sol_cost_before_fee = new_virtual_sol
        .checked_sub(&BigUint::from(virtual_sol))
        .ok_or_else(|| PumpfunError::Calculation("Calculation resulted in negative SOL cost".to_string()))?;
    let sol_cost_before_fee_u64 = sol_cost_before_fee.to_u64().ok_or_else(|| PumpfunError::Calculation("SOL cost before fee exceeds u64::MAX".to_string()))?;
    if fee_bps >= 10000 { return Err(PumpfunError::Calculation("Fee basis points cannot be 100% or more".to_string())); }
    let numerator = sol_cost_before_fee_u64 as u128 * 10000u128;
    let denominator = 10000u128 - fee_bps as u128;
    if denominator == 0 { return Err(PumpfunError::Calculation("Fee calculation denominator is zero".to_string())); }
    let total_sol_needed = (numerator + denominator - 1) / denominator;
    total_sol_needed.try_into().map_err(|_| PumpfunError::Calculation("Total SOL needed exceeds u64::MAX".to_string()))
}

/// Calculates the maximum SOL cost allowed based on expected cost and slippage.
fn apply_slippage_to_sol_cost(
    expected_cost: u64,
    slippage_basis_points: u64,
) -> u64 {
    let numerator = expected_cost as u128 * (10000u128 + slippage_basis_points as u128);
    let denominator = 10000u128;
    let max_cost = (numerator + denominator - 1) / denominator;
    max_cost.try_into().unwrap_or(u64::MAX).max(expected_cost)
}

// Renamed: Helper function to build, sign, send and confirm with retry for BlockhashNotFound
async fn build_sign_send_and_confirm_with_retry(
    rpc_client: &Arc<RpcClient>,
    message: VersionedMessage, // Take the unsigned message directly
    signers: &[&Keypair],      // Keep signers
    commitment: CommitmentConfig,
) -> Result<Signature> {
    const MAX_RETRIES: u8 = 3;
    const RETRY_DELAY_MS: u64 = 500;

    // Loop for retries
    for attempt in 0..MAX_RETRIES {
        // Get latest blockhash
        let latest_blockhash = rpc_client.get_latest_blockhash().await
            .map_err(|e| PumpfunError::SolanaClient(e))?;

        // Clone the original message and update its blockhash
        let mut message_for_attempt = message.clone();
        match &mut message_for_attempt {
            VersionedMessage::V0(msg) => msg.recent_blockhash = latest_blockhash,
            _ => return Err(PumpfunError::Build("Legacy transactions not supported".to_string())),
        }

        // Create and sign the VersionedTransaction for this attempt using try_new
        let tx_for_attempt = VersionedTransaction::try_new(message_for_attempt, signers)
            .map_err(|e| PumpfunError::Signing(format!("Failed to sign transaction attempt {}: {}", attempt + 1, e)))?;

        // Use a blocking client instance specifically for send_and_confirm
        let blocking_rpc_client = solana_client::rpc_client::RpcClient::new_with_commitment(rpc_client.url(), commitment);

        // Send the newly signed transaction for this attempt
        match blocking_rpc_client.send_and_confirm_transaction_with_spinner_and_commitment(&tx_for_attempt, commitment) {
            Ok(signature) => {
                info!("Transaction confirmed on attempt {} with blockhash {}", attempt + 1, latest_blockhash);
                return Ok(signature);
            }
            Err(e) => {
                error!("Attempt {}: Send/confirm error: {}", attempt + 1, e);
                let is_blockhash_not_found = if let ClientError { kind: ClientErrorKind::RpcError(rpc_error), .. } = &e {
                    rpc_error.to_string().contains("Blockhash not found") ||
                    rpc_error.to_string().contains("Transaction simulation failed: Blockhash not found")
                } else {
                    false
                };

                if is_blockhash_not_found && attempt < MAX_RETRIES - 1 {
                    warn!("Blockhash {} expired. Retrying in {}ms...", latest_blockhash, RETRY_DELAY_MS);
                    sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                    // Continue loop to try again
                } else {
                    // Return the error if it's not BlockhashNotFound or it's the last attempt
                    return Err(e.into());
                }
            }
        }
    }

    // If the loop completes without returning Ok or a specific error, it means all retries failed.
    Err(PumpfunError::Transaction(
        "Failed to confirm transaction after max retries due to blockhash issues.".to_string(),
    ))
}

// --- Main Create Token Function ---
pub async fn create_token(
    token_config: TokenConfig,
    jito_bundle: bool,
    zombie_buy_token_amount: u64,
    _keys_path_arg: Option<&str>,
) -> Result<()> {
    // --- Styles & Initial Setup ---
    let success_style = style("").green().bold();
    let error_style = style("").red().bold();
    let info_style = style("").cyan();

    // --- Load Creator Wallet ---
    let creator_keypair = get_wallet_keypair()
        .map_err(|e| PumpfunError::Wallet(format!("Failed to load creator wallet: {}", e)))?;
    let creator_pubkey = creator_keypair.pubkey();
    println!("{} Creator Wallet: {}", style("🔑").green(), creator_pubkey);

    // --- Generate New Token Mint Keypair ---
    let mint_keypair = Keypair::new();
    let token_mint_pubkey = mint_keypair.pubkey();
    println!("{} Generated New Token Mint: {}", style("✨").green(), token_mint_pubkey);

    // --- Clients (RPC & HTTP for Jito/IPFS) ---
    let rpc_url = get_rpc_url();
    let commitment_config = get_commitment_config();
    let rpc_client = Arc::new(RpcClient::new_with_commitment(rpc_url.clone(), commitment_config));

    // --- Fetch GlobalState ---
    println!("\n{} Fetching Pump.fun Global State...", style("🌍").cyan());
    let global_state_data = rpc_client.get_account_data(&GLOBAL_STATE_PUBKEY).await
        .map_err(|e| PumpfunError::SolanaClient(e))?;
    const DISCRIMINATOR_LENGTH: usize = 8;
    const EXPECTED_GLOBAL_STATE_SIZE: usize = 105; 
    let required_length = DISCRIMINATOR_LENGTH + EXPECTED_GLOBAL_STATE_SIZE;
    info!("Fetched GlobalState account data length: {}", global_state_data.len());
    if global_state_data.len() < required_length {
        return Err(PumpfunError::Deserialization(format!(
            "GlobalState account data too short ({} bytes), expected at least {} bytes",
            global_state_data.len(), required_length
        )));
    }
    let data_slice = &global_state_data[DISCRIMINATOR_LENGTH..required_length];
    let global_state = GlobalState::try_from_slice(data_slice)
        .map_err(|e| PumpfunError::Deserialization(format!("Failed to deserialize GlobalState slice ({} bytes): {}", data_slice.len(), e)))?;
    println!("  {} GlobalState loaded.", success_style);

    // --- Load Config ---
    println!("\n{} Loading Configuration...", style("⚙️").cyan());
    let initial_buy_sol = token_config.initial_buy_amount;
    let initial_buy_lamports = (initial_buy_sol * native_token::LAMPORTS_PER_SOL as f64) as u64;
    info!("Requested initial creator buy SOL: {} SOL ({} lamports)", initial_buy_sol, initial_buy_lamports);
    let slippage_str = env::var("BUY_SLIPPAGE_BPS").unwrap_or_else(|_| "500".to_string());
    let slippage_basis_points = slippage_str.parse::<u64>()
        .map_err(|e| PumpfunError::Config(format!("Invalid BUY_SLIPPAGE_BPS '{}': {}", slippage_str, e)))?;
    info!("Using slippage tolerance: {} basis points", slippage_basis_points);
    let priority_fee_str = env::var("PRIORITY_FEE").unwrap_or_else(|_| "1000000".to_string());
    let priority_fee = priority_fee_str.parse::<u64>()
         .map_err(|e| PumpfunError::Config(format!("Invalid PRIORITY_FEE '{}': {}", priority_fee_str, e)))?;
    info!("Using priority fee: {} micro-lamports", priority_fee);
    
    // --- Declare max_zombie_buys here so it's available later ---
    let max_zombie_buys: usize = env::var("MAX_ZOMBIE_BUYS").unwrap_or_else(|_| "5".to_string()).parse().unwrap_or(5);

    // --- Jito specific config (only load if needed, but path is disabled) ---
    if jito_bundle { 
        // Load Jito config vars here just to check they exist if flag is passed,
        // but don't use them as the path returns error.
        let _jito_bundles_url = env::var("JITO_JSON_RPC_URL")
            .map_err(|_| PumpfunError::Config("JITO_JSON_RPC_URL env var not set for --jito-bundle".into()))?;
        let _tip_account_str = env::var("JITO_TIP_ACCOUNT")
             .map_err(|_| PumpfunError::Config("JITO_TIP_ACCOUNT env var not set for --jito-bundle".into()))?;
        let _tip_sol_str = env::var("JITO_TIP_AMOUNT_SOL").unwrap_or_else(|_| "0.001".to_string());
        let _max_zombie_buys_str = env::var("MAX_ZOMBIE_BUYS").unwrap_or_else(|_| "5".to_string());
        info!("Jito bundle mode enabled.");
    }
    println!("  {} Configuration loaded successfully.", success_style);


    // --- Restore Upload Metadata --- 
    println!("\n{} Uploading Metadata...", style("☁️").cyan());
    // --- Initialize PumpfunMetadata with all fields ---
    let metadata = PumpfunMetadata {
        name: token_config.name.clone(),
        symbol: token_config.symbol.clone(),
        description: token_config.description.clone(),
        image: token_config.image_url.clone(), // Use provided URL or path
        // Add missing fields (using defaults or env vars)
        twitter: env::var("TOKEN_TWITTER").ok(),
        telegram: env::var("TOKEN_TELEGRAM").ok(),
        website: env::var("TOKEN_WEBSITE").ok(),
        show_name: env::var("TOKEN_SHOW_NAME").unwrap_or_else(|_| "true".to_string()).parse().unwrap_or(true),
    };
    // --- Fix upload_metadata_to_ipfs call ---
    let metadata_uri: String = upload_metadata_to_ipfs(&metadata).await?;
    println!("  {} Metadata uploaded: {}", success_style, metadata_uri);

    // --- Calculate PDAs using new mint ---
    println!("\n{} Calculating PDAs...", style("🔑").cyan());
    let (bonding_curve_pk, _bonding_curve_bump) = find_bonding_curve_pda(&token_mint_pubkey)?;
    let (metadata_pk, _metadata_bump) = find_metadata_pda(&token_mint_pubkey)?;
    info!("  Bonding Curve PDA: {}", bonding_curve_pk);
    info!("  Metadata PDA: {}", metadata_pk);
    println!("  {} PDAs calculated.", success_style);

    // --- Calculate Buy Args (using initial virtual reserves) --- 
    let estimated_initial_virtual_sol = global_state.initial_virtual_sol_reserves;
    let estimated_initial_virtual_token = global_state.initial_virtual_token_reserves;
    let fee_bps = global_state.fee_basis_points;
    let initial_token_amount = calculate_tokens_out(
        initial_buy_lamports,
        estimated_initial_virtual_sol,
        estimated_initial_virtual_token,
        fee_bps,
    )?;
    let initial_max_sol_cost = apply_slippage_to_sol_cost(initial_buy_lamports, slippage_basis_points); // Apply slippage to lamports

    // --- Restore Build Core Create Instruction --- 
    println!("\n{} Building Core Create Instruction...", style("🛠️").cyan());
    let core_create_instruction = build_pump_create_instruction(
        &creator_pubkey,
        &token_mint_pubkey,
        &bonding_curve_pk,
        &metadata_pk,
        &metadata_uri,
        &token_config.name,
        &token_config.symbol,
    )?;
    info!("  Built Core Create instruction.");

    // --- Build Initial Buy Instruction (for dev) ---
    println!("\n{} Building Initial Developer Buy Instruction...", style("🛠️").cyan());
    let core_initial_buy_instruction = if initial_buy_lamports > 0 && initial_token_amount > 0 {
        Some(build_pump_buy_instruction(
            &creator_pubkey,
            &token_mint_pubkey,
            &bonding_curve_pk,
            // &global_state.fee_recipient, // Removed: Fee recipient is now a constant in the builder
            &creator_pubkey,              // Creator's vault is the creator's own account
            initial_token_amount,
            initial_max_sol_cost
        )?)
    } else {
        None
    };
    if core_initial_buy_instruction.is_some() {
         info!("  Built Core Initial Creator Buy instruction (target_tokens: {}, max_sol: {} lamports).", initial_token_amount, initial_max_sol_cost);
    } else {
        warn!("Skipping Initial Creator Buy instruction build (amount or calculated tokens is 0).");
    }

    // ==================================================
    // === Jito Bundle Path vs Direct RPC Path ===
    // ==================================================

    // --- Uncomment Jito Bundle Path --- 
    if jito_bundle {
        // --- Jito Bundle Path: Single Atomic Transaction --- 
        info!("{} Preparing SINGLE ATOMIC Jito bundle...", style("⚛️").cyan());

        // --- Load Jito Config ---
        let jito_base_url = env::var("JITO_JSON_RPC_URL") 
            .map_err(|_| PumpfunError::Config("JITO_JSON_RPC_URL env var not set".into()))?;
        let tip_account_str = env::var("JITO_TIP_ACCOUNT")
             .map_err(|_| PumpfunError::Config("JITO_TIP_ACCOUNT env var not set".into()))?;
        let tip_account_pk = tip_account_str.parse::<Pubkey>()
             .map_err(|e| PumpfunError::Config(format!("Invalid JITO_TIP_ACCOUNT '{}': {}", tip_account_str, e)))?;
        let tip_sol_str = env::var("JITO_TIP_AMOUNT_SOL").unwrap_or_else(|_| "0.001".to_string());
        let tip_sol = tip_sol_str.parse::<f64>()
            .map_err(|e| PumpfunError::Config(format!("Invalid JITO_TIP_AMOUNT_SOL '{}': {}", tip_sol_str, e)))?;
        let tip_lamports = (tip_sol * native_token::LAMPORTS_PER_SOL as f64) as u64;

        info!("  Jito JSON-RPC Base URL: {}", jito_base_url);
        info!("  Tip Account: {}", tip_account_pk);
        info!("  Tip Amount: {} SOL ({} lamports)", tip_sol, tip_lamports);
        info!("Maximum zombie buys to include: {}", max_zombie_buys);

        // --- Load Zombie Wallets ---
        let zombie_keypairs: Vec<Keypair> = load_zombie_wallets_from_file(None)?
            .into_iter()
            .take(max_zombie_buys)
            .collect::<Vec<_>>();
        if zombie_keypairs.is_empty() && max_zombie_buys > 0 {
            warn!("No zombie wallets found or MAX_ZOMBIE_BUYS=0. No zombie buys included.");
        } else {
            info!("Including {} zombie wallets in bundle.", zombie_keypairs.len());
        }

        // --- Gather Instructions for the Single Bundle --- 
        let mut bundle_instructions: Vec<Instruction> = Vec::new();

        // 1. Compute Budget (Set high initially, adjust later if needed)
        let estimated_cu = 400_000 // Base create
                         + 50_000 // Creator ATA 
                         + if core_initial_buy_instruction.is_some() { 200_000 } else { 0 } // Creator buy
                         + (zombie_keypairs.len() as u64 * (50_000 + 200_000)) // Zombie ATAs + Buys
                         + 50_000; // Tip
        info!("Estimated Compute Units for bundle: {}", estimated_cu);
        // Convert u64 CU estimate to u32 for the instruction
        let compute_unit_limit_u32 = estimated_cu.max(600_000).try_into().unwrap_or(1_400_000);
        bundle_instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit_u32)); 
        bundle_instructions.push(ComputeBudgetInstruction::set_compute_unit_price(priority_fee));

        // 2. Create Token Instruction (Initializes Mint FIRST)
        info!("Adding Core Create instruction...");
        bundle_instructions.push(core_create_instruction.clone());

        // 3. Create Creator ATA (Now that Mint exists)
        info!("Adding Creator ATA creation instruction...");
        bundle_instructions.push(create_associated_token_account(
            &creator_pubkey, // Payer
            &creator_pubkey, // Owner
            &token_mint_pubkey, // Mint (Now exists)
            &spl_token::id(), // Token program
        ));

        // 4. Create Zombie ATAs (Now that Mint exists)
        info!("Adding Zombie ATA creation instructions...");
        for zombie_kp in &zombie_keypairs {
            let zombie_pk = zombie_kp.pubkey();
            // Add ATA Creation for Zombie
            bundle_instructions.push(create_associated_token_account(
                &creator_pubkey, // Payer for zombie ATAs 
                &zombie_pk,      // Owner is the zombie
                &token_mint_pubkey,
                &spl_token::id(),
            ));
            info!("  Added ATA create for zombie {}", zombie_pk);
        }
        
        // 5. Creator Initial Buy Instruction (Requires Creator ATA)
        if let Some(buy_ix) = &core_initial_buy_instruction {
            info!("Adding Creator Initial Buy instruction...");
            bundle_instructions.push(buy_ix.clone());
        }

        // 6. Zombie Buy Instructions (Requires Zombie ATAs)
        info!("Adding Zombie Buy instructions...");
        for zombie_kp in &zombie_keypairs {
            let zombie_pk = zombie_kp.pubkey();
            // Calculate and Add Buy for Zombie
            let zombie_buy_lamports = calculate_sol_cost(
                zombie_buy_token_amount, 
                estimated_initial_virtual_sol, 
                estimated_initial_virtual_token, 
                fee_bps
            )?;
            let zombie_max_sol_cost = apply_slippage_to_sol_cost(zombie_buy_lamports, slippage_basis_points);
            let zombie_buy_instruction = build_pump_buy_instruction(
                &zombie_pk,
                &token_mint_pubkey,
                &bonding_curve_pk,
                // &global_state.fee_recipient, // Removed: Fee recipient is now a constant in the builder
                &creator_pubkey,              // Creator's vault is still the original creator's account
                zombie_buy_token_amount,
                zombie_max_sol_cost
            )?;
            bundle_instructions.push(zombie_buy_instruction);
            info!("  Added buy for zombie {} (Target tokens: {}, Max SOL: {} lamports)", zombie_pk, zombie_buy_token_amount, zombie_max_sol_cost);
        }

        // 7. Jito Tip Instruction
        info!("Adding Jito Tip instruction...");
        bundle_instructions.push(system_instruction::transfer(
            &creator_pubkey,
            &tip_account_pk,
            tip_lamports,
        ));

        // --- Gather All Signers (Order doesn't matter here) ---
        let mut bundle_signers: Vec<&Keypair> = vec![&creator_keypair, &mint_keypair];
        info!("Adding bundle signers (creator, mint, zombies)...");
        for zombie_kp in &zombie_keypairs {
            bundle_signers.push(zombie_kp);
        }
        bundle_signers.dedup_by_key(|kp| kp.pubkey()); 
        info!("Total unique signers for bundle: {}", bundle_signers.len());

        // --- [NEW] Explicit Balance Check before Building Bundle ---
        let current_creator_balance = rpc_client.get_balance(&creator_pubkey).await?;
        info!("Current creator balance fetched BEFORE building bundle: {} lamports ({} SOL)", 
            current_creator_balance, 
            current_creator_balance as f64 / native_token::LAMPORTS_PER_SOL as f64
        );
        // --- [END NEW] Balance Check ---

        // --- Build the Single Bundle Transaction --- 
        info!("Building the single bundle transaction...");
        let latest_blockhash = rpc_client.get_latest_blockhash().await?;
        let bundle_message_v0 = TransactionMessageV0::try_compile(
            &creator_pubkey, // Fee payer is the creator
            &bundle_instructions,
            &[], // No LUTs for now
            latest_blockhash
        ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile bundle V0 message: {}", e)))?;
        
        let bundle_tx = VersionedTransaction::try_new(
            VersionedMessage::V0(bundle_message_v0),
            &bundle_signers // Sign with ALL required keypairs
        ).map_err(|e| PumpfunError::Transaction(format!("Failed to sign bundle transaction: {}", e)))?;
        info!("Built and signed the atomic bundle transaction.");

        // --- Simulate the Combined Bundle Transaction --- 
        info!("Simulating atomic bundle transaction...");
        // Pass None for accounts_to_include as we don't need specific account data back
        simulate_and_check(&rpc_client, &bundle_tx, "Atomic Bundle TX", None /* accounts_to_fetch */).await?;
        info!("Simulation successful!");

        // --- Serialize the Single Bundle Transaction ---
        let bundle_tx_bytes = bincode::serialize(&bundle_tx)
            .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize bundle_tx: {}", e)))?;
        debug!("Serialized atomic bundle_tx size: {} bytes", bundle_tx_bytes.len());
        let bundle_tx_base64 = BASE64_STANDARD.encode(&bundle_tx_bytes);
        info!("Serialized bundle transaction to base64.");

        // --- Construct JSON-RPC Request (Single Transaction Bundle) --- 
        let bundle_params = vec![bundle_tx_base64]; // Bundle now contains only one tx
        let encoding_param = json!({ "encoding": "base64" });
        let request_payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [bundle_params, encoding_param]
        });
        info!("  Constructed sendBundle JSON-RPC payload with base64 encoding specified.");

        // --- Send Bundle via HTTP POST ---
        let http_client = ReqwestClient::new();
        let bundles_endpoint = jito_base_url; // Use the full URL from .env
        info!("  Sending bundle to: {}", bundles_endpoint);

        match http_client.post(&bundles_endpoint)
            .json(&request_payload)
            .send()
            .await {
            Ok(response) => {
                let status = response.status();
                let response_text = response.text().await.unwrap_or_else(|_| "Failed to read response text".to_string());
                debug!("Jito response status: {}", status);
                debug!("Jito response body: {}", response_text);

                if status.is_success() {
                    match serde_json::from_str::<JsonValue>(&response_text) {
                        Ok(json_response) => {
                            if let Some(bundle_id) = json_response.get("result").and_then(|v| v.as_str()) {
                                println!("    {} Bundle submitted successfully via JSON-RPC! Bundle ID: {}", success_style, bundle_id);
                            } else if let Some(error) = json_response.get("error") {
                                error!("    {} Jito API Error: {}", error_style, error);
                                return Err(PumpfunError::Jito(format!("Jito API error: {}", error)));
                            } else {
                                error!("    {} Unexpected Jito JSON response format (Success Status): {}", error_style, response_text);
                                return Err(PumpfunError::Jito("Unexpected Jito JSON response format (Success Status)".to_string()));
                            }
                        }
                        Err(e) => {
                            error!("    {} Failed to parse Jito JSON response (Success Status): {} - Body: {}", error_style, e, response_text);
                            return Err(PumpfunError::Jito(format!("Failed to parse Jito JSON response (Success Status): {}", e)));
                        }
                    }
                } else {
                     match serde_json::from_str::<JsonValue>(&response_text) {
                         Ok(json_response) => {
                             if let Some(error) = json_response.get("error") {
                                 error!("    {} Jito API Error (Status {}): {}", error_style, status, error);
                                 return Err(PumpfunError::Jito(format!("Jito API error (Status {}): {}", status, error)));
                             } else {
                                 error!("    {} Jito request failed with status {} and non-standard error body: {}", error_style, status, response_text);
                                 return Err(PumpfunError::Jito(format!("Jito request failed with status {} and non-standard error body", status)));
                             }
                         }
                         Err(_) => {
                             error!("    {} Jito request failed with status {}: {}", error_style, status, response_text);
                             return Err(PumpfunError::Jito(format!("Jito request failed with status {}: {}", status, response_text)));
                         }
                     }
                }
            }
            Err(e) => {
                error!("    {} Failed to send HTTP request to Jito: {}", error_style, e);
                return Err(PumpfunError::Http(e));
            }
        }
    } else {
        // --- Direct RPC Path (Original Logic) ---
        info!("{} Using DIRECT RPC path (not Jito bundle)...", style("📡").cyan());

        // --- Build Create Transaction Message ---
        let mut instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(600_000), // Increased CU limit
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee), // Use configured priority fee
            core_create_instruction,
        ];
        let mut buy_instruction_added_to_create_tx = false; // Track if buy was added
        if let Some(buy_ix) = &core_initial_buy_instruction { // Borrow here
            // Add ATA creation before buy if it's part of this transaction
            instructions.push(create_associated_token_account(
                &creator_pubkey, // Payer
                &creator_pubkey, // Owner
                &token_mint_pubkey, // Mint
                &spl_token::id(), // Token program
            ));
            instructions.push(buy_ix.clone()); // Clone the instruction for use here
            buy_instruction_added_to_create_tx = true;
        }
        let create_message_v0 = TransactionMessageV0::try_compile(
            &creator_pubkey,
            &instructions,
            &[], // No LUTs for now
            rpc_client.get_latest_blockhash().await?,
        ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile create V0 message: {}", e)))?;
        let create_message = VersionedMessage::V0(create_message_v0);

        // --- Sign and Send Create Transaction ---
        println!("{} Sending Create Transaction...", style("🚀").cyan());
        let create_signature = build_sign_send_and_confirm_with_retry(
            &rpc_client,
            create_message,
            &[&creator_keypair, &mint_keypair], // Mint keypair also signs create
            commitment_config,
        ).await?;
        println!("  {} Create Transaction Confirmed: {}", success_style, create_signature);
        println!("  {} Token Mint: {}", success_style, token_mint_pubkey);
        println!("  {} Bonding Curve: {}", success_style, bonding_curve_pk);
        println!("  {} Metadata: {}", success_style, metadata_pk);

        // --- Create and Send Initial Buy Transaction (if applicable, separate from create) ---
        if initial_buy_lamports > 0 && !buy_instruction_added_to_create_tx {
            // Check core_initial_buy_instruction here. If it's Some, it means a buy was intended
            // but not added to the create_tx. We consume it here for the separate transaction.
            if let Some(buy_instruction_to_send_separately) = core_initial_buy_instruction {
                warn!("Initial buy was not part of create transaction, attempting separate buy.");
                let buyer_token_ata = get_associated_token_address(&creator_pubkey, &token_mint_pubkey);
                match rpc_client.get_account(&buyer_token_ata).await {
                    Ok(_) => info!("  Buyer ATA {} already exists.", buyer_token_ata),
                    Err(_) => {
                        info!("  Buyer ATA {} does not exist. Creating...", buyer_token_ata);
                        let ata_instructions = vec![
                            ComputeBudgetInstruction::set_compute_unit_limit(50_000),
                            ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
                            create_associated_token_account(
                                &creator_pubkey,
                                &creator_pubkey,
                                &token_mint_pubkey,
                                &spl_token::id(),
                            ),
                        ];
                        let ata_message_v0 = TransactionMessageV0::try_compile(
                            &creator_pubkey,
                            &ata_instructions,
                            &[],
                            rpc_client.get_latest_blockhash().await?,
                        ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile ATA V0 message: {}", e)))?;
                        let ata_message = VersionedMessage::V0(ata_message_v0);

                        let ata_signature = build_sign_send_and_confirm_with_retry(
                            &rpc_client,
                            ata_message,
                            &[&creator_keypair],
                            commitment_config,
                        ).await?;
                        println!("    {} Buyer ATA Creation Confirmed: {}", success_style, ata_signature);
                        sleep(Duration::from_secs(2)).await; // Wait for ATA to be queryable
                    }
                }

                let buy_instructions_separate = vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(300_000), 
                    ComputeBudgetInstruction::set_compute_unit_price(priority_fee),
                    buy_instruction_to_send_separately, // Use the consumed instruction
                ];
                let buy_message_v0_separate = TransactionMessageV0::try_compile(
                    &creator_pubkey,
                    &buy_instructions_separate,
                    &[],
                    rpc_client.get_latest_blockhash().await?,
                ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile separate buy V0 message: {}", e)))?;
                let buy_message_separate = VersionedMessage::V0(buy_message_v0_separate);
                
                println!("{} Sending Initial Developer Buy Transaction (Separate)...", style("💸").cyan());
                let buy_signature = build_sign_send_and_confirm_with_retry(
                    &rpc_client,
                    buy_message_separate,
                    &[&creator_keypair], 
                    commitment_config,
                ).await?;
                println!("  {} Initial Developer Buy Transaction Confirmed: {}", success_style, buy_signature);
            } else {
                warn!("  Skipping separate buy as core_initial_buy_instruction was None, despite initial_buy_lamports > 0 and not added to create_tx.");
            }
        }
    }

    println!(
        "\n{} Token Creation Process Complete! {}",
        success_style, token_mint_pubkey
    );
    println!(
        "{} Pump.fun Link: https://pump.fun/{}",
        style("🔗").green(),
        token_mint_pubkey
    );

    Ok(())
}