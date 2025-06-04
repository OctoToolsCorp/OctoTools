// src/utils/mod.rs

// --- Use statements from src/utils.rs ---
use crate::errors::{PumpfunError, Result}; // Assuming crate::errors is accessible
use log::{debug, error};
use rand::Rng;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    pubkey::Pubkey,
    signature::Keypair,
    transaction::VersionedTransaction,
};
use console::Style;
use std::time::Duration;

// --- Submodule declarations ---
pub mod balance;
pub mod transaction; // From src/utils.rs
pub mod transaction_history; // Newly added

// --- Re-export key functions ---
pub use balance::{get_sol_balance, get_token_balance, get_token_decimals, format_token_amount, check_token_account_exists, check_token_exists, get_token_account};
// Add other re-exports from transaction and transaction_history as needed.

// --- Utility functions from src/utils.rs (original content) ---

/// Sleep for the specified milliseconds
pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Get a random number between min and max (inclusive)
pub fn get_random_number(min: usize, max: usize) -> usize {
    let mut rng = rand::thread_rng();
    rng.gen_range(min..=max)
}

/// Find the mint authority for a given program ID
pub fn get_mint_authority(program_id: &Pubkey) -> Result<Pubkey> {
    let (mint_authority, _) = Pubkey::find_program_address(&[b"mint-authority"], program_id);
    debug!("Mint authority: {}", mint_authority);
    Ok(mint_authority)
}

/// Find the bonding curve for a given mint and program ID
pub fn get_bonding_curve(mint: &Pubkey, program_id: &Pubkey) -> Result<Pubkey> {
    let (bonding_curve, _) = Pubkey::find_program_address(
        &[b"bonding-curve", mint.as_ref()],
        program_id,
    );
    debug!("Bonding curve: {}", bonding_curve);
    Ok(bonding_curve)
}

/// Find the metadata account for a given mint and program ID
pub fn get_metadata_account(mint: &Pubkey, program_id: &Pubkey) -> Result<Pubkey> {
    let (metadata_account, _) = Pubkey::find_program_address(
        &[b"metadata", mint.as_ref()],
        program_id,
    );
    debug!("Metadata account: {}", metadata_account);
    Ok(metadata_account)
}

/// Calculate the amount of tokens that can be bought with a given amount of SOL
pub async fn get_amount_in(amount_out: f64, reserve_in: f64, reserve_out: f64) -> Result<f64> {
    let numerator = reserve_in * amount_out * 1000.0;
    let denominator = (reserve_out - amount_out) * 997.0;
    if denominator <= 0.0 {
        return Err(PumpfunError::InvalidParameter("Invalid denominator in price calculation".to_string()));
    }
    let amount_in = numerator / denominator;
    Ok(amount_in)
}

/// Calculate token amounts evenly distributed among wallets
pub async fn calculate_token_amounts(total_amount: f64, count: usize) -> Result<Vec<f64>> {
    if count == 0 {
        return Err(PumpfunError::InvalidParameter("Wallet count must be greater than zero".to_string()));
    }
    
    let equal_amount = total_amount / count as f64;
    let mut token_amounts = Vec::new();
    
    for _ in 0..count {
        // Add some randomness: between 90% and 110% of the equal amount
        let random_factor = 0.9 + (rand::random::<f64>() * 0.2);
        let token_amount = equal_amount * random_factor;
        token_amounts.push(token_amount);
    }
    
    // Make sure we don't exceed the total amount
    let actual_total: f64 = token_amounts.iter().sum();
    if actual_total > total_amount {
        // Scale down proportionally
        let scale_factor = total_amount / actual_total;
        for amount in token_amounts.iter_mut() {
            *amount *= scale_factor;
        }
    }
    
    Ok(token_amounts)
}

/// Calculate token distribution with min/max percentages
pub fn calculate_token_amounts_min_max(
    total_tokens: f64, 
    wallet_count: usize,
    min_percent: f64, 
    max_percent: f64
) -> Result<Vec<f64>> {
    if wallet_count == 0 {
        return Err(PumpfunError::InvalidParameter("Wallet count must be greater than 0".to_string()));
    }
    
    if min_percent > max_percent {
        return Err(PumpfunError::InvalidParameter(
            format!("Min percent ({}) cannot be greater than max percent ({})", min_percent, max_percent)
        ));
    }
    
    // Available token percentage to distribute
    let total_percent = 100.0;
    let min_total = min_percent * wallet_count as f64;
    
    if min_total > total_percent {
        return Err(PumpfunError::InvalidParameter(
            format!("Minimum total ({}) exceeds 100% with {} wallets", min_total, wallet_count)
        ));
    }
    
    let mut rng = rand::thread_rng();
    let mut amounts = Vec::with_capacity(wallet_count);
    let mut remaining = total_tokens;
    let min_amount = total_tokens * (min_percent / 100.0);
    let max_amount = total_tokens * (max_percent / 100.0);
    
    // First pass: Assign minimum amounts
    for _ in 0..wallet_count {
        amounts.push(min_amount);
        remaining -= min_amount;
    }
    
    // Second pass: Distribute remaining randomly
    let mut weights = Vec::with_capacity(wallet_count);
    for _ in 0..wallet_count {
        weights.push(rng.gen_range(1.0..100.0));
    }
    
    let total_weight: f64 = weights.iter().sum();
    
    for i in 0..wallet_count {
        // Calculate proportional share
        let share = weights[i] / total_weight;
        let additional = remaining * share;
        
        // Make sure we don't exceed max_amount
        let new_amount = (amounts[i] + additional).min(max_amount);
        let added = new_amount - amounts[i];
        
        amounts[i] = new_amount;
        remaining -= added;
    }
    
    // Distribute any remaining to the first wallet
    if remaining > 0.0 && !amounts.is_empty() {
        // Find the wallet with the smallest amount
        let min_idx = amounts.iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);
            
        amounts[min_idx] += remaining;
    }
    
    Ok(amounts)
}

/// Simulate SOL amounts required for buying tokens
pub async fn get_sol_amounts_simulate(
    init_sol_reserve: f64,
    init_token_reserve: f64,
    token_list: &[f64],
) -> Result<Vec<f64>> {
    let mut token_reserve = init_token_reserve;
    let mut sol_reserve = init_sol_reserve;
    let mut sol_amounts = Vec::new();
    
    for token_amount in token_list {
        let sol_amount = get_amount_in(*token_amount, sol_reserve, token_reserve).await? + 0.03;
        sol_amounts.push(sol_amount);
        
        token_reserve -= token_amount;
        sol_reserve += sol_amount;
    }
    
    Ok(sol_amounts)
}

/// Get wallet balance in SOL
pub async fn get_wallet_balance(rpc_client: &RpcClient, wallet_pubkey: &Pubkey) -> Result<f64> {
    match rpc_client.get_balance(wallet_pubkey).await {
        Ok(balance) => {
            let sol_balance = balance as f64 / 1_000_000_000.0; // Convert lamports to SOL
            Ok(sol_balance)
        }
        Err(e) => {
            error!("Failed to get wallet balance: {}", e);
            Err(PumpfunError::SolanaClient(e))
        }
    }
}

/// Format a SOL balance for display
pub fn format_sol_balance(balance: f64) -> String {
    format!("{:.5} SOL", balance)
}

/// Format a token balance for display
pub fn format_token_balance(balance: u64, decimals: u8) -> String {
    let decimal_factor = 10u64.pow(decimals as u32) as f64;
    let token_amount = balance as f64 / decimal_factor;
    format!("{:.2}", token_amount)
}

/// Add priority fee to transaction
pub fn with_compute_budget(priority_fee: u64) -> solana_sdk::instruction::Instruction {
    ComputeBudgetInstruction::set_compute_unit_price(priority_fee)
}

/// Check if a transaction is confirmed
pub async fn wait_for_transaction_confirmation(
    rpc_client: &RpcClient,
    signature_str: &str,
) -> Result<bool> {
    // Retry up to 30 times (30 seconds total)
    for _ in 0..30 {
        let signature = signature_str.parse().map_err(|_| 
            PumpfunError::Transaction(format!("Invalid signature format: {}", signature_str))
        )?;
        
        match rpc_client.get_signature_status(&signature).await {
            Ok(Some(status)) => {
                if status.is_ok() {
                    return Ok(true);
                } else {
                    return Err(PumpfunError::Transaction(format!(
                        "Transaction failed with status: {:?}",
                        status
                    )));
                }
            }
            Ok(None) => { 
                debug!("Signature status not found yet for {}, retrying...", signature_str);
            },
            Err(e) => { 
                 error!("Error getting signature status: {}", e);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }

    Err(PumpfunError::Transaction(
        "Transaction confirmation timed out".to_string(),
    ))
}

/// Submit a transaction and confirm it
pub async fn submit_and_confirm_transaction(
    rpc_client: &RpcClient,
    transaction: VersionedTransaction,
    label: &str,
) -> Result<String> {
    let info_style = Style::new().cyan();
    let success_style = Style::new().green().bold();
    
    println!("{} {}", info_style.apply_to("Sending transaction:"), label);
    
    let signature = match rpc_client.send_transaction(&transaction).await {
        Ok(sig) => sig.to_string(),
        Err(e) => {
            error!("Failed to send transaction: {}", e);
            return Err(PumpfunError::from(e));
        }
    };
    
    println!("{} {}", info_style.apply_to("Transaction signature:"), signature);
    println!("{}", info_style.apply_to("Waiting for confirmation..."));
    
    // Wait for confirmation
    match wait_for_transaction_confirmation(rpc_client, &signature).await {
        Ok(_) => {
            println!("{} Transaction confirmed!", success_style.apply_to("✓"));
            debug!("Transaction {} confirmed", signature);
            Ok(signature)
        }
        Err(e) => {
            error!("Transaction confirmation failed: {}", e);
            Err(e)
        }
    }
}

/// Create keypair from seed phrase
pub fn keypair_from_seed(seed_phrase: &str) -> Result<Keypair> {
    let seed = seed_phrase.as_bytes();
    
    if seed.len() < 32 {
        return Err(PumpfunError::InvalidParameter(
            "Seed phrase must be at least 32 bytes".to_string()
        ));
    }
    
    let mut seed_bytes = [0u8; 32];
    for (i, &byte) in seed.iter().enumerate().take(32) {
        seed_bytes[i] = byte;
    }
    
    Ok(Keypair::from_bytes(&seed_bytes)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to create keypair from seed: {}", e)))?)
}