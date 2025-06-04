use crate::config::{get_commitment_config, get_rpc_url, CONFIG}; // Keep CONFIG if used for lamports_per_sol
use crate::errors::{PumpfunError, Result};
use crate::wallet::{get_wallet_keypair, load_zombie_wallets_from_file}; // Added load_zombie_wallets_from_file
use crate::models::settings::AppSettings; // <-- Correct import path
use log::{info, warn, error};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::address_lookup_table::instruction::{
    create_lookup_table, extend_lookup_table, freeze_lookup_table
};
use solana_sdk::{
    instruction::Instruction,
    // Removed message::Message
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
    compute_budget::ComputeBudgetInstruction,
    message::VersionedMessage,
};
use std::time::Duration;
use tokio::time::timeout as tokio_timeout; // Added for explicit timeout
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::str::FromStr;

const MAX_ADDRESSES_PER_EXTEND: usize = 20;
const DEFAULT_ALT_ADDRESS_FILE: &str = "./alt_addresses.json"; // Corrected filename

pub async fn create_lookup_table_command(
    addresses_file_path_arg: Option<String>,
) -> Result<()> {
    // Load settings first to get keys_path
    let settings = AppSettings::load();

    let addresses_file_path = addresses_file_path_arg
        .as_deref()
        .unwrap_or(DEFAULT_ALT_ADDRESS_FILE);

    println!(
        "\n🛠️ Creating Address Lookup Table (ALT)... Using addresses from {}",
        addresses_file_path
    );

    // 1. Load Addresses from the specified JSON file (derived addresses)
    println!("Loading derived addresses from {}...", addresses_file_path);
    let derived_addresses: Vec<Pubkey> = { // Scope the file loading
        let alt_file_path_buf = PathBuf::from(addresses_file_path);
        let file = File::open(&alt_file_path_buf)
            .map_err(|e| PumpfunError::Config(format!("Failed to open address file '{}': {}", addresses_file_path, e)))?;
        let reader = BufReader::new(file);
        let addresses_str: Vec<String> = serde_json::from_reader(reader)
            .map_err(|e| PumpfunError::Config(format!("Failed to parse address file '{}': {}", addresses_file_path, e)))?;

        addresses_str
            .iter()
            .map(|s| Pubkey::from_str(s)
                 .map_err(|e| PumpfunError::InvalidInput(format!("Invalid address '{}' in {}: {}", s, addresses_file_path, e))))
            .collect::<Result<Vec<Pubkey>>>()?
    };
    info!("Loaded {} derived addresses from file.", derived_addresses.len());

    // 2. Load Minter Pubkey from settings
    let minter_pubkey = AppSettings::get_pubkey_from_privkey_str(&settings.dev_wallet_private_key)
        .and_then(|s: String| Pubkey::from_str(&s).ok())
        .ok_or_else(|| PumpfunError::Config("Failed to load/parse minter_wallet_private_key from settings.".to_string()))?;
    info!("Loaded Minter Pubkey: {}", minter_pubkey);

    // 3. Load Zombie Pubkeys from keys file specified in settings
    let zombie_keypairs = load_zombie_wallets_from_file(Some(&settings.keys_path))?;
    let zombie_pubkeys: Vec<Pubkey> = zombie_keypairs.iter().map(|kp| kp.pubkey()).collect();
    info!("Loaded {} zombie pubkeys from {}", zombie_pubkeys.len(), settings.keys_path);

    // 4. Combine all addresses and remove duplicates
    let mut all_addresses = std::collections::HashSet::new();
    all_addresses.extend(derived_addresses);
    all_addresses.insert(minter_pubkey);
    all_addresses.extend(zombie_pubkeys);

    // Convert HashSet back to Vec
    let addresses_to_add: Vec<Pubkey> = all_addresses.into_iter().collect();

    if addresses_to_add.is_empty() {
         return Err(PumpfunError::Config("No addresses found to add to ALT (derived, minter, zombies).".to_string()));
    }
    info!("Total unique addresses to add to ALT: {}", addresses_to_add.len());


    // 5. Get Authority (Parent Wallet) and RPC Client
    let authority_keypair = get_wallet_keypair()?;
    let authority_pubkey = authority_keypair.pubkey();
    println!("{} {}", "Using Authority Wallet:", authority_pubkey);

    let rpc_url = get_rpc_url();
    let commitment_config = get_commitment_config();
    let rpc_client = RpcClient::new_with_timeout_and_commitment(
        rpc_url.clone(), 
        Duration::from_secs(90), // 90-second timeout
        commitment_config
    );
    
    // Check authority balance (async)
    let balance = rpc_client.get_balance(&authority_pubkey).await?;
    info!("Authority balance: {} SOL", balance as f64 / CONFIG.lamports_per_sol as f64);
    if balance < 1000000 { // Rough check for rent + fees
        warn!("Authority wallet balance is low, ALT creation might fail.");
    }

    // 3. Create Lookup Table Instruction
    println!("Sending CreateLookupTable transaction...");
    let recent_slot = rpc_client.get_slot_with_commitment(commitment_config).await?;
    let (create_ix, lookup_table_address) = create_lookup_table(
        authority_pubkey, // Authority
        authority_pubkey, // Payer
        recent_slot,      // Use the freshly fetched recent slot
    );
    println!("{} {}", "New ALT Address:", lookup_table_address);
    send_and_confirm_ix(&rpc_client, &authority_keypair, create_ix, "Create ALT").await?;

    // 4. Extend Lookup Table Instruction(s) using addresses from the file
    println!(
        "{} {} addresses from file...",
        "Sending ExtendLookupTable transaction(s) to add",
        addresses_to_add.len()
    );
    for chunk in addresses_to_add.chunks(MAX_ADDRESSES_PER_EXTEND) {
        let extend_ix = extend_lookup_table(
            lookup_table_address,
            authority_pubkey,       // Authority
            Some(authority_pubkey), // Payer
            chunk.to_vec(),         // Use the chunk read from the file
        );
        send_and_confirm_ix(
            &rpc_client,
            &authority_keypair,
            extend_ix,
            &format!("Extend ALT ({} addresses)", chunk.len()),
        )
        .await?;
        // Optional: Add a small delay between chunks if needed
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    // 5. Freeze Lookup Table Instruction
    println!("Sending FreezeLookupTable transaction...");
    let freeze_ix = freeze_lookup_table(
        lookup_table_address,
        authority_pubkey, // Authority
    );
    send_and_confirm_ix(&rpc_client, &authority_keypair, freeze_ix, "Freeze ALT").await?;

    println!(
        "\n{}",
        "✅ Address Lookup Table created and frozen successfully!"
    );
    println!("ALT Address: {}", lookup_table_address);
    println!("Please use this address with the --alt-address flag for bundled creates.");

    Ok(())
}

async fn send_and_confirm_ix(
    rpc_client: &RpcClient,
    payer: &Keypair,
    instruction: Instruction,
    label: &str,
) -> Result<()> {
    const CONFIRMATION_TIMEOUT_SECONDS: u64 = 120; // 2 minutes timeout for each confirmation
    let latest_blockhash = rpc_client.get_latest_blockhash().await?;

    // Define a priority fee (e.g., 1,000,000 microlamports)
    // You might want to make this configurable later
    const PRIORITY_FEE_MICROLAMPORTS: u64 = 2_000_000; // Increased priority fee
    let priority_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(PRIORITY_FEE_MICROLAMPORTS);

    // Explicitly set a compute unit limit
    const COMPUTE_UNIT_LIMIT: u32 = 600_000; // Increased limit further
    let compute_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT);

    // Create a Version 0 message including the priority fee ix and the main ix
    let message = VersionedMessage::V0(
        solana_sdk::message::v0::Message::try_compile(
            &payer.pubkey(),
            &[compute_limit_ix, priority_fee_ix, instruction], // Added compute_limit_ix
            &[], // No address lookup tables needed for this specific tx
            latest_blockhash,
        ).map_err(|e| PumpfunError::Transaction(format!("Failed to compile V0 message for {}: {}", label, e)))?
    );

    // Sign the VersionedTransaction
    let tx = VersionedTransaction::try_new(
        message,
        &[payer] // Only the payer needs to sign
    ).map_err(|e| PumpfunError::Wallet(format!("Failed to sign {} tx: {}", label, e)))?;

    info!("Sending {} transaction with priority fee... Signature: {}", label, tx.signatures[0]);
    
    let confirmation_future = rpc_client.send_and_confirm_transaction_with_spinner(&tx);

    match tokio_timeout(Duration::from_secs(CONFIRMATION_TIMEOUT_SECONDS), confirmation_future).await {
        Ok(Ok(signature)) => { // Timeout didn't occur, and transaction succeeded
            info!("{} transaction confirmed. Signature: {}", label, signature);
            Ok(())
        }
        Ok(Err(client_error)) => { // Timeout didn't occur, but transaction failed
            error!("Failed to send/confirm {} tx: {}", label, client_error);
            error!("Failed Transaction Signature: {}", tx.signatures[0]);
            Err(PumpfunError::Transaction(format!("Failed to send/confirm {} tx (Sig: {}): {}", label, tx.signatures[0], client_error)))
        }
        Err(_elapsed) => { // Timeout occurred
            error!("Timeout waiting for {} transaction confirmation ({}s). Signature: {}", label, CONFIRMATION_TIMEOUT_SECONDS, tx.signatures[0]);
            Err(PumpfunError::Transaction(format!("Timeout waiting for {} tx confirmation (Sig: {}) after {}s", label, tx.signatures[0], CONFIRMATION_TIMEOUT_SECONDS)))
        }
    }
}