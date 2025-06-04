use log::{error, info, warn, debug};
use crate::config::{get_commitment_config, get_rpc_url};
use crate::errors::{PumpfunError, Result};
use crate::utils::transaction::{send_jito_bundle_via_json_rpc, sign_and_send_versioned_transaction, get_jito_bundle_statuses};
// Unused: use crate::wallet::{get_wallet_keypair};

// Raydium specific imports will be added back if Exchange::Raydium is chosen
use reqwest::Client as ReqwestClient; // For Raydium API calls

use crate::pump_instruction_builders::{
    build_pump_buy_instruction,
    find_bonding_curve_pda,
    BondingCurveAccount,
};
use borsh::BorshDeserialize;
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero, CheckedSub};

use anyhow::Context; // Keep anyhow
use console::Style; // Keep console
// ReqwestClient might be unneeded if send_jito_bundle_via_json_rpc handles HTTP
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::{
    // instruction::Instruction, // May not be needed directly
    native_token::{LAMPORTS_PER_SOL},
    pubkey::Pubkey,
    system_instruction::{self},
};
use solana_sdk::{
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    compute_budget::ComputeBudgetInstruction,
    message::{v0::{Message as MessageV0}, VersionedMessage},
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
    commitment_config::CommitmentConfig, // Added
};
use spl_associated_token_account::get_associated_token_address;
use spl_associated_token_account::instruction::create_associated_token_account;
use spl_token::{self, native_mint::ID as NATIVE_SOL_MINT, state::Account as SplTokenAccount}; // Use NATIVE_SOL_MINT consistently
use solana_program::program_pack::Pack as ProgramPack; // Import Pack from solana_program
use std::str::FromStr;
use std::sync::Arc;
use std::env;
use std::time::Duration; // Added for ATA creation delay
use tokio::time::sleep; // Added for ATA creation delay
use solana_client::rpc_config::{RpcSimulateTransactionConfig}; // Added
use solana_transaction_status::UiTransactionEncoding; // Added

// --- Exchange Enum ---
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exchange {
    PumpFun,
    Raydium,
}

// --- Constants ---
// const JITO_TIP_ACCOUNTS: [&str; 8] = [ ... ];
const COMPUTE_UNIT_LIMIT_BUY_PUMPFUN: u32 = 600_000; // As per snippet for buy+tip
const COMPUTE_UNIT_LIMIT_ATA_CREATE: u32 = 50_000; // Retain for clarity
const MAX_TRANSACTIONS_PER_BUNDLE: usize = 5; // Max total transactions Jito allows in a bundle
const MAX_BUYS_PER_PUMPFUN_BUNDLE: usize = 4; // Max "buy" transactions in a PumpFun bundle
const BONDING_CURVE_DISCRIMINATOR_LENGTH: usize = 8;
const DEFAULT_PUMPFUN_FEE_BPS: u64 = 100; // 1% fee
const BUNDLE_CONFIRMATION_POLL_ATTEMPTS: u32 = 20; // Max polls for a single bundle submission
const BUNDLE_CONFIRMATION_POLL_DELAY_MS: u64 = 3000; // 3 seconds between polls
const BUNDLE_RESUBMISSION_ATTEMPTS: u32 = 3; // Max attempts to resubmit a bundle for a chunk
 
 // --- Calculation Helpers (Copied from user snippet) ---
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

fn apply_slippage_to_sol_cost(
    expected_cost: u64,
    slippage_basis_points: u64,
) -> u64 {
    let numerator = expected_cost as u128 * (10000u128 + slippage_basis_points as u128);
    let denominator = 10000u128;
    let max_cost = (numerator + denominator - 1) / denominator; // Ceiling division
    max_cost.try_into().unwrap_or(u64::MAX).max(expected_cost)
}

// --- Simulation Helper (Copied from user snippet, adapted for Arc<RpcClient>) ---
async fn simulate_and_check(
    rpc_client: &Arc<RpcClient>, // Changed to Arc<RpcClient>
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
        inner_instructions: false, // Set to true if needed for debugging pump.fun interactions
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


/// Executes an atomic buy for specified wallets on Pump.fun, each with a specific SOL amount,
/// split into multiple transactions within a single Jito bundle.
pub async fn atomic_buy(
    exchange: Exchange, // New parameter
    mint_address_str: String,
    participating_wallets_with_amounts: Vec<(Keypair, f64)>,
    slippage_bps: u64,
    priority_fee_lamports_per_tx: u64,
    lookup_table_address_str: Option<String>,
    jito_tip_sol_per_tx: f64, // Used for Pump.fun per-tx tip
    jito_overall_tip_sol: f64, // Used for Raydium bundle-wide tip
    jito_block_engine_url: String,
    jito_tip_account_pubkey_str: String,
) -> Result<()> {
    let info_style = Style::new().cyan();
    let success_style = Style::new().green();
    let warning_style = Style::new().yellow();
    let error_style = Style::new().red();

    let action_description = match exchange {
        Exchange::PumpFun => "Pump.fun Atomic Buy Jito Bundle",
        Exchange::Raydium => "Raydium Atomic Buy Jito Bundle",
    };
    println!(
        "\n{}",
        info_style.apply_to(format!("⚛️  Preparing Multi-Wallet {}...", action_description)).bold()
    );

    // --- Configuration & Initialization ---
    let rpc_url = get_rpc_url();
    let commitment_config = get_commitment_config();
    let rpc_client = Arc::new(RpcClient::new_with_commitment(rpc_url.clone(), commitment_config));
    let http_client = Arc::new(ReqwestClient::new()); // For Raydium API
    
    // This will be the target token mint for both exchanges
    let target_mint_pubkey = Pubkey::from_str(&mint_address_str)
        .map_err(|e| PumpfunError::InvalidInput(format!("Invalid target mint address format '{}': {}", mint_address_str, e)))?;
    
    let tip_account_pubkey = Pubkey::from_str(&jito_tip_account_pubkey_str)
        .map_err(|e| PumpfunError::Config(format!("Invalid Jito Tip Account Pubkey from settings '{}': {}", jito_tip_account_pubkey_str, e)))?;
    
    // --- Verify Target Mint Exists ---
    info!("Verifying if target mint {} exists...", target_mint_pubkey);
    match rpc_client.get_account(&target_mint_pubkey).await {
        Ok(_) => {
            info!("Target mint {} confirmed to exist.", target_mint_pubkey);
        }
        Err(e) => {
            error!("Failed to fetch target mint account {}: {}. This token may not exist or there's an RPC issue.", target_mint_pubkey, e);
            return Err(PumpfunError::Token(format!(
                "Target mint {} does not exist or could not be fetched. RPC error: {}",
                target_mint_pubkey, e
            )));
        }
    }

    // Initialize lookup_tables
    let mut active_lookup_tables: Vec<AddressLookupTableAccount> = Vec::new();
    if let Some(addr_str) = &lookup_table_address_str {
        if !addr_str.trim().is_empty() {
            info!("User provided ALT Address: {}", addr_str);
            let lookup_table_pubkey = Pubkey::from_str(addr_str.trim())
                .map_err(|e| PumpfunError::InvalidInput(format!("Invalid user lookup table address format '{}': {}", addr_str, e)))?;
            
            let alt_account_data = rpc_client.get_account_data(&lookup_table_pubkey).await
                .context(format!("Failed to fetch user-provided ALT account data for {}", lookup_table_pubkey))?;
            let alt = AddressLookupTable::deserialize(&alt_account_data)
                .map_err(|e| PumpfunError::Deserialization(format!("Failed to deserialize user-provided ALT account {}: {}", lookup_table_pubkey, e)))?;
            info!("Successfully fetched and deserialized user-provided ALT {} containing {} addresses.", lookup_table_pubkey, alt.addresses.len());
            active_lookup_tables.push(AddressLookupTableAccount {
                key: lookup_table_pubkey,
                addresses: alt.addresses.to_vec(),
            });
        }
    }

    println!("Target Mint to Buy ({}): {}", format!("{:?}", exchange), target_mint_pubkey);
    println!("Slippage (Basis Points): {}", slippage_bps);
    println!("Priority Fee per Tx (Microlamports): {}", priority_fee_lamports_per_tx);
    println!("Jito Tip Account (from settings): {}", tip_account_pubkey);
    
    if exchange == Exchange::PumpFun {
        println!("Jito Tip Amount per Tx in Bundle (Pump.fun - embedded in each buy tx): {:.6} SOL ({} lamports)", jito_tip_sol_per_tx, (jito_tip_sol_per_tx * LAMPORTS_PER_SOL as f64) as u64);
        if jito_overall_tip_sol > 0.0 {
            println!("Overall Jito Tip Amount per Bundle (Pump.fun - separate tx): {:.6} SOL ({} lamports)", jito_overall_tip_sol, (jito_overall_tip_sol * LAMPORTS_PER_SOL as f64) as u64);
        }
    } else { // Raydium
        println!("Overall Jito Tip Amount per Bundle (Raydium - separate tx): {:.6} SOL ({} lamports)", jito_overall_tip_sol, (jito_overall_tip_sol * LAMPORTS_PER_SOL as f64) as u64);
    }


    if participating_wallets_with_amounts.is_empty() {
        return Err(PumpfunError::Wallet("No participating wallets provided.".into()));
    }
    println!("Processing {} participating wallets for {} buy.", participating_wallets_with_amounts.len(), format!("{:?}", exchange));

    // global_bundle_transactions removed as bundles are sent per chunk for both exchanges.
    let mut total_successful_buy_preparations_overall = 0; // For Raydium, tracks total successful buys for the single bundle

    if exchange == Exchange::PumpFun {
        // Fetch PUMPFUN_FEE_BPS from env or use default
        let fee_bps_str = env::var("PUMPFUN_FEE_BPS").unwrap_or_else(|_| DEFAULT_PUMPFUN_FEE_BPS.to_string());
        let pump_fee_bps = fee_bps_str.parse::<u64>().unwrap_or(DEFAULT_PUMPFUN_FEE_BPS);
        info!("Using Pump.fun fee basis points: {}", pump_fee_bps);

        // Fetch Bonding Curve PDA
        let (bonding_curve_pk, _) = find_bonding_curve_pda(&target_mint_pubkey)?;
        info!("Derived Pump.fun Bonding Curve PDA for {}: {}", target_mint_pubkey, bonding_curve_pk);

        // --- Verify Bonding Curve Account Exists ---
        info!("Verifying if bonding curve account {} exists...", bonding_curve_pk);
        if rpc_client.get_account(&bonding_curve_pk).await.is_err() {
            error!(
                "Bonding curve account {} does not exist or could not be fetched for token {}. This token may not be a pump.fun token or its curve is not initialized.",
                bonding_curve_pk, target_mint_pubkey
            );
            return Err(PumpfunError::Token(format!(
                "Failed to find or access bonding curve for token {}.\n\
Possible reasons:\n\
  - The token address may be incorrect.\n\
  - The token may not be a pump.fun token.\n\
  - The pump.fun bonding curve may not have been initialized yet.\n\
Please verify the token address and its status on pump.fun.\n\
Associated bonding curve PDA checked: {}.",
                target_mint_pubkey, bonding_curve_pk
            )));
        }
        info!("Bonding curve account {} confirmed to exist.", bonding_curve_pk);

        let jito_tip_lamports_per_tx = (jito_tip_sol_per_tx * LAMPORTS_PER_SOL as f64) as u64;
        let overall_jito_tip_lamports = (jito_overall_tip_sol * LAMPORTS_PER_SOL as f64) as u64;

        for (chunk_index, wallet_chunk) in participating_wallets_with_amounts.chunks(MAX_BUYS_PER_PUMPFUN_BUNDLE).enumerate() {
            info!("Processing Pump.fun wallet chunk #{} ({} wallets)", chunk_index + 1, wallet_chunk.len());
            let mut current_chunk_bundle_transactions: Vec<VersionedTransaction> = Vec::with_capacity(MAX_TRANSACTIONS_PER_BUNDLE);
            let mut successful_buys_in_chunk = 0;
            let mut bundle_tip_payer_keypair_for_chunk: Option<Keypair> = None; // To store an owned Keypair for the tip

            for (wallet_index_in_chunk, (wallet_keypair_ref, sol_amount_to_spend)) in wallet_chunk.iter().enumerate() {
                if current_chunk_bundle_transactions.len() >= MAX_BUYS_PER_PUMPFUN_BUNDLE {
                    warn!("{}", warning_style.apply_to(format!(
                        "Reached max buy transactions ({}) for Pump.fun chunk #{}, skipping remaining {} wallets in this chunk.",
                        MAX_BUYS_PER_PUMPFUN_BUNDLE,
                        chunk_index + 1,
                        wallet_chunk.len() - wallet_index_in_chunk
                    )));
                    break;
                }

                let buyer_pk = wallet_keypair_ref.pubkey();
                info!("  Processing Pump.fun buy for wallet #{} in chunk #{} ({})", wallet_index_in_chunk + 1, chunk_index + 1, buyer_pk);

                let lamports_to_spend_for_this_wallet = (*sol_amount_to_spend * LAMPORTS_PER_SOL as f64) as u64;
                if lamports_to_spend_for_this_wallet == 0 {
                    warn!("    Skipping wallet {} due to zero SOL amount.", buyer_pk);
                    continue;
                }
                info!("    Amount to spend: {:.6} SOL ({} lamports)", sol_amount_to_spend, lamports_to_spend_for_this_wallet);

                // --- 1. Check/Create Buyer's Associated Token Account (ATA) ---
                let buyer_ata_pk = get_associated_token_address(&buyer_pk, &target_mint_pubkey);
                info!("    Checking buyer ATA: {}", buyer_ata_pk);
                if rpc_client.get_account(&buyer_ata_pk).await.is_err() {
                    info!("    Buyer ATA {} not found. Creating...", buyer_ata_pk);
                    let ata_creation_ix = create_associated_token_account(&buyer_pk, &buyer_pk, &target_mint_pubkey, &spl_token::id());
                    let latest_blockhash_ata = rpc_client.get_latest_blockhash().await.context("Failed to get blockhash for ATA creation")?;
                    let ata_tx_msg = MessageV0::try_compile(
                        &buyer_pk,
                        &[ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT_ATA_CREATE), ata_creation_ix],
                        &active_lookup_tables,
                        latest_blockhash_ata,
                    ).map_err(|e| PumpfunError::Build(format!("Failed to compile ATA creation message: {}", e)))?;
                    let ata_tx = VersionedTransaction::try_new(VersionedMessage::V0(ata_tx_msg), &[wallet_keypair_ref])
                        .map_err(|e| PumpfunError::Signing(format!("Failed to sign ATA creation transaction: {}", e)))?;

                    if let Err(sim_err) = simulate_and_check(&rpc_client, &ata_tx, "ATA Creation TX", commitment_config).await {
                        error!("    ATA creation simulation failed for wallet {}: {}. Skipping this wallet.", buyer_pk, sim_err);
                        continue;
                    }
                    match sign_and_send_versioned_transaction(&rpc_client, ata_tx, &[wallet_keypair_ref]).await {
                        Ok(signature) => {
                            info!("    ✅ ATA Creation for {} Confirmed! Sig: {}", buyer_ata_pk, signature);
                            sleep(Duration::from_secs(5)).await;
                            let mut ata_verified = false;
                            for attempt in 1..=3 {
                                if rpc_client.get_account(&buyer_ata_pk).await.is_ok() {
                                    info!("    ATA {} confirmed after creation (attempt {}).", buyer_ata_pk, attempt);
                                    ata_verified = true;
                                    break;
                                }
                                warn!("    ATA {} not yet found after creation (attempt {}). Retrying in 2s...", buyer_ata_pk, attempt);
                                sleep(Duration::from_secs(2)).await;
                            }
                            if !ata_verified {
                                error!("    ATA {} could not be confirmed after creation. Skipping wallet.", buyer_ata_pk);
                                continue;
                            }
                        }
                        Err(e) => { error!("    ❌ ATA Creation for {} Failed: {}. Skipping.", buyer_ata_pk, e); continue; }
                    }
                } else {
                    info!("    Buyer ATA {} already exists.", buyer_ata_pk);
                }

                // --- 2. Fetch Current Bonding Curve State ---
                let bonding_curve_data = rpc_client.get_account_data(&bonding_curve_pk).await?;
                const EXPECTED_BONDING_CURVE_STRUCT_SIZE: usize = 73;
                let data_offset = BONDING_CURVE_DISCRIMINATOR_LENGTH;
                if bonding_curve_data.len() < data_offset + EXPECTED_BONDING_CURVE_STRUCT_SIZE {
                    error!("    Bonding curve data too short for {}. Skipping.", buyer_pk); continue;
                }
                let struct_data_slice = &bonding_curve_data[data_offset..(data_offset + EXPECTED_BONDING_CURVE_STRUCT_SIZE)];
                let bonding_curve_state = match BondingCurveAccount::try_from_slice(struct_data_slice) {
                    Ok(state) => state,
                    Err(e) => { error!("    Failed to deserialize bonding curve for {}: {}. Skipping.", buyer_pk, e); continue; }
                };
                info!("    Curve state for {}: virtual_sol={}, virtual_token={}", buyer_pk, bonding_curve_state.virtual_sol_reserves, bonding_curve_state.virtual_token_reserves);

                // --- 3. Calculate Pump.fun Buy Arguments ---
                let expected_tokens_out = match calculate_tokens_out(lamports_to_spend_for_this_wallet, bonding_curve_state.virtual_sol_reserves, bonding_curve_state.virtual_token_reserves, pump_fee_bps) {
                    Ok(tokens) if tokens > 0 => tokens,
                    Ok(_) => { warn!("    Calculated 0 tokens for {}. Skipping.", buyer_pk); continue; }
                    Err(e) => { error!("    Error calculating tokens for {}: {}. Skipping.", buyer_pk, e); continue; }
                };
                let max_sol_cost_for_this_buy = apply_slippage_to_sol_cost(lamports_to_spend_for_this_wallet, slippage_bps);
                info!("    Buy for {}: expected_tokens={}, max_sol_cost={}", buyer_pk, expected_tokens_out, max_sol_cost_for_this_buy);

                // --- 4. Build Pump.fun Buy Transaction ---
                let (creator_vault_pk, _) = match crate::pump_instruction_builders::find_creator_vault_pda(&bonding_curve_state.creator) {
                    Ok(pda) => pda,
                    Err(e) => { error!("    Failed to find creator_vault_pda for {}: {}. Skipping.", buyer_pk, e); continue; }
                };
                let pump_buy_ix = match build_pump_buy_instruction(&buyer_pk, &target_mint_pubkey, &bonding_curve_pk, &creator_vault_pk, expected_tokens_out, max_sol_cost_for_this_buy) {
                    Ok(ix) => ix,
                    Err(e) => { error!("    Failed to build buy ix for {}: {}. Skipping.", buyer_pk, e); continue; }
                };
                let mut instructions_for_this_tx = vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT_BUY_PUMPFUN),
                    ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports_per_tx),
                    pump_buy_ix,
                ];
                if jito_tip_lamports_per_tx > 0 {
                    instructions_for_this_tx.push(system_instruction::transfer(&buyer_pk, &tip_account_pubkey, jito_tip_lamports_per_tx));
                }
                let latest_blockhash_buy = rpc_client.get_latest_blockhash().await.context("Failed to get blockhash for Pump.fun buy")?;
                let buy_tx_msg_v0 = match MessageV0::try_compile(&buyer_pk, &instructions_for_this_tx, &active_lookup_tables, latest_blockhash_buy) {
                    Ok(msg) => msg,
                    Err(e) => { error!("    Failed to compile buy msg for {}: {}. Skipping.", buyer_pk, e); continue; }
                };
                let buy_tx = match VersionedTransaction::try_new(VersionedMessage::V0(buy_tx_msg_v0), &[wallet_keypair_ref]) {
                    Ok(tx) => tx,
                    Err(e) => { error!("    Failed to sign buy tx for {}: {}. Skipping.", buyer_pk, e); continue; }
                };

                // --- 5. Simulate Pump.fun Buy Transaction ---
                if let Err(sim_err) = simulate_and_check(&rpc_client, &buy_tx, &format!("Pump.fun Buy TX for {}", buyer_pk), commitment_config).await {
                    error!("    Pump.fun buy simulation failed for {}: {}. Skipping.", buyer_pk, sim_err);
                    continue;
                }

                current_chunk_bundle_transactions.push(buy_tx);
                successful_buys_in_chunk += 1;
                if bundle_tip_payer_keypair_for_chunk.is_none() {
                    // Store an owned copy of the keypair for the tip payer of this chunk
                    bundle_tip_payer_keypair_for_chunk = Some(Keypair::from_bytes(&wallet_keypair_ref.to_bytes()).expect("Failed to clone keypair for tip"));
                }
                info!("    Successfully prepared Pump.fun buy for {} in chunk #{}", buyer_pk, chunk_index + 1);
            } // End wallet loop for current chunk

            // Add overall Jito tip for this chunk if applicable
            if !current_chunk_bundle_transactions.is_empty() && overall_jito_tip_lamports > 0 {
                if let Some(tip_payer_keypair) = &bundle_tip_payer_keypair_for_chunk { // Use the owned keypair
                    if current_chunk_bundle_transactions.len() < MAX_TRANSACTIONS_PER_BUNDLE {
                        info!("  Adding overall Jito tip ({} lamports) to Pump.fun chunk #{} paid by {}", overall_jito_tip_lamports, chunk_index + 1, tip_payer_keypair.pubkey());
                        let tip_tx_blockhash = rpc_client.get_latest_blockhash().await.context("Failed to get blockhash for Pump.fun chunk tip")?;
                        let tip_instruction = system_instruction::transfer(&tip_payer_keypair.pubkey(), &tip_account_pubkey, overall_jito_tip_lamports);
                        let tip_message = MessageV0::try_compile(&tip_payer_keypair.pubkey(), &[tip_instruction], &active_lookup_tables, tip_tx_blockhash)?;
                        let tip_transaction = VersionedTransaction::try_new(VersionedMessage::V0(tip_message), &[tip_payer_keypair])?; // Sign with the owned keypair
                        current_chunk_bundle_transactions.push(tip_transaction);
                    } else {
                        warn!("  Skipping overall Jito tip for Pump.fun chunk #{} as bundle is full ({} txs).", chunk_index + 1, current_chunk_bundle_transactions.len());
                    }
                } else {
                     warn!("  Skipping overall Jito tip for Pump.fun chunk #{} as no successful buys to pay for it.", chunk_index + 1);
                }
            }

            // Send the current chunk's bundle
            if !current_chunk_bundle_transactions.is_empty() {
                let mut bundle_confirmed_for_chunk = false;
                for resubmit_attempt in 0..BUNDLE_RESUBMISSION_ATTEMPTS {
                    if resubmit_attempt > 0 {
                        info!("{}", warning_style.apply_to(format!("Resubmitting Jito bundle for Pump.fun chunk #{} (Attempt {}/{})", chunk_index + 1, resubmit_attempt + 1, BUNDLE_RESUBMISSION_ATTEMPTS)));
                        // Consider if blockhash refresh is needed for transactions before resubmitting.
                        // For now, resending the same serialized transactions.
                        sleep(Duration::from_millis(1000)).await; // Brief pause before resubmitting
                    }

                    info!("Sending Jito bundle for Pump.fun chunk #{} ({} txs, {} buys, Resubmit attempt {}) to {}", chunk_index + 1, current_chunk_bundle_transactions.len(), successful_buys_in_chunk, resubmit_attempt + 1, jito_block_engine_url);
                    match send_jito_bundle_via_json_rpc(current_chunk_bundle_transactions.clone(), &jito_block_engine_url).await {
                        Ok(bundle_id) => {
                            println!(
                                "\n{}",
                                info_style.apply_to(format!("Pump.fun Jito Bundle for Chunk #{} (Submission Attempt {}) Submitted. Bundle ID: {}. Waiting for confirmation...", chunk_index + 1, resubmit_attempt + 1, bundle_id)).bold()
                            );
                            println!("  Polling for Jito bundle {} confirmation (Max {} attempts, {}ms delay)...", bundle_id, BUNDLE_CONFIRMATION_POLL_ATTEMPTS, BUNDLE_CONFIRMATION_POLL_DELAY_MS);

                            let mut poll_attempt = 0;
                            loop {
                                if poll_attempt >= BUNDLE_CONFIRMATION_POLL_ATTEMPTS {
                                    println!("{}", warning_style.apply_to(format!("  Bundle {} for chunk #{} did NOT confirm after {} polls. Will attempt resubmission if available.", bundle_id, chunk_index + 1, BUNDLE_CONFIRMATION_POLL_ATTEMPTS)));
                                    break; // Break polling loop, go to next resubmission attempt
                                }
                                poll_attempt += 1;
                                debug!("Polling status for bundle {} (Poll attempt {}/{})", bundle_id, poll_attempt, BUNDLE_CONFIRMATION_POLL_ATTEMPTS);
                                println!("  Attempting to get status for bundle {} (Poll {}/{})", bundle_id, poll_attempt, BUNDLE_CONFIRMATION_POLL_ATTEMPTS);
                                sleep(Duration::from_millis(BUNDLE_CONFIRMATION_POLL_DELAY_MS)).await;

                                match get_jito_bundle_statuses(vec![bundle_id.clone()], &jito_block_engine_url).await {
                                    Ok(Some(status_result)) => {
                                        if let Some(bundle_info) = status_result.value.iter().find(|b| b.bundle_id == bundle_id) {
                                            let mut jito_reported_error_details = "None".to_string();
                                            let mut is_jito_error = false;
                                            if let Some(err_obj) = &bundle_info.err {
                                                if err_obj.ok.is_none() {
                                                    jito_reported_error_details = format!("{:?}", err_obj);
                                                    is_jito_error = true;
                                                    error!("Jito reported an error for bundle {}: {}", bundle_id, jito_reported_error_details);
                                                }
                                            }
                                            println!("  Bundle {} Status: Confirmation='{}', JitoError='{}', Details: {}", bundle_id, bundle_info.confirmation_status, is_jito_error, if is_jito_error {&jito_reported_error_details} else {"N/A"});

                                            if is_jito_error {
                                                println!("{}", warning_style.apply_to(format!("  Bundle {} failed due to Jito error. Will attempt resubmission.", bundle_id)));
                                                break; // Break polling, try resubmission
                                            }

                                            match bundle_info.confirmation_status.to_lowercase().as_str() {
                                                "confirmed" | "finalized" => {
                                                    println!(
                                                        "{}",
                                                        success_style.apply_to(format!("✅ Bundle {} for Chunk #{} Confirmed (Status: {})!", bundle_id, chunk_index + 1, bundle_info.confirmation_status)).bold()
                                                    );
                                                    bundle_confirmed_for_chunk = true;
                                                    total_successful_buy_preparations_overall += successful_buys_in_chunk;
                                                    break; // Break polling loop
                                                }
                                                "processed" => {
                                                    println!("  Bundle {} is 'processed'. Continuing to poll for 'confirmed' or 'finalized'.", bundle_id);
                                                }
                                                other_status => {
                                                    println!("  Bundle {} status is '{}'. Continuing to poll.", bundle_id, other_status);
                                                }
                                            }
                                        } else {
                                            println!("  Bundle {} not found in Jito status results yet (Poll {}). Continuing to poll.", bundle_id, poll_attempt);
                                        }
                                    }
                                    Ok(None) => {
                                        println!("  Bundle {} not yet found by Jito (getBundleStatuses returned null result) (Poll {}). Continuing to poll.", bundle_id, poll_attempt);
                                    }
                                    Err(e) => {
                                        println!("{}", warning_style.apply_to(format!("  Error fetching status for bundle {} (Poll {}): {}. Will retry polling.", bundle_id, poll_attempt, e)));
                                    }
                                }
                                if bundle_confirmed_for_chunk {
                                    break; // Break polling loop
                                }
                            } // End polling loop

                            if bundle_confirmed_for_chunk {
                                break; // Break resubmission loop
                            }
                        }
                        Err(e) => {
                            error!("Failed to send Jito bundle for Pump.fun chunk #{} (Submission Attempt {}): {}", chunk_index + 1, resubmit_attempt + 1, e);
                            println!("{}", warning_style.apply_to(format!("  Submission Attempt {} for Pump.fun chunk #{} failed: {}. Retrying if attempts remain.", resubmit_attempt + 1, chunk_index + 1, e )));
                            if resubmit_attempt < BUNDLE_RESUBMISSION_ATTEMPTS - 1 {
                                sleep(Duration::from_millis(BUNDLE_CONFIRMATION_POLL_DELAY_MS)).await; // Wait before next resubmission
                            }
                        }
                    }
                } // End resubmission loop

                if !bundle_confirmed_for_chunk {
                     println!("{}", error_style.apply_to(format!("❌ Failed to confirm Jito bundle for Pump.fun chunk #{} after {} submission attempts.", chunk_index + 1, BUNDLE_RESUBMISSION_ATTEMPTS)));
                } else {
                     println!("{}", success_style.apply_to(format!("Successfully processed and confirmed {} buy transactions in Pump.fun chunk #{}'s bundle.", successful_buys_in_chunk, chunk_index + 1)));
                }

            } else {
                println!("Pump.fun chunk #{} resulted in no transactions to send.", chunk_index + 1);
            }
        } // End chunk loop for PumpFun
 
        if total_successful_buy_preparations_overall == 0 && !participating_wallets_with_amounts.is_empty() {
             return Err(PumpfunError::Bundling("No Pump.fun buy transactions were successfully confirmed across all chunks.".to_string()));
        }
        println!("Finished processing all Pump.fun chunks. Total successful and confirmed buys: {}", total_successful_buy_preparations_overall);
        return Ok(());
 
    } else if exchange == Exchange::Raydium {
        // error_style is already defined at the top of the function
        info!("Attempting Raydium buy logic with chunking and bundle confirmation...");
        let overall_jito_tip_lamports = (jito_overall_tip_sol * LAMPORTS_PER_SOL as f64) as u64;

        // Try to get launchpad market info once. This will determine the strategy within chunks.
        let launchpad_program_id_str = "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj";
        let launchpad_program_id = Pubkey::from_str(launchpad_program_id_str)
            .expect("Static Raydium Launchpad Program ID should be valid");

        let market_keys_option = match crate::api::raydium_launchpad::get_launchpad_market_info(&rpc_client, &launchpad_program_id, &target_mint_pubkey, &NATIVE_SOL_MINT).await {
            Ok(Some(keys)) => {
                info!("Using Raydium Launchpad market keys: {:?}", keys);
                Some(keys)
            }
            Ok(None) => {
                info!("No Raydium Launchpad market info for {}. Will use generic Raydium swap API.", target_mint_pubkey);
                None
            }
            Err(e) => {
                error!("Error fetching Raydium Launchpad market info for {}: {}. Aborting Raydium processing.", target_mint_pubkey, e);
                return Err(PumpfunError::Api(format!("Failed to get Raydium Launchpad market info: {}", e)));
            }
        };

        // MAX_BUYS_PER_PUMPFUN_BUNDLE is reused here for consistency (4 buys per chunk)
        for (chunk_index, wallet_chunk) in participating_wallets_with_amounts.chunks(MAX_BUYS_PER_PUMPFUN_BUNDLE).enumerate() {
            info!("Processing Raydium wallet chunk #{} ({} wallets)", chunk_index + 1, wallet_chunk.len());
            let mut current_chunk_bundle_transactions: Vec<VersionedTransaction> = Vec::with_capacity(MAX_TRANSACTIONS_PER_BUNDLE);
            let mut successful_buys_in_chunk = 0;
            let mut bundle_tip_payer_keypair_for_chunk: Option<Keypair> = None;

            for (wallet_index_in_chunk, (wallet_keypair_ref, sol_amount_to_spend)) in wallet_chunk.iter().enumerate() {
                let buyer_pk = wallet_keypair_ref.pubkey();
                info!("  Processing Raydium buy for wallet #{} in chunk #{} ({})", wallet_index_in_chunk + 1, chunk_index + 1, buyer_pk);

                let lamports_to_spend_for_this_wallet = (*sol_amount_to_spend * LAMPORTS_PER_SOL as f64) as u64;
                if lamports_to_spend_for_this_wallet == 0 {
                    warn!("    Skipping wallet {} due to zero SOL amount.", buyer_pk);
                    continue;
                }
                info!("    Amount to spend: {:.6} SOL ({} lamports)", sol_amount_to_spend, lamports_to_spend_for_this_wallet);

                // --- ATA Check for Target Token (common for both Raydium paths) ---
                let buyer_target_token_ata = get_associated_token_address(&buyer_pk, &target_mint_pubkey);
                info!("    Checking buyer's target token ATA: {}", buyer_target_token_ata);
                if rpc_client.get_account(&buyer_target_token_ata).await.is_err() {
                    info!("    Target ATA {} not found. Creating...", buyer_target_token_ata);
                    let create_ata_ix = create_associated_token_account(&buyer_pk, &buyer_pk, &target_mint_pubkey, &spl_token::id());
                    let latest_blockhash_ata = rpc_client.get_latest_blockhash().await.context("Failed to get blockhash for target ATA creation")?;
                    let ata_tx_msg = MessageV0::try_compile(&buyer_pk, &[ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT_ATA_CREATE), create_ata_ix], &active_lookup_tables, latest_blockhash_ata)?;
                    let ata_tx = VersionedTransaction::try_new(VersionedMessage::V0(ata_tx_msg), &[wallet_keypair_ref])?;
                    if let Err(sim_err) = simulate_and_check(&rpc_client, &ata_tx, &format!("Create Target ATA for {}", buyer_pk), commitment_config).await {
                        error!("    Target ATA creation simulation failed for {}: {}. Skipping.", buyer_pk, sim_err); continue;
                    }
                    match sign_and_send_versioned_transaction(&rpc_client, ata_tx, &[wallet_keypair_ref]).await {
                        Ok(sig) => { info!("    ✅ Target ATA {} created. Sig: {}", buyer_target_token_ata, sig); sleep(Duration::from_secs(5)).await; }
                        Err(e) => { error!("    ❌ Target ATA creation failed for {}: {}. Skipping.", buyer_target_token_ata, e); continue; }
                    }
                } else { info!("    Buyer's target token ATA {} already exists.", buyer_target_token_ata); }


                let mut prepared_txs_for_this_wallet_buy: Vec<VersionedTransaction> = Vec::new();

                if let Some(market_keys) = &market_keys_option {
                    // --- Raydium Launchpad Logic ---
                    info!("    Using Raydium Launchpad logic for wallet {}", buyer_pk);
                    let wsol_mint = NATIVE_SOL_MINT;
                    let buyer_wsol_ata = get_associated_token_address(&buyer_pk, &wsol_mint);
                    // (WSOL ATA setup logic - condensed)
                    let mut wsol_ata_setup_instructions: Vec<solana_program::instruction::Instruction> = Vec::new();
                    let mut needs_wsol_setup_tx = false;
                    // ... (Full WSOL ATA check, creation, funding logic as in the original Raydium block)
                    match rpc_client.get_account(&buyer_wsol_ata).await {
                        Ok(wsol_account_data) => {
                            match SplTokenAccount::unpack_from_slice(&wsol_account_data.data) {
                                Ok(wsol_token_account) => {
                                    if wsol_token_account.amount < lamports_to_spend_for_this_wallet {
                                        let deficit = lamports_to_spend_for_this_wallet.saturating_sub(wsol_token_account.amount);
                                        wsol_ata_setup_instructions.push(system_instruction::transfer(&buyer_pk, &buyer_wsol_ata, deficit));
                                        wsol_ata_setup_instructions.push(spl_token::instruction::sync_native(&spl_token::id(), &buyer_wsol_ata)?);
                                        needs_wsol_setup_tx = true;
                                    }
                                }
                                Err(_) => {
                                    wsol_ata_setup_instructions.push(create_associated_token_account(&buyer_pk, &buyer_pk, &wsol_mint, &spl_token::id()));
                                    wsol_ata_setup_instructions.push(system_instruction::transfer(&buyer_pk, &buyer_wsol_ata, lamports_to_spend_for_this_wallet));
                                    wsol_ata_setup_instructions.push(spl_token::instruction::sync_native(&spl_token::id(), &buyer_wsol_ata)?);
                                    needs_wsol_setup_tx = true;
                                }
                            }
                        }
                        Err(_) => {
                            wsol_ata_setup_instructions.push(create_associated_token_account(&buyer_pk, &buyer_pk, &wsol_mint, &spl_token::id()));
                            wsol_ata_setup_instructions.push(system_instruction::transfer(&buyer_pk, &buyer_wsol_ata, lamports_to_spend_for_this_wallet));
                            wsol_ata_setup_instructions.push(spl_token::instruction::sync_native(&spl_token::id(), &buyer_wsol_ata)?);
                            needs_wsol_setup_tx = true;
                        }
                    }

                    if needs_wsol_setup_tx && !wsol_ata_setup_instructions.is_empty() {
                        let mut final_wsol_setup_instructions = vec![ ComputeBudgetInstruction::set_compute_unit_limit(200_000), ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports_per_tx)];
                        final_wsol_setup_instructions.extend(wsol_ata_setup_instructions);
                        let latest_blockhash_wsol_setup = rpc_client.get_latest_blockhash().await?;
                        let wsol_setup_tx_msg = MessageV0::try_compile(&buyer_pk, &final_wsol_setup_instructions, &active_lookup_tables, latest_blockhash_wsol_setup)?;
                        let wsol_setup_tx = VersionedTransaction::try_new(VersionedMessage::V0(wsol_setup_tx_msg), &[wallet_keypair_ref])?;
                        if simulate_and_check(&rpc_client, &wsol_setup_tx, &format!("WSOL ATA Setup for Launchpad {}", buyer_pk), commitment_config).await.is_err() { continue; }
                        if sign_and_send_versioned_transaction(&rpc_client, wsol_setup_tx, &[wallet_keypair_ref]).await.is_err() { continue; }
                        sleep(Duration::from_secs(7)).await;
                        // Re-verify WSOL balance
                         if let Ok(acc_data) = rpc_client.get_account(&buyer_wsol_ata).await {
                            if let Ok(token_acc) = SplTokenAccount::unpack_from_slice(&acc_data.data) {
                                if token_acc.amount < lamports_to_spend_for_this_wallet {
                                    error!("    WSOL ATA {} has insufficient balance {} after setup for Launchpad. Skipping.", buyer_wsol_ata, token_acc.amount); continue;
                                }
                            } else { error!("    Failed to unpack WSOL ATA {} after setup for Launchpad. Skipping.", buyer_wsol_ata); continue; }
                        } else { error!("    Failed to confirm WSOL ATA {} after setup for Launchpad. Skipping.", buyer_wsol_ata); continue; }
                    }
                    // Build Launchpad Buy Tx
                    let min_amount_out_placeholder: u64 = 1; // Placeholder
                    match crate::api::raydium_launchpad::build_raydium_launchpad_buy_transaction(wallet_keypair_ref, market_keys, &wsol_mint, &target_mint_pubkey, lamports_to_spend_for_this_wallet, min_amount_out_placeholder, Some(priority_fee_lamports_per_tx)).await {
                        Ok(instructions) => {
                            let latest_blockhash = rpc_client.get_latest_blockhash().await?;
                            let message = MessageV0::try_compile(&buyer_pk, &instructions, &active_lookup_tables, latest_blockhash)?;
                            let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[wallet_keypair_ref])?;
                            if simulate_and_check(&rpc_client, &tx, &format!("Raydium Launchpad Buy for {}", buyer_pk), commitment_config).await.is_ok() {
                                prepared_txs_for_this_wallet_buy.push(tx);
                            } else { error!("    Raydium Launchpad buy simulation failed for {}. Skipping.", buyer_pk); continue; }
                        }
                        Err(e) => { error!("    Failed to build Raydium Launchpad buy for {}: {}. Skipping.", buyer_pk, e); continue; }
                    }
                } else {
                    // --- Generic Raydium Swap Logic ---
                    info!("    Using generic Raydium Swap API for wallet {}", buyer_pk);
                    let wsol_mint = spl_token::native_mint::id();
                    let raydium_quote = match crate::api::raydium::get_raydium_quote(&http_client, wsol_mint, target_mint_pubkey, lamports_to_spend_for_this_wallet, slippage_bps as u16, "V0", "ExactIn").await {
                        Ok(quote) => quote,
                        Err(e) => { error!("    Raydium quote API failed for {}: {}. Skipping.", buyer_pk, e); continue; }
                    };
                    let fetched_api_txs = match crate::api::raydium::get_raydium_swap_transactions(&http_client, raydium_quote.clone(), buyer_pk, Some(priority_fee_lamports_per_tx), "V0", true, target_mint_pubkey == NATIVE_SOL_MINT, None, Some(buyer_target_token_ata)).await {
                        Ok(txs) if !txs.is_empty() => txs,
                        Ok(_) => { error!("    Raydium swap API returned no transactions for {}. Skipping.", buyer_pk); continue; }
                        Err(e) => { error!("    Raydium swap API failed for {}: {}. Skipping.", buyer_pk, e); continue; }
                    };
                    for (tx_idx, tx_from_api) in fetched_api_txs.into_iter().enumerate() {
                        if !tx_from_api.message.is_signer(tx_from_api.message.static_account_keys().iter().position(|k| k == &buyer_pk).unwrap_or(usize::MAX)) {
                            error!("    Wallet {} not signer in Raydium API tx #{}. Skipping.", buyer_pk, tx_idx + 1); prepared_txs_for_this_wallet_buy.clear(); break;
                        }
                        match VersionedTransaction::try_new(tx_from_api.message.clone(), &[wallet_keypair_ref]) {
                            Ok(signed_tx) => {
                                if simulate_and_check(&rpc_client, &signed_tx, &format!("Raydium Swap API TX #{} for {}", tx_idx + 1, buyer_pk), commitment_config).await.is_ok() {
                                    prepared_txs_for_this_wallet_buy.push(signed_tx);
                                } else { error!("    Raydium swap API tx #{} simulation failed for {}. Skipping.", tx_idx + 1, buyer_pk); prepared_txs_for_this_wallet_buy.clear(); break; }
                            }
                            Err(e) => { error!("    Failed to re-sign Raydium API tx #{} for {}: {}. Skipping.", tx_idx + 1, buyer_pk, e); prepared_txs_for_this_wallet_buy.clear(); break; }
                        }
                    }
                }

                // Add successfully prepared transactions for this wallet to the current chunk's bundle if they fit
                if !prepared_txs_for_this_wallet_buy.is_empty() {
                    // Max buy "operations" per chunk is MAX_BUYS_PER_PUMPFUN_BUNDLE (4). This is implicitly handled by the outer loop.
                    // Now check if the *actual transactions* fit within MAX_TRANSACTIONS_PER_BUNDLE (5).
                    let space_for_tip = if overall_jito_tip_lamports > 0 { 1 } else { 0 };
                    let num_txs_for_this_wallet_buy = prepared_txs_for_this_wallet_buy.len(); // Store length before move
                    if current_chunk_bundle_transactions.len() + num_txs_for_this_wallet_buy <= MAX_TRANSACTIONS_PER_BUNDLE - space_for_tip {
                        current_chunk_bundle_transactions.extend(prepared_txs_for_this_wallet_buy); // .clone() no longer strictly needed here if not used after, but extend consumes it.
                        successful_buys_in_chunk += 1; // This wallet's buy operation is counted as one successful buy for the chunk
                        if bundle_tip_payer_keypair_for_chunk.is_none() {
                            bundle_tip_payer_keypair_for_chunk = Some(Keypair::from_bytes(&wallet_keypair_ref.to_bytes()).expect("Failed to clone keypair for Raydium tip"));
                        }
                        info!("    Successfully prepared Raydium buy operation for {} ({} txs) in chunk #{}", buyer_pk, num_txs_for_this_wallet_buy, chunk_index + 1);
                    } else {
                        warn!("    Cannot fit Raydium transactions for wallet {} in current bundle ({} existing + {} new > max {} including space for tip). This buy operation will be skipped for this bundle.",
                              buyer_pk, current_chunk_bundle_transactions.len(), prepared_txs_for_this_wallet_buy.len(), MAX_TRANSACTIONS_PER_BUNDLE);
                        // This buy operation is skipped for this bundle. It won't be automatically retried in a new bundle by this loop structure.
                    }
                }
            } // End wallet loop for current Raydium chunk

            // Add overall Jito tip for this Raydium chunk if applicable
            if successful_buys_in_chunk > 0 && overall_jito_tip_lamports > 0 {
                if let Some(tip_payer_keypair) = &bundle_tip_payer_keypair_for_chunk {
                    if current_chunk_bundle_transactions.len() < MAX_TRANSACTIONS_PER_BUNDLE {
                        info!("  Adding overall Jito tip ({} lamports) to Raydium chunk #{} paid by {}", overall_jito_tip_lamports, chunk_index + 1, tip_payer_keypair.pubkey());
                        let tip_tx_blockhash = rpc_client.get_latest_blockhash().await.context("Failed to get blockhash for Raydium chunk tip")?;
                        let tip_instruction = system_instruction::transfer(&tip_payer_keypair.pubkey(), &tip_account_pubkey, overall_jito_tip_lamports);
                        let tip_message = MessageV0::try_compile(&tip_payer_keypair.pubkey(), &[tip_instruction], &active_lookup_tables, tip_tx_blockhash)?;
                        let tip_transaction = VersionedTransaction::try_new(VersionedMessage::V0(tip_message), &[tip_payer_keypair])?;
                        current_chunk_bundle_transactions.push(tip_transaction);
                    } else {
                        warn!("  Skipping overall Jito tip for Raydium chunk #{} as bundle is full ({} txs).", chunk_index + 1, current_chunk_bundle_transactions.len());
                    }
                } else {
                     warn!("  Skipping overall Jito tip for Raydium chunk #{} as no successful buys to pay for it.", chunk_index + 1);
                }
            }
// Send the Raydium chunk's bundle
if !current_chunk_bundle_transactions.is_empty() {
    let mut bundle_confirmed_for_chunk = false;
    for resubmit_attempt in 0..BUNDLE_RESUBMISSION_ATTEMPTS {
        if resubmit_attempt > 0 {
            info!("{}", warning_style.apply_to(format!("Resubmitting Jito bundle for Raydium chunk #{} (Attempt {}/{})", chunk_index + 1, resubmit_attempt + 1, BUNDLE_RESUBMISSION_ATTEMPTS)));
            sleep(Duration::from_millis(1000)).await;
        }

        info!("Sending Jito bundle for Raydium chunk #{} ({} txs, {} successful buy operations, Resubmit attempt {}) to {}", chunk_index + 1, current_chunk_bundle_transactions.len(), successful_buys_in_chunk, resubmit_attempt + 1, jito_block_engine_url);
        match send_jito_bundle_via_json_rpc(current_chunk_bundle_transactions.clone(), &jito_block_engine_url).await {
            Ok(bundle_id) => {
                println!(
                    "\n{}",
                    info_style.apply_to(format!("Raydium Jito Bundle for Chunk #{} (Attempt {}) Submitted. Bundle ID: {}. Waiting for confirmation...", chunk_index + 1, resubmit_attempt + 1, bundle_id)).bold()
                );

                let mut poll_attempt = 0;
                loop {
                    if poll_attempt >= BUNDLE_CONFIRMATION_POLL_ATTEMPTS {
                        warn!("{}", warning_style.apply_to(format!("Bundle {} for Raydium chunk #{} did not confirm after {} polls. Will attempt resubmission.", bundle_id, chunk_index + 1, BUNDLE_CONFIRMATION_POLL_ATTEMPTS)));
                        break;
                    }
                    poll_attempt += 1;
                    debug!("Polling status for Raydium bundle {} (Poll attempt {}/{})", bundle_id, poll_attempt, BUNDLE_CONFIRMATION_POLL_ATTEMPTS);
                    println!("  Polling status for Raydium bundle {} (Attempt {}/{})", bundle_id, poll_attempt, BUNDLE_CONFIRMATION_POLL_ATTEMPTS);
                    sleep(Duration::from_millis(BUNDLE_CONFIRMATION_POLL_DELAY_MS)).await;

                    match get_jito_bundle_statuses(vec![bundle_id.clone()], &jito_block_engine_url).await {
                        Ok(Some(status_result)) => {
                            if let Some(bundle_info) = status_result.value.iter().find(|b| b.bundle_id == bundle_id) {
                                let mut jito_reported_error = false;
                                if let Some(err_obj) = &bundle_info.err {
                                    if err_obj.ok.is_none() {
                                        error!("Jito reported an error for Raydium bundle {}: {:?}", bundle_id, bundle_info.err);
                                        jito_reported_error = true;
                                    }
                                }
                                println!("  Raydium Bundle {} Status: Confirmation='{}', JitoError='{}', RawErrField: {:?}", bundle_id, bundle_info.confirmation_status, jito_reported_error, bundle_info.err);

                                if jito_reported_error {
                                    warn!("{}", warning_style.apply_to(format!("Raydium Bundle {} failed according to Jito. Will attempt resubmission.", bundle_id)));
                                    break;
                                }

                                match bundle_info.confirmation_status.to_lowercase().as_str() {
                                    "confirmed" | "finalized" => {
                                        println!(
                                            "{}",
                                            success_style.apply_to(format!("✅ Raydium Bundle {} for Chunk #{} Confirmed (Status: {})!", bundle_id, chunk_index + 1, bundle_info.confirmation_status)).bold()
                                        );
                                        bundle_confirmed_for_chunk = true;
                                        total_successful_buy_preparations_overall += successful_buys_in_chunk;
                                        break;
                                    }
                                    "processed" => {
                                        info!("Raydium Bundle {} is processed, awaiting further confirmation.", bundle_id);
                                    }
                                    _ => {
                                        info!("Raydium Bundle {} status: {}. Continuing to poll.", bundle_id, bundle_info.confirmation_status);
                                    }
                                }
                            } else {
                                info!("Raydium Bundle {} not found in Jito status results yet (Poll {}). Continuing to poll.", bundle_id, poll_attempt);
                            }
                        }
                        Ok(None) => {
                            info!("Raydium Bundle {} not yet found by Jito (Poll {}). Continuing to poll.", bundle_id, poll_attempt);
                        }
                        Err(e) => {
                            warn!("Error fetching status for Raydium bundle {} (Poll {}): {}. Will retry polling.", bundle_id, poll_attempt, e);
                        }
                    }
                    if bundle_confirmed_for_chunk {
                        break;
                    }
                } // End polling loop

                if bundle_confirmed_for_chunk {
                    break; // Break resubmission loop
                }
            }
            Err(e) => {
                error!("Failed to send Jito bundle for Raydium chunk #{} (Resubmit attempt {}): {}", chunk_index + 1, resubmit_attempt + 1, e);
                if resubmit_attempt < BUNDLE_RESUBMISSION_ATTEMPTS - 1 {
                    sleep(Duration::from_millis(BUNDLE_CONFIRMATION_POLL_DELAY_MS)).await;
                }
            }
        }
    } // End resubmission loop
    
    if !bundle_confirmed_for_chunk {
        error!("{}", warning_style.apply_to(format!("Failed to confirm Jito bundle for Raydium chunk #{} after {} resubmission attempts.", chunk_index + 1, BUNDLE_RESUBMISSION_ATTEMPTS)));
    } else {
        println!("  Successfully processed and confirmed {} Raydium buy transactions in this chunk's bundle.", successful_buys_in_chunk);
    }

} else {
    info!("Raydium chunk #{} resulted in no transactions to send.", chunk_index + 1);
}
} // End chunk loop for Raydium

if total_successful_buy_preparations_overall == 0 && !participating_wallets_with_amounts.is_empty() {
return Err(PumpfunError::Bundling("No Raydium buy transactions were successfully confirmed across all chunks.".to_string()));
}
info!("Finished processing all Raydium chunks. Total successful and confirmed buy operations: {}", total_successful_buy_preparations_overall);

    
    } else { // Should not be reached if enum is handled exhaustively (e.g. if PumpFun was the only other variant)
        // This case handles if `exchange` is neither PumpFun nor Raydium, which shouldn't happen with current enum.
        // However, to be safe and ensure all paths return:
        error!("Invalid exchange type encountered: {:?}", exchange);
        return Err(PumpfunError::Config(format!("Unsupported exchange type: {:?}", exchange)));
    }

    Ok(())
}