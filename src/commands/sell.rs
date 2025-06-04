use crate::api::pumpfun::sell_token as api_sell_token;
use crate::config::{get_commitment_config, get_rpc_url};
use crate::errors::{PumpfunError, Result};
use crate::wallet::{get_wallet_keypair, load_zombie_wallets_from_file};
use crate::utils::balance::{get_token_balance, get_sol_balance};
use crate::utils::transaction::{sign_and_send_versioned_transaction};
use console::Style;
use log::{info, warn};
use solana_program::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::str::FromStr;
use bs58;

/// Sell a token on Pump.fun
pub async fn sell_token(
    mint_address: String,
    amount: Option<f64>,
    slippage: Option<f64>,
    priority_fee: Option<u64>,
    zombie_index: Option<usize>,
    keys_path_arg: Option<&str>,
    sell_all_zombies: bool,
) -> Result<()> {
    // Initialize the console styling
    let info_style = Style::new().cyan();
    let _success_style = Style::new().green().bold();
    
    println!("\n{}", info_style.apply_to("💰 Preparing to sell token on Pump.fun...").bold());
    
    // Create non-blocking RPC client
    let rpc_url = get_rpc_url();
    let commitment_config = get_commitment_config();
    let _rpc_client = solana_client::nonblocking::rpc_client::RpcClient::new_with_commitment(
        rpc_url, 
        commitment_config
    );
    
    // Parse the mint address
    let mint_pubkey = Pubkey::from_str(&mint_address)?;
    println!("Token Mint Address: {}", mint_pubkey);
    
    // Load zombie wallets
    let zombies = load_zombie_wallets_from_file(keys_path_arg)?;
    let mut wallets_to_sell: Vec<String> = Vec::new(); // Store private key strings

    // Determine which wallets to use for selling
    if sell_all_zombies {
        // Use all zombie wallets
        println!("{}", info_style.apply_to("Using all zombie wallets for selling..."));
        for zombie in zombies {
            wallets_to_sell.push(bs58::encode(zombie.to_bytes()).into_string());
        }
    } else if let Some(index) = zombie_index {
        // Use the wallet specified by index
        if let Some(zombie) = zombies.get(index) {
            wallets_to_sell.push(bs58::encode(zombie.to_bytes()).into_string());
        } else {
            return Err(PumpfunError::Wallet(format!("Invalid zombie wallet index: {}", index)));
        }
    } else {
        // If no specific index and not using all, default to the parent wallet
        println!("{}", info_style.apply_to("Using parent wallet for selling..."));
        let parent_keypair = get_wallet_keypair()?;
        wallets_to_sell.push(bs58::encode(parent_keypair.to_bytes()).into_string());
    }

    if wallets_to_sell.is_empty() {
        println!("No wallets selected for selling.");
        return Ok(());
    }

    // Use user-provided amount or default to all tokens
    let percentage_to_sell = if let Some(amt) = amount {
        amt.min(100.0) // Cap at 100% which means all tokens
    } else {
        100.0 // Default to selling 100% (all tokens)
    };
    
    println!("Attempting to sell {}% of token {} using {} wallet(s).", 
             percentage_to_sell, mint_pubkey, wallets_to_sell.len());

    let error_style = Style::new().red();
    let success_style = Style::new().green().bold(); // Added success style
    let mut sold_successfully = false; // Flag to track if we sold

    for wallet_pk_str in wallets_to_sell {
        if sold_successfully { // Check if we already sold
            break;
        }

        // Recreate Keypair from stored private key string
        let keypair_bytes = match bs58::decode(&wallet_pk_str).into_vec() {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("{}", error_style.apply_to(format!("Error decoding private key: {}", e)));
                continue; // Skip to next wallet
            }
        };
        let wallet = match Keypair::from_bytes(&keypair_bytes) {
            Ok(kp) => kp,
            Err(e) => {
                eprintln!("{}", error_style.apply_to(format!("Error creating keypair from bytes: {}", e)));
                continue; // Skip to next wallet
            }
        };

        let wallet_pubkey = wallet.pubkey();
        info!("Processing wallet: {}", wallet_pubkey);

        // --- Check Token Balance --- 
        let token_balance = match get_token_balance(&_rpc_client, &wallet_pubkey, &mint_pubkey).await {
            Ok(balance) => balance,
            Err(e) => {
                // Log error but continue to next wallet (e.g., ATA might not exist)
                info!("Could not get token balance for {}: {}. Skipping.", wallet_pubkey, e);
                continue;
            }
        };

        if token_balance == 0 {
            info!("Wallet {} has 0 balance for mint {}. Skipping.", wallet_pubkey, mint_pubkey);
            continue; // Skip to next wallet
        }

        println!("Found token balance {} in wallet {}. Attempting sell...", token_balance, wallet_pubkey);

        // --- Check SOL Balance (Optional but good practice) ---
        match get_sol_balance(&_rpc_client, &wallet_pubkey).await {
            Ok(sol_balance) => {
                let min_sol_balance = LAMPORTS_PER_SOL / 100; // 0.01 SOL
                if sol_balance < min_sol_balance {
                    warn!("Wallet {} has low SOL balance ({:.6} SOL). Transaction might fail.", 
                          wallet_pubkey, sol_balance as f64 / LAMPORTS_PER_SOL as f64);
                }
            },
            Err(e) => {
                 warn!("Could not check SOL balance for {}: {}. Proceeding anyway.", wallet_pubkey, e);
            }
        }
        
        // --- Prepare Sell Args ---
        // Default slippage to 10% if not provided
        let slippage_percent = slippage.unwrap_or(10.0) as u8;
        // Default priority fee to 100,000 lamports if not provided
        let priority_fee_lamports = priority_fee.unwrap_or(100_000) as f64 / LAMPORTS_PER_SOL as f64;
        // Use the percentage_to_sell calculated earlier (defaults to 100% if amount wasn't specified)

        println!("  Sell Args: {}% tokens, slippage {}%, priority fee {} SOL", 
                 percentage_to_sell, slippage_percent, priority_fee_lamports);

        // --- Call API and Send Transaction --- 
        println!("  🚀 Executing sell transaction for {}...", wallet_pubkey);
        match api_sell_token(
            &wallet,
            &mint_pubkey,
            percentage_to_sell, // Pass the calculated percentage
            slippage_percent,
            priority_fee_lamports,
        ).await {
            Ok(versioned_transaction) => {
                match sign_and_send_versioned_transaction(
                    &_rpc_client,
                    versioned_transaction,
                    &[&wallet],
                ).await {
                    Ok(signature) => {
                        println!("  {} {}", 
                            success_style.apply_to("✓ Transaction sent:"),
                            signature
                        );
                        println!("  Explorer: https://explorer.solana.com/tx/{}", signature);
                        sold_successfully = true; // Set flag to stop looping
                        break; // Exit loop after successful send
                    },
                    Err(e) => {
                         eprintln!("{}", error_style.apply_to(
                             format!("Sell transaction failed for {}: {}. Trying next wallet...", wallet_pubkey, e)
                         ));
                        // Continue to the next wallet
                    }
                }
            },
            Err(e) => {
                eprintln!("{}", error_style.apply_to(
                    format!("Failed to create sell transaction via API for {}: {}. Trying next wallet...", wallet_pubkey, e)
                ));
                // Continue to the next wallet
            }
        }
    } // End of loop through wallets

    if !sold_successfully && sell_all_zombies { // Check flag only if --all-zombies was used
         println!("\n{} No suitable zombie wallet found with tokens to sell, or all attempts failed.", info_style.apply_to("ℹ️"));
    }

    Ok(())
}