use crate::errors::{Result, PumpfunError};
use super::utils::load_keypair_from_string;
use crate::wallet::{load_zombie_wallets_from_file};
use crate::models::settings::AppSettings; // <-- Correct import path
use solana_client::nonblocking::rpc_client::RpcClient;
// use solana_client::rpc_request::TokenAccountsFilter; // Unused
// use spl_token::{instruction::close_account, state::Account as TokenAccount}; // Unused
// use solana_sdk::program_pack::Pack; // Unused
use solana_sdk::message::{v0::Message as MessageV0}; // Removed unused VersionedMessage
// use anyhow::anyhow; // Unused import of macro, Result is used from crate::errors
// use solana_account_decoder::{UiAccountEncoding, UiAccountData}; // Unused
// use base64::Engine; // Unused
use solana_sdk::{
    signature::{Signer, Keypair},
    transaction::Transaction,
    system_instruction,
    native_token,
    // pubkey::Pubkey, // Unused
};
use log::{info, error, warn}; // Removed unused debug
// use futures::future::join_all; // Unused
use tokio::time::Duration;
use console::{style};
use std::sync::Arc;
// use std::str::FromStr; // Unused
// use bs58; // Unused

// Helper function to load keypair from private key string
// load_keypair_from_string moved to src/key_utils.rs


/// Gather SOL from all simulation wallets and the dev wallet to the parent wallet defined in settings.
pub async fn gather_sol(_keys_path_arg: Option<&str>) -> Result<()> { // Prefixed unused arg
    // Load settings
    let settings = AppSettings::load();

    // Load Parent Keypair from settings
    info!("[Gather SOL] Attempting to load Parent wallet. Private key from settings: '{}'", &settings.parent_wallet_private_key);
    let parent_keypair = load_keypair_from_string(&settings.parent_wallet_private_key, "Parent")?;
    let parent_pubkey = parent_keypair.pubkey();
    info!("[Gather SOL] Parent wallet loaded. Pubkey: {}", parent_pubkey);

    // Load Minter Keypair from settings (optional)
    info!("[Gather SOL] Attempting to load Dev (Minter) wallet. Private key from settings: '{}'", &settings.dev_wallet_private_key);
    let minter_keypair_opt = match load_keypair_from_string(&settings.dev_wallet_private_key, "Dev") {
        Ok(kp) => Some(kp),
        Err(e) => {
            warn!("Could not load Minter wallet, will skip gathering from it: {}", e);
            None
        }
    };

    // Setup RPC Client
    let rpc_url = settings.solana_rpc_url.clone();
    let commitment_config = settings.get_commitment_config()?;
    let rpc_client = Arc::new(RpcClient::new_with_commitment(rpc_url, commitment_config));

    println!("Gathering SOL to Parent Wallet...");
    println!("{} {}", style("Parent Wallet:").cyan(), parent_pubkey);

    // Load simulation wallets from the path specified in settings
    let sim_wallets_path = &settings.keys_path;
    println!("Loading simulation wallets from: {}", sim_wallets_path);
    let simulation_keypairs = load_zombie_wallets_from_file(Some(sim_wallets_path)) // Use path from settings
        .map_err(|e| PumpfunError::Wallet(format!("Failed to load simulation wallets from {}: {}", sim_wallets_path, e)))?;

    // Combine Minter (if loaded) and Simulation wallets into a single list of sources
    let mut source_keypairs: Vec<Keypair> = Vec::new();
    if let Some(minter_kp) = minter_keypair_opt {
         // Avoid adding parent if it's also the minter
        let minter_pubkey = minter_kp.pubkey();
        info!("[Gather SOL] Dev (Minter) wallet loaded. Pubkey: {}", minter_pubkey);
        if minter_pubkey != parent_pubkey {
            println!("Including Minter wallet: {}", minter_pubkey);
            source_keypairs.push(minter_kp);
        } else {
             println!("Minter wallet ({}) is the same as Parent wallet ({}), skipping.", minter_pubkey, parent_pubkey);
        }
    }
    for sim_kp in simulation_keypairs {
        // Avoid adding parent if it's also in the sim list
        if sim_kp.pubkey() != parent_pubkey {
             source_keypairs.push(sim_kp);
        } else {
            println!("Simulation wallet {} is the same as Parent wallet, skipping.", sim_kp.pubkey());
        }
    }


    if source_keypairs.is_empty() {
        println!("No source wallets (Minter or Simulation) found to gather from (excluding Parent).");
        return Ok(());
    } else {
         println!("Found {} source wallets to gather from.", source_keypairs.len());
    }

    // Process wallets sequentially
    for source_keypair in source_keypairs { // Iterate through combined keypairs
        let rpc_client = rpc_client.clone();
        let parent_pubkey = parent_pubkey.clone(); // Clone parent_pubkey
        let source_pubkey = source_keypair.pubkey(); // Get pubkey

        // Skip if source is the same as parent
        if source_pubkey == parent_pubkey {
             info!("[{}] Skipping: Source is parent.", source_pubkey);
             continue; // Move to the next wallet in the loop
        }

        info!("[{}] Processing wallet for simple SOL transfer...", source_pubkey);

        // --- Step 1: Get Balance ---
        let balance = match rpc_client.get_balance(&source_pubkey).await {
             Ok(b) => b,
             Err(e) => {
                 error!("[{}] Failed to get balance: {}", source_pubkey, e);
                 // Add a delay even on failure before trying next wallet
                 tokio::time::sleep(Duration::from_millis(200)).await;
                 continue; // Skip to next wallet
             }
        };
        info!("[{}] Current balance: {}", source_pubkey, balance);

        // If balance is zero or very low, skip further processing
        if balance <= 5000 { // Keep 0.000005 SOL for potential future fees if needed, or adjust threshold
            info!("[{}] Balance is too low ({}), skipping transfer.", source_pubkey, balance);
            // Add a small delay before next wallet
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        // --- Step 2: Get Blockhash and Estimate Fee ---
        let latest_blockhash = match rpc_client.get_latest_blockhash().await {
             Ok(bh) => bh,
             Err(e) => {
                 error!("[{}] Failed to get blockhash: {}", source_pubkey, e);
                 tokio::time::sleep(Duration::from_millis(200)).await;
                 continue; // Skip to next wallet
             }
        };
        // info!("[{}] Got blockhash {}", source_pubkey, latest_blockhash); // Less verbose logging

        // Create a dummy transfer instruction just for fee calculation
        let dummy_instruction = system_instruction::transfer(&source_pubkey, &parent_pubkey, 1); // Amount doesn't matter for fee calc
        let fee_calc_message = match MessageV0::try_compile(&source_pubkey, &[dummy_instruction], &[], latest_blockhash) {
             Ok(msg) => msg,
             Err(e) => {
                 error!("[{}] Failed to compile fee calc message: {}", source_pubkey, e);
                 tokio::time::sleep(Duration::from_millis(200)).await;
                 continue; // Skip to next wallet
             }
        };
        let fee = match rpc_client.get_fee_for_message(&fee_calc_message).await {
             Ok(f) => f,
             Err(e) => {
                 error!("[{}] Failed to get fee: {}", source_pubkey, e);
                 // Use a default fee if estimation fails, or skip
                 warn!("[{}] Using default fee estimate (5000 lamports) due to error.", source_pubkey);
                 5000 // Default fee, adjust if necessary
                 // Alternatively: continue;
             }
        };
        info!("[{}] Estimated transfer fee: {}", source_pubkey, fee);

        // --- Step 3: Calculate Amount and Send Transaction ---
        if balance <= fee {
             info!("[{}] Skipping transfer: Balance ({}) <= fee ({})", source_pubkey, balance, fee);
        } else {
            let lamports_to_send = balance - fee;
            info!("[{}] Calculated transfer amount: {}", source_pubkey, lamports_to_send);

            let transfer_instruction = system_instruction::transfer(&source_pubkey, &parent_pubkey, lamports_to_send);
            let transaction = Transaction::new_signed_with_payer(&[transfer_instruction], Some(&source_pubkey), &[&source_keypair], latest_blockhash);
            // info!("[{}] Built simple transfer transaction", source_pubkey); // Less verbose

            // Send and Confirm
            match rpc_client.send_and_confirm_transaction_with_spinner(&transaction).await {
                Ok(signature) => {
                    info!(
                        "[{}] Successfully transferred {:.9} SOL. Signature: {}",
                        source_pubkey, lamports_to_send as f64 / native_token::LAMPORTS_PER_SOL as f64, signature
                    );
                }
                Err(e) => {
                    error!("[{}] SOL transfer transaction failed: {}", source_pubkey, e);
                    // Log error but continue to the next wallet
                }
            }
        }

        // Add a delay before processing the next wallet
        tokio::time::sleep(Duration::from_millis(500)).await; // Keep or adjust delay

    } // End of sequential for loop

    println!("Finished simple SOL gathering process.");
    Ok(())
}