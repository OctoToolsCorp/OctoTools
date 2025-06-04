// modern_app.rs
use eframe::egui::{self, Color32, FontId, Margin, Pos2, Rect, Rounding, Stroke, Vec2, Visuals};
use std::time::Duration;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::str::FromStr;
use std::path::Path;
use std::sync::Arc;
use std::fs::File;
use std::io::{Write, BufWriter};
use serde::Serialize;

use anyhow::{anyhow, Context, Result};
use log::{info, error, warn};
use tokio::sync::mpsc::{self, UnboundedSender, UnboundedReceiver};
use reqwest::Client as ReqwestClient;
use bs58;
// base64 and bincode are not used directly in modern_app.rs based on warnings, but might be used by dependencies or pasted code.
// For now, I will remove them as per the explicit unused import warnings for modern_app.rs.
// If compilation fails due to this, they might need to be re-added if used by code brought in from app.rs.
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use bincode;
use futures::stream::{StreamExt}; // Removed iter
use tokio::time::sleep;
use chrono::Utc;
use rfd::FileDialog;
use rand::Rng;
use rand::SeedableRng; // Added for StdRng::from_entropy
use rand::seq::SliceRandom; // Added for choose method

// Solana specific imports
use solana_client::{
    // rpc_client::RpcClient, // Unused in modern_app.rs
    nonblocking::rpc_client::RpcClient as AsyncRpcClient,
};
use solana_sdk::{
    address_lookup_table::{instruction as alt_instruction},
    commitment_config::{CommitmentConfig},
    compute_budget::{self},
    message::{v0::Message as MessageV0, VersionedMessage},
    native_token::{lamports_to_sol, sol_to_lamports},
    pubkey::Pubkey,
    signature::{Keypair, Signer}, // Removed Signature
    system_instruction,
    system_program,
    sysvar,
    transaction::{Transaction, VersionedTransaction},
};
use spl_associated_token_account::get_associated_token_address;
use spl_token;
use serde_json::Value as JsonValue;

#[derive(Serialize)]
struct WalletFileFormat {
    wallets: Vec<LoadedWalletInfo>,
}

// Crate specific imports
use egui_extras::{self, RetainedImage}; // Added for image loaders
use crate::models::wallet::WalletInfo as LoadedWalletInfo;
use crate::models::{LaunchParams, LaunchStatus, AltCreationStatus, settings::AppSettings}; // Added LaunchParams
// Unused: use crate::models::settings::JITO_BLOCK_ENGINE_ENDPOINTS; // Added for direct access
use crate::commands::pump::PumpStatus;
use crate::pump_instruction_builders::{
    find_bonding_curve_pda, find_metadata_pda,
    GLOBAL_STATE_PUBKEY, METADATA_PROGRAM_ID,
    PUMPFUN_PROGRAM_ID, PUMPFUN_MINT_AUTHORITY, FEE_RECIPIENT_PUBKEY, EVENT_AUTHORITY_PUBKEY,
};
use crate::config; // For get_rpc_url, etc.
use crate::utils::{self, transaction_history::{PnlSummary, calculate_pnl, get_transaction_history_for_token}}; // For transaction utils and PnL
use crate::api; // For pumpfun api calls


// --- Volume Bot Simulation Task ---
pub async fn run_volume_bot_simulation_task(
    rpc_url: String,
    wallets_file_path: String,
    token_mint_str: String,
    max_cycles: u32,
    slippage_bps: u16,
    priority_fee_lamports: u64, // This is the Solana compute budget priority fee
    initial_buy_sol_amount: f64, // Added parameter for initial buy SOL amount
    status_sender: UnboundedSender<Result<String, String>>,
    wallets_data_sender: UnboundedSender<Vec<LoadedWalletInfo>>,
    // Jito specific settings for volume bot
    use_jito_for_volume_bot: bool,
    jito_block_engine_url_vb: String,
    jito_tip_account_pk_str_vb: String,
    jito_tip_lamports_vb: u64,
) -> Result<(), anyhow::Error> {
    info!(
        "Starting Volume Bot Simulation Task (New Strategy): Wallets File: {}, Token: {}, Max Cycles: {}, Slippage: {} bps, Solana Priority Fee: {} lamports, Initial Buy SOL: {}, Use Jito: {}, Jito BE: {}, Jito Tip Acc: {}, Jito Tip: {} lamports",
        wallets_file_path, token_mint_str, max_cycles, slippage_bps, priority_fee_lamports, initial_buy_sol_amount, use_jito_for_volume_bot, jito_block_engine_url_vb, jito_tip_account_pk_str_vb, jito_tip_lamports_vb
    );
let _ = status_sender.send(Ok(format!("Volume Bot Task Started (New Strategy). Token: {}. Max Cycles: {}. Jito: {}", token_mint_str, max_cycles, use_jito_for_volume_bot)));

    let loaded_wallets: Vec<LoadedWalletInfo> = match File::open(&wallets_file_path) {
        Ok(file) => {
            let reader = std::io::BufReader::new(file);
            match serde_json::from_reader(reader) {
                Ok(w) => w,
                Err(e) => {
                    let err_msg = format!("Failed to parse wallets from {}: {}", wallets_file_path, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg.clone()));
                    return Err(anyhow!(err_msg));
                }
            }
        }
        Err(e) => {
            let err_msg = format!("Failed to open wallets file {}: {}", wallets_file_path, e);
            error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg.clone()));
            return Err(anyhow!(err_msg));
        }
    };

    if loaded_wallets.is_empty() {
        let err_msg = "No wallets loaded for volume bot simulation.".to_string();
        warn!("{}", err_msg);
        let _ = status_sender.send(Err(err_msg.clone()));
        return Err(anyhow!(err_msg));
    }
    info!("Successfully loaded {} wallets for simulation.", loaded_wallets.len());
    if let Err(e) = wallets_data_sender.send(loaded_wallets.clone()) {
        error!("Failed to send loaded volume bot wallets to main app: {}", e);
    }

    let rpc_client = Arc::new(AsyncRpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed()));
    let http_client = ReqwestClient::new();
    let mut rng = rand::rngs::StdRng::from_entropy();

    let token_mint_pubkey = match Pubkey::from_str(&token_mint_str) {
        Ok(pk) => pk,
        Err(e) => {
            let err_msg = format!("Invalid token mint address {}: {}", token_mint_str, e);
            error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg.clone()));
            return Err(anyhow!(err_msg));
        }
    };

    let min_sol_for_tx = sol_to_lamports(0.0005); // Minimum SOL to consider for a transaction (e.g., covers fees + tiny buy)

    // --- Initial Three Buys ---
    info!("Starting initial 3 buy operations...");
    let _ = status_sender.send(Ok("Phase 1: Initial 3 Buys Starting.".to_string()));
    for i in 0..3 {
        let _ = status_sender.send(Ok(format!("Initial Buy {}/3", i + 1)));
        if loaded_wallets.is_empty() {
            warn!("No wallets available for initial buy {}", i + 1);
            let _ = status_sender.send(Err(format!("No wallets for initial buy {}", i+1)));
            continue;
        }
        let wallet_info = match loaded_wallets.choose(&mut rng) {
            Some(wi) => wi,
            None => { // Should not happen if loaded_wallets is not empty
                warn!("Failed to select a random wallet for initial buy {}", i + 1);
                let _ = status_sender.send(Err(format!("Wallet selection failed for initial buy {}", i+1)));
                continue;
            }
        };

        let keypair = match crate::commands::utils::load_keypair_from_string(&wallet_info.private_key, "VolumeBotWallet") {
            Ok(kp) => kp,
            Err(e) => {
                error!("Failed to load keypair for {}: {}. Skipping wallet for initial buy.", wallet_info.public_key, e);
                let _ = status_sender.send(Err(format!("Keypair load failed for {} (Initial Buy)", wallet_info.public_key)));
                continue;
            }
        };
        let wallet_pk_str = wallet_info.public_key.clone();
        let wallet_pubkey = keypair.pubkey();

        match rpc_client.get_balance(&wallet_pubkey).await {
            Ok(sol_balance_lamports) => {
                let target_initial_sol_spend_on_token_lamports = sol_to_lamports(initial_buy_sol_amount); // Use the destructured variable

                let buyer_token_ata_initial = get_associated_token_address(&wallet_pubkey, &token_mint_pubkey);
                let mut ata_rent_needed_lamports_initial = 0;
                match rpc_client.get_account(&buyer_token_ata_initial).await {
                    Ok(_) => {
                        info!("Initial Buy {}/3: Wallet {} Buyer ATA {} for {} already exists.", i + 1, wallet_pk_str, buyer_token_ata_initial, token_mint_str); // Corrected: Use token_mint_str
                    }
                    Err(_) => {
                        info!("Initial Buy {}/3: Wallet {} Buyer ATA {} for {} not found. Estimating rent.", i + 1, wallet_pk_str, buyer_token_ata_initial, token_mint_str); // Corrected: Use token_mint_str
                        ata_rent_needed_lamports_initial = ATA_RENT_LAMPORTS;
                    }
                }

                // Increased buffer to cover a potential additional ATA by Jupiter + base fees
                const TX_FEE_SAFETY_BUFFER_LAMPORTS: u64 = ATA_RENT_LAMPORTS + 5_000;
                let total_fixed_costs_initial = priority_fee_lamports + ata_rent_needed_lamports_initial + TX_FEE_SAFETY_BUFFER_LAMPORTS;

                if sol_balance_lamports <= total_fixed_costs_initial {
                    error!("Initial Buy {}/3: Wallet {} Insufficient SOL for fixed costs + safety buffer. Has {} lamports, needs > {} (Prio: {}, ATA: {}, Buffer: {}). Skipping.",
                        i + 1, wallet_pk_str, sol_balance_lamports, total_fixed_costs_initial, priority_fee_lamports, ata_rent_needed_lamports_initial, TX_FEE_SAFETY_BUFFER_LAMPORTS);
                    let _ = status_sender.send(Err(format!("Initial Buy {}/3 ({}): Low SOL for fees+safety buffer (Need ~{} SOL). Skip.", i + 1, wallet_pk_str, lamports_to_sol(total_fixed_costs_initial))));
                    continue;
                }

                let max_sol_available_for_actual_purchase_initial = sol_balance_lamports - total_fixed_costs_initial;
                let actual_sol_to_spend_for_quote_initial = std::cmp::min(target_initial_sol_spend_on_token_lamports, max_sol_available_for_actual_purchase_initial);

                if actual_sol_to_spend_for_quote_initial < sol_to_lamports(0.000001) { // Dust check
                    info!("Initial Buy {}/3: Wallet {} Actual SOL to spend for quote is too low ({} lamports) after fixed costs. Balance: {}. TargetSpend: {}. MaxAvailable: {}. Skipping.",
                        i + 1, wallet_pk_str, actual_sol_to_spend_for_quote_initial, sol_balance_lamports, target_initial_sol_spend_on_token_lamports, max_sol_available_for_actual_purchase_initial);
                    let _ = status_sender.send(Ok(format!("Initial Buy {}/3 ({}): Miniscule actual buy amount after fees. Skip.", i + 1, wallet_pk_str)));
                    continue;
                }

                let total_lamports_for_tx_attempt_initial = actual_sol_to_spend_for_quote_initial + total_fixed_costs_initial;
                if sol_balance_lamports < total_lamports_for_tx_attempt_initial {
                     error!("Initial Buy {}/3: Wallet {} Insufficient SOL (final check with safety buffer). Has {} lamports, needs {} (ActualSpend: {}, Prio: {}, ATA: {}, Buffer: {}). Skipping.",
                        i + 1, wallet_pk_str, sol_balance_lamports, total_lamports_for_tx_attempt_initial, actual_sol_to_spend_for_quote_initial, priority_fee_lamports, ata_rent_needed_lamports_initial, TX_FEE_SAFETY_BUFFER_LAMPORTS);
                    let _ = status_sender.send(Err(format!("Initial Buy {}/3 ({}): Low SOL (final check with safety buffer, Need: ~{} SOL). Skip.", i + 1, wallet_pk_str, lamports_to_sol(total_lamports_for_tx_attempt_initial))));
                    continue;
                }

                info!("Initial Buy {}/3: Wallet {} ({}), Attempting to spend ~{} SOL on token (Target: ~{} SOL). FixedCosts (Prio+ATA+SafetyBuffer): ~{} SOL. Balance: ~{} SOL.",
                    i + 1, wallet_pk_str, wallet_pubkey,
                    lamports_to_sol(actual_sol_to_spend_for_quote_initial),
                    lamports_to_sol(target_initial_sol_spend_on_token_lamports),
                    lamports_to_sol(total_fixed_costs_initial),
                    lamports_to_sol(sol_balance_lamports));
                let _ = status_sender.send(Ok(format!("Initial Buy {}/3 from {}: Spending ~{} SOL (actual) on token.", i + 1, wallet_pk_str, lamports_to_sol(actual_sol_to_spend_for_quote_initial))));

                match get_jupiter_quote_v6(&http_client, SOL_MINT_ADDRESS_STR, &token_mint_str, actual_sol_to_spend_for_quote_initial, slippage_bps).await {
                    Ok(quote_res) => {
                        match get_jupiter_swap_transaction(&http_client, &quote_res, &wallet_pk_str, priority_fee_lamports).await {
                            Ok(swap_res) => {
                                match BASE64_STANDARD.decode(swap_res.swap_transaction) {
                                    Ok(tx_data) => {
                                        match bincode::deserialize::<VersionedTransaction>(&tx_data[..]) {
                                            Ok(versioned_tx) => {
                                                match crate::utils::transaction::sign_and_send_versioned_transaction(rpc_client.as_ref(), versioned_tx, &[&keypair]).await {
                                                    Ok(signature) => {
                                                        info!("Initial Buy {}/3: Wallet {} BUY successful. Sig: {}", i + 1, wallet_pk_str, signature);
                                                        let _ = status_sender.send(Ok(format!("Initial Buy {}/3 ({}) OK. Sig: {}", i + 1, wallet_pk_str, signature)));
                                                    }
                                                    Err(e) => {
                                                        let error_msg_detail = format!(
                                                            "Initial Buy {}/3: Wallet {} BUY tx failed: {}",
                                                            i + 1,
                                                            wallet_pk_str,
                                                            e
                                                        );
                                                        error!("{}", error_msg_detail); // Log the full error locally
                                                        // Always send Ok to status_sender for this specific tx failure to allow continuation
                                                        let _ = status_sender.send(Ok(format!("Handled TX Error (initial buy attempt {}/3 for {}): {}", i + 1, wallet_pk_str, e)));
                                                    }
                                                }
                                            }
                                            Err(e) => { error!("Initial Buy {}/3: Wallet {} deserialize error: {}", i + 1, wallet_pk_str, e); let _ = status_sender.send(Err(format!("Initial Buy {}/3 ({}) Deserialize Err", i + 1, wallet_pk_str))); }
                                        }
                                    }
                                    Err(e) => { error!("Initial Buy {}/3: Wallet {} decode error: {}", i + 1, wallet_pk_str, e); let _ = status_sender.send(Err(format!("Initial Buy {}/3 ({}) Decode Err", i + 1, wallet_pk_str)));}
                                }
                            }
                            Err(e) => { error!("Initial Buy {}/3: Wallet {} swap API error: {}", i + 1, wallet_pk_str, e); let _ = status_sender.send(Err(format!("Initial Buy {}/3 ({}) Swap API Err", i + 1, wallet_pk_str)));}
                        }
                    }
                    Err(e) => { error!("Initial Buy {}/3: Wallet {} quote API error: {}", i + 1, wallet_pk_str, e); let _ = status_sender.send(Err(format!("Initial Buy {}/3 ({}) Quote API Err", i + 1, wallet_pk_str)));}
                }
            }
            Err(e) => {
                error!("Initial Buy {}/3: Failed to get SOL balance for {}: {}", i + 1, wallet_pk_str, e);
                let _ = status_sender.send(Err(format!("Initial Buy {}/3 ({}) Bal Fetch Err", i + 1, wallet_pk_str)));
            }
        }
        sleep(Duration::from_millis(rng.gen_range(1000..3000))).await; // Delay after each initial buy
    }
    info!("Initial 3 buy operations completed.");
    let _ = status_sender.send(Ok("Phase 1: Initial 3 Buys Completed.".to_string()));

    // --- Main Trading Loop ---
    info!("Starting main trading loop for {} cycles...", max_cycles);
    let _ = status_sender.send(Ok(format!("Phase 2: Main Trading Loop ({} cycles) Starting.", max_cycles)));

    for cycle in 0..max_cycles {
        let _ = status_sender.send(Ok(format!("Main Cycle {}/{}", cycle + 1, max_cycles)));
        if loaded_wallets.is_empty() {
            warn!("No wallets available for main trading cycle {}", cycle + 1);
             let _ = status_sender.send(Err(format!("No wallets for main cycle {}", cycle+1)));
            break; // No wallets to trade with
        }

        let action_rand: f64 = rng.gen(); // Random float between 0.0 and 1.0
        let action = if action_rand < 0.5 { "buy" } else { "sell" }; // 50/50 chance

        info!("Main Cycle {}/{}: Action: {}", cycle + 1, max_cycles, action.to_uppercase());
        let _ = status_sender.send(Ok(format!("Cycle {}/{}: Action decided: {}", cycle + 1, max_cycles, action.to_uppercase())));

        let wallet_info = match loaded_wallets.choose(&mut rng) {
             Some(wi) => wi,
             None => {
                warn!("Failed to select a random wallet for main cycle {}", cycle + 1);
                let _ = status_sender.send(Err(format!("Wallet selection failed for main cycle {}", cycle+1)));
                continue;
            }
        };
        let keypair = match crate::commands::utils::load_keypair_from_string(&wallet_info.private_key, "VolumeBotWallet") {
            Ok(kp) => kp,
            Err(e) => {
                error!("Main Cycle {}/{}: Failed to load keypair for {}: {}. Skipping.", cycle + 1, max_cycles, wallet_info.public_key, e);
                let _ = status_sender.send(Err(format!("Cycle {} Keypair Ld Fail: {}",cycle + 1, wallet_info.public_key)));
                continue;
            }
        };
        let wallet_pk_str = wallet_info.public_key.clone();
        let wallet_pubkey = keypair.pubkey();

        match action {
            "buy" => {
                match rpc_client.get_balance(&wallet_pubkey).await {
                    Ok(sol_balance_lamports) => {
                        if sol_balance_lamports < min_sol_for_tx {
                            info!("Main Cycle {}/{}: Wallet {} Insufficient SOL ({} lamports, < min_sol_for_tx {}). Skipping buy.", cycle + 1, max_cycles, wallet_pk_str, sol_balance_lamports, min_sol_for_tx);
                            let _ = status_sender.send(Ok(format!("Cycle {} ({}): Low SOL ({} SOL) for buy. Skip.",cycle + 1, wallet_pk_str, lamports_to_sol(sol_balance_lamports))));
                            continue;
                        }

                        // Buffer for base fees + up to TWO additional ATAs Jupiter might need.
                        const TX_FEE_SAFETY_BUFFER_LAMPORTS_ACTION: u64 = (2 * ATA_RENT_LAMPORTS) + 5_000;
                        let buyer_token_ata_action = get_associated_token_address(&wallet_pubkey, &token_mint_pubkey);
                        let mut ata_rent_needed_lamports_action = 0;
                        match rpc_client.get_account(&buyer_token_ata_action).await {
                            Ok(_) => {
                                info!("Main Cycle {}/{}: Wallet {} Buyer ATA {} for {} already exists.", cycle + 1, max_cycles, wallet_pk_str, buyer_token_ata_action, token_mint_str);
                            }
                            Err(_) => {
                                info!("Main Cycle {}/{}: Wallet {} Buyer ATA {} for {} not found. Estimating rent.", cycle + 1, max_cycles, wallet_pk_str, buyer_token_ata_action, token_mint_str);
                                ata_rent_needed_lamports_action = ATA_RENT_LAMPORTS;
                            }
                        }

                        let total_fixed_costs_action = priority_fee_lamports + ata_rent_needed_lamports_action + TX_FEE_SAFETY_BUFFER_LAMPORTS_ACTION;

                        if sol_balance_lamports <= total_fixed_costs_action {
                            error!("Main Cycle {}/{}: Wallet {} Insufficient SOL for fixed costs + buffer. Has {} lamports, needs > {} (Prio: {}, ATA: {}, Buffer: {}). Skipping buy.",
                                cycle + 1, max_cycles, wallet_pk_str, sol_balance_lamports, total_fixed_costs_action, priority_fee_lamports, ata_rent_needed_lamports_action, TX_FEE_SAFETY_BUFFER_LAMPORTS_ACTION);
                            let _ = status_sender.send(Err(format!("Cycle {} ({}): Low SOL for fees+buffer (Need ~{} SOL). Skip.",cycle + 1, wallet_pk_str, lamports_to_sol(total_fixed_costs_action))));
                            continue;
                        }

                        let max_sol_available_for_token_purchase_action = sol_balance_lamports - total_fixed_costs_action;

                        let spend_percentage = 0.50; // Fixed 50% as per user request
                        let target_sol_based_on_50_percent_balance = (sol_balance_lamports as f64 * spend_percentage) as u64;

                        let actual_sol_to_spend_for_quote_action = std::cmp::min(target_sol_based_on_50_percent_balance, max_sol_available_for_token_purchase_action);

                        if actual_sol_to_spend_for_quote_action < sol_to_lamports(0.000001) {
                             info!("Main Cycle {}/{}: Wallet {} Calculated actual SOL to spend is too low ({} lamports) after fixed costs & 50% target. Balance: {}. MaxAvailableForPurchase: {}. Skipping.",
                                cycle + 1, max_cycles, wallet_pk_str, actual_sol_to_spend_for_quote_action, sol_balance_lamports, max_sol_available_for_token_purchase_action);
                             let _ = status_sender.send(Ok(format!("Cycle {} ({}): Miniscule actual buy amount (50% target) after fees. Skip.",cycle + 1, wallet_pk_str)));
                             continue;
                        }

                        let final_required_for_action_buy = actual_sol_to_spend_for_quote_action + total_fixed_costs_action;
                        if sol_balance_lamports < final_required_for_action_buy {
                             error!("Main Cycle {}/{}: Wallet {} Insufficient SOL (final check). Has {} lamports, needs {} (ActualSpend: {}, Prio: {}, ATA: {}, Buffer: {}). Skipping buy.",
                                cycle + 1, max_cycles, wallet_pk_str, sol_balance_lamports, final_required_for_action_buy, actual_sol_to_spend_for_quote_action, priority_fee_lamports, ata_rent_needed_lamports_action, TX_FEE_SAFETY_BUFFER_LAMPORTS_ACTION);
                            let _ = status_sender.send(Err(format!("Cycle {} ({}): Low SOL (final check, Need: ~{} SOL). Skip.",cycle + 1, wallet_pk_str, lamports_to_sol(final_required_for_action_buy))));
                            continue;
                        }

                        info!("Main Cycle {}/{}: Wallet {} BUY with ~{} SOL (actual, {}% target of balance, capped by available after costs). FixedCosts (Prio+ATA+Buffer): ~{} SOL. Balance: ~{} SOL.",
                            cycle + 1, max_cycles, wallet_pk_str,
                            lamports_to_sol(actual_sol_to_spend_for_quote_action),
                            spend_percentage * 100.0,
                            lamports_to_sol(total_fixed_costs_action),
                            lamports_to_sol(sol_balance_lamports)
                        );
                        let _ = status_sender.send(Ok(format!("Cycle {} ({}): BUY with ~{} SOL (actual, {}% target)",cycle + 1, wallet_pk_str, lamports_to_sol(actual_sol_to_spend_for_quote_action), spend_percentage * 100.0)));

                        match get_jupiter_quote_v6(&http_client, SOL_MINT_ADDRESS_STR, &token_mint_str, actual_sol_to_spend_for_quote_action, slippage_bps).await {
                            Ok(quote_res) => {
                                match get_jupiter_swap_transaction(&http_client, &quote_res, &wallet_pk_str, priority_fee_lamports).await {
                                    Ok(swap_res) => {
                                        match BASE64_STANDARD.decode(swap_res.swap_transaction) {
                                            Ok(tx_data) => {
                                                match bincode::deserialize::<VersionedTransaction>(&tx_data[..]) {
                                                    Ok(versioned_tx) => {
                                                        match crate::utils::transaction::sign_and_send_versioned_transaction(rpc_client.as_ref(), versioned_tx, &[&keypair]).await {
                                                            Ok(signature) => { info!("Cycle {} ({}): BUY OK. Sig: {}",cycle + 1, wallet_pk_str, signature); let _ = status_sender.send(Ok(format!("Cycle {} ({}): BUY OK. Sig: {}",cycle + 1, wallet_pk_str, signature))); }
                                                            Err(e) => { error!("Cycle {} ({}): BUY Tx Send/Confirm FAIL: {}",cycle + 1, wallet_pk_str, e); let _ = status_sender.send(Ok(format!("Handled Cycle {} ({}): BUY Tx Send/Confirm FAIL: {}",cycle + 1, wallet_pk_str, e))); }
                                                        }
                                                    } Err(e) => { error!("Cycle {} ({}): BUY Deserialize Err: {}",cycle + 1, wallet_pk_str, e); let _ = status_sender.send(Ok(format!("Handled Cycle {} ({}): BUY Deserialize Err: {}",cycle + 1, wallet_pk_str, e))); }
                                                }
                                            } Err(e) => { error!("Cycle {} ({}): BUY Decode Err: {}",cycle + 1, wallet_pk_str, e); let _ = status_sender.send(Ok(format!("Handled Cycle {} ({}): BUY Decode Err: {}",cycle + 1, wallet_pk_str, e))); }
                                        }
                                    } Err(e) => { error!("Cycle {} ({}): BUY Swap API Err: {}",cycle + 1, wallet_pk_str, e); let _ = status_sender.send(Ok(format!("Handled Cycle {} ({}): BUY Swap API Err: {}",cycle + 1, wallet_pk_str, e))); }
                                }
                            } Err(e) => { error!("Cycle {} ({}): BUY Quote API Err: {}",cycle + 1, wallet_pk_str, e); let _ = status_sender.send(Ok(format!("Handled Cycle {} ({}): BUY Quote API Err: {}",cycle + 1, wallet_pk_str, e))); }
                        }
                    // This closing brace was missing in the previous REPLACE block, causing syntax errors.
                    // It correctly closes the `Ok(sol_balance_lamports) => {` block.
                    }
                    Err(e) => { error!("Cycle {} ({}): BUY Bal Fetch Err: {}",cycle + 1, wallet_pk_str, e); let _ = status_sender.send(Ok(format!("Handled Cycle {} ({}): BUY Bal Fetch Err: {}",cycle + 1, wallet_pk_str, e))); }
                }
            }
            "sell" => {
                // Find wallets holding the token
                let mut token_holding_wallets = Vec::new();
                for w_info in &loaded_wallets {
                    let current_wallet_keypair = match crate::commands::utils::load_keypair_from_string(&w_info.private_key, "VolumeBotWalletTemp") {
                        Ok(kp) => kp,
                        Err(_) => continue, // Skip if keypair load fails
                    };
                    let current_wallet_pubkey = current_wallet_keypair.pubkey();
                    let token_ata = get_associated_token_address(&current_wallet_pubkey, &token_mint_pubkey);
                    match rpc_client.get_token_account_balance(&token_ata).await {
                        Ok(balance_response) => {
                            if let Ok(token_balance_u64) = balance_response.amount.parse::<u64>() {
                                if token_balance_u64 > 0 {
                                    token_holding_wallets.push((w_info.clone(), token_balance_u64, current_wallet_keypair));
                                }
                            }
                        }
                        Err(_) => { /* ATA might not exist, or other error, effectively 0 balance */ }
                    }
                }

                if token_holding_wallets.is_empty() {
                    info!("Main Cycle {}/{}: No wallets found holding the token {}. Skipping SELL.", cycle + 1, max_cycles, token_mint_str);
                    let _ = status_sender.send(Ok(format!("Cycle {}/{}: No tokens to SELL. Skipping.", cycle + 1, max_cycles)));
                } else {
                    // Randomly select one of the token-holding wallets
                    let (selected_wallet_info, tokens_to_sell_lamports, selected_keypair) = token_holding_wallets.choose(&mut rng).unwrap().clone(); // unwrap is safe due to is_empty check
                    let selected_wallet_pk_str = selected_wallet_info.public_key.clone();

                    info!("Main Cycle {}/{}: Wallet {} SELL {} tokens (100%)", cycle + 1, max_cycles, selected_wallet_pk_str, tokens_to_sell_lamports);
                    let _ = status_sender.send(Ok(format!("Cycle {} ({}): SELL {} tokens",cycle + 1, selected_wallet_pk_str, tokens_to_sell_lamports)));

                    match get_jupiter_quote_v6(&http_client, &token_mint_str, SOL_MINT_ADDRESS_STR, *tokens_to_sell_lamports, slippage_bps).await {
                        Ok(quote_res) => {
                            match get_jupiter_swap_transaction(&http_client, &quote_res, &selected_wallet_pk_str, priority_fee_lamports).await {
                                Ok(swap_res) => {
                                    match BASE64_STANDARD.decode(swap_res.swap_transaction) {
                                        Ok(tx_data) => {
                                            match bincode::deserialize::<VersionedTransaction>(&tx_data[..]) {
                                                Ok(versioned_tx) => { // No longer mutable
                                                    // Priority fee is now handled by Jupiter via the API request
                                                    match crate::utils::transaction::sign_and_send_versioned_transaction(rpc_client.as_ref(), versioned_tx, &[&selected_keypair]).await {
                                                        Ok(signature) => { info!("Cycle {} ({}): SELL OK. Sig: {}",cycle + 1, selected_wallet_pk_str, signature); let _ = status_sender.send(Ok(format!("Cycle {} ({}): SELL OK. Sig: {}",cycle + 1, selected_wallet_pk_str, signature))); }
                                                        Err(e) => { error!("Cycle {} ({}): SELL FAIL: {}",cycle + 1, selected_wallet_pk_str, e); let _ = status_sender.send(Err(format!("Cycle {} ({}): SELL FAIL: {}",cycle + 1, selected_wallet_pk_str, e))); }
                                                    }
                                                } Err(e) => { error!("Cycle {} ({}): SELL Deserialize Err: {}",cycle + 1, selected_wallet_pk_str, e); let _ = status_sender.send(Err(format!("Cycle {} ({}): SELL Deserialize Err",cycle + 1, selected_wallet_pk_str))); }
                                            }
                                        } Err(e) => { error!("Cycle {} ({}): SELL Decode Err: {}",cycle + 1, selected_wallet_pk_str, e); let _ = status_sender.send(Err(format!("Cycle {} ({}): SELL Decode Err",cycle + 1, selected_wallet_pk_str))); }
                                    }
                                } Err(e) => { error!("Cycle {} ({}): SELL Swap API Err: {}",cycle + 1, selected_wallet_pk_str, e); let _ = status_sender.send(Err(format!("Cycle {} ({}): SELL Swap API Err",cycle + 1, selected_wallet_pk_str))); }
                            }
                        } Err(e) => { error!("Cycle {} ({}): SELL Quote API Err: {}",cycle + 1, selected_wallet_pk_str, e); let _ = status_sender.send(Err(format!("Cycle {} ({}): SELL Quote API Err",cycle + 1, selected_wallet_pk_str))); }
                    }

                }
            }
            _ => {} // Should not happen
        }
        sleep(Duration::from_millis(rng.gen_range(1000..5000))).await; // Delay after each main cycle action
    }

    info!("Volume Bot Simulation Task (New Strategy) Completed.");
    let _ = status_sender.send(Ok("Volume Bot Task (New Strategy) Completed.".to_string()));
    Ok(())
}

// --- Aurora Theme Color Constants ---
pub const AURORA_BG_DEEP_SPACE: Color32 = Color32::from_rgb(8, 10, 18);  // Slightly darker for better contrast
pub const AURORA_BG_PANEL: Color32 = Color32::from_rgb(18, 28, 50);      // Slightly adjusted for better contrast
pub const AURORA_ACCENT_NEON_BLUE: Color32 = Color32::from_rgb(0, 191, 255);  // Cyan/Electric blue
pub const AURORA_ACCENT_PURPLE: Color32 = Color32::from_rgb(138, 43, 226);    // Vibrant purple
pub const AURORA_ACCENT_TEAL: Color32 = Color32::from_rgb(0, 206, 209);       // Bright teal
pub const AURORA_ACCENT_MAGENTA: Color32 = Color32::from_rgb(255, 0, 255);    // Vibrant magenta
pub const AURORA_ACCENT_GREEN: Color32 = Color32::from_rgb(50, 220, 100);     // Added vibrant green for success states
pub const AURORA_ACCENT_ORANGE: Color32 = Color32::from_rgb(255, 140, 0);     // Added orange for warnings/alerts
pub const AURORA_TEXT_PRIMARY: Color32 = Color32::from_rgb(230, 235, 250);    // Slightly brighter for better readability
pub const AURORA_TEXT_MUTED_TEAL: Color32 = Color32::from_rgb(110, 150, 160); // Slightly brighter for better readability
pub const AURORA_TEXT_GLOW: Color32 = Color32::from_rgb(0, 150, 200);         // Brighter glow text
pub const AURORA_STROKE_DARK_BLUE_TINT: Color32 = Color32::from_rgba_premultiplied(25, 35, 60, 150); // Slightly adjusted
pub const AURORA_STROKE_GLOW_EDGE: Color32 = Color32::from_rgb(0, 90, 120);   // Brighter glow edge

// --- Constants from app.rs ---
const JUPITER_QUOTE_API_URL_V6: &str = "https://quote-api.jup.ag/v6/quote";
const JUPITER_SWAP_API_URL_V6: &str = "https://quote-api.jup.ag/v6/swap";
const SOL_MINT_ADDRESS_STR: &str = "So11111111111111111111111111111111111111112";
const ATA_RENT_LAMPORTS: u64 = 2_039_280;
const FEE_RECIPIENT_ADDRESS_FOR_SELL_STR: &str = "5UTHcSCMRdyphViB3obHTVWhQnViuFHRTc1hAy8pS173";
const MAX_ADDRESSES_PER_EXTEND: usize = 20;
const ZOMBIE_CHUNK_COMPUTE_LIMIT: u32 = 1_400_000;
const MAX_ZOMBIES_PER_CHUNK: usize = 6;

// --- Structs and Enums (previously copied) ---
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct JupiterQuoteResponse {
    out_amount: String,
    #[serde(flatten)]
    other_fields: JsonValue,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct JupiterSwapResponse {
    swap_transaction: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JupiterSwapRequestPayload<'a> {
    quote_response: &'a JupiterQuoteResponse,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prioritization_fee_lamports: Option<u64>,
}

#[derive(serde::Serialize, Debug)]
struct JitoSendTransactionParamsConfig {
    encoding: String,
    #[serde(rename = "skipPreflight")]
    skip_preflight: bool,
}

#[derive(serde::Serialize, Debug)]
struct JitoSendTransactionRequest<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: (String, JitoSendTransactionParamsConfig),
}

#[derive(serde::Deserialize, Debug)]
struct JitoSendTransactionResponse {
    jsonrpc: String,
    result: Option<String>,
    error: Option<JsonValue>,
    id: u64,
}

#[derive(Clone, Debug)]
struct WalletInfo {
    address: String,
    name: String, // New field for custom wallet name
    is_primary: bool, // Will be set to false, name field is preferred
    is_dev_wallet: bool,
    is_parent: bool,
    sol_balance: Option<f64>,
    wsol_balance: Option<f64>, // Added WSOL balance
    target_mint_balance: Option<String>,
    error: Option<String>,
    is_loading: bool,
    is_selected: bool,
    sell_percentage_input: String,
    sell_amount_input: String,     // Used for the "Sell" view (individual wallet sell)
    is_selling: bool,              // Used for the "Sell" view (individual wallet sell)
    atomic_buy_sol_amount_input: String, // For the "Atomic Buy" view (buy SOL amount)
    // New fields for Atomic Sell within the Atomic Buy/Multi-Wallet view
    atomic_sell_token_amount_input: String, // Can be a number or "100%"
    atomic_is_selling: bool, // Specific to atomic sell action from this view
    atomic_sell_status_message: Option<String>, // Status for this specific wallet's atomic sell
}

#[derive(Debug)]
enum FetchResult {
    Success {
        address: String,
        sol_balance: f64,
        wsol_balance: Option<f64>, // Added WSOL balance
        target_mint_balance: Option<String>,
    },
    Failure {
        address: String,
        error: String,
    },
}

impl FetchResult {
    fn address(&self) -> &str {
        match self {
            FetchResult::Success { address, .. } => address,
            FetchResult::Failure { address, .. } => address,
        }
    }
}

#[derive(Debug, Clone)]
struct WalletBalanceInfo {
    id: usize,
    pubkey: String,
    sol_balance: u64,
    token_balance: u64,
    sol_usd: f64,
    token_usd: f64,
    total_usd: f64,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum LaunchPlatform {
    PumpFun,
    BonkRaydium, // Represents Raydium Launchpad
}

impl LaunchPlatform {
    fn name(&self) -> &'static str {
        match self {
            LaunchPlatform::PumpFun => "Pump.fun",
            LaunchPlatform::BonkRaydium => "Bonk (Raydium)",
        }
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum AppView {
    Sell,
    Disperse,
    Gather,
    Launch,
    AtomicBuy,
    Check,
    Alt,
    Settings, // Restored
    VolumeBot,
    // Dashboard, // This was a ModernApp specific view, can be re-added if needed
}

#[derive(Debug)]
enum PrecalcResult {
    Log(String), // For intermediate log messages
    Success(String, Vec<Pubkey>),
    Failure(String),
}

#[derive(Debug, Clone)]
enum SellStatus {
    Idle,
    InProgress(String),
    WalletSuccess(String, String),
    WalletFailure(String, String),
    MassSellComplete(String),
    GatherAndSellComplete(String),
    Failure(String),
}

// LaunchParams moved to src/models/mod.rs

#[derive(Clone)]
struct VolumeBotStartParams {
    rpc_url: String,
    wallets_file_path: String,
    token_mint_str: String,
    max_cycles: u32,
    slippage_bps: u16,
    solana_priority_fee: u64,
    initial_buy_sol_amount: f64,
    status_sender: UnboundedSender<Result<String, String>>,
    wallets_data_sender: UnboundedSender<Vec<LoadedWalletInfo>>,
    use_jito: bool,
    jito_be_url: String,
    jito_tip_account: String,
    jito_tip_lamports: u64,
}

#[derive(Clone)]
struct FetchBalancesTaskParams {
    rpc_url: String,
    addresses_to_check: Vec<String>,
    target_mint: String,
    balance_fetch_sender: UnboundedSender<FetchResult>,
}

#[derive(Clone)]
struct LoadVolumeWalletsFromFileParams {
    file_path: String,
    wallets_sender: UnboundedSender<Vec<LoadedWalletInfo>>,
    status_sender: UnboundedSender<Result<String, String>>,
}

#[derive(Clone)]
struct DistributeTotalSolTaskParams {
    rpc_url: String,
    source_funding_keypair_str: String,
    wallets_to_fund: Vec<LoadedWalletInfo>,
    total_sol_to_distribute: f64,
    status_sender: UnboundedSender<Result<String, String>>,
}

#[derive(Clone)]
struct FundVolumeWalletsTaskParams {
    rpc_url: String,
    parent_keypair_str: String,
    wallets_to_fund: Vec<LoadedWalletInfo>,
    funding_amount_lamports: u64,
    status_sender: UnboundedSender<Result<String, String>>,
}

#[derive(Clone)]
struct GatherAllFundsTaskParams {
    rpc_url: String,
    parent_private_key_str: String,
    volume_wallets: Vec<LoadedWalletInfo>,
    token_mint_ca_str: String,
    status_sender: UnboundedSender<Result<String, String>>,
}

#[derive(serde::Deserialize, Debug)]
struct WalletData {
    name: String,
    key: String,
    address: String,
}

struct Particle {
    pos: Pos2,
    vel: Vec2,
    radius: f32,
    color: Color32,
}

impl Particle {
    fn new(rect: Rect) -> Self {
        let mut rng = rand::thread_rng();
        use rand::Rng;
        Self {
            pos: Pos2::new(rng.gen_range(rect.left()..=rect.right()), rng.gen_range(rect.top()..=rect.bottom())),
            vel: Vec2::new(rng.gen_range(-0.3..=0.3), rng.gen_range(-0.3..=0.3)),
            radius: rng.gen_range(0.5..=1.5),
            color: Color32::WHITE.linear_multiply(rng.gen_range(0.2..=0.5)), // Changed to white with variable alpha
        }
    }

    fn update(&mut self, rect: Rect) {
        self.pos += self.vel;
        if self.pos.x < rect.left() || self.pos.x > rect.right() { self.vel.x *= -1.0; }
        if self.pos.y < rect.top() || self.pos.y > rect.bottom() { self.vel.y *= -1.0; }
        self.pos.x = self.pos.x.clamp(rect.left(), rect.right());
        self.pos.y = self.pos.y.clamp(rect.top(), rect.bottom());
    }
}

pub struct ModernApp {
    time: f64,
    particles: Vec<Particle>,
    background_image: Option<RetainedImage>,
    // style: egui::Style, // Removed: No longer storing style in ModernApp for this test
    app_settings: AppSettings,
    current_view: AppView,
    create_token_name: String,
    create_token_symbol: String,
    create_token_description: String,
    create_token_image_path: String,
    create_dev_buy_amount_tokens: f64,
    create_token_decimals: u8,
    create_jito_bundle: bool,
    create_zombie_amount: u64,
    sell_mint: String,
    sell_slippage_percent: f64,
    sell_priority_fee_lamports: u64,
    pump_mint: String,
    pump_buy_amount_sol: f64,
    pump_sell_threshold_sol: f64,
    pump_slippage_percent: f64,
    pump_priority_fee_lamports: u64,
    pump_jito_tip_sol: f64,
    pump_lookup_table_address: String,
    pump_private_key_string: String,
    pump_use_jito_bundle: bool,
    pump_is_running: Arc<AtomicBool>,
    pump_task_handle: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    wallets: Vec<WalletInfo>,
    loaded_wallet_data: Vec<LoadedWalletInfo>,
    wallet_load_error: Option<String>,
    balances_loading: bool,
    fetch_balances_in_progress: bool, // Added for tracking balance fetch status
    disperse_in_progress: bool,
    gather_in_progress: bool,
    gather_tasks_expected: usize,
    gather_tasks_completed: usize,
    balance_tasks_expected: usize,
    balance_tasks_completed: usize,
    parent_sol_balance_display: Option<f64>,
    total_sol_balance_display: Option<f64>,
    balance_fetch_sender: UnboundedSender<FetchResult>,
    balance_fetch_receiver: UnboundedReceiver<FetchResult>,
    disperse_result_sender: UnboundedSender<Result<String, String>>,
    disperse_result_receiver: UnboundedReceiver<Result<String, String>>,
    gather_result_sender: UnboundedSender<Result<String, String>>,
    gather_result_receiver: UnboundedReceiver<Result<String, String>>,
    alt_precalc_result_sender: UnboundedSender<PrecalcResult>,
    alt_precalc_result_receiver: UnboundedReceiver<PrecalcResult>,
    alt_creation_status_sender: UnboundedSender<AltCreationStatus>,
    alt_creation_status_receiver: UnboundedReceiver<AltCreationStatus>,
    alt_deactivate_status_sender: UnboundedSender<Result<String, String>>,
    alt_deactivate_status_receiver: UnboundedReceiver<Result<String, String>>,
    sell_status_sender: UnboundedSender<SellStatus>,
    sell_status_receiver: UnboundedReceiver<SellStatus>,
    sell_log_messages: Vec<String>,
    sell_mass_reverse_in_progress: bool,
    sell_gather_and_sell_in_progress: bool,
    pump_status_sender: UnboundedSender<PumpStatus>,
    pump_status_receiver: UnboundedReceiver<PumpStatus>,
    pump_log_messages: Vec<String>,
    pump_in_progress: bool,
    launch_status_sender: UnboundedSender<LaunchStatus>,
    launch_status_receiver: UnboundedReceiver<LaunchStatus>,
    launch_main_tx_priority_fee_micro_lamports: u64, // Added
    launch_jito_actual_tip_sol: f64,                 // Added
    launch_log_messages: Vec<String>,
    launch_token_name: String,
    launch_token_symbol: String,
    launch_token_description: String,
    launch_token_image_url: String,
    launch_dev_buy_sol: f64,
    launch_zombie_buy_sol: f64,
    launch_alt_address: String,
    launch_mint_keypair_path: String,
    launch_in_progress: bool,
    launch_use_jito_bundle: bool,
    launch_twitter: String,
    launch_telegram: String,
    launch_website: String,
    launch_simulate_only: bool,
    launch_platform: LaunchPlatform, // Added for Bonk/Pump.fun selection
    last_operation_result: Option<Result<String, String>>,
    check_view_target_mint: String,
    volume_bot_source_wallets_file: String,
    disperse_sol_amount: f64,
    alt_view_status: String,
    last_generated_volume_wallets_file: Option<String>,
    alt_log_messages: Vec<String>,
    alt_generated_mint_pubkey: Option<String>,
    alt_precalc_addresses: Vec<Pubkey>,
    alt_address: Option<Pubkey>,
    alt_creation_in_progress: bool,
    alt_precalc_in_progress: bool,
    alt_deactivation_in_progress: bool,
    alt_deactivation_status_message: String,
    available_alt_addresses: Vec<String>,
    available_mint_keypairs: Vec<String>,
    transfer_recipient_address: String,
    transfer_sol_amount: f64,
    transfer_selected_sender: Option<String>,
    transfer_in_progress: bool,
    transfer_status_sender: UnboundedSender<Result<String, String>>,
    transfer_status_receiver: UnboundedReceiver<Result<String, String>>,
    transfer_log_messages: Vec<String>,
    sim_token_mint: String,
    sim_max_cycles: usize,
    sim_slippage_bps: u16,
    volume_bot_initial_buy_sol_input: String,
    sim_in_progress: bool,
    sim_log_messages: Vec<String>,
    sim_status_sender: UnboundedSender<Result<String, String>>,
    sim_status_receiver: UnboundedReceiver<Result<String, String>>,
    sim_task_handle: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    start_volume_bot_request: Option<VolumeBotStartParams>,
    start_fetch_balances_request: Option<FetchBalancesTaskParams>,
    start_load_volume_wallets_request: Option<LoadVolumeWalletsFromFileParams>,
    start_distribute_total_sol_request: Option<DistributeTotalSolTaskParams>,
    start_fund_volume_wallets_request: Option<FundVolumeWalletsTaskParams>,
    start_gather_all_funds_request: Option<GatherAllFundsTaskParams>,
    volume_bot_num_wallets_to_generate: usize,
    volume_bot_wallets_generated: bool,
    volume_bot_generation_in_progress: bool,
    volume_bot_wallets: Vec<LoadedWalletInfo>,
    volume_bot_wallet_gen_status_sender: UnboundedSender<Result<String, String>>,
    volume_bot_wallet_gen_status_receiver: UnboundedReceiver<Result<String, String>>,
    volume_bot_wallets_sender: UnboundedSender<Vec<LoadedWalletInfo>>,
    volume_bot_wallets_receiver: UnboundedReceiver<Vec<LoadedWalletInfo>>,
    volume_bot_funding_per_wallet_sol: f64,
    volume_bot_funding_in_progress: bool,
    volume_bot_funding_status_sender: UnboundedSender<Result<String, String>>,
    volume_bot_funding_status_receiver: UnboundedReceiver<Result<String, String>>,
    volume_bot_total_sol_to_distribute_input: String,
    volume_bot_funding_source_private_key_input: String,
    volume_bot_funding_log_messages: Vec<String>,
    volume_bot_wallet_display_infos: Vec<WalletInfo>,
    awaiting_final_gather_status: bool, // New flag
    settings_feedback: Option<String>,
    settings_generate_count: usize,
    sim_token_mint_orig: String,
    sim_num_wallets_orig: usize,
    sim_wallets_file_orig: String,
    sim_initial_sol_orig: f64,
    sim_max_steps_orig: usize,
    sim_slippage_bps_orig: u16,
    sim_in_progress_orig: bool,
    sim_log_messages_orig: Vec<String>,
    sim_task_handle_orig: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    sim_status_sender_orig: UnboundedSender<Result<String, String>>,
    sim_status_receiver_orig: UnboundedReceiver<Result<String, String>>,
    sim_consolidate_sol_in_progress_orig: bool,
    sim_consolidate_sol_status_sender_orig: UnboundedSender<Result<String, String>>,
    sim_consolidate_sol_status_receiver_orig: UnboundedReceiver<Result<String, String>>,
launch_completion_link: Option<String>, // To store the pump.fun link
    sim_wallet_gen_in_progress: bool,
    sim_wallet_gen_status_sender: UnboundedSender<Result<String, String>>,
    sim_wallet_gen_status_receiver: UnboundedReceiver<Result<String, String>>,
    monitor_active: Arc<AtomicBool>,
    monitor_task_handle: Option<tokio::task::JoinHandle<()>>,
    monitor_balances: Vec<WalletBalanceInfo>,
    monitor_prices: Option<(f64, f64)>,
    monitor_update_sender: UnboundedSender<(Vec<WalletBalanceInfo>, Option<(f64, f64)>)>,
    monitor_update_receiver: UnboundedReceiver<(Vec<WalletBalanceInfo>, Option<(f64, f64)>)>,

    // PnL Calculation State
    pnl_summary: Option<PnlSummary>,
    pnl_calculation_in_progress: bool,
    pnl_error_message: Option<String>,
    pnl_calc_sender: UnboundedSender<Result<PnlSummary, String>>,
    pnl_calc_receiver: UnboundedReceiver<Result<PnlSummary, String>>,

    // --- Atomic Buy View State (from app.rs) ---
    atomic_buy_mint_address: String,
    atomic_buy_slippage_bps: u64,
    atomic_buy_priority_fee_lamports_per_tx: u64,
    atomic_buy_alt_address: String, // For UI parity, though command might not use it
    atomic_buy_exchange: String, // "pumpfun" or "raydium"
    atomic_buy_in_progress: bool,
    atomic_buy_status_sender: UnboundedSender<Result<String, String>>,
    atomic_buy_status_receiver: UnboundedReceiver<Result<String, String>>,
    atomic_buy_log_messages: Vec<String>,
    atomic_buy_jito_tip_sol: f64, // This was likely for the overall bundle tip
    atomic_buy_jito_tip_per_tx_str: String, // For UI input of per-tx tip
    atomic_buy_jito_tip_overall_str: String, // For UI input of overall bundle tip (if different or also configurable)
    atomic_buy_task_handle: Option<tokio::task::JoinHandle<()>>,

    // --- Atomic Sell View State (related to Atomic Buy view) ---
    atomic_sell_in_progress: bool,
    atomic_sell_task_handle: Option<tokio::task::JoinHandle<()>>,
    atomic_sell_status_sender: UnboundedSender<Result<(String, String), (String, String)>>,
    atomic_sell_status_receiver: UnboundedReceiver<Result<(String, String), (String, String)>>,

    // --- Unwrap WSOL State ---
    unwrap_wsol_in_progress: bool,
    unwrap_wsol_log_messages: Vec<String>,
    unwrap_wsol_status_sender: UnboundedSender<Result<String, String>>,
    unwrap_wsol_status_receiver: UnboundedReceiver<Result<String, String>>,
    unwrap_wsol_task_handle: Option<tokio::task::JoinHandle<()>>, // Added

    // --- Mix Wallets State (for Launch View) ---
    mix_wallets_in_progress: bool,
    mix_wallets_log_messages: Vec<String>,
    mix_wallets_status_sender: UnboundedSender<Result<String, String>>,
    mix_wallets_status_receiver: UnboundedReceiver<Result<String, String>>,
    start_mix_wallets_request: Option<()>, // Using Option<()> as a simple trigger
    mix_wallets_task_handle: Option<tokio::task::JoinHandle<()>>,

    // --- Retry Zombie to Mixed Wallets Funding State ---
    last_mixed_wallets_file_path: Option<String>,
    retry_zombie_to_mixed_funding_in_progress: bool,
    retry_zombie_to_mixed_funding_log_messages: Vec<String>,
    retry_zombie_to_mixed_funding_status_sender: UnboundedSender<Result<String, String>>,
    retry_zombie_to_mixed_funding_status_receiver: UnboundedReceiver<Result<String, String>>,
    retry_zombie_to_mixed_funding_task_handle: Option<tokio::task::JoinHandle<()>>,

    // --- ALT Management State (from app.rs, already present in ModernApp due to prior merge) ---
    // alt_view_status: String, // Already present
    // alt_log_messages: Vec<String>, // Already present
    // alt_generated_mint_pubkey: Option<String>, // Already present
    // alt_precalc_addresses: Vec<Pubkey>, // Already present
    // alt_address: Option<Pubkey>, // Already present
    // alt_creation_in_progress: bool, // Already present
    // alt_precalc_in_progress: bool, // Already present
    // alt_deactivation_in_progress: bool, // Already present
    // alt_deactivation_status_message: String, // Already present
    // available_alt_addresses: Vec<String>, // Already present and initialized in new
    // available_mint_keypairs: Vec<String>, // Already present and initialized in new
    // alt_precalc_result_sender, alt_precalc_result_receiver, are already present
    // alt_creation_status_sender, alt_creation_status_receiver, are already present
    // alt_deactivate_status_sender, alt_deactivate_status_receiver, are already present
}

// --- Helper functions & tasks (copied from app.rs, may need adjustments) ---
// Enums PrecalcResult and AltCreationStatus are already defined in ModernApp from PumpFunApp merge

async fn get_jupiter_quote_v6(
    http_client: &ReqwestClient,
    input_mint_str: &str,
    output_mint_str: &str,
    amount_lamports: u64,
    slippage_bps: u16,
) -> Result<JupiterQuoteResponse, anyhow::Error> {
    let params = [
        ("inputMint", input_mint_str),
        ("outputMint", output_mint_str),
        ("amount", &amount_lamports.to_string()),
        ("slippageBps", &slippage_bps.to_string()),
        ("onlyDirectRoutes", "false"),
    ];
    let response = http_client
        .get(JUPITER_QUOTE_API_URL_V6)
        .query(&params)
        .send()
        .await
        .context("Failed to send Jupiter quote API request")?;
    let status_code = response.status();
    if !status_code.is_success() {
        let error_text = response.text().await.unwrap_or_else(|_| "Unknown API error".to_string());
        return Err(anyhow!("Jupiter quote API request failed with status {}: {}", status_code, error_text));
    }
    response.json::<JupiterQuoteResponse>().await.context("Failed to parse Jupiter quote API response")
}

async fn get_jupiter_swap_transaction(
    http_client: &ReqwestClient,
    quote_resp: &JupiterQuoteResponse,
    user_pk_str: &str,
    priority_fee_lamports: u64,
) -> Result<JupiterSwapResponse, anyhow::Error> {
    let payload = JupiterSwapRequestPayload {
        quote_response: quote_resp,
        user_public_key: user_pk_str.to_string(),
        wrap_and_unwrap_sol: true,
        prioritization_fee_lamports: if priority_fee_lamports > 0 { Some(priority_fee_lamports) } else { None },
    };
    let response = http_client
        .post(JUPITER_SWAP_API_URL_V6)
        .json(&payload)
        .send()
        .await
        .context("Failed to send Jupiter swap API request")?;
    let status_code = response.status();
    if !status_code.is_success() {
        let error_text = response.text().await.unwrap_or_else(|_| "Unknown API error while getting swap transaction".to_string());
        return Err(anyhow!("Jupiter swap API request failed with status {}: {}", status_code, error_text));
    }
    response.json::<JupiterSwapResponse>().await.context("Failed to parse Jupiter swap API response")
}

async fn send_transaction_via_jito_raw(
    http_client: &ReqwestClient,
    jito_block_engine_base_url: &str,
    base64_encoded_tx: String,
    status_sender: &UnboundedSender<Result<String, String>>,
) -> Result<String, anyhow::Error> {
    let jito_send_tx_url = format!("{}/api/v1/transactions", jito_block_engine_base_url.trim_end_matches('/'));
    let request_payload = JitoSendTransactionRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "sendTransaction",
        params: (
            base64_encoded_tx.clone(),
            JitoSendTransactionParamsConfig {
                encoding: "base64".to_string(),
                skip_preflight: true,
            },
        ),
    };
    info!("Sending transaction to Jito: URL: {}, Payload: {:?}", jito_send_tx_url, serde_json::to_string(&request_payload).unwrap_or_default());
    let _ = status_sender.send(Ok(format!("Sending to Jito: {}...", jito_send_tx_url)));
    let response = http_client.post(&jito_send_tx_url).json(&request_payload).send().await
        .with_context(|| format!("Failed to send Jito sendTransaction request to {}", jito_send_tx_url))?;
    let response_status = response.status();
    let response_text = response.text().await.unwrap_or_else(|_| "Unknown Jito API error text".to_string());
    info!("Jito sendTransaction response status: {}, body: {}", response_status, response_text);
    if !response_status.is_success() {
        let err_msg = format!("Jito sendTransaction API request failed with status {}: {}", response_status, response_text);
        error!("{}", err_msg);
        let _ = status_sender.send(Err(err_msg.clone()));
        return Err(anyhow!(err_msg));
    }
    match serde_json::from_str::<JitoSendTransactionResponse>(&response_text) {
        Ok(jito_response) => {
            if let Some(sig) = jito_response.result {
                info!("Jito sendTransaction successful, signature: {}", sig);
                let _ = status_sender.send(Ok(format!("Jito sendTransaction OK: {}", sig)));
                Ok(sig)
            } else if let Some(err_payload) = jito_response.error {
                let err_msg = format!("Jito sendTransaction returned an error: {:?}", err_payload);
                error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg.clone()));
                Err(anyhow!(err_msg))
            } else {
                let err_msg = "Jito sendTransaction response had no signature and no error.".to_string();
                error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg.clone()));
                Err(anyhow!(err_msg))
            }
        }
        Err(e) => {
            let err_msg = format!("Failed to parse Jito sendTransaction response: {}. Raw text: {}", e, response_text);
            error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg.clone()));
            Err(anyhow!(err_msg))
        }
    }
}

async fn generate_volume_bot_wallets_task(
    num_wallets: usize,
    status_sender: UnboundedSender<Result<String, String>>,
    wallets_sender: UnboundedSender<Vec<LoadedWalletInfo>>
) {
    log::info!("Task: Starting generation of {} volume bot wallets.", num_wallets);
    let _ = status_sender.send(Ok(format!("Generation task started for {} wallets...", num_wallets)));
    if num_wallets == 0 {
        log::warn!("Task: num_wallets is 0, nothing to generate.");
        let _ = status_sender.send(Ok("Completed: 0 wallets generated.".to_string()));
        let _ = wallets_sender.send(Vec::new());
        return;
    }
    let mut generated_wallets = Vec::new();
    let base_name = "vol_bot_wallet";
    for i in 0..num_wallets {
        let keypair = Keypair::new();
        let pk_bs58 = keypair.pubkey().to_string();
        let sk_bs58 = bs58::encode(keypair.to_bytes()).into_string();
        let wallet_info = LoadedWalletInfo {
            name: Some(format!("{}_{}", base_name, i + 1)),
            private_key: sk_bs58,
            public_key: pk_bs58,
        };
        generated_wallets.push(wallet_info);
        if (i + 1) % 10 == 0 || (i + 1) == num_wallets {
            log::info!("Generated {}/{} volume bot wallets.", i + 1, num_wallets);
        }
        if num_wallets > 20 && i % 5 == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    log::info!("Successfully generated {} volume bot wallets in memory.", generated_wallets.len());
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("volume_wallets_{}.json", timestamp);
    match File::create(&filename) {
        Ok(file) => {
            let writer = BufWriter::new(file);
            match serde_json::to_writer_pretty(writer, &generated_wallets) {
                Ok(_) => {
                    log::info!("Successfully saved generated wallets to {}", filename);
                    if wallets_sender.send(generated_wallets).is_err() {
                        log::error!("Failed to send generated wallets (after saving) from task to UI.");
                        let _ = status_sender.send(Err("Failed to send wallets to UI, but file saved.".to_string()));
                        return;
                    }
                    if status_sender.send(Ok(format!("Successfully generated {} wallets and saved to {}", num_wallets, filename))).is_err() {
                        log::error!("Failed to send success status (file saved) from wallet gen task to UI.");
                    }
                }
                Err(e) => {
                    let err_msg = format!("Failed to serialize and save wallets to {}: {}", filename, e);
                    log::error!("{}", err_msg);
                    if wallets_sender.send(generated_wallets).is_err() {
                         log::error!("Failed to send generated wallets (file save failed) from task to UI.");
                    }
                    let _ = status_sender.send(Err(err_msg));
                }
            }
        }
        Err(e) => {
            let err_msg = format!("Failed to create file {}: {}", filename, e);
            log::error!("{}", err_msg);
            if wallets_sender.send(generated_wallets).is_err() {
                log::error!("Failed to send generated wallets (file create failed) from task to UI.");
            }
            let _ = status_sender.send(Err(err_msg));
        }
    }
}


impl ModernApp {
    fn refresh_application_state(&mut self) {
        log::info!("Initiating HARD refresh of application state...");

        // 1. Abort Active Tasks & Reset Flags
        log::info!("Hard Refresh: Aborting active tasks and resetting progress flags.");
        if let Some(handle) = self.pump_task_handle.take() { handle.abort(); }
        self.pump_is_running.store(false, AtomicOrdering::Relaxed);
        self.pump_in_progress = false;

        if let Some(handle) = self.sim_task_handle.take() { handle.abort(); }
        self.sim_in_progress = false;

        if let Some(handle) = self.sim_task_handle_orig.take() { handle.abort(); }
        self.sim_in_progress_orig = false;
        self.sim_consolidate_sol_in_progress_orig = false;
        self.sim_wallet_gen_in_progress = false;

        if let Some(handle) = self.atomic_buy_task_handle.take() { handle.abort(); }
        self.atomic_buy_in_progress = false;

        if let Some(handle) = self.monitor_task_handle.take() { handle.abort(); }
        self.monitor_active.store(false, AtomicOrdering::Relaxed);
        self.monitor_balances.clear();
        self.monitor_prices = None;

        self.balances_loading = false;
        self.disperse_in_progress = false;
        self.gather_in_progress = false;
        self.alt_creation_in_progress = false;
        self.alt_precalc_in_progress = false;
        self.alt_deactivation_in_progress = false;
        self.launch_in_progress = false;
        self.transfer_in_progress = false;
        self.sell_mass_reverse_in_progress = false;
        self.sell_gather_and_sell_in_progress = false;
        for wallet in self.wallets.iter_mut() { wallet.is_selling = false; }
        self.volume_bot_generation_in_progress = false;
        self.volume_bot_funding_in_progress = false;
        // Reset task counters
        self.balance_tasks_expected = 0;
        self.balance_tasks_completed = 0;
        self.gather_tasks_expected = 0;
        self.gather_tasks_completed = 0;

        // 2. Clear Pending Task Requests
        log::info!("Hard Refresh: Clearing pending task requests.");
        self.start_volume_bot_request = None;
        self.start_fetch_balances_request = None;
        self.start_load_volume_wallets_request = None;
        self.start_distribute_total_sol_request = None;
        self.start_fund_volume_wallets_request = None;
        self.start_gather_all_funds_request = None;

        // 3. Clear Logs and Status Messages
        log::info!("Hard Refresh: Clearing logs and status messages.");
        self.sell_log_messages.clear();
        self.pump_log_messages.clear();
        self.launch_log_messages.clear();
        self.alt_log_messages.clear();
        self.transfer_log_messages.clear();
        self.sim_log_messages.clear();
        self.sim_log_messages_orig.clear();
        self.volume_bot_funding_log_messages.clear();
        self.atomic_buy_log_messages.clear();

        self.alt_view_status = "Ready.".to_string();
        self.alt_deactivation_status_message = "ALT Deactivation Idle".to_string();
        self.settings_feedback = None;
        self.last_generated_volume_wallets_file = None;
        self.volume_bot_wallet_display_infos.clear();

        // 4. Reload Wallets & Data
        log::info!("Hard Refresh: Reloading wallet data.");
        let (loaded_data, display_wallets, error) =
            match crate::wallet::load_keys_from_file(Path::new(&self.app_settings.keys_path)) {
                Ok(loaded_keys) => {
                    log::info!("Refreshed: Loaded {} wallets from {}", loaded_keys.wallets.len(), &self.app_settings.keys_path);
                    let mut loaded_data_mut = loaded_keys.wallets.clone();
                    let mut zombie_counter = 0;
                    let mut wallet_infos_for_ui: Vec<WalletInfo> = loaded_data_mut.iter().map(|loaded_info| {
                        let dev_wallet_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key);
                        let is_dev_wallet = dev_wallet_pubkey_str_opt.as_ref().map_or(false, |dpk| dpk == &loaded_info.public_key);

                        let parent_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                        let is_parent = parent_pubkey_str_opt.as_ref().map_or(false, |ppk| ppk == &loaded_info.public_key);

                        let name = if is_dev_wallet {
                            "Dev Wallet".to_string()
                        } else if is_parent {
                            "Parent Wallet".to_string()
                        } else {
                            zombie_counter += 1;
                            format!("Zombie {}", zombie_counter)
                        };

                        WalletInfo {
                            address: loaded_info.public_key.clone(),
                            name,
                            is_primary: false,
                            is_dev_wallet,
                            is_parent,
                            sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                            sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                            atomic_buy_sol_amount_input: "0.0".to_string(),
                            atomic_sell_token_amount_input: String::new(), // New field
                            atomic_is_selling: false, // New field
                            atomic_sell_status_message: None, // New field
                        }
                    }).collect();

                    if let Some(dev_wallet_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key) {
                        if !wallet_infos_for_ui.iter().any(|w| w.address == dev_wallet_pubkey_str) {
                            let is_parent_also = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key).map_or(false, |ppk| ppk == dev_wallet_pubkey_str);
                            wallet_infos_for_ui.push(WalletInfo {
                                address: dev_wallet_pubkey_str.clone(),
                                name: "Dev Wallet".to_string(),
                                is_primary: false,
                                is_dev_wallet: true,
                                is_parent: is_parent_also,
                                sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                                sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                                atomic_buy_sol_amount_input: "0.0".to_string(),
                                atomic_sell_token_amount_input: String::new(), // New field
                                atomic_is_selling: false, // New field
                                atomic_sell_status_message: None, // New field
                            });
                            if !loaded_data_mut.iter().any(|w| w.public_key == dev_wallet_pubkey_str) {
                                loaded_data_mut.push(LoadedWalletInfo { name: Some("Dev (from Settings)".to_string()), private_key: self.app_settings.dev_wallet_private_key.clone(), public_key: dev_wallet_pubkey_str });
                            }
                        }
                    }
                    if let Some(parent_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key) {
                         if !wallet_infos_for_ui.iter().any(|w| w.address == parent_pubkey_str) {
                            wallet_infos_for_ui.push(WalletInfo {
                                address: parent_pubkey_str.clone(),
                                name: "Parent Wallet".to_string(),
                                is_primary: false,
                                is_dev_wallet: false,
                                is_parent: true,
                                sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                                sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                                atomic_buy_sol_amount_input: "0.0".to_string(),
                                atomic_sell_token_amount_input: String::new(), // New field
                                atomic_is_selling: false, // New field
                                atomic_sell_status_message: None, // New field
                            });
                        }
                    }
                    (loaded_data_mut, wallet_infos_for_ui, None)
                }
                Err(e) => {
                    let err_msg = format!("Hard Refresh: Failed to load keys from {}: {}", self.app_settings.keys_path, e);
                    log::error!("{}", err_msg);
                    (Vec::new(), Vec::new(), Some(err_msg))
                }
            };

        self.loaded_wallet_data = loaded_data;
        self.wallets = display_wallets;
        self.wallet_load_error = error.clone();

        if self.wallet_load_error.is_none() {
            log::info!("Hard Refresh: Triggering balance fetch for all wallets.");
            self.trigger_balance_fetch(None);
        } else {
            log::warn!("Hard Refresh: Skipping balance fetch due to wallet load error: {:?}", self.wallet_load_error);
            self.balances_loading = false;
        }

        log::info!("Hard Refresh: Reloading available ALTs and Mint keypairs.");
        self.load_available_alts();
        self.load_available_mint_keypairs();

        if let Some(err) = &self.wallet_load_error {
            self.last_operation_result = Some(Err(format!("Hard refresh completed with errors (wallet load: {}).", err)));
        } else {
            self.last_operation_result = Some(Ok("Application hard refresh complete.".to_string()));
        }
        log::info!("Application hard refresh process completed.");
    }


    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Install image loaders (e.g., for SVG support)
        egui_extras::install_image_loaders(&cc.egui_ctx);

        // --- Font Setup ---
        let mut fonts = egui::FontDefinitions::default();

        // Load Komet Extralight
        fonts.font_data.insert(
            "komet_extralight".to_owned(),
            egui::FontData::from_static(include_bytes!("../../assets/komet-extralight.otf")),
        );
        // Load Komet Heavy
        fonts.font_data.insert(
            "komet_heavy".to_owned(),
            egui::FontData::from_static(include_bytes!("../../assets/komet-heavy.otf")),
        );

        // Set Komet Extralight as the default proportional font
        fonts.families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "komet_extralight".to_owned());

        // Set Komet Heavy for headings and titles
        fonts.families
            .entry(egui::FontFamily::Name("KometHeavy".into()))
            .or_default()
            .push("komet_heavy".to_owned());

        // Load DigitLoFi font data (double underscore)
        fonts.font_data.insert(
            "digit_lofi_gui".to_owned(), // Key for the font data
            egui::FontData::from_static(include_bytes!("../../assets/DIGILF__.TTF")), // Using DIGILF__.TTF
        );

        // Bind the "digit_lofi_gui" data to the FontFamily::Name("DigitLoFiGui")
        fonts.families
            .entry(egui::FontFamily::Name("DigitLoFiGui".into())) // Family name
            .or_default()
            .push("digit_lofi_gui".to_owned()); // The key of the font data to use

        cc.egui_ctx.set_fonts(fonts);

        // --- Aurora Theme Setup ---
        let mut visuals = Visuals::dark();
        visuals.dark_mode = true;
        visuals.override_text_color = Some(AURORA_TEXT_PRIMARY);

        // Non-interactive widgets (default state for many widgets, including buttons)
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, AURORA_TEXT_PRIMARY);
        visuals.widgets.noninteractive.bg_fill = AURORA_BG_PANEL; // Changed to dark blue
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.6)); // Matching brighter subtle border
        visuals.widgets.noninteractive.rounding = Rounding::same(8.0); // Increased rounding

        // Inactive widgets (e.g., disabled state, but also sometimes default for complex inputs)
        visuals.widgets.inactive.bg_fill = AURORA_BG_PANEL; // Changed to dark blue
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, AURORA_TEXT_PRIMARY); // Ensure text is primary
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.5)); // Border like other inputs
        visuals.widgets.inactive.rounding = Rounding::same(8.0); // Increased rounding

        // Hovered widgets (primarily for buttons)
        visuals.widgets.hovered.bg_fill = AURORA_ACCENT_PURPLE.linear_multiply(0.25); // Dark & Subtle Hover Fill (very dark purple)
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, AURORA_TEXT_PRIMARY);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.5, AURORA_ACCENT_PURPLE.linear_multiply(0.4)); // Dark & Subtle Hover Stroke
        visuals.widgets.hovered.rounding = Rounding::same(8.0); // Increased rounding

        // Active widgets (e.g., button pressed)
        visuals.widgets.active.bg_fill = AURORA_ACCENT_PURPLE.linear_multiply(0.4); // Dark & Subtle Active Fill (dark purple)
        visuals.widgets.active.fg_stroke = Stroke::new(1.0, AURORA_TEXT_PRIMARY);
        visuals.widgets.active.bg_stroke = Stroke::new(2.0, AURORA_ACCENT_PURPLE.linear_multiply(0.6)); // Dark & Subtle Active Stroke
        visuals.widgets.active.rounding = Rounding::same(8.0); // Increased rounding

        // For open things like ComboBox menus
        visuals.widgets.open.bg_fill = AURORA_BG_PANEL;
        visuals.widgets.open.fg_stroke = Stroke::new(1.0, AURORA_TEXT_PRIMARY);
        visuals.widgets.open.bg_stroke = Stroke::new(1.0, AURORA_ACCENT_NEON_BLUE);
        visuals.widgets.open.rounding = Rounding::same(8.0); // Increased rounding

        visuals.selection.bg_fill = AURORA_ACCENT_PURPLE.linear_multiply(0.5);
        visuals.window_rounding = Rounding::same(0.0);
        visuals.window_shadow = egui::epaint::Shadow::NONE;
        visuals.popup_shadow = egui::epaint::Shadow { offset: Vec2::ZERO, blur: 10.0, spread: 0.0, color: Color32::from_rgb(0, 57, 77) };

        visuals.extreme_bg_color = AURORA_BG_PANEL; // Revert RED, set to desired dark blue
        visuals.widgets.inactive.bg_fill = AURORA_BG_PANEL;
        visuals.widgets.noninteractive.bg_fill = AURORA_BG_PANEL;

        // visuals.background_fill = AURORA_BG_PANEL; // Removed, this field does not exist in current egui::Visuals
        visuals.window_fill = AURORA_BG_DEEP_SPACE; // Main window background remains very dark
        visuals.panel_fill = AURORA_BG_PANEL; // Reverted to original

        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = visuals;
        style.text_styles = [
            (egui::TextStyle::Heading, FontId::new(28.0, egui::FontFamily::Name("KometHeavy".into()))),
            (egui::TextStyle::Name("AuroraTitle".into()), FontId::new(38.0, egui::FontFamily::Name("KometHeavy".into()))),
            (egui::TextStyle::Name("NavItem".into()), FontId::new(18.0, egui::FontFamily::Proportional)), // Uses komet_extralight
            (egui::TextStyle::Body, FontId::new(16.0, egui::FontFamily::Proportional)),      // Uses komet_extralight
            (egui::TextStyle::Button, FontId::new(16.0, egui::FontFamily::Proportional)),   // Uses komet_extralight
            (egui::TextStyle::Small, FontId::new(12.0, egui::FontFamily::Proportional)),    // Uses komet_extralight
            // Monospace will use its default or what's configured in FontDefinitions for Monospace family
            (egui::TextStyle::Monospace, FontId::monospace(15.0)),
        ].into();
        style.spacing.item_spacing = Vec2::new(12.0, 12.0);
        style.spacing.window_margin = Margin::same(0.0);
        style.spacing.button_padding = Vec2::new(10.0, 8.0);
        cc.egui_ctx.set_style(style); // No longer cloning, as it's not stored in self

        let initial_rect = Rect::from_min_size(Pos2::ZERO, cc.egui_ctx.screen_rect().size());
        let particles = (0..50).map(|_| Particle::new(initial_rect)).collect();

        let background_image = RetainedImage::from_image_bytes(
            "circuit_background",
            include_bytes!("../../assets/circuit.png")
        ).map_err(|e| {
            log::error!("Failed to load background image: {:?}", e);
            e
        }).ok();

        let app_settings = AppSettings::load();
        let (loaded_data_vec, wallets_for_display, wallet_load_error) = match crate::wallet::load_keys_from_file(Path::new(&app_settings.keys_path)) {
            Ok(loaded_keys) => {
                log::info!("Loaded {} wallets from {}", loaded_keys.wallets.len(), &app_settings.keys_path);
                let mut loaded_data_mut = loaded_keys.wallets.clone();
                let mut zombie_counter = 0;
                let mut wallet_infos_for_ui: Vec<WalletInfo> = loaded_data_mut.iter().map(|loaded_info| {
                    let dev_wallet_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&app_settings.dev_wallet_private_key);
                    let is_dev_wallet = dev_wallet_pubkey_str_opt.as_ref().map_or(false, |dpk| dpk == &loaded_info.public_key);

                    let parent_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&app_settings.parent_wallet_private_key);
                    let is_parent = parent_pubkey_str_opt.as_ref().map_or(false, |ppk| ppk == &loaded_info.public_key);

                    let name = if is_dev_wallet {
                        "Dev Wallet".to_string()
                    } else if is_parent {
                        "Parent Wallet".to_string()
                    } else {
                        zombie_counter += 1;
                        format!("Zombie {}", zombie_counter)
                    };
                    WalletInfo {
                        address: loaded_info.public_key.clone(),
                        name,
                        is_primary: false,
                        is_dev_wallet,
                        is_parent,
                        sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                        sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                        atomic_buy_sol_amount_input: "0.0".to_string(),
                        atomic_sell_token_amount_input: String::new(), // New field
                        atomic_is_selling: false, // New field
                        atomic_sell_status_message: None, // New field
                    }
                }).collect();
            if let Some(dev_wallet_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&app_settings.dev_wallet_private_key) {
                if !wallet_infos_for_ui.iter().any(|w| w.address == dev_wallet_pubkey_str) {
                    log::info!("Adding Dev wallet ({}) from settings to display list.", dev_wallet_pubkey_str);
                    let is_parent_also = AppSettings::get_pubkey_from_privkey_str(&app_settings.parent_wallet_private_key).map_or(false, |ppk| ppk == dev_wallet_pubkey_str);
                    wallet_infos_for_ui.push(WalletInfo {
                        address: dev_wallet_pubkey_str.clone(),
                        name: "Dev Wallet".to_string(),
                        is_primary: false,
                        is_dev_wallet: true,
                        is_parent: is_parent_also,
                        sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                        sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                        atomic_buy_sol_amount_input: "0.0".to_string(),
                        atomic_sell_token_amount_input: String::new(), // New field
                        atomic_is_selling: false, // New field
                        atomic_sell_status_message: None, // New field
                    });
                    if !loaded_data_mut.iter().any(|w| w.public_key == dev_wallet_pubkey_str) {
                        loaded_data_mut.push(LoadedWalletInfo { name: Some("Dev (from Settings)".to_string()), private_key: app_settings.dev_wallet_private_key.clone(), public_key: dev_wallet_pubkey_str });
                        }
                    }
                }
                if let Some(parent_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&app_settings.parent_wallet_private_key) {
                     if !wallet_infos_for_ui.iter().any(|w| w.address == parent_pubkey_str) {
                        log::info!("Adding Parent wallet ({}) from settings to display list.", parent_pubkey_str);
                        wallet_infos_for_ui.push(WalletInfo {
                            address: parent_pubkey_str.clone(),
                            name: "Parent Wallet".to_string(),
                            is_primary: false,
                            is_dev_wallet: false,
                            is_parent: true,
                            sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                            sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                            atomic_buy_sol_amount_input: "0.0".to_string(),
                            atomic_sell_token_amount_input: String::new(), // New field
                            atomic_is_selling: false, // New field
                            atomic_sell_status_message: None, // New field
                        });
                         if !loaded_data_mut.iter().any(|w| w.public_key == parent_pubkey_str) {
                            loaded_data_mut.push(LoadedWalletInfo { name: Some("Parent (from Settings)".to_string()), private_key: app_settings.parent_wallet_private_key.clone(), public_key: parent_pubkey_str });
                        }
                    }
                }
                (loaded_data_mut, wallet_infos_for_ui, None)
            }
            Err(e) => {
                log::error!("Failed to load keys from {}: {}", &app_settings.keys_path, e);
                (Vec::new(), Vec::new(), Some(format!("Failed to load keys from {}: {}", app_settings.keys_path, e)))
            }
        };

        let (balance_fetch_sender, balance_fetch_receiver) = mpsc::unbounded_channel();
        let (atomic_buy_status_sender, atomic_buy_status_receiver) = mpsc::unbounded_channel();
        let (disperse_result_sender, disperse_result_receiver) = mpsc::unbounded_channel();
        let (gather_result_sender, gather_result_receiver) = mpsc::unbounded_channel();
        let (alt_precalc_result_sender, alt_precalc_result_receiver) = mpsc::unbounded_channel();
        let (alt_creation_status_sender, alt_creation_status_receiver) = mpsc::unbounded_channel();
        let (alt_deactivate_status_sender, alt_deactivate_status_receiver) = mpsc::unbounded_channel();
        let (launch_status_sender, launch_status_receiver) = mpsc::unbounded_channel();
        let (sell_status_sender, sell_status_receiver) = mpsc::unbounded_channel();
        let (pump_status_sender, pump_status_receiver) = mpsc::unbounded_channel();
        let (transfer_status_sender, transfer_status_receiver) = mpsc::unbounded_channel();
        let (sim_status_sender, sim_status_receiver) = mpsc::unbounded_channel();
        let (sim_status_sender_orig, sim_status_receiver_orig) = mpsc::unbounded_channel();
        let (sim_wallet_gen_status_sender, sim_wallet_gen_status_receiver) = mpsc::unbounded_channel();
        let (monitor_update_sender, monitor_update_receiver) = mpsc::unbounded_channel();
        let (volume_bot_wallet_gen_status_sender, volume_bot_wallet_gen_status_receiver) = mpsc::unbounded_channel();
        let (volume_bot_wallets_sender, volume_bot_wallets_receiver) = mpsc::unbounded_channel();
        let (volume_bot_funding_status_sender, volume_bot_funding_status_receiver) = mpsc::unbounded_channel();
        let (sim_consolidate_sol_status_sender_orig, sim_consolidate_sol_status_receiver_orig) = mpsc::unbounded_channel();
        let (pnl_calc_sender, pnl_calc_receiver) = mpsc::unbounded_channel();
        let (atomic_sell_status_sender, atomic_sell_status_receiver) = mpsc::unbounded_channel();
        let (unwrap_wsol_status_sender, unwrap_wsol_status_receiver) = mpsc::unbounded_channel(); // New channel for unwrap WSOL
        let (mix_wallets_status_sender, mix_wallets_status_receiver) = mpsc::unbounded_channel(); // New channel for Mix Wallets
        let (retry_zombie_to_mixed_funding_status_sender, retry_zombie_to_mixed_funding_status_receiver) = mpsc::unbounded_channel(); // Channel for retry funding

        let mut modern_app = Self {
            time: 0.0, particles, background_image, app_settings: app_settings.clone(), current_view: AppView::Launch,
            create_token_name: String::new(), create_token_symbol: String::new(), create_token_description: String::new(),
            create_token_image_path: String::new(), create_dev_buy_amount_tokens: 1_000_000.0, create_token_decimals: 6,
            create_jito_bundle: false, create_zombie_amount: 0, sell_mint: String::new(), sell_slippage_percent: 10.0,
            sell_priority_fee_lamports: 100_000, pump_mint: String::new(), pump_buy_amount_sol: 0.01,
            pump_sell_threshold_sol: 0.005, pump_slippage_percent: 10.0, pump_priority_fee_lamports: 100_000,
            pump_jito_tip_sol: 0.0, pump_lookup_table_address: String::new(), pump_private_key_string: String::new(),
            pump_use_jito_bundle: true, pump_is_running: Arc::new(AtomicBool::new(false)), pump_task_handle: None,
            wallets: wallets_for_display, loaded_wallet_data: loaded_data_vec, wallet_load_error,
            balances_loading: false,
            fetch_balances_in_progress: false, // Initialized
            disperse_in_progress: false,
            gather_in_progress: false,
            gather_tasks_expected: 0,
            gather_tasks_completed: 0,
            balance_tasks_expected: 0,
            balance_tasks_completed: 0,
            parent_sol_balance_display: None,
            total_sol_balance_display: None,
            balance_fetch_sender,
            balance_fetch_receiver, disperse_result_sender, disperse_result_receiver, gather_result_sender,
            gather_result_receiver, alt_precalc_result_sender, alt_precalc_result_receiver, alt_creation_status_sender,
            alt_creation_status_receiver, alt_deactivate_status_sender, alt_deactivate_status_receiver, sell_status_sender,
            sell_status_receiver, sell_log_messages: Vec::new(), sell_mass_reverse_in_progress: false,
            sell_gather_and_sell_in_progress: false, pump_status_sender, pump_status_receiver, pump_log_messages: Vec::new(),
            pump_in_progress: false, launch_status_sender, launch_status_receiver, launch_log_messages: Vec::new(),
            launch_token_name: String::new(), launch_token_symbol: String::new(), launch_token_description: String::new(),
            launch_token_image_url: String::new(), launch_dev_buy_sol: 0.0, launch_zombie_buy_sol: 0.0,
            launch_alt_address: String::new(), launch_mint_keypair_path: String::new(), launch_in_progress: false,
            launch_use_jito_bundle: true, launch_twitter: String::new(), launch_telegram: String::new(),
            launch_website: String::new(), launch_simulate_only: false,
            launch_platform: LaunchPlatform::PumpFun, // Default to Pump.fun
            launch_main_tx_priority_fee_micro_lamports: 500_000, // Added default
            launch_jito_actual_tip_sol: 0.001, // Added default
            launch_completion_link: None, // Initialize new field
            last_operation_result: None,
            check_view_target_mint: String::new(), volume_bot_source_wallets_file: String::new(), disperse_sol_amount: 0.01,
            alt_view_status: "Ready.".to_string(), last_generated_volume_wallets_file: None, alt_log_messages: Vec::new(),
            alt_generated_mint_pubkey: None, alt_precalc_addresses: Vec::new(), alt_address: None,
            alt_creation_in_progress: false, alt_precalc_in_progress: false, alt_deactivation_in_progress: false,
            alt_deactivation_status_message: "ALT Deactivation Idle".to_string(), available_alt_addresses: Vec::new(),
            available_mint_keypairs: Vec::new(), transfer_recipient_address: String::new(), transfer_sol_amount: 0.01,
            transfer_selected_sender: None, transfer_in_progress: false, transfer_status_sender, transfer_status_receiver,
            transfer_log_messages: Vec::new(), sim_token_mint: String::new(), sim_max_cycles: 10, sim_slippage_bps: 50,
            volume_bot_initial_buy_sol_input: "0.01".to_string(), sim_in_progress: false, sim_log_messages: Vec::new(),
            sim_status_sender, sim_status_receiver, sim_task_handle: None, start_volume_bot_request: None,
            start_fetch_balances_request: None, start_load_volume_wallets_request: None, start_distribute_total_sol_request: None,
            start_fund_volume_wallets_request: None, start_gather_all_funds_request: None,
            volume_bot_num_wallets_to_generate: 10, volume_bot_wallets_generated: false, volume_bot_generation_in_progress: false,
            volume_bot_wallets: Vec::new(), volume_bot_wallet_gen_status_sender, volume_bot_wallet_gen_status_receiver,
            volume_bot_wallets_sender, volume_bot_wallets_receiver, volume_bot_funding_per_wallet_sol: 0.01,
            volume_bot_funding_in_progress: false, volume_bot_funding_status_sender, volume_bot_funding_status_receiver,
            volume_bot_total_sol_to_distribute_input: "0.1".to_string(), volume_bot_funding_source_private_key_input: String::new(),
            volume_bot_funding_log_messages: Vec::new(), volume_bot_wallet_display_infos: Vec::new(),
            awaiting_final_gather_status: false, // Initialize new flag
            settings_feedback: None,
            settings_generate_count: 1, sim_token_mint_orig: String::new(), sim_num_wallets_orig: 20,
            sim_wallets_file_orig: "sim_wallets.json".to_string(), sim_initial_sol_orig: 0.05, sim_max_steps_orig: 50,
            sim_slippage_bps_orig: 1500, sim_in_progress_orig: false, sim_log_messages_orig: Vec::new(),
            sim_task_handle_orig: None, sim_status_sender_orig, sim_status_receiver_orig,
            sim_consolidate_sol_in_progress_orig: false, sim_consolidate_sol_status_sender_orig, sim_consolidate_sol_status_receiver_orig,
            sim_wallet_gen_in_progress: false, sim_wallet_gen_status_sender, sim_wallet_gen_status_receiver,
            monitor_active: Arc::new(AtomicBool::new(false)), monitor_task_handle: None, monitor_balances: Vec::new(),
            monitor_prices: None, monitor_update_sender, monitor_update_receiver,
            pnl_summary: None,
            pnl_calculation_in_progress: false,
            pnl_error_message: None,
            pnl_calc_sender,
            pnl_calc_receiver,

            // --- Atomic Buy View State Init (from app.rs) ---
            atomic_buy_mint_address: String::new(),
            atomic_buy_slippage_bps: 200, // Default 2% (200 bps) from app.rs
            atomic_buy_priority_fee_lamports_per_tx: 100_000, // Default 0.0001 SOL per tx from app.rs
            atomic_buy_alt_address: String::new(), // Initialize, though command might not use it
            // This field should be part of the main Self struct initialization as well
            // It was correctly added to the struct definition and the sub-section for Atomic Buy state,
            // but also needs to be listed here in the primary Self { ... } block.
            // The previous diff added it to the sub-section, this ensures it's in the main list.
            atomic_buy_exchange: "pumpfun".to_string(),
            atomic_buy_in_progress: false,
            atomic_buy_status_sender, // Channel created above
            atomic_buy_status_receiver, // Channel created above
            atomic_buy_log_messages: Vec::new(),
            atomic_buy_jito_tip_sol: 0.0001, // Default Jito tip from app.rs (likely for overall bundle)
            atomic_buy_jito_tip_per_tx_str: "0.0".to_string(), // Initialized
            atomic_buy_jito_tip_overall_str: "0.0001".to_string(), // Initialized, matching old atomic_buy_jito_tip_sol
            atomic_buy_task_handle: None,

            // Initialize new atomic_sell fields
            atomic_sell_in_progress: false,
            atomic_sell_task_handle: None,
            atomic_sell_status_sender,
            atomic_sell_status_receiver,

            // Initialize Unwrap WSOL State
            unwrap_wsol_in_progress: false,
            unwrap_wsol_log_messages: Vec::new(),
            unwrap_wsol_status_sender,
            unwrap_wsol_status_receiver,
            unwrap_wsol_task_handle: None, // Added

            // Mix Wallets State
            mix_wallets_in_progress: false,
            mix_wallets_log_messages: Vec::new(),
            mix_wallets_status_sender,
            mix_wallets_status_receiver,
            start_mix_wallets_request: None,
            mix_wallets_task_handle: None,

            // Retry Zombie to Mixed Wallets Funding State
            last_mixed_wallets_file_path: None,
            retry_zombie_to_mixed_funding_in_progress: false,
            retry_zombie_to_mixed_funding_log_messages: Vec::new(),
            retry_zombie_to_mixed_funding_status_sender,
            retry_zombie_to_mixed_funding_status_receiver,
            retry_zombie_to_mixed_funding_task_handle: None,
            // Removed duplicated lines below

            // Ensure all other atomic_buy fields are correctly listed here if they were missed.
            // Based on the struct definition, these are:
            // atomic_buy_mint_address: String::new(), (already present at 1568)
            // atomic_buy_slippage_bps: 200, (already present at 1569)
            // atomic_buy_priority_fee_lamports_per_tx: 100_000, (already present at 1570)
            // The other atomic_buy_ fields like _sol_per_wallet, _selected_alt, _wallets_to_use, _select_all_wallets
            // are typically initialized based on user input or defaults from app_settings,
            // so their direct initialization here might differ or be handled by app_settings load.
            // For now, focusing on the missing `atomic_buy_exchange`.
        };
        modern_app.load_available_alts();
        modern_app.load_available_mint_keypairs();
        modern_app
    }

    // Copied helper methods from PumpFunApp (impl ModernApp)
    fn load_available_alts(&mut self) {
        self.available_alt_addresses.clear();
        const ALT_FILE: &str = "alt_address.txt";
        match std::fs::read_to_string(ALT_FILE) {
            Ok(content) => {
                let alt_addr_str = content.trim();
                if !alt_addr_str.is_empty() {
                    match Pubkey::from_str(alt_addr_str) {
                        Ok(_) => {
                            log::info!("Loaded ALT address {} from {}", alt_addr_str, ALT_FILE);
                            self.available_alt_addresses.push(alt_addr_str.to_string());
                        }
                        Err(e) => log::warn!("File {} contains invalid pubkey '{}': {}", ALT_FILE, alt_addr_str, e),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => log::info!("ALT file {} not found.", ALT_FILE),
            Err(e) => log::error!("Failed to read ALT file {}: {}", ALT_FILE, e),
        }
    }

    fn load_available_mint_keypairs(&mut self) {
        self.available_mint_keypairs.clear();
        log::info!("Scanning current directory for potential mint keypair .json files...");
        match std::fs::read_dir(".") {
            Ok(entries) => {
                for entry_result in entries {
                    if let Ok(entry) = entry_result {
                        let path = entry.path();
                        if path.is_file() && path.extension().map_or(false, |ext| ext == "json") {
                            let path_str = path.to_string_lossy().to_string();
                            match std::fs::read_to_string(&path) {
                                Ok(content) => {
                                    match serde_json::from_str::<Vec<u8>>(&content) {
                                        Ok(bytes) if bytes.len() == 64 => {
                                            if Keypair::from_bytes(&bytes).is_ok() {
                                                log::info!("  Found valid keypair file: {}", path_str);
                                                self.available_mint_keypairs.push(path_str);
                                            } else { log::trace!("  Skipping file (not a valid keypair format): {}", path_str); }
                                        }
                                        _ => log::trace!("  Skipping file (not Vec<u8> or wrong length): {}", path_str),
                                    }
                                }
                                Err(e) => log::warn!("  Failed to read potential keypair file {}: {}", path_str, e),
                            }
                        }
                    }
                }
                 log::info!("Found {} potential keypair files.", self.available_mint_keypairs.len());
            }
            Err(e) => log::error!("Failed to read current directory to scan for keypairs: {}", e),
        }
    }

    fn load_wallets_from_settings_path(&mut self) {
        let (loaded_data, display_wallets, error) =
            match crate::wallet::load_keys_from_file(Path::new(&self.app_settings.keys_path)) {
                Ok(loaded_keys) => {
                    log::info!("Loaded {} wallets from {}", loaded_keys.wallets.len(), &self.app_settings.keys_path);
                    let mut zombie_counter = 0; // Counter for "Zombie X" naming
                    let mut wallet_infos_for_ui: Vec<WalletInfo> = loaded_keys.wallets.iter().map(|loaded_info| {
                        let dev_wallet_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key);
                        let is_dev_wallet = dev_wallet_pubkey_str_opt.as_ref().map_or(false, |dpk| dpk == &loaded_info.public_key);

                        let parent_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                        let is_parent = parent_pubkey_str_opt.as_ref().map_or(false, |ppk| ppk == &loaded_info.public_key);

                        let name = if is_dev_wallet {
                            "Dev Wallet".to_string()
                        } else if is_parent {
                            "Parent Wallet".to_string()
                        } else {
                            zombie_counter += 1;
                            format!("Zombie {}", zombie_counter)
                        };

                        WalletInfo {
                            address: loaded_info.public_key.clone(),
                            name, // Use the determined name
                            is_primary: false, // No longer using "Primary" for the first zombie
                            is_dev_wallet,
                            is_parent,
                            sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                            sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                            atomic_buy_sol_amount_input: "0.0".to_string(),
                            atomic_sell_token_amount_input: String::new(), // New field
                            atomic_is_selling: false, // New field
                            atomic_sell_status_message: None, // New field
                        }
                    }).collect();

                    // Add Dev Wallet if not already loaded from keys_path
                    if let Some(dev_wallet_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key) {
                        if !wallet_infos_for_ui.iter().any(|w| w.address == dev_wallet_pubkey_str) {
                            log::info!("Adding Dev wallet ({}) from settings to display list.", dev_wallet_pubkey_str);
                            // Check if this Dev wallet is also the Parent wallet
                            let is_parent_also = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key).map_or(false, |ppk| ppk == dev_wallet_pubkey_str);
                            wallet_infos_for_ui.push(WalletInfo {
                                address: dev_wallet_pubkey_str.clone(),
                                name: "Dev Wallet".to_string(),
                                is_primary: false,
                                is_dev_wallet: true,
                                is_parent: is_parent_also, // Mark if it's also parent
                                sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                                sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                                atomic_buy_sol_amount_input: "0.0".to_string(),
                                atomic_sell_token_amount_input: String::new(), // New field
                                atomic_is_selling: false, // New field
                                atomic_sell_status_message: None, // New field
                            });
                        }
                    }

                    // Add Parent Wallet if not already loaded from keys_path and not the Dev wallet (already added)
                    if let Some(parent_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key) {
                         if !wallet_infos_for_ui.iter().any(|w| w.address == parent_pubkey_str) {
                            log::info!("Adding Parent wallet ({}) from settings to display list.", parent_pubkey_str);
                            wallet_infos_for_ui.push(WalletInfo {
                                address: parent_pubkey_str.clone(),
                                name: "Parent Wallet".to_string(),
                                is_primary: false,
                                is_dev_wallet: false, // It's not the Dev wallet if we are in this block
                                is_parent: true,
                                sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: false, is_selected: false,
                                sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                                atomic_buy_sol_amount_input: "0.0".to_string(),
                                atomic_sell_token_amount_input: String::new(), // New field
                                atomic_is_selling: false, // New field
                                atomic_sell_status_message: None, // New field
                            });
                        }
                    }
                    self.settings_feedback = Some(format!("Successfully loaded {} wallets from {}", loaded_keys.wallets.len(), self.app_settings.keys_path));
                    (loaded_keys.wallets, wallet_infos_for_ui, None)
                }
                Err(e) => {
                    log::error!("Failed to load keys from {}: {}", &self.app_settings.keys_path, e);
                    let error_msg = format!("Failed to load keys from {}: {}", self.app_settings.keys_path, e);
                    self.settings_feedback = Some(error_msg.clone());
                    (Vec::new(), Vec::new(), Some(error_msg))
                }
            };
        self.loaded_wallet_data = loaded_data; // This stores the raw LoadedWalletInfo
        self.wallets = display_wallets;       // This stores the GUI-specific WalletInfo with names
        self.wallet_load_error = error;
    }

    fn trigger_balance_fetch(&mut self, target_mint_override: Option<String>) {
        if self.balances_loading { log::warn!("Balance fetch already in progress."); return; }
        for wallet in self.wallets.iter_mut() {
            wallet.sol_balance = None; wallet.target_mint_balance = None; wallet.error = None; wallet.is_loading = true;
        }
        let mut addresses_to_check: Vec<String> = self.wallets.iter().map(|w| w.address.clone()).collect();
        if let Some(parent_addr) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key) {
             if !addresses_to_check.contains(&parent_addr) { addresses_to_check.push(parent_addr); }
        }
        if let Some(minter_addr) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key) {
             if !addresses_to_check.contains(&minter_addr) { addresses_to_check.push(minter_addr); }
        }
        addresses_to_check.sort(); addresses_to_check.dedup();
        log::info!("Triggering balance fetch for {} unique wallets...", addresses_to_check.len());
        self.balances_loading = true; self.last_operation_result = Some(Ok("Fetching balances...".to_string()));
        let num_wallets_to_fetch = addresses_to_check.len();
        if num_wallets_to_fetch == 0 {
            log::warn!("No wallets loaded to fetch balances for.");
            self.balances_loading = false; self.last_operation_result = Some(Err("No wallets loaded.".to_string()));
            self.balance_tasks_expected = 0; self.balance_tasks_completed = 0;
            return;
        }
        self.balance_tasks_expected = num_wallets_to_fetch; self.balance_tasks_completed = 0;
        let rpc_url = self.app_settings.solana_rpc_url.clone();
        let target_mint = target_mint_override.unwrap_or_else(|| self.check_view_target_mint.trim().to_string());
        let sender = self.balance_fetch_sender.clone();
        tokio::spawn(async move {
            fetch_balances_task(rpc_url, addresses_to_check, target_mint, sender).await;
        });
    }

    fn calculate_totals(&self) -> (f64, Option<f64>) {
        let mut total_sol = 0.0;
        let mut current_total_tokens = 0.0;
        let mut has_token_balances = false;
        for wallet_info in self.wallets.iter().filter(|w| !w.is_dev_wallet && !w.is_primary) {
            if let Some(balance) = wallet_info.sol_balance { total_sol += balance; }
            if let Some(balance_str) = &wallet_info.target_mint_balance {
                 match balance_str.replace(',', "").parse::<f64>() {
                    Ok(val) => { current_total_tokens += val; has_token_balances = true; }
                    Err(_) => {}
                }
            }
        }
        (total_sol, if has_token_balances { Some(current_total_tokens) } else { None })
    }

    // ... (Other helper methods like trigger_individual_sell, trigger_mass_sell_reverse, etc. will be here)

    // DUPLICATE refresh_application_state (lines 1238-1386) REMOVED
    // The following is the start of the next function:
    // --- Sell View (Adapted from app.rs with Aurora Theme) ---


    // --- Sell View (Adapted from app.rs with Aurora Theme) ---
    fn show_sell_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |frame_content_ui| { // Renamed 'ui' to 'frame_content_ui' for clarity
                egui::ScrollArea::vertical()
                    .id_source("sell_view_main_scroll_modern") // Added a unique ID for the scroll area
                    .auto_shrink([false, false]) // Ensures the scroll area expands to fill available space
                    .show(frame_content_ui, |scroll_ui| { // Content within this closure will be scrollable
                // All direct children of the original .show() lambda now use scroll_ui
                scroll_ui.vertical_centered(|ui| { // This 'ui' is for the vertical_centered content, derived from scroll_ui
                    ui.label(
                        egui::RichText::new("💸 Sell Tokens")
                            .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                            .color(Color32::WHITE),
                    );
                });
                scroll_ui.add_space(5.0);
                scroll_ui.separator();
                scroll_ui.add_space(15.0);

                scroll_ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| {
                    ui_centered.set_max_width(1400.0); // Max width for content

                    // --- Inputs Card ---
                    show_section_header(ui_centered, "📉 Sell Configuration", AURORA_ACCENT_MAGENTA);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.horizontal_wrapped(|ui_inputs| {
                            ui_inputs.label(electric_label_text("Mint Address:"));
                            egui::Frame::none()
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                .rounding(ui_inputs.style().visuals.widgets.inactive.rounding)
                                .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                .show(ui_inputs, |cell_ui| {
                                    cell_ui.add(
                                        egui::TextEdit::singleline(&mut self.sell_mint)
                                            .desired_width(300.0)
                                            .text_color(AURORA_TEXT_PRIMARY)
                                            .frame(false)
                                    );
                                });
                            if create_electric_button(ui_inputs, "🔄 Refresh Balances", None).clicked() {
                                if !self.sell_mint.is_empty() {
                                    self.trigger_balance_fetch(Some(self.sell_mint.clone()));
                                } else {
                                    self.trigger_balance_fetch(None);
                                    self.last_operation_result = Some(Err("Enter Mint Address for token balances.".to_string()));
                                }
                            }
                            if self.balances_loading {
                                ui_inputs.add(egui::Spinner::new().size(16.0));
                            }
                        });
                        ui_frame.horizontal_wrapped(|ui_inputs| {
                            ui_inputs.label(electric_label_text("Slippage (%):"));
                            ui_inputs.add(egui::DragValue::new(&mut self.sell_slippage_percent).speed(0.1).clamp_range(0.0..=50.0).suffix("%"));
                            ui_inputs.add_space(15.0);
                            ui_inputs.label(electric_label_text("Priority Fee (lamports):"));
                            ui_inputs.add(egui::DragValue::new(&mut self.sell_priority_fee_lamports).speed(1000.0).clamp_range(0..=10_000_000));
                        });
                    });
                    ui_centered.add_space(20.0);

                    // --- Wallet Dashboard Card ---
                    show_section_header(ui_centered, "💼 Wallet Dashboard", AURORA_ACCENT_TEAL);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        egui::ScrollArea::vertical() // This is an existing scroll area, keep it
                            .id_source("sell_wallet_scroll_modern")
                            .max_height(500.0)
                            .auto_shrink([false, false])
                            .show(ui_frame, |ui_scroll_wallet| { // Renamed ui_scroll to ui_scroll_wallet for clarity
                                egui::Grid::new("sell_wallet_grid_modern")
                                    .num_columns(6)
                                    .spacing([10.0, 6.0])
                                    .striped(true)
                                    .show(ui_scroll_wallet, |ui_grid| {
                                        ui_grid.label(egui::RichText::new("Type").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                        ui_grid.label(egui::RichText::new("Address").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                        ui_grid.label(egui::RichText::new("SOL").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                        ui_grid.label(egui::RichText::new("Tokens").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                        ui_grid.label(egui::RichText::new("Sell Config").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                        ui_grid.label(egui::RichText::new("Actions").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                        ui_grid.end_row();

                                        let mut trading_wallet_counter = 0;
                                        let wallets_clone = self.wallets.clone();

                                        for i in 0..wallets_clone.len() {
                                            let wallet_display_info = &wallets_clone[i];

                                            let type_label_text = if wallet_display_info.is_parent {
                                                egui::RichText::new("Parent").color(AURORA_ACCENT_TEAL)
                                            } else if wallet_display_info.is_dev_wallet {
                                                egui::RichText::new("Dev").color(AURORA_ACCENT_PURPLE)
                                            } else if wallet_display_info.is_primary {
                                                egui::RichText::new("Primary").color(AURORA_ACCENT_NEON_BLUE)
                                            } else {
                                                trading_wallet_counter += 1;
                                                egui::RichText::new(format!("Bot {}", trading_wallet_counter)).color(AURORA_TEXT_PRIMARY)
                                            };
                                            ui_grid.label(type_label_text.font(FontId::proportional(14.0)));

                                            let address_short = format!("{}...{}", &wallet_display_info.address[..6], &wallet_display_info.address[wallet_display_info.address.len()-4..]);
                                            ui_grid.label(egui::RichText::new(&address_short).font(FontId::monospace(14.0))).on_hover_text(&wallet_display_info.address);

                                            ui_grid.label(egui::RichText::new(wallet_display_info.sol_balance.map_or("-".to_string(), |bal| format!("{:.4}", bal))).font(FontId::monospace(14.0)));
                                            ui_grid.label(egui::RichText::new(wallet_display_info.target_mint_balance.as_deref().unwrap_or("-")).font(FontId::monospace(14.0)));

                                            let wallet_info_mutable = &mut self.wallets[i];
                                            ui_grid.horizontal(|ui_inputs_cell| {
                                                egui::Frame::none()
                                                    .fill(AURORA_BG_PANEL)
                                                    .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                                    .rounding(ui_inputs_cell.style().visuals.widgets.inactive.rounding)
                                                    .inner_margin(egui::Margin::symmetric(4.0, 1.0))
                                                    .show(ui_inputs_cell, |cell_ui| {
                                                        cell_ui.add(
                                                            egui::TextEdit::singleline(&mut wallet_info_mutable.sell_percentage_input)
                                                                .desired_width(45.0)
                                                                .hint_text("%")
                                                                .text_color(AURORA_TEXT_PRIMARY)
                                                                .frame(false)
                                                        );
                                                    });
                                                ui_inputs_cell.add_space(4.0);
                                                egui::Frame::none()
                                                    .fill(AURORA_BG_PANEL)
                                                    .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                                    .rounding(ui_inputs_cell.style().visuals.widgets.inactive.rounding)
                                                    .inner_margin(egui::Margin::symmetric(4.0, 1.0))
                                                    .show(ui_inputs_cell, |cell_ui| {
                                                        cell_ui.add(
                                                            egui::TextEdit::singleline(&mut wallet_info_mutable.sell_amount_input)
                                                                .min_size(Vec2::new(120.0, 0.0))
                                                                .hint_text("Amt")
                                                                .text_color(AURORA_TEXT_PRIMARY)
                                                                .frame(false)
                                                        );
                                                    });
                                            });

                                            ui_grid.horizontal(|ui_actions| {
                                                let is_selling_this_wallet = wallet_display_info.is_selling;
                                                let sell_actions_enabled = !is_selling_this_wallet && !self.sell_mass_reverse_in_progress && !self.sell_gather_and_sell_in_progress;

                                                if create_electric_button(ui_actions, "Sell %", None).on_hover_text("Sell specified percentage").clicked() && sell_actions_enabled {
                                                    self.trigger_individual_sell(wallet_display_info.address.clone(), true);
                                                }
                                                if create_electric_button(ui_actions, "Sell Amt", None).on_hover_text("Sell specified amount").clicked() && sell_actions_enabled {
                                                    self.trigger_individual_sell(wallet_display_info.address.clone(), false);
                                                }
                                                if is_selling_this_wallet {
                                                    ui_actions.add_space(4.0);
                                                    ui_actions.add(egui::Spinner::new().size(14.0));
                                                }
                                            });
                                            ui_grid.end_row();
                                        }
                                    });
                            });
                    });
                    ui_centered.add_space(20.0);

                    // --- Logs and Mass Actions Card ---
                    show_section_header(ui_centered, "📊 Logs & Mass Actions", AURORA_ACCENT_NEON_BLUE);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.strong(electric_label_text("Sell Logs:"));
                        egui::ScrollArea::vertical().id_source("sell_log_scroll_modern").max_height(150.0).stick_to_bottom(true).show(ui_frame, |ui_log_scroll| { // This is an existing scroll area
                            for msg in &self.sell_log_messages {
                                ui_log_scroll.label(egui::RichText::new(msg).font(FontId::monospace(12.0)));
                            }
                        });
                        ui_frame.separator();
                        ui_frame.add_space(10.0);
                        ui_frame.strong(electric_label_text("Mass Sell Actions:"));
                        ui_frame.horizontal_wrapped(|ui_mass_actions| {
                            let mass_actions_enabled = !self.sell_mass_reverse_in_progress && !self.sell_gather_and_sell_in_progress && self.wallets.iter().all(|w| !w.is_selling);

                            if create_electric_button(ui_mass_actions, "Mass Sell (Reverse Order)", None).on_hover_text("Sell 100% from all wallets, last to first.").clicked() && mass_actions_enabled {
                                self.trigger_mass_sell_reverse();
                            }
                            if self.sell_mass_reverse_in_progress {
                                ui_mass_actions.add_space(5.0); ui_mass_actions.add(egui::Spinner::new().size(16.0));
                            }
                            ui_mass_actions.add_space(10.0);
                            if create_electric_button(ui_mass_actions, "Gather All & Sell from Primary", None).on_hover_text("Gather tokens to primary, then sell all from primary.").clicked() && mass_actions_enabled {
                                self.trigger_gather_and_sell();
                            }
                            if self.sell_gather_and_sell_in_progress {
                                ui_mass_actions.add_space(5.0); ui_mass_actions.add(egui::Spinner::new().size(16.0));
                            }
                        });
                    });
                     // Display last operation result (general for the view)
                    if let Some(result) = &self.last_operation_result {
                        ui_centered.add_space(10.0);
                        match result {
                            Ok(msg) => ui_centered.label(egui::RichText::new(format!("✅ {}", msg)).color(AURORA_ACCENT_TEAL)),
                            Err(e) => ui_centered.label(egui::RichText::new(format!("❌ {}", e)).color(AURORA_ACCENT_MAGENTA)),
                        };
                    }
                    ui_centered.add_space(20.0); // Bottom padding
                }); // End Centered Layout (content of scroll_ui.with_layout)
            }); // End ScrollArea
            }); // End Outer Frame
    }

    // Copied from app.rs
    fn trigger_individual_sell(&mut self, address: String, is_percentage: bool) {
        info!("Triggered individual sell for address: {}, is_percentage: {}", address, is_percentage);
        if let Some(wallet_idx) = self.wallets.iter().position(|w| w.address == address) {
            let wallet_info = &mut self.wallets[wallet_idx];
            wallet_info.is_selling = true;
            let input_value = if is_percentage { wallet_info.sell_percentage_input.clone() } else { wallet_info.sell_amount_input.clone() };
            let mint_address = self.sell_mint.clone();
            let wallet_address = address.clone();
            let slippage_percent = self.sell_slippage_percent;
            let priority_fee_lamports = self.sell_priority_fee_lamports;
            let status_sender = self.sell_status_sender.clone();
            let private_key_opt = self.loaded_wallet_data.iter().find(|w| w.public_key == address).map(|w| w.private_key.clone());

            if let Some(private_key) = private_key_opt {
                tokio::spawn(async move {
                    info!("Individual Sell Task: Started for wallet {}", wallet_address);
                    info!("Individual Sell Task: Sending SellStatus::InProgress - Selling from wallet");
                    let send_result = status_sender.send(SellStatus::InProgress(format!("Selling from wallet {}", wallet_address)));
                    info!("Individual Sell Task: Sent SellStatus::InProgress - Selling from wallet. Result: {:?}", send_result.is_ok());

                    let amount_value = match input_value.trim().parse::<f64>() {
                        Ok(v) if v > 0.0 => v,
                        _ => {
                            info!("Individual Sell Task: Invalid sell amount/percentage. Sending WalletFailure.");
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, "Invalid sell amount/percentage".to_string()));
                            info!("Individual Sell Task: Sent WalletFailure for invalid amount.");
                            return;
                        }
                    };
                    info!("Individual Sell Task: Parsed amount_value: {}", amount_value);

                    let keypair_bytes = match bs58::decode(&private_key).into_vec() {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            info!("Individual Sell Task: Error decoding private key. Sending WalletFailure.");
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Error decoding private key: {}", e)));
                            info!("Individual Sell Task: Sent WalletFailure for PK decode error.");
                            return;
                        }
                    };
                    info!("Individual Sell Task: Decoded private key.");

                    let wallet = match solana_sdk::signature::Keypair::from_bytes(&keypair_bytes) {
                        Ok(kp) => kp,
                        Err(e) => {
                            info!("Individual Sell Task: Error creating keypair. Sending WalletFailure.");
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Error creating keypair: {}", e)));
                            info!("Individual Sell Task: Sent WalletFailure for keypair creation error.");
                            return;
                        }
                    };
                    info!("Individual Sell Task: Created keypair.");

                    let mint_pubkey = match solana_sdk::pubkey::Pubkey::from_str(&mint_address) {
                        Ok(pk) => pk,
                        Err(e) => {
                            info!("Individual Sell Task: Invalid mint address. Sending WalletFailure.");
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Invalid mint address: {}", e)));
                            info!("Individual Sell Task: Sent WalletFailure for invalid mint address.");
                            return;
                        }
                    };
                    info!("Individual Sell Task: Parsed mint_pubkey.");

                    let dynamic_fee_lamports: u64 = {
                        info!("Individual Sell Task: Calculating dynamic fee lamports...");
                        let calc_rpc_client = AsyncRpcClient::new(config::get_rpc_url());
                        let http_client = ReqwestClient::new();
                        let seller_ata = get_associated_token_address(&wallet.pubkey(), &mint_pubkey);
                        let (seller_token_balance_lamports, token_decimals) = match calc_rpc_client.get_token_account_balance(&seller_ata).await {
                            Ok(balance_response) => match balance_response.amount.parse::<u64>() {
                                Ok(bal_lamports) => (bal_lamports, balance_response.decimals),
                                Err(_) => {
                                    info!("Individual Sell Task: Fee Calc - Failed to parse token balance. Sending WalletFailure.");
                                    let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Failed to parse token balance.".to_string()));
                                    info!("Individual Sell Task: Sent WalletFailure for fee calc balance parse error.");
                                    return;
                                }
                            },
                            Err(_) => (0, 0),
                        };
                        info!("Individual Sell Task: Fee Calc - Seller balance: {}, Decimals: {}", seller_token_balance_lamports, token_decimals);

                        if seller_token_balance_lamports == 0 && !is_percentage {
                            info!("Individual Sell Task: Fee Calc - No tokens to sell. Sending WalletFailure.");
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: No tokens to sell.".to_string()));
                            info!("Individual Sell Task: Sent WalletFailure for no tokens to sell (fee calc).");
                            return;
                        }
                        let tokens_for_quote_lamports = if is_percentage {
                            if seller_token_balance_lamports == 0 {
                                info!("Individual Sell Task: Fee Calc - Selling 0 tokens (zero balance for percentage). Sending WalletFailure.");
                                let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Selling 0 tokens (zero balance).".to_string()));
                                info!("Individual Sell Task: Sent WalletFailure for zero balance (percentage fee calc).");
                                return;
                            }
                            (seller_token_balance_lamports as f64 * amount_value / 100.0) as u64
                        } else { spl_token::ui_amount_to_amount(amount_value, token_decimals) };
                        info!("Individual Sell Task: Fee Calc - Tokens for quote: {}", tokens_for_quote_lamports);

                        if tokens_for_quote_lamports == 0 {
                            info!("Individual Sell Task: Fee Calc - Calculated tokens to sell is zero. Sending WalletFailure.");
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Calculated tokens to sell is zero.".to_string()));
                            info!("Individual Sell Task: Sent WalletFailure for zero calculated tokens (fee calc).");
                            return;
                        }

                        info!("Individual Sell Task: Sending SellStatus::InProgress - Getting Jupiter quote (Fee Calc)");
                        let send_result_quote_fee = status_sender.send(SellStatus::InProgress(format!("Getting Jupiter quote for {} (Fee Calc)", wallet_address)));
                        info!("Individual Sell Task: Sent SellStatus::InProgress - Getting Jupiter quote (Fee Calc). Result: {:?}", send_result_quote_fee.is_ok());

                        match get_jupiter_quote_v6(&http_client, &mint_address, SOL_MINT_ADDRESS_STR, tokens_for_quote_lamports, slippage_percent as u16).await {
                            Ok(quote_resp) => match quote_resp.out_amount.parse::<u64>() {
                                Ok(sol_received) => {
                                    info!("Individual Sell Task: Fee Calc - Jupiter quote SOL received: {}", sol_received);
                                    if sol_received == 0 { 0 } else { std::cmp::max(1, sol_received / 1000) }
                                },
                                Err(_) => {
                                    info!("Individual Sell Task: Fee Calc - Failed to parse SOL out_amount. Sending WalletFailure.");
                                    let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Failed to parse SOL out_amount.".to_string()));
                                    info!("Individual Sell Task: Sent WalletFailure for SOL out_amount parse error (fee calc).");
                                    return;
                                }
                            },
                            Err(e) => {
                                info!("Individual Sell Task: Fee Calc - Jupiter quote failed: {}. Sending WalletFailure.", e);
                                let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Fee Calc: Jupiter quote failed: {}", e)));
                                info!("Individual Sell Task: Sent WalletFailure for Jupiter quote failure (fee calc).");
                                return;
                            }
                        }
                    };
                    info!("Individual Sell Task: Calculated dynamic_fee_lamports: {}", dynamic_fee_lamports);

                    info!("Individual Sell Task: Sending SellStatus::InProgress - Creating Pump.fun sell tx");
                    let send_result_create_tx = status_sender.send(SellStatus::InProgress(format!("Creating Pump.fun sell tx for {}", wallet_address)));
                    info!("Individual Sell Task: Sent SellStatus::InProgress - Creating Pump.fun sell tx. Result: {:?}", send_result_create_tx.is_ok());

                    match api::pumpfun::sell_token(&wallet, &mint_pubkey, amount_value, slippage_percent as u8, priority_fee_lamports as f64 / solana_sdk::native_token::LAMPORTS_PER_SOL as f64).await {
                        Ok(versioned_transaction) => {
                            info!("Individual Sell Task: Pump.fun sell_token call successful. Versioned Tx created.");
                            info!("Individual Sell Task: Sending SellStatus::InProgress - Sending main sell tx");
                            let send_result_sending_main = status_sender.send(SellStatus::InProgress(format!("Sending main sell tx for {}", wallet_address)));
                            info!("Individual Sell Task: Sent SellStatus::InProgress - Sending main sell tx. Result: {:?}", send_result_sending_main.is_ok());

                            let main_sell_rpc_client = AsyncRpcClient::new(config::get_rpc_url());
                            match utils::transaction::sign_and_send_versioned_transaction(&main_sell_rpc_client, versioned_transaction, &[&wallet]).await {
                                Ok(signature) => {
                                    info!("Individual Sell Task: Main sell tx successful. Signature: {}", signature);
                                    info!("Individual Sell Task: Sending SellStatus::WalletSuccess (main sell)");
                                    let send_result_main_success = status_sender.send(SellStatus::WalletSuccess(wallet_address.clone(), signature.to_string()));
                                    info!("Individual Sell Task: Sent SellStatus::WalletSuccess (main sell). Result: {:?}", send_result_main_success.is_ok());

                                    if dynamic_fee_lamports > 0 {
                                        info!("Individual Sell Task: Dynamic fee > 0 ({} lamports). Spawning fee payment task.", dynamic_fee_lamports);
                                        let fee_rpc_url = config::get_rpc_url();
                                        let fee_wallet_keypair_bytes = wallet.to_bytes();
                                        let fee_status_sender = status_sender.clone();
                                        let fee_wallet_address = wallet_address.clone();
                                        tokio::spawn(async move {
                                            info!("Fee Payment Task: Started for wallet {}", fee_wallet_address);
                                            let fee_wallet = Keypair::from_bytes(&fee_wallet_keypair_bytes).expect("Fee task: Failed to recreate keypair from bytes");
                                            let recipient_pubkey = Pubkey::from_str(FEE_RECIPIENT_ADDRESS_FOR_SELL_STR).expect("Fee task: Invalid fee recipient pubkey");
                                            let transfer_ix = system_instruction::transfer(&fee_wallet.pubkey(), &recipient_pubkey, dynamic_fee_lamports);
                                            let compute_budget_ix = compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports);
                                            let fee_tx_rpc_client = AsyncRpcClient::new(fee_rpc_url);
                                            if let Ok(latest_blockhash) = fee_tx_rpc_client.get_latest_blockhash().await {
                                                info!("Fee Payment Task: Got latest blockhash for fee tx.");
                                                let tx = Transaction::new_signed_with_payer(&[compute_budget_ix, transfer_ix], Some(&fee_wallet.pubkey()), &[&fee_wallet], latest_blockhash);
                                                match fee_tx_rpc_client.send_and_confirm_transaction_with_spinner(&tx).await {
                                                    Ok(fee_sig) => {
                                                        info!("Fee Payment Task: Fee tx successful. Sig: {}", fee_sig);
                                                        info!("Fee Payment Task: Sending SellStatus::WalletSuccess (fee)");
                                                        let _ = fee_status_sender.send(SellStatus::WalletSuccess(fee_wallet_address, format!("Fee ({} lamports) sent. Sig: {}", dynamic_fee_lamports, fee_sig)));
                                                        info!("Fee Payment Task: Sent SellStatus::WalletSuccess (fee).");
                                                    },
                                                    Err(e) => {
                                                        info!("Fee Payment Task: Fee tx send/confirm failed: {}", e);
                                                        info!("Fee Payment Task: Sending SellStatus::WalletFailure (fee)");
                                                        let _ = fee_status_sender.send(SellStatus::WalletFailure(fee_wallet_address, format!("Fee Tx: Send failed: {}", e)));
                                                        info!("Fee Payment Task: Sent SellStatus::WalletFailure (fee).");
                                                    }
                                                }
                                            } else {
                                                info!("Fee Payment Task: Failed to get blockhash for fee tx.");
                                                info!("Fee Payment Task: Sending SellStatus::WalletFailure (fee blockhash)");
                                                let _ = fee_status_sender.send(SellStatus::WalletFailure(fee_wallet_address, "Fee Tx: Blockhash failed".to_string()));
                                                info!("Fee Payment Task: Sent SellStatus::WalletFailure (fee blockhash).");
                                            }
                                        });
                                    } else {
                                        info!("Individual Sell Task: Dynamic fee is 0. Sending SellStatus::WalletSuccess (main sell, no fee).");
                                        let send_result_no_fee = status_sender.send(SellStatus::WalletSuccess(wallet_address.clone(), "Main sell successful. Fee was 0.".to_string()));
                                        info!("Individual Sell Task: Sent SellStatus::WalletSuccess (main sell, no fee). Result: {:?}", send_result_no_fee.is_ok());
                                    }
                                },
                                Err(e) => {
                                    info!("Individual Sell Task: Main sell tx failed: {}. Sending WalletFailure.", e);
                                    let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Main sell tx failed: {}", e)));
                                    info!("Individual Sell Task: Sent WalletFailure for main sell tx failure.");
                                }
                            }
                        },
                        Err(e) => {
                            info!("Individual Sell Task: Failed to create main sell transaction (api::pumpfun::sell_token error): {}. Sending WalletFailure.", e);
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Failed to create main sell transaction: {}", e)));
                            info!("Individual Sell Task: Sent WalletFailure for pumpfun sell_token error.");
                        }
                    }
                    info!("Individual Sell Task: Finished for wallet {}", wallet_address);
                });
            } else {
                wallet_info.is_selling = false;
                let _ = self.sell_status_sender.send(SellStatus::WalletFailure(address, "Private key not found".to_string()));
            }
        }
    }

    fn trigger_mass_sell_reverse(&mut self) {
        info!("Triggered mass sell reverse."); // This is the existing console log
        self.sell_mass_reverse_in_progress = true;
        let mint_address = self.sell_mint.clone();
        let slippage_percent = self.sell_slippage_percent;
        let priority_fee_lamports = self.sell_priority_fee_lamports;
        let status_sender = self.sell_status_sender.clone();
        let mut wallet_data: Vec<(String, String)> = self.loaded_wallet_data.iter().map(|w| (w.public_key.clone(), w.private_key.clone())).collect();
        wallet_data.reverse();

        tokio::spawn(async move {
            info!("Mass Sell Reverse Task: Started.");
            info!("Mass Sell Reverse Task: Sending SellStatus::InProgress - Starting mass sell");
            let send_result_start = status_sender.send(SellStatus::InProgress("Starting mass sell in reverse order".to_string()));
            info!("Mass Sell Reverse Task: Sent SellStatus::InProgress - Starting mass sell. Result: {:?}", send_result_start.is_ok());

            let mut success_count = 0; let mut failure_count = 0;
            for (wallet_address, private_key) in wallet_data {
                info!("Mass Sell Reverse Task: Processing wallet {}", wallet_address);
                info!("Mass Sell Reverse Task: Sending SellStatus::InProgress - Attempting to sell from wallet {}", wallet_address);
                let send_result_attempt = status_sender.send(SellStatus::InProgress(format!("Attempting to sell from wallet {}", wallet_address)));
                info!("Mass Sell Reverse Task: Sent SellStatus::InProgress - Attempting to sell. Result: {:?}", send_result_attempt.is_ok());

                let keypair_bytes = match bs58::decode(&private_key).into_vec() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        info!("Mass Sell Reverse Task: Wallet {} - Error decoding PK: {}. Sending WalletFailure.", wallet_address, e);
                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Error decoding PK: {}", e)));
                        info!("Mass Sell Reverse Task: Wallet {} - Sent WalletFailure for PK decode error.", wallet_address);
                        failure_count += 1;
                        continue;
                    }
                };
                info!("Mass Sell Reverse Task: Wallet {} - Decoded private key.", wallet_address);

                let wallet = match solana_sdk::signature::Keypair::from_bytes(&keypair_bytes) {
                    Ok(kp) => kp,
                    Err(e) => {
                        info!("Mass Sell Reverse Task: Wallet {} - Error creating keypair: {}. Sending WalletFailure.", wallet_address, e);
                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Error creating keypair: {}", e)));
                        info!("Mass Sell Reverse Task: Wallet {} - Sent WalletFailure for keypair creation error.", wallet_address);
                        failure_count += 1;
                        continue;
                    }
                };
                info!("Mass Sell Reverse Task: Wallet {} - Created keypair.", wallet_address);

                let mint_pubkey = match solana_sdk::pubkey::Pubkey::from_str(&mint_address) {
                    Ok(pk) => pk,
                    Err(e) => {
                        info!("Mass Sell Reverse Task: Wallet {} - Invalid mint address: {}. Sending WalletFailure.", wallet_address, e);
                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Invalid mint address: {}", e)));
                        info!("Mass Sell Reverse Task: Wallet {} - Sent WalletFailure for invalid mint address.", wallet_address);
                        failure_count += 1;
                        continue;
                    }
                };
                info!("Mass Sell Reverse Task: Wallet {} - Parsed mint_pubkey.", wallet_address);

                info!("Mass Sell Reverse Task: Wallet {} - Calling api::pumpfun::sell_token.", wallet_address);
                match api::pumpfun::sell_token(&wallet, &mint_pubkey, 100.0, slippage_percent as u8, priority_fee_lamports as f64 / solana_sdk::native_token::LAMPORTS_PER_SOL as f64).await {
                    Ok(versioned_transaction) => {
                        info!("Mass Sell Reverse Task: Wallet {} - pumpfun::sell_token OK. Signing and sending tx.", wallet_address);
                        let rpc_client_mass_sell = AsyncRpcClient::new(config::get_rpc_url());
                        match utils::transaction::sign_and_send_versioned_transaction(&rpc_client_mass_sell, versioned_transaction, &[&wallet]).await {
                            Ok(signature) => {
                                info!("Mass Sell Reverse Task: Wallet {} - Sell transaction successful. Sig: {}. Sending WalletSuccess.", wallet_address, signature);
                                let _ = status_sender.send(SellStatus::WalletSuccess(wallet_address.clone(), signature.to_string()));
                                info!("Mass Sell Reverse Task: Wallet {} - Sent WalletSuccess.", wallet_address);
                                success_count += 1;
                            }
                            Err(e) => {
                                info!("Mass Sell Reverse Task: Wallet {} - Sell transaction failed: {}. Sending WalletFailure.", wallet_address, e);
                                let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Sell transaction failed: {}", e)));
                                info!("Mass Sell Reverse Task: Wallet {} - Sent WalletFailure for tx failure.", wallet_address);
                                failure_count += 1;
                            }
                        }
                    }
                    Err(e) => {
                        info!("Mass Sell Reverse Task: Wallet {} - Failed to create sell transaction (pumpfun::sell_token error): {}. Sending WalletFailure.", wallet_address, e);
                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Failed to create sell transaction: {}", e)));
                        info!("Mass Sell Reverse Task: Wallet {} - Sent WalletFailure for pumpfun error.", wallet_address);
                        failure_count += 1;
                    }
                }
                info!("Mass Sell Reverse Task: Wallet {} - Sleeping for 500ms.", wallet_address);
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await; // Avoid rate limiting
            }
            info!("Mass Sell Reverse Task: Loop finished. Successful: {}, Failed: {}. Sending MassSellComplete.", success_count, failure_count);
            let send_result_complete = status_sender.send(SellStatus::MassSellComplete(format!("Mass sell reverse order complete. Successful: {}, Failed: {}", success_count, failure_count)));
            info!("Mass Sell Reverse Task: Sent MassSellComplete. Result: {:?}. Task Finished.", send_result_complete.is_ok());
        });
    }

    fn trigger_gather_and_sell(&mut self) {
        self.sell_gather_and_sell_in_progress = true;
        if self.loaded_wallet_data.is_empty() {
            let _ = self.sell_status_sender.send(SellStatus::Failure("No wallets loaded".to_string()));
            self.sell_gather_and_sell_in_progress = false; return;
        }
        let primary_wallet_info = self.loaded_wallet_data[0].clone(); // Assuming first is primary
        let mint_address = self.sell_mint.clone();
        let slippage_percent = self.sell_slippage_percent;
        let priority_fee_lamports = self.sell_priority_fee_lamports;
        let status_sender = self.sell_status_sender.clone();
        let all_wallet_data = self.loaded_wallet_data.clone();

        tokio::spawn(async move {
            let _ = status_sender.send(SellStatus::InProgress("Starting gather and sell operation".to_string()));
            let mint_pubkey = match Pubkey::from_str(&mint_address) { Ok(pk) => pk, Err(e) => { let _ = status_sender.send(SellStatus::Failure(format!("Invalid mint: {}", e))); return; } };
            let primary_keypair_bytes = match bs58::decode(&primary_wallet_info.private_key).into_vec() { Ok(b) => b, Err(e) => {let _ = status_sender.send(SellStatus::Failure(format!("Decode primary PK fail: {}",e))); return;}};
            let primary_keypair = match Keypair::from_bytes(&primary_keypair_bytes) { Ok(kp) => kp, Err(e) => {let _ = status_sender.send(SellStatus::Failure(format!("Primary KP fail: {}",e))); return;}};

            let _ = status_sender.send(SellStatus::InProgress("Gathering tokens to primary wallet...".to_string()));
            let mut gather_success_count = 0; let mut gather_failure_count = 0;
            let rpc_client_gather = AsyncRpcClient::new(config::get_rpc_url());

            for wallet_info_iter in all_wallet_data.iter().skip(1) { // Skip primary for gathering
                let wallet_address_iter = wallet_info_iter.public_key.clone();
                let _ = status_sender.send(SellStatus::InProgress(format!("Attempting to gather from {}", wallet_address_iter)));
                let sender_kp_bytes = match bs58::decode(&wallet_info_iter.private_key).into_vec() {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address_iter, format!("Gather: Error decoding PK: {}", e)));
                        gather_failure_count += 1;
                        continue;
                    }
                };
                let sender_kp = match Keypair::from_bytes(&sender_kp_bytes) {
                    Ok(kp) => kp,
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address_iter, format!("Gather: Error creating keypair: {}", e)));
                        gather_failure_count += 1;
                        continue;
                    }
                };
                let sender_token_account = get_associated_token_address(&sender_kp.pubkey(), &mint_pubkey);
                let recipient_token_account = get_associated_token_address(&primary_keypair.pubkey(), &mint_pubkey);

                match rpc_client_gather.get_token_account_balance(&sender_token_account).await {
                    Ok(token_amount_resp) => {
                        match token_amount_resp.amount.parse::<u64>() {
                            Ok(amount) => {
                                if amount > 0 {
                                    match spl_token::instruction::transfer(&spl_token::ID, &sender_token_account, &recipient_token_account, &sender_kp.pubkey(), &[], amount) {
                                        Ok(transfer_ix) => {
                                            let compute_budget_ix = compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports);
                                            match rpc_client_gather.get_latest_blockhash().await {
                                                Ok(recent_blockhash) => {
                                                    let tx = Transaction::new_signed_with_payer(&[compute_budget_ix, transfer_ix], Some(&sender_kp.pubkey()), &[&sender_kp], recent_blockhash);
                                                    match rpc_client_gather.send_and_confirm_transaction(&tx).await {
                                                        Ok(sig) => {
                                                            let _ = status_sender.send(SellStatus::WalletSuccess(wallet_address_iter.clone(), format!("Gather successful. Sig: {}", sig)));
                                                            gather_success_count += 1;
                                                        }
                                                        Err(e) => {
                                                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address_iter.clone(), format!("Gather transaction failed: {}", e)));
                                                            gather_failure_count += 1;
                                                        }
                                                    }
                                                }
                                                Err(_) => { // Error from get_latest_blockhash
                                                    let _ = status_sender.send(SellStatus::WalletFailure(wallet_address_iter.clone(), "Gather: Failed to get blockhash.".to_string()));
                                                    gather_failure_count += 1;
                                                }
                                            }
                                        }
                                        Err(_) => { // Error from spl_token::instruction::transfer
                                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address_iter.clone(), "Gather: Failed to create transfer instruction.".to_string()));
                                            gather_failure_count += 1;
                                        }
                                    }
                                } else { // amount is 0 or less
                                     let _ = status_sender.send(SellStatus::InProgress(format!("Gather: Wallet {} has no tokens to gather.", wallet_address_iter)));
                                }
                            }
                            Err(_) => { // Error from token_amount_resp.amount.parse
                                let _ = status_sender.send(SellStatus::WalletFailure(wallet_address_iter.clone(), "Gather: Failed to parse token balance.".to_string()));
                                gather_failure_count += 1;
                            }
                        }
                    }
                    Err(_) => { // Error from rpc_client_gather.get_token_account_balance
                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address_iter.clone(), "Gather: Failed to get token account balance.".to_string()));
                        gather_failure_count += 1;
                    }
                }
                sleep(Duration::from_millis(200)).await; // Small delay to avoid rate limiting
            }

            let _ = status_sender.send(SellStatus::InProgress(format!("Gathering complete. Attempting to sell all tokens from primary wallet {}...", primary_wallet_info.public_key)));
            // Simplified dynamic fee calculation for gather & sell
            // let dynamic_fee_lamports_gather_sell: u64 = 0; // Placeholder

            match api::pumpfun::sell_token(&primary_keypair, &mint_pubkey, 100.0, slippage_percent as u8, priority_fee_lamports as f64 / sol_to_lamports(1.0) as f64).await {
                Ok(vt) => {
                    let rpc_client_sell = AsyncRpcClient::new(config::get_rpc_url());
                    match utils::transaction::sign_and_send_versioned_transaction(&rpc_client_sell, vt, &[&primary_keypair]).await {
                        Ok(sig) => {
                            let _ = status_sender.send(SellStatus::WalletSuccess(primary_wallet_info.public_key.clone(), format!("Primary sell successful. Sig: {}", sig)));
                            let _ = status_sender.send(SellStatus::GatherAndSellComplete(format!("Gather & Sell complete. Gathered from {} wallets ({} success, {} fail). Primary wallet {} sold all tokens.", all_wallet_data.len() -1, gather_success_count, gather_failure_count, primary_wallet_info.public_key)));
                        }
                        Err(e) => {
                            let _ = status_sender.send(SellStatus::WalletFailure(primary_wallet_info.public_key.clone(), format!("Primary sell transaction failed: {}",e)));
                            let _ = status_sender.send(SellStatus::GatherAndSellComplete(format!("Gather & Sell complete. Gathered from {} wallets ({} success, {} fail). Primary wallet {} FAILED to sell: {}",all_wallet_data.len() -1, gather_success_count, gather_failure_count, primary_wallet_info.public_key, e)));
                        }
                    }
                }
                Err(e) => {
                    let _ = status_sender.send(SellStatus::WalletFailure(primary_wallet_info.public_key.clone(), format!("Failed to create primary sell transaction: {}",e)));
                    let _ = status_sender.send(SellStatus::GatherAndSellComplete(format!("Gather & Sell complete. Gathered from {} wallets ({} success, {} fail). Create primary sell FAILED: {}",all_wallet_data.len() -1, gather_success_count, gather_failure_count, e)));
                }
            }
        });
    }

    fn show_disperse_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("💨 Disperse SOL")
                            .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                            .color(AURORA_TEXT_PRIMARY),
                    );
                });
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(15.0);

                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| {
                    ui_centered.set_max_width(1400.0); // Max width for content

                    // --- Information Panel ---
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.vertical_centered_justified(|ui_info| {
                            ui_info.label(electric_label_text("Distribute SOL from your Parent Wallet to selected trading/bot wallets.").italics());
                        });
                        ui_frame.separator();
                        let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                        if let Some(parent_pk_str) = parent_pk_str_opt.as_ref() {
                            ui_frame.horizontal_wrapped(|ui_parent_info| {
                                ui_parent_info.label(electric_label_text("Parent Wallet (Source):"));
                                ui_parent_info.monospace(egui::RichText::new(parent_pk_str).color(AURORA_ACCENT_TEAL).strong());
                                if let Some(wallet) = self.wallets.iter().find(|w| w.address == *parent_pk_str && w.is_parent) {
                                    if wallet.is_loading {
                                        ui_parent_info.add(egui::Spinner::new().size(16.0));
                                    } else if let Some(balance) = wallet.sol_balance {
                                        ui_parent_info.label(electric_label_text(&format!("Balance: ◎ {:.4}", balance)));
                                    } else {
                                        ui_parent_info.label(electric_label_text("Balance: N/A"));
                                    }
                                } else {
                                     ui_parent_info.label(electric_label_text("Balance: (Refresh needed)"));
                                }
                            });
                        } else {
                            ui_frame.label(electric_label_text("Parent Wallet: Not set in Settings.").color(AURORA_ACCENT_MAGENTA));
                        }
                    });
                    ui_centered.add_space(20.0);

                    // --- Configuration Section ---
                    show_section_header(ui_centered, "💸 Amount & Targets", AURORA_ACCENT_PURPLE);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.horizontal(|ui_cfg| {
                            ui_cfg.label(electric_label_text("SOL Amount per Target Wallet:"));
                            ui_cfg.add(egui::DragValue::new(&mut self.disperse_sol_amount)
                                .speed(0.001)
                                .clamp_range(0.0..=1000.0) // Use clamp_range for DragValue
                                .max_decimals(9)
                                .prefix("◎ "));
                        });
                        ui_frame.separator();
                        ui_frame.horizontal(|ui_select_btns| {
                            if create_electric_button(ui_select_btns, "Select All Trading/Bots", None).clicked() {
                                let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                for wallet in self.wallets.iter_mut() {
                                    if Some(&wallet.address) != parent_pk_str_opt.as_ref() { // Don't select parent
                                        wallet.is_selected = true;
                                    }
                                }
                            }
                            if create_electric_button(ui_select_btns, "Deselect All", None).clicked() {
                                for wallet in self.wallets.iter_mut() {
                                    wallet.is_selected = false;
                                }
                            }
                        });

                        // --- Wallet Selection Table ---
                        use egui_extras::{TableBuilder, Column};
                        egui::ScrollArea::vertical()
                            .id_source("disperse_wallet_scroll_modern") // Unique ID
                            .max_height(500.0) // Increased height
                            .auto_shrink([false, false])
                            .show(ui_frame, |ui_scroll| {
                                TableBuilder::new(ui_scroll)
                                    .striped(true)
                                    .resizable(true)
                                    .column(Column::initial(60.0).at_least(50.0).clip(true))  // Select
                                    .column(Column::initial(80.0).at_least(60.0).clip(true))  // Type
                                    .column(Column::remainder().at_least(200.0).clip(true))    // Address
                                    .column(Column::initial(120.0).at_least(100.0).clip(true)) // Current SOL
                                    .header(20.0, |mut header| {
                                        header.col(|ui| { ui.strong("Select"); });
                                        header.col(|ui| { ui.strong("Type"); });
                                        header.col(|ui| { ui.strong("Address"); });
                                        header.col(|ui| { ui.strong("Current SOL"); });
                                    })
                                    .body(|mut body| {
                                        let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                        let mut trading_wallet_counter = 0;

                                        for wallet_info in self.wallets.iter_mut() {
                                            if Some(&wallet_info.address) == parent_pk_str_opt.as_ref() {
                                                continue; // Skip parent wallet from selection list
                                            }
                                            body.row(18.0, |mut row| {
                                                row.col(|ui| {
                                                    ui.checkbox(&mut wallet_info.is_selected, "");
                                                });

                                                row.col(|ui| {
                                                    let type_label_text = if wallet_info.is_dev_wallet {
                                                        egui::RichText::new("Dev").color(AURORA_ACCENT_PURPLE)
                                                    } else if wallet_info.is_primary {
                                                        egui::RichText::new("Primary").color(AURORA_ACCENT_NEON_BLUE)
                                                    } else {
                                                        trading_wallet_counter += 1;
                                                        egui::RichText::new(format!("Bot {}", trading_wallet_counter)).color(AURORA_TEXT_PRIMARY)
                                                    };
                                                    ui.label(type_label_text.font(FontId::proportional(14.0)));
                                                });

                                                row.col(|ui| {
                                                    ui.label(egui::RichText::new(&wallet_info.address).font(FontId::monospace(14.0)).color(AURORA_TEXT_PRIMARY)).on_hover_text(&wallet_info.address);
                                                });

                                                row.col(|ui_cell| {
                                                    if wallet_info.is_loading {
                                                        ui_cell.add(egui::Spinner::new().size(16.0));
                                                    } else {
                                                        ui_cell.label(egui::RichText::new(wallet_info.sol_balance.map_or_else(|| "-".to_string(), |b| format!("◎ {:.4}", b))).color(AURORA_TEXT_PRIMARY).font(FontId::monospace(14.0)));
                                                        if let Some(err_msg) = &wallet_info.error {
                                                            ui_cell.label(egui::RichText::new("⚠️").color(AURORA_ACCENT_MAGENTA)).on_hover_text(err_msg);
                                                        }
                                                    }
                                                });
                                            });
                                        }
                                    });
                            });
                    }); // End Configuration and Wallet Selection Frame
                    ui_centered.add_space(20.0);

                    // --- Action Button Section ---
                    ui_centered.vertical_centered_justified(|ui_action_vertical| {
                        ui_action_vertical.horizontal(|ui_action_horizontal| {
                            let app_settings_key_clone = self.app_settings.parent_wallet_private_key.clone();
                            // Safely parse parent keypair
                            let parent_keypair: Option<Keypair> = if !app_settings_key_clone.is_empty() {
                                bs58::decode(&app_settings_key_clone).into_vec().ok()
                                    .and_then(|bytes| Keypair::from_bytes(&bytes).ok())
                            } else {
                                None
                            };

                            let selected_wallets_count = self.wallets.iter().filter(|w| w.is_selected && Some(&w.address) != AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key).as_ref() ).count();

                            let can_disperse = parent_keypair.is_some() &&
                                               self.disperse_sol_amount > 0.0 &&
                                               selected_wallets_count > 0 &&
                                               !self.disperse_in_progress && !self.fetch_balances_in_progress;

                            let button_text_str = format!("Disperse ◎{} to {} Wallets", self.disperse_sol_amount, selected_wallets_count);

                            let disperse_button = egui::Button::new(
                                    egui::RichText::new(&button_text_str)
                                        .color(AURORA_TEXT_PRIMARY)
                                        .font(FontId::new(16.0, egui::FontFamily::Proportional))
                                )
                                .min_size(egui::vec2(220.0, 35.0)) // Example size, adjust as needed or use helper's default
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.0, AURORA_ACCENT_NEON_BLUE))
                                .rounding(Rounding::same(4.0));

                            if ui_action_horizontal.add_enabled(can_disperse, disperse_button)
                                .on_hover_text("Ensure Parent Key is valid, amount > 0, and wallets are selected.")
                                .clicked() {
                                if let Some(parent_kp) = parent_keypair {
                                    let targets: Vec<String> = self.wallets.iter()
                                        .filter(|w| w.is_selected && Some(&w.address) != AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key).as_ref())
                                        .map(|w| w.address.clone())
                                        .collect();

                                    if targets.is_empty() {
                                        self.last_operation_result = Some(Err("No target wallets selected for dispersal.".to_string()));
                                    } else {
                                        self.last_operation_result = Some(Ok(format!("Initiating disperse of ◎{} to {} wallets...", self.disperse_sol_amount, targets.len())));
                                        self.disperse_in_progress = true;
                                        let rpc_url = self.app_settings.solana_rpc_url.clone();
                                        let sender_channel = self.disperse_result_sender.clone();
                                        let amount_lamports = sol_to_lamports(self.disperse_sol_amount);

                                        tokio::spawn(disperse_sol_task(rpc_url, parent_kp, targets, amount_lamports, sender_channel));
                                    }
                                } else {
                                    self.last_operation_result = Some(Err("Parent wallet private key is invalid or not set.".to_string()));
                                }
                            }

                            ui_action_horizontal.add_space(10.0);

                            let refresh_button_widget = egui::Button::new(
                                    egui::RichText::new("🔄 Refresh Balances")
                                        .color(AURORA_TEXT_PRIMARY)
                                        .font(FontId::new(16.0, egui::FontFamily::Proportional))
                                )
                                .min_size(egui::vec2(200.0, 35.0)) // Consistent size
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.0, AURORA_ACCENT_TEAL)) // Consistent styling
                                .rounding(Rounding::same(4.0));

                            if ui_action_horizontal.add_enabled(!self.fetch_balances_in_progress && !self.disperse_in_progress, refresh_button_widget)
                                .on_hover_text("Refresh SOL and token balances for all wallets.")
                                .clicked() {
                                self.trigger_balance_fetch(None);
                                self.last_operation_result = Some(Ok("Balance refresh triggered for all wallets.".to_string()));
                            }
                        });

                        if self.disperse_in_progress || self.fetch_balances_in_progress {
                            ui_action_vertical.add_space(5.0);
                            ui_action_vertical.add(egui::Spinner::new().size(24.0));
                        }
                    });
                     // Display last operation result
                    if let Some(result) = &self.last_operation_result {
                        ui_centered.add_space(10.0);
                        match result {
                            Ok(msg) => ui_centered.label(egui::RichText::new(format!("✅ {}", msg)).color(AURORA_ACCENT_TEAL)),
                            Err(e) => ui_centered.label(egui::RichText::new(format!("❌ {}", e)).color(AURORA_ACCENT_MAGENTA)),
                        };
                    }
                    ui_centered.add_space(20.0); // Bottom padding
                }); // End Centered Layout
            }); // End Outer Frame
    }

    fn show_gather_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("📥 Gather SOL to Parent")
                            .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                            .color(AURORA_TEXT_PRIMARY),
                    );
                });
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(15.0);

                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| {
                    ui_centered.set_max_width(1400.0); // Max width for content

                    // --- Information Panel ---
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.vertical_centered_justified(|ui_info| {
                            ui_info.label(electric_label_text("Consolidate SOL from selected wallets back to your Parent Wallet.").italics());
                        });
                        ui_frame.separator();
                        let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                        if let Some(parent_pk_str) = parent_pk_str_opt.as_ref() {
                            ui_frame.horizontal_wrapped(|ui_parent_info| {
                                ui_parent_info.label(electric_label_text("Parent Wallet (Recipient):"));
                                ui_parent_info.monospace(egui::RichText::new(parent_pk_str).color(AURORA_ACCENT_TEAL).strong());
                                if let Some(wallet) = self.wallets.iter().find(|w| w.address == *parent_pk_str && w.is_parent) {
                                    if wallet.is_loading {
                                        ui_parent_info.add(egui::Spinner::new().size(16.0));
                                    } else if let Some(balance) = wallet.sol_balance {
                                        ui_parent_info.label(electric_label_text(&format!("Current Balance: ◎ {:.4}", balance)));
                                    } else {
                                        ui_parent_info.label(electric_label_text("Current Balance: N/A"));
                                    }
                                } else {
                                     ui_parent_info.label(electric_label_text("Current Balance: (Refresh needed)"));
                                }
                            });
                        } else {
                            ui_frame.label(electric_label_text("Parent Wallet: Not set in Settings. Gathering not possible.").color(AURORA_ACCENT_MAGENTA));
                        }
                    });
                    ui_centered.add_space(20.0);

                    // --- Wallet Selection Section ---
                    show_section_header(ui_centered, "🎯 Select Source Wallets", AURORA_ACCENT_PURPLE);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.horizontal(|ui_select_btns| {
                            if create_electric_button(ui_select_btns, "Select All Trading/Bots", None).clicked() {
                                let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                for wallet in self.wallets.iter_mut() {
                                    // Only select if not parent and has some SOL balance (or is loading, implying it might have SOL)
                                    if Some(&wallet.address) != parent_pk_str_opt.as_ref() && (wallet.sol_balance.unwrap_or(0.0) > 0.0 || wallet.is_loading) {
                                        wallet.is_selected = true;
                                    }
                                }
                            }
                            if create_electric_button(ui_select_btns, "Deselect All", None).clicked() {
                                for wallet in self.wallets.iter_mut() {
                                    wallet.is_selected = false;
                                }
                            }
                        });
                        ui_frame.separator();

                        use egui_extras::{TableBuilder, Column};
                        egui::ScrollArea::vertical()
                            .id_source("gather_wallet_scroll")
                            .max_height(500.0) // Increased height
                            .auto_shrink([false, false])
                            .show(ui_frame, |ui_scroll| {
                                TableBuilder::new(ui_scroll)
                                    .striped(true)
                                    .resizable(true)
                                    .column(Column::initial(60.0).at_least(50.0).clip(true))  // Gather?
                                    .column(Column::initial(80.0).at_least(60.0).clip(true))  // Type
                                    .column(Column::remainder().at_least(200.0).clip(true))    // Address
                                    .column(Column::initial(120.0).at_least(100.0).clip(true)) // Current SOL
                                    .header(20.0, |mut header| {
                                        header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.label(egui::RichText::new("Gather?").strong().color(AURORA_TEXT_PRIMARY)); });
                                        header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.label(egui::RichText::new("Type").strong().color(AURORA_TEXT_PRIMARY)); });
                                        header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.label(egui::RichText::new("Address").strong().color(AURORA_TEXT_PRIMARY)); });
                                        header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_aligned| ui_aligned.label(egui::RichText::new("Current SOL").strong().color(AURORA_TEXT_PRIMARY)));});
                                    })
                                    .body(|mut body| {
                                        let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                        let mut trading_wallet_counter = 0;

                                        for wallet_info in self.wallets.iter_mut() {
                                            body.row(20.0, |mut row| {
                                                if Some(&wallet_info.address) == parent_pk_str_opt.as_ref() {
                                                    // Display parent as non-selectable recipient for context
                                                    row.col(|ui| { ui.label(""); }); // No checkbox for parent
                                                    row.col(|ui| { ui.label(egui::RichText::new("Parent (Recipient)").color(AURORA_ACCENT_TEAL).font(FontId::proportional(14.5))); });
                                                    row.col(|ui| { ui.label(egui::RichText::new(&wallet_info.address).font(FontId::monospace(14.0)).color(AURORA_TEXT_PRIMARY)); });
                                                    row.col(|ui_cell| { ui_cell.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_cell_inner| { ui_cell_inner.label(egui::RichText::new(wallet_info.sol_balance.map_or_else(|| "N/A".to_string(), |b| format!("◎ {:.4}", b))).color(AURORA_TEXT_PRIMARY).font(FontId::monospace(14.0))); }); });
                                                } else {
                                                    // Only allow selection if wallet has some SOL or is loading (might have SOL)
                                                    let can_be_selected = wallet_info.sol_balance.unwrap_or(0.0) > 0.000005 || wallet_info.is_loading; // Keep a tiny threshold for fees
                                                    row.col(|ui| { ui.add_enabled(can_be_selected, egui::Checkbox::new(&mut wallet_info.is_selected, "")); });

                                                    row.col(|ui| {
                                                        let type_label_text = if wallet_info.is_dev_wallet {
                                                            egui::RichText::new("Dev").color(AURORA_ACCENT_PURPLE)
                                                        } else if wallet_info.is_primary {
                                                            egui::RichText::new("Primary").color(AURORA_ACCENT_NEON_BLUE)
                                                        } else {
                                                            trading_wallet_counter += 1;
                                                            egui::RichText::new(format!("Bot {}", trading_wallet_counter)).color(AURORA_TEXT_PRIMARY)
                                                        };
                                                        ui.label(type_label_text.font(FontId::proportional(14.5)));
                                                    });

                                                    row.col(|ui| {
                                                        ui.label(egui::RichText::new(&wallet_info.address).font(FontId::monospace(14.0)).color(AURORA_TEXT_PRIMARY)).on_hover_text(&wallet_info.address);
                                                    });

                                                    row.col(|ui_cell| { // Current SOL column
                                                        ui_cell.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_cell_inner| {
                                                            if let Some(err_msg) = &wallet_info.error {
                                                                ui_cell_inner.label(egui::RichText::new("⚠️").color(AURORA_ACCENT_MAGENTA)).on_hover_text(err_msg);
                                                                ui_cell_inner.add_space(4.0);
                                                            }
                                                            if wallet_info.is_loading {
                                                                ui_cell_inner.add(egui::Spinner::new().size(15.0));
                                                            } else {
                                                                match &wallet_info.sol_balance {
                                                                    Some(balance) => { ui_cell_inner.label(egui::RichText::new(format!("◎ {:.4}", balance)).color(AURORA_TEXT_PRIMARY).font(FontId::monospace(14.0))); }
                                                                    None => { ui_cell_inner.label(egui::RichText::new("-").color(AURORA_TEXT_MUTED_TEAL).font(FontId::monospace(14.0))); }
                                                                }
                                                            }
                                                        });
                                                    });
                                                }
                                            });
                                        }
                                    });
                            });
                    }); // End Configuration and Wallet Selection Frame
                    ui_centered.add_space(20.0);

                    // --- Action Button Section ---
                    ui_centered.vertical_centered_justified(|ui_action_vertical| {
                        ui_action_vertical.horizontal(|ui_action_horizontal| {
                            let parent_key_is_set = !self.app_settings.parent_wallet_private_key.is_empty();
                            let selected_source_wallets_count = self.wallets.iter()
                                .filter(|w| w.is_selected && Some(&w.address) != AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key).as_ref())
                                .count();

                            let can_gather = parent_key_is_set && selected_source_wallets_count > 0 && !self.gather_in_progress && !self.fetch_balances_in_progress;

                            let button_text = format!("Gather SOL from {} Wallets", selected_source_wallets_count);

                            let gather_button_widget = egui::Button::new(
                                    egui::RichText::new(&button_text)
                                        .color(AURORA_TEXT_PRIMARY)
                                        .font(FontId::new(16.0, egui::FontFamily::Proportional))
                                )
                                .min_size(egui::vec2(220.0, 35.0))
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.0, AURORA_ACCENT_NEON_BLUE))
                                .rounding(Rounding::same(4.0));

                            if ui_action_horizontal.add_enabled(can_gather, gather_button_widget)
                                .on_hover_text("Ensure Parent Key is set and source wallets are selected.")
                                .clicked() {

                                let source_wallets_info: Vec<LoadedWalletInfo> = self.wallets.iter()
                                    .filter(|w| w.is_selected && Some(&w.address) != AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key).as_ref())
                                    .filter_map(|w_info| self.loaded_wallet_data.iter().find(|lw| lw.public_key == w_info.address).cloned())
                                    .collect();

                                if source_wallets_info.is_empty() {
                                    self.last_operation_result = Some(Err("No valid source wallets selected or found in loaded data.".to_string()));
                                } else {
                                    self.last_operation_result = Some(Ok(format!("Initiating SOL gather from {} wallets...", source_wallets_info.len())));
                                    self.gather_in_progress = true;
                                    self.gather_tasks_expected = source_wallets_info.len();
                                    self.gather_tasks_completed = 0;

                                    let rpc_url = self.app_settings.solana_rpc_url.clone();
                                    let parent_pk_str = self.app_settings.parent_wallet_private_key.clone(); // For the task
                                    let gather_sender = self.gather_result_sender.clone();

                                    let delay_between_gathers = Duration::from_millis(500);
                                    let wallets_to_gather_from = source_wallets_info.clone();

                                    if let Some(parent_pubkey_str_from_settings) = AppSettings::get_pubkey_from_privkey_str(&parent_pk_str) {
                                        match Pubkey::from_str(&parent_pubkey_str_from_settings) {
                                            Ok(recipient_pubkey) => {
                                                tokio::spawn(async move {
                                                    log::info!("Starting sequential gather for {} selected wallets with delay: {:?}", wallets_to_gather_from.len(), delay_between_gathers);
                                                    for wallet_data in wallets_to_gather_from {
                                                        let rpc_c = rpc_url.clone();
                                                        let sender_c = gather_sender.clone();
                                                        gather_sol_from_zombie_task(rpc_c, wallet_data, recipient_pubkey, sender_c).await;
                                                        sleep(delay_between_gathers).await;
                                                    }
                                                    log::info!("Sequential gather stream processing for selected wallets completed.");
                                                });
                                            }
                                            Err(e) => {
                                                let err_msg = format!("Failed to parse parent pubkey '{}' for gather: {}", parent_pubkey_str_from_settings, e);
                                                log::error!("{}", err_msg);
                                                let _ = gather_sender.send(Err(err_msg));
                                                self.gather_in_progress = false;
                                            }
                                        }
                                    } else {
                                        let err_msg = "Parent private key from settings is invalid or could not be derived into a public key.".to_string();
                                        log::error!("{}", err_msg);
                                        let _ = gather_sender.send(Err(err_msg));
                                        self.gather_in_progress = false;
                                    }
                                }
                            }

                            ui_action_horizontal.add_space(10.0);

                            let refresh_button_widget = egui::Button::new(
                                    egui::RichText::new("🔄 Refresh Balances")
                                        .color(AURORA_TEXT_PRIMARY)
                                        .font(FontId::new(16.0, egui::FontFamily::Proportional))
                                )
                                .min_size(egui::vec2(200.0, 35.0))
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.0, AURORA_ACCENT_TEAL))
                                .rounding(Rounding::same(4.0));

                            if ui_action_horizontal.add_enabled(!self.fetch_balances_in_progress && !self.gather_in_progress, refresh_button_widget)
                                .on_hover_text("Refresh SOL and token balances for all wallets.")
                                .clicked() {
                                self.trigger_balance_fetch(None);
                                self.last_operation_result = Some(Ok("Balance refresh triggered for all wallets.".to_string()));
                            }
                        });

                        if self.gather_in_progress || self.fetch_balances_in_progress {
                            ui_action_vertical.add_space(5.0);
                            ui_action_vertical.add(egui::Spinner::new().size(24.0));
                        }
                    });
                     // Display last operation result
                    if let Some(result) = &self.last_operation_result {
                        ui_centered.add_space(10.0);
                        match result {
                            Ok(msg) => ui_centered.label(egui::RichText::new(format!("✅ {}", msg)).color(AURORA_ACCENT_TEAL)),
                            Err(e) => ui_centered.label(egui::RichText::new(format!("❌ {}", e)).color(AURORA_ACCENT_MAGENTA)),
                        };
                    }
                    ui_centered.add_space(20.0); // Bottom padding
                }); // End Centered Layout
            }); // End Outer Frame
    }
    // --- Launch View (Adapted from app.rs with Aurora Theme) ---
    fn show_launch_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("🚀 Coordinated Launch")
                            .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                            .color(AURORA_TEXT_PRIMARY),
                    );
                });
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(15.0);

                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| {
                    ui_centered.set_max_width(900.0);

                    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui_centered, |ui_scroll| {
                        create_electric_frame().show(ui_scroll, |ui_frame| {
                            ui_frame.label(electric_label_text("Create a new token and perform initial buys from parent and other wallets in a single (potentially Jito bundled) operation.").italics());
                            ui_frame.add_space(5.0);
                            ui_frame.label(electric_label_text("Ensure Dev Wallet (Minter) and Parent Wallet are set in Settings. Other wallets for buys are from the loaded keys file.").color(AURORA_ACCENT_TEAL));
                            ui_frame.add_space(8.0);
                            ui_frame.label(egui::RichText::new("⚠️ Important: If using an ALT, create/select it first in ALT Management. Ensure Jito settings are correct in App Settings if using Jito bundles.").strong().color(AURORA_ACCENT_MAGENTA));
                        });
                        ui_scroll.add_space(15.0);

                        // --- Token Details Section ---
                        show_section_header(ui_scroll, "📜 Token Details", AURORA_ACCENT_NEON_BLUE);
                        create_electric_frame().show(ui_scroll, |ui_frame| {
                            egui::Grid::new("launch_token_details_grid_modern")
                                .num_columns(2).min_col_width(120.0).spacing([20.0, 10.0])
                                .show(ui_frame, |ui_grid| {
                                    ui_grid.label(electric_label_text("Name:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.launch_token_name)
                                                    .desired_width(f32::INFINITY)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Symbol:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.launch_token_symbol)
                                                    .desired_width(f32::INFINITY)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Description:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::multiline(&mut self.launch_token_description)
                                                    .desired_width(f32::INFINITY)
                                                    .desired_rows(2)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Image URL:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::ZERO) // Frame will contain the horizontal layout
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.horizontal(|ui_img| {
                                                // Adjust inner margin for the TextEdit itself if needed, or rely on frame's padding
                                                let text_edit_frame = egui::Frame::none()
                                                    .inner_margin(egui::Margin::symmetric(4.0, 2.0)) // Padding for TextEdit
                                                    .show(ui_img, |text_edit_parent_ui| {
                                                        text_edit_parent_ui.add(
                                                            egui::TextEdit::singleline(&mut self.launch_token_image_url)
                                                                .hint_text("https://... or local path")
                                                                .desired_width(text_edit_parent_ui.available_width()) // Fill available width inside its own padding frame
                                                                .text_color(AURORA_TEXT_PRIMARY)
                                                                .frame(false)
                                                        );
                                                    });
                                                // The Browse button is outside the TextEdit's immediate padding frame but inside the cell_ui.horizontal
                                                if create_electric_button(ui_img, "Browse", None).clicked() {
                                                    if let Some(path) = FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg", "gif"]).pick_file() {
                                                        self.launch_token_image_url = path.to_string_lossy().to_string();
                                                    }
                                                }
                                            });
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Website URL:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.launch_website)
                                                    .desired_width(f32::INFINITY)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Twitter URL:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.launch_twitter)
                                                    .hint_text("https://twitter.com/...")
                                                    .desired_width(f32::INFINITY)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Telegram URL:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.launch_telegram)
                                                    .hint_text("https://t.me/...")
                                                    .desired_width(f32::INFINITY)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();
                                });
                        });
                        ui_scroll.add_space(15.0);

                        // --- Launch Parameters Section ---
                        show_section_header(ui_scroll, "⚙️ Launch Parameters", AURORA_ACCENT_PURPLE);
                        create_electric_frame().show(ui_scroll, |ui_frame| {
                            egui::Grid::new("launch_params_grid_modern")
                                .num_columns(2).min_col_width(120.0).spacing([20.0, 10.0])
                                .show(ui_frame, |ui_grid| {
                                    ui_grid.label(electric_label_text("Launch Platform:"));
                                    ui_grid.horizontal(|ui_platform_select| {
                                        ui_platform_select.selectable_value(
                                            &mut self.launch_platform,
                                            LaunchPlatform::PumpFun,
                                            LaunchPlatform::PumpFun.name()
                                        );
                                        ui_platform_select.add_space(10.0);
                                        ui_platform_select.selectable_value(
                                            &mut self.launch_platform,
                                            LaunchPlatform::BonkRaydium,
                                            LaunchPlatform::BonkRaydium.name()
                                        );
                                    });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Dev Wallet Buy (SOL):"));
                                    ui_grid.add(egui::DragValue::new(&mut self.launch_dev_buy_sol).speed(0.001).max_decimals(9).prefix("◎ "));
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Other Wallets Buy (SOL Each):"));
                                    ui_grid.add(egui::DragValue::new(&mut self.launch_zombie_buy_sol).speed(0.001).max_decimals(9).prefix("◎ "));
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Mint Keypair File:"));
                                    ui_grid.horizontal(|ui_kp| {
                                        egui::ComboBox::from_id_source("launch_mint_kp_combo_modern")
                                            .selected_text(if self.launch_mint_keypair_path.is_empty() { "Generate New" } else { self.launch_mint_keypair_path.as_str() })
                                            .show_ui(ui_kp, |ui_combo| {
                                                ui_combo.selectable_value(&mut self.launch_mint_keypair_path, "".to_string(), "Generate New");
                                                for kp_path in &self.available_mint_keypairs {
                                                    ui_combo.selectable_value(&mut self.launch_mint_keypair_path, kp_path.clone(), kp_path);
                                                }
                                            });
                                        if create_electric_button(ui_kp, "🔄", None).on_hover_text("Rescan for keypair files").clicked() {
                                            self.load_available_mint_keypairs();
                                            ui_kp.ctx().request_repaint(); // Request a repaint to update the ComboBox
                                        }
                                    });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Address Lookup Table (ALT):"));
                                    ui_grid.horizontal(|ui_alt|{
                                        egui::ComboBox::from_id_source("launch_alt_combo_modern")
                                            .selected_text(if self.launch_alt_address.is_empty() { "None (Not Recommended)" } else { self.launch_alt_address.as_str() })
                                            .show_ui(ui_alt, |ui_combo| {
                                                ui_combo.selectable_value(&mut self.launch_alt_address, "".to_string(), "None");
                                                for alt_addr_str in &self.available_alt_addresses {
                                                    ui_combo.selectable_value(&mut self.launch_alt_address, alt_addr_str.clone(), alt_addr_str);
                                                }
                                            });
                                        if create_electric_button(ui_alt, "🔄", None).on_hover_text("Reload alt_address.txt").clicked() {
                                            self.load_available_alts();
                                        }
                                    });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Jito Bundle:"));
                                    ui_grid.checkbox(&mut self.launch_use_jito_bundle, "Use Jito for launch bundle").on_hover_text("Requires Jito config in Settings");
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Simulate Only:"));
                                    ui_grid.checkbox(&mut self.launch_simulate_only, "Build transactions without sending");
                                    ui_grid.end_row();

                                    if self.app_settings.dev_wallet_private_key.trim().is_empty() {
                                        // This is a new row for the warning
                                        ui_grid.label(""); // Empty cell for alignment in the first column
                                        ui_grid.colored_label(
                                            AURORA_ACCENT_MAGENTA,
                                            egui::RichText::new("⚠️ Dev Wallet not set in Settings! Launch will fail.").strong(),
                                        );
                                        ui_grid.end_row();
                                    }
                                });
                        });
                        ui_scroll.add_space(20.0);
                    // --- Action Button & Logs ---
                    create_electric_frame().show(ui_scroll, |ui_frame| {
                        ui_frame.vertical_centered_justified(|ui_action_area| {
                            let dev_wallet_set = !self.app_settings.dev_wallet_private_key.trim().is_empty();
                            let launch_button_enabled = !self.launch_in_progress &&
                                                      !self.launch_token_name.trim().is_empty() &&
                                                      !self.launch_token_symbol.trim().is_empty() &&
                                                      self.launch_dev_buy_sol > 0.0 &&
                                                      dev_wallet_set; // Added Dev Wallet check

                            if !dev_wallet_set && !self.launch_token_name.trim().is_empty() && !self.launch_token_symbol.trim().is_empty() && self.launch_dev_buy_sol > 0.0 {
                                // This message appears if other conditions for launch are met, but dev wallet is the blocker.
                                ui_action_area.colored_label(AURORA_ACCENT_MAGENTA, "Button disabled: Dev Wallet must be set in App Settings.");
                                ui_action_area.add_space(5.0); // Add some space before the button
                            }

                            let launch_button = egui::Button::new(egui::RichText::new("🚀 Launch Token Now").color(AURORA_TEXT_PRIMARY))
                                .min_size(Vec2::new(200.0, 35.0))
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.0, AURORA_ACCENT_NEON_BLUE))
                                .rounding(Rounding::same(4.0));

                            if ui_action_area.add_enabled(launch_button_enabled, launch_button).clicked() {
                                self.launch_in_progress = true;
                                self.launch_log_messages.clear();
                                self.launch_log_messages.push("🚀 Launch Initiated...".to_string());
                                self.last_operation_result = Some(Ok("Launch initiated...".to_string()));

                                let params = LaunchParams {
                                    name: self.launch_token_name.trim().to_string(),
                                    symbol: self.launch_token_symbol.trim().to_string(),
                                    description: self.launch_token_description.trim().to_string(),
                                    image_url: self.launch_token_image_url.trim().to_string(),
                                    dev_buy_sol: self.launch_dev_buy_sol,
                                    zombie_buy_sol: self.launch_zombie_buy_sol,
                                    minter_private_key_str: self.app_settings.dev_wallet_private_key.clone(),
                                    slippage_bps: (self.app_settings.default_slippage_percent * 100.0) as u64, // Convert percent to bps
                                    priority_fee: self.app_settings.get_default_priority_fee_lamports(),
                                    alt_address_str: self.launch_alt_address.trim().to_string(),
                                    mint_keypair_path_str: self.launch_mint_keypair_path.trim().to_string(),
                                    // Filter out dev and parent wallets from the zombie list for launch_buy
                                    loaded_wallet_data: {
                                        let dev_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key);
                                        let parent_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);

                                        self.loaded_wallet_data.iter()
                                            .filter(|wallet_info| {
                                                let mut is_special_wallet = false;
                                                if let Some(ref dpk_str) = dev_pubkey_str_opt {
                                                    if wallet_info.public_key == *dpk_str {
                                                        is_special_wallet = true;
                                                    }
                                                }
                                                if !is_special_wallet { // Only check parent if not already identified as dev
                                                    if let Some(ref ppk_str) = parent_pubkey_str_opt {
                                                        if wallet_info.public_key == *ppk_str {
                                                            is_special_wallet = true;
                                                        }
                                                    }
                                                }
                                                !is_special_wallet // Keep if not a special (dev/parent) wallet
                                            })
                                            .cloned()
                                            .collect()
                                    },
                                    use_jito_bundle: self.launch_use_jito_bundle,
                                    jito_block_engine_url: self.app_settings.selected_jito_block_engine_url.clone(),
                                    twitter: self.launch_twitter.trim().to_string(),
                                    telegram: self.launch_telegram.trim().to_string(),
                                    website: self.launch_website.trim().to_string(),
                                    simulate_only: self.launch_simulate_only,
                                    main_tx_priority_fee_micro_lamports: self.launch_main_tx_priority_fee_micro_lamports, // Added
                                    jito_actual_tip_sol: self.launch_jito_actual_tip_sol,                 // Added
                                };
                                log::info!("Launch Parameters: {:?}", params);
                                self.launch_log_messages.push(format!("Launch Params: {:#?} for platform: {:?}", params, self.launch_platform)); // Also push to UI log (pretty print)
                                let status_sender_clone = self.launch_status_sender.clone();
                                let selected_platform = self.launch_platform; // Clone platform for use in async block

                                tokio::spawn(async move {
                                    let result = match selected_platform {
                                        LaunchPlatform::PumpFun => {
                                            crate::commands::launch_buy::launch_buy(
                                                params.name, params.symbol, params.description, params.image_url,
                                                params.dev_buy_sol, params.zombie_buy_sol, params.slippage_bps,
                                                params.alt_address_str,
                                                if params.mint_keypair_path_str.is_empty() { None } else { Some(params.mint_keypair_path_str.clone()) },
                                                params.minter_private_key_str, params.loaded_wallet_data,
                                                params.simulate_only, status_sender_clone.clone(),
                                                params.jito_block_engine_url,
                                                params.main_tx_priority_fee_micro_lamports,
                                                params.jito_actual_tip_sol,
                                            ).await
                                        }
                                        LaunchPlatform::BonkRaydium => {
                                            // Ensure the new command is correctly referenced
                                            crate::commands::bonk_launch::launch_on_raydium(
                                                params, // Pass the whole params struct
                                                status_sender_clone.clone(),
                                            ).await
                                        }
                                    };

                                    if let Err(e) = result {
                                        log::error!("Launch task finished with error for {:?}: {}", selected_platform, e);
                                        let _ = status_sender_clone.send(LaunchStatus::Failure(format!("Launch task error ({:?}): {}", selected_platform, e)));
                                    }
                                });
                            }

                            ui_action_area.add_space(15.0); // Space before Mix Wallets button

                            let mix_wallets_button_enabled = !self.mix_wallets_in_progress && !self.launch_in_progress && !self.wallets.is_empty();
                            let mix_button = egui::Button::new(egui::RichText::new("🌪️ Mix Wallets").color(AURORA_TEXT_PRIMARY))
                                .min_size(Vec2::new(200.0, 35.0))
                                .fill(AURORA_BG_PANEL) // Consistent styling
                                .stroke(Stroke::new(1.0, AURORA_ACCENT_PURPLE)); // Different accent

                            if ui_action_area.add_enabled(mix_wallets_button_enabled, mix_button)
                                .on_hover_text("Generate new wallets and transfer SOL from current wallets.")
                                .clicked() {
                                self.start_mix_wallets_request = Some(());
                                self.mix_wallets_log_messages.clear();
                                self.mix_wallets_log_messages.push("🌪️ Mix Wallets process initiated...".to_string());
                                self.last_operation_result = Some(Ok("Mix Wallets initiated.".to_string()));
                                // self.mix_wallets_in_progress will be set to true when the task is spawned
                            }

                            ui_action_area.add_space(5.0); // Space between buttons

                            // --- Retry Fund Mixed Wallets Button ---
                            let retry_button_enabled = self.last_mixed_wallets_file_path.is_some()
                                                        && !self.retry_zombie_to_mixed_funding_in_progress
                                                        && !self.mix_wallets_in_progress
                                                        && !self.launch_in_progress;
                            let retry_button_text = egui::RichText::new("🔁 Retry Fund Mixed Wallets")
                                .color(if retry_button_enabled { AURORA_TEXT_PRIMARY } else { Color32::DARK_GRAY });
                            let retry_button = egui::Button::new(retry_button_text)
                                .min_size(Vec2::new(200.0, 35.0))
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.0, AURORA_ACCENT_TEAL)); // Different accent

                            if ui_action_area.add_enabled(retry_button_enabled, retry_button)
                                .on_hover_text("Retry funding the last generated mixed wallets from available zombie wallets.")
                                .clicked() {
                                if let Some(mixed_wallets_path) = &self.last_mixed_wallets_file_path {
                                    self.retry_zombie_to_mixed_funding_in_progress = true;
                                    self.retry_zombie_to_mixed_funding_log_messages.clear();
                                    self.retry_zombie_to_mixed_funding_log_messages.push("🚀 Initiating Retry Funding task...".to_string());
                                    self.launch_log_messages.push("🔁 Retry Funding Mixed Wallets task initiated.".to_string());
                                    self.last_operation_result = Some(Ok("Retry Funding initiated.".to_string()));

                                    let task_rpc_url = self.app_settings.solana_rpc_url.clone();
                                    let task_mixed_wallets_path = mixed_wallets_path.clone();
                                    // Clone necessary data for the task
                                    let dev_pubkey_str = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key);
                                    let parent_pubkey_str = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);

                                    let task_zombie_wallets = self.loaded_wallet_data.iter()
                                        .filter(|w| {
                                            let is_dev = dev_pubkey_str.as_ref().map_or(false, |dpk| dpk == &w.public_key);
                                            let is_parent = parent_pubkey_str.as_ref().map_or(false, |ppk| ppk == &w.public_key);
                                            !is_dev && !is_parent
                                        })
                                        .cloned()
                                        .collect::<Vec<LoadedWalletInfo>>();
                                    let task_sol_per_wallet = self.app_settings.mix_sol_per_new_wallet; // Use existing setting or add a new one
                                    let task_priority_fee = AppSettings::get_default_priority_fee_lamports(&self.app_settings); // Explicit call
                                    let status_sender_clone = self.retry_zombie_to_mixed_funding_status_sender.clone();

                                    let retry_task_handle = tokio::spawn(async move {
                                        if let Err(e) = Self::retry_fund_mixed_from_zombies_task(
                                            task_rpc_url,
                                            task_mixed_wallets_path,
                                            task_zombie_wallets,
                                            task_sol_per_wallet,
                                            task_priority_fee,
                                            status_sender_clone.clone(),
                                        ).await {
                                            log::error!("Retry Fund Mixed Wallets task ended with error: {}", e);
                                            let _ = status_sender_clone.send(Err(format!("Retry Fund Mixed Wallets task failed: {}", e)));
                                        }
                                    });
                                    self.retry_zombie_to_mixed_funding_task_handle = Some(retry_task_handle);
                                } else {
                                    self.retry_zombie_to_mixed_funding_log_messages.push("❌ Cannot retry: Path to mixed wallets file is missing.".to_string());
                                    self.launch_log_messages.push("❌ Retry Funding failed: Path to mixed wallets file is missing.".to_string());
                                }
                            }
                            // --- End Retry Button ---


                            if self.launch_in_progress || self.mix_wallets_in_progress || self.retry_zombie_to_mixed_funding_in_progress {
                                ui_action_area.add_space(5.0);
                                ui_action_area.horizontal(|ui_spin_combined| {
                                    if self.launch_in_progress {
                                        ui_spin_combined.add(egui::Spinner::new().size(20.0));
                                        ui_spin_combined.label("Launching...");
                                    }
                                    if self.mix_wallets_in_progress {
                                        if self.launch_in_progress { ui_spin_combined.add_space(10.0); } // Add space if both are running
                                        ui_spin_combined.add(egui::Spinner::new().size(20.0));
                                        ui_spin_combined.label("Mixing...");
                                    }
                                });
                            }
                        });
                        ui_frame.add_space(10.0);
                        ui_frame.strong(electric_label_text("Launch Log:"));
                        egui::ScrollArea::vertical().id_source("launch_log_scroll_modern").max_height(150.0).stick_to_bottom(true).show(ui_frame, |ui_log_scroll| {
                            for msg in &self.launch_log_messages {
                                ui_log_scroll.label(egui::RichText::new(msg).font(FontId::monospace(12.0)));
                            }
                        });
if self.launch_in_progress {
                            ui_frame.add_space(5.0);
                            ui_frame.horizontal(|ui_prog| {
                                ui_prog.spinner();
                                ui_prog.label("Launch in progress...");
                            });
                        } else if let Some(link) = &self.launch_completion_link {
                            ui_frame.add_space(10.0);
                            ui_frame.label(electric_label_text("🎉 Launch Successful! 🎉").size(18.0).color(AURORA_ACCENT_TEAL));
                            ui_frame.horizontal(|ui_link| {
                                ui_link.label("Pump.fun Link:");
                                ui_link.hyperlink_to(link, link.clone()).on_hover_text("Open link in browser");
                            });
                            if ui_frame.button("📋 Copy Link").on_hover_text("Copy pump.fun link to clipboard").clicked() {
                                ui_frame.output_mut(|o| o.copied_text = link.clone());
                                self.launch_log_messages.push("📋 Link copied to clipboard!".to_string()); // Add to log for feedback
                            }
                        } else if !self.launch_in_progress && !self.launch_log_messages.is_empty() {
                            // If not in progress, no link, but there are messages, it might be an error or other completion.
                            if let Some(Err(e)) = &self.last_operation_result {
                                 ui_frame.add_space(5.0);
                                 ui_frame.colored_label(Color32::LIGHT_RED, format!("Last operation failed: {}", e));
                            } else if let Some(Ok(s)) = &self.last_operation_result {
                                 ui_frame.add_space(5.0);
                                 // Avoid showing generic "Launch completed" if a more specific link message was already shown or error.
                                 if !s.starts_with("🎉 LAUNCH COMPLETE!") && !s.starts_with("Launch successful:") && !s.starts_with("Simulated:") {
                                    ui_frame.colored_label(Color32::LIGHT_GREEN, format!("Last operation: {}", s));
                                 }
                            }
                        }
                    });
                    ui_scroll.add_space(20.0); // Bottom padding
                }); // End of ScrollArea
            });
        });
    }
    fn show_atomic_buy_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui_frame_content| { // Original ui from Frame renamed to ui_frame_content
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false]) // Allow scroll area to take full width/height
                    .show(ui_frame_content, |ui| { // This 'ui' is now for all scrollable content
                    ui.vertical_centered(|ui_title_centered| { // ui_title_centered is derived from the scrollable 'ui'
                        ui_title_centered.label(
                            egui::RichText::new("💥 Atomic Multi-Wallet Buy")
                                .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                                .color(AURORA_TEXT_PRIMARY),
                        );
                    });
                    ui.add_space(5.0); // This 'ui' is from ScrollArea
                    ui.separator();    // This 'ui' is from ScrollArea
                    ui.add_space(15.0);  // This 'ui' is from ScrollArea

                    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| { // ui_centered is now derived from the scrollable 'ui'
                        ui_centered.set_max_width(1400.0); // Max width for content

                        // --- Configuration Card ---
                        show_section_header(ui_centered, "⚙️ Bundle Configuration", AURORA_ACCENT_NEON_BLUE);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        egui::Grid::new("atomic_buy_inputs_grid_modern")
                            .num_columns(2)
                            .spacing([15.0, 10.0])
                            .striped(false) // No stripes inside this card for cleaner look
                            .show(ui_frame, |ui_grid| {
                                ui_grid.label(electric_label_text("Token Mint Address:"));
                                egui::Frame::none()
                                    .fill(AURORA_BG_PANEL)
                                    .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                    .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                    .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                    .show(ui_grid, |cell_ui| {
                                        cell_ui.add(
                                            egui::TextEdit::singleline(&mut self.atomic_buy_mint_address)
                                                .desired_width(f32::INFINITY)
                                                .text_color(AURORA_TEXT_PRIMARY)
                                                .frame(false)
                                        );
                                    });
                                ui_grid.end_row();

                                ui_grid.label(electric_label_text("Slippage (bps):"));
                                ui_grid.add(egui::DragValue::new(&mut self.atomic_buy_slippage_bps).speed(10.0).clamp_range(0..=10000));
                                ui_grid.end_row();

                                ui_grid.label(electric_label_text("Priority Fee per Tx (lamports):"));
                                ui_grid.add(egui::DragValue::new(&mut self.atomic_buy_priority_fee_lamports_per_tx).speed(1000.0));
                                ui_grid.end_row();

                                ui_grid.label(electric_label_text("Jito Tip for Bundle (SOL):"));
                                ui_grid.add(egui::DragValue::new(&mut self.atomic_buy_jito_tip_sol).speed(0.00001).max_decimals(9).clamp_range(0.0..=f64::MAX));
                                ui_grid.end_row();

                                // Exchange Selection
                                ui_grid.label(electric_label_text("Exchange:"));
                                egui::ComboBox::from_id_source("atomic_buy_exchange_combo_modern_3") // Unique ID
                                    .selected_text(if self.atomic_buy_exchange == "pumpfun" { "Pump.fun" } else { "Bonk" })
                                    .show_ui(ui_grid, |ui_combo| {
                                        ui_combo.selectable_value(&mut self.atomic_buy_exchange, "pumpfun".to_string(), "Pump.fun");
                                        ui_combo.selectable_value(&mut self.atomic_buy_exchange, "raydium".to_string(), "Bonk");
                                    });
                                ui_grid.end_row();

                                // ALT Address - Note: The command crate::commands::atomic_buy::atomic_buy currently does not use an ALT.
                                // This UI element is for future use or if the command is updated.
                                ui_grid.label(electric_label_text("ALT Address (Optional):").italics());
                                 ui_grid.horizontal(|ui_alt_cell| {
                                    egui::ComboBox::from_id_source("atomic_buy_alt_combo")
                                        .selected_text(if self.atomic_buy_alt_address.is_empty() { "None" } else { self.atomic_buy_alt_address.as_str() })
                                        .show_ui(ui_alt_cell, |ui_combo| {
                                            ui_combo.selectable_value(&mut self.atomic_buy_alt_address, "".to_string(), "None");
                                            for alt_addr in &self.available_alt_addresses { // Assuming self.available_alt_addresses is populated
                                                ui_combo.selectable_value(&mut self.atomic_buy_alt_address, alt_addr.clone(), alt_addr.as_str());
                                            }
                                        });
                                    if create_electric_button(ui_alt_cell, "🔄", Some(Vec2::new(30.0,ui_alt_cell.available_height()))).on_hover_text("Reload ALTs from alt_address.txt").clicked() {
                                        self.load_available_alts();
                                    }
                                });
                                ui_grid.end_row();
                            });
                    });
                    ui_centered.add_space(20.0);

                    // --- Wallet Selection Card ---
                    show_section_header(ui_centered, "💳 Wallet Selection & Amounts", AURORA_ACCENT_PURPLE);
                    let mut sell_action_trigger: Option<(String, String, String)> = None; // Moved declaration here
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.horizontal(|ui_select_btns| {
                            if create_electric_button(ui_select_btns, "Select All Trading/Bots", None).clicked() {
                                let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                for wallet in self.wallets.iter_mut() {
                                    if Some(&wallet.address) != parent_pk_str_opt.as_ref() && !wallet.is_primary && !wallet.is_dev_wallet {
                                        wallet.is_selected = true;
                                    }
                                }
                            }
                            if create_electric_button(ui_select_btns, "Deselect All", None).clicked() {
                                for wallet in self.wallets.iter_mut() {
                                    wallet.is_selected = false;
                                }
                            }
                        });
                        ui_frame.separator();

                        egui::ScrollArea::vertical().id_source("atomic_buy_wallet_scroll_modern").max_height(400.0).show(ui_frame, |ui_scroll| {
                            egui::Grid::new("atomic_buy_wallet_grid_modern")
                                .num_columns(7) // Increased to 7 columns
                                .spacing([10.0, 6.0])
                                .striped(true)
                                .show(ui_scroll, |ui_grid| {
                                    ui_grid.label(egui::RichText::new("Include").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                    ui_grid.label(egui::RichText::new("Wallet Address").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                    ui_grid.label(egui::RichText::new("SOL Balance").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                    ui_grid.label(egui::RichText::new("WSOL Bal").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong()); // New WSOL Header
                                    ui_grid.label(egui::RichText::new("Token Balance").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                    ui_grid.label(egui::RichText::new("SOL to Buy With").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                    ui_grid.label(egui::RichText::new("Token Sell Amount / Action").font(FontId::proportional(15.0)).color(AURORA_TEXT_MUTED_TEAL).strong());
                                    ui_grid.end_row();

                                    let parent_pubkey_str = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                    // sell_action_trigger is now declared outside this loop, in the parent scope of this card

                                    for wallet_info in self.wallets.iter_mut() {
                                        // Exclude parent wallet from being selectable for atomic buy/sell actions from this view
                                        if Some(wallet_info.address.clone()) == parent_pubkey_str {
                                            continue;
                                        }
                                        ui_grid.checkbox(&mut wallet_info.is_selected, "");
                                        let short_addr = if wallet_info.address.len() > 10 {
                                            format!("{}...{}", &wallet_info.address[..4], &wallet_info.address[wallet_info.address.len()-4..])
                                        } else {
                                            wallet_info.address.clone()
                                        };
                                        ui_grid.label(egui::RichText::new(&short_addr).font(FontId::monospace(14.0))).on_hover_text(&wallet_info.address);
                                        ui_grid.label(egui::RichText::new(wallet_info.sol_balance.map_or_else(|| "-".to_string(), |b| format!("{:.4}", b))).font(FontId::monospace(14.0)));
                                        // Display WSOL Balance
                                        ui_grid.label(egui::RichText::new(wallet_info.wsol_balance.map_or_else(|| "-".to_string(), |b| format!("{:.4}", b))).font(FontId::monospace(14.0)));
                                        // Display Target Mint Balance
                                        ui_grid.label(egui::RichText::new(wallet_info.target_mint_balance.as_deref().unwrap_or("-")).font(FontId::monospace(14.0)));
                                        egui::Frame::none()
                                            .fill(AURORA_BG_PANEL)
                                            .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                            .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                            .inner_margin(egui::Margin::symmetric(4.0, 1.0)) // Reduced vertical margin
                                            .show(ui_grid, |cell_ui| {
                                                cell_ui.add(
                                                    egui::TextEdit::singleline(&mut wallet_info.atomic_buy_sol_amount_input)
                                                        .desired_width(80.0)
                                                        .hint_text("e.g., 0.01")
                                                        .text_color(AURORA_TEXT_PRIMARY)
                                                        .frame(false)
                                                );
                                            });

                                        // New column for Sell Amount / Action
                                        ui_grid.vertical(|sell_ui| {
                                            sell_ui.horizontal(|h_ui| {
                                                egui::Frame::none()
                                                    .fill(AURORA_BG_PANEL)
                                                    .stroke(Stroke::new(1.5, AURORA_ACCENT_PURPLE.linear_multiply(0.7)))
                                                    .rounding(h_ui.style().visuals.widgets.inactive.rounding)
                                                    .inner_margin(egui::Margin::symmetric(4.0, 1.0))
                                                    .show(h_ui, |cell_ui| {
                                                        cell_ui.add(
                                                            egui::TextEdit::singleline(&mut wallet_info.atomic_sell_token_amount_input)
                                                                .desired_width(80.0)
                                                                .hint_text("Amt or 100%")
                                                                .text_color(AURORA_TEXT_PRIMARY)
                                                                .frame(false)
                                                        );
                                                    });
                                                if create_electric_button(h_ui, "Sell Amt", Some(Vec2::new(60.0, h_ui.available_height()))).on_hover_text("Sell specified token amount").clicked() {
                                                    sell_action_trigger = Some((wallet_info.address.clone(), "amount".to_string(), wallet_info.atomic_sell_token_amount_input.clone()));
                                                }
                                                if create_electric_button(h_ui, "Sell All", Some(Vec2::new(60.0, h_ui.available_height()))).on_hover_text("Sell 100% of this token for this wallet").clicked() {
                                                    sell_action_trigger = Some((wallet_info.address.clone(), "all".to_string(), "100%".to_string())); // "100%" signifies intent
                                                }
                                            });
                                            // Removed "Selling..." spinner and text from here:
                                            // if wallet_info.atomic_is_selling {
                                            //     sell_ui.horizontal(|s_ui|{
                                            //         s_ui.spinner();
                                            //         s_ui.label(egui::RichText::new("Selling...").italics().small());
                                            //     });
                                            // }
                                            // Removed status display from here:
                                            // if let Some(status) = &wallet_info.atomic_sell_status_message {
                                            //     let status_color = if status.starts_with("✅") || status.starts_with("ℹ️") { AURORA_ACCENT_TEAL } else { AURORA_ACCENT_MAGENTA };
                                            //     sell_ui.label(egui::RichText::new(status).small().color(status_color));
                                            // }
                                        });
                                        ui_grid.end_row();
                                    }
                                });
                        });
                    });
                    ui_centered.add_space(20.0);

                    // --- Action and Logs Card ---
                    show_section_header(ui_centered, "🚀 Action & Logs", AURORA_ACCENT_TEAL);
                    create_electric_frame().show(ui_centered, |ui_frame| {

                        // Process Sell Action (moved here, after wallet loop, before buy button logic)
                        if let Some((wallet_address, sell_type, sell_amount_input_value)) = sell_action_trigger.take() { // .take() to consume it
                            if self.atomic_buy_mint_address.trim().is_empty() {
                                // Removed: wallet_info.atomic_sell_status_message = Some("Token Mint Address must be set to sell.".to_string());
                                self.atomic_buy_log_messages.push(format!("[Sell ERROR for {}]: Token Mint Address must be set.", wallet_address));
                            } else if self.atomic_sell_in_progress {
                                 // Removed: wallet_info.atomic_sell_status_message = Some("Another sell operation is already in progress.".to_string());
                                self.atomic_buy_log_messages.push(format!("[Sell IGNORED for {}]: Another sell operation is in progress.", wallet_address));
                            } else {
                                // Find the wallet_info again to update it
                                if let Some(wallet_info) = self.wallets.iter_mut().find(|w| w.address == wallet_address) {
                                    wallet_info.atomic_is_selling = true;
                                    wallet_info.atomic_sell_status_message = Some("Initiating sell...".to_string());
                                }
                                self.atomic_sell_in_progress = true;
                                self.last_operation_result = Some(Ok(format!("Atomic Sell for {} initiated.", wallet_address)));
                                self.atomic_buy_log_messages.push(format!("Initiating Atomic Sell for wallet {} (Type: {})...", wallet_address, sell_type));

                                let mint_address_for_sell = self.atomic_buy_mint_address.trim().to_string();

                                let rpc_client_for_task_sell = Arc::new(AsyncRpcClient::new(self.app_settings.solana_rpc_url.clone())); // Renamed to avoid conflict
                                let status_sender_clone_sell = self.atomic_sell_status_sender.clone(); // Renamed
                                let wallet_address_clone_sell = wallet_address.clone(); // Renamed
                                let loaded_wallet_data_clone_sell = self.loaded_wallet_data.clone(); // Renamed

                                let slippage_bps_clone_sell = self.atomic_buy_slippage_bps; // Renamed
                                let priority_fee_lamports_clone_sell = self.atomic_buy_priority_fee_lamports_per_tx; // Renamed
                                let jito_tip_sol_per_tx_clone_sell = self.atomic_buy_jito_tip_sol; // Renamed
                                let jito_overall_tip_sol_clone_sell = self.atomic_buy_jito_tip_sol; // Renamed
                                let selected_jito_url_clone_sell = self.app_settings.selected_jito_block_engine_url.clone(); // Renamed
                                let jito_tip_account_clone_sell = self.app_settings.jito_tip_account.clone(); // Renamed
                                let exchange_sell_cloned_sell = self.atomic_buy_exchange.clone(); // Renamed
                                let app_settings_clone_sell = self.app_settings.clone(); // Renamed

                                self.atomic_sell_task_handle = Some(tokio::spawn(async move {
                                    let sell_result_async: Result<(), String> = async { // Renamed
                                        let seller_keypair = loaded_wallet_data_clone_sell.iter().find(|lw| lw.public_key == wallet_address_clone_sell)
                                            .and_then(|lw| bs58::decode(&lw.private_key).into_vec().ok())
                                            .and_then(|bytes| Keypair::from_bytes(&bytes).ok())
                                            .ok_or_else(|| "Failed to load seller keypair.".to_string())?;

                                        let token_mint_to_sell_pk = Pubkey::from_str(&mint_address_for_sell)
                                            .map_err(|e| format!("Invalid token mint for sell: {}", e))?;

                                        let rpc_for_balance_check = Arc::new(AsyncRpcClient::new(app_settings_clone_sell.solana_rpc_url.clone()));

                                        let amount_tokens_to_sell_u64 = if sell_type == "all" {
                                            let seller_ata = get_associated_token_address(&seller_keypair.pubkey(), &token_mint_to_sell_pk);
                                            match rpc_for_balance_check.get_token_account_balance(&seller_ata).await {
                                                Ok(balance_resp) => {
                                                    balance_resp.amount.parse::<u64>().map_err(|_| "Failed to parse token balance for 100% sell".to_string())?
                                                },
                                                Err(e) => return Err(format!("Failed to get token balance for 100% sell: {}", e)),
                                            }
                                        } else {
                                            let amount_f64 = sell_amount_input_value.parse::<f64>()
                                                .map_err(|_| "Invalid sell amount. Must be a number.".to_string())?;

                                            let rpc_for_decimals = Arc::new(AsyncRpcClient::new(app_settings_clone_sell.solana_rpc_url.clone()));
                                            let token_decimals = match crate::utils::get_token_decimals(&rpc_for_decimals, &token_mint_to_sell_pk).await { // Corrected path
                                                Ok(decimals) => decimals,
                                                Err(e) => return Err(format!("Failed to get token decimals for sell amount conversion: {}", e)),
                                            };
                                            spl_token::ui_amount_to_amount(amount_f64, token_decimals)
                                        };

                                        if amount_tokens_to_sell_u64 == 0 {
                                            return Err("Amount to sell is zero (after parsing or balance check).".to_string());
                                        }

                                        let sell_exchange_enum = match exchange_sell_cloned_sell.as_str() {
                                            "raydium" => crate::commands::atomic_sell::SellExchange::Raydium,
                                            _ => crate::commands::atomic_sell::SellExchange::PumpFun,
                                        };

                                        crate::commands::atomic_sell::atomic_sell(
                                            rpc_client_for_task_sell,
                                            &app_settings_clone_sell,
                                            sell_exchange_enum,
                                            vec![(seller_keypair, crate::commands::atomic_sell::WalletSellParams { token_mint_to_sell: token_mint_to_sell_pk, amount_tokens_to_sell: amount_tokens_to_sell_u64 })],
                                            slippage_bps_clone_sell,
                                            priority_fee_lamports_clone_sell,
                                            jito_tip_sol_per_tx_clone_sell,
                                            jito_overall_tip_sol_clone_sell,
                                            selected_jito_url_clone_sell,
                                            jito_tip_account_clone_sell,
                                        ).await.map_err(|e| format!("Atomic sell command failed: {}", e))?;
                                        Ok(())
                                    }.await;

                                    if let Err(e) = status_sender_clone_sell.send(match sell_result_async {
                                        Ok(_) => Ok((wallet_address_clone_sell, "✅ Sell task completed.".to_string())),
                                        Err(e) => Err((wallet_address_clone_sell, format!("❌ Sell task failed: {}", e))),
                                    }) {
                                        error!("Failed to send atomic sell completion status: {}", e);
                                    }
                                }));
                            }
                        }

                        // Add the Unwrap WSOL button here, before the buy button's closure
if create_electric_button(ui_frame, "💰 Fetch All Balances", Some(Vec2::new(280.0, 40.0)))
                            .on_hover_text("Fetch SOL balances for all loaded wallets and Token balances for the current Mint Address.")
                            .clicked() && !self.balances_loading && !self.atomic_buy_in_progress && !self.atomic_sell_in_progress && !self.unwrap_wsol_in_progress {

                            self.atomic_buy_log_messages.push("💰 Balance fetch for all wallets initiated...".to_string()); // Log to main atomic log
                            self.last_operation_result = Some(Ok("Balance fetch initiated.".to_string()));

                            let mint_address_for_token_balance = if self.atomic_buy_mint_address.trim().is_empty() {
                                None
                            } else {
                                Some(self.atomic_buy_mint_address.trim().to_string())
                            };
                            self.trigger_balance_fetch(mint_address_for_token_balance);
                        }
                        if self.balances_loading {
                             ui_frame.horizontal(|ui_h_spinner| {
                                ui_h_spinner.add(egui::Spinner::new().size(16.0));
                                ui_h_spinner.label(egui::RichText::new("Fetching balances...").italics().small());
                            });
                        }
                        ui_frame.add_space(10.0); // Space between fetch balances and unwrap button
                        if create_electric_button(ui_frame, "🦴 Unbonk All SOL", Some(Vec2::new(280.0, 40.0)))
                            .on_hover_text("Attempt to unwrap WSOL (unbonk) from all loaded wallets.")
                            .clicked() && !self.unwrap_wsol_in_progress && !self.atomic_buy_in_progress && !self.atomic_sell_in_progress {

                            self.unwrap_wsol_in_progress = true;
                            self.unwrap_wsol_log_messages.clear();
                            self.unwrap_wsol_log_messages.push("🚀 Starting SOL unbonking process...".to_string());
                            self.atomic_buy_log_messages.push("🦴 Unbonk All SOL process started...".to_string());
                            self.last_operation_result = Some(Ok("SOL unbonking initiated.".to_string()));

                            let wallets_to_process_unwrap = self.loaded_wallet_data.clone(); // Use loaded_wallet_data for private keys
                            let rpc_client_unwrap_clone = Arc::new(AsyncRpcClient::new(self.app_settings.solana_rpc_url.clone()));
                            let status_sender_unwrap_clone = self.unwrap_wsol_status_sender.clone();

                            tokio::spawn(async move {
                                let wsol_mint_pk = spl_token::native_mint::ID;
                                let mut unwrapped_count = 0;
                                let mut error_count = 0;

                                for loaded_wallet in wallets_to_process_unwrap {
                                    let _ = status_sender_unwrap_clone.send(Ok(format!("Processing wallet for WSOL: {}", loaded_wallet.public_key)));

                                    let keypair_result = bs58::decode(&loaded_wallet.private_key).into_vec()
                                        .map_err(|e| format!("bs58 decode error for {}: {}", loaded_wallet.public_key, e))
                                        .and_then(|bytes| Keypair::from_bytes(&bytes).map_err(|e| format!("Keypair from_bytes error for {}: {}", loaded_wallet.public_key, e)));

                                    let keypair = match keypair_result {
                                        Ok(kp) => kp,
                                        Err(e) => {
                                            let _ = status_sender_unwrap_clone.send(Err(format!("Failed to load keypair for {}: {}", loaded_wallet.public_key, e)));
                                            error_count += 1;
                                            continue;
                                        }
                                    };
                                    let wallet_pk = keypair.pubkey();
                                    let wsol_ata = get_associated_token_address(&wallet_pk, &wsol_mint_pk);

                                    match rpc_client_unwrap_clone.get_token_account_balance(&wsol_ata).await {
                                        Ok(balance_response) => {
                                            if balance_response.ui_amount.unwrap_or(0.0) > 0.0 {
                                                let _ = status_sender_unwrap_clone.send(Ok(format!("Wallet {} has {} WSOL. Attempting unwrap.", loaded_wallet.public_key, balance_response.ui_amount_string)));

                                                let close_ix_result = spl_token::instruction::close_account(
                                                    &spl_token::ID,
                                                    &wsol_ata,
                                                    &wallet_pk,
                                                    &wallet_pk,
                                                    &[],
                                                );

                                                let close_ix = match close_ix_result {
                                                    Ok(ix) => ix,
                                                    Err(e) => {
                                                        let _ = status_sender_unwrap_clone.send(Err(format!("Failed to create close_account instruction for {}: {}", loaded_wallet.public_key, e)));
                                                        error_count += 1;
                                                        continue;
                                                    }
                                                };

                                                let latest_blockhash_result = rpc_client_unwrap_clone.get_latest_blockhash().await;
                                                let latest_blockhash = match latest_blockhash_result {
                                                     Ok(bh) => bh,
                                                     Err(e) => {
                                                        let _ = status_sender_unwrap_clone.send(Err(format!("Failed to get blockhash for {}: {}", loaded_wallet.public_key, e)));
                                                        error_count += 1;
                                                        continue;
                                                     }
                                                };

                                                let message_result = MessageV0::try_compile(&wallet_pk, &[close_ix], &[], latest_blockhash);
                                                let message = match message_result {
                                                    Ok(msg) => msg,
                                                    Err(e) => {
                                                        let _ = status_sender_unwrap_clone.send(Err(format!("Failed to compile message for {}: {}", loaded_wallet.public_key, e)));
                                                        error_count += 1;
                                                        continue;
                                                    }
                                                };

                                                let tx_result = VersionedTransaction::try_new(VersionedMessage::V0(message), &[&keypair]);
                                                let tx = match tx_result {
                                                    Ok(transaction) => transaction,
                                                    Err(e) => {
                                                         let _ = status_sender_unwrap_clone.send(Err(format!("Failed to sign transaction for {}: {}", loaded_wallet.public_key, e)));
                                                         error_count += 1;
                                                         continue;
                                                    }
                                                };

                                                match utils::transaction::sign_and_send_versioned_transaction(rpc_client_unwrap_clone.as_ref(), tx, &[&keypair]).await {
                                                    Ok(signature) => {
                                                        let _ = status_sender_unwrap_clone.send(Ok(format!("✅ Wallet {}: Successfully unwrapped WSOL. Sig: {}", loaded_wallet.public_key, signature)));
                                                        unwrapped_count += 1;
                                                    }
                                                    Err(e) => {
                                                        let _ = status_sender_unwrap_clone.send(Err(format!("❌ Wallet {}: Failed to send unwrap transaction: {}", loaded_wallet.public_key, e)));
                                                        error_count += 1;
                                                    }
                                                }
                                            } else {
                                                let _ = status_sender_unwrap_clone.send(Ok(format!("ℹ️ Wallet {}: No WSOL to unwrap.", loaded_wallet.public_key)));
                                            }
                                        }
                                        Err(_e) => {
                                            let _ = status_sender_unwrap_clone.send(Ok(format!("ℹ️ Wallet {}: WSOL ATA not found or error fetching balance.", loaded_wallet.public_key)));
                                        }
                                    }
                                    sleep(Duration::from_millis(200)).await; // Shorter delay between wallets
                                }
                                let _ = status_sender_unwrap_clone.send(Ok(format!("🏁 WSOL unwrapping process finished. Unwrapped for {} wallets, {} errors.", unwrapped_count, error_count)));
                            });
                        }
                        if self.unwrap_wsol_in_progress {
                            ui_frame.horizontal(|ui_h_spinner| {
                                ui_h_spinner.add(egui::Spinner::new().size(16.0));
                                ui_h_spinner.label(egui::RichText::new("Unwrapping WSOL...").italics().small());
                            });
                        }
                        ui_frame.add_space(10.0); // Space between unwrap and buy buttons

                        ui_frame.vertical_centered_justified(|ui_action_area| {
                            let mut selected_wallets_for_buy_check: Vec<(String, f64)> = Vec::new();
                            for w in self.wallets.iter().filter(|w_info| w_info.is_selected && Some(w_info.address.clone()) != AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key)) {
                                if let Ok(amount) = w.atomic_buy_sol_amount_input.parse::<f64>() {
                                    if amount > 0.0 {
                                        selected_wallets_for_buy_check.push((w.address.clone(), amount));
                                    }
                                }
                            }
                            let can_launch = !self.atomic_buy_in_progress &&
                                             !self.atomic_buy_mint_address.trim().is_empty() &&
                                             !selected_wallets_for_buy_check.is_empty();

                            if create_electric_button(ui_action_area, "Launch Atomic Buy Bundle", Some(Vec2::new(280.0, 40.0))).on_hover_text("Ensure Mint, Wallets, and Amounts are set.").clicked() && can_launch {
                                self.atomic_buy_in_progress = true;
                                self.atomic_buy_log_messages.push("Initiating Atomic Buy Bundle...".to_string());
                                self.last_operation_result = Some(Ok("Atomic Buy Bundle initiated.".to_string()));

                                let mint_address_clone = self.atomic_buy_mint_address.trim().to_string();
                                let slippage_bps_clone = self.atomic_buy_slippage_bps;
                                let priority_fee_lamports_per_tx_clone = self.atomic_buy_priority_fee_lamports_per_tx;
                                let jito_tip_sol_bundle_clone = self.atomic_buy_jito_tip_sol;
                                let status_sender_clone = self.atomic_buy_status_sender.clone();
                                let loaded_wallet_data_clone = self.loaded_wallet_data.clone();
                                let selected_jito_url_clone = self.app_settings.selected_jito_block_engine_url.clone();
                                // Use the single configured Jito tip account from settings
                                let jito_tip_account_to_use = self.app_settings.jito_tip_account.clone();
                                if jito_tip_account_to_use.is_empty() && jito_tip_sol_bundle_clone > 0.0 {
                                    warn!("Jito tip account in settings is empty, but a tip amount is specified. This will likely cause an error in the command.");
                                    // The command will handle the error if the pubkey is invalid.
                                }

                                // Corrected: The command expects Option<String>
                                let alt_address_str_clone = if self.atomic_buy_alt_address.trim().is_empty() {
                                    None
                                } else {
                                    Some(self.atomic_buy_alt_address.trim().to_string())
                                };
                                let jito_tip_per_tx_clone = self.atomic_buy_jito_tip_per_tx_str.parse::<f64>().unwrap_or(0.0);
                                let jito_tip_overall_bundle_clone = self.atomic_buy_jito_tip_overall_str.parse::<f64>().unwrap_or(0.0);

                                let mut participating_keypairs_with_amounts: Vec<(Keypair, f64)> = Vec::new();
                                let mut wallets_missing_keypairs: Vec<String> = Vec::new();

                                for (pk_str, sol_amount) in &selected_wallets_for_buy_check {
                                    if let Some(loaded_wallet) = loaded_wallet_data_clone.iter().find(|lw| lw.public_key == *pk_str) {
                                        match bs58::decode(&loaded_wallet.private_key).into_vec() {
                                            Ok(keypair_bytes) => {
                                                match Keypair::from_bytes(&keypair_bytes) {
                                                    Ok(kp) => participating_keypairs_with_amounts.push((kp, *sol_amount)),
                                                    Err(e) => wallets_missing_keypairs.push(format!("{} (keypair error: {})", pk_str, e)),
                                                }
                                            }, Err(e) => wallets_missing_keypairs.push(format!("{} (decode error: {})", pk_str, e)),
                                        }
                                    } else { wallets_missing_keypairs.push(format!("{} (not found in loaded data)", pk_str)); }
                                }

                                if !wallets_missing_keypairs.is_empty() {
                                    let err_summary = format!("Failed to prepare bundle. Keypair issues for: {}. See log.", wallets_missing_keypairs.join(", "));
                                    self.atomic_buy_log_messages.push(err_summary.clone());
                                    status_sender_clone.send(Err(err_summary)).unwrap_or_default();
                                    self.atomic_buy_in_progress = false;
                                } else if participating_keypairs_with_amounts.is_empty() {
                                    let err_summary = "No valid wallets with amounts and loadable keypairs to participate.".to_string();
                                    self.atomic_buy_log_messages.push(err_summary.clone());
                                    status_sender_clone.send(Err(err_summary)).unwrap_or_default();
                                    self.atomic_buy_in_progress = false;
                                } else {
                                    self.atomic_buy_log_messages.push(format!("Spawning atomic buy task for {} wallets...", participating_keypairs_with_amounts.len()));
                                    // This is the correct SEARCH block content now
                                    let exchange_for_buy_cloned = self.atomic_buy_exchange.clone();
                                    // jito_tip_per_tx_clone and jito_tip_overall_bundle_clone are now defined outside this else block
                                    // Correctly assign the JoinHandle from tokio::spawn
                                    self.atomic_buy_task_handle = Some(tokio::spawn(async move {
                                        let exchange_enum = match exchange_for_buy_cloned.as_str() {
                                            "raydium" => crate::commands::atomic_buy::Exchange::Raydium,
                                            _ => crate::commands::atomic_buy::Exchange::PumpFun,
                                        };

                                        let result = crate::commands::atomic_buy::atomic_buy(
                                            exchange_enum,
                                            mint_address_clone,
                                            participating_keypairs_with_amounts,
                                            slippage_bps_clone,
                                            priority_fee_lamports_per_tx_clone,
                                            alt_address_str_clone,
                                            jito_tip_per_tx_clone, // Use the distinct per-transaction tip
                                            jito_tip_overall_bundle_clone, // Use the distinct overall bundle tip
                                            selected_jito_url_clone,
                                            jito_tip_account_to_use,
                                        ).await;
                                        match result {
                                            Ok(_) => {
                                                let _ = status_sender_clone.send(Ok("Atomic Buy Bundle task reported completion.".to_string()));
                                            }
                                            Err(e) => {
                                                let _ = status_sender_clone.send(Err(format!("Atomic Buy Bundle task failed: {}", e)));
                                            }
                                        }
                                    })); // Correctly close the tokio::spawn call here
                                 } // This closes the else block from line 3425
                            } // This closes the if/else if/else block from wallet checks
                            if self.atomic_buy_in_progress {
                                ui_action_area.add_space(5.0);
                                ui_action_area.horizontal(|ui_h_spinner| {
                                    ui_h_spinner.add(egui::Spinner::new().size(20.0));
                                    ui_h_spinner.label(egui::RichText::new("Processing bundle...").italics());
                                });
                            }
                        });
                        ui_frame.add_space(10.0);
                        ui_frame.label(electric_label_text("Atomic Buy Log:"));
                        egui::ScrollArea::vertical().id_source("atomic_buy_log_scroll_modern").max_height(150.0).stick_to_bottom(true).show(ui_frame, |ui_scroll_log| {
                            for msg in &self.atomic_buy_log_messages { // Iterate directly
                                ui_scroll_log.label(egui::RichText::new(msg).font(FontId::monospace(12.0)));
                            }
                        });
                    });
                     // Display last operation result (general for the view)
                    if let Some(result) = &self.last_operation_result {
                        ui_centered.add_space(10.0);
                        match result {
                            Ok(msg) => ui_centered.label(egui::RichText::new(format!("✅ {}", msg)).color(AURORA_ACCENT_TEAL)),
                            Err(e) => ui_centered.label(egui::RichText::new(format!("❌ {}", e)).color(AURORA_ACCENT_MAGENTA)),
                        };
                    }
                    ui_centered.add_space(20.0); // Bottom padding

                }); // End Centered Layout
            }); // End ScrollArea
        }); // End Outer Frame for Atomic Buy view
    }

    fn show_check_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("🔍 Balance Check")
                            .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                            .color(AURORA_TEXT_PRIMARY),
                    );
                });
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(15.0);

                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| {
                    ui_centered.set_max_width(1400.0); // Max width for content

                    // --- Input Section ---
                    show_section_header(ui_centered, "📬 Target & Fetch", AURORA_ACCENT_TEAL);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.horizontal_wrapped(|ui_input| {
                            ui_input.label(electric_label_text("Target Mint (Optional):"));
                            egui::Frame::none()
                                .fill(AURORA_BG_PANEL)
                                .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                .rounding(ui_input.style().visuals.widgets.inactive.rounding)
                                .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                .show(ui_input, |cell_ui| {
                                    cell_ui.add(
                                        egui::TextEdit::singleline(&mut self.check_view_target_mint)
                                            .desired_width(300.0) // Adjust width as needed
                                            .text_color(AURORA_TEXT_PRIMARY)
                                            .frame(false)
                                    );
                                });
                            if create_electric_button(ui_input, "🔄 Fetch Balances", None).clicked() && !self.balances_loading {
                                let mint_to_check = self.check_view_target_mint.trim();
                                if mint_to_check.is_empty() {
                                    self.trigger_balance_fetch(None);
                                } else {
                                    self.trigger_balance_fetch(Some(mint_to_check.to_string()));
                                }
                            }
                            if !self.check_view_target_mint.trim().is_empty() {
                                if create_electric_button(ui_input, "📈 Calculate P&L", None).clicked() {
                                    if !self.pnl_calculation_in_progress {
                                        self.pnl_calculation_in_progress = true;
                                        self.pnl_summary = None;
                                        self.pnl_error_message = None;

                                        let rpc_url = self.app_settings.solana_rpc_url.clone();
                                        let target_mint_str = self.check_view_target_mint.trim().to_string();
                                        let pnl_calc_sender_clone = self.pnl_calc_sender.clone();

                                        let user_wallet_address_str_option = self.loaded_wallet_data.first()
                                            .map(|w| w.public_key.clone());

                                        match user_wallet_address_str_option {
                                            Some(user_wallet_address_str) => {
                                                tokio::spawn(async move {
                                                    let result = ModernApp::calculate_pnl_task(
                                                        rpc_url,
                                                        user_wallet_address_str,
                                                        target_mint_str,
                                                    ).await;
                                                    if let Err(e) = pnl_calc_sender_clone.send(result) {
                                                        error!("Failed to send PnL calculation result: {}", e);
                                                    }
                                                });
                                            }
                                            None => {
                                                self.pnl_error_message = Some("Primary wallet address not found. Cannot calculate P&L.".to_string());
                                                self.pnl_calculation_in_progress = false;
                                            }
                                        }
                                    }
                                }
                            }
                            if self.balances_loading || self.pnl_calculation_in_progress { // Show spinner if either is loading
                                ui_input.add(egui::Spinner::new().size(20.0));
                                if self.pnl_calculation_in_progress {
                                     ui_input.label("Calculating P&L...");
                                }
                            }
                        });
                    });
                    ui_centered.add_space(10.0); // Reduced space after input section

                    // Display PnL Info (moved up before summary if calculating or has data)
                    if self.pnl_calculation_in_progress && self.pnl_summary.is_none() && self.pnl_error_message.is_none() {
                        // Already handled by spinner in input section, but can add a specific message here if needed
                    } else if let Some(err_msg) = &self.pnl_error_message {
                         create_electric_frame().show(ui_centered, |ui_frame| {
                            ui_frame.colored_label(Color32::RED, format!("P&L Error: {}", err_msg));
                        });
                        ui_centered.add_space(10.0);
                    } else if let Some(summary) = &self.pnl_summary {
                        show_section_header(ui_centered, "📈 P&L Details", AURORA_ACCENT_MAGENTA);
                        create_electric_frame().show(ui_centered, |ui_frame| {
                            ui_frame.vertical(|ui_pnl| {
                                ui_pnl.label(format!("Token: {}", summary.token_mint.map_or_else(|| "N/A".to_string(), |pk| pk.to_string())));
                                ui_pnl.label(format!("Realized P&L (SOL): {:.4}", summary.realized_pnl_sol));
                                ui_pnl.label(format!("Remaining Tokens Held: {}", summary.remaining_tokens_held));
                                ui_pnl.label(format!("Cost Basis of Remaining (SOL): {:.4}", summary.cost_basis_of_remaining_tokens_sol / sol_to_lamports(1.0) as f64));
                                if summary.remaining_tokens_held > 0 && summary.cost_basis_of_remaining_tokens_sol > 0.0 {
                                    ui_pnl.label(format!("Avg. Cost of Remaining (SOL/Token): {:.8}", summary.average_cost_of_remaining_tokens_sol_per_token / sol_to_lamports(1.0) as f64));
                                } else {
                                    ui_pnl.label("Avg. Cost of Remaining (SOL/Token): N/A");
                                }
                                ui_pnl.label(format!("Total SOL Invested in Buys: {:.4}", summary.total_sol_invested_in_buys as f64 / sol_to_lamports(1.0) as f64));
                                ui_pnl.label(format!("Total SOL Received from Sells: {:.4}", summary.total_sol_received_from_sells as f64 / sol_to_lamports(1.0) as f64));
                                ui_pnl.label(format!("Total Fees Paid (SOL): {:.8}", summary.total_fees_paid_lamports as f64 / sol_to_lamports(1.0) as f64));
                                ui_pnl.label(format!("Total Tokens Bought: {}", summary.total_tokens_bought));
                                ui_pnl.label(format!("Total Tokens Sold: {}", summary.total_tokens_sold));
                            });
                        });
                        ui_centered.add_space(20.0);
                    }


                    // --- Summary Section ---
                    show_section_header(ui_centered, "📊 Summary (Trading Wallets)", AURORA_ACCENT_PURPLE);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        let (total_sol, total_tokens_option) = self.calculate_totals(); // Assumes calculate_totals filters for trading/zombie wallets
                        ui_frame.horizontal_wrapped(|ui_summary| {
                            ui_summary.label(electric_label_text("Total SOL:"));
                            ui_summary.label(egui::RichText::new(format!("◎ {:.6}", total_sol)).color(AURORA_TEXT_PRIMARY).strong());
                            ui_summary.add_space(20.0);
                            if let Some(total_tokens) = total_tokens_option {
                                ui_summary.label(electric_label_text("Total Tokens:"));
                                ui_summary.label(egui::RichText::new(format!("Σ {:.2}", total_tokens)).color(AURORA_TEXT_PRIMARY).strong());
                            } else if !self.check_view_target_mint.trim().is_empty() {
                                ui_summary.label(electric_label_text("Total Tokens:"));
                                ui_summary.label(egui::RichText::new("Σ N/A").color(AURORA_TEXT_MUTED_TEAL));
                            }
                        });
                    });
                    ui_centered.add_space(20.0);

                    // --- Detailed Wallet Balances Section ---
                    show_section_header(ui_centered, "💳 Wallet Details", AURORA_ACCENT_NEON_BLUE);
                    let container_width = ui_centered.available_size_before_wrap().x;

                    create_electric_frame().show(ui_centered, |ui_frame| {
                        if let Some(err) = &self.wallet_load_error {
                            ui_frame.colored_label(AURORA_ACCENT_MAGENTA, err);
                        } else {
                            use egui_extras::{TableBuilder, Column};
                            egui::ScrollArea::vertical()
                                .id_source("check_view_scroll_area")
                                .max_height(600.0)
                                .auto_shrink([false, false]) // Allow scroll area to be wide
                                // .min_scrolled_width(container_width - 30.0) // This is handled by TableBuilder now
                                .show(ui_frame, |ui_scroll| {
                                    TableBuilder::new(ui_scroll)
                                        .striped(true)
                                        .resizable(true)
                                        .cell_layout(egui::Layout::left_to_right(egui::Align::Center)) // Default cell layout
                                        .column(Column::initial(90.0).at_least(70.0).clip(true))  // Type
                                        .column(Column::remainder().at_least(250.0).clip(true))    // Address
                                        .column(Column::initial(130.0).at_least(110.0).clip(true)) // SOL Balance - h_align removed
                                        .column(Column::initial(130.0).at_least(110.0).clip(true)) // Token Balance - h_align removed
                                        .header(24.0, |mut header| { // Increased header height
                                            header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.label(egui::RichText::new("Type").strong().color(AURORA_TEXT_PRIMARY)); });
                                            header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.label(egui::RichText::new("Address").strong().color(AURORA_TEXT_PRIMARY)); });
                                            header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.label(egui::RichText::new("SOL Balance").strong().color(AURORA_TEXT_PRIMARY)); });
                                            header.col(|ui| { ui.painter().rect_filled(ui.available_rect_before_wrap(), Rounding::ZERO, AURORA_BG_PANEL.linear_multiply(0.8)); ui.label(egui::RichText::new("Token Balance").strong().color(AURORA_TEXT_PRIMARY)); });
                                        })
                                        .body(|mut body| {
                                            let mut trading_wallet_counter = 0;
                                            for wallet_info in &self.wallets {
                                                body.row(20.0, |mut row| { // Increased row height
                                                    row.col(|ui| {
                                                        let type_label_text = if wallet_info.is_parent {
                                                            egui::RichText::new("Parent").color(AURORA_ACCENT_TEAL)
                                                        } else if wallet_info.is_dev_wallet {
                                                            egui::RichText::new("Dev").color(AURORA_ACCENT_PURPLE)
                                                        } else if wallet_info.is_primary {
                                                            egui::RichText::new("Primary").color(AURORA_ACCENT_NEON_BLUE)
                                                        } else {
                                                            trading_wallet_counter += 1;
                                                            egui::RichText::new(format!("Bot {}", trading_wallet_counter)).color(AURORA_TEXT_PRIMARY)
                                                        };
                                                        ui.label(type_label_text.font(FontId::proportional(14.5))); // Slightly larger font
                                                    });

                                                    row.col(|ui| {
                                                        ui.label(egui::RichText::new(&wallet_info.address).font(FontId::monospace(14.0)).color(AURORA_TEXT_PRIMARY)).on_hover_text(&wallet_info.address);
                                                    });

                                                    row.col(|ui_cell| { // SOL Balance column
                                                        ui_cell.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_cell_inner| {
                                                            if let Some(err_msg) = &wallet_info.error {
                                                                ui_cell_inner.label(egui::RichText::new("⚠️").color(AURORA_ACCENT_MAGENTA)).on_hover_text(err_msg);
                                                                ui_cell_inner.add_space(4.0);
                                                            }
                                                            if wallet_info.is_loading {
                                                                ui_cell_inner.add(egui::Spinner::new().size(15.0));
                                                            } else {
                                                                match &wallet_info.sol_balance {
                                                                    Some(balance) => { ui_cell_inner.label(egui::RichText::new(format!("◎ {:.4}", balance)).color(AURORA_TEXT_PRIMARY).font(FontId::monospace(14.0))); }
                                                                    None => { ui_cell_inner.label(egui::RichText::new("-").color(AURORA_TEXT_MUTED_TEAL).font(FontId::monospace(14.0))); }
                                                                }
                                                            }
                                                        });
                                                    });

                                                    row.col(|ui_cell| { // Token Balance column
                                                        ui_cell.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_cell_inner| {
                                                            // Error already shown in SOL balance column if present for the same wallet
                                                            if wallet_info.is_loading {
                                                                ui_cell_inner.add(egui::Spinner::new().size(15.0));
                                                            } else {
                                                                match &wallet_info.target_mint_balance {
                                                                    Some(balance_str) if !balance_str.is_empty() && balance_str != "-" && balance_str != "0" => {
                                                                        ui_cell_inner.label(egui::RichText::new(balance_str).color(AURORA_TEXT_PRIMARY).font(FontId::monospace(14.0)));
                                                                    }
                                                                    _ => { ui_cell_inner.label(egui::RichText::new("-").color(AURORA_TEXT_MUTED_TEAL).font(FontId::monospace(14.0))); }
                                                                }
                                                            }
                                                        });
                                                    });
                                                });
                                            }
                                        });
                                });
                        }
                    });
                    ui_centered.add_space(20.0); // Bottom padding
                }); // End Centered Layout
            }); // End Outer Frame
    }

    // fn show_alt_view(&mut self, ui: &mut egui::Ui) { ui.label(egui::RichText::new("ALT MGMT VIEW - Placeholder").font(FontId::new(24.0, egui::FontFamily::Name("KometHeavy".into())))); } // Original placeholder removed

    fn show_settings_view(&mut self, ui: &mut egui::Ui) {
        // Outer frame for the entire settings view
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Make the main settings frame transparent to show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                // Main Title - Centered
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Application Settings")
                            .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                            .color(AURORA_TEXT_PRIMARY),
                    );
                });
                ui.add_space(5.0);
                ui.separator(); // Separator below title
                ui.add_space(15.0);

                // Center the main settings content block horizontally
                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| {
                    // Constrain the maximum width of the settings content block
                    ui_centered.set_max_width(800.0); // Max width for the settings content

                    egui::ScrollArea::vertical()
                        .id_source("settings_scroll_area")
                        .auto_shrink([false, false]) // Horizontal shrink false, vertical scroll true
                        .show(ui_centered, |ui_scroll| {
                            // Section: RPC & Connection Settings
                            show_section_header(ui_scroll, "📡 RPC & Connection", AURORA_ACCENT_NEON_BLUE);
                            create_electric_frame().show(ui_scroll, |ui_frame| {
                                egui::Grid::new("rpc_settings_grid")
                                    .num_columns(2)
                                    .spacing([20.0, 12.0])
                                    .striped(false)
                                    .show(ui_frame, |ui_grid| {
                                        ui_grid.label(electric_label_text("Solana RPC URL:"));
                                        egui::Frame::none()
                                            .fill(AURORA_BG_PANEL)
                                            .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                            .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                            .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                            .show(ui_grid, |cell_ui| {
                                                cell_ui.add(
                                                    egui::TextEdit::singleline(&mut self.app_settings.solana_rpc_url)
                                                        .desired_width(f32::INFINITY)
                                                        .text_color(AURORA_TEXT_PRIMARY) // Ensure text color is set
                                                        .frame(false)
                                                );
                                            });
                                        ui_grid.end_row();

                                        ui_grid.label(electric_label_text("Solana WS URL:"));
                                        egui::Frame::none()
                                            .fill(AURORA_BG_PANEL)
                                            .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                            .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                            .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                            .show(ui_grid, |cell_ui| {
                                                cell_ui.add(
                                                    egui::TextEdit::singleline(&mut self.app_settings.solana_ws_url)
                                                        .desired_width(f32::INFINITY)
                                                        .text_color(AURORA_TEXT_PRIMARY) // Ensure text color is set
                                                        .frame(false)
                                                );
                                            });
                                        ui_grid.end_row();

                                        ui_grid.label(electric_label_text("Commitment Level:"));
                                        egui::ComboBox::from_id_source("commitment_combo")
                                            .selected_text(self.app_settings.commitment.to_uppercase())
                                            .show_ui(ui_grid, |ui_combo| {
                                                ui_combo.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                                                ui_combo.selectable_value(&mut self.app_settings.commitment, "processed".to_string(), "Processed");
                                                ui_combo.selectable_value(&mut self.app_settings.commitment, "confirmed".to_string(), "Confirmed");
                                                ui_combo.selectable_value(&mut self.app_settings.commitment, "finalized".to_string(), "Finalized");
                                            });
                                        ui_grid.end_row();
                                    });
                            });
                            ui_scroll.add_space(20.0);

                            // Section: Jito Configuration
                            show_section_header(ui_scroll, "🔷 Jito Configuration", AURORA_ACCENT_PURPLE);
                            create_electric_frame().show(ui_scroll, |ui_frame| {
                                egui::Grid::new("jito_settings_grid")
                                    .num_columns(2)
                                    .spacing([20.0, 12.0])
                                    .striped(false)
                                    .show(ui_frame, |ui_grid| {
                                        ui_grid.label(electric_label_text("Jito Block Engine Endpoint:"));
                                        let selected_url = self.app_settings.selected_jito_block_engine_url.clone();
                                        egui::ComboBox::from_id_source("jito_endpoint_combo")
                                            .selected_text(selected_url.split('/').nth(2).unwrap_or(&selected_url))
                                            .show_ui(ui_grid, |ui_combo| {
                                                ui_combo.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                                                for endpoint in crate::models::settings::JITO_BLOCK_ENGINE_ENDPOINTS.iter() {
                                                    ui_combo.selectable_value(
                                                        &mut self.app_settings.selected_jito_block_engine_url,
                                                        endpoint.to_string(),
                                                        endpoint.split('/').nth(2).unwrap_or(endpoint),
                                                    );
                                                }
                                            });
                                        ui_grid.end_row();

                                        // Jito Tip Account (General) - Input
                                        ui_grid.label(electric_label_text("Jito Tip Account Pubkey (General):"));
                                        egui::Frame::none()
                                            .fill(AURORA_BG_PANEL)
                                            .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                            .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                            .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                            .show(ui_grid, |cell_ui| {
                                                cell_ui.add(
                                                    egui::TextEdit::singleline(&mut self.app_settings.jito_tip_account)
                                                        .desired_width(f32::INFINITY)
                                                        .text_color(AURORA_TEXT_PRIMARY)
                                                        .frame(false)
                                                );
                                            });
                                        ui_grid.end_row();

                                        // Removed the derived pubkey row as the input is now the public key itself.

                                        ui_grid.label(electric_label_text("Jito Tip Amount (SOL - General):"));
                                        ui_grid.add(egui::DragValue::new(&mut self.app_settings.jito_tip_amount_sol).speed(0.00001).max_decimals(5).range(0.0..=1.0).prefix("◎ "));
                                        ui_grid.end_row();

                                        ui_grid.label(electric_label_text("Volume Bot Jito Settings:"));
                                        ui_grid.vertical(|ui_vert| {
                                            // Removed duplicate checkbox. Kept the one with electric_label_text for consistency.
                                            ui_vert.checkbox(&mut self.app_settings.use_jito_for_volume_bot, electric_label_text("Use Jito for Volume Bot Transactions"));

                                            // Jito Tip Account PK (Volume Bot) - Input
                                            ui_vert.label(electric_label_text("Jito Tip Account PK (Volume Bot):"));
                                            egui::Frame::none()
                                                .fill(AURORA_BG_PANEL)
                                                .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                                .rounding(ui_vert.style().visuals.widgets.inactive.rounding)
                                                .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                                .show(ui_vert, |cell_ui| {
                                                    cell_ui.add(
                                                        egui::TextEdit::singleline(&mut self.app_settings.jito_tip_account_pk_str_volume_bot)
                                                            .desired_width(f32::INFINITY)
                                                            .text_color(AURORA_TEXT_PRIMARY)
                                                            .frame(false)
                                                    );
                                                });

                                            // Jito Tip Account PK (Volume Bot) - Derived Pubkey (Display only if input is a valid PK)
                                            // We assume jito_tip_account_pk_str_volume_bot *is* the public key here as per its name.
                                            // If it were a private key, we'd derive. For a public key, just display it.
                                            if !self.app_settings.jito_tip_account_pk_str_volume_bot.is_empty() {
                                                match Pubkey::from_str(&self.app_settings.jito_tip_account_pk_str_volume_bot) {
                                                    Ok(pk) => {
                                                        ui_vert.horizontal(|ui_h_vb_pk|{
                                                            ui_h_vb_pk.label(electric_label_text("VB Tip Account Pubkey:"));
                                                            ui_h_vb_pk.monospace(egui::RichText::new(pk.to_string()).color(AURORA_TEXT_PRIMARY).font(egui::FontId::monospace(14.0)));
                                                        });
                                                    }
                                                    Err(_) => {
                                                        ui_vert.label(electric_label_text("VB Tip Account Pubkey: Invalid").color(AURORA_ACCENT_MAGENTA));
                                                    }
                                                }
                                            } else {
                                                 ui_vert.label(electric_label_text("VB Tip Account Pubkey: -").italics());
                                            }

                                            ui_vert.horizontal(|ui_horiz| {
                                                ui_horiz.label(electric_label_text("Jito Tip (Lamports - Volume Bot):"));
                                                ui_horiz.add(egui::DragValue::new(&mut self.app_settings.jito_tip_lamports_volume_bot).speed(100.0).range(0..=1_000_000));
                                            });
                                            ui_vert.label(electric_label_text(&format!("(Equivalent to ◎{})", lamports_to_sol(self.app_settings.jito_tip_lamports_volume_bot))));
                                        });
                                        ui_grid.end_row();
                                    });
                            });
                            ui_scroll.add_space(20.0);

                            // Section: Wallet & Key Management
                            show_section_header(ui_scroll, "🔑 Wallet & Key Management", AURORA_ACCENT_TEAL);
                            create_electric_frame().show(ui_scroll, |ui_frame| {
                                egui::Grid::new("wallet_keys_grid")
                                    .num_columns(2)
                                    .spacing([20.0, 12.0])
                                    .striped(false)
                                    .show(ui_frame, |ui_grid| {
                                        ui_grid.label(electric_label_text("Keys File Path:"));
                                        ui_grid.horizontal(|ui_horiz| {
                                            let full_path_str = self.app_settings.keys_path.clone();
                                            let display_path = std::path::Path::new(&full_path_str)
                                                .file_name()
                                                .map(|name| name.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| full_path_str.clone()); // Fallback to full path if no filename or if path is empty
                                            ui_horiz.label(display_path).on_hover_text(&full_path_str);
                                            if create_electric_button(ui_horiz, "Select File", None).clicked() {
                                                if let Some(path) = FileDialog::new().add_filter("JSON", &["json"]).set_directory(".").pick_file() {
                                                    self.app_settings.keys_path = path.to_string_lossy().to_string();
                                                    self.settings_feedback = Some("New keys file selected. Save and reload wallets.".to_string());
                                                }
                                            }
                                            if create_electric_button(ui_horiz, "Load Keys", None).clicked() {
                                                self.load_wallets_from_settings_path();
                                            }
                                        });
                                        ui_grid.end_row();

                                        ui_grid.label(electric_label_text("Generate New Keys File:"));
                                        ui_grid.horizontal(|ui_horiz| {
                                            ui_horiz.add(egui::DragValue::new(&mut self.settings_generate_count).speed(1.0).range(1..=1000).prefix("Count: "));
                                            if create_electric_button(ui_horiz, "Generate & Save", None).clicked() {
                                                match crate::key_utils::generate_and_save_keypairs(Path::new("."), self.settings_generate_count) {
                                                    Ok(new_path) => {
                                                        let new_path_str = new_path.file_name().unwrap_or_default().to_string_lossy().to_string();
                                                        self.settings_feedback = Some(format!("Generated {} keys: {}. Select and load if desired.", self.settings_generate_count, new_path_str));
                                                    }
                                                    Err(e) => { self.settings_feedback = Some(format!("Error generating keys: {}", e)); error!("Error generating keys: {}", e); }
                                                }
                                            }
                                        });
                                        ui_grid.end_row();

                                        // Parent Wallet Private Key - Input
                                        ui_grid.label(electric_label_text("Parent Wallet Private Key:"));
                                        egui::Frame::none()
                                            .fill(AURORA_BG_PANEL)
                                            .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                            .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                            .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                            .show(ui_grid, |cell_ui| {
                                                cell_ui.add(
                                                    egui::TextEdit::singleline(&mut self.app_settings.parent_wallet_private_key)
                                                        .password(true)
                                                        .desired_width(f32::INFINITY)
                                                        .text_color(AURORA_TEXT_PRIMARY)
                                                        .frame(false)
                                                );
                                            });
                                        ui_grid.end_row();

                                        // Parent Wallet Private Key - Derived Pubkey
                                        ui_grid.label(electric_label_text("Parent Pubkey:"));
                                        if let Some(pubkey) = crate::models::settings::AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key) {
                                            ui_grid.monospace(egui::RichText::new(&pubkey).color(AURORA_TEXT_PRIMARY).font(egui::FontId::monospace(14.0)));
                                        } else {
                                            ui_grid.label(electric_label_text("-").italics());
                                        }
                                        ui_grid.end_row();

                                        // Dev Wallet Private Key - Input + Button
                                        ui_grid.label(electric_label_text("Dev Wallet Private Key:"));
                                        ui_grid.horizontal(|ui_inner| {
                                            let mut temp_dev_pk = self.app_settings.dev_wallet_private_key.clone();
                                            let response = egui::Frame::none()
                                                .fill(AURORA_BG_PANEL)
                                                .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                                .rounding(ui_inner.style().visuals.widgets.inactive.rounding)
                                                .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                                .show(ui_inner, |cell_ui| {
                                                    cell_ui.add(
                                                        egui::TextEdit::singleline(&mut temp_dev_pk)
                                                            .password(true)
                                                            .id_source("dev_wallet_pk_input")
                                                            .text_color(AURORA_TEXT_PRIMARY)
                                                            .frame(false)
                                                            .desired_width(cell_ui.available_width()) // Fill the frame
                                                    )
                                                }).inner; // Get the response from the TextEdit
                                            // Adjust the desired_width of the Frame itself if needed, or the parent horizontal layout item spacing.
                                            // For now, assuming the horizontal layout handles spacing with the button.
                                            if response.changed() {
                                                self.app_settings.dev_wallet_private_key = temp_dev_pk;
                                            }
                                            if create_electric_button(ui_inner, "Generate New", None).on_hover_text("Generates a new dev wallet, saves to dev_wallet.json, and populates the key here.").clicked() {
                                                match crate::key_utils::generate_single_dev_wallet() {
                                                    Ok((priv_key_bs58, pub_key_bs58, filename)) => {
                                                        self.app_settings.dev_wallet_private_key = priv_key_bs58;
                                                        self.settings_feedback = Some(format!("New Dev Wallet ({}) saved to {}. Save settings to persist.", pub_key_bs58, filename));
                                                    }
                                                    Err(e) => { self.settings_feedback = Some(format!("Error generating dev wallet: {}", e)); error!("Error generating dev wallet: {}", e);}
                                                }
                                            }
                                        });
                                        ui_grid.end_row();

                                        // Dev Wallet Private Key - Derived Pubkey
                                        ui_grid.label(electric_label_text("Dev Wallet Pubkey:"));
                                        if let Some(pubkey) = crate::models::settings::AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key) {
                                            ui_grid.monospace(egui::RichText::new(&pubkey).color(AURORA_TEXT_PRIMARY).font(egui::FontId::monospace(14.0)));
                                        } else {
                                            ui_grid.label(electric_label_text("-").italics());
                                        }
                                        ui_grid.end_row();
                                    });
                            });
                            ui_scroll.add_space(20.0);

                            // Section: Default Transaction Parameters
                            show_section_header(ui_scroll, "✈️ Transaction Defaults", AURORA_ACCENT_MAGENTA);
                            create_electric_frame().show(ui_scroll, |ui_frame| {
                                egui::Grid::new("tx_defaults_grid")
                                    .num_columns(2)
                                    .spacing([20.0, 12.0])
                                    .striped(false)
                                    .show(ui_frame, |ui_grid| {
                                    ui_grid.label(electric_label_text("Default Priority Fee (SOL):"));
                                    ui_grid.add(egui::DragValue::new(&mut self.app_settings.default_priority_fee_sol).speed(0.00001).max_decimals(9).range(0.0..=f64::MAX).prefix("◎ "));
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Default Slippage (%):"));
                                    ui_grid.add(egui::DragValue::new(&mut self.app_settings.default_slippage_percent).speed(0.1).range(0.0..=100.0).suffix("%"));
                                    ui_grid.end_row();
                                    });
                            });
                            ui_scroll.add_space(20.0);

                            // Section: API URLs (REMOVED)
                            // show_section_header(ui_scroll, "🔗 API URLs", AURORA_ACCENT_NEON_BLUE);
                            // create_electric_frame().show(ui_scroll, |ui_frame| {
                            //     egui::Grid::new("api_urls_grid")
                            //         .num_columns(2)
                            //         .spacing([20.0, 12.0])
                            //         .striped(false)
                            //         .show(ui_frame, |ui_grid| {
                            //             add_labeled_electric_input(ui_grid, "PumpFun API URL:", &mut self.app_settings.pumpfun_api_url, false);
                            //             ui_grid.end_row();
                            //             add_labeled_electric_input(ui_grid, "PumpFun Portal API URL:", &mut self.app_settings.pumpfun_portal_api_url, false);
                            //             ui_grid.end_row();
                            //             add_labeled_electric_input(ui_grid, "IPFS API URL:", &mut self.app_settings.ipfs_api_url, false);
                            //             ui_grid.end_row();
                            //         });
                            // });
                            // ui_scroll.add_space(20.0); // Space after API URLs also removed

                            // Save Button and Feedback - Placed inside the ScrollArea but outside specific sections
                            if let Some(feedback_msg) = &self.settings_feedback {
                                ui_scroll.label(
                                    egui::RichText::new(feedback_msg)
                                        .color(if feedback_msg.starts_with("Error") || feedback_msg.starts_with("Failed") {AURORA_ACCENT_MAGENTA} else {AURORA_ACCENT_TEAL})
                                        .italics(),
                                );
                                ui_scroll.add_space(10.0);
                            }


                            // Centering the button within the scroll area's available width (respecting max_width of parent)
                            ui_scroll.vertical_centered_justified(|ui_button_centered|{
                                if create_electric_button(ui_button_centered, "💾 Save All Settings", None).clicked() {
                                    log::info!("Save Settings button clicked.");
                                    match self.app_settings.save() {
                                        Ok(_) => {
                                            log::info!("Settings saved successfully.");
                                            self.settings_feedback = Some("Settings saved successfully.".to_string());
                                            self.load_wallets_from_settings_path();
                                            self.load_available_alts();
                                            self.load_available_mint_keypairs();
                                        }
                                        Err(e) => {
                                            log::error!("Failed to save settings: {}", e);
                                            self.settings_feedback = Some(format!("Failed to save settings: {}", e));
                                        }
                                    }
                                }
                            });
                            ui_scroll.add_space(20.0); // Bottom padding inside scroll area
                        }); // End ScrollArea
                }); // End Centered Layout
            }); // End outer_frame.show
    }






    // Navigation button (already defined, kept for sidebar)
//                }); // REMOVED - Stray from previous conflict resolution?
//        }); // End of ui.columns - REMOVED - Stray from previous conflict resolution?
//    } // REMOVED - Stray from previous conflict resolution?

    // Navigation button (already defined, kept for sidebar)
    fn show_volume_bot_view(&mut self, ui: &mut egui::Ui) {
        // Main Title for the View
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("📈 Volume Bot")
                    .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                    .color(AURORA_TEXT_PRIMARY),
            );
        });
        ui.add_space(5.0);
        ui.separator();
        ui.add_space(15.0);

        // Outer centered layout for the two columns
        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered_columns| {
            ui_centered_columns.set_max_width(1100.0); // Max width for the two-column layout

            ui_centered_columns.columns(2, |columns| {
                // --- Left Column (Controls) ---
                egui::ScrollArea::vertical()
                    .id_source("volume_bot_controls_scroll_left_themed")
                    .auto_shrink([false, false])
                    .show(&mut columns[0], |ui_scroll_left| {

                        // --- Wallet Setup Section ---
                        show_section_header(ui_scroll_left, "💼 Wallet Setup", AURORA_ACCENT_NEON_BLUE);
                        create_electric_frame().show(ui_scroll_left, |ui_frame| {
                            ui_frame.label(electric_label_text("Generate New Wallets for Volume Bot:"));
                            ui_frame.horizontal(|ui_gen| {
                                ui_gen.label(electric_label_text("Number to generate:"));
                                ui_gen.add(egui::DragValue::new(&mut self.volume_bot_num_wallets_to_generate).range(1..=200));
                            });
                            if self.volume_bot_generation_in_progress {
                                ui_frame.horizontal(|ui_prog| { ui_prog.spinner(); ui_prog.label(electric_label_text("Generating wallets...")); });
                            } else {
                                if create_electric_button(ui_frame, "Generate & Save Wallets", None).clicked() {
                                    if self.volume_bot_num_wallets_to_generate > 0 {
                                        self.volume_bot_generation_in_progress = true;
                                        self.volume_bot_wallets_generated = false;
                                        self.volume_bot_wallets.clear();
                                        self.last_generated_volume_wallets_file = None;
                                        self.sim_log_messages.push(format!("Starting generation of {} wallets...", self.volume_bot_num_wallets_to_generate));
                                        self.last_operation_result = Some(Ok(format!("Generation of {} wallets initiated.", self.volume_bot_num_wallets_to_generate)));
                                        let num_to_gen = self.volume_bot_num_wallets_to_generate;
                                        let status_sender_clone = self.volume_bot_wallet_gen_status_sender.clone();
                                        let wallets_sender_clone = self.volume_bot_wallets_sender.clone();
                                        tokio::spawn(generate_volume_bot_wallets_task(num_to_gen, status_sender_clone, wallets_sender_clone));
                                    } else {
                                        self.last_operation_result = Some(Err("Number of wallets must be > 0.".to_string()));
                                    }
                                }
                            }
                            if self.volume_bot_wallets_generated {
                                if let Some(filename) = &self.last_generated_volume_wallets_file {
                                    ui_frame.label(electric_label_text(&format!("Generated {} wallets: {}", self.volume_bot_wallets.len(), filename)));
                                } else if !self.volume_bot_wallets.is_empty() {
                                    ui_frame.label(electric_label_text(&format!("Generated {} wallets (file info pending).", self.volume_bot_wallets.len())));
                                }
                            }
                            ui_frame.add_space(10.0);
                            ui_frame.separator();
                            ui_frame.add_space(10.0);

                            ui_frame.label(electric_label_text("Load Existing Wallets File:"));
                            ui_frame.horizontal(|ui_load| {
                                // Wrap TextEdit in a Frame for styling
                                let available_width_for_text_edit = ui_load.available_width() - 120.0; // Approx width for button
                                egui::Frame::none()
                                    .fill(AURORA_BG_PANEL)
                                    .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                    .rounding(ui_load.style().visuals.widgets.inactive.rounding)
                                    .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                    .show(ui_load, |cell_ui| {
                                        cell_ui.add(
                                            egui::TextEdit::singleline(&mut self.volume_bot_source_wallets_file)
                                                .hint_text("path/to/wallets.json")
                                                .desired_width(available_width_for_text_edit.max(50.0)) // Ensure some min width
                                                .text_color(AURORA_TEXT_PRIMARY)
                                                .frame(false)
                                        );
                                    });
                                if create_electric_button(ui_load, "📂 Browse", Some(Vec2::new(80.0, ui_load.available_height()))).clicked() { // Sized browse button
                                    if let Some(path) = FileDialog::new().add_filter("JSON", &["json"]).set_directory(".").pick_file() {
                                        self.volume_bot_source_wallets_file = path.to_string_lossy().into_owned();
                                    }
                                }
                            });
                            if create_electric_button(ui_frame, "Load Wallets from File", None).clicked() {
                                // ... (Load logic unchanged)
                                if !self.volume_bot_source_wallets_file.trim().is_empty() {
                                    self.volume_bot_wallets.clear();
                                    self.volume_bot_wallets_generated = false;
                                    let params = LoadVolumeWalletsFromFileParams {
                                        file_path: self.volume_bot_source_wallets_file.trim().to_string(),
                                        wallets_sender: self.volume_bot_wallets_sender.clone(),
                                        status_sender: self.sim_status_sender.clone(),
                                    };
                                    self.start_load_volume_wallets_request = Some(params);
                                    self.last_operation_result = Some(Ok("Wallet load request queued.".to_string()));
                                } else {
                                    self.last_operation_result = Some(Err("Wallets file path is empty.".to_string()));
                                }
                            }
                            if !self.volume_bot_wallets.is_empty() && !self.volume_bot_wallets_generated {
                                 ui_frame.label(electric_label_text(&format!("Loaded {} wallets from file.", self.volume_bot_wallets.len())));
                            }
                            ui_frame.add_space(10.0);
                            ui_frame.separator();
                            ui_frame.add_space(10.0);

                            ui_frame.horizontal(|ui_refresh| {
                                ui_refresh.strong(electric_label_text("Volume Wallet Balances"));
                                if create_electric_button(ui_refresh, "🔄 Refresh", None).on_hover_text("Refresh SOL & Token balances (Token CA must be set below)").clicked() {
                                    // ... (Refresh logic unchanged)
                                    if !self.sim_token_mint.trim().is_empty() {
                                        let addresses_to_check: Vec<String> = self.volume_bot_wallets.iter().map(|lw| lw.public_key.clone()).collect();
                                        if !addresses_to_check.is_empty() {
                                            self.balances_loading = true;
                                            self.last_operation_result = Some(Ok(format!("Fetching balances for {} volume wallets...", addresses_to_check.len())));
                                            self.volume_bot_wallet_display_infos.clear();
                                            for (index, lw_info) in self.volume_bot_wallets.iter().enumerate() {
                                                self.volume_bot_wallet_display_infos.push(WalletInfo {
                                                    address: lw_info.public_key.clone(),
                                                    name: format!("Volume Wallet {}", index + 1),
                                                    is_primary: false, is_dev_wallet: false, is_parent: false,
                                                    sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: true,
                                                    is_selected: false, sell_percentage_input: String::new(), sell_amount_input: String::new(), is_selling: false,
                                                    atomic_buy_sol_amount_input: String::new(),
                                                    atomic_sell_token_amount_input: String::new(), // New field
                                                    atomic_is_selling: false, // New field
                                                    atomic_sell_status_message: None, // New field
                                                });
                                            }
                                            let params = FetchBalancesTaskParams {
                                                rpc_url: self.app_settings.solana_rpc_url.clone(),
                                                addresses_to_check,
                                                target_mint: self.sim_token_mint.trim().to_string(),
                                                balance_fetch_sender: self.balance_fetch_sender.clone(),
                                            };
                                            self.start_fetch_balances_request = Some(params);
                                        } else { self.last_operation_result = Some(Err("No volume wallets loaded to refresh.".into())); }
                                    } else { self.last_operation_result = Some(Err("Set Token CA in Sim Config to refresh token balances.".into())); }
                                }
                                if self.balances_loading { ui_refresh.spinner(); }
                            });
                            if !self.volume_bot_wallet_display_infos.is_empty() {
                                egui::ScrollArea::vertical().id_source("vol_wallet_bal_scroll_themed").max_height(100.0).auto_shrink([false,true]).show(ui_frame, |ui_s| {
                                    for wallet_info in &self.volume_bot_wallet_display_infos {
                                        ui_s.horizontal_wrapped(|ui_bal_line| {
                                            ui_bal_line.monospace(format!("{}...", &wallet_info.address[0..8]));
                                            if wallet_info.is_loading { ui_bal_line.spinner(); }
                                            else {
                                                ui_bal_line.label(electric_label_text(&format!("SOL: {:.4}", wallet_info.sol_balance.unwrap_or(0.0))));
                                                if let Some(token_bal_str) = &wallet_info.target_mint_balance { ui_bal_line.label(electric_label_text(&format!("Token: {}", token_bal_str))); }
                                                else { ui_bal_line.label(electric_label_text("Token: N/A")); }
                                                if let Some(err) = &wallet_info.error { ui_bal_line.colored_label(AURORA_ACCENT_MAGENTA, format!("Err: {}", err));}
                                            }
                                        });
                                    }
                                });
                            } else if !self.volume_bot_wallets.is_empty() {
                                ui_frame.label(electric_label_text(&format!("{} wallets ready. Click Refresh to see balances.", self.volume_bot_wallets.len())));
                            }
                        });
                        ui_scroll_left.add_space(15.0);

                        // --- Funding & Collection Section ---
                        show_section_header(ui_scroll_left, "💸 Funding & Collection", AURORA_ACCENT_PURPLE);
                        let wallets_available = !self.volume_bot_wallets.is_empty();
                        create_electric_frame().show(ui_scroll_left, |ui_frame| {
                            ui_frame.add_enabled_ui(wallets_available, |ui_enabled_frame| {
                                ui_enabled_frame.label(electric_label_text("Distribute Total SOL from a Source Wallet:"));
                                ui_enabled_frame.horizontal(|ui_dist_total| {
                                    ui_dist_total.label(electric_label_text("Total SOL:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_dist_total.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 1.0)) // Reduced vertical margin
                                        .show(ui_dist_total, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.volume_bot_total_sol_to_distribute_input)
                                                    .desired_width(80.0)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_dist_total.label(electric_label_text("Source Wallet PK:"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_dist_total.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 1.0)) // Reduced vertical margin
                                        .show(ui_dist_total, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.volume_bot_funding_source_private_key_input)
                                                    .password(true)
                                                    .desired_width(150.0) // Adjusted width
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                });
                                let can_distribute_total = wallets_available && self.volume_bot_total_sol_to_distribute_input.parse::<f64>().unwrap_or(0.0) > 0.0 && !self.volume_bot_funding_source_private_key_input.is_empty();
                                if create_electric_button(ui_enabled_frame, "Distribute from Source PK", None).clicked() && can_distribute_total {
                                    // ... (Distribute logic unchanged)
                                     if let Ok(total_sol) = self.volume_bot_total_sol_to_distribute_input.parse::<f64>() {
                                        if total_sol > 0.0 {
                                            self.volume_bot_funding_in_progress = true;
                                            self.volume_bot_funding_log_messages.push("Initiating SOL total distribution...".to_string());
                                            let params = DistributeTotalSolTaskParams {
                                                rpc_url: self.app_settings.solana_rpc_url.clone(),
                                                source_funding_keypair_str: self.volume_bot_funding_source_private_key_input.trim().to_string(),
                                                wallets_to_fund: self.volume_bot_wallets.clone(),
                                                total_sol_to_distribute: total_sol,
                                                status_sender: self.volume_bot_funding_status_sender.clone(),
                                            };
                                            self.start_distribute_total_sol_request = Some(params);
                                        } else { self.last_operation_result = Some(Err("Total SOL must be > 0".into())); }
                                    } else { self.last_operation_result = Some(Err("Invalid Total SOL amount".into())); }
                                }
                                ui_enabled_frame.add_space(10.0);

                                ui_enabled_frame.label(electric_label_text("Fund from Parent Wallet (from Settings):"));
                                ui_enabled_frame.horizontal(|ui_fund_parent| {
                                    ui_fund_parent.label(electric_label_text("Parent:"));
                                    if let Some(pk_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key) {
                                         ui_fund_parent.monospace(format!("{}...", &pk_str[0..8]));
                                    } else { ui_fund_parent.colored_label(AURORA_ACCENT_MAGENTA, "Not Set"); }
                                    ui_fund_parent.label(electric_label_text("SOL per wallet:"));
                                    ui_fund_parent.add(egui::DragValue::new(&mut self.volume_bot_funding_per_wallet_sol).speed(0.001).range(0.0..=10.0).prefix("◎ "));
                                });
                                let can_fund_from_parent = wallets_available && !self.app_settings.parent_wallet_private_key.is_empty() && self.volume_bot_funding_per_wallet_sol > 0.0;
                                if create_electric_button(ui_enabled_frame, "Fund from Parent", None).clicked() && can_fund_from_parent {
                                    // ... (Fund from Parent logic unchanged)
                                    self.volume_bot_funding_in_progress = true;
                                    self.volume_bot_funding_log_messages.push(format!("Funding ◎{} to {} wallets from parent...", self.volume_bot_funding_per_wallet_sol, self.volume_bot_wallets.len()));
                                    let params = FundVolumeWalletsTaskParams {
                                        rpc_url: self.app_settings.solana_rpc_url.clone(),
                                        parent_keypair_str: self.app_settings.parent_wallet_private_key.clone(),
                                        wallets_to_fund: self.volume_bot_wallets.clone(),
                                        funding_amount_lamports: sol_to_lamports(self.volume_bot_funding_per_wallet_sol),
                                        status_sender: self.volume_bot_funding_status_sender.clone(),
                                    };
                                    self.start_fund_volume_wallets_request = Some(params);
                                }
                                 if self.volume_bot_funding_in_progress {
                                    ui_enabled_frame.horizontal(|ui_prog_fund| { ui_prog_fund.spinner(); ui_prog_fund.label(electric_label_text("Funding operation in progress...")); });
                                }
                                ui_enabled_frame.label(electric_label_text("Funding Log:"));
                                egui::ScrollArea::vertical().id_source("funding_log_scroll_themed").max_height(60.0).auto_shrink([false, true]).stick_to_bottom(true).show(ui_enabled_frame, |ui_s| {
                                    for msg in &self.volume_bot_funding_log_messages { ui_s.monospace(msg); }
                                });
                                ui_enabled_frame.add_space(10.0);

                                if create_electric_button(ui_enabled_frame, "Gather All Funds to Parent Wallet", None).on_hover_text("Gathers SOL and specified Token (from Sim Config) to Parent.").clicked() {
                                    // ... (Gather All Funds logic unchanged)
                                     if self.app_settings.parent_wallet_private_key.trim().is_empty() {
                                        self.last_operation_result = Some(Err("Parent wallet not set in Settings".into()));
                                    } else {
                                        self.volume_bot_funding_log_messages.push("Gather all funds request queued...".to_string());
                                        let params = GatherAllFundsTaskParams {
                                            rpc_url: self.app_settings.solana_rpc_url.clone(),
                                            parent_private_key_str: self.app_settings.parent_wallet_private_key.clone(),
                                            volume_wallets: self.volume_bot_wallets.clone(),
                                            token_mint_ca_str: self.sim_token_mint.trim().to_string(),
                                            status_sender: self.volume_bot_funding_status_sender.clone(),
                                        };
                                        self.start_gather_all_funds_request = Some(params);
                                    }
                                }
                            });
                        });
                        ui_scroll_left.add_space(15.0);

                        // --- Simulation Configuration & Control Section ---
                        show_section_header(ui_scroll_left, "⚙️ Simulation Run", AURORA_ACCENT_TEAL);
                        create_electric_frame().show(ui_scroll_left, |ui_frame| {
                            egui::Grid::new("sim_config_grid_themed")
                                .num_columns(2)
                                .spacing([10.0, 8.0])
                                .show(ui_frame, |ui_grid| {
                                    ui_grid.label(electric_label_text("Token CA (Mint Address):"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.sim_token_mint)
                                                    .desired_width(f32::INFINITY)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Initial Buy per Wallet (SOL):"));
                                    egui::Frame::none()
                                        .fill(AURORA_BG_PANEL)
                                        .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7)))
                                        .rounding(ui_grid.style().visuals.widgets.inactive.rounding)
                                        .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                        .show(ui_grid, |cell_ui| {
                                            cell_ui.add(
                                                egui::TextEdit::singleline(&mut self.volume_bot_initial_buy_sol_input)
                                                    .desired_width(f32::INFINITY)
                                                    .text_color(AURORA_TEXT_PRIMARY)
                                                    .frame(false)
                                            );
                                        });
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Max Cycles (Buy/Sell pairs):"));
                                    ui_grid.add(egui::DragValue::new(&mut self.sim_max_cycles).range(1..=1000));
                                    ui_grid.end_row();

                                    ui_grid.label(electric_label_text("Slippage BPS (Jupiter):"));
                                    ui_grid.add(egui::DragValue::new(&mut self.sim_slippage_bps).range(0..=10000));
                                    ui_grid.end_row();
                                });
                            ui_frame.add_space(10.0);

                            if self.sim_in_progress {
                                if create_electric_button(ui_frame, "🛑 Stop Volume Bot", None).clicked() {
                                    // ... (Stop logic remains the same)
                                    self.sim_in_progress = false;
                                    if let Some(handle) = self.sim_task_handle.take() {
                                        handle.abort();
                                        self.sim_log_messages.push("Volume Bot task aborted by user.".to_string());
                                        self.last_operation_result = Some(Ok("Volume Bot stopped.".to_string()));
                                    }
                                }
                                ui_frame.horizontal(|ui_prog_sim| {ui_prog_sim.spinner(); ui_prog_sim.label(electric_label_text("Simulation Running..."));});
                            } else {
                                let can_start_sim = !self.volume_bot_wallets.is_empty() && !self.sim_token_mint.trim().is_empty();
                                let tooltip_text = if self.volume_bot_wallets.is_empty() { "Load or generate wallets first." }
                                                   else if self.sim_token_mint.trim().is_empty() { "Token CA must be specified." }
                                                   else { "Start the volume bot simulation." };
                                if create_electric_button(ui_frame, "🚀 Start Volume Bot", None).on_hover_text(tooltip_text).clicked() && can_start_sim {
                                    // ... (Start Sim logic remains the same)
                                     let initial_buy_sol = self.volume_bot_initial_buy_sol_input.parse::<f64>().unwrap_or(0.0);
                                    if initial_buy_sol <= 0.0 {
                                        self.last_operation_result = Some(Err("Initial buy SOL must be > 0".to_string()));
                                    } else {
                                        self.sim_in_progress = true;
                                        self.sim_log_messages.clear();
                                        self.sim_log_messages.push("Volume Bot start requested...".to_string());
                                        let params = VolumeBotStartParams {
                                            rpc_url: self.app_settings.solana_rpc_url.clone(),
                                            wallets_file_path: self.volume_bot_source_wallets_file.trim().to_string(),
                                            token_mint_str: self.sim_token_mint.trim().to_string(),
                                            max_cycles: self.sim_max_cycles as u32,
                                            slippage_bps: self.sim_slippage_bps,
                                            solana_priority_fee: self.app_settings.get_default_priority_fee_lamports(),
                                            initial_buy_sol_amount: initial_buy_sol,
                                            status_sender: self.sim_status_sender.clone(),
                                            wallets_data_sender: self.volume_bot_wallets_sender.clone(),
                                            use_jito: self.app_settings.use_jito_for_volume_bot,
                                            jito_be_url: self.app_settings.selected_jito_block_engine_url.clone(),
                                            jito_tip_account: self.app_settings.jito_tip_account_pk_str_volume_bot.clone(),
                                            jito_tip_lamports: self.app_settings.jito_tip_lamports_volume_bot,
                                        };
                                        self.start_volume_bot_request = Some(params);
                                        self.last_operation_result = Some(Ok("Volume Bot initiated.".to_string()));
                                    }
                                }
                            }
                        });
                        ui_scroll_left.add_space(20.0); // Bottom padding for left scroll area
                    }); // End of left column scroll area

                // --- Right Column (Logs) ---
                egui::ScrollArea::vertical()
                    .id_source("volume_bot_log_scroll_right_themed")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(&mut columns[1], |ui_scroll_right| {
                        show_section_header(ui_scroll_right, "📜 Volume Bot Activity Log", AURORA_ACCENT_MAGENTA);
                        create_electric_frame().show(ui_scroll_right, |ui_frame| {
                            if self.sim_log_messages.is_empty() {
                                ui_frame.label(electric_label_text("No log messages yet.").italics());
                            } else {
                                egui::ScrollArea::vertical().id_source("log_inner_scroll_themed").auto_shrink([false,true]).show(ui_frame, |ui_s|{
                                    for msg in &self.sim_log_messages {
                                        ui_s.label(egui::RichText::new(msg).font(FontId::monospace(12.0)));
                                    }
                                });
                            }
                        });
                    });
            }); // Closes ui_centered_columns.columns(2, |columns| { ... })
        }); // Closes ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered_columns| { ... })
    }

    // Navigation button (already defined, kept for sidebar)
//                }); // REMOVED - Stray from previous conflict resolution?
//        }); // End of ui.columns - REMOVED - Stray from previous conflict resolution?
//    } // REMOVED - Stray from previous conflict resolution?

    // Navigation button with improved visual design
    fn nav_button(ui: &mut egui::Ui, _icon_svg_path: &'static str, current_view: &mut AppView, view: AppView) {
        let is_selected = *current_view == view;

        // Define the size for the SVG icon itself
        let svg_icon_size = Vec2::new(80.0, 80.0); // Restored to original large size

        // Define the clickable area size with better proportions
        let button_widget_size = Vec2::new(80.0, 80.0); // Restored to original large size

        // Get the available width for centering
        let available_width = ui.available_width();

        // Create a frame for the button with proper styling
        let frame = egui::Frame::none()
            .fill(if is_selected {
                AURORA_ACCENT_TEAL.linear_multiply(0.15) // Very subtle background for selected state
            } else {
                Color32::TRANSPARENT // No background normally
            })
            .rounding(egui::Rounding::same(8.0)) // Rounded corners
            .stroke(if is_selected {
                Stroke::new(1.5, AURORA_ACCENT_TEAL) // Highlight border for selected
            } else {
                Stroke::NONE // No border normally
            });

        // Allocate space for the button
        let (overall_rect, response) = ui.allocate_exact_size(Vec2::new(available_width, button_widget_size.y), egui::Sense::click());

        // Center the button within the allocated space
        let visual_rect = Rect::from_center_size(
            Pos2::new(overall_rect.center().x, overall_rect.center().y),
            button_widget_size
        );

        // Draw the frame
        ui.painter().rect(
            visual_rect,
            frame.rounding,
            frame.fill,
            frame.stroke
        );

        // Determine icon color based on state
        let image_tint = if is_selected {
            AURORA_ACCENT_TEAL // Bright teal for selected
        } else if response.hovered() {
            AURORA_TEXT_PRIMARY // Full brightness on hover
        } else {
            AURORA_TEXT_PRIMARY.linear_multiply(0.75) // Slightly dimmed when inactive
        };

        // Get the correct SVG source based on view
        let image_source = match view {
            AppView::Launch => egui::include_image!("../../assets/launch.svg"),
            AppView::Sell => egui::include_image!("../../assets/sell.svg"),
            AppView::AtomicBuy => egui::include_image!("../../assets/atomic_buy.svg"),
            AppView::Disperse => egui::include_image!("../../assets/disperse.svg"),
            AppView::Gather => egui::include_image!("../../assets/gather.svg"),
            AppView::Alt => egui::include_image!("../../assets/alt.svg"),
            AppView::VolumeBot => egui::include_image!("../../assets/volume_bot.svg"),
            AppView::Check => egui::include_image!("../../assets/balance.svg"),
            AppView::Settings => egui::include_image!("../../assets/settings.svg"),
        };

        // Create the image widget
        let image_widget = egui::Image::new(image_source)
            .max_size(svg_icon_size)
            .tint(image_tint);

        // Draw the image centered within the visual_rect
        ui.allocate_ui_at_rect(visual_rect, |image_ui| {
            image_ui.centered_and_justified(|centered_image_ui| {
                centered_image_ui.add(image_widget);
            });
        });

        // Add label text below the icon
        let label_text = match view {
            AppView::Launch => "Launch",
            AppView::Sell => "Sell",
            AppView::AtomicBuy => "Atomic Buy",
            AppView::Disperse => "Disperse",
            AppView::Gather => "Gather",
            AppView::Alt => "ALT Mgmt",
            AppView::VolumeBot => "Volume Bot",
            AppView::Check => "Check",
            AppView::Settings => "Settings",
        };

        // Add the label text centered below the icon
        ui.vertical_centered(|ui| {
            ui.add(egui::Label::new(
                egui::RichText::new(label_text)
                    .size(12.0)
                    .color(if is_selected {
                        AURORA_ACCENT_TEAL
                    } else if response.hovered() {
                        AURORA_TEXT_PRIMARY
                    } else {
                        AURORA_TEXT_MUTED_TEAL
                    })
            ));
        });

        // Handle click
        if response.clicked() {
            *current_view = view;
        }

        ui.add_space(25.0); // Restored original spacing for larger buttons
    }

    // Enhanced status bar with more information and better visual design
    fn show_status_bar(&mut self, ctx: &egui::Context) {
        // Create a frame for the status bar
        let status_frame = egui::Frame::default()
            .fill(AURORA_BG_DEEP_SPACE) // Base color
            .inner_margin(egui::Margin::symmetric(10.0, 5.0)) // Add some padding
            .stroke(Stroke::new(1.0, AURORA_STROKE_GLOW_EDGE)); // Add a subtle top border

        egui::TopBottomPanel::bottom("aurora_status_bar")
            .frame(status_frame)
            .resizable(true)
            .default_height(36.0) // Slightly taller for better visibility
            .min_height(24.0) // Minimum sensible height
            .show_separator_line(false) // Hide the default separator, we have our own stroke
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Left section: Status indicator
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let status_font = FontId::new(14.0, egui::FontFamily::Proportional);

                        // Status indicator with icon
                        let (status_icon, status_text, status_color) = if let Some(Err(_)) = &self.last_operation_result {
                            ("⚠️", "Warning", AURORA_ACCENT_ORANGE)
                        } else {
                            ("✓", "Ready", AURORA_ACCENT_GREEN)
                        };

                        ui.label(egui::RichText::new(status_icon).size(16.0).color(status_color));
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(format!("Status: {}", status_text))
                            .color(status_color)
                            .font(status_font.clone()));

                        // Add RPC connection status
                        ui.add_space(15.0);
                        ui.separator();
                        ui.add_space(15.0);

                        let rpc_status_color = AURORA_ACCENT_TEAL;
                        ui.label(egui::RichText::new("🔌")
                            .color(rpc_status_color)
                            .size(14.0));
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("RPC: Connected")
                            .color(rpc_status_color)
                            .font(status_font.clone()));
                    });

                    // Center section: Last operation result
                    ui.with_layout(egui::Layout::centered_and_justified(egui::Direction::LeftToRight), |ui| {
                        let status_font = FontId::new(14.0, egui::FontFamily::Proportional);

                        if let Some(Ok(msg)) = &self.last_operation_result {
                            // Success message with animation effect (pulsing)
                            let time = ui.ctx().input(|i| i.time);
                            let pulse = 0.5 + (time.sin() * 0.5).abs() * 0.3; // Pulsing effect between 0.5 and 0.8 opacity

                            ui.label(
                                egui::RichText::new(format!("✅ {}", msg))
                                    .color(AURORA_ACCENT_GREEN.linear_multiply(pulse as f32))
                                    .font(status_font.clone())
                            );
                        } else if let Some(Err(e)) = &self.last_operation_result {
                            // Error message
                            ui.label(
                                egui::RichText::new(format!("❌ {}", e))
                                    .color(AURORA_ACCENT_MAGENTA)
                                    .font(status_font.clone())
                            );
                        }
                    });

                    // Right section: System info
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let status_font = FontId::new(14.0, egui::FontFamily::Proportional);

                        // Add current time
                        let now = chrono::Local::now();
                        let time_str = now.format("%H:%M:%S").to_string();
                        ui.label(egui::RichText::new(&time_str)
                            .color(AURORA_TEXT_PRIMARY)
                            .font(status_font.clone()));
                        ui.label(egui::RichText::new("🕒")
                            .color(AURORA_TEXT_PRIMARY)
                            .size(14.0));

                        ui.add_space(15.0);
                        ui.separator();
                        ui.add_space(15.0);

                        // Add network indicator
                        let network = "Mainnet"; // Could be dynamic based on settings
                        ui.label(egui::RichText::new(network)
                            .color(AURORA_ACCENT_NEON_BLUE)
                            .font(status_font.clone()));
                        ui.label(egui::RichText::new("🌐")
                            .color(AURORA_ACCENT_NEON_BLUE)
                            .size(14.0));
                    });
                });
            });
    }
    // All other helper methods and task functions from PumpFunApp will be inserted here by the script.
    // (e.g., trigger_individual_sell, generate_and_precalc_task, run_volume_bot_simulation_task, etc.)

    // --- Copied sell logic ---
    // This duplicated function is being removed.

    // This duplicated function block is being removed.

    // --- ALT Management View (Adapted from app.rs with Aurora Theme) ---
    fn show_alt_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::none()
            .fill(Color32::TRANSPARENT) // Show particles
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("🔧 ALT Management")
                            .font(FontId::new(32.0, egui::FontFamily::Name("DigitLoFiGui".into()))) // Changed to DigitLoFiGui
                            .color(AURORA_TEXT_PRIMARY),
                    );
                });
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(15.0);

                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui_centered| {
                    ui_centered.set_max_width(700.0); // Max width for content

                    show_section_header(ui_centered, "💡 ALT Information", AURORA_ACCENT_TEAL);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.label(electric_label_text("Create or manage Address Lookup Tables (ALTs) for efficient transactions.").italics());
                        ui_frame.label(electric_label_text("Ensure your Parent Wallet (from Settings) has SOL for creation/deactivation fees.").color(AURORA_TEXT_MUTED_TEAL));
                    });
                    ui_centered.add_space(15.0);

                    // --- Current ALT Address ---
                    show_section_header(ui_centered, "📄 Current ALT", AURORA_ACCENT_NEON_BLUE);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.horizontal_wrapped(|ui_alt_display| {
                            ui_alt_display.label(electric_label_text("Loaded ALT Address:"));
                            if self.available_alt_addresses.is_empty() || self.available_alt_addresses[0].is_empty() {
                                ui_alt_display.label(egui::RichText::new("None Loaded (alt_address.txt is empty or not found)").color(AURORA_ACCENT_MAGENTA).italics());
                            } else {
                                ui_alt_display.monospace(egui::RichText::new(self.available_alt_addresses[0].clone()).color(AURORA_TEXT_PRIMARY).strong());
                                if create_electric_button(ui_alt_display, "📋 Copy", Some(Vec2::new(60.0, ui_alt_display.available_height()))).clicked() {
                                    ui_alt_display.output_mut(|o| o.copied_text = self.available_alt_addresses[0].clone());
                                    self.last_operation_result = Some(Ok("ALT Address copied!".to_string()));
                                }
                            }
                            if create_electric_button(ui_alt_display, "🔄 Reload File", Some(Vec2::new(120.0, ui_alt_display.available_height()))).clicked() {
                                self.load_available_alts();
                                self.last_operation_result = Some(Ok("Reloaded alt_address.txt".to_string()));
                            }
                        });
                        if let Some(created_alt) = &self.alt_address { // Display if a new one was just created
                            if self.available_alt_addresses.is_empty() || self.available_alt_addresses[0] != created_alt.to_string() {
                                ui_frame.horizontal_wrapped(|ui_new_alt|{
                                    ui_new_alt.label(electric_label_text("Newly Created (unsaved):").color(AURORA_ACCENT_TEAL));
                                    ui_new_alt.monospace(egui::RichText::new(created_alt.to_string()).color(AURORA_ACCENT_TEAL).strong());
                                });
                                ui_frame.label(electric_label_text("Save this to alt_address.txt manually if you want to use it by default.").italics().color(AURORA_TEXT_MUTED_TEAL));
                            }
                        }
                    });
                    ui_centered.add_space(15.0);

                    // --- ALT Creation Section ---
                    show_section_header(ui_centered, "➕ Create New ALT", AURORA_ACCENT_PURPLE);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.label(electric_label_text("Step 1: Generate a new Mint Keypair and precalculate addresses for the ALT."));
                        if self.alt_precalc_in_progress {
                            ui_frame.horizontal(|ui_h_spinner| { ui_h_spinner.spinner(); ui_h_spinner.label(electric_label_text("Precalculating addresses...")); });
                        } else {
                            if create_electric_button(ui_frame, "Generate & Precalculate", Some(egui::vec2(ui_frame.available_width(), 35.0))).clicked() {
                                self.alt_view_status = "Generating & Precalculating...".to_string();
                                self.alt_log_messages.push(self.alt_view_status.clone());
                                self.alt_precalc_in_progress = true;
                                self.alt_generated_mint_pubkey = None;
                                self.alt_precalc_addresses.clear();
                                self.alt_address = None; // Clear previously created ALT for this session

                                let sender = self.alt_precalc_result_sender.clone();
                                let parent_pk_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                let zombie_pks: Vec<Pubkey> = self.loaded_wallet_data.iter()
                                    .filter_map(|lw| Pubkey::from_str(&lw.public_key).ok())
                                    .collect();
                                let settings_clone = self.app_settings.clone();
                                tokio::spawn(async move {
                                    Self::generate_and_precalc_task(parent_pk_str_opt, zombie_pks, settings_clone, sender).await;
                                });
                            }
                        }

                        if let Some(mint_pk) = &self.alt_generated_mint_pubkey {
                            ui_frame.add_space(5.0);
                            ui_frame.label(electric_label_text(&format!("Generated Mint for Precalc: {}", mint_pk)));
                            ui_frame.label(electric_label_text(&format!("Found {} addresses for ALT.", self.alt_precalc_addresses.len())));
                            if !self.alt_precalc_addresses.is_empty() {
                                ui_frame.add_space(5.0);
                                ui_frame.label(electric_label_text("Step 2: Create the ALT on-chain with these addresses."));
                                if self.alt_creation_in_progress {
                                    ui_frame.horizontal(|ui_h_spinner| { ui_h_spinner.spinner(); ui_h_spinner.label(electric_label_text("Creating ALT on-chain...")); });
                                } else {
                                    if create_electric_button(ui_frame, "Create ALT Now", Some(egui::vec2(ui_frame.available_width(), 35.0))).clicked() {
                                        self.alt_view_status = "Creating ALT...".to_string();
                                        self.alt_log_messages.push(self.alt_view_status.clone());
                                        self.alt_creation_in_progress = true;
                                        let rpc_url = self.app_settings.solana_rpc_url.clone();
                                        let parent_key = self.app_settings.parent_wallet_private_key.clone();
                                        let addresses = self.alt_precalc_addresses.clone();
                                        let sender_status = self.alt_creation_status_sender.clone();
                                        tokio::spawn(async move {
                                            Self::create_alt_task(rpc_url, parent_key, addresses, sender_status).await;
                                        });
                                    }
                                }
                            }
                        }
                    });
                    ui_centered.add_space(15.0);

                    // --- ALT Deactivation/Close Section ---
                    show_section_header(ui_centered, "🗑️ Deactivate & Close ALT", AURORA_ACCENT_MAGENTA);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        ui_frame.label(electric_label_text("Deactivate and close an existing ALT. Use with caution. The ALT address from alt_address.txt will be targeted if loaded."));
                        ui_frame.add_space(5.0);
                        ui_frame.label(electric_label_text(&format!("Status: {}", self.alt_deactivation_status_message)));

                        let alt_to_deactivate_str = self.available_alt_addresses.get(0).cloned().unwrap_or_default();
                        let can_deactivate = !alt_to_deactivate_str.is_empty() && !self.app_settings.parent_wallet_private_key.is_empty() && !self.alt_deactivation_in_progress;

                        let hover_text = if alt_to_deactivate_str.is_empty() {
                            "Load an ALT first via alt_address.txt".to_string()
                        } else {
                            format!("Target ALT: {}", alt_to_deactivate_str)
                        };

                        if create_electric_button(ui_frame, &format!("Deactivate & Close: {}", if alt_to_deactivate_str.is_empty() { "(No ALT Loaded)" } else { &alt_to_deactivate_str[..10] }), None)
                            .on_hover_text(&hover_text)
                            .clicked() && can_deactivate {
                            if let Ok(alt_pubkey) = Pubkey::from_str(&alt_to_deactivate_str) {
                                self.alt_deactivation_in_progress = true;
                                self.alt_deactivation_status_message = format!("Initiating deactivation for {}", alt_to_deactivate_str);
                                self.alt_log_messages.push(self.alt_deactivation_status_message.clone());
                                let rpc_url = self.app_settings.solana_rpc_url.clone();
                                let parent_key = self.app_settings.parent_wallet_private_key.clone();
                                let sender = self.alt_deactivate_status_sender.clone();
                                tokio::spawn(async move {
                                    Self::deactivate_and_close_alt_task(rpc_url, alt_pubkey, parent_key, sender).await;
                                });
                            } else {
                                self.alt_deactivation_status_message = "Invalid ALT address loaded.".to_string();
                                self.alt_log_messages.push("Error: Attempted to deactivate an invalid ALT address.".to_string());
                            }
                        }
                        if self.alt_deactivation_in_progress {
                            ui_frame.horizontal(|ui_h_spinner| { ui_h_spinner.spinner(); ui_h_spinner.label(electric_label_text("Deactivation in progress...")); });
                        }
                    });
                    ui_centered.add_space(15.0);

                    // --- Log Display ---
                    show_section_header(ui_centered, "📊 ALT Logs", AURORA_TEXT_GLOW);
                    create_electric_frame().show(ui_centered, |ui_frame| {
                        egui::ScrollArea::vertical().id_source("alt_log_scroll_modern").max_height(150.0).stick_to_bottom(true).show(ui_frame, |ui_log_scroll| {
                            for msg in &self.alt_log_messages {
                                ui_log_scroll.label(egui::RichText::new(msg).font(FontId::monospace(12.0)));
                            }
                        });
                    });
                    ui_centered.add_space(20.0);
                });
            });
    }

    // --- ALT Task 1: Generate Key & Precalculate Addresses ---
    async fn generate_and_precalc_task(
        parent_pubkey_str_option: Option<String>,
        zombie_pubkeys: Vec<Pubkey>,
        settings: AppSettings,
        sender: UnboundedSender<PrecalcResult>,
    ) {
        let _ = sender.send(PrecalcResult::Log("Starting key generation and address precalculation...".to_string()));
        log::info!("Starting key generation and address precalculation...");

        let task_result = async {
            let _ = sender.send(PrecalcResult::Log("Generating new mint keypair...".to_string()));
            let mint_keypair = Keypair::new();
            let mint_pubkey = mint_keypair.pubkey();
            let log_msg_mint_generated = format!("Generated new Mint Keypair. Pubkey: {}", mint_pubkey);
            let _ = sender.send(PrecalcResult::Log(log_msg_mint_generated.clone()));
            log::info!("{}", log_msg_mint_generated);

            let keypair_filename = format!("mint_{}.json", mint_pubkey);
            let log_msg_saving_kp = format!("Saving generated mint keypair to: {}", keypair_filename);
            let _ = sender.send(PrecalcResult::Log(log_msg_saving_kp.clone()));
            log::info!("{}", log_msg_saving_kp);

            let keypair_bytes = mint_keypair.to_bytes().to_vec();
            let keypair_json = serde_json::to_string(&keypair_bytes)
                .map_err(|e| anyhow!("Failed to serialize generated keypair: {}", e))?;

            let keypair_filename_clone = keypair_filename.clone(); // Clone for capture
            tokio::task::spawn_blocking(move || { // Removed sender from capture
                std::fs::write(&keypair_filename_clone, keypair_json)
            }).await.map_err(|e| anyhow!("Failed to join spawn_blocking for keypair write: {}", e))?
                 .map_err(|e| anyhow!("Failed to write keypair file {}: {}", keypair_filename, e))?;

            let log_msg_kp_saved = "✅ Mint keypair saved successfully.".to_string();
            let _ = sender.send(PrecalcResult::Log(log_msg_kp_saved.clone()));
            log::info!("{}", log_msg_kp_saved);

            let _ = sender.send(PrecalcResult::Log("Resolving payer pubkey...".to_string()));
            let payer_pubkey = match parent_pubkey_str_option {
                Some(ref pk_str) => Pubkey::from_str(pk_str)
                    .map_err(|e| anyhow!("Invalid Parent Pubkey String provided: {}", e))?,
                None => return Err(anyhow!("Parent pubkey not available (is Parent Wallet Private Key set in settings?)")),
            };
            let log_msg_payer_resolved = format!("Using Payer Pubkey: {}", payer_pubkey);
            let _ = sender.send(PrecalcResult::Log(log_msg_payer_resolved.clone()));
            log::debug!("{}", log_msg_payer_resolved);
            let _ = sender.send(PrecalcResult::Log(format!("Using {} Zombie Pubkeys.", zombie_pubkeys.len())));
            log::debug!("Using {} Zombie Pubkeys.", zombie_pubkeys.len());

            let _ = sender.send(PrecalcResult::Log("Resolving minter pubkey...".to_string()));
            let minter_pubkey = AppSettings::get_pubkey_from_privkey_str(&settings.dev_wallet_private_key)
                .and_then(|s| Pubkey::from_str(&s).ok())
                .ok_or_else(|| anyhow!("Failed to load/parse Dev Wallet Private Key from settings."))?;
            let log_msg_minter_resolved = format!("Using Minter Pubkey: {}", minter_pubkey);
            let _ = sender.send(PrecalcResult::Log(log_msg_minter_resolved.clone()));
            log::info!("{}", log_msg_minter_resolved);

            let _ = sender.send(PrecalcResult::Log("Finding PDAs and ATAs...".to_string()));
            let (bonding_curve_pubkey, _) = find_bonding_curve_pda(&mint_pubkey)
                .map_err(|e| anyhow!("Failed to find bonding curve PDA: {}", e))?;
            let (metadata_pubkey, _) = find_metadata_pda(&mint_pubkey)
                .map_err(|e| anyhow!("Failed to find metadata PDA: {}", e))?;

            let bonding_curve_vault_ata = get_associated_token_address(&bonding_curve_pubkey, &mint_pubkey);
            let payer_ata = get_associated_token_address(&payer_pubkey, &mint_pubkey);
            let minter_ata = get_associated_token_address(&minter_pubkey, &mint_pubkey);
            let zombie_atas: Vec<Pubkey> = zombie_pubkeys.iter()
                .map(|zombie_pk| get_associated_token_address(zombie_pk, &mint_pubkey))
                .collect();
            let _ = sender.send(PrecalcResult::Log("PDAs and ATAs found.".to_string()));

            let _ = sender.send(PrecalcResult::Log("Collecting all addresses for ALT...".to_string()));
            let mut all_addresses = HashSet::new();
            all_addresses.insert(PUMPFUN_PROGRAM_ID);
            all_addresses.insert(spl_token::ID);
            all_addresses.insert(spl_associated_token_account::ID);
            all_addresses.insert(system_program::ID);
            all_addresses.insert(sysvar::rent::ID);
            all_addresses.insert(compute_budget::ID);
            all_addresses.insert(METADATA_PROGRAM_ID);
            all_addresses.insert(GLOBAL_STATE_PUBKEY);
            all_addresses.insert(PUMPFUN_MINT_AUTHORITY);
            all_addresses.insert(FEE_RECIPIENT_PUBKEY);
            all_addresses.insert(EVENT_AUTHORITY_PUBKEY);
            all_addresses.insert(bonding_curve_pubkey);
            all_addresses.insert(metadata_pubkey);
            all_addresses.insert(payer_pubkey);
            all_addresses.insert(payer_ata);
            all_addresses.insert(minter_pubkey);
            all_addresses.insert(minter_ata);
            all_addresses.insert(bonding_curve_vault_ata);
            for (zombie_pk, zombie_ata) in zombie_pubkeys.iter().zip(zombie_atas.iter()) {
                all_addresses.insert(*zombie_pk);
                all_addresses.insert(*zombie_ata);
            }
            if let Ok(tip_acc_pk) = settings.get_jito_tip_account_pubkey() {
                all_addresses.insert(tip_acc_pk);
            }
            all_addresses.insert(mint_pubkey);
            let sorted_addresses: Vec<Pubkey> = all_addresses.into_iter().collect();

            let log_msg_precalc_done = format!("Precalculated {} unique addresses.", sorted_addresses.len());
            let _ = sender.send(PrecalcResult::Log(log_msg_precalc_done.clone()));
            log::info!("{}", log_msg_precalc_done);
            Ok((mint_pubkey.to_string(), sorted_addresses))
        }.await;

        match task_result {
            Ok((mint_pk_str, addresses)) => {
                let _ = sender.send(PrecalcResult::Success(mint_pk_str, addresses));
            }
            Err(e) => {
                let err_msg = format!("ALT Precalculation task failed: {}", e);
                log::error!("{}", err_msg);
                let _ = sender.send(PrecalcResult::Log(err_msg.clone())); // Send error as a log too
                let _ = sender.send(PrecalcResult::Failure(e.to_string()));
            }
        }
    }

    async fn create_alt_task(
        rpc_url: String,
        parent_private_key_str: String,
        addresses_to_add: Vec<Pubkey>,
        sender: UnboundedSender<AltCreationStatus>
    ) {
        // Log entry into the task immediately
        log::info!("CREATE_ALT_TASK: Entered for {} addresses.", addresses_to_add.len());
        let _ = sender.send(AltCreationStatus::LogMessage(format!("CREATE_ALT_TASK: Entered for {} addresses.", addresses_to_add.len())));

        // Then send Starting, which should update the main status
        let _ = sender.send(AltCreationStatus::Starting);
        let _ = sender.send(AltCreationStatus::LogMessage("CREATE_ALT_TASK: 'Starting' status sent.".to_string()));
        // This log was redundant with the one above, keeping the more specific one.
        // log::info!("Starting ALT creation task with {} addresses.", addresses_to_add.len());

        let task_result: Result<Pubkey, anyhow::Error> = async {
            const MAX_STEP_ATTEMPTS: usize = 3;
            let _ = sender.send(AltCreationStatus::LogMessage("Validating parent private key...".to_string()));
            if parent_private_key_str.is_empty() {
                let err_msg = "Parent wallet private key is empty in settings".to_string();
                let _ = sender.send(AltCreationStatus::LogMessage(format!("[ERROR] {}", err_msg)));
                return Err(anyhow!(err_msg));
            }
            let _ = sender.send(AltCreationStatus::LogMessage("Decoding parent private key...".to_string()));
            let pk_bytes = bs58::decode(&parent_private_key_str).into_vec()
                .map_err(|e| {
                    let err_msg = format!("Invalid base58 parent key: {}", e);
                    let _ = sender.send(AltCreationStatus::LogMessage(format!("[ERROR] {}", err_msg)));
                    anyhow!(err_msg)
                })?;
            let _ = sender.send(AltCreationStatus::LogMessage("Creating payer keypair...".to_string()));
            let payer = Keypair::from_bytes(&pk_bytes)
                .map_err(|e| {
                    let err_msg = format!("Failed to create parent Keypair: {}", e);
                    let _ = sender.send(AltCreationStatus::LogMessage(format!("[ERROR] {}", err_msg)));
                    anyhow!(err_msg)
                })?;
            let payer_pubkey = payer.pubkey();
            let _ = sender.send(AltCreationStatus::LogMessage(format!("Payer pubkey: {}", payer_pubkey)));

            let _ = sender.send(AltCreationStatus::LogMessage(format!("Initializing RPC client for: {}", rpc_url)));
            let client = AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

            let _ = sender.send(AltCreationStatus::LogMessage("Fetching recent slot...".to_string()));
            let recent_slot = client.get_slot_with_commitment(CommitmentConfig::confirmed()).await
                .map_err(|e| {
                    let err_msg = format!("Failed to get recent slot: {}", e);
                    let _ = sender.send(AltCreationStatus::LogMessage(format!("[ERROR] {}", err_msg)));
                    anyhow!(err_msg)
                })?;
            let _ = sender.send(AltCreationStatus::LogMessage(format!("Recent slot: {}. Creating lookup table instruction...", recent_slot)));
            let (lookup_table_ix, lookup_table_address) = alt_instruction::create_lookup_table(payer_pubkey, payer_pubkey, recent_slot);
            let log_msg_derived_alt = format!("Derived ALT Address: {}", lookup_table_address);
            let _ = sender.send(AltCreationStatus::LogMessage(log_msg_derived_alt.clone()));
            log::info!("{}", log_msg_derived_alt);

            let _ = sender.send(AltCreationStatus::LogMessage("Loading app settings for fees...".to_string()));
            let app_settings_loaded = AppSettings::load(); // Load once
            let priority_fee = app_settings_loaded.get_default_priority_fee_lamports();
            let compute_limit = 1_400_000u32; // Default, consider making configurable
            let _ = sender.send(AltCreationStatus::LogMessage(format!("Using Priority Fee: {}, Compute Limit: {}", priority_fee, compute_limit)));
            let delay_between_actions = Duration::from_millis(1500); // Renamed for clarity
            let retry_delay = Duration::from_secs(5);

            fn is_unrecoverable_alt_error(error_message: &str) -> bool {
                error_message.contains("Attempt to debit an account but found no record of a prior credit") ||
                error_message.contains("insufficient lamports") // Another common non-recoverable by simple retry
                // Add other specific error strings here if needed
            }

            // 1. Create Lookup Table
            let create_label = "Create ALT".to_string();
            let mut create_attempts = 0;
            loop {
                create_attempts += 1;
                let _ = sender.send(AltCreationStatus::Sending(format!("{} (Attempt {}/{})", create_label, create_attempts, MAX_STEP_ATTEMPTS)));

                match Self::send_alt_tx_internal(&client, &payer, lookup_table_ix.clone(), &create_label, priority_fee, compute_limit).await {
                    Ok((sig, logs)) => {
                        for log_msg in logs { let _ = sender.send(AltCreationStatus::LogMessage(log_msg)); }
                        let _ = sender.send(AltCreationStatus::Confirmed(create_label.clone(), sig));
                        break; // Success, exit loop for this step
                    }
                    Err((e, logs)) => {
                        let error_msg_str = format!("{}", e);
                        for log_msg in logs { let _ = sender.send(AltCreationStatus::LogMessage(log_msg)); }
                        let _ = sender.send(AltCreationStatus::Failure(format!("Attempt {}/{} for {} failed: {}", create_attempts, MAX_STEP_ATTEMPTS, create_label, error_msg_str)));

                        if is_unrecoverable_alt_error(&error_msg_str) || create_attempts >= MAX_STEP_ATTEMPTS {
                            let final_error_msg = format!("Failed to {} after {} attempts due to: {}. Halting task.", create_label, create_attempts, error_msg_str);
                            let _ = sender.send(AltCreationStatus::LogMessage(final_error_msg.clone()));
                            return Err(anyhow!(final_error_msg));
                        }
                        tokio::time::sleep(retry_delay).await;
                    }
                }
            }
            sleep(delay_between_actions).await;

            // 2. Extend Lookup Table
            let extend_label_base = "Extend ALT";
            let num_extend_batches = (addresses_to_add.len() + MAX_ADDRESSES_PER_EXTEND - 1) / MAX_ADDRESSES_PER_EXTEND;
            for (i, chunk) in addresses_to_add.chunks(MAX_ADDRESSES_PER_EXTEND).enumerate() {
                let extend_label = format!("{} Batch {}/{}", extend_label_base, i + 1, num_extend_batches);
                let _ = sender.send(AltCreationStatus::LogMessage(format!("Preparing to {} with {} addresses", extend_label, chunk.len())));
                let mut extend_attempts = 0;
                loop {
                    extend_attempts += 1;
                    let _ = sender.send(AltCreationStatus::Sending(format!("{} (Attempt {}/{})", extend_label, extend_attempts, MAX_STEP_ATTEMPTS)));

                    let extend_ix = alt_instruction::extend_lookup_table(lookup_table_address, payer_pubkey, Some(payer_pubkey), chunk.to_vec());
                    match Self::send_alt_tx_internal(&client, &payer, extend_ix.clone(), &extend_label, priority_fee, compute_limit).await {
                        Ok((sig, logs)) => {
                            for log_msg in logs { let _ = sender.send(AltCreationStatus::LogMessage(log_msg)); }
                            let _ = sender.send(AltCreationStatus::Confirmed(extend_label.clone(), sig));
                            break; // Success for this chunk
                        }
                        Err((e, logs)) => {
                            let error_msg_str = format!("{}", e);
                            for log_msg in logs { let _ = sender.send(AltCreationStatus::LogMessage(log_msg)); }
                            let _ = sender.send(AltCreationStatus::Failure(format!("Attempt {}/{} for {} failed: {}", extend_attempts, MAX_STEP_ATTEMPTS, extend_label, error_msg_str)));

                            if is_unrecoverable_alt_error(&error_msg_str) || extend_attempts >= MAX_STEP_ATTEMPTS {
                                let final_error_msg = format!("Failed to {} after {} attempts due to: {}. Halting task.", extend_label, extend_attempts, error_msg_str);
                                let _ = sender.send(AltCreationStatus::LogMessage(final_error_msg.clone()));
                                return Err(anyhow!(final_error_msg));
                            }
                            tokio::time::sleep(retry_delay).await;
                        }
                    }
                }
                if i < num_extend_batches - 1 { // Check if it's not the last batch
                    let _ = sender.send(AltCreationStatus::LogMessage(format!("Waiting before next extend batch ({}ms)...", delay_between_actions.as_millis())));
                    sleep(delay_between_actions).await;
                }
            }
            log::info!("All Extend ALT instructions confirmed.");

            // 3. Freeze Lookup Table (Assuming this stage exists and follows a similar pattern)
            let freeze_label = "Freeze ALT".to_string();
            let mut freeze_attempts = 0;
            loop {
                freeze_attempts += 1;
                let _ = sender.send(AltCreationStatus::Sending(format!("{} (Attempt {}/{})", freeze_label, freeze_attempts, MAX_STEP_ATTEMPTS)));

                let freeze_ix = alt_instruction::freeze_lookup_table(lookup_table_address, payer_pubkey);
                match Self::send_alt_tx_internal(&client, &payer, freeze_ix.clone(), &freeze_label, priority_fee, compute_limit).await {
                    Ok((sig, logs)) => {
                        for log_msg in logs { let _ = sender.send(AltCreationStatus::LogMessage(log_msg)); }
                        let _ = sender.send(AltCreationStatus::Confirmed(freeze_label.clone(), sig));
                        break; // Success
                    }
                    Err((e, logs)) => {
                        let error_msg_str = format!("{}", e);
                        for log_msg in logs { let _ = sender.send(AltCreationStatus::LogMessage(log_msg)); }
                        let _ = sender.send(AltCreationStatus::Failure(format!("Attempt {}/{} for {} failed: {}", freeze_attempts, MAX_STEP_ATTEMPTS, freeze_label, error_msg_str)));

                        if is_unrecoverable_alt_error(&error_msg_str) || freeze_attempts >= MAX_STEP_ATTEMPTS {
                            let final_error_msg = format!("Failed to {} after {} attempts due to: {}. Halting task.", freeze_label, freeze_attempts, error_msg_str);
                            let _ = sender.send(AltCreationStatus::LogMessage(final_error_msg.clone()));
                            return Err(anyhow!(final_error_msg));
                        }
                        tokio::time::sleep(retry_delay).await;
                    }
                }
            }
            log::info!("Freeze ALT instruction confirmed.");

            let file_path = "alt_address.txt"; // Consider making this configurable or part of settings
            let mut file = std::fs::File::create(file_path).map_err(|e| anyhow!("Failed to create file {}: {}", file_path, e))?;
            file.write_all(lookup_table_address.to_string().as_bytes()).map_err(|e| anyhow!("Failed to write to file {}: {}", file_path, e))?;
            log::info!("Successfully saved ALT address to {}.", file_path);
            let _ = sender.send(AltCreationStatus::LogMessage(format!("Successfully saved ALT address {} to {}.", lookup_table_address, file_path)));

            Ok(lookup_table_address)
        }.await;

        match task_result {
            Ok(alt_address) => { let _ = sender.send(AltCreationStatus::Success(alt_address)); }
            Err(e) => {
                log::error!("ALT Creation task ultimately failed: {}", e);
                // The specific failure message would have been sent already by the failing step.
                // Here we send a general task failure.
                let _ = sender.send(AltCreationStatus::Failure(format!("ALT creation process failed: {}", e)));
            }
        }
    }
    async fn deactivate_and_close_alt_task(
        rpc_url: String,
        alt_address: Pubkey,
        authority_private_key_str: String,
        status_sender: UnboundedSender<Result<String, String>>
    ) {
        let _ = status_sender.send(Ok(format!("Starting deactivation & close for ALT: {}", alt_address)));
        log::info!("Starting deactivation & close task for ALT: {}", alt_address);
        let task_result: Result<(), anyhow::Error> = async {
            if authority_private_key_str.is_empty() {
                return Err(anyhow!("Authority private key is empty."));
            }
            let pk_bytes = bs58::decode(&authority_private_key_str).into_vec()
                .map_err(|e| anyhow!("Invalid base58 authority key: {}", e))?;
            let authority = Keypair::from_bytes(&pk_bytes)
                .map_err(|e| anyhow!("Failed to create authority Keypair: {}", e))?;
            let client = AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

            let priority_fee = AppSettings::load().get_default_priority_fee_lamports();
            let compute_limit = 200_000u32;

            let deactivate_label = format!("Deactivate ALT {}", alt_address);
            let _ = status_sender.send(Ok(format!("Attempting: {}", deactivate_label)));
            let deactivate_ix = alt_instruction::deactivate_lookup_table(alt_address, authority.pubkey());
            match Self::send_alt_tx_internal(&client, &authority, deactivate_ix, &deactivate_label, priority_fee, compute_limit).await {
                Ok((sig, logs)) => {
                    for log_msg in logs { let _ = status_sender.send(Ok(log_msg)); }
                    let _ = status_sender.send(Ok(format!("Deactivation sent for {}: {}", alt_address, sig)));
                }
                Err((e, logs)) => {
                    for log_msg in logs { let _ = status_sender.send(Ok(log_msg)); } // Send logs even on error
                    let _ = status_sender.send(Err(format!("Failed deactivation for {}: {}", alt_address, e)));
                    // Optionally return Err here if this failure should stop the whole deactivate_and_close_alt_task
                }
            }
            let _ = status_sender.send(Ok(format!("Waiting after deactivation attempt for {}...", alt_address)));
            tokio::time::sleep(Duration::from_secs(10)).await;

            let close_label = format!("Close ALT {}", alt_address);
            let _ = status_sender.send(Ok(format!("Attempting: {}", close_label)));
            let close_ix = alt_instruction::close_lookup_table(alt_address, authority.pubkey(), authority.pubkey());
            match Self::send_alt_tx_internal(&client, &authority, close_ix, &close_label, priority_fee, compute_limit).await {
                Ok((sig, logs)) => {
                    for log_msg in logs { let _ = status_sender.send(Ok(log_msg)); }
                    let _ = status_sender.send(Ok(format!("Close sent for {}: {}", alt_address, sig)));
                    Ok(())
                }
                Err((e, logs)) => {
                    for log_msg in logs { let _ = status_sender.send(Ok(log_msg)); }
                    Err(anyhow!(format!("Failed close for {}: {} (Error: {})", alt_address, e, e)))
                }
            }
        }.await;
        match task_result {
            Ok(_) => { let _ = status_sender.send(Ok(format!("Deactivation/Close process done for {}. Check explorer.", alt_address))); }
            Err(e) => { let _ = status_sender.send(Err(format!("ALT Deactivation/Close task failed: {}", e))); }
        }
    }

    async fn send_alt_tx_internal(
        client: &AsyncRpcClient,
        payer: &Keypair,
        instruction: solana_sdk::instruction::Instruction,
        label: &str,
        priority_fee_microlamports: u64,
        compute_unit_limit: u32,
    ) -> Result<(solana_sdk::signature::Signature, Vec<String>), (anyhow::Error, Vec<String>)> {
        const MAX_RETRIES: u8 = 3;
        const RETRY_DELAY: Duration = Duration::from_secs(2);
        let mut logs: Vec<String> = Vec::new();
        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 0..MAX_RETRIES {
            let attempt_num = attempt + 1;
            logs.push(format!(
                "{}: Attempt {}/{}...",
                label, attempt_num, MAX_RETRIES
            ));
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }
            let current_blockhash = match client.get_latest_blockhash().await {
                Ok(bh) => bh,
                Err(e) => {
                    let msg = format!(
                        "Blockhash fetch failed for {} (attempt {}): {}",
                        label, attempt_num, e
                    );
                    logs.push(msg.clone());
                    last_error = Some(anyhow!(msg));
                    continue;
                }
            };
            logs.push(format!(
                "{}: Got blockhash {} (attempt {})",
                label, current_blockhash, attempt_num
            ));

            let priority_fee_ix = compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports);
            let compute_limit_ix = compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit);

            let message = match MessageV0::try_compile(
                &payer.pubkey(),
                &[compute_limit_ix, priority_fee_ix, instruction.clone()],
                &[],
                current_blockhash,
            ) {
                Ok(msg) => VersionedMessage::V0(msg),
                Err(e) => {
                    let compile_err_msg = format!("Compile V0 msg failed for {}: {}", label, e);
                    logs.push(compile_err_msg.clone());
                    return Err((anyhow!(compile_err_msg), logs));
                }
            };

            let tx = match VersionedTransaction::try_new(message, &[payer]) {
                Ok(signed_tx) => signed_tx,
                Err(e) => {
                    let sign_err_msg = format!("Sign {} tx failed: {}", label, e);
                    logs.push(sign_err_msg.clone());
                    return Err((anyhow!(sign_err_msg), logs));
                }
            };

            logs.push(format!(
                "{}: Sending transaction (attempt {})...",
                label, attempt_num
            ));
            match client.send_and_confirm_transaction_with_spinner(&tx).await {
                Ok(sig) => {
                    logs.push(format!(
                        "{}: Transaction confirmed (attempt {})! Signature: {}",
                        label, attempt_num, sig
                    ));
                    return Ok((sig, logs));
                }
                Err(e) => {
                    let error_string = e.to_string();
                    let err_msg = format!(
                        "{}: Send/Confirm failed (attempt {}): {}",
                        label, attempt_num, error_string
                    );
                    logs.push(err_msg.clone());
                    last_error = Some(anyhow!(err_msg)); // Use the detailed message for anyhow
                    let is_recoverable = error_string.contains("blockhash not found")
                        || error_string.contains("timed out")
                        || error_string.contains("Unable to confirm transaction");
                    if !is_recoverable {
                        logs.push(format!("{}: Non-recoverable error, stopping retries.", label));
                        break;
                    }
                }
            }
        }
        let final_err_msg = format!("{} failed after {} attempts", label, MAX_RETRIES);
        logs.push(final_err_msg.clone());
// --- Mix Wallets Task ---
        Err((last_error.unwrap_or_else(|| anyhow!(final_err_msg)), logs))
    }

async fn calculate_pnl_task(
        rpc_url: String,
        user_wallet_address_str: String,
        token_mint_str: String,
    ) -> Result<PnlSummary, String> {
        info!("Starting PnL calculation for token: {} on wallet: {}", token_mint_str, user_wallet_address_str);
        let rpc_client = AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

        let user_wallet_pubkey = Pubkey::from_str(&user_wallet_address_str)
            .map_err(|e| format!("Invalid user wallet address for PnL: {} - {}", user_wallet_address_str, e))?;

        let token_mint_pubkey = Pubkey::from_str(&token_mint_str)
            .map_err(|e| format!("Invalid token mint address for PnL: {} - {}", token_mint_str, e))?;

        // Fetch transaction history
        match get_transaction_history_for_token(&rpc_client, &user_wallet_pubkey, &token_mint_pubkey, Some(1000), None, None).await {
            Ok(transactions) => {
                if transactions.is_empty() {
                    info!("No transactions found for PnL calculation (token: {}, wallet: {}).", token_mint_str, user_wallet_address_str);
                     return Ok(PnlSummary { token_mint: Some(token_mint_pubkey), ..Default::default() });
                }
                info!("Fetched {} transactions for PnL calculation (token: {}, wallet: {}). Calculating PnL...", transactions.len(), token_mint_str, user_wallet_address_str);
                match calculate_pnl(&transactions, &token_mint_pubkey) {
                    Ok(summary) => {
                        info!("PnL calculation successful for token: {}", token_mint_str);
                        Ok(summary)
                    }
                    Err(e) => {
                        error!("Error calculating PnL for token {}: {}", token_mint_str, e);
                        Err(format!("Error calculating PnL: {}", e))
                    }
                }
            }
            Err(e) => {
                error!("Error fetching transaction history for PnL (token: {}): {}", token_mint_str, e);
                Err(format!("Error fetching transaction history: {}", e))
            }
        }
    }

    pub async fn run_mix_wallets_task(
        rpc_url: String,
        original_wallets_data: Vec<LoadedWalletInfo>,
        status_sender: UnboundedSender<Result<String, String>>,
        minter_pubkey_to_exclude: Option<String>,
        parent_pubkey_to_exclude: Option<String>,
    ) -> Result<(), anyhow::Error> {
        info!("Starting Mix Wallets Task. Original wallet count: {}.", original_wallets_data.len());

        let wallets_to_process = original_wallets_data.into_iter().filter(|wallet| {
            let mut exclude = false;
            if let Some(ref minter_pk_str) = minter_pubkey_to_exclude {
                if wallet.public_key == *minter_pk_str {
                    info!("Excluding dev/minter wallet from mix: {}", wallet.public_key);
                    let _ = status_sender.send(Ok(format!("INFO: Excluding dev/minter wallet {} from mix.", wallet.public_key)));
                    exclude = true;
                }
            }
            if !exclude { // Only check parent if not already excluded by minter
                if let Some(ref parent_pk_str) = parent_pubkey_to_exclude {
                    if wallet.public_key == *parent_pk_str {
                        info!("Excluding parent wallet from mix: {}", wallet.public_key);
                        let _ = status_sender.send(Ok(format!("INFO: Excluding parent wallet {} from mix.", wallet.public_key)));
                        exclude = true;
                    }
                }
            }
            !exclude
        }).collect::<Vec<LoadedWalletInfo>>();

        info!("Wallets to process after exclusion: {}.", wallets_to_process.len());
        let _ = status_sender.send(Ok(format!("🌪️ Mix Wallets Task Started for {} filtered wallets.", wallets_to_process.len())));

        if wallets_to_process.is_empty() {
            let err_msg = "No wallets remaining to mix after excluding dev/parent wallets.".to_string();
            warn!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg.clone()));
            return Err(anyhow!(err_msg));
        }

        let rpc_client = Arc::new(AsyncRpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed()));
        let mut new_mixed_wallets_to_save: Vec<LoadedWalletInfo> = Vec::new();
        let mut keypair_map: Vec<(Keypair, Keypair)> = Vec::new();

        for original_wallet_loaded_info in &wallets_to_process { // Iterate over filtered list
            let _ = status_sender.send(Ok(format!("Processing original wallet: {}", original_wallet_loaded_info.public_key)));

            let original_keypair = match crate::commands::utils::load_keypair_from_string(&original_wallet_loaded_info.private_key, "MixOriginalWallet") {
                Ok(kp) => kp,
                Err(e) => {
                    let err_msg = format!("Failed to load original keypair for {}: {}. Skipping this wallet.", original_wallet_loaded_info.public_key, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg.clone()));
                    continue;
                }
            };

            let new_keypair = Keypair::new();
            let new_pubkey_str = new_keypair.pubkey().to_string();
            let new_privkey_str = bs58::encode(new_keypair.to_bytes()).into_string();

            new_mixed_wallets_to_save.push(LoadedWalletInfo {
                name: Some(format!("Mixed_{}", &original_wallet_loaded_info.public_key[0..std::cmp::min(8, original_wallet_loaded_info.public_key.len())])),
                public_key: new_pubkey_str.clone(),
                private_key: new_privkey_str,
            });
            keypair_map.push((original_keypair, new_keypair));
            let _ = status_sender.send(Ok(format!("Generated new wallet {} for original {}", new_pubkey_str, original_wallet_loaded_info.public_key)));
        }

        // This entire async fn retry_fund_mixed_from_zombies_task block was incorrectly placed.
        // It will be moved after the run_mix_wallets_task's closing brace.
        // For now, this diff removes it from its current incorrect position.

        if !new_mixed_wallets_to_save.is_empty() {
            let timestamp = Utc::now().format("%Y%m%d_%H%M%S").to_string();
            let filename = format!("mixed_wallets_{}.json", timestamp);
            match File::create(&filename) {
                Ok(file) => {
                    let writer = BufWriter::new(file);
                    let wallets_to_save_formatted = WalletFileFormat { wallets: new_mixed_wallets_to_save.clone() }; // Clone if new_mixed_wallets_to_save is used later, otherwise can move.
                    match serde_json::to_writer_pretty(writer, &wallets_to_save_formatted) {
                        Ok(_) => {
                            info!("Successfully saved {} new mixed wallets to {}", new_mixed_wallets_to_save.len(), filename);
                            let _ = status_sender.send(Ok(format!("✅ Saved {} new mixed wallets to {}", new_mixed_wallets_to_save.len(), filename)));
                        }
                        Err(e) => {
                            let err_msg = format!("Failed to serialize new mixed wallets to {}: {}", filename, e);
                            error!("{}", err_msg);
                            let _ = status_sender.send(Err(err_msg.clone()));
                        }
                    }
                }
                Err(e) => {
                    let err_msg = format!("Failed to create file {}: {}", filename, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg.clone()));
                }
            }
        } else if keypair_map.is_empty() {
            let err_msg = "No new wallets were generated (all original wallets might have had errors). Halting mix.".to_string();
            warn!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg.clone()));
            return Err(anyhow!(err_msg));
        }

        let mut successful_transfers = 0;
        let mut failed_transfers = 0;

        for (original_kp, new_kp) in keypair_map {
            let original_pubkey = original_kp.pubkey();
            let new_pubkey = new_kp.pubkey();
            let _ = status_sender.send(Ok(format!("Initiating SOL transfer from {} to {}", original_pubkey, new_pubkey)));

            match rpc_client.get_balance(&original_pubkey).await {
                Ok(balance_lamports) => {
                    if balance_lamports == 0 {
                        let _ = status_sender.send(Ok(format!("ℹ️ Wallet {} has 0 SOL. Skipping transfer.", original_pubkey)));
                        continue;
                    }

                    const TRANSACTION_FEE_LAMPORTS: u64 = 5000;
                    if balance_lamports <= TRANSACTION_FEE_LAMPORTS {
                        let _ = status_sender.send(Err(format!("❌ Wallet {} has insufficient balance ({:.6} SOL) to cover transaction fee. Skipping transfer.", original_pubkey, lamports_to_sol(balance_lamports))));
                        failed_transfers += 1;
                        continue;
                    }

                    let amount_to_transfer = balance_lamports - TRANSACTION_FEE_LAMPORTS;

                    let instruction = system_instruction::transfer(&original_pubkey, &new_pubkey, amount_to_transfer);
                    let latest_blockhash = match rpc_client.get_latest_blockhash().await {
                        Ok(bh) => bh,
                        Err(e) => {
                            let err_msg = format!("Failed to get blockhash for transfer from {}: {}", original_pubkey, e);
                            error!("{}", err_msg);
                            let _ = status_sender.send(Err(err_msg));
                            failed_transfers += 1;
                            continue;
                        }
                    };

                    let message = MessageV0::try_compile(&original_pubkey, &[instruction], &[], latest_blockhash)
                        .map_err(|e| anyhow!("Failed to compile message for transfer from {}: {}", original_pubkey, e))?;

                    let transaction = VersionedTransaction::try_new(VersionedMessage::V0(message), &[&original_kp])
                         .map_err(|e| anyhow!("Failed to sign transaction for transfer from {}: {}", original_pubkey, e))?;

                    match utils::transaction::sign_and_send_versioned_transaction(rpc_client.as_ref(), transaction, &[&original_kp]).await {
                        Ok(signature) => {
                            let _ = status_sender.send(Ok(format!("✅ Transfer from {} to {} successful ({:.6} SOL). Sig: {}", original_pubkey, new_pubkey, lamports_to_sol(amount_to_transfer), signature)));
                            successful_transfers += 1;
                        }
                        Err(e) => {
                            let err_msg = format!("❌ Transfer from {} to {} failed: {}", original_pubkey, new_pubkey, e);
                            error!("{}", err_msg);
                            let _ = status_sender.send(Err(err_msg));
                            failed_transfers += 1;
                        }
                    }
                }
                Err(e) => {
                    let err_msg = format!("Failed to get balance for original wallet {}: {}", original_pubkey, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg));
                    failed_transfers += 1;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let final_message = format!("🏁 Mix Wallets Task Finished. Successful transfers: {}. Failed transfers: {}.", successful_transfers, failed_transfers);
        info!("{}", final_message);
        let _ = status_sender.send(Ok(final_message));
        Ok(())
    }

    // Moved async fn retry_fund_mixed_from_zombies_task to be here:
    async fn retry_fund_mixed_from_zombies_task(
        rpc_url: String,
        mixed_wallets_file_path: String,
        zombie_wallets_data: Vec<LoadedWalletInfo>, // Available "zombie" wallets
        sol_to_send_per_mixed_wallet: f64,
        priority_fee_lamports: u64,
        status_sender: UnboundedSender<Result<String, String>>,
    ) -> Result<(), anyhow::Error> {
        info!("Starting Retry Fund Mixed Wallets Task. Mixed file: {}, Zombies available: {}, SOL per mixed: {}",
            mixed_wallets_file_path, zombie_wallets_data.len(), sol_to_send_per_mixed_wallet);
        let _ = status_sender.send(Ok(format!("🔄 Retry Funding Task Started. Mixed Wallets File: {}. Zombies: {}. SOL per: {}",
            mixed_wallets_file_path, zombie_wallets_data.len(), sol_to_send_per_mixed_wallet)));

        let rpc_client = Arc::new(AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed()));
        let lamports_to_send = sol_to_lamports(sol_to_send_per_mixed_wallet);

        if lamports_to_send == 0 {
            let msg = "Amount of SOL to send per mixed wallet is 0. No transfers will be made.".to_string();
            warn!("{}", msg);
            let _ = status_sender.send(Ok(msg));
            return Ok(());
        }

        // 1. Load mixed wallets from the specified file
        let mixed_wallets_content = match std::fs::read_to_string(&mixed_wallets_file_path) {
            Ok(content) => content,
            Err(e) => {
                let err_msg = format!("Failed to read mixed wallets file {}: {}", mixed_wallets_file_path, e);
                error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg.clone()));
                return Err(anyhow!(err_msg));
            }
        };

        let mixed_wallets_to_fund: Vec<LoadedWalletInfo> = match serde_json::from_str(&mixed_wallets_content) {
            Ok(wallets) => wallets,
            Err(e) => {
                let pk_list: Vec<String> = mixed_wallets_content.lines().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                if !pk_list.is_empty() {
                    info!("Parsed mixed wallets file as a list of private keys ({} found).", pk_list.len());
                    pk_list.into_iter().map(|pk_str| {
                        let derived_pubkey = AppSettings::get_pubkey_from_privkey_str(&pk_str).unwrap_or_else(|| "INVALID_PK".to_string());
                        LoadedWalletInfo { public_key: derived_pubkey, private_key: pk_str, name: Some("Mixed Wallet (from list)".to_string()) }
                    }).collect()
                } else {
                    let err_msg = format!("Failed to parse mixed wallets file {} as JSON array of WalletInfo or list of private keys: {}", mixed_wallets_file_path, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg.clone()));
                    return Err(anyhow!(err_msg));
                }
            }
        };

        if mixed_wallets_to_fund.is_empty() {
            let msg = "No mixed wallets found in the file to fund.".to_string();
            warn!("{}", msg);
            let _ = status_sender.send(Ok(msg));
            return Ok(());
        }
        info!("Loaded {} mixed wallets to attempt funding.", mixed_wallets_to_fund.len());

        let available_zombies = zombie_wallets_data.into_iter().filter_map(|zw| {
            bs58::decode(&zw.private_key).into_vec().ok().and_then(|bytes| Keypair::from_bytes(&bytes).ok()).map(|kp| (kp, zw.public_key))
        }).collect::<Vec<(Keypair, String)>>();

        if available_zombies.is_empty() {
            let msg = "No valid zombie wallets available to source funds from.".to_string();
            warn!("{}", msg);
            let _ = status_sender.send(Err(msg));
            return Ok(());
        }

        let mut funded_count = 0;
        let mut funding_errors = 0;

        for (i, mixed_wallet_info) in mixed_wallets_to_fund.iter().enumerate() {
            let mixed_wallet_pubkey = match Pubkey::from_str(&mixed_wallet_info.public_key) {
                Ok(pk) => pk,
                Err(e) => {
                    let err_msg = format!("Invalid public key for mixed wallet {}: {}. Skipping.", mixed_wallet_info.public_key, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg));
                    funding_errors += 1;
                    continue;
                }
            };
            let _ = status_sender.send(Ok(format!("Attempting to fund mixed wallet {}/{} ({})...", i + 1, mixed_wallets_to_fund.len(), mixed_wallet_pubkey)));

            let mut funded_this_mixed_wallet = false;
            let mut attempt_count = 0;
            const MAX_ZOMBIE_ATTEMPTS_PER_MIXED: usize = 5;

            for zombie_idx in 0..available_zombies.len() {
                if attempt_count >= MAX_ZOMBIE_ATTEMPTS_PER_MIXED {
                    let _ = status_sender.send(Err(format!("Max zombie attempts reached for mixed wallet {}. Skipping.", mixed_wallet_pubkey)));
                    break;
                }
                attempt_count += 1;

                let (zombie_keypair, _zombie_pubkey_str) = &available_zombies[zombie_idx]; // _zombie_pubkey_str not used directly
                let zombie_pubkey = zombie_keypair.pubkey();

                match rpc_client.get_balance(&zombie_pubkey).await {
                    Ok(balance) => {
                        let fee_estimate = 5000 + priority_fee_lamports;
                        if balance > lamports_to_send + fee_estimate {
                            let _ = status_sender.send(Ok(format!("Found zombie {} with {} SOL to fund {}.", zombie_pubkey, lamports_to_sol(balance), mixed_wallet_pubkey)));

                            let instructions = vec![
                                compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(200_000),
                                compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports * 5),
                                system_instruction::transfer(&zombie_pubkey, &mixed_wallet_pubkey, lamports_to_send),
                            ];
                            let latest_blockhash = match rpc_client.get_latest_blockhash().await {
                                Ok(h) => h,
                                Err(e) => {
                                    let err_msg = format!("Failed to get blockhash for transfer from {}: {}", zombie_pubkey, e);
                                    error!("{}", err_msg);
                                    let _ = status_sender.send(Err(err_msg));
                                    continue;
                                }
                            };
                            let message = VersionedMessage::V0(MessageV0::try_compile(&zombie_pubkey, &instructions, &[], latest_blockhash).unwrap());
                            let transaction = VersionedTransaction::try_new(message, &[zombie_keypair]).unwrap(); // Removed mut

                            match rpc_client.send_and_confirm_transaction_with_spinner(&transaction).await {
                                Ok(signature) => {
                                    let msg = format!("✅ Successfully funded mixed wallet {} from zombie {} with {} SOL. Sig: {}", mixed_wallet_pubkey, zombie_pubkey, sol_to_send_per_mixed_wallet, signature);
                                    info!("{}", msg);
                                    let _ = status_sender.send(Ok(msg));
                                    funded_count += 1;
                                    funded_this_mixed_wallet = true;
                                    break;
                                }
                                Err(e) => {
                                    let err_msg = format!("❌ Failed to send {} SOL from zombie {} to mixed {}: {}", sol_to_send_per_mixed_wallet, zombie_pubkey, mixed_wallet_pubkey, e);
                                    error!("{}", err_msg);
                                    let _ = status_sender.send(Err(err_msg));
                                }
                            }
                        } else {
                             let _ = status_sender.send(Ok(format!("Zombie {} has insufficient balance ({} SOL) to send {} SOL. Trying next.", zombie_pubkey, lamports_to_sol(balance), sol_to_send_per_mixed_wallet)));
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("Failed to get balance for zombie {}: {}", zombie_pubkey, e);
                        error!("{}", err_msg);
                        let _ = status_sender.send(Err(err_msg));
                    }
                }
                if funded_this_mixed_wallet { break; }
            }

            if !funded_this_mixed_wallet {
                let _ = status_sender.send(Err(format!("Could not find a suitable zombie with enough funds for mixed wallet {}.", mixed_wallet_pubkey)));
                funding_errors +=1;
            }
             tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let final_msg = format!("🏁 Retry Funding Task Finished. Successfully funded: {}/{}. Errors: {}", funded_count, mixed_wallets_to_fund.len(), funding_errors);
        info!("{}", final_msg);
        let _ = status_sender.send(Ok(final_msg));
        Ok(())
    }
} // Closing brace for impl ModernApp

// --- Standalone Async Tasks (copied from app.rs, may need path adjustments for crate:: items) ---
// Note: These functions are now outside `impl ModernApp`. If they need `self`,
// they must be converted to methods or take `ModernApp`'s relevant state as parameters.

pub(crate) async fn fetch_balances_task(
    rpc_url: String,
    wallets_to_check: Vec<String>,
    target_mint_str: String,
    sender: UnboundedSender<FetchResult>,
) {
    let client = Arc::new(AsyncRpcClient::new(rpc_url));
    let target_mint_pubkey: Option<Pubkey> = if target_mint_str.is_empty() { None } else { Pubkey::from_str(&target_mint_str).ok() };
    let fetches = futures::stream::iter(wallets_to_check.into_iter().map(|address_str| {
        let client_clone = Arc::clone(&client);
        let sender_clone = sender.clone();
        let target_mint_pk_clone = target_mint_pubkey.clone();
        async move {
            let address_pubkey_result = Pubkey::from_str(&address_str);
            let result: Result<(f64, Option<f64>, Option<String>), anyhow::Error> = async { // Added Option<f64> for WSOL
                let address_pubkey = address_pubkey_result.map_err(|e| anyhow!("Invalid address {}: {}", address_str, e))?;

                // Fetch SOL balance
                let sol_balance_lamports = client_clone.get_balance(&address_pubkey).await.context(format!("Failed to get SOL balance for {}", address_str))?;
                let sol_balance = lamports_to_sol(sol_balance_lamports);

                // Fetch WSOL balance
                let wsol_mint_pubkey = spl_token::native_mint::ID;
                let wsol_ata_pubkey = spl_associated_token_account::get_associated_token_address(&address_pubkey, &wsol_mint_pubkey);
                let wsol_balance_f64: Option<f64> = match client_clone.get_token_account_balance(&wsol_ata_pubkey).await {
                    Ok(ui_token_amount) => ui_token_amount.ui_amount, // This is already Option<f64> or f64, ensure it's Option<f64>
                    Err(_e) => { // If ATA not found or other error, treat as 0 or None
                        Some(0.0) // Or None if you prefer to distinguish no account vs zero balance
                    }
                };

                // Fetch Target Mint balance
                let mut target_mint_balance_str: Option<String> = None;
                if let Some(mint_pk) = target_mint_pk_clone {
                    let ata_pubkey = spl_associated_token_account::get_associated_token_address(&address_pubkey, &mint_pk);
                    match client_clone.get_token_account_balance(&ata_pubkey).await {
                        Ok(ui_token_amount) => target_mint_balance_str = Some(ui_token_amount.ui_amount_string),
                        Err(e) => {
                            if e.to_string().contains("AccountNotFound") || e.to_string().contains("could not find account") {
                                target_mint_balance_str = Some("0".to_string());
                            } else {
                                warn!("Failed to get target token balance for ATA {} (Owner: {}): {}", ata_pubkey, address_str, e);
                                target_mint_balance_str = None; // Keep as None on other errors
                            }
                        }
                    }
                }
                Ok((sol_balance, wsol_balance_f64, target_mint_balance_str))
            }.await;
            let fetch_result = match result {
                Ok((sol, wsol, token)) => FetchResult::Success { address: address_str.clone(), sol_balance: sol, wsol_balance: wsol, target_mint_balance: token },
                Err(e) => FetchResult::Failure { address: address_str.clone(), error: e.to_string() },
            };
            if let Err(e) = sender_clone.send(fetch_result) { error!("Failed to send balance result for {}: {}", address_str, e); }
        }
    }));
    fetches.for_each_concurrent(10, |fut| fut).await;
}

async fn load_volume_wallets_from_file_task(
    wallets_file_path: String,
    wallets_data_sender: UnboundedSender<Vec<LoadedWalletInfo>>,
    status_sender: UnboundedSender<Result<String, String>>,
) {
    info!("Load Wallets Task: Attempting to load wallets from {}", wallets_file_path);
    if wallets_file_path.trim().is_empty() {
        let _ = status_sender.send(Err("Wallet file path is empty.".to_string())); return;
    }
    match File::open(&wallets_file_path) {
        Ok(file) => {
            let reader = std::io::BufReader::new(file);
            match serde_json::from_reader(reader) {
                Ok(loaded_wallets_vec) => {
                    let wallets_to_send: Vec<LoadedWalletInfo> = loaded_wallets_vec;
                    let msg = format!("Successfully loaded {} wallets from {}.", wallets_to_send.len(), wallets_file_path);
                    info!("{}", msg);
                    let _ = status_sender.send(Ok(msg));
                    if wallets_data_sender.send(wallets_to_send).is_err() { error!("Failed to send loaded wallets to main app."); }
                }
                Err(e) => { let _ = status_sender.send(Err(format!("Failed to parse wallets file {}: {}", wallets_file_path, e))); }
            }
        }
        Err(e) => { let _ = status_sender.send(Err(format!("Failed to open wallets file {}: {}", wallets_file_path, e))); }
    };
}

async fn gather_sol_from_zombie_task(
    rpc_url: String,
    zombie_wallet_data: LoadedWalletInfo, // Use the aliased struct with private key
    recipient_pubkey: Pubkey,          // Parent wallet pubkey
    sender: UnboundedSender<Result<String, String>>,
) {
    let zombie_pubkey_str = zombie_wallet_data.public_key;
    let zombie_privkey_str = zombie_wallet_data.private_key;
    let zombie_name = zombie_wallet_data.name.unwrap_or_else(|| format!("{}...", &zombie_pubkey_str[..6]));

    log::info!("Starting SOL gather task for zombie: {} ({})", zombie_name, zombie_pubkey_str);

    let task_result = async {
        // Load zombie keypair
        let zombie_pk_bytes = bs58::decode(&zombie_privkey_str).into_vec()
            .map_err(|e| anyhow!("Invalid base58 format for zombie {}: {}", zombie_pubkey_str, e))?;
        let zombie_keypair = Keypair::from_bytes(&zombie_pk_bytes)
             .map_err(|e| anyhow!("Failed to create Keypair for zombie {}: {}", zombie_pubkey_str, e))?;
        let zombie_pubkey = zombie_keypair.pubkey();
        if zombie_pubkey.to_string() != zombie_pubkey_str { // Sanity check
            return Err(anyhow!("Private key does not match public key for zombie {}", zombie_pubkey_str));
        }

        // Create Async RPC client
        let client = Arc::new(AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed()));

        // --- Simplified SOL Transfer Logic ---

        // --- Step 1: Get Balance ---
        let balance = client.get_balance(&zombie_pubkey).await?;
        log::info!("Zombie {} current balance: {}", zombie_name, balance);

        // If balance is zero or very low, skip further processing
        if balance <= 5000 { // Keep 0.000005 SOL threshold
            log::info!("Zombie {} balance is too low ({}), skipping transfer.", zombie_name, balance);
            // Return Ok with a skip message
            return Ok::<String, anyhow::Error>(format!("Skipped {}: Balance {} <= 5000 lamports", zombie_name, balance));
        }

        // --- Step 2: Get Blockhash and Estimate Fee ---
        let latest_blockhash = client.get_latest_blockhash().await?;
        // log::debug!("Got blockhash {} for {}", latest_blockhash, zombie_name); // Less verbose

        // Create a dummy transfer instruction just for fee calculation
        let dummy_instruction = system_instruction::transfer(&zombie_pubkey, &recipient_pubkey, 1); // Amount doesn't matter
        let fee_calc_message = MessageV0::try_compile(&zombie_pubkey, &[dummy_instruction], &[], latest_blockhash)?;
        let fee = match client.get_fee_for_message(&fee_calc_message).await {
            Ok(f) => f,
            Err(e) => {
                log::warn!("Failed to get fee for {}: {}. Using default 5000 lamports.", zombie_name, e);
                5000 // Default fee
            }
        };
        log::info!("Zombie {} estimated transfer fee: {}", zombie_name, fee);

        // --- Step 3: Calculate Amount and Send Transaction ---
        if balance <= fee {
             log::info!("Skipping transfer for {}: Balance ({}) <= fee ({})", zombie_name, balance, fee);
             return Ok::<String, anyhow::Error>(format!("Skipped {}: Balance {} <= fee {}", zombie_name, balance, fee));
        }

        let lamports_to_send = balance - fee;
        log::info!("Calculated transfer amount for {}: {}", zombie_name, lamports_to_send);

        let transfer_instruction = system_instruction::transfer(
            &zombie_pubkey,      // From zombie
            &recipient_pubkey,   // To parent
            lamports_to_send,    // Final calculated amount
        );

        let transaction = Transaction::new_signed_with_payer(
            &[transfer_instruction], // Only the transfer instruction
            Some(&zombie_pubkey),    // Zombie pays the fee
            &[&zombie_keypair],      // Zombie signs
            latest_blockhash,
        );
        // log::debug!("Built simple transfer transaction for {}", zombie_name); // Less verbose

        // Send and Confirm
        let signature = client.send_and_confirm_transaction_with_spinner(&transaction).await?;
        log::info!("Successfully transferred SOL from {}: {}", zombie_name, signature);

        Ok(format!(
            "Transferred {:.9} SOL from {} ({})",
            lamports_to_send as f64 / 1_000_000_000.0,
            zombie_name,
            signature
        ))
    }.await;

    // Send final result back to main thread
    match task_result {
        Ok(msg) => {
            let _ = sender.send(Ok(msg));
        }
        Err(e) => {
             log::error!("Gather task failed for zombie {}: {}", zombie_name, e);
            // Include zombie name/pubkey in error message sent back
            let _ = sender.send(Err(format!("Failed for {}: {}", zombie_name, e)));
        }
    }
}
// ... (Other standalone async tasks: gather_all_funds_from_volume_wallets_task, disperse_sol_task, gather_sol_from_zombie_task, etc.)
// ... (generate_and_precalc_task, create_alt_task, deactivate_and_close_alt_task, send_alt_tx_internal)
// ... (run_volume_bot_simulation_task, fund_volume_bot_wallets_task, distribute_total_sol_to_volume_wallets_task)
// The full bodies of these functions from app.rs would be here.
// For brevity in this diff, I'm omitting their full bodies but they should be copied.

// Placeholder for the many task functions that would be copied here.
// It's crucial that these are copied correctly from app.rs

async fn disperse_sol_task(
    rpc_url: String,
    payer_keypair: Keypair, // Take ownership again for 'static lifetime
    recipient_addresses: Vec<String>,
    amount_lamports_each: u64,
    sender: UnboundedSender<Result<String, String>>,
) {
    info!("Starting SOL dispersal task...");
    let client = Arc::new(AsyncRpcClient::new(rpc_url)); // Use Async client
    let payer_pubkey = payer_keypair.pubkey();
    // Removed: let mut instructions = Vec::new(); // Instructions will be built per batch
    let mut recipients_processed_overall = 0; // Track total valid recipients processed

    // Validate addresses and count valid ones *before* batching
    let valid_recipient_pubkeys: Vec<Pubkey> = recipient_addresses.iter()
        .filter_map(|addr_str| {
            match Pubkey::from_str(addr_str) {
                Ok(recipient_pubkey) => {
                    if recipient_pubkey == payer_pubkey {
                        warn!("Dispersal: Skipping transfer to self (payer address): {}", payer_pubkey);
                        None
                    } else {
                        recipients_processed_overall += 1;
                        Some(recipient_pubkey)
                    }
                }
                Err(e) => {
                    warn!("Skipping invalid recipient address '{}': {}", addr_str, e);
                    None
                }
            }
        })
        .collect();

    if valid_recipient_pubkeys.is_empty() {
        let _ = sender.send(Err("No valid (non-payer) recipient addresses found.".to_string()));
        return;
    }

    info!("Attempting to disperse to {} valid recipients.", recipients_processed_overall);

    let task_future = async {
        let mut batch_results: Vec<Result<String, String>> = Vec::new();
        let mut successful_tx_count_total_wallets = 0; // Counts wallets, not txs
        let mut failed_tx_count_total_wallets = 0;    // Counts wallets, not txs

        let batch_size = 15; // Max transfers per tx (Solana limit is around 17-20 system transfers)
        let num_batches = (valid_recipient_pubkeys.len() + batch_size - 1) / batch_size;
        let delay_between_batches = Duration::from_millis(500); // 0.5s delay between sending batches

        for (batch_index, chunk_of_pubkeys) in valid_recipient_pubkeys.chunks(batch_size).enumerate() {
            log::info!("Processing batch {}/{} ({} wallets)...", batch_index + 1, num_batches, chunk_of_pubkeys.len());

            let instructions_batch: Vec<_> = chunk_of_pubkeys.iter()
                .map(|target_pubkey| {
                    system_instruction::transfer(
                        &payer_pubkey,
                        target_pubkey,
                        amount_lamports_each,
                    )
                })
                .collect();

            if instructions_batch.is_empty() {
                log::warn!("Batch {}/{} had no instructions to send (should not happen if valid_recipient_pubkeys was not empty).", batch_index + 1, num_batches);
                continue;
            }

            let batch_tx_result = async {
                let latest_blockhash = client.get_latest_blockhash().await?;
                log::debug!("Got blockhash {} for batch {}", latest_blockhash, batch_index + 1);

                // Create a new transaction for each batch
                // The payer_keypair is captured by the outer async block and available here
                let tx = Transaction::new_signed_with_payer(
                    &instructions_batch,
                    Some(&payer_pubkey),
                    &[&payer_keypair], // Payer signs
                    latest_blockhash,
                );
                log::debug!("Batch {} transaction signed.", batch_index + 1);

                let signature = client.send_and_confirm_transaction_with_spinner(&tx).await?;
                log::info!("Batch {}/{} confirmed! Signature: {}", batch_index + 1, num_batches, signature);
                Ok::<String, anyhow::Error>(signature.to_string())
            }.await;

            match batch_tx_result {
                Ok(sig) => {
                    successful_tx_count_total_wallets += chunk_of_pubkeys.len(); // All wallets in this batch are considered successful
                    batch_results.push(Ok(format!("Batch {}/{} ({} wallets) OK: {}", batch_index + 1, num_batches, chunk_of_pubkeys.len(), sig)));
                }
                Err(e) => {
                    failed_tx_count_total_wallets += chunk_of_pubkeys.len(); // All wallets in this batch are considered failed
                     log::error!("Batch {}/{} failed: {}", batch_index + 1, num_batches, e);
                     batch_results.push(Err(format!("Batch {} ({} wallets) failed: {}", batch_index + 1, chunk_of_pubkeys.len(), e)));
                }
            }

            if batch_index < num_batches - 1 {
                log::debug!("Waiting {:?} before next batch...", delay_between_batches);
                sleep(delay_between_batches).await;
            }
        }

         let final_message = format!(
             "Dispersal finished. Sent to ~{} wallets successfully, ~{} failed (across {} batches).",
             successful_tx_count_total_wallets, failed_tx_count_total_wallets, num_batches
         );
        log::info!("{}", final_message);
        // Also send each batch result to UI for more granular logging if desired
        for res in batch_results {
            match res {
                Ok(msg) => { let _ = sender.send(Ok(msg)); },
                Err(emsg) => { let _ = sender.send(Err(emsg)); },
            }
        }

        if failed_tx_count_total_wallets > 0 {
            // The individual batch errors were already sent. This is an overall summary.
            Err(anyhow!("{}", final_message)) // Return an error if any batch failed for the overall task status
        } else {
            Ok(final_message) // Return success if all batches were successful
        }
    };

    let task_result = task_future.await;

    match task_result {
        Ok(summary) => {
            let _ = sender.send(Ok(summary)); // Send final summary
        }
        Err(e) => {
             log::error!("Dispersal task failed overall: {}", e);
            // The error message from anyhow already contains the summary string.
            let _ = sender.send(Err(format!("Dispersal failed: {}", e)));
        }
    }
}

async fn gather_all_funds_and_sell_task(
    rpc_url: String,
    parent_private_key_str: String,
    volume_wallets: Vec<LoadedWalletInfo>,
    token_mint_ca_str: String,
    slippage_percent: f64, // Added for selling
    priority_fee_lamports: u64, // Added for selling
    status_sender: UnboundedSender<Result<String, String>>,
) {
    info!("Starting Gather All Funds & Sell task. Token: {}, Parent Sell Slippage: {}%, Prio Fee: {}", token_mint_ca_str, slippage_percent, priority_fee_lamports);
    let _ = status_sender.send(Ok("🚀 Starting Gather All Funds & Sell operation...".to_string()));

    let client = Arc::new(AsyncRpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed()));
    let _http_client = ReqwestClient::new(); // For Jupiter quotes if needed for sell

    // 1. Load Parent Keypair
    let parent_keypair = match crate::commands::utils::load_keypair_from_string(&parent_private_key_str, "GatherParent") {
        Ok(kp) => kp,
        Err(e) => {
            let _ = status_sender.send(Err(format!("❌ Failed to load parent keypair: {}", e)));
            return;
        }
    };
    let parent_pubkey = parent_keypair.pubkey();
    let _ = status_sender.send(Ok(format!("🔑 Parent wallet loaded: {}", parent_pubkey)));

    // 2. Parse Token Mint Pubkey
    let token_mint_pubkey = match Pubkey::from_str(&token_mint_ca_str) {
        Ok(pk) => pk,
        Err(e) => {
            let _ = status_sender.send(Err(format!("❌ Invalid token mint address {}: {}", token_mint_ca_str, e)));
            return;
        }
    };
    let _ = status_sender.send(Ok(format!("🪙 Token mint: {}", token_mint_pubkey)));
    let parent_token_ata = get_associated_token_address(&parent_pubkey, &token_mint_pubkey);

    // --- Pre-step: Ensure Parent ATA for the token exists ---
    let _ = status_sender.send(Ok(format!("ℹ️ Checking/Creating Parent ATA {} for token {}...", parent_token_ata, token_mint_pubkey)));
    match client.get_account(&parent_token_ata).await {
        Ok(_) => {
            let _ = status_sender.send(Ok(format!("✅ Parent ATA {} already exists.", parent_token_ata)));
        }
        Err(_) => { // Account not found, try to create it
            let _ = status_sender.send(Ok(format!("ℹ️ Parent ATA {} not found. Attempting to create...", parent_token_ata))); // Test comment
            let create_ata_ix = spl_associated_token_account::instruction::create_associated_token_account(
                &parent_pubkey, // Payer
                &parent_pubkey, // Wallet address for the new ATA
                &token_mint_pubkey,
                &spl_token::ID, // Token program ID
            );
            let instructions = vec![
                compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(50_000), // Typical for ATA creation
                compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports),
                create_ata_ix,
            ];
            match client.get_latest_blockhash().await {
                Ok(latest_blockhash) => {
                    let tx = Transaction::new_signed_with_payer(
                        &instructions,
                        Some(&parent_pubkey),
                        &[&parent_keypair], // Parent pays for its own ATA
                        latest_blockhash,
                    );
                    match client.send_and_confirm_transaction_with_spinner(&tx).await {
                        Ok(signature) => {
                            let _ = status_sender.send(Ok(format!("✅ Parent ATA {} created successfully. Sig: {}", parent_token_ata, signature)));
                        }
                        Err(e) => {
                            let _ = status_sender.send(Err(format!("❌ Failed to create Parent ATA {}: {}. Halting operation.", parent_token_ata, e)));
                            return; // Critical failure, cannot proceed
                        }
                    }
                }
                Err(e) => {
                    let _ = status_sender.send(Err(format!("❌ Failed to get blockhash for Parent ATA creation: {}. Halting operation.", e)));
                    return; // Critical failure
                }
            }
        }
    }
    // End Pre-step

    // --- 3. Gather Tokens ---
    let _ = status_sender.send(Ok("💰 Phase 1: Gathering tokens to parent wallet...".to_string()));
    let mut tokens_gathered_success_count = 0;
    let mut tokens_gathered_failure_count = 0;
    // parent_token_ata is already defined above

    for wallet_info in &volume_wallets {
        if wallet_info.public_key == parent_pubkey.to_string() {
            info!("Skipping token gather from parent wallet itself: {}", wallet_info.public_key);
            continue;
        }
        let _ = status_sender.send(Ok(format!("⏳ Gathering tokens from {}...", wallet_info.public_key)));
        let source_wallet_keypair = match crate::commands::utils::load_keypair_from_string(&wallet_info.private_key, "GatherVolumeWallet") {
            Ok(kp) => kp,
            Err(e) => {
                let _ = status_sender.send(Err(format!("❌ Failed to load keypair for {}: {}", wallet_info.public_key, e)));
                tokens_gathered_failure_count += 1;
                continue;
            }
        };
        let source_wallet_pubkey = source_wallet_keypair.pubkey();
        let source_token_ata = get_associated_token_address(&source_wallet_pubkey, &token_mint_pubkey);

        match client.get_token_account_balance(&source_token_ata).await {
            Ok(balance_response) => {
                match balance_response.amount.parse::<u64>() {
                    Ok(token_balance) if token_balance > 0 => {
                        let mut instructions = vec![
                            compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(200_000), // Base limit for transfer
                            compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee_lamports), // Use main priority fee for consistency
                        ];
                        // Check if parent ATA needs creation (simplified: assume it exists or transfer handles it)
                        // If not, add create ATA instruction for parent_token_ata, with payer = source_wallet_pubkey

                        instructions.push(spl_token::instruction::transfer(
                            &spl_token::ID,
                            &source_token_ata,
                            &parent_token_ata,
                            &source_wallet_pubkey,
                            &[],
                            token_balance,
                        ).unwrap()); // Assuming unwrap is safe here for valid accounts

                        let _ = status_sender.send(Ok(format!("⏳ [{}] TokenGather: Getting latest blockhash...", wallet_info.public_key)));
                        match client.get_latest_blockhash().await {
                            Ok(latest_blockhash) => {
                                let _ = status_sender.send(Ok(format!("ℹ️ [{}] TokenGather: Got blockhash {}. Creating tx...", wallet_info.public_key, latest_blockhash)));
                                let tx = Transaction::new_signed_with_payer(
                                    &instructions,
                                    Some(&source_wallet_pubkey),
                                    &[&source_wallet_keypair],
                                    latest_blockhash,
                                );
                                let _ = status_sender.send(Ok(format!("⏳ [{}] TokenGather: Tx created. Sending...", wallet_info.public_key)));
                                match client.send_and_confirm_transaction_with_spinner(&tx).await {
                                    Ok(signature) => {
                                        let _ = status_sender.send(Ok(format!("✅ Tokens gathered from {}. Sig: {}", wallet_info.public_key, signature)));
                                        tokens_gathered_success_count += 1;
                                    }
                                    Err(e) => {
                                        let _ = status_sender.send(Err(format!("❌ Token gather TX failed for {}: {}", wallet_info.public_key, e)));
                                        tokens_gathered_failure_count += 1;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = status_sender.send(Err(format!("❌ Failed to get blockhash for token gather from {}: {}", wallet_info.public_key, e)));
                                tokens_gathered_failure_count += 1;
                            }
                        }
                    }
                    Ok(_) => { // Balance is 0
                        let _ = status_sender.send(Ok(format!("ℹ️ No tokens to gather from {}.", wallet_info.public_key)));
                    }
                    Err(e) => { // Failed to parse balance
                        let _ = status_sender.send(Err(format!("❌ Failed to parse token balance for {}: {}", wallet_info.public_key, e)));
                        tokens_gathered_failure_count += 1;
                    }
                }
            }
            Err(_e) => { // Failed to get token account balance (e.g., ATA doesn't exist)
                let _ = status_sender.send(Ok(format!("ℹ️ No token account or 0 balance for {} (Token: {}). Skipping.", wallet_info.public_key, token_mint_ca_str)));
            }
        }
        sleep(Duration::from_millis(200)).await; // Small delay
    }
    let _ = status_sender.send(Ok(format!("🏁 Token Gathering Phase Complete. Success: {}, Fail: {}", tokens_gathered_success_count, tokens_gathered_failure_count)));

    // --- 4. Gather SOL ---
    // --- 4. Gather SOL (Revised Logic) ---
    // --- 4. Gather SOL (EXACTLY like gather_sol_from_zombie_task: dynamic fee estimation for simple transfer) ---
    let _ = status_sender.send(Ok("💰 Phase 2: Gathering SOL to parent wallet (Dynamic Fee Estimation for Simple Transfer)...".to_string()));
    let mut sol_gathered_success_count = 0;
    let mut sol_gathered_failure_count = 0;
    // BASE_SOL_TRANSFER_FEE is not used here; fee is dynamically estimated.

    for wallet_info in &volume_wallets {
        let current_wallet_pk_str = wallet_info.public_key.clone();
        if current_wallet_pk_str == parent_pubkey.to_string() {
            info!("Skipping SOL gather from parent wallet itself: {}", current_wallet_pk_str);
            let _ = status_sender.send(Ok(format!("ℹ️ Skipping SOL gather from parent wallet itself: {}", current_wallet_pk_str)));
            continue;
        }

        let _ = status_sender.send(Ok(format!("⏳ Checking SOL for {}:", current_wallet_pk_str)));
        let source_wallet_keypair = match crate::commands::utils::load_keypair_from_string(&wallet_info.private_key, "GatherSOLVolumeWalletZero") {
            Ok(kp) => kp,
            Err(e) => {
                let _ = status_sender.send(Err(format!("❌ Failed to load keypair for SOL gather from {}: {}", current_wallet_pk_str, e)));
                sol_gathered_failure_count += 1;
                continue;
            }
        };
        let source_wallet_pubkey = source_wallet_keypair.pubkey();

        match client.get_balance(&source_wallet_pubkey).await {
            Ok(balance_lamports) => {
                let _ = status_sender.send(Ok(format!("ℹ️ [{}] SOLGather: Current balance: {} lamports.", current_wallet_pk_str, balance_lamports)));

                if balance_lamports <= 5000 { // Minimum threshold to attempt anything (covers base fee)
                    let _ = status_sender.send(Ok(format!("ℹ️ [{}] SOLGather: Balance {} <= 5000 lamports. Skipping.", current_wallet_pk_str, balance_lamports)));
                    // continue; // This was inside the for loop, so it's correct here
                } else {
                    // Dynamically estimate fee for a simple system transfer
                    let latest_blockhash_for_fee_est = match client.get_latest_blockhash().await {
                        Ok(bh) => bh,
                        Err(e) => {
                            let _ = status_sender.send(Err(format!("❌ [{}] SOLGather: Failed to get blockhash for fee estimation: {}", current_wallet_pk_str, e)));
                            sol_gathered_failure_count += 1;
                            // continue; // This was inside the for loop
                            return; // If we can't get blockhash, we can't proceed with this wallet in this iteration.
                                    // However, the original code used `continue` which is correct for a loop.
                                    // This block is inside a `tokio::spawn` so `return` exits the task for this wallet.
                                    // For loop context, `continue` is appropriate. Let's assume this whole block is per wallet.
                                    // The read_file was for the loop, so `continue` is the right semantic.
                        }
                    };

                    let dummy_instruction_for_fee = system_instruction::transfer(&source_wallet_pubkey, &parent_pubkey, 1);
                    let fee_calc_message = match MessageV0::try_compile(&source_wallet_pubkey, &[dummy_instruction_for_fee.clone()], &[], latest_blockhash_for_fee_est) {
                        Ok(msg) => msg,
                        Err(e) => {
                            let _ = status_sender.send(Err(format!("❌ [{}] SOLGather: Failed to compile message for fee estimation: {}", current_wallet_pk_str, e)));
                            sol_gathered_failure_count += 1;
                            // continue;
                            return; // As above, for loop context, continue is better.
                        }
                    };

                    let actual_network_fee = match client.get_fee_for_message(&fee_calc_message).await {
                        Ok(f) if f > 0 => f,
                        Ok(_) => {
                            let _ = status_sender.send(Ok(format!("⚠️ [{}] SOLGather: Got 0 fee from estimation, using default 5000.", current_wallet_pk_str)));
                            5000
                        }
                        Err(e) => {
                            let _ = status_sender.send(Ok(format!("⚠️ [{}] SOLGather: Failed to get dynamic fee: {}. Using default 5000.", current_wallet_pk_str, e)));
                            5000
                        }
                    };
                    let _ = status_sender.send(Ok(format!("ℹ️ [{}] SOLGather: Dynamically estimated network fee for simple transfer: {} lamports.", current_wallet_pk_str, actual_network_fee)));

                    if balance_lamports <= actual_network_fee {
                        let _ = status_sender.send(Ok(format!("ℹ️ [{}] SOLGather: Balance ({}) <= dynamically estimated fee ({}). Skipping.", current_wallet_pk_str, balance_lamports, actual_network_fee)));
                        // continue;
                    } else {
                        let amount_to_transfer = balance_lamports - actual_network_fee;
                        let _ = status_sender.send(Ok(format!("ℹ️ [{}] SOLGather: Calculated to transfer: {} lamports. (Aiming to leave 0 after fee of {}).", current_wallet_pk_str, amount_to_transfer, actual_network_fee)));

                        if amount_to_transfer > 0 { // Should always be true if balance_lamports > actual_network_fee
                            let _ = status_sender.send(Ok(format!("⏳ [{}] SOLGather: Attempting transfer of {} lamports...", current_wallet_pk_str, amount_to_transfer)));

                            let latest_blockhash_for_tx = match client.get_latest_blockhash().await {
                                 Ok(bh) => bh,
                                 Err(e) => {
                                    let _ = status_sender.send(Err(format!("❌ [{}] SOLGather: Failed to get blockhash for actual transaction: {}", current_wallet_pk_str, e)));
                                    sol_gathered_failure_count += 1;
                                    // continue;
                                    return; // As above.
                                }
                            };

                            let transfer_ix = system_instruction::transfer(&source_wallet_pubkey, &parent_pubkey, amount_to_transfer);
                            let instructions = vec![transfer_ix]; // NO ComputeBudget instructions

                            let _ = status_sender.send(Ok(format!("⏳ [{}] SOLGather: Got blockhash {}. Creating tx...", current_wallet_pk_str, latest_blockhash_for_tx)));
                            let tx = Transaction::new_signed_with_payer(
                                &instructions,
                                Some(&source_wallet_pubkey),
                                &[&source_wallet_keypair],
                                latest_blockhash_for_tx,
                            );
                            let _ = status_sender.send(Ok(format!("⏳ [{}] SOLGather: Tx created. Sending...", current_wallet_pk_str)));
                            match client.send_and_confirm_transaction_with_spinner(&tx).await {
                                Ok(signature) => {
                                    let _ = status_sender.send(Ok(format!("✅ SOL gathered from {}. Amount: {} lamports. Sig: {}", current_wallet_pk_str, amount_to_transfer, signature)));
                                    sol_gathered_success_count += 1;
                                }
                                Err(e) => {
                                    let _ = status_sender.send(Err(format!("❌ SOL gather TX failed for {}: {}. Attempted to send {} from balance {}.", current_wallet_pk_str, e, amount_to_transfer, balance_lamports)));
                                    sol_gathered_failure_count += 1;
                                }
                            }
                        }
                        // No 'else' needed for amount_to_transfer > 0, as it's covered by outer if/else
                    }
                }
            }
            Err(e) => {
                let _ = status_sender.send(Err(format!("❌ Failed to get SOL balance for {}: {}", current_wallet_pk_str, e)));
                sol_gathered_failure_count += 1;
            }
        }
        sleep(Duration::from_millis(300)).await; // Keep the slightly increased delay
    }
    let _ = status_sender.send(Ok(format!("🏁 SOL Gathering Phase Complete. Success: {}, Fail: {}", sol_gathered_success_count, sol_gathered_failure_count)));

    // --- 5. Parent Sells Tokens ---
    let _ = status_sender.send(Ok(format!("💸 Phase 3: Parent wallet {} attempting to sell all {} tokens...", parent_pubkey, token_mint_ca_str)));

    // Get parent's final token balance
    match client.get_token_account_balance(&parent_token_ata).await {
        Ok(balance_response) => {
            match balance_response.amount.parse::<u64>() {
                Ok(total_tokens_to_sell) if total_tokens_to_sell > 0 => {
                    let _ = status_sender.send(Ok(format!("ℹ️ Parent wallet has {} tokens to sell.", total_tokens_to_sell)));
                    // Using pumpfun::sell_token which expects amount_value as f64 (percentage or UI amount)
                    // For 100% sell, we pass 100.0 as f64.
                    // The priority_fee_lamports is u64, pumpfun::sell_token expects f64 SOL for priority_fee.
                    let priority_fee_sol = lamports_to_sol(priority_fee_lamports);

                    match api::pumpfun::sell_token(&parent_keypair, &token_mint_pubkey, 100.0, slippage_percent as u8, priority_fee_sol).await {
                        Ok(versioned_tx) => {
                            match crate::utils::transaction::sign_and_send_versioned_transaction(client.as_ref(), versioned_tx, &[&parent_keypair]).await {
                                Ok(signature) => {
                                    let _ = status_sender.send(Ok(format!("🎉🎉🎉 Parent wallet {} successfully sold all {} tokens! Sig: {}", parent_pubkey, token_mint_ca_str, signature)));
                                    let _ = status_sender.send(Ok("✅ Gather All Funds & Sell operation completed successfully.".to_string()));
                                }
                                Err(e) => {
                                    let _ = status_sender.send(Err(format!("❌ Parent wallet {} failed to send sell transaction for {}: {}", parent_pubkey, token_mint_ca_str, e)));
                                    let _ = status_sender.send(Err("❗ Gather All Funds & Sell operation completed with errors during final sell.".to_string()));
                                }
                            }
                        }
                        Err(e) => {
                            let _ = status_sender.send(Err(format!("❌ Parent wallet {} failed to create sell transaction for {}: {}", parent_pubkey, token_mint_ca_str, e)));
                            let _ = status_sender.send(Err("❗ Gather All Funds & Sell operation completed with errors creating final sell tx.".to_string()));
                        }
                    }
                }
                Ok(_) => { // total_tokens_to_sell is 0
                    let _ = status_sender.send(Ok(format!("ℹ️ Parent wallet has no {} tokens to sell after gathering.", token_mint_ca_str)));
                    let _ = status_sender.send(Ok("✅ Gather All Funds & Sell operation completed (no tokens to sell for parent).".to_string()));
                }
                Err(e) => {
                    let _ = status_sender.send(Err(format!("❌ Failed to parse parent's final token balance for {}: {}", token_mint_ca_str, e)));
                    let _ = status_sender.send(Err("❗ Gather All Funds & Sell operation completed with errors getting parent final token balance.".to_string()));
                }
            }
        }
        Err(e) => {
            let _ = status_sender.send(Err(format!("❌ Failed to get parent's final token account balance for {}: {}", token_mint_ca_str, e)));
            let _ = status_sender.send(Err("❗ Gather All Funds & Sell operation completed with errors getting parent final token balance.".to_string()));
        }
    }
}


// Example: A simplified create_alt_task structure
async fn create_alt_task(
    _rpc_url: String, _parent_private_key_str: String, _addresses_to_add: Vec<Pubkey>,
    sender: UnboundedSender<AltCreationStatus>
) {
    // Full logic from app.rs needed here
    let _ = sender.send(AltCreationStatus::Failure("Create ALT task placeholder executed".to_string()));
}


// Helper struct for Mix Wallets Task parameters
#[derive(Debug, Clone)]
struct MixWalletsTaskParams {
    rpc_url: String,
    original_wallets_data: Vec<LoadedWalletInfo>,
    minter_pubkey_str: Option<String>,
    parent_pubkey_str: Option<String>,
}
impl eframe::App for ModernApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ctx.set_style(self.style.clone()); // Removed: No longer re-applying style from self here

        self.time += ctx.input(|i| i.stable_dt).max(0.001) as f64;
        ctx.request_repaint_after(Duration::from_millis(16));
        let screen_rect = ctx.screen_rect();
        for particle in self.particles.iter_mut() {
            particle.update(screen_rect);
        }

        // --- Process All Incoming Messages (Copied and adapted from PumpFunApp::update) ---

        // --- Mix Wallets Task Request ---
        if self.start_mix_wallets_request.is_some() {
            self.start_mix_wallets_request = None; // Consume the request

            if self.mix_wallets_in_progress {
                log::warn!("Mix wallets request ignored: task already in progress.");
                self.mix_wallets_log_messages.push("⚠️ Mix wallets request ignored: task already in progress.".to_string());
                self.launch_log_messages.push("⚠️ Mix wallets request (Launch View) ignored: task already in progress.".to_string());
            } else if self.loaded_wallet_data.is_empty() {
                log::warn!("Mix wallets request ignored: No wallets loaded to mix.");
                self.mix_wallets_log_messages.push("⚠️ Mix wallets request ignored: No wallets loaded to mix (check keys file).".to_string());
                self.launch_log_messages.push("⚠️ Mix wallets request (Launch View) ignored: No wallets loaded.".to_string());
                self.last_operation_result = Some(Err("No wallets loaded to mix.".to_string()));
            } else {
                self.mix_wallets_in_progress = true;
                self.mix_wallets_log_messages.clear();
                self.mix_wallets_log_messages.push("🚀 Spawning Mix Wallets task...".to_string());
                self.launch_log_messages.push("🌪️ Mix Wallets task initiated from Launch View.".to_string());

                let minter_pubkey_str = if !self.app_settings.dev_wallet_private_key.is_empty() {
                    AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key)
                } else {
                    None
                };
                let parent_pubkey_str = if !self.app_settings.parent_wallet_private_key.is_empty() {
                    AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key)
                } else {
                    None
                };

                let task_params = MixWalletsTaskParams {
                    rpc_url: self.app_settings.solana_rpc_url.clone(),
                    original_wallets_data: self.loaded_wallet_data.clone(),
                    minter_pubkey_str,
                    parent_pubkey_str,
                };
                let status_sender_clone = self.mix_wallets_status_sender.clone();

                let task_handle = tokio::spawn(async move {
                    if let Err(e) = ModernApp::run_mix_wallets_task(
                        task_params.rpc_url,
                        task_params.original_wallets_data,
                        status_sender_clone.clone(),
                        task_params.minter_pubkey_str, // Pass minter pubkey for exclusion
                        task_params.parent_pubkey_str, // Pass parent pubkey for exclusion
                    ).await {
                        log::error!("Mix Wallets task ended with error: {}", e);
                        let _ = status_sender_clone.send(Err(format!("Mix Wallets task failed: {}", e)));
                    }
                });
                self.mix_wallets_task_handle = Some(task_handle);
            }
        }

        // --- Mix Wallets Status Receiver ---
        while let Ok(mix_status_result) = self.mix_wallets_status_receiver.try_recv() {
            match mix_status_result {
                Ok(message) => {
                    self.mix_wallets_log_messages.push(message.clone());
                    self.launch_log_messages.push(format!("[Mix]: {}", message));
                    if message.starts_with("🏁 Mix Wallets Task Finished") {
                        self.mix_wallets_in_progress = false;
                        self.last_operation_result = Some(Ok(message.clone())); // Clone message for potential use
                        // Try to extract and store the filename
                        if let Some(path_part) = message.strip_prefix("🏁 Mix Wallets Task Finished. Wallets saved to: ") {
                            self.last_mixed_wallets_file_path = Some(path_part.trim().to_string());
                            log::info!("Captured last mixed wallets file path: {}", path_part.trim());
                        }
                    }
                }
                Err(error_message) => {
                    let full_err_msg = format!("ERROR: {}", error_message);
                    self.mix_wallets_log_messages.push(full_err_msg.clone());
                    self.launch_log_messages.push(format!("[Mix ERROR]: {}", error_message));
                    self.mix_wallets_in_progress = false;
                    self.last_operation_result = Some(Err(error_message));
                }
            }
            ctx.request_repaint();
        }

        // --- Retry Zombie to Mixed Wallets Funding Status Receiver ---
        while let Ok(retry_funding_status_result) = self.retry_zombie_to_mixed_funding_status_receiver.try_recv() {
            match retry_funding_status_result {
                Ok(message) => {
                    self.retry_zombie_to_mixed_funding_log_messages.push(message.clone());
                    self.launch_log_messages.push(format!("[Retry Fund]: {}", message)); // Also log to main launch log for visibility
                    if message.starts_with("🏁") || message.starts_with("✅") || message.starts_with("❌") { // Common prefixes for final messages
                        self.retry_zombie_to_mixed_funding_in_progress = false;
                        self.last_operation_result = Some(Ok(message));
                    }
                }
                Err(error_message) => {
                    let full_err_msg = format!("ERROR: {}", error_message);
                    self.retry_zombie_to_mixed_funding_log_messages.push(full_err_msg.clone());
                    self.launch_log_messages.push(format!("[Retry Fund ERROR]: {}", error_message));
                    self.retry_zombie_to_mixed_funding_in_progress = false;
                    self.last_operation_result = Some(Err(error_message));
                }
            }
            ctx.request_repaint();
        }

        while let Ok(fetch_result) = self.balance_fetch_receiver.try_recv() {
            self.balance_tasks_completed += 1;
            match fetch_result {
                FetchResult::Success { address, sol_balance, wsol_balance, target_mint_balance } => { // Added wsol_balance
                    if self.volume_bot_wallets.iter().any(|lw| lw.public_key == address) {
                        if let Some(wallet_info) = self.volume_bot_wallet_display_infos.iter_mut().find(|wi| wi.address == address) {
                            wallet_info.sol_balance = Some(sol_balance); wallet_info.wsol_balance = wsol_balance; wallet_info.target_mint_balance = target_mint_balance; wallet_info.error = None; wallet_info.is_loading = false;
                        }
                    } else if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == address) {
                        wallet.sol_balance = Some(sol_balance); wallet.wsol_balance = wsol_balance; wallet.target_mint_balance = target_mint_balance; wallet.error = None; wallet.is_loading = false;
                    }
                }
                FetchResult::Failure { address, error } => {
                    if self.volume_bot_wallets.iter().any(|lw| lw.public_key == address) {
                        if let Some(wallet_info) = self.volume_bot_wallet_display_infos.iter_mut().find(|wi| wi.address == address) {
                            wallet_info.error = Some(error); wallet_info.is_loading = false;
                        }
                    } else if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == address) {
                        wallet.error = Some(error); wallet.is_loading = false;
                    }
                }
            }
            if self.balance_tasks_completed >= self.balance_tasks_expected { self.balances_loading = false; }
        }

        let mut new_parent_sol_display: Option<f64> = None;
        let mut new_total_sol_display: f64 = 0.0;
        let mut counted_addresses_for_total: HashSet<String> = HashSet::new();
        for wallet_info in &self.wallets {
            if wallet_info.is_parent { new_parent_sol_display = wallet_info.sol_balance; }
            if let Some(sol) = wallet_info.sol_balance {
                if wallet_info.is_parent || wallet_info.is_dev_wallet || (!wallet_info.is_parent && !wallet_info.is_dev_wallet) {
                    if counted_addresses_for_total.insert(wallet_info.address.clone()) { new_total_sol_display += sol; }
                }
            }
        }
        self.parent_sol_balance_display = new_parent_sol_display;
        self.total_sol_balance_display = Some(new_total_sol_display);

        while let Ok(disperse_result) = self.disperse_result_receiver.try_recv() { self.last_operation_result = Some(disperse_result); self.disperse_in_progress = false; }
        while let Ok(gather_result) = self.gather_result_receiver.try_recv() {
            self.gather_tasks_completed += 1; self.last_operation_result = Some(gather_result);
            if self.gather_tasks_completed >= self.gather_tasks_expected { self.gather_in_progress = false; self.last_operation_result = Some(Ok(format!("Gather complete: {} tasks.", self.gather_tasks_completed))); }
        }
        while let Ok(precalc_result) = self.alt_precalc_result_receiver.try_recv() {
            self.alt_precalc_in_progress = false;
            match precalc_result {
                PrecalcResult::Log(msg) => {
                    self.alt_log_messages.push(format!("[PRECALC_DETAIL]: {}", msg));
                    self.alt_view_status = msg; // Update view status with the latest log
                }
                PrecalcResult::Success(s, v) => {
                    self.alt_generated_mint_pubkey = Some(s);
                    self.alt_precalc_addresses = v;
                    self.alt_view_status = "Precalc OK. Ready to Create ALT.".to_string();
                    self.alt_log_messages.push("Precalc OK. Ready to Create ALT.".to_string());
                    self.load_available_mint_keypairs(); // Refresh mint keypair list
                }
                PrecalcResult::Failure(e) => {
                    self.alt_view_status = format!("Precalc Fail: {}", e);
                    self.alt_log_messages.push(format!("[ERROR] Precalc: {}", e));
                }
            }
        }
        // Removed potentially problematic duplicate try_recv for alt_creation_status_receiver
        // while let Ok(_status) = self.alt_creation_status_receiver.try_recv() { /* Full logic from app.rs */ }
        while let Ok(status) = self.sell_status_receiver.try_recv() {
            match status {
                SellStatus::Idle => {
                    // Potentially add a log or UI update if needed
                }
                SellStatus::InProgress(msg) => {
                    let log_message = format!("[Sell In Progress] {}", msg);
                    info!("{}", log_message);
                    self.sell_log_messages.push(log_message);
                    // Individual wallet selling flags are handled by trigger_individual_sell
                }
                SellStatus::WalletSuccess(addr, sig_or_msg) => {
                    let log_message = format!("[Sell Success: {}] {}", addr, sig_or_msg);
                    info!("{}", log_message);
                    self.sell_log_messages.push(log_message);
                    if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == addr) {
                        wallet.is_selling = false; // Clear loading for this wallet
                    }
                    self.trigger_balance_fetch(Some(self.sell_mint.clone()));
                }
                SellStatus::WalletFailure(addr, err_msg) => {
                    let log_message = format!("[Sell Failure: {}] {}", addr, err_msg);
                    error!("{}", log_message); // Log as error
                    self.sell_log_messages.push(log_message);
                     if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == addr) {
                        wallet.is_selling = false; // Clear loading for this wallet
                    }
                }
                SellStatus::MassSellComplete(msg) => {
                    let log_message = format!("[Mass Sell Complete] {}", msg);
                    info!("{}", log_message);
                    self.sell_log_messages.push(log_message);
                    self.sell_mass_reverse_in_progress = false;
                    for wallet in self.wallets.iter_mut() { // Ensure all wallets are marked not selling
                        wallet.is_selling = false;
                    }
                    self.trigger_balance_fetch(Some(self.sell_mint.clone()));
                }
                SellStatus::GatherAndSellComplete(msg) => {
                    let log_message = format!("[Gather & Sell Complete] {}", msg);
                    info!("{}", log_message);
                    self.sell_log_messages.push(log_message);
                    self.sell_gather_and_sell_in_progress = false;
                     for wallet in self.wallets.iter_mut() { // Ensure all wallets are marked not selling
                        wallet.is_selling = false;
                    }
                    self.trigger_balance_fetch(Some(self.sell_mint.clone()));
                }
                SellStatus::Failure(err_msg) => {
                    let log_message = format!("[Sell General Error] {}", err_msg);
                    error!("{}", log_message); // Log as error
                    self.sell_log_messages.push(log_message);
                    self.sell_mass_reverse_in_progress = false;
                    self.sell_gather_and_sell_in_progress = false;
                    for wallet in self.wallets.iter_mut() {
                        wallet.is_selling = false;
                    }
                }
            }
        }
        // while let Ok(_status) = self.pump_status_receiver.try_recv() { /* Full logic from app.rs */ } // Keep original pump handling below

        // --- PUMP.FUN BOT --- (Original handling from previous state, ensure it's not duplicated or conflicting)
        if let Ok(status) = self.pump_status_receiver.try_recv() {
            match status {
                PumpStatus::InProgress(msg) => { // Changed from Log and InProgress to just InProgress
                    info!("Pump InProgress: {}", msg);
                    self.pump_log_messages.push(msg);
                    self.pump_in_progress = true; // Ensure it's marked as in progress
                }
                PumpStatus::Failure(err_msg) => { // Changed from Error
                    error!("Pump Failure: {}", err_msg);
                    self.pump_log_messages.push(format!("FAILURE: {}", err_msg));
                    self.pump_in_progress = false;
                    self.pump_is_running.store(false, AtomicOrdering::Relaxed);
                    if let Some(handle) = self.pump_task_handle.take() {
                        handle.abort();
                    }
                }
                PumpStatus::Success(summary_msg) => { // Changed from Completed
                    info!("Pump Success: {}", summary_msg);
                    self.pump_log_messages.push(format!("SUCCESS: {}", summary_msg));
                    self.pump_in_progress = false;
                    self.pump_is_running.store(false, AtomicOrdering::Relaxed);
                     if let Some(handle) = self.pump_task_handle.take() {
                        tokio::spawn(async move {
                            if let Err(e) = handle.await {
                                error!("Error joining pump task: {:?}", e);
                            }
                        });
                    }
                }
                PumpStatus::Idle => {
                    // Optionally handle Idle state, e.g., log or update UI
                    info!("Pump status: Idle");
                }
            }
        }

        // --- PNL CALCULATION ---
        if let Ok(pnl_result) = self.pnl_calc_receiver.try_recv() {
            self.pnl_calculation_in_progress = false;
            match pnl_result {
                Ok(summary) => {
                    self.pnl_summary = Some(summary);
                    self.pnl_error_message = None;
                }
                Err(err_msg) => {
                    self.pnl_summary = None;
                    self.pnl_error_message = Some(err_msg);
                }
            }
        }

        // --- WSOL Unwrapping Status ---
        while let Ok(unwrap_status_result) = self.unwrap_wsol_status_receiver.try_recv() {
            match unwrap_status_result {
                Ok(message) => {
                    self.unwrap_wsol_log_messages.push(message.clone());
                    self.atomic_buy_log_messages.push(format!("[Unbonk]: {}", message));
                    if message.starts_with("🏁 WSOL unwrapping process finished") {
                        self.unwrap_wsol_in_progress = false;
                        self.last_operation_result = Some(Ok(message));
                    }
                }
                Err(error_message) => {
                    self.unwrap_wsol_log_messages.push(format!("ERROR: {}", error_message));
                    self.atomic_buy_log_messages.push(format!("[Unbonk ERROR]: {}", error_message));
                    self.unwrap_wsol_in_progress = false;
                    self.last_operation_result = Some(Err(error_message));
                }
            }
            ctx.request_repaint();
        }

        while let Ok(status) = self.launch_status_receiver.try_recv() {
            match status {
                LaunchStatus::Starting => {
                    self.launch_log_messages.push("🚀 Launch sequence initiated by task.".to_string());
                    self.launch_completion_link = None; // Reset link on new launch
                    // self.launch_in_progress is typically set by the caller of the task
                }
                LaunchStatus::Log(message) => {
                    self.launch_log_messages.push(message);
                }
                LaunchStatus::UploadingMetadata => {
                    self.launch_log_messages.push("☁️ Uploading token metadata...".to_string());
                }
                LaunchStatus::PreparingTx(tx_label) => {
                    self.launch_log_messages.push(format!("🛠️ Preparing Transaction: {}", tx_label));
                }
                LaunchStatus::MetadataUploaded(uri) => {
                    self.launch_log_messages.push(format!("✅ Metadata Uploaded: {}", uri));
                }
                LaunchStatus::SimulatingTx(tx_label) => {
                    self.launch_log_messages.push(format!("🔍 Simulating Transaction: {}", tx_label));
                }
                LaunchStatus::SimulationSuccess(tx_label) => {
                    self.launch_log_messages.push(format!("✅ Simulation Successful: {}", tx_label));
                }
                LaunchStatus::SimulationFailed(tx_label, error_msg) => {
                    let full_error_message = format!("❌ Simulation Failed: {} - Error: {}", tx_label, error_msg);
                    self.launch_log_messages.push(full_error_message.clone());
                    if !self.launch_simulate_only { // Corrected: Check against the direct field
                        self.last_operation_result = Some(Err(full_error_message));
                        self.launch_in_progress = false; // Stop if not simulate only
                    }
                }
                LaunchStatus::SimulationWarning(tx_label, warning_msg) => {
                    let full_warning_message = format!("⚠️ Simulation Warning: {} - Detail: {}", tx_label, warning_msg);
                    self.launch_log_messages.push(full_warning_message);
                }
                LaunchStatus::SimulationSkipped(tx_label) => {
                    self.launch_log_messages.push(format!("⏩ Simulation Skipped: {}", tx_label));
                }
                LaunchStatus::BuildFailed(tx_label, error_msg) => {
                    let full_error_message = format!("🧱 Build Failed: {} - Error: {}", tx_label, error_msg);
                    self.launch_log_messages.push(full_error_message.clone());
                    self.last_operation_result = Some(Err(full_error_message));
                    self.launch_in_progress = false;
                }
                LaunchStatus::SubmittingBundle(count) => {
                    self.launch_log_messages.push(format!("📦 Submitting Jito bundle with {} transactions...", count));
                }
                LaunchStatus::BundleSubmitted(bundle_id) => {
                    self.launch_log_messages.push(format!("✉️ Jito Bundle Submitted successfully! Bundle ID: {}", bundle_id));
                    self.launch_log_messages.push("ℹ️ Monitor Jito Bundle status separately using the Bundle ID.".to_string());
                    // Don't set last_operation_result or launch_in_progress here, wait for final status
                }
                LaunchStatus::Success(message) => { // Generic success from task wrapper
                    self.launch_log_messages.push(format!("🎉 Launch Task Reported Success: {}", message));
                    self.last_operation_result = Some(Ok(message));
                    self.launch_in_progress = false;
                }
                LaunchStatus::Failure(error_msg) => { // Generic failure from task wrapper
                    self.launch_log_messages.push(format!("☠️ Launch Task Reported Failure: {}", error_msg));
                    self.last_operation_result = Some(Err(error_msg));
                    self.launch_in_progress = false;
                }
                LaunchStatus::Error(error_msg) => { // Specific error during launch steps
                    self.launch_log_messages.push(format!("🔥 Launch Error: {}", error_msg));
                    self.last_operation_result = Some(Err(error_msg));
                    self.launch_in_progress = false;
                }
                LaunchStatus::PreparingZombieTxs => {
                    self.launch_log_messages.push("🧟 Preparing zombie transactions...".to_string());
                }
                LaunchStatus::SimulatedOnly(message) => {
                    self.launch_log_messages.push(format!("🏁 Simulated Only: {}", message));
                    self.last_operation_result = Some(Ok(format!("Simulated: {}", message)));
                    self.launch_in_progress = false;
                }
                LaunchStatus::BundleLanded(bundle_id, status_msg) => {
                    self.launch_log_messages.push(format!("🛬 Bundle {} Landed: {}", bundle_id, status_msg));
                    // Still wait for overall Success/Failure/CompletedWithLink
                }
                LaunchStatus::Completed => { // Generic completion, link might come separately
                    self.launch_log_messages.push("✅ Launch Task Process Completed.".to_string());
                    if self.last_operation_result.is_none() && self.launch_completion_link.is_none() {
                        self.last_operation_result = Some(Ok("Launch completed.".to_string()));
                    }
                    self.launch_in_progress = false;
                }
                LaunchStatus::LaunchCompletedWithLink(link) => {
                    let success_message = format!("🎉 LAUNCH COMPLETE! 🎉 Pump.fun Link: {}", link);
                    self.launch_log_messages.push(success_message.clone());
                    self.launch_completion_link = Some(link.clone());
                    self.last_operation_result = Some(Ok(success_message)); // Set a success result
                    self.launch_in_progress = false;
                    info!("[LaunchTask Completed GUI] Link: {}", link);
                }
            }
            ctx.request_repaint(); // Ensure UI updates when log messages change
        }
        while let Ok(_result) = self.transfer_status_receiver.try_recv() { /* Full logic from app.rs */ }
        while let Ok(status_result) = self.volume_bot_wallet_gen_status_receiver.try_recv() {
            self.volume_bot_generation_in_progress = false;
            match status_result {
                Ok(msg) => {
                    self.sim_log_messages.push(format!("[Wallet Gen]: {}", msg));
                    self.last_operation_result = Some(Ok(msg.clone()));
                    // Try to parse filename: "Successfully generated X wallets and saved to Y"
                    if let Some(idx) = msg.rfind("saved to ") {
                        let filename = msg[(idx + "saved to ".len())..].to_string();
                        self.last_generated_volume_wallets_file = Some(filename);
                    }
                }
                Err(e) => {
                    self.sim_log_messages.push(format!("[Wallet Gen ERROR]: {}", e));
                    self.last_operation_result = Some(Err(e));
                }
            }
            ctx.request_repaint();
        }
        while let Ok(wallets_vec) = self.volume_bot_wallets_receiver.try_recv() {
            self.sim_log_messages.push(format!("[Wallet Gen]: Received {} wallets from generation task.", wallets_vec.len()));
            self.volume_bot_wallets = wallets_vec;
            self.volume_bot_wallets_generated = true; // Mark that wallets are now from a generation process
            self.volume_bot_wallet_display_infos.clear(); // Clear old display infos, new balances need fetching
            // Potentially update self.loaded_wallet_data if these are meant to be globally available for other ops
            // For now, keeping them specific to volume_bot_wallets.
            ctx.request_repaint();
        }
        while let Ok(status_result) = self.volume_bot_funding_status_receiver.try_recv() {
            match status_result {
                Ok(msg) => {
                    self.volume_bot_funding_log_messages.push(format!("[Funding/Collection]: {}", msg));

                    // Define final success message fragments
                    let final_gather_success_msg_fragment = "✅ Gather All Funds & Sell operation completed successfully.";
                    let final_gather_no_sell_msg_fragment = "✅ Gather All Funds & Sell operation completed (no tokens to sell for parent).";
                    let final_dispersal_msg_fragment = "Dispersal finished"; // For simple disperse

                    // Define final error/halt message fragments for gather task
                    let final_gather_error_fragment = "❗ Gather All Funds & Sell operation completed with errors";
                    let gather_halt_ata_fragment = "Parent ATA creation: Halting operation";
                    let gather_halt_parent_kp_fragment = "Failed to load parent keypair";
                    let gather_halt_mint_fragment = "Invalid token mint address";

                    let is_final_success_message_for_gather = msg.contains(final_gather_success_msg_fragment) || msg.contains(final_gather_no_sell_msg_fragment);
                    let is_final_error_message_for_gather = msg.contains(final_gather_error_fragment) ||
                                                            msg.contains(gather_halt_ata_fragment) ||
                                                            msg.contains(gather_halt_parent_kp_fragment) ||
                                                            msg.contains(gather_halt_mint_fragment);

                    let is_final_message_for_dispersal = msg.contains(final_dispersal_msg_fragment);

                    // Update last_operation_result logic:
                    // If it's a final success or a final error message for the relevant active operation, update it.
                    // Otherwise, intermediate Ok messages do not overwrite an existing Err.
                    if self.awaiting_final_gather_status {
                        if is_final_success_message_for_gather || is_final_error_message_for_gather {
                            self.last_operation_result = Some(Ok(msg.clone())); // For gather, errors are also sent as Ok(error_summary_string) by the task
                        }
                    } else { // Not awaiting gather, so it's a simple disperse or other funding op
                        if is_final_message_for_dispersal {
                             self.last_operation_result = Some(Ok(msg.clone()));
                        }
                        // Potentially allow other non-batch intermediate Ok messages to update if no error is present
                        else if !msg.contains("Batch") && self.last_operation_result.as_ref().map_or(true, |res| res.is_ok()) {
                            self.last_operation_result = Some(Ok(msg.clone()));
                        }
                    }

                    // The rest of the logic for resetting flags and triggering balance refresh:
                    let is_truly_final_message_for_gather_task = is_final_success_message_for_gather || is_final_error_message_for_gather;
                    // Note: `final_gather_success_msg` string variable below is now shadowed by the fragment but should be fine as it's re-declared.
                    let final_gather_success_msg = "✅ Gather All Funds & Sell operation completed successfully.".to_string();
                    let final_gather_no_sell_msg = "✅ Gather All Funds & Sell operation completed (no tokens to sell for parent).".to_string();
                    let final_gather_error_critical_msg1 = "❗ Gather All Funds & Sell operation completed with errors during final sell.".to_string();
                    let final_gather_error_critical_msg2 = "❗ Gather All Funds & Sell operation completed with errors creating final sell tx.".to_string();
                    let final_gather_error_critical_msg3 = "❗ Gather All Funds & Sell operation completed with errors getting parent final token balance.".to_string();
                    // Matches messages from gather_all_funds_and_sell_task if parent ATA creation or keypair loading fails early
                    let final_gather_error_ata_halt_msg_fragment = "Parent ATA creation: Halting operation".to_string();
                    let final_gather_error_parent_kp_halt_msg_fragment = "Failed to load parent keypair".to_string();
                    let final_gather_error_mint_halt_msg_fragment = "Invalid token mint address".to_string();


                    let is_truly_final_message_for_gather_task = msg == final_gather_success_msg ||
                                                                msg == final_gather_no_sell_msg ||
                                                                msg == final_gather_error_critical_msg1 ||
                                                                msg == final_gather_error_critical_msg2 ||
                                                                msg == final_gather_error_critical_msg3 ||
                                                                msg.contains(&final_gather_error_ata_halt_msg_fragment) ||
                                                                msg.contains(&final_gather_error_parent_kp_halt_msg_fragment) ||
                                                                msg.contains(&final_gather_error_mint_halt_msg_fragment);

                    if self.awaiting_final_gather_status && is_truly_final_message_for_gather_task {
                        self.volume_bot_funding_in_progress = false;
                        self.awaiting_final_gather_status = false;

                        self.sim_log_messages.push("[Auto-Refresh]: Triggering balance refresh for volume wallets after FINAL gather op status.".to_string());
                        if !self.sim_token_mint.trim().is_empty() && !self.volume_bot_wallets.is_empty() {
                            let addresses_to_check: Vec<String> = self.volume_bot_wallets.iter().map(|lw| lw.public_key.clone()).collect();
                            if !addresses_to_check.is_empty() {
                                self.balances_loading = true;
                                self.balance_tasks_expected = addresses_to_check.len();
                                self.balance_tasks_completed = 0;
                                self.volume_bot_wallet_display_infos.clear();
                                for (index, lw_info) in self.volume_bot_wallets.iter().enumerate() {
                                    self.volume_bot_wallet_display_infos.push(WalletInfo {
                                        address: lw_info.public_key.clone(),
                                        name: format!("Volume Wallet {}", index + 1),
                                        is_primary: false, is_dev_wallet: false, is_parent: false,
                                        sol_balance: None, wsol_balance: None, target_mint_balance: None, error: None, is_loading: true,
                                        is_selected: false, sell_percentage_input: String::new(), sell_amount_input: String::new(), is_selling: false,
                                        atomic_buy_sol_amount_input: String::new(),
                                        atomic_sell_token_amount_input: String::new(), // New field
                                        atomic_is_selling: false, // New field
                                        atomic_sell_status_message: None, // New field
                                    });
                                }
                                let params = FetchBalancesTaskParams {
                                    rpc_url: self.app_settings.solana_rpc_url.clone(),
                                    addresses_to_check,
                                    target_mint: self.sim_token_mint.trim().to_string(),
                                    balance_fetch_sender: self.balance_fetch_sender.clone(),
                                };
                                self.start_fetch_balances_request = Some(params);
                            } else {
                                self.sim_log_messages.push("[Auto-Refresh]: No volume wallets to refresh balances for.".to_string());
                            }
                        } else {
                             self.sim_log_messages.push("[Auto-Refresh]: Skipped balance refresh (no token mint or no volume wallets).".to_string());
                        }
                    } else if !self.awaiting_final_gather_status && msg.contains("Dispersal finished") {
                        // This is for the simpler disperse_sol_task (Distribute Total SOL / Fund from Parent)
                        self.volume_bot_funding_in_progress = false;
                        // No auto-refresh for simple disperse by default.
                    }
                    // Intermediate messages from any task do not change `volume_bot_funding_in_progress` or `awaiting_final_gather_status`
                }
                Err(e) => { // This Err is if the channel itself fails, or if a task sends an Err variant directly
                    self.volume_bot_funding_log_messages.push(format!("[Funding/Collection Task ERROR]: {}", e));
                    self.last_operation_result = Some(Err(e.clone())); // Always update on Err
                    // If an error occurs from the sender, the task has likely ended.
                    self.volume_bot_funding_in_progress = false;
                    if self.awaiting_final_gather_status {
                        self.awaiting_final_gather_status = false; // Reset flag as the awaited task has errored.
                    }
                }
            }
            ctx.request_repaint();
        }
        while let Ok(status_result) = self.sim_status_receiver.try_recv() {
            match status_result {
                Ok(msg) => {
                    self.sim_log_messages.push(format!("[SIM_TASK]: {}", msg));
                    // Check for completion messages from the simulation task
                    if msg.contains("Volume Bot Task Completed") || msg.contains("Finished all cycles") || msg.contains("stopped due to error") || msg.contains("Task aborted") {
                        self.sim_in_progress = false;
                        self.last_operation_result = Some(Ok(msg)); // Final status
                    } else if msg.contains("Successfully loaded") && msg.contains("wallets from") {
                        // This is a status from wallet loading, not the sim loop itself. Don't change sim_in_progress.
                        self.last_operation_result = Some(Ok(msg));
                    }
                    // Other Ok messages are intermediate progress updates for the simulation.
                }
                Err(e) => {
                    self.sim_log_messages.push(format!("[SIM_TASK ERROR]: {}", e));
                    self.last_operation_result = Some(Err(e.clone())); // Store the error
                    self.sim_in_progress = false; // Error means simulation task stopped
                }
            }
            ctx.request_repaint();
        }
        while let Ok(_status_result) = self.sim_status_receiver_orig.try_recv() { /* Full logic from app.rs */ }
        while let Ok(_status_result) = self.sim_consolidate_sol_status_receiver_orig.try_recv() { /* Full logic from app.rs */ }
        while let Ok(_status_result) = self.sim_wallet_gen_status_receiver.try_recv() { /* Full logic from app.rs */ }
        while let Ok((balances, prices)) = self.monitor_update_receiver.try_recv() { self.monitor_balances = balances; self.monitor_prices = prices; }

        // Process ALT Precalc results
        while let Ok(precalc_result) = self.alt_precalc_result_receiver.try_recv() {
            self.alt_precalc_in_progress = false;
            match precalc_result {
                PrecalcResult::Log(msg) => {
                    self.alt_log_messages.push(format!("[PRECALC_DETAIL]: {}", msg));
                    self.alt_view_status = msg; // Update view status with the latest log
                }
                PrecalcResult::Success(mint_pk_str, addresses) => {
                    self.alt_generated_mint_pubkey = Some(mint_pk_str.clone()); // Clone if you need to use it elsewhere
                    self.alt_precalc_addresses = addresses;
                    let success_msg = format!("Precalculation successful. Generated Mint: {}. Ready to create ALT.", mint_pk_str);
                    self.alt_view_status = success_msg.clone();
                    self.alt_log_messages.push(success_msg);
                    self.last_operation_result = Some(Ok("ALT Precalculation Succeeded.".to_string()));
                    self.load_available_mint_keypairs(); // Refresh mint keypair list
                }
                PrecalcResult::Failure(err_msg) => {
                    self.alt_view_status = format!("Precalculation failed: {}", err_msg);
                    self.alt_log_messages.push(format!("[ERROR] Precalc: {}", err_msg));
// Process Unwrap WSOL status updates
        while let Ok(unwrap_result) = self.unwrap_wsol_status_receiver.try_recv() {
            match unwrap_result {
                Ok(msg) => {
                    self.unwrap_wsol_log_messages.push(format!("[Unwrap WSOL]: {}", msg));
                    self.atomic_buy_log_messages.push(format!("[Unwrap WSOL]: {}", msg)); // Log to main atomic log
                    if msg.starts_with("🏁") { // Check for final message
                        self.unwrap_wsol_in_progress = false;
                        self.last_operation_result = Some(Ok(msg));
                        self.trigger_balance_fetch(None); // Refresh all balances
                    } else if msg.starts_with("✅") || msg.starts_with("ℹ️") {
                        if self.last_operation_result.as_ref().map_or(true, |r| r.is_ok()) {
                           // self.last_operation_result = Some(Ok(msg)); // Avoid being too noisy
                        }
                    }
                }
                Err(err_msg) => {
                    self.unwrap_wsol_log_messages.push(format!("[Unwrap WSOL ERROR]: {}", err_msg));
                    self.atomic_buy_log_messages.push(format!("[Unwrap WSOL ERROR]: {}", err_msg));
                    self.last_operation_result = Some(Err(err_msg));
                    self.unwrap_wsol_in_progress = false; // Stop on error
                }
            }
            if !self.unwrap_wsol_in_progress && self.unwrap_wsol_task_handle.is_some() { // Check if a task was running
                 if let Some(handle) = &self.unwrap_wsol_task_handle {
                     if handle.is_finished() {
                        self.unwrap_wsol_task_handle = None;
                        self.unwrap_wsol_in_progress = false; // Explicitly set here too
                        log::trace!("Unwrap WSOL task handle processed, cleared, and progress flag reset.");
                     }
                 }
            } else if !self.unwrap_wsol_in_progress {
                 // This case means unwrap_wsol_in_progress was already false (e.g. by "🏁" or Err)
                 // OR there was no task handle to begin with.
                 // If there was a handle and it wasn't taken above, but progress is false, take it now.
                 if self.unwrap_wsol_task_handle.is_some() {
                    self.unwrap_wsol_task_handle = None;
                    log::trace!("Unwrap WSOL task handle cleared as operation is no longer in progress.");
                 } else {
                    log::trace!("Unwrap WSOL processing finished or errored (no active task handle or already cleared).");
                 }
            }
            ctx.request_repaint();
        }
                    self.last_operation_result = Some(Err(format!("ALT Precalculation Failed: {}", err_msg)));
                }
            }
        }

        log::debug!("GUI_UPDATE_REACHED_CHECKPOINT: About to check alt_creation_status_receiver.");

        // Simplified direct check of the channel state for ALT creation
        // Restore simple while let loop for processing ALT creation status
        while let Ok(status) = self.alt_creation_status_receiver.try_recv() {
            log::error!("GUI_UPDATE_PROCESS: Processing AltCreationStatus: {:?}", status); // High visibility log for any message
            match status {
                AltCreationStatus::Starting => {
                    log::error!("GUI_UPDATE: Received AltCreationStatus::Starting");
                    self.alt_creation_in_progress = true;
                    let msg = "GUI_UPDATE: ALT creation process STARTED!".to_string();
                    self.alt_view_status = msg.clone();
                    self.alt_log_messages.push(msg);
                    self.last_operation_result = Some(Ok("ALT creation initiated by task...".to_string()));
                }
                AltCreationStatus::Sending(msg_desc) => { // Renamed `msg` to `msg_desc` to avoid conflict if used inside
                    log::error!("GUI_UPDATE: Received AltCreationStatus::Sending: {}", msg_desc);
                    self.alt_creation_in_progress = true;
                    self.alt_view_status = msg_desc.clone(); // Show current step description
                    self.alt_log_messages.push(format!("[Sending] {}", msg_desc));
                }
                AltCreationStatus::Confirmed(desc, sig) => {
                    log::error!("GUI_UPDATE: Received AltCreationStatus::Confirmed: {} - {}", desc, sig);
                    self.alt_creation_in_progress = true; // Still in progress until final success/failure
                    let msg_confirm = format!("Transaction Confirmed ({}): {}", desc, sig);
                    self.alt_view_status = msg_confirm.clone();
                    self.alt_log_messages.push(msg_confirm);
                }
                AltCreationStatus::LogMessage(log_msg) => {
                    log::error!("GUI_UPDATE: Received AltCreationStatus::LogMessage: {}", log_msg);
                    self.alt_log_messages.push(format!("[ALT_TASK_DETAIL]: {}", log_msg));
                    // self.alt_view_status = log_msg; // Optionally update main status with detailed log
                }
                AltCreationStatus::Success(alt_address) => {
                    log::error!("GUI_UPDATE: Received AltCreationStatus::Success: {}", alt_address);
                    self.alt_creation_in_progress = false;
                    self.alt_address = Some(alt_address);
                    let success_msg = format!("ALT successfully created: {}. Saved to alt_address.txt", alt_address);
                    self.alt_view_status = success_msg.clone();
                    self.alt_log_messages.push(success_msg.clone());
                    self.last_operation_result = Some(Ok(success_msg));
                    self.load_available_alts(); // Refresh the loaded ALT in UI
                }
                AltCreationStatus::Failure(err_msg) => {
                    log::error!("GUI_UPDATE: Received AltCreationStatus::Failure: {}", err_msg);
                    self.alt_creation_in_progress = false;
                    let full_err_msg = format!("ALT Creation failed: {}", err_msg);
                    self.alt_view_status = full_err_msg.clone();
                    self.alt_log_messages.push(format!("[ERROR] Create ALT: {}", err_msg));
                    self.last_operation_result = Some(Err(full_err_msg));
                }
            }
            ctx.request_repaint(); // Ensure UI updates for each processed ALT creation message
        }
        // The comments about replacing the while loop are now outdated and removed.
        // The duplicated match block below this point should be implicitly removed by this diff replacing the whole section.

        // Process ALT Deactivation/Close status updates
        while let Ok(result) = self.alt_deactivate_status_receiver.try_recv() {
            self.alt_deactivation_in_progress = false; // Operation step finished or errored overall
            match result {
                Ok(msg) => {
                    self.alt_deactivation_status_message = msg.clone();
                    self.alt_log_messages.push(format!("[De/Close ALT] {}", msg));
                    if msg.contains("Successfully closed ALT") || msg.contains("process completed") {
                        self.last_operation_result = Some(Ok(msg));
                        self.alt_address = None; // Clear the created ALT as it's closed
                        self.load_available_alts(); // Refresh UI
                    } else if msg.contains("Failed") { // If the message itself indicates a failure step
                         self.last_operation_result = Some(Err(msg));
                    } else {
                        // Intermediate success message, keep it as Ok for now
                        self.last_operation_result = Some(Ok(msg));
                    }
                }
                Err(err_msg) => {
                    self.alt_deactivation_status_message = format!("Deactivation/Close Error: {}", err_msg);
                    self.alt_log_messages.push(format!("[ERROR] De/Close ALT: {}", err_msg));
                    self.last_operation_result = Some(Err(err_msg));
                }
            }
        }

        // Process Atomic Buy status updates
        while let Ok(result) = self.atomic_buy_status_receiver.try_recv() {
            self.atomic_buy_in_progress = false; // Operation finished or errored
            match result {
                Ok(msg) => {
                    self.atomic_buy_log_messages.push(format!("[SUCCESS] {}", msg));
                    self.last_operation_result = Some(Ok(msg));
                    // Optionally, trigger a balance refresh for participating wallets
                    self.trigger_balance_fetch(Some(self.atomic_buy_mint_address.clone()));
                }
                Err(err_msg) => {
                    self.atomic_buy_log_messages.push(format!("[ERROR] {}", err_msg));
                    self.last_operation_result = Some(Err(err_msg));
                }
            }
            if let Some(_handle) = self.atomic_buy_task_handle.take() {
                // Task is complete, handle is implicitly dropped.
                // If it could panic, one might join here, but typical flow is it sends result.
                 log::trace!("Atomic buy task handle processed.");
            }
        }

        // Process Atomic Sell status updates
        while let Ok(sell_status_result) = self.atomic_sell_status_receiver.try_recv() {
            // General flag, individual wallet atomic_is_selling is handled below
            self.atomic_sell_in_progress = false;
            match sell_status_result {
                Ok((wallet_address, msg)) => {
                    if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == wallet_address) {
                        wallet.atomic_sell_status_message = Some(msg.clone());
                        wallet.atomic_is_selling = false; // Reset flag for this specific wallet
                    }
                    self.atomic_buy_log_messages.push(format!("[Sell Status: {}]: {}", wallet_address, msg));
                    self.last_operation_result = Some(Ok(format!("Sell for {}: {}", wallet_address, msg)));
                }
                Err((wallet_address, err_msg)) => {
                    if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == wallet_address) {
                        wallet.atomic_sell_status_message = Some(format!("Error: {}", err_msg));
                        wallet.atomic_is_selling = false; // Reset flag for this specific wallet
                    }
                    self.atomic_buy_log_messages.push(format!("[Sell ERROR: {}]: {}", wallet_address, err_msg));
                    self.last_operation_result = Some(Err(format!("Sell for {} failed: {}", wallet_address, err_msg)));
                }
            }
            if let Some(_handle) = self.atomic_sell_task_handle.take() {
                log::trace!("Atomic sell task handle processed for wallet.");
            }
            ctx.request_repaint();
        }
        // ... (other message processing loops)

        // --- Deferred Task Spawning (Copied and adapted) ---
        if let Some(params) = self.start_load_volume_wallets_request.take() {
            self.sim_log_messages.push(format!("Spawning Load Wallets from File task: {}", params.file_path));
            tokio::spawn(async move {
                load_volume_wallets_from_file_task(
                    params.file_path,
                    params.wallets_sender, // This is self.volume_bot_wallets_sender
                    params.status_sender,  // This is self.sim_status_sender
                ).await;
            });
            ctx.request_repaint();
        }

        if let Some(params) = self.start_volume_bot_request.take() {
            self.sim_log_messages.push("Spawning Volume Bot task...".to_string());
            // Assuming the core logic is in a function like this:
            // Adjust the path if your actual simulation function is elsewhere.
            let task = tokio::spawn(async move {
                let task_result = self::run_volume_bot_simulation_task( // Call function in current module
                    params.rpc_url,
                    params.wallets_file_path,
                    params.token_mint_str,
                    params.max_cycles,
                    params.slippage_bps,
                    params.solana_priority_fee,
                    params.initial_buy_sol_amount,
                    params.status_sender.clone(), // Clone for use within the task
                    params.wallets_data_sender,
                    params.use_jito,
                    params.jito_be_url,
                    params.jito_tip_account,
                    params.jito_tip_lamports,
                ).await;

                match task_result {
                    Ok(_) => {
                        // Task completed successfully (it should have sent its own Ok status messages)
                        Ok(())
                    }
                    Err(e) => {
                        log::error!("Volume Bot Simulation Task itself returned an error: {:?}", e);
                        // Send a status message for this unhandled task error
                        let _ = params.status_sender.send(Err(format!("Volume Bot Task unhandled error: {}", e)));
                        Err(e) // Propagate the error as the JoinHandle's result
                    }
                }
            });
            self.sim_task_handle = Some(task);
            self.sim_in_progress = true; // Ensure this is set when task starts
            ctx.request_repaint();
        }
        if let Some(params) = self.start_fetch_balances_request.take() {
            self.sim_log_messages.push(format!("Spawning Fetch Balances task for {} addresses, Target Mint: {}", params.addresses_to_check.len(), params.target_mint));
            // Set balances_loading and expected tasks here, as the UI trigger might have already done it,
            // but this ensures it's set if called from elsewhere or if UI logic changes.
            self.balances_loading = true;
            self.balance_tasks_expected = params.addresses_to_check.len();
            self.balance_tasks_completed = 0;

            // The UI part that triggers this already populates volume_bot_wallet_display_infos with loading states.
            // If not, it should be done here or before setting the request.

            tokio::spawn(async move {
                fetch_balances_task(
                    params.rpc_url,
                    params.addresses_to_check,
                    params.target_mint,
                    params.balance_fetch_sender,
                ).await;
                // Task sends individual results; completion is tracked by balance_tasks_completed vs balance_tasks_expected
            });
            ctx.request_repaint();
        }

        if let Some(params) = self.start_distribute_total_sol_request.take() {
            self.volume_bot_funding_log_messages.push(format!(
                "Spawning Distribute Total SOL task: {} SOL to {} wallets from source.",
                params.total_sol_to_distribute,
                params.wallets_to_fund.len()
            ));
            self.volume_bot_funding_in_progress = true; // Ensure UI shows progress

            tokio::spawn(async move {
                let source_keypair = match crate::commands::utils::load_keypair_from_string(&params.source_funding_keypair_str, "DisperseSource") {
                    Ok(kp) => kp,
                    Err(e) => {
                        let _ = params.status_sender.send(Err(format!("Failed to load source keypair for dispersal: {}", e)));
                        return;
                    }
                };

                let recipient_addresses_str: Vec<String> = params.wallets_to_fund.iter().map(|w| w.public_key.clone()).collect();
                if recipient_addresses_str.is_empty() {
                    let _ = params.status_sender.send(Err("No recipient wallets provided for dispersal.".to_string()));
                    return;
                }

                let num_recipients = recipient_addresses_str.len() as u64;
                if num_recipients == 0 { // Should be caught by is_empty, but as a safeguard
                     let _ = params.status_sender.send(Err("Cannot disperse to zero recipients.".to_string()));
                    return;
                }
                let amount_lamports_each = sol_to_lamports(params.total_sol_to_distribute) / num_recipients;

                if amount_lamports_each == 0 {
                    let _ = params.status_sender.send(Err(format!(
                        "Calculated amount per wallet is 0 lamports (Total SOL: {}, Wallets: {}). Dispersal aborted.",
                        params.total_sol_to_distribute, num_recipients
                    )));
                    return;
                }

                // Call the existing disperse_sol_task
                disperse_sol_task(
                    params.rpc_url,
                    source_keypair, // Now a Keypair
                    recipient_addresses_str, // Vec<String>
                    amount_lamports_each, // u64
                    params.status_sender,
                )
                .await;
            });
            ctx.request_repaint();
        }

        if let Some(params) = self.start_fund_volume_wallets_request.take() {
            self.volume_bot_funding_log_messages.push(format!(
                "Spawning Fund Volume Wallets task: {} lamports each to {} wallets from parent.",
                params.funding_amount_lamports,
                params.wallets_to_fund.len()
            ));
             self.volume_bot_funding_in_progress = true; // Ensure UI shows progress

            tokio::spawn(async move {
                let parent_keypair = match crate::commands::utils::load_keypair_from_string(&params.parent_keypair_str, "FundFromParent") {
                    Ok(kp) => kp,
                    Err(e) => {
                        let _ = params.status_sender.send(Err(format!("Failed to load parent keypair for funding: {}", e)));
                        return;
                    }
                };
                let recipient_addresses_str: Vec<String> = params.wallets_to_fund.iter().map(|w| w.public_key.clone()).collect();
                 if recipient_addresses_str.is_empty() {
                    let _ = params.status_sender.send(Err("No recipient wallets provided for funding from parent.".to_string()));
                    return;
                }
                // Call the existing disperse_sol_task, as it handles batching SOL transfers
                disperse_sol_task(
                    params.rpc_url,
                    parent_keypair,
                    recipient_addresses_str,
                    params.funding_amount_lamports, // This is already per wallet
                    params.status_sender,
                )
                .await;
            });
            ctx.request_repaint();
        }

        if let Some(params) = self.start_gather_all_funds_request.take() {
            self.volume_bot_funding_log_messages.push(format!(
                "Spawning Gather All Funds & Sell task for token: {}",
                params.token_mint_ca_str
            ));
            self.volume_bot_funding_in_progress = true; // Use the same flag for UI feedback

            // Get slippage and priority fee from the main sell settings for the parent's sell operation
            let sell_slippage_percent = self.sell_slippage_percent;
            let sell_priority_fee_lamports = self.sell_priority_fee_lamports;

            tokio::spawn(async move {
                gather_all_funds_and_sell_task(
                    params.rpc_url,
                    params.parent_private_key_str,
                    params.volume_wallets,
                    params.token_mint_ca_str,
                    sell_slippage_percent,
                    sell_priority_fee_lamports,
                    params.status_sender,
                )
                .await;
            });
            ctx.request_repaint();
        }
        // ... (other deferred task spawns)


        // --- Aurora UI Rendering (existing) ---
        egui::SidePanel::left("aurora_side_panel")
            .resizable(false)
            .exact_width(230.0)
            .frame(egui::Frame::none().fill(AURORA_BG_DEEP_SPACE)) // SidePanel itself is transparent to main background
            .show_separator_line(false) // Explicitly remove the separator line
            .show(ctx, |ui| {
                // --- Logo and Title Area (directly on SidePanel's transparent background) ---
                ui.add_space(25.0);
                ui.vertical_centered(|ui| {
                    let logo_image_source = egui::include_image!("../../assets/octop.svg"); // Changed to octop.svg
                    let image_widget = egui::Image::new(logo_image_source)
                        .max_height(60.0) // Increased logo size
                        .max_width(60.0)  // Increased logo size
                        .tint(Color32::WHITE); // Ensure white tint to use SVG's own colors
                    ui.add(image_widget);
                    ui.add_space(5.0);
                    ui.label(egui::RichText::new("octo tools").font(FontId::new(26.0, egui::FontFamily::Name("DigitLoFiGui".into()))).color(AURORA_TEXT_PRIMARY));
                });
                ui.add_space(15.0);
                // No separator here, separator will be part of the glass panel content if needed

                // --- Glass Panel for Buttons ---
                let mut glass_content_frame = egui::Frame::default();
                glass_content_frame.fill = AURORA_BG_DEEP_SPACE; // Match CentralPanel background
                glass_content_frame.rounding = Rounding { nw: 0.0, ne: 12.0, sw: 0.0, se: 12.0 };
                glass_content_frame.stroke = Stroke::NONE; // Manual border drawing
                glass_content_frame.shadow = egui::epaint::Shadow {
                    offset: Vec2::new(2.0, 2.0),
                    blur: 8.0,
                    spread: 1.0,
                    color: Color32::from_black_alpha(90),
                };
                // Inner margin for the content *within* the glass panel
                glass_content_frame.inner_margin = Margin { left: 10.0, right: 10.0, top: 20.0, bottom: 20.0 };


                let glass_panel_response = glass_content_frame.show(ui, |ui| {
                    // The ScrollArea now goes *inside* the glass panel
                    egui::ScrollArea::vertical()
                        .id_source("sidebar_scroll_area_glass")
                        .auto_shrink([false, true]) // Shrink vertically to content
                        .show(ui, |ui| {
                            // --- Tools Section ---
                            ui.label(egui::RichText::new("Tools").font(FontId::new(16.0, egui::FontFamily::Proportional)).color(AURORA_TEXT_MUTED_TEAL).strong());
                            ui.add_space(4.0);
                            ModernApp::nav_button(ui, "../../assets/volume_bot.svg", &mut self.current_view, AppView::VolumeBot);
                            ModernApp::nav_button(ui, "../../assets/atomic_buy.svg", &mut self.current_view, AppView::AtomicBuy);
                            ModernApp::nav_button(ui, "../../assets/launch.svg", &mut self.current_view, AppView::Launch);
                            ModernApp::nav_button(ui, "../../assets/sell.svg", &mut self.current_view, AppView::Sell);
                        ui.add_space(10.0);
                            // ui.separator(); // Removed separator
                            ui.add_space(10.0);

                            // --- Operations Section ---
                            ui.label(egui::RichText::new("Operations").font(FontId::new(16.0, egui::FontFamily::Proportional)).color(AURORA_TEXT_MUTED_TEAL).strong());
                            ui.add_space(4.0);
                            ModernApp::nav_button(ui, "../../assets/gather.svg", &mut self.current_view, AppView::Gather);
                            ModernApp::nav_button(ui, "../../assets/disperse.svg", &mut self.current_view, AppView::Disperse);
                            ModernApp::nav_button(ui, "../../assets/check.svg", &mut self.current_view, AppView::Check);
                            ui.add_space(10.0);
                            // ui.separator(); // Removed separator
                            ui.add_space(10.0);

                            // --- Settings Section ---
                            ui.label(egui::RichText::new("Settings").font(FontId::new(16.0, egui::FontFamily::Proportional)).color(AURORA_TEXT_MUTED_TEAL).strong());
                            ui.add_space(4.0);
                            ModernApp::nav_button(ui, "../../assets/settings.svg", &mut self.current_view, AppView::Settings);
                            ModernApp::nav_button(ui, "../../assets/alt.svg", &mut self.current_view, AppView::Alt);
                            ui.add_space(10.0);
                            // --- Refresh App State Button ---
                            if ui.add_sized(Vec2::new(ui.available_width(), 30.0), // Consistent moderate height
                                egui::Button::new(egui::RichText::new("🔄 Refresh App").font(FontId::new(15.0, egui::FontFamily::Proportional)).color(AURORA_TEXT_PRIMARY))
                                    .fill(AURORA_BG_PANEL.linear_multiply(0.8)) // Slightly different background or keep same as nav items
                                    .stroke(Stroke::new(1.0, AURORA_ACCENT_TEAL)) // Accent stroke
                                ).on_hover_text("Reload wallets, balances, and other app states.").clicked() {
                                self.refresh_application_state();
                            }
                            ui.add_space(10.0); // Space at the end of scrollable content
                    });
                });

                // Manually draw the border for the glass_panel_rect
                let _glass_panel_rect = glass_panel_response.response.rect; // FIX 1: Access .response.rect
                // let painter = ui.painter_at(glass_panel_rect); // Get painter for the specific rect // Border Removed
                // let border_color = Color32::from_rgba_premultiplied(180, 200, 255, 90); // Border Removed
                // let border_thickness = 0.8; // Border Removed
                // let r = 12.0; // radius // Border Removed

                // Define the stroke for the borders
                // let border_stroke: egui::epaint::PathStroke = Stroke::new(border_thickness, border_color).into(); // Border Removed

                // Path 1: Top border + Top-Right arc
                // let mut top_border_path = egui::epaint::PathShape { // Border Removed
                //     points: Vec::new(), // Border Removed
                //     closed: false, // Border Removed
                //     fill: Color32::TRANSPARENT, // Border Removed
                //     stroke: border_stroke.clone(), // Clone border_stroke for the first path // Border Removed
                // }; // Border Removed
                // top_border_path.points.push(glass_panel_rect.left_top()); // Start at actual top-left corner of the panel // Border Removed
                // top_border_path.points.push(Pos2::new(glass_panel_rect.right() - r, glass_panel_rect.top())); // Line to start of TR curve // Border Removed

                // let tr_arc_center = Pos2::new(glass_panel_rect.right() - r, glass_panel_rect.top() + r); // Border Removed
                // for i in 0..=10 { // 11 points for a smooth quarter circle // Border Removed
                    // Angle from -PI/2 (pointing upwards from arc center) to 0 (pointing right from arc center) // Border Removed
                    // let angle = -std::f32::consts::FRAC_PI_2 + (std::f32::consts::FRAC_PI_2 * i as f32 / 10.0); // Border Removed
                    // top_border_path.points.push(tr_arc_center + Vec2::new(angle.cos() * r, angle.sin() * r)); // Border Removed
                // } // Arc ends at (glass_panel_rect.right, glass_panel_rect.top + r) // Border Removed
                // painter.add(top_border_path); // Border Removed

                // Path 2: Bottom border + Bottom-Right arc
                // let mut bottom_border_path = egui::epaint::PathShape { // Border Removed
                //     points: Vec::new(), // Border Removed
                //     closed: false, // Border Removed
                //     fill: Color32::TRANSPARENT, // Border Removed
                //     stroke: border_stroke, // Border Removed
                // }; // Border Removed
                // bottom_border_path.points.push(glass_panel_rect.left_bottom()); // Start at actual bottom-left corner // Border Removed
                // bottom_border_path.points.push(Pos2::new(glass_panel_rect.right() - r, glass_panel_rect.bottom())); // Line to start of BR curve (from panel's perspective) // Border Removed

                // let br_arc_center = Pos2::new(glass_panel_rect.right() - r, glass_panel_rect.bottom() - r); // Border Removed
                // for i in (0..=10).rev() { // Iterate backwards to draw from (right-r, bottom) up to (right, bottom-r) // Border Removed
                    // Angle from PI/2 (pointing downwards from arc center) to 0 (pointing right from arc center) // Border Removed
                    // let angle = std::f32::consts::FRAC_PI_2 * i as f32 / 10.0; // Border Removed
                    // bottom_border_path.points.push(br_arc_center + Vec2::new(angle.cos() * r, angle.sin() * r)); // Border Removed
                // } // Arc ends at (glass_panel_rect.right, glass_panel_rect.bottom - r) // Border Removed
                // painter.add(bottom_border_path); // Border Removed


                            // --- "Hard Refresh App" Button REMOVED ---
                            // ui.add_space(15.0); // Add some space before the refresh button
                            // if ui.add_sized(Vec2::new(ui.available_width() - 20.0, 35.0), // Respect panel inner_margin
                            //     egui::Button::new(egui::RichText::new("🔄 Hard Refresh App").font(FontId::new(15.0, egui::FontFamily::Proportional)).color(AURORA_TEXT_PRIMARY))
                            //         .fill(AURORA_BG_PANEL.linear_multiply(0.9))
                            //         .stroke(Stroke::new(1.0, AURORA_ACCENT_TEAL))
                            //     ).on_hover_text("Resets active tasks, clears logs, reloads wallets, balances, ALTs, and mint keypairs.").clicked() {
                            //     self.refresh_application_state();
                            // }
                            // ui.add_space(15.0); // Add some space after the refresh button before version info

                // Version and other footer items (drawn directly on SidePanel's transparent background)
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(15.0);
                    // --- Live Balance Dashboard Removed from here ---
                    ui.add_space(10.0);
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::TRANSPARENT)) // Ensure panel itself is transparent
            .show(ctx, |ui| {
                // Draw background image if available
                if let Some(texture) = &self.background_image {
                    let texture_id = texture.texture_id(ctx);
                    let image_size = texture.size_vec2();
                    let panel_rect = ui.max_rect(); // The area we want to cover

                    // Calculate the scale factor to cover the panel while maintaining aspect ratio
                    let panel_aspect = panel_rect.width() / panel_rect.height();
                    let image_aspect = image_size.x / image_size.y;

                    let scale = if image_aspect > panel_aspect {
                        // Image is wider than panel aspect ratio -> fit to panel height, so width will be larger than panel width
                        panel_rect.height() / image_size.y
                    } else {
                        // Image is taller or same aspect as panel -> fit to panel width, so height will be larger than panel height
                        panel_rect.width() / image_size.x
                    };

                    let scaled_size = image_size * scale;

                    // Center the scaled image within the panel
                    let draw_rect = Rect::from_center_size(panel_rect.center(), scaled_size);

                    ui.painter().image(
                        texture_id,
                        draw_rect, // Draw the image potentially larger than panel_rect, centered. Painter clips to ui.max_rect().
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), // Full UV of the texture
                        Color32::WHITE, // No tint
                    );
                } else {
                    log::warn!("ModernApp: Background image is None, not drawing.");
                }

                let painter = ui.painter_at(ui.max_rect()); // This painter is for particles
                for p in &self.particles { painter.circle_filled(p.pos, p.radius, p.color); }
                let rect = ui.max_rect(); let _center = rect.center();
                let time_mult = (self.time * 0.2).sin() * 0.5 + 0.5;
                let _max_radius = rect.width().min(rect.height()) * 0.6;
                let _c1 = AURORA_ACCENT_PURPLE.linear_multiply(0.03 + time_mult as f32 * 0.03);
                let _c2 = AURORA_ACCENT_TEAL.linear_multiply(0.02 + (1.0-time_mult) as f32 * 0.02);


                // --- Live Balance Dashboard (Bottom Right) ---
                let screen_rect = ui.ctx().screen_rect(); // Get screen dimensions
                let initial_pos = egui::pos2(screen_rect.right() - 300.0, screen_rect.bottom() - 200.0); // Approx bottom-right
                egui::Area::new("live_balance_dashboard_area".into())
                    .movable(true)
                    .default_pos(initial_pos)
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui_area_for_dashboard| {
                        egui::Frame::none()
                            .fill(AURORA_BG_PANEL.linear_multiply(0.85)) // Slightly translucent panel background
                            .stroke(egui::Stroke::new(1.0, AURORA_STROKE_DARK_BLUE_TINT))
                            .rounding(egui::Rounding::same(8.0)) // Slightly larger rounding for a bigger panel
                            .inner_margin(Margin::same(12.0)) // Increased padding
                            .show(ui_area_for_dashboard, |ui_dashboard| {
                                ui_dashboard.set_max_width(280.0); // Increased width
                                ui_dashboard.strong(egui::RichText::new("Live Balances").color(AURORA_TEXT_PRIMARY).font(FontId::new(18.0, egui::FontFamily::Proportional))); // Increased font size
                                ui_dashboard.separator();
                                ui_dashboard.add_space(8.0); // Increased space

                                let label_font = FontId::new(15.0, egui::FontFamily::Proportional); // Increased font size
                                let value_font = FontId::new(16.0, egui::FontFamily::Monospace); // Increased font size

                                ui_dashboard.horizontal(|ui_bal| {
                                    ui_bal.label(egui::RichText::new("Parent:").font(label_font.clone()).color(AURORA_TEXT_MUTED_TEAL));
                                    ui_bal.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_val| {
                                        ui_val.label(egui::RichText::new(format!("◎ {:.4}", self.parent_sol_balance_display.unwrap_or(0.0))).font(value_font.clone()).color(AURORA_ACCENT_TEAL));
                                    });
                                });

                                let other_wallets_sol: f64 = self.wallets.iter()
                                    .filter(|w| !w.is_parent)
                                    .filter_map(|w| w.sol_balance)
                                    .sum();

                                ui_dashboard.horizontal(|ui_bal| {
                                    ui_bal.label(egui::RichText::new("Others:").font(label_font.clone()).color(AURORA_TEXT_MUTED_TEAL));
                                    ui_bal.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_val| {
                                        ui_val.label(egui::RichText::new(format!("◎ {:.4}", other_wallets_sol)).font(value_font.clone()).color(AURORA_TEXT_PRIMARY));
                                    });
                                });

                                ui_dashboard.horizontal(|ui_bal| {
                                    ui_bal.label(egui::RichText::new("Total:").font(label_font.clone()).color(AURORA_TEXT_MUTED_TEAL));
                                    ui_bal.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_val| {
                                        ui_val.label(egui::RichText::new(format!("◎ {:.4}", self.total_sol_balance_display.unwrap_or(0.0))).font(value_font.clone()).color(AURORA_ACCENT_PURPLE).strong());
                                    });
                                });
                            });
                    });

                // --- Main Content Area (with placeholders for views) ---
                match self.current_view {
                    AppView::Launch => self.show_launch_view(ui),
                    AppView::Sell => self.show_sell_view(ui),
                    AppView::AtomicBuy => self.show_atomic_buy_view(ui),
                    AppView::Disperse => self.show_disperse_view(ui),
                    AppView::Gather => self.show_gather_view(ui),
                    AppView::Alt => self.show_alt_view(ui),
                    AppView::VolumeBot => self.show_volume_bot_view(ui),
                    AppView::Check => self.show_check_view(ui),
                    AppView::Settings => self.show_settings_view(ui),
                    // AppView::Alt => self.show_alt_view(ui), // Removed duplicate entry for Alt view
                    // Add other views as needed
                }
            });

        self.show_status_bar(ctx);
    }
}

// Helper functions for the electric theme

fn show_section_header(ui: &mut egui::Ui, title: &str, original_color: egui::Color32) { // Renamed color to original_color
    ui.horizontal(|ui| {
        let text = egui::RichText::new(title)
            .color(Color32::WHITE) // Changed to WHITE
            .font(egui::FontId::new(22.0, egui::FontFamily::Proportional)) // Reverted to Proportional
            .strong();

        ui.label(text);

        // Draw a line after the label
        let title_rect = ui.min_rect();
        let padding = 10.0;
        let line_start = egui::pos2(title_rect.max.x + padding, title_rect.center().y);
        let line_end = egui::pos2(ui.max_rect().max.x - padding, title_rect.center().y);

        // Change the line color to a variant of white
        ui.painter().line_segment(
            [line_start, line_end],
            egui::Stroke::new(1.5, Color32::WHITE.linear_multiply(0.5)), // White line, slightly transparent
        );
    });
}

fn create_electric_frame() -> egui::Frame {
    egui::Frame::none()
        .fill(Color32::from_rgba_premultiplied(20, 30, 55, 128)) // AURORA_BG_PANEL with 50% alpha
        .stroke(egui::Stroke::new(1.0, AURORA_STROKE_DARK_BLUE_TINT))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(15.0)
}

fn electric_label_text(text: &str) -> egui::RichText {
    egui::RichText::new(text)
        .color(AURORA_TEXT_MUTED_TEAL)
        .font(egui::FontId::new(14.0, egui::FontFamily::Proportional))
}

fn add_labeled_electric_input(ui: &mut egui::Ui, label: &str, value: &mut String, is_password: bool) {
    ui.horizontal(|ui| {
        ui.label(electric_label_text(label));

        // Wrap TextEdit in a custom frame for background control
        egui::Frame::none()
            .fill(AURORA_BG_PANEL) // Set desired dark blue background
            .stroke(Stroke::new(1.5, AURORA_ACCENT_NEON_BLUE.linear_multiply(0.7))) // Use new prominent border
            .rounding(ui.style().visuals.widgets.inactive.rounding) // Use existing rounding
            .inner_margin(egui::Margin::symmetric(4.0, 2.0)) // Small inner padding for the TextEdit
            .show(ui, |ui_frame| {
                let mut text_edit = egui::TextEdit::singleline(value)
                    .font(egui::FontId::new(14.0, egui::FontFamily::Proportional))
                    .text_color(AURORA_TEXT_PRIMARY)
                    .frame(false) // Make TextEdit frameless to inherit background
                    .desired_width(ui_frame.available_width());

                if is_password {
                    text_edit = text_edit.password(true);
                }

                ui_frame.add(text_edit);
            });
    });
}

fn create_electric_button(ui: &mut egui::Ui, text: &str, min_size_override: Option<Vec2>) -> egui::Response {
    // let padding = egui::vec2(10.0, 5.0); // padding seems unused now

    let button = egui::Button::new(
            egui::RichText::new(text)
                .color(AURORA_TEXT_PRIMARY)
        )
        .min_size(min_size_override.unwrap_or_else(|| egui::vec2(100.0, 30.0))) // Use override or default
        .fill(AURORA_BG_PANEL)
        .stroke(egui::Stroke::new(1.0, AURORA_ACCENT_NEON_BLUE))
        .rounding(egui::Rounding::same(4.0));

    ui.add(button)
}

fn display_derived_pubkey(ui: &mut egui::Ui, label: &str, private_key_str: &str) {
    if let Some(pubkey) = crate::models::settings::AppSettings::get_pubkey_from_privkey_str(private_key_str) {
        ui.horizontal(|ui| {
            ui.label(electric_label_text(label));
            ui.monospace(
                egui::RichText::new(&pubkey)
                    .color(AURORA_TEXT_PRIMARY)
                    .font(egui::FontId::new(14.0, egui::FontFamily::Monospace))
            );
        });
    }
}
