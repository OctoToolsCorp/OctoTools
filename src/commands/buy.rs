use crate::api::pumpfun::buy_token as api_buy_token;
use crate::config::{get_commitment_config, get_rpc_url}; // Removed CONFIG
use crate::errors::{PumpfunError, Result};
use anyhow::Context as AnyhowContext; // Import the Context trait
use crate::wallet::get_wallet_keypair;
use crate::utils::transaction::sign_and_send_versioned_transaction;
use console::{style, Style};
use log::{info, error, debug}; // Removed warn
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::RpcClient as BlockingRpcClient; // Needed for retry helper
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    signature::{Signer}, // Removed Keypair, Signature
    message::VersionedMessage,
    transaction::VersionedTransaction,
    message::v0::Message as TransactionMessageV0,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    system_instruction,
    native_token,
};
use std::str::FromStr;
use std::env;
use std::time::Duration;
// use tokio::time::sleep; // Ensured this is present - Removed as unused

// --- Jito / Bundle Related Imports ---
use crate::pump_instruction_builders::{ // Need instruction builders
    build_pump_buy_instruction,
    find_bonding_curve_pda,
    BondingCurveAccount, GlobalState, GLOBAL_STATE_PUBKEY // Need these structs and constant
};
use borsh::BorshDeserialize; // For deserializing bonding curve and global state
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero, CheckedSub};
use reqwest::Client as ReqwestClient;
use serde_json::{json, Value as JsonValue};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use bincode;
use solana_client::rpc_config::{RpcSimulateTransactionConfig}; // Removed RpcSimulateTransactionAccountsConfig
use solana_transaction_status::UiTransactionEncoding;

// --- Add ATA related imports ---
use spl_associated_token_account::get_associated_token_address;
use spl_associated_token_account::instruction::create_associated_token_account;
use spl_token; // Added for spl_token::id()
// --- End ATA related imports ---

// ATA Rent Constants
const ATA_RENT_LAMPORTS: u64 = 2_039_280; // Standard rent for an ATA
const ATA_RENT_SOL: f64 = ATA_RENT_LAMPORTS as f64 / native_token::LAMPORTS_PER_SOL as f64;

// --- Calculation Helpers (Copied from create.rs) ---

/// Calculates the amount of tokens received for a given SOL input amount, considering fees.
fn calculate_tokens_out(
    sol_in: u64,
    virtual_sol: u64,
    virtual_token: u64,
    fee_bps: u64,
) -> Result<u64> {
    if virtual_sol == 0 || virtual_token == 0 {
        return Err(PumpfunError::Calculation("Bonding curve reserves cannot be zero".to_string()));
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

// --- Simulation Helper (Copied from create.rs) ---
async fn simulate_and_check(
    rpc_client: &RpcClient,
    transaction: &VersionedTransaction,
    tx_description: &str,
    commitment: CommitmentConfig,
) -> Result<()> {
    info!("  Simulating {}...", tx_description);
    
    let sim_config = RpcSimulateTransactionConfig {
        sig_verify: false, 
        replace_recent_blockhash: true, 
        commitment: Some(commitment),
        encoding: Some(UiTransactionEncoding::Base64), 
        accounts: None, 
        min_context_slot: None,
        inner_instructions: false,
    };

    match rpc_client.simulate_transaction_with_config(transaction, sim_config).await {
        Ok(response) => {
            if let Some(err) = response.value.err {
                error!("{} Simulation FAILED: {:?}", tx_description, err);
                if let Some(logs) = response.value.logs {
                    error!("Simulation Logs:");
                    for log in logs {
                        error!("- {}", log);
                    }
                }
                Err(PumpfunError::TransactionSimulation(format!(
                    "{} simulation failed: {:?}",
                    tx_description,
                    err
                )))
            } else {
                info!("  ✅ {} Simulation Successful.", tx_description);
                if let Some(units) = response.value.units_consumed {
                    debug!("  {} Simulated Compute Units: {}", tx_description, units);
                }
                Ok(())
            }
        }
        Err(e) => {
            error!("Error during {} simulation request: {}", tx_description, e);
            Err(PumpfunError::SolanaClient(e))
        }
    }
}

/// Buy tokens from Pump.fun
pub async fn buy_token(
    mint: &str, 
    amount: f64, // Amount of SOL to spend
    slippage: u8, 
    priority_fee: f64, 
    jito_bundle: bool, // Added parameter
    _keys_path_arg: Option<&str>, // Added parameter (currently unused here)
) -> Result<()> {
    // Initialize the console styling
    let success_style = Style::new().green().bold();
    let info_style = Style::new().cyan();
    let warning_style = Style::new().yellow();
    
    println!("{}", style("💰 Buying tokens from Pump.fun").cyan().bold());
    
    // Initialize RPC client
    let rpc_url = get_rpc_url();
    let commitment_config = get_commitment_config();
    let rpc_client = RpcClient::new_with_commitment(rpc_url.clone(), commitment_config); // Clone for blocking client
    let _blocking_rpc_client = BlockingRpcClient::new_with_commitment(rpc_url, commitment_config); // Need this for standard path
    
    // Parse mint address
    let mint_pubkey = Pubkey::from_str(mint)
        .map_err(|_| PumpfunError::InvalidParameter(format!("Invalid mint address: {}", mint)))?;
    
    // Get wallet keypair
    let wallet_keypair = get_wallet_keypair()?;
    println!("{} {}", 
        info_style.apply_to("Using wallet:"),
        wallet_keypair.pubkey()
    );
    
    // Check if the token exists
    match rpc_client.get_account(&mint_pubkey).await {
        Ok(_) => {
            println!("{}", success_style.apply_to("✓ Token exists"));
        },
        Err(_) => {
            let error_msg = format!("Token {} does not exist", mint);
            println!("{} {}", 
                warning_style.apply_to("⚠️ Error:"),
                error_msg
            );
            return Err(PumpfunError::Token(error_msg));
        }
    }
    
    // Check wallet balance
    let balance = rpc_client.get_balance(&wallet_keypair.pubkey())
        .await
        .map_err(|e| PumpfunError::SolanaClient(e))?;
        
    let sol_balance = balance as f64 / native_token::LAMPORTS_PER_SOL as f64; // Use native_token constant
    
    println!("{} {:.6} SOL",
        info_style.apply_to("Wallet balance:"),
        sol_balance
    );
    
    // Determine potential ATA creation cost
    let buyer_pk = wallet_keypair.pubkey();
    let buyer_ata_for_mint = get_associated_token_address(&buyer_pk, &mint_pubkey);
    let mut estimated_ata_cost_sol = 0.0;

    // Check if the ATA for the token being bought exists
    match rpc_client.get_account(&buyer_ata_for_mint).await {
        Ok(_) => {
            debug!("Buyer ATA {} for mint {} already exists. No extra rent needed for this.", buyer_ata_for_mint, mint_pubkey);
        }
        Err(_) => {
            // Assuming error means account not found, will need to create it.
            // A more robust check could inspect the error type.
            debug!("Buyer ATA {} for mint {} not found. Estimating rent cost.", buyer_ata_for_mint, mint_pubkey);
            estimated_ata_cost_sol = ATA_RENT_SOL;
        }
    }

    // Ensure minimum balance (amount to spend + general fees + ATA rent if needed)
    let general_transaction_fees_sol = 0.01; // Base estimate for network/priority fees for the main buy tx
    let mut min_balance_needed = amount + general_transaction_fees_sol;

    if estimated_ata_cost_sol > 0.0 {
        min_balance_needed += estimated_ata_cost_sol;
        println!("{} Estimated {:.6} SOL for new Associated Token Account (ATA) creation.",
            info_style.apply_to("Note:"),
            estimated_ata_cost_sol
        );
    }

    if sol_balance < min_balance_needed {
        let error_message = format!(
            "Insufficient wallet balance. Required: {:.6} SOL (Spend: {:.6} SOL, Est. ATA Rent: {:.6} SOL, Est. Fees: {:.4} SOL), Available: {:.6} SOL",
            min_balance_needed, amount, estimated_ata_cost_sol, general_transaction_fees_sol, sol_balance
        );
        println!("{} {}", warning_style.apply_to("⚠️ Error:"), error_message);
        return Err(PumpfunError::InsufficientBalance(error_message));
    }
    
    // Print buy details
    println!("\n{}", style("📝 Buy Details:").cyan().bold());
    println!("Token Mint: {}", mint);
    println!("Amount: {:.6} SOL", amount);
    println!("Slippage: {}%", slippage);
    println!("Priority Fee: {:.6} SOL", priority_fee);
    
    // --- Jito Bundle Path vs Standard RPC Path ---
    if jito_bundle {
        // --- Jito Bundle Path ---
        info!("Preparing Jito bundle for buy...");

        // Load Jito Config
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

        // --- Check and Create Buyer ATA if necessary ---
        let buyer_pk = wallet_keypair.pubkey();
        let buyer_ata = get_associated_token_address(&buyer_pk, &mint_pubkey);
        info!("Checking buyer ATA: {}", buyer_ata);

        match rpc_client.get_account(&buyer_ata).await {
            Ok(_) => {
                info!("  Buyer ATA already exists.");
            }
            Err(e) => {
                // Assuming error means account not found, proceed to create
                // More robust error handling could check the specific error kind
                info!("  Buyer ATA not found ({}). Creating...", e);

                let ata_creation_ix = create_associated_token_account(
                    &buyer_pk, // payer
                    &buyer_pk, // wallet address
                    &mint_pubkey, // CA
                    &spl_token::id(), // token program id
                );

                let latest_blockhash = rpc_client.get_latest_blockhash().await
                    .map_err(|e| PumpfunError::SolanaClient(e))?;
                
                // Correctly compile the V0 message
                let ata_tx_msg = TransactionMessageV0::try_compile(
                    &buyer_pk, // payer
                    &[ata_creation_ix], // instruction
                    &[], // no LUTs needed for ATA creation
                    latest_blockhash,
                ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile ATA creation message: {}", e)))?;
                let ata_tx = VersionedTransaction::try_new(
                    VersionedMessage::V0(ata_tx_msg),
                    &[&wallet_keypair]
                ).map_err(|e| PumpfunError::Transaction(format!("Failed to build ATA creation tx: {}", e)))?;
                
                info!("  Sending ATA creation transaction...");
                match sign_and_send_versioned_transaction(&rpc_client, ata_tx, &[&wallet_keypair]).await {
                    Ok(signature) => {
                        info!("  ✅ ATA Creation Confirmed! Signature: {}", signature);
                        info!("  Waiting briefly for ATA state propagation...");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        error!("  ❌ ATA Creation Failed: {}", e);
                        return Err(PumpfunError::Transaction(format!("Failed to create required buyer ATA: {}", e)));
                    }
                }
            }
        }
        // --- End ATA Check/Create ---

        info!("  Jito JSON-RPC Base URL: {}", jito_base_url);
        info!("  Tip Account: {}", tip_account_pk);
        info!("  Tip Amount: {} SOL ({} lamports)", tip_sol, tip_lamports);

        // Fetch Current Bonding Curve State
        let (bonding_curve_pk, _) = find_bonding_curve_pda(&mint_pubkey)?;
        info!("  Fetching bonding curve state for {}: {}", mint_pubkey, bonding_curve_pk);
        let bonding_curve_data = rpc_client.get_account_data(&bonding_curve_pk).await
            .map_err(|e| PumpfunError::SolanaClient(e))?;
        // Deserialize (assuming the structure matches) - adjust offset/struct if needed
        // Need to find the correct offset for BondingCurveAccount within the account data
        // This often requires inspecting the account data structure or referring to an IDL
        // Assuming an 8-byte discriminator like in GlobalState for now
        const BONDING_CURVE_DISCRIMINATOR_LENGTH: usize = 8;
        if bonding_curve_data.len() <= BONDING_CURVE_DISCRIMINATOR_LENGTH {
            return Err(PumpfunError::Deserialization(format!(
                "Bonding curve account data too short ({} bytes) for {}",
                bonding_curve_data.len(), bonding_curve_pk
            )));
        }
        let bonding_curve_state = BondingCurveAccount::try_from_slice(&bonding_curve_data[BONDING_CURVE_DISCRIMINATOR_LENGTH..])
            .map_err(|e| PumpfunError::Deserialization(format!("Failed to deserialize bonding curve {}: {}", bonding_curve_pk, e)))?;
        info!("  Current curve state: virtual_sol={}, virtual_token={}", 
             bonding_curve_state.virtual_sol_reserves, bonding_curve_state.virtual_token_reserves);

        // Calculate Buy Args based on Current State
        let buy_lamports = (amount * native_token::LAMPORTS_PER_SOL as f64) as u64;
        // TODO: Fetch current fee_bps from GlobalState or assume it's constant
        //       Fetching GlobalState adds another RPC call. For now, assume 1% as placeholder.
        let fee_basis_points_str = env::var("PUMPFUN_FEE_BPS").unwrap_or_else(|_| "100".to_string()); // Example: Use env var or default
        let fee_bps = fee_basis_points_str.parse::<u64>().unwrap_or(100); // Default to 100 bps (1%)
        // info!("  Using fee basis points: {}", fee_bps); // Uncomment for debugging

        let expected_token_amount = calculate_tokens_out(
            buy_lamports,
            bonding_curve_state.virtual_sol_reserves, 
            bonding_curve_state.virtual_token_reserves,
            fee_bps,
        )?;
        let slippage_basis_points = slippage as u64 * 100; // Convert % to basis points
        let max_sol_cost = apply_slippage_to_sol_cost(buy_lamports, slippage_basis_points);
        info!("  Calculated buy: expected_tokens={}, max_sol_cost={} lamports", expected_token_amount, max_sol_cost);

        if expected_token_amount == 0 {
            return Err(PumpfunError::Calculation("Calculated token amount is zero. Check input SOL amount or curve state.".to_string()));
        }

        // Fetch GlobalState to get the correct fee_recipient for the buy instruction
        info!("  Fetching GlobalState for fee_recipient...");
        let global_state_data_for_buy = rpc_client.get_account_data(&GLOBAL_STATE_PUBKEY).await
            .map_err(|e| PumpfunError::SolanaClient(e)) // This maps to your custom error
            .map_err(anyhow::Error::from) // Convert your custom error to anyhow::Error
            .with_context(|| "Failed to fetch GlobalState for buy instruction fee_recipient")?; // Use with_context for closure-based context
        
        if global_state_data_for_buy.len() <= BONDING_CURVE_DISCRIMINATOR_LENGTH { // Re-use discriminator length constant name, though it's for GlobalState here
             return Err(PumpfunError::Deserialization(format!(
                "GlobalState account data too short ({} bytes) for deserialization",
                global_state_data_for_buy.len()
            )));
        }
        let global_state_for_buy = GlobalState::deserialize(&mut &global_state_data_for_buy[BONDING_CURVE_DISCRIMINATOR_LENGTH..])
            .map_err(|e| PumpfunError::Deserialization(format!("Failed to deserialize GlobalState for buy: {}",e)))?;
        info!("  Using fee_recipient from GlobalState: {}", global_state_for_buy.fee_recipient);

        // Build Buy Instruction
        let buy_instruction = build_pump_buy_instruction(
            &wallet_keypair.pubkey(),
            &mint_pubkey,
            &bonding_curve_pk,
            // &global_state_for_buy.fee_recipient, // Removed: Fee recipient is now a constant in the builder
            &bonding_curve_state.creator,       // Creator's vault from the fetched bonding curve
            expected_token_amount,
            max_sol_cost,
        )?;

        // Build Tip Instruction
        let tip_instruction = system_instruction::transfer(&wallet_keypair.pubkey(), &tip_account_pk, tip_lamports);

        // Build Single Buy+Tip Transaction
        let priority_fee_microlamports = (priority_fee * 1_000_000.0) as u64; // Convert SOL priority fee
        let instructions = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(600_000), // Higher limit for buy+tip
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports), 
            buy_instruction,
            tip_instruction,
        ];
        let latest_blockhash = rpc_client.get_latest_blockhash().await?;
        let message_v0 = TransactionMessageV0::try_compile(
            &wallet_keypair.pubkey(),
            &instructions,
            &[], // No LUTs
            latest_blockhash
        ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile buy/tip V0 message: {}", e)))?;
        let message = VersionedMessage::V0(message_v0);
        let signatures = vec![wallet_keypair.sign_message(&message.serialize())];
        let buy_tip_tx = VersionedTransaction {
            signatures,
            message,
        };
        info!("  Built and signed Buy/Tip transaction for bundle.");

        // Simulate Transaction
        simulate_and_check(&rpc_client, &buy_tip_tx, "Buy/Tip TX", commitment_config).await?;

        // Serialize and Encode
        let tx_bytes = bincode::serialize(&buy_tip_tx)
            .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize buy_tip_tx: {}", e)))?;
        debug!("Serialized buy_tip_tx size: {} bytes", tx_bytes.len());
        let tx_base64 = BASE64_STANDARD.encode(&tx_bytes);

        // Construct JSON-RPC Payload (Single TX Bundle)
        let bundle_params = vec![tx_base64]; 
        let encoding_param = json!({ "encoding": "base64" });
        let request_payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [bundle_params, encoding_param]
        });

        // Send Bundle via HTTP POST
        let http_client = ReqwestClient::new();
        let bundles_endpoint = jito_base_url;
        info!("  Sending buy/tip bundle to: {}", bundles_endpoint);
        match http_client.post(&bundles_endpoint).json(&request_payload).send().await {
            Ok(response) => {
                let status = response.status();
                let response_text = response.text().await.unwrap_or_else(|_| "Failed to read response text".to_string());
                debug!("Jito response status: {}", status);
                debug!("Jito response body: {}", response_text);
                if status.is_success() {
                    match serde_json::from_str::<JsonValue>(&response_text) {
                        Ok(json_response) => {
                            if let Some(bundle_id) = json_response.get("result").and_then(|v| v.as_str()) {
                                println!("    {} Bundle submitted successfully via JSON-RPC! Bundle ID: {}", success_style.apply_to("✅"), bundle_id);
                                println!("\n{}", success_style.apply_to("✅ Buy order submitted via Jito bundle!"));
                                println!("Bundle ID: {}", bundle_id);
                                println!("\nUse getBundleStatuses or Jito Explorer to check landing status.");
                            } else if let Some(error) = json_response.get("error") {
                                error!("    {} Jito API Error: {}", warning_style.apply_to("⚠️"), error);
                                return Err(PumpfunError::Jito(format!("Jito API error: {}", error)));
                            } else {
                                error!("    {} Unexpected Jito JSON response format: {}", warning_style.apply_to("⚠️"), response_text);
                                return Err(PumpfunError::Jito("Unexpected Jito JSON response format".to_string()));
                            }
                        }
                        Err(e) => {
                            error!("    {} Failed to parse Jito JSON response: {} - Body: {}", warning_style.apply_to("⚠️"), e, response_text);
                            return Err(PumpfunError::Jito(format!("Failed to parse Jito JSON response: {}", e)));
                        }
                    }
                } else {
                    error!("    {} Jito request failed with status {}: {}", warning_style.apply_to("⚠️"), status, response_text);
                    return Err(PumpfunError::Jito(format!("Jito request failed with status {}: {}", status, response_text)));
                }
            }
            Err(e) => {
                error!("    {} Failed to send HTTP request to Jito: {}", warning_style.apply_to("⚠️"), e);
                return Err(PumpfunError::Jito(format!("HTTP request failed: {}", e)));
            }
        }

    } else {
        // --- Standard RPC Path (using API helper) ---
        println!("\n{}", style("🚀 Sending buy transaction via standard RPC...").cyan().bold());
        
        // --- Check and Create Buyer ATA if necessary ---
        let buyer_pk = wallet_keypair.pubkey();
        let buyer_ata = get_associated_token_address(&buyer_pk, &mint_pubkey);
        info!("Checking buyer ATA: {}", buyer_ata);

        match rpc_client.get_account(&buyer_ata).await {
            Ok(_) => {
                info!("  Buyer ATA already exists.");
            }
            Err(e) => {
                // Assuming error means account not found, proceed to create
                info!("  Buyer ATA not found ({}). Creating...", e);

                let ata_creation_ix = create_associated_token_account(
                    &buyer_pk, // payer
                    &buyer_pk, // wallet address
                    &mint_pubkey, // CA
                    &spl_token::id(), // token program id
                );

                let latest_blockhash = rpc_client.get_latest_blockhash().await
                    .map_err(|e| PumpfunError::SolanaClient(e))?;
                
                // Correctly compile the V0 message
                let ata_tx_msg = TransactionMessageV0::try_compile(
                    &buyer_pk, // payer
                    &[ata_creation_ix], // instruction
                    &[], // no LUTs needed for ATA creation
                    latest_blockhash,
                ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile ATA creation message: {}", e)))?;
                let ata_tx = VersionedTransaction::try_new(
                    VersionedMessage::V0(ata_tx_msg),
                    &[&wallet_keypair]
                ).map_err(|e| PumpfunError::Transaction(format!("Failed to build ATA creation tx: {}", e)))?;

                info!("  Sending ATA creation transaction...");
                match sign_and_send_versioned_transaction(&rpc_client, ata_tx, &[&wallet_keypair]).await {
                    Ok(signature) => {
                        info!("  ✅ ATA Creation Confirmed! Signature: {}", signature);
                        info!("  Waiting briefly for ATA state propagation...");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        error!("  ❌ ATA Creation Failed: {}", e);
                        return Err(PumpfunError::Transaction(format!("Failed to create required buyer ATA: {}", e)));
                    }
                }
            }
        }
        // --- End ATA Check/Create ---

        // Now call the existing API function which handles the buy via RPC
        match api_buy_token(
            &wallet_keypair,
            &mint_pubkey,
            amount,
            slippage,
            priority_fee,
        ).await {
            Ok(versioned_transaction) => {
                println!("{}", info_style.apply_to("Sending transaction..."));
                match sign_and_send_versioned_transaction(
                    &rpc_client,
                    versioned_transaction,
                    &[&wallet_keypair],
                ).await {
                    Ok(signature) => {
                        println!("{} {}", 
                            success_style.apply_to("✓ Transaction sent:"),
                            signature
                        );
                        
                        println!("\n{}", success_style.apply_to("✅ Buy order completed successfully!"));
                        println!("Transaction: {}", signature);
                        println!("\nYou can view the transaction at: https://explorer.solana.com/tx/{}", signature);
                        println!("Or check your tokens at: https://pump.fun/token/{}", mint);
                    },
                    Err(e) => {
                        println!("\n{} Buy transaction failed", info_style.apply_to("❌ Error:"));
                        println!("Error details: {}", e);
                        return Err(PumpfunError::Transaction(format!("Failed to send transaction: {}", e)));
                    }
                }
            },
            Err(e) => {
                println!("\n{} Failed to create buy transaction", info_style.apply_to("❌ Error:"));
                println!("Error details: {}", e);
                return Err(e);
            }
        }
    }
    
    Ok(())
} 