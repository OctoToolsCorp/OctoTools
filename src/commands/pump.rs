
use std::sync::Arc;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering}; // Added for toggle
// use std::sync::mpsc::{Sender, channel}; // Removed unused Sender, channel
use tokio::time::{self, Duration}; // Added for delay

use anyhow::{Result, Context, anyhow};
use borsh::BorshDeserialize;
use log::{info, error, warn}; // Removed unused debug
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    // commitment_config::CommitmentConfig, // Removed unused CommitmentConfig
    bs58,
    native_token,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{VersionedMessage, v0}, // Removed unused self
    transaction::VersionedTransaction,
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    system_instruction,
};
use spl_associated_token_account::{get_associated_token_address, instruction::create_associated_token_account};
use spl_token::ID as TOKEN_PROGRAM_ID;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value as JsonValue};
use reqwest::Client as ReqwestClient;
use num_bigint::BigUint;
use num_traits::{CheckedSub, Zero, ToPrimitive}; // Added ToPrimitive
use crate::errors::PumpfunError;
// use crate::models::wallet::{/*WalletKeys,*/ /*WalletInfo*/}; // Removed unused wallet models
use crate::wallet::load_keys_from_file;
use crate::config::CONFIG;
use crate::pump_instruction_builders::{
    BondingCurveAccount,
    find_bonding_curve_pda,
    calculate_tokens_out,
    calculate_sol_out,
    apply_slippage_to_tokens_out,
    build_pump_buy_instruction,
    build_pump_sell_instruction,
    GLOBAL_STATE_PUBKEY,
    // Remove unused imports from here if confirmed later
    // PUMPFUN_PROGRAM_ID,
    // apply_slippage_to_sol_cost,
    // find_bonding_curve_sol_vault_pda,
    // SYSTEM_PROGRAM_ID,
    GlobalState,
};
// use solana_sdk::message::v0::MessageAddressTableLookup; // Removed unused ALT lookup message part

use tokio::sync::mpsc::UnboundedSender; // Import UnboundedSender

/// Status updates from pump operations
#[derive(Debug, Clone)]
pub enum PumpStatus {
    Idle,
    InProgress(String),
    Success(String),
    Failure(String),
}

pub async fn pump_token(
    mint_str: String,
    buy_amount_sol: f64,
    sell_threshold_sol: f64,
    slippage_bps: u64,
    priority_fee_lamports: u64,
    jito_tip_lamports: Option<u64>,
    lookup_table_str: Option<String>,
    global_keys_path: Option<&str>, // Renamed for clarity
    pump_keys_path: Option<String>, // New arg
    private_key_str: Option<String>, // New arg, renamed for clarity
    is_running: Arc<AtomicBool>, // Added for toggle
    status_sender: UnboundedSender<PumpStatus>, // Changed sender type and message type
) -> Result<()> {
    status_sender.send(PumpStatus::InProgress(format!("Initiating pump command for mint: {}", mint_str))).ok();
    status_sender.send(PumpStatus::InProgress(format!("Buy Amount (SOL): {}", buy_amount_sol))).ok();
    status_sender.send(PumpStatus::InProgress(format!("Sell Threshold (SOL): {}", sell_threshold_sol))).ok();
    status_sender.send(PumpStatus::InProgress(format!("Slippage (bps): {}", slippage_bps))).ok();
    status_sender.send(PumpStatus::InProgress(format!("Priority Fee (lamports): {}", priority_fee_lamports))).ok();
    if let Some(tip) = jito_tip_lamports {
        status_sender.send(PumpStatus::InProgress(format!("Jito Tip (lamports): {}", tip))).ok();
    }
    if let Some(lut) = &lookup_table_str {
        status_sender.send(PumpStatus::InProgress(format!("Using Lookup Table: {}", lut))).ok();
    }
    status_sender.send(PumpStatus::InProgress(format!("Using keys file: {}", global_keys_path.unwrap_or("./keys.json")))).ok();

    // --- Configuration & Wallet Loading ---
    // Access global config directly
    let config = &CONFIG;
    
    let payer_keypair = load_payer_keypair(&private_key_str, &pump_keys_path, global_keys_path)
        .context("Failed to load payer keypair")?;
    info!("Using payer keypair: {}", payer_keypair.pubkey());

    // --- Client Setup ---
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        config.solana_rpc_url.clone(), // Use field from CONFIG
        config.get_commitment_config()?,
    ));
    let payer_balance = rpc_client.get_balance(&payer_keypair.pubkey()).await?;
    info!("Payer balance: {} SOL", payer_balance as f64 / 1_000_000_000.0);

    // --- Argument Parsing & Validation ---
    let mint_pubkey = Pubkey::from_str(&mint_str)
        .map_err(|e| anyhow!("Invalid mint address '{}': {}", mint_str, e))?;
    let lookup_table = lookup_table_str
        .map(|s| Pubkey::from_str(&s))
        .transpose()
        .map_err(|e| anyhow!("Invalid lookup table address: {}", e))?;

    // --- Pre-fetch ALT and prepare Vec (like launch_buy.rs, handling Option) ---
    let alt_lookup_vec: Vec<AddressLookupTableAccount> = match lookup_table {
        Some(addr) => {
            info!("Fetching Address Lookup Table account: {}", addr);
            let alt_account_data = rpc_client.get_account_data(&addr).await?;
            let alt_state = AddressLookupTable::deserialize(&alt_account_data)
                 .map_err(|e| anyhow!("Failed to deserialize ALT account state {}: {}", addr, e))?;
            info!("Successfully deserialized ALT state for {}. Contains {} addresses.", addr, alt_state.addresses.len());
            // Construct the struct and put it in a Vec
            vec![AddressLookupTableAccount { 
                key: addr,
                addresses: alt_state.addresses.to_vec(),
            }]
        }
        None => {
            info!("No lookup table provided.");
            vec![] // Create an empty Vec if no ALT address was given
        }
    };

    // --- Get Pump.fun Global State & Pool Info ---
    info!("Fetching Pump.fun global state...");
    let global_state_account_data = rpc_client.get_account_data(&GLOBAL_STATE_PUBKEY).await
        .context("Failed to fetch Global State account data")?;
    
    // Deserialize the Global State, skipping the 8-byte discriminator
    const DISCRIMINATOR_LEN: usize = 8;
    if global_state_account_data.len() <= DISCRIMINATOR_LEN {
        return Err(anyhow!("Global state account data too short to contain discriminator and data ({} bytes)", global_state_account_data.len()));
    }
    let global_state = GlobalState::deserialize(&mut &global_state_account_data[DISCRIMINATOR_LEN..])
        .context("Failed to deserialize Global State")?;
    info!("  Deserialized Global State: Fee Basis Points = {}, Fee Recipient = {}", global_state.fee_basis_points, global_state.fee_recipient);

    info!("Fetching bonding curve info for mint: {}", mint_pubkey);
    let (bonding_curve_pda, _bump) = find_bonding_curve_pda(&mint_pubkey)?;
    info!("Derived bonding curve PDA: {}", bonding_curve_pda);

    let bonding_curve_account_data = rpc_client.get_account_data(&bonding_curve_pda).await?;
    

    // Deserialize bonding curve account (skip Anchor discriminator) without requiring exact slice length
    let mut curve_data = &bonding_curve_account_data[8..];
    let mut bonding_curve = BondingCurveAccount::deserialize(&mut curve_data)
        .context("Failed to deserialize BondingCurveAccount data")?;

    info!("Bonding Curve Info:");
    info!("  Virtual SOL Reserves: {}", bonding_curve.virtual_sol_reserves);
    info!("  Virtual Token Reserves: {}", bonding_curve.virtual_token_reserves);
    info!("  Real SOL Reserves: {}", bonding_curve.real_sol_reserves);
    info!("  Real Token Reserves: {}", bonding_curve.real_token_reserves);
    info!("  Total Token Supply: {}", bonding_curve.token_total_supply);
    info!("  Is Complete: {}", bonding_curve.complete);

    if bonding_curve.complete {
        warn!("Bonding curve is marked as complete. Buys/sells may not be possible.");
        // Decide if we should exit or proceed cautiously.
        // return Err(anyhow!("Cannot pump on a completed bonding curve."));
    }



    // --- Pumping Iteration ---
    status_sender.send(PumpStatus::InProgress("Pump started".to_string())).unwrap();
    // Prepare Jito variables for bundle submission
    let mut jito_url: String = String::new();
    let mut http_client: ReqwestClient = ReqwestClient::new();
    let mut request_payload: JsonValue = json!({});
    if is_running.load(Ordering::Relaxed) {
        // --- Build Buy Transaction ---
        info!("Building buy transaction...");
        let buy_amount_lamports = (buy_amount_sol * native_token::LAMPORTS_PER_SOL as f64) as u64;
        info!("  Buy Amount (lamports): {}", buy_amount_lamports);

        // 1. Calculate expected tokens out using deserialized state
        let expected_tokens_out = calculate_tokens_out(
            buy_amount_lamports,
            bonding_curve.virtual_sol_reserves,
            bonding_curve.virtual_token_reserves,
            global_state.fee_basis_points, // Use deserialized value
        )?;
        info!("  Expected Tokens Out (before slippage): {}", expected_tokens_out);

        // 2. Apply slippage to get minimum tokens out
        let min_tokens_out = apply_slippage_to_tokens_out(
            expected_tokens_out,
            slippage_bps, // User-provided slippage
        );
        info!("  Minimum Tokens Out (after {} bps slippage): {}", slippage_bps, min_tokens_out);

        // 3. Prepare accounts needed for buy instruction (referencing build_pump_buy_instruction definition)
        let buyer_pk = payer_keypair.pubkey();
        let buyer_ata = get_associated_token_address(&buyer_pk, &mint_pubkey);
        // let (bonding_curve_sol_vault_pda, _) = find_bonding_curve_sol_vault_pda(&bonding_curve_pda)?;

        // Initialize the master instruction list for the single transaction
        let mut all_instructions: Vec<Instruction> = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000), // Maximize CU limit for combined TX
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports),
        ];
        info!("Checking if buyer ATA {} exists...", buyer_ata);
        match rpc_client.get_account(&buyer_ata).await {
            Ok(_) => {
                info!("  Buyer ATA already exists.");
            }
            Err(solana_client::client_error::ClientError { kind: solana_client::client_error::ClientErrorKind::RpcError(solana_client::rpc_request::RpcError::ForUser(ref message)), .. })
                if message.starts_with("AccountNotFound") || message.contains("could not find account") => {
                warn!("  Buyer ATA not found. Adding creation instruction.");
                let create_ata_ix = create_associated_token_account(
                    &payer_keypair.pubkey(),    // Payer
                    &buyer_pk,                 // Owner of the ATA
                    &mint_pubkey,              // Token Mint
                    &TOKEN_PROGRAM_ID,         // Token Program ID
                );
                // instructions.push(create_ata_ix); // Add to the combined list instead
                all_instructions.push(create_ata_ix);
            }
            Err(e) => {
                // Different error, potentially transient RPC issue, bubble it up
                error!("Error checking buyer ATA: {}", e);
                return Err(anyhow!("Failed to check buyer ATA {}: {}", buyer_ata, e));
            }
        }

        // 4. Build buy instruction
        info!("Building pump buy instruction...");
        let buy_ix = build_pump_buy_instruction(
            &buyer_pk,
            &mint_pubkey,
            &bonding_curve_pda,
            &bonding_curve.creator, // creator_vault_pk from fetched bonding curve
            min_tokens_out,
            buy_amount_lamports
        )?;
        // instructions.push(buy_ix); // Add buy instruction after potential ATA creation - Add to combined list
        all_instructions.push(buy_ix);

        // 5. Compute budget / priority fee moved to the start of instructions vec

        // --- Build Sell Transactions ---
        info!("Preparing sell transactions...");

        // 1. Determine Token Amount Bought (Using fallback for now)
        let total_tokens_to_sell = min_tokens_out; // Use min_tokens_out as fallback
        if total_tokens_to_sell == 0 {
            warn!("Calculated 0 tokens bought, skipping sell transactions.");
            // Potentially proceed only with the buy bundle? Or return error?
            // For now, let's continue to bundle sending logic, which will just contain the buy.
        } else {
            info!("Estimated total tokens bought (minimum): {}", total_tokens_to_sell);

            // 2. Calculate Sell Batches
            let sell_threshold_lamports = (sell_threshold_sol * native_token::LAMPORTS_PER_SOL as f64) as u64;
            info!("Target maximum SOL per sell: {} lamports ({:.6} SOL)", sell_threshold_lamports, sell_threshold_sol);

            // Find token amount per batch using calculate_sol_out
            // We use the current bonding curve state for this calculation.
            // A more accurate approach would refetch the curve state after the buy.
            let batch_token_amount = find_sell_batch_size(
                sell_threshold_lamports,
                &bonding_curve,
                global_state.fee_basis_points, // Pass deserialized value directly
            ).context("Failed to determine sell batch size")?;

            if batch_token_amount == 0 {
                error!("Calculated sell batch size is 0. Cannot proceed with sells.");
                // This might happen if even 1 token sells for more than the threshold.
                return Err(anyhow!("Sell threshold is too low, cannot create sell batches."));
            }
            info!("Determined token amount per sell batch: {}", batch_token_amount);

            let num_full_batches = total_tokens_to_sell / batch_token_amount;
            let remainder_amount = total_tokens_to_sell % batch_token_amount;
            let total_sell_txs = num_full_batches + if remainder_amount > 0 { 1 } else { 0 };
            info!("Need {} sell instructions ({} full batches, {} remainder)",
                total_sell_txs, num_full_batches, remainder_amount);

            // 3. Build Sell Instructions & Add to `all_instructions`
            for i in 0..total_sell_txs {
                let tokens_for_this_tx = if i < num_full_batches {
                    batch_token_amount
                } else {
                    remainder_amount
                };

                if tokens_for_this_tx == 0 { continue; }

                info!("Building sell instruction {}/{}: Selling {} tokens...", i + 1, total_sell_txs, tokens_for_this_tx);
                info!("  Current Curve State: V_SOL={}, V_TOK={}", bonding_curve.virtual_sol_reserves, bonding_curve.virtual_token_reserves);

                // Calculate expected SOL out (net) for this batch using deserialized state
                let expected_sol_out_net = calculate_sol_out(
                    tokens_for_this_tx,
                    bonding_curve.virtual_sol_reserves,
                    bonding_curve.virtual_token_reserves,
                    global_state.fee_basis_points, // Use deserialized value
                )?;

                // Recalculate Gross SOL out for curve state update
                let sol_increase_gross = {
                    let k_sol = BigUint::from(bonding_curve.virtual_sol_reserves);
                    let k_token = BigUint::from(bonding_curve.virtual_token_reserves);
                    let k = k_sol.clone() * k_token;
                    let current_tokens = BigUint::from(bonding_curve.virtual_token_reserves);
                    let tokens_to_sell = BigUint::from(tokens_for_this_tx);
                    // Ensure we don't subtract more than available
                    let new_virtual_token_big = current_tokens.checked_sub(&tokens_to_sell)
                        .ok_or_else(|| PumpfunError::Calculation("Calculation resulted in negative SOL out (gross) for state update".to_string()))?;

                    if new_virtual_token_big.is_zero() {
                        // Handle selling the exact remaining amount - gross increase is the current virtual SOL?
                        // This edge case needs careful thought based on exact curve math.
                        // For now, let's proceed assuming it doesn't hit zero, or use the net value as fallback.
                        warn!("Sell results in zero virtual tokens. Using net SOL for state update (approximation).");
                        BigUint::from(expected_sol_out_net) // Approximation
                    } else {
                        let new_virtual_sol_big = &k / new_virtual_token_big;
                        new_virtual_sol_big.checked_sub(&k_sol)
                           .ok_or_else(|| PumpfunError::Calculation("Calculation resulted in negative SOL out (gross) for state update".to_string()))?
                    }
                };
                let sol_increase_gross_u64 = sol_increase_gross.to_u64().unwrap_or(u64::MAX); // Handle potential overflow

                // Apply slippage to get minimum *net* SOL out for the instruction
                let min_sol_output_net = apply_slippage_to_tokens_out(expected_sol_out_net, slippage_bps);
                info!("  Expected SOL Out (Net): {}, Min SOL Out (Net): {}, Gross SOL Increase (Curve): {}",
                    expected_sol_out_net, min_sol_output_net, sol_increase_gross_u64);

                // Build sell instruction using min_sol_output_net
                let sell_ix = build_pump_sell_instruction(
                    &payer_keypair.pubkey(),
                    &mint_pubkey,
                    &bonding_curve_pda,
                    tokens_for_this_tx,
                    min_sol_output_net
                )?;

                // Add the sell instruction to the master list
                all_instructions.push(sell_ix);
                info!("  Added sell instruction #{} to the main transaction.", i + 1);

                // Update local bonding curve state for the *next* iteration
                bonding_curve.virtual_token_reserves = bonding_curve.virtual_token_reserves.saturating_sub(tokens_for_this_tx);
                bonding_curve.virtual_sol_reserves = bonding_curve.virtual_sol_reserves.saturating_add(sol_increase_gross_u64);
                info!("  Updated Curve State: V_SOL={}, V_TOK={}", bonding_curve.virtual_sol_reserves, bonding_curve.virtual_token_reserves);
            }
        }

        // --- Add Jito Tip Instruction (if requested) BEFORE compiling the message ---
        if let Some(tip_amount) = jito_tip_lamports {
            if tip_amount > 0 {
                info!("Adding Jito tip instruction for {} lamports.", tip_amount);
                let tip_account_str = env::var("JITO_TIP_ACCOUNT")
                     .map_err(|_| anyhow!("JITO_TIP_ACCOUNT env var not set but --jito-tip provided"))?;
                let tip_account_pk = Pubkey::from_str(&tip_account_str)
                    .context("Failed to parse JITO_TIP_ACCOUNT pubkey")?;

                let tip_ix = system_instruction::transfer(
                    &payer_keypair.pubkey(),
                    &tip_account_pk,
                    tip_amount,
                );
                // Add the tip instruction to the list
                all_instructions.push(tip_ix);
                info!("Added Jito tip instruction to the main transaction list.");
            }
        }
        // --- End Add Tip Instruction ---

        // --- Create ONE Transaction for the Bundle ---
        info!("Creating single transaction with {} total instructions...", all_instructions.len());
        warn!("WARNING: Combining many instructions into one transaction may exceed size or compute limits!");

        // Fetch latest blockhash for the combined transaction
        let combined_blockhash = rpc_client.get_latest_blockhash().await.context("Failed to fetch blockhash for combined transaction")?;
        info!("Using blockhash: {:?} for the combined transaction", combined_blockhash);

        let combined_message = VersionedMessage::V0(v0::Message::try_compile(
            &payer_keypair.pubkey(),
            &all_instructions, // Use the list with ALL instructions
            &alt_lookup_vec,
            combined_blockhash,
        )?);

        let combined_tx = VersionedTransaction::try_new(combined_message, &[&payer_keypair])
            .context("Failed to create the combined versioned transaction (check size limit?)")?;
        info!("Created signed versioned transaction with all instructions.");

        // Skipping transaction simulation; proceeding directly to send bundle

        // Prepare the bundle (will contain only the single combined transaction)
        let bundle_transactions = vec![combined_tx];

        // --- Bundle Transactions (Original logic, now just handles the single TX + optional tip) ---
        info!("Finalizing bundle with {} transaction(s)...", bundle_transactions.len());
        if bundle_transactions.is_empty() {
            warn!("No transactions to bundle. Exiting.");
            return Ok(());
        }

        // --- Temporarily Comment Out Jito Tip Logic ---
        /*
        // Add Jito tip instruction if requested
        let mut final_bundle_txs = bundle_transactions; // Clone or move ownership
        if let Some(tip_amount) = jito_tip_lamports {
            if tip_amount > 0 {
                info!("Adding Jito tip of {} lamports.", tip_amount);
                let tip_account_str = env::var("JITO_TIP_ACCOUNT")
                     .map_err(|_| anyhow!("JITO_TIP_ACCOUNT env var not set but --jito-tip provided"))?;
                let tip_account_pk = Pubkey::from_str(&tip_account_str)?;

                let tip_ix = system_instruction::transfer(
                    &payer_keypair.pubkey(),
                    &tip_account_pk,
                    tip_amount,
                );

                // Tip needs its own transaction - Jito requires the tip TX to be the *last* one in the bundle
                // It also typically does NOT need to be signed by the user, but check Jito docs.
                // Let's create it unsigned for now.
                let tip_latest_blockhash = rpc_client.get_latest_blockhash().await?;
                let tip_message = VersionedMessage::V0(v0::Message::try_compile(
                    &payer_keypair.pubkey(),
                    &[tip_ix], // Only the transfer instruction
                    &[], // No LUT needed for simple transfer
                    tip_latest_blockhash,
                )?);

                // Create the transaction WITHOUT signing it yet.
                // Jito bundles usually expect the user-signed TXs first, then the unsigned tip TX.
                let tip_tx = VersionedTransaction {
                    signatures: vec![], // No signatures yet
                    message: tip_message
                };

                final_bundle_txs.push(tip_tx); // Add tip TX at the end
                info!("Added Jito tip transaction to bundle.");
            }
        }
        */
        // Use the original bundle_transactions directly since tip is handled within combined_tx
        let final_bundle_txs = bundle_transactions;
        // --- End Comment Out ---

        // --- Send Bundle ---
        info!("Serializing and sending bundle...");
        // Prepare Jito endpoint
        jito_url = env::var("JITO_JSON_RPC_URL")
            .map_err(|_| anyhow!("JITO_JSON_RPC_URL env var not set"))?;

        // Serialize transactions to base64
        let serialized_txs: Vec<String> = final_bundle_txs.iter()
            .map(|tx| {
                let tx_bytes = bincode::serialize(tx)?;
                Ok(BASE64_STANDARD.encode(tx_bytes))
            })
            .collect::<Result<Vec<String>>>()?;

        // Construct Jito sendBundle request ACCORDING TO DOCUMENTATION
        request_payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [ // Outer array for params
                serialized_txs, // First element: the Vec<String> which becomes the inner JSON array
                json!({ "encoding": "base64" }) // Second element: options object as JSON value
            ]
        });

        // --- DEBUG: Log the request payload ---
        match serde_json::to_string_pretty(&request_payload) {
            Ok(json_string) => info!("Sending JSON payload to Jito:\n{}", json_string),
            Err(e) => warn!("Failed to serialize request payload for logging: {}", e),
        }
        // --- END DEBUG ---

        // Send request

        http_client = ReqwestClient::new();
        time::sleep(Duration::from_secs(3)).await; // Delay before sending bundle
    }
    status_sender.send(PumpStatus::Success("Pump stopped".to_string())).unwrap();
    info!("Sending bundle to Jito endpoint: {}", jito_url);
    
    let response = http_client.post(&jito_url)
        .json(&request_payload)
        .send()
        .await?;

    // Process response
    let response_status = response.status();
    let response_text: String = response.text().await?;
    
    if response_status.is_success() {
        info!("Jito response status: {}", response_status);
        info!("Jito response body: {}", response_text);
        // Attempt to parse the bundle ID
        match serde_json::from_str::<JsonValue>(&response_text) {
            Ok(json_response) => {
                if let Some(bundle_id) = json_response.get("result").and_then(|r| r.as_str()) {
                    info!("✅ Successfully sent bundle! Bundle ID: {}", bundle_id);
                    println!("Bundle ID: {}", bundle_id); // Also print plainly for user
                } else if let Some(error) = json_response.get("error") {
                    error!("Jito RPC returned an error: {:?}", error);
                    return Err(anyhow!("Jito RPC error: {:?}", error));
                } else {
                    warn!("Could not parse bundle ID from Jito response.");
                }
            }
            Err(e) => {
                warn!("Failed to parse Jito JSON response: {}. Body: {}", e, response_text);
            }
        }
    } else {
        error!("Jito request failed! Status: {}", response_status);
        error!("Jito response body: {}", response_text);
        return Err(anyhow!("Jito bundle submission failed with status {}", response_status));
    }

    info!("Pump command finished.");

    Ok(())
}

/// Helper function to load the designated payer keypair based on provided arguments.
fn load_payer_keypair(
    private_key_str: &Option<String>,
    pump_keys_path: &Option<String>,
    global_keys_path: Option<&str>,
) -> Result<Keypair> {
    if let Some(pk_str) = private_key_str {
        info!("Loading keypair from provided private key string.");
        let bytes = bs58::decode(pk_str).into_vec()?;
        Keypair::from_bytes(&bytes)
            .map_err(|e| anyhow!("Failed to construct keypair from bytes: {}", e))
    } else {
        let default_path = PathBuf::from("./keys.json");
        let effective_keys_path = pump_keys_path
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| global_keys_path.map(PathBuf::from))
            .unwrap_or(default_path);
        
        info!("Loading keypair from file: {}", effective_keys_path.display());
        
        // Use the correct function
        let wallet_keys = load_keys_from_file(&effective_keys_path)?;
        
        // Extract the first wallet's private key
        if let Some(first_wallet) = wallet_keys.wallets.first() { 
            info!("Using first wallet found in file: {}", first_wallet.public_key);
            // Decode base58 first, then use from_bytes for error handling
            let pk_bytes = bs58::decode(&first_wallet.private_key).into_vec()?;
            Keypair::from_bytes(&pk_bytes)
                 .map_err(|e| anyhow!("Failed to construct keypair from bytes for wallet {} from file {}: {}", 
                                       first_wallet.public_key, effective_keys_path.display(), e))
        } else {
            Err(anyhow!("No wallets found in file: {}", effective_keys_path.display()))
        }
    }
}

/// Helper to find the max token amount that sells for less than the threshold.
/// Uses binary search for efficiency.
fn find_sell_batch_size(
    sell_threshold_lamports: u64,
    bonding_curve: &BondingCurveAccount,
    global_fee_bps: u64,
) -> Result<u64> {
    if sell_threshold_lamports == 0 { 
        warn!("Sell threshold is 0 lamports, returning batch size 0.");
        return Ok(0); 
    }
    if bonding_curve.virtual_token_reserves == 0 {
        warn!("Bonding curve has 0 virtual token reserves, returning batch size 0.");
        return Ok(0);
    }

    let mut low: u64 = 1; // Minimum possible batch size
    let mut high: u64 = bonding_curve.virtual_token_reserves; // Maximum possible batch size
    let mut best_batch_size: u64 = 0;

    info!("Starting binary search for sell batch size (Range: {} - {})", low, high);

    // Check if selling 1 token already exceeds the threshold
    match calculate_sol_out(1, bonding_curve.virtual_sol_reserves, bonding_curve.virtual_token_reserves, global_fee_bps) {
        Ok(sol_out_for_one) => {
            if sol_out_for_one >= sell_threshold_lamports {
                warn!("Selling 1 token (yields {} lamports) already meets/exceeds threshold {}. Batch size will be 0.", sol_out_for_one, sell_threshold_lamports);
                return Ok(0);
            }
            // If selling 1 is okay, we know best_batch_size is at least 1
            best_batch_size = 1;
        }
        Err(PumpfunError::Calculation(msg)) if msg.contains("cannot divide by zero") => {
             warn!("Calculation error (divide by zero?) when checking SOL out for 1 token. Returning 0 batch size.");
             return Ok(0); // Cannot even calculate for 1 token
        }
        Err(e) => {
            error!("Error calculating SOL out for 1 token: {}", e);
            return Err(anyhow!(e).context("Failed initial SOL calculation in batch size search"));
        }
    }

    while low <= high {
        // Prevent overflow when calculating mid, especially if high is near u64::MAX
        let mid = low + (high - low) / 2;
        
        // Avoid infinite loop if mid doesn't change (can happen if low/high are stuck)
        if mid == 0 { break; } // Should not happen if low starts at 1, but safety first
        if mid == best_batch_size && mid == low { break; } // Avoid getting stuck if best_batch_size stalls low end

        // info!("  Testing batch size: {} (Low: {}, High: {})", mid, low, high); // Debug log

        match calculate_sol_out(
            mid, 
            bonding_curve.virtual_sol_reserves, 
            bonding_curve.virtual_token_reserves, 
            global_fee_bps
        ) {
            Ok(sol_out) => {
                if sol_out < sell_threshold_lamports {
                    // This batch size is valid (under threshold)
                    // We might be able to go higher, so store this as a potential best
                    // and move the lower bound up.
                    best_batch_size = mid; 
                    // info!("    Yields {} lamports (< threshold {}). Valid. New best: {}. Increasing low to {}", sol_out, sell_threshold_lamports, best_batch_size, mid + 1); // Debug log
                    low = mid.saturating_add(1); 
                } else {
                    // This batch size is too high (meets or exceeds threshold)
                    // We need to try smaller sizes.
                    // info!("    Yields {} lamports (>= threshold {}). Invalid. Decreasing high to {}", sol_out, sell_threshold_lamports, mid - 1); // Debug log
                    high = mid.saturating_sub(1);
                }
            }
            Err(PumpfunError::Calculation(msg)) if msg.contains("cannot divide by zero") => {
                 // This implies mid >= virtual_token_reserves, which means 'high' was too high.
                 warn!("Sell calculation hit divide by zero for batch size {}. Reducing high.", mid);
                 high = mid.saturating_sub(1);
            }
            Err(e) => {
                error!("Error during sell batch size binary search for {} tokens: {}", mid, e);
                // Decide whether to bail or try to continue. Bailing is safer.
                return Err(anyhow!(e).context(format!("Failed during binary search calculation for batch size {}", mid)));
            }
        }
        // Check if bounds crossed
        if low > high { break; }
    }

    info!("Binary search finished. Determined optimal sell batch size: {}", best_batch_size);
    Ok(best_batch_size)
}