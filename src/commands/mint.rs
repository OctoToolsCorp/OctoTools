use crate::api::pumpfun::upload_metadata_to_ipfs;
use crate::config::{CONFIG, get_commitment_config, get_rpc_url};
use crate::errors::Result;
use crate::utils::transaction::{add_priority_fee, send_and_confirm_transaction};
use crate::wallet::{get_wallet_keypair, get_token_keypair, load_token_keypair, wallet_create_keypair};
use console::{Style};
use log::{debug, info};
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::native_token::LAMPORTS_PER_SOL;
use solana_program::system_instruction;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use spl_token::instruction as token_instruction;
use spl_token::state::Mint;
use std::str::FromStr;
use tokio::fs;

/// Mint a new Solana SPL token
pub async fn mint_token(
    name: String,
    symbol: String,
    decimals: u8,
    supply: u64,
    image: Option<String>,
    description: Option<String>,
) -> Result<()> {
    // Initialize the console styling
    let success_style = Style::new().green().bold();
    let info_style = Style::new().cyan();
    
    // Validate inputs
    if name.is_empty() || symbol.is_empty() {
        return Err("Name and symbol cannot be empty".into());
    }
    
    println!("\n{}", info_style.apply_to("🚀 Starting token creation process...").bold());
    
    // Get RPC client
    let rpc_url = get_rpc_url();
    let commitment_config = get_commitment_config();
    let rpc_client = RpcClient::new_with_commitment(rpc_url, commitment_config);
    
    // Get main wallet keypair
    let wallet_keypair = get_wallet_keypair()?;
    let wallet_pubkey = wallet_keypair.pubkey();
    println!("Wallet Address: {}", wallet_pubkey);
    
    // Check if token already exists
    let token_result = load_token_keypair();
    if token_result.is_ok() {
        let token_keypair = token_result?;
        let mint_account = token_keypair.pubkey().to_string();
        println!("\n{} Token already exists with address: {}", info_style.apply_to("⚠️ Warning:"), mint_account);
        println!("To create a new token, either delete the existing token keypair or use a different config directory.");
        return Ok(());
    }
    
    // Ensure we have enough SOL to pay for the transactions
    let wallet_balance = rpc_client.get_balance(&wallet_pubkey).await?;
    let min_balance = LAMPORTS_PER_SOL / 10; // 0.1 SOL
    
    if wallet_balance < min_balance {
        println!("\n{} Insufficient SOL balance", info_style.apply_to("⚠️ Warning:"));
        println!("Current balance: {} SOL", wallet_balance as f64 / LAMPORTS_PER_SOL as f64);
        println!("Minimum required: {} SOL", min_balance as f64 / LAMPORTS_PER_SOL as f64);
        println!("Please add more SOL to your wallet and try again.");
        return Ok(());
    }
    
    // Create a new token mint keypair
    println!("\n{}", info_style.apply_to("🔑 Creating CA...").bold());
    let token_keypair = wallet_create_keypair("token".to_string())?;
    let token_pubkey = token_keypair.pubkey();
    println!("Token Mint Address: {}", token_pubkey);
    
    // Calculate minimum rent
    let rent = rpc_client.get_minimum_balance_for_rent_exemption(Mint::LEN).await?;
    
    // Create mint account transaction
    let mut create_mint_tx = Transaction::new_with_payer(
        &[
            system_instruction::create_account(
                &wallet_pubkey,
                &token_pubkey,
                rent,
                Mint::LEN as u64,
                &spl_token::id(),
            ),
            token_instruction::initialize_mint(
                &spl_token::id(),
                &token_pubkey,
                &wallet_pubkey,
                None,
                decimals,
            )?,
        ],
        Some(&wallet_pubkey),
    );
    
    // Add priority fee
    add_priority_fee(&mut create_mint_tx, 100_000);
    
    // Get blockhash
    let recent_blockhash = rpc_client.get_latest_blockhash().await?;
    create_mint_tx.sign(&[&wallet_keypair, &token_keypair], recent_blockhash);
    
    // Send and confirm the transaction
    println!("Creating CA account...");
    match send_and_confirm_transaction(&rpc_client, create_mint_tx).await {
        Ok(signature) => println!("Transaction signature: {}", signature),
        Err(e) => {
            println!("Failed to create mint account: {}", e);
            return Err(e.into());
        }
    }
    
    // Create token account for the wallet
    println!("\n{}", info_style.apply_to("💼 Creating wallet token account...").bold());
    let create_account_ix = spl_associated_token_account::instruction::create_associated_token_account(
        &wallet_pubkey,
        &wallet_pubkey,
        &token_pubkey,
        &spl_token::id(),
    );
    
    let token_account = spl_associated_token_account::get_associated_token_address(
        &wallet_pubkey,
        &token_pubkey,
    );
    
    println!("Associated Token Account: {}", token_account);
    
    // Create transaction
    let mut create_account_tx = Transaction::new_with_payer(&[create_account_ix], Some(&wallet_pubkey));
    
    // Add priority fee
    add_priority_fee(&mut create_account_tx, 100_000);
    
    // Sign and send transaction
    let recent_blockhash = rpc_client.get_latest_blockhash().await?;
    create_account_tx.sign(&[&wallet_keypair], recent_blockhash);
    
    println!("Creating associated token account...");
    match send_and_confirm_transaction(&rpc_client, create_account_tx).await {
        Ok(signature) => println!("Transaction signature: {}", signature),
        Err(e) => {
            println!("Failed to create token account: {}", e);
            return Err(e.into());
        }
    }
    
    // Mint tokens to the owner's account
    println!("\n{}", info_style.apply_to("💰 Minting tokens...").bold());
    let mint_to_ix = token_instruction::mint_to(
        &spl_token::id(),
        &token_pubkey,
        &token_account,
        &wallet_pubkey,
        &[&wallet_pubkey],
        supply,
    )?;
    
    let mut mint_tx = Transaction::new_with_payer(&[mint_to_ix], Some(&wallet_pubkey));
    
    // Add priority fee
    add_priority_fee(&mut mint_tx, 100_000);
    
    // Sign and send transaction
    let recent_blockhash = rpc_client.get_latest_blockhash().await?;
    mint_tx.sign(&[&wallet_keypair], recent_blockhash);
    
    println!("Minting {} tokens...", supply);
    match send_and_confirm_transaction(&rpc_client, mint_tx).await {
        Ok(signature) => println!("Transaction signature: {}", signature),
        Err(e) => {
            println!("Failed to mint tokens: {}", e);
            return Err(e.into());
        }
    }
    
    // Upload metadata if image is provided
    if let Some(image_path) = image {
        println!("\n{}", info_style.apply_to("🖼️ Uploading token metadata...").bold());
        
        // Prepare metadata JSON
        let metadata = json!({
            "name": name,
            "symbol": symbol,
            "description": description.unwrap_or_else(|| format!("{} token on Solana", name)),
            "decimals": decimals,
            "image": image_path
        });
        
        // Upload metadata to IPFS
        match upload_metadata_to_ipfs(metadata).await {
            Ok(ipfs_url) => {
                println!("Metadata uploaded successfully!");
                println!("IPFS URL: {}", ipfs_url);
            },
            Err(e) => {
                println!("Warning: Failed to upload metadata: {}", e);
                println!("Your token has been created, but metadata upload failed.");
            }
        }
    }
    
    // Save token information to environment
    let env_content = format!(
        "TOKEN_NAME=\"{}\"\nTOKEN_SYMBOL=\"{}\"\nTOKEN_DECIMALS={}\nTOKEN_SUPPLY={}\n",
        name, symbol, decimals, supply
    );
    
    // Save to .env file (append to existing if it exists)
    match fs::write(".env", env_content).await {
        Ok(_) => println!("Token information saved to .env file"),
        Err(e) => println!("Warning: Failed to save token info to .env: {}", e),
    }
    
    println!("\n{}", success_style.apply_to("✅ Token created successfully!").bold());
    println!("Token Name: {}", name);
    println!("Token Symbol: {}", symbol);
    println!("Token Decimals: {}", decimals);
    println!("Token Supply: {}", supply);
    println!("Mint Address: {}", token_pubkey);
    
    Ok(())
} 