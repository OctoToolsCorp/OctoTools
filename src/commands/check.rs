use crate::errors::{Result}; // Removed unused PumpfunError
use crate::wallet::{get_wallet_keypair, load_zombie_wallets_from_file};
use crate::utils::balance::{get_sol_balance, get_token_balance};
use crate::models::settings::AppSettings; // <-- Correct import path
use console::{Style, style};
use prettytable::{Table, row};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use std::str::FromStr;
use std::env; // Keep for TOKEN_MINT_ADDRESS for now
use bs58; // Import bs58

/// Check wallet balances
pub async fn check_wallets(keys_path_arg: Option<&str>) -> Result<()> {
    // Load settings
    let settings = AppSettings::load();

    // Initialize the console styling
    let info_style = Style::new().cyan();

    println!("\n{}", info_style.apply_to("🔍 Checking Wallet Balances...").bold());

    // Initialize RPC client using settings
    let rpc_url = settings.solana_rpc_url.clone();
    let commitment_config = settings.get_commitment_config()?;
    let rpc_client = RpcClient::new_with_commitment(rpc_url, commitment_config);

    // Get mint address from token keypair or environment
    // TODO: Consider moving TOKEN_MINT_ADDRESS to settings as well
    let mint_pubkey_str = env::var("TOKEN_MINT_ADDRESS").ok(); // Try to get from env instead
    if mint_pubkey_str.is_none() {
        println!("{} Please set TOKEN_MINT_ADDRESS env var to check token balances.", style("⚠️").yellow());
        return Ok(()); // Cannot check token balance without mint
    }
    let mint_pubkey = Pubkey::from_str(&mint_pubkey_str.unwrap())?;
    println!("{} {}", info_style.apply_to("Using Token mint (from env): "), mint_pubkey);
    
    // Create a table for displaying wallet information
    let mut table = Table::new();
    table.add_row(row!["Wallet", "SOL Balance", "Token Balance"]);
    
    // Track totals
    let mut total_sol: u64 = 0;
    let mut total_tokens: u64 = 0; // Ensure this is u64 to match get_token_balance return
    
    // Prepare wallet info container
    let mut wallet_info: Vec<(String, u64, u64)> = Vec::new();
    
    // Check parent wallet (from settings)
    let parent_pk_str = &settings.parent_wallet_private_key;
    if !parent_pk_str.is_empty() {
        let decode_result = bs58::decode(parent_pk_str).into_vec();
        if let Ok(pk_bytes_vec) = decode_result {
             // pk_bytes_vec is Vec<u8>
             match Keypair::from_bytes(&pk_bytes_vec) {
                 Ok(parent_kp) => {
                     let parent_addr = parent_kp.pubkey();
                     // Fetch balances
                     let sol_balance = get_sol_balance(&rpc_client, &parent_addr).await?;
                     let token_balance = get_token_balance(&rpc_client, &parent_addr, &mint_pubkey).await?;
                     total_sol = total_sol.saturating_add(sol_balance);
                     total_tokens = total_tokens.saturating_add(token_balance);
                     wallet_info.push(("Parent (Settings)".to_string(), sol_balance, token_balance));
                 }
                 Err(e) => {
                     println!("{} Invalid parent_wallet_private_key in settings: {}. Skipping parent wallet check.", style("⚠️").yellow(), e);
                 }
             }
        } else if let Err(e) = decode_result {
             println!("{} Failed to decode parent_wallet_private_key (not base58?): {}. Skipping parent wallet check.", style("⚠️").yellow(), e);
        }
    } else {
        println!("{} parent_wallet_private_key not set in settings; skipping parent wallet check.", style("⚠️").yellow());
    }

    // Check dev wallet (from settings or first key in file - assuming get_wallet_keypair uses settings implicitly or needs update)
    // TODO: Verify get_wallet_keypair uses settings.dev_wallet_private_key or keys_path correctly
    let minter_kp = get_wallet_keypair()?;
    let minter_addr = minter_kp.pubkey();
    // Fetch balances
    let sol_balance = get_sol_balance(&rpc_client, &minter_addr).await?;
    let token_balance = get_token_balance(&rpc_client, &minter_addr, &mint_pubkey).await?;
    total_sol = total_sol.saturating_add(sol_balance);
    total_tokens = total_tokens.saturating_add(token_balance);
    wallet_info.push(("Minter".to_string(), sol_balance, token_balance));
    
    // Check other zombie wallets
    let wallets = load_zombie_wallets_from_file(keys_path_arg)
        .map_err(|e| {
            println!("Failed to load zombie wallets: {}", e);
            e // Return the error
        })?;
    
    if wallets.is_empty() {
        println!("No zombie wallets found in {}", keys_path_arg.unwrap_or("./keys.json"));
        // Don't return Ok here, proceed to print table with main wallet
    }
    
    for (index, zombie_kp) in wallets.iter().enumerate() { // Iterate through Keypairs
        let pubkey = zombie_kp.pubkey(); // Get pubkey from keypair
        
        // Get balances
        let sol_balance = get_sol_balance(&rpc_client, &pubkey).await?;
        let token_balance = get_token_balance(&rpc_client, &pubkey, &mint_pubkey).await?;
        
        total_sol = total_sol.saturating_add(sol_balance);
        total_tokens = total_tokens.saturating_add(token_balance);
        
        wallet_info.push((format!("Zombie {}", index), sol_balance, token_balance)); // Use index as name
    }
    
    // Add totals row (only if there are wallets other than main, or always? Let's show always)
    wallet_info.push(("TOTAL".to_string(), total_sol, total_tokens));
    
    // Add all wallets to the table
    for (name, sol, tokens) in wallet_info {
        // Format balances - Ensure correct formatting for u64
        let sol_formatted = format!("{:.6} SOL", sol as f64 / 1_000_000_000.0); // Convert u64 to f64 for division
        let tokens_formatted = format!("{}", tokens); // u64 formats directly
        
        table.add_row(row![name, sol_formatted, tokens_formatted]);
    }
    
    // Print the table
    table.printstd();
    
    Ok(())
} 