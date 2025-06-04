// src/simulation/original.rs

// --- Imports ---
// Add necessary imports based on the copied code.
// These will likely include std lib, solana_*, spl_*, reqwest, serde, log, etc.
// Adjust paths as needed (e.g., use crate::...)

use std::{
    collections::HashSet,
    // error::Error as StdError, // Removed
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter}, // Removed Write
    path::{Path}, // Removed PathBuf
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant}, // Added Instant
};
use anyhow::{anyhow, Context, Result}; // Using anyhow for simpler error handling initially
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
// use borsh::{BorshDeserialize, BorshSerialize}; // Removed
use bs58;
use chrono::Utc; // For logging timestamp
use log::{debug, error, info, warn};
use rand::seq::SliceRandom;
use rand::Rng;
// use rand::rngs::StdRng; // Removed
// use rand::SeedableRng; // Removed
use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use serde_json::{json}; // Removed Value
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    // rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig}, // Add if needed
    // rpc_response::{RpcSimulateTransactionResult, RpcTokenAccountBalance}, // Add if needed
};
use solana_program::{
    instruction::Instruction,
    native_token::{lamports_to_sol, sol_to_lamports},
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
    // system_program, // Removed
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    signature::{Keypair, Signature, Signer},
    transaction::{Transaction, TransactionError, VersionedTransaction},
    // message::Message, // Removed
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token::{
    error::TokenError,
    instruction::{close_account, transfer as spl_transfer},
    state::Account as TokenAccount, // For Account::LEN
    ID as TOKEN_PROGRAM_ID,
};
use thiserror::Error;
use tokio::time::{sleep}; // Removed timeout


// --- Constants ---
pub const SOL_MINT_ADDRESS: &str = "So11111111111111111111111111111111111111112"; // Made public
const JUPITER_QUOTE_API_URL: &str = "https://quote-api.jup.ag/v6/quote";
const JUPITER_SWAP_API_URL: &str = "https://quote-api.jup.ag/v6/swap";
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
// Simulation specific constants (adjust if needed)
const MIN_BUY_LAMPORTS: u64 = 10_000_000; // 0.01 SOL
const MAX_BUY_LAMPORTS_PERCENT: f64 = 0.8; // Max 80% of available SOL for buy attempt
const MIN_SELL_TOKENS: u64 = 1; // Minimum token amount to attempt selling (adjust based on decimals)
const FEE_BUFFER_SWAP: u64 = 10_000_000; // 0.01 SOL buffer for swap fees + potential slippage cost
const FEE_BUFFER_TRANSFER: u64 = 500_000; // 0.0005 SOL buffer for transfer fees
const CONSOLIDATION_FEE_ESTIMATE: u64 = 2_005_000; // Fee for token/SOL transfers during consolidation
const RENT_EXEMPT_MINIMUM: u64 = 890_880; // Minimum lamports for rent exemption (system account)
const SIMULATION_TRANSFER_COMPUTE_LIMIT: u32 = 200_000; // CU Limit for simple transfers in simulation
const SIMULATION_SWAP_COMPUTE_LIMIT: u32 = 600_000; // CU Limit for Jupiter swaps in simulation
const PRIORITY_FEE_MICRO_LAMPORTS: u64 = 750_000; // Priority fee for simulation transactions (Module Level)


// --- Error Enum ---
// Using anyhow::Error for the main run function, but keeping specific errors internally might be useful.
// Let's adapt the SwapError from pumper/src/main.rs
#[derive(Debug, Error)]
pub enum OriginalSimError {
    #[error("Invalid public key: {0}")]
    InvalidPublicKey(String),
    #[error("Invalid private key: {0}")]
    InvalidPrivateKey(#[from] solana_sdk::signer::SignerError),
    #[error("HTTP request error: {0}")]
    HttpRequestError(#[from] reqwest::Error),
    #[error("JSON serialization/deserialization error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Solana client error: {0}")]
    SolanaClientError(#[from] solana_client::client_error::ClientError),
    #[error("Base58 decode error: {0}")]
    Base58DecodeError(#[from] bs58::decode::Error),
    #[error("Base64 decode error: {0}")]
    Base64DecodeError(#[from] base64::DecodeError),
    #[error("Bincode deserialize/serialize error: {0}")]
    BincodeError(#[from] bincode::Error),
    #[error("API returned an error: {0}")]
    ApiError(String),
    #[error("IO Error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("No suitable wallet found for action")]
    NoSuitableWallet,
    #[error("SPL Token Error: {0}")]
    SplTokenError(#[from] TokenError),
    #[error("Program error: {0:?}")]
    ProgramError(#[from] ProgramError),
    #[error("Transaction error: {0:?}")]
    TransactionError(TransactionError),
    #[error("Simulation finished")] // May not be needed as an error
    SimulationFinished,
    #[error("Confirmation timed out for signature: {0}")] // Added for timeout
    ConfirmationTimeout(Signature),
    #[error("Configuration error: {0}")]
    ConfigError(String),
}


// --- Data Structs ---

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WalletData { // Made fields public
    pub name: String,
    pub address: String, // Made public
    pub key: String, // Made public
}

// Represents a wallet in the simulation
pub struct Wallet {
    pub id: usize,
    pub keypair: Keypair,
}

impl Wallet {
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }
}

// Debug implementation for Wallet
impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("id", &self.id)
            .field("pubkey", &self.keypair.pubkey().to_string())
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogEntry {
    timestamp: String, // Added timestamp
    step: usize,
    event_type: String,
    details: serde_json::Value,
}

// --- Jupiter API Structs ---

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct QuoteResponse {
    out_amount: String,
    #[serde(flatten)]
    other_fields: serde_json::Value,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SwapRequestPayload<'a> {
    #[serde(rename = "quoteResponse")]
    quote_response: &'a QuoteResponse,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    // prioritization_fee_lamports: String, // Use compute_unit_price instead
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_unit_price: Option<u64>, // Added for explicit fee control
    #[serde(skip_serializing_if = "Option::is_none")]
    compute_unit_limit: Option<u32>, // Added for explicit limit control
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    swap_transaction: String, // base64 encoded transaction
}

// --- Wallet Loading ---
// (Copied and adapted from pumper/src/main.rs load_or_generate_wallets)
// Using OriginalSimError now
// Made public
pub fn load_or_generate_wallets(file_path: &str, num_wallets: usize) -> Result<Vec<Wallet>, OriginalSimError> {
    let path = Path::new(file_path);
    let mut wallets = Vec::new();
    let mut wallets_data: Vec<WalletData> = Vec::new();


    info!("load_or_generate_wallets: Starting process for file: {}", file_path);
    debug!("load_or_generate_wallets: Checking if file exists at {}", file_path);
    if path.exists() {
        debug!("load_or_generate_wallets: File exists. Attempting to open {}...", file_path);
        match File::open(path) {
            Ok(file) => {
                debug!("load_or_generate_wallets: File opened successfully. Attempting to read...");
                let reader = BufReader::new(file);
                debug!("load_or_generate_wallets: Attempting to parse JSON from reader...");
                match serde_json::from_reader::<_, Vec<WalletData>>(reader) { // Ensure type annotation is correct
                    Ok(loaded_data) => {
                        info!("load_or_generate_wallets: JSON parsed successfully. Loaded {} wallet entries.", loaded_data.len());
                        wallets_data = loaded_data;
                        debug!("load_or_generate_wallets: Starting loop to process loaded wallet data.");
                        for data in &wallets_data {
                            debug!("load_or_generate_wallets: Processing wallet entry: {}", data.name);
                            let parsed_id = data.name.trim_start_matches("wallet").parse::<usize>();
                            let wallet_id = match parsed_id {
                                Ok(id) => id,
                                Err(_) => {
                                    warn!("load_or_generate_wallets: Could not parse ID from wallet name '{}'. Skipping.", data.name);
                                    continue;
                                }
                            };
                            debug!("load_or_generate_wallets: Parsed wallet ID: {}", wallet_id);

                            match bs58::decode(&data.key).into_vec() {
                                Ok(private_key_bytes) => {
                                    match Keypair::from_bytes(&private_key_bytes) {
                                        Ok(keypair) => {
                                            if keypair.pubkey().to_string() != data.address {
                                                warn!(
                                                    "Public key mismatch for wallet '{}' (ID {}). Loaded: {}, Derived: {}. Skipping.",
                                                    data.name, wallet_id, data.address, keypair.pubkey()
                                                );
                                                continue;
                                            }
                                            wallets.push(Wallet { id: wallet_id, keypair });
                                        }
                                        Err(e) => {
                                            warn!("Failed to create keypair from bytes for wallet '{}' (ID {}): {}. Skipping.", data.name, wallet_id, e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to decode base58 private key for wallet '{}' (ID {}): {}. Skipping.", data.name, wallet_id, e);
                                }
                            }
                        }
                        info!("load_or_generate_wallets: Finished loop processing loaded wallet data.");
                        if wallets.len() < num_wallets {
                            info!("load_or_generate_wallets: Loaded {} valid wallets, need {}. Generating {} more...", wallets.len(), num_wallets, num_wallets - wallets.len());
                        } else if wallets.len() > num_wallets {
                            info!("load_or_generate_wallets: Loaded {} wallets, using the first {}.", wallets.len(), num_wallets);
                            wallets.truncate(num_wallets);
                            wallets_data.truncate(num_wallets); // Also truncate data to save correctly
                        } else {
                            info!("load_or_generate_wallets: Loaded exactly {} wallets.", num_wallets);
                            // Sort wallets by ID to ensure wallet 0 is first
                            wallets.sort_by_key(|w| w.id);
                            info!("load_or_generate_wallets: Wallets loaded and sorted. Returning.");
                            return Ok(wallets);
                        }
                    }
                    Err(e) => {
                        warn!("load_or_generate_wallets: Failed to parse wallets file '{}': {}. Generating new wallets.", file_path, e);
                        // Clear potentially partially loaded data
                        wallets.clear();
                        wallets_data.clear();
                    }
                }
            }
            Err(e) => {
                warn!("load_or_generate_wallets: Failed to open wallets file '{}': {}. Generating new wallets.", file_path, e);
            }
        }

    } else {
        debug!("load_or_generate_wallets: File does not exist at {}. Generating new wallets.", file_path);
    }

    // --- Generation ---
    debug!("load_or_generate_wallets: Entering wallet generation block.");
    let start_index = wallets.len();
    if wallets_data.capacity() < num_wallets {
        wallets_data.reserve(num_wallets - wallets_data.capacity());
    }
    // Ensure wallet 0 is generated first if needed
    if start_index == 0 && num_wallets > 0 {
         let keypair = Keypair::new();
         let id = 0;
         info!("Generated Wallet ID: {}, Pubkey: {}", id, keypair.pubkey());
         let wallet_data_entry = WalletData {
             name: format!("wallet{}", id),
             address: keypair.pubkey().to_string(),
             key: keypair.to_base58_string(),
         };
         wallets.push(Wallet { id, keypair });
         wallets_data.push(wallet_data_entry);
    }
    // Generate remaining wallets
    for i in wallets.len()..num_wallets {
        let keypair = Keypair::new();
        let id = i; // Assign sequential IDs
        info!("Generated Wallet ID: {}, Pubkey: {}", id, keypair.pubkey());
        let wallet_data_entry = WalletData {
            name: format!("wallet{}", id),
            address: keypair.pubkey().to_string(),
            key: keypair.to_base58_string(),
        };
        wallets.push(Wallet { id, keypair });
        // This logic assumes wallets_data might have gaps if loading failed partially,
        // but we cleared it on parse error. Simpler push should work.
        wallets_data.push(wallet_data_entry);
    }

    // --- Generation ---
    info!("load_or_generate_wallets: Starting wallet generation phase.");
    let start_index = wallets.len();
    if wallets_data.capacity() < num_wallets {
        wallets_data.reserve(num_wallets - wallets_data.capacity());
    }
    // Ensure wallet 0 is generated first if needed
    if start_index == 0 && num_wallets > 0 {
         debug!("load_or_generate_wallets: Generating wallet 0.");
         let keypair = Keypair::new();
         let id = 0;
         info!("Generated Wallet ID: {}, Pubkey: {}", id, keypair.pubkey());
         let wallet_data_entry = WalletData {
             name: format!("wallet{}", id),
             address: keypair.pubkey().to_string(),
             key: keypair.to_base58_string(),
         };
         wallets.push(Wallet { id, keypair });
         wallets_data.push(wallet_data_entry);
    }
    // Generate remaining wallets
    info!("load_or_generate_wallets: Generating remaining wallets from index {} up to {}.", wallets.len(), num_wallets);
    for i in wallets.len()..num_wallets {
        debug!("load_or_generate_wallets: Generating wallet {}.", i);
        let keypair = Keypair::new();
        let id = i; // Assign sequential IDs
        info!("Generated Wallet ID: {}, Pubkey: {}", id, keypair.pubkey());
        let wallet_data_entry = WalletData {
            name: format!("wallet{}", id),
            address: keypair.pubkey().to_string(),
            key: keypair.to_base58_string(),
        };
        wallets.push(Wallet { id, keypair });
        // This logic assumes wallets_data might have gaps if loading failed partially,
        // but we cleared it on parse error. Simpler push should work.
        wallets_data.push(wallet_data_entry);
    }
    info!("load_or_generate_wallets: Finished wallet generation phase. Generated {} wallets.", wallets.len() - start_index);



    // --- Save ---
    debug!("load_or_generate_wallets: Starting wallet saving phase.");
    // Ensure we only save the requested number of wallets
    wallets_data.truncate(num_wallets);
    debug!("load_or_generate_wallets: Saving {} wallet entries to {}...", wallets_data.len(), file_path);
    match File::create(path) { // Use File::create to truncate if exists
        Ok(file) => {
            debug!("load_or_generate_wallets: Wallets file created/opened for writing.");
            let writer = BufWriter::new(file);
            debug!("load_or_generate_wallets: Attempting to serialize and write JSON.");
            if let Err(e) = serde_json::to_writer_pretty(writer, &wallets_data) {
                error!("load_or_generate_wallets: Error saving wallets file: {}", e);
                // Don't return error here, just log it, as wallets are generated in memory
            } else {
                info!("load_or_generate_wallets: Wallets saved successfully.");
            }
        }
        Err(e) => {
            error!("load_or_generate_wallets: Error creating wallets file for saving: {}", e);
            // Don't return error here
        }
    }
    debug!("load_or_generate_wallets: Finished wallet saving phase.");

    // Sort wallets by ID before returning, crucial for assuming wallet 0 is minter
    info!("load_or_generate_wallets: Sorting wallets by ID.");
    wallets.sort_by_key(|w| w.id);
    wallets.truncate(num_wallets); // Ensure correct number is returned
    info!("load_or_generate_wallets: Wallet loading/generation complete. Returning {} wallets.", wallets.len());
    Ok(wallets)
}


// --- Balance & Transfer Helpers ---
// (Copied and adapted from pumper/src/main.rs, using OriginalSimError)

pub(crate) async fn get_sol_balance(rpc_client: &RpcClient, pubkey: &Pubkey) -> Result<u64, OriginalSimError> {
    Ok(rpc_client.get_balance(pubkey).await?)
}

pub(crate) async fn get_token_balance(
    rpc_client: &RpcClient,
    owner_pubkey: &Pubkey,
    mint_pubkey: &Pubkey,
) -> Result<u64, OriginalSimError> {
    let ata_pubkey = get_associated_token_address(owner_pubkey, mint_pubkey);
    match rpc_client.get_token_account_balance(&ata_pubkey).await {
        Ok(ui_token_amount) => {
            match ui_token_amount.amount.parse::<u64>() {
                Ok(amount_val) => Ok(amount_val),
                Err(_) => {
                    error!("Failed to parse token amount string: {}", ui_token_amount.amount);
                    Ok(0)
                }
            }
        }
        Err(e) => {
            if e.to_string().contains("AccountNotFound") || e.to_string().contains("could not find account") {
                Ok(0)
            } else {
                error!("Error getting token balance for {}: {}", ata_pubkey, e);
                Err(OriginalSimError::SolanaClientError(e))
            }
        }
    }
}

// Simplified create_and_send_tx (no Jito initially)
pub(crate) async fn create_and_send_tx(
    rpc_client: &RpcClient,
    instructions: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
) -> Result<Signature, OriginalSimError> {
    let recent_blockhash = rpc_client.get_latest_blockhash().await?;

    // Use module-level constant
    let priority_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(PRIORITY_FEE_MICRO_LAMPORTS);
    let compute_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(SIMULATION_TRANSFER_COMPUTE_LIMIT); // Added CU Limit

    let mut final_instructions = vec![compute_limit_ix, priority_fee_ix]; // Add both CU limit and priority fee
    final_instructions.extend_from_slice(instructions);


    let mut tx = Transaction::new_with_payer(&final_instructions, Some(&payer.pubkey()));

    let mut all_signers = vec![payer];
    all_signers.extend_from_slice(signers);

    tx.sign(&all_signers, recent_blockhash);

    // --- Send and Manually Confirm ---
    debug!(" -> Sending transfer/simple transaction...");
    let signature = rpc_client.send_transaction(&tx).await
        .map_err(OriginalSimError::SolanaClientError)?;
    info!(" -> Transaction sent. Signature: {}", signature);

    debug!(" -> Confirming transaction {}...", signature);
    let confirmation_start = Instant::now();
    let confirmation_timeout = Duration::from_secs(60); // 60 second timeout for confirmation

    loop {
        if confirmation_start.elapsed() > confirmation_timeout {
            error!(" -> Confirmation timed out for {}", signature);
            return Err(OriginalSimError::ConfirmationTimeout(signature)); // Use new error variant
        }

        match rpc_client.get_signature_statuses(&[signature]).await {
            Ok(statuses) => {
                if let Some(Some(status)) = statuses.value.get(0) {
                    if let Some(confirmation_status) = &status.confirmation_status {
                        match confirmation_status {
                            solana_transaction_status::TransactionConfirmationStatus::Processed => {
                                debug!(" -> {} status: Processed. Waiting for confirmation...", signature);
                            }
                            solana_transaction_status::TransactionConfirmationStatus::Confirmed |
                            solana_transaction_status::TransactionConfirmationStatus::Finalized => {
                                info!(" -> Transaction {} confirmed/finalized.", signature);
                                if let Some(err) = &status.err {
                                     error!("    -> On-chain execution failed! Error: {:?}", err);
                                     return Err(OriginalSimError::TransactionError(err.clone()));
                                }
                                return Ok(signature);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!(" -> Error fetching status for {}: {:?}. Retrying...", signature, e);
            }
        }
        sleep(Duration::from_secs(2)).await; // Wait 2 seconds before polling again
    }
    // --- End Send and Manually Confirm ---
}


pub(crate) async fn transfer_sol(
    rpc_client: &RpcClient,
    from: &Keypair,
    to: &Pubkey,
    amount_lamports: u64,
) -> Result<Signature, OriginalSimError> {
    if amount_lamports == 0 {
        debug!("Skipping SOL transfer of 0 lamports.");
        return Ok(Signature::default());
    }
    info!(
        "Transferring {} SOL from {} to {}",
        lamports_to_sol(amount_lamports), from.pubkey(), to
    );
    let ix = system_instruction::transfer(&from.pubkey(), to, amount_lamports);
    create_and_send_tx(rpc_client, &[ix], from, &[]).await
}

// Adapted transfer_spl_tokens
pub(crate) async fn transfer_spl_tokens(
    rpc_client: &RpcClient,
    token_mint: &Pubkey,
    from_wallet: &Keypair,
    to_pubkey: &Pubkey,
    amount_tokens: u64,
    minter_keypair: &Keypair, // Minter pays for ATA creation if needed
) -> Result<Signature, OriginalSimError> {
     if amount_tokens == 0 {
        debug!("Skipping SPL transfer of 0 tokens.");
        return Ok(Signature::default());
    }

    let from_pubkey = from_wallet.pubkey();
    let source_ata = get_associated_token_address(&from_pubkey, token_mint);
    let dest_ata = get_associated_token_address(to_pubkey, token_mint);

    info!(
        "Transferring {} tokens ({}) from {} (ATA {}) to {} (ATA {})",
        amount_tokens, token_mint, from_pubkey, source_ata, to_pubkey, dest_ata
    );

    let mut instructions = Vec::new();
    let mut ata_creation_needed = false;

    // Check if destination ATA exists
    match rpc_client.get_account(&dest_ata).await {
        Ok(_) => {
            debug!("Destination ATA {} already exists.", dest_ata);
        }
        Err(e) => {
            if e.to_string().contains("AccountNotFound") || e.to_string().contains("could not find account") {
                debug!("Destination ATA {} does not exist. Will attempt creation.", dest_ata);
                ata_creation_needed = true;
            } else {
                error!("Error checking destination ATA {}: {}", dest_ata, e);
                return Err(OriginalSimError::SolanaClientError(e));
            }
        }
    }

    // Create ATA if needed, paid by MINTER
    if ata_creation_needed {
        let rent_exempt_minimum_ata = rpc_client.get_minimum_balance_for_rent_exemption(TokenAccount::LEN).await?;
        info!("Rent-exempt minimum for ATA: {} lamports", rent_exempt_minimum_ata);

        // Check MINTER balance before attempting ATA creation tx
        const ATA_CREATION_FEE_BUFFER: u64 = 1_000_000; // Example buffer
        let minter_balance = get_sol_balance(rpc_client, &minter_keypair.pubkey()).await?;
        if minter_balance < rent_exempt_minimum_ata + ATA_CREATION_FEE_BUFFER {
            error!("MINTER has insufficient SOL ({}) to pay for ATA creation fee + rent (needs ~{}).", minter_balance, rent_exempt_minimum_ata + ATA_CREATION_FEE_BUFFER);
            return Err(OriginalSimError::ConfigError("MINTER insufficient funds for ATA creation".to_string()));
        }

        info!("Creating destination ATA {} (paid by MINTER)...", dest_ata);
        let create_ata_ix = create_associated_token_account(
            &minter_keypair.pubkey(), // Payer is MINTER
            to_pubkey,                // Owner of the new ATA
            token_mint,
            &TOKEN_PROGRAM_ID,
        );
        // Send ATA creation transaction separately, signed only by MINTER
        match create_and_send_tx(rpc_client, &[create_ata_ix], minter_keypair, &[]).await {
            Ok(sig) => {
                info!("ATA creation transaction successful: {}", sig);
                sleep(Duration::from_secs(5)).await; // Allow confirmation
            }
            Err(e) => {
                error!("Failed to create destination ATA {} paid by MINTER: {:?}", dest_ata, e);
                return Err(e); // Propagate the error
            }
        }
    }

    // Add the transfer instruction
    instructions.push(
        spl_transfer(
            &TOKEN_PROGRAM_ID,
            &source_ata,
            &dest_ata,
            &from_pubkey, // Authority (owner of source_ata)
            &[],          // Signer pubkeys (only from_wallet needed, implicitly payer)
            amount_tokens,
        )?, // Propagate potential SPL token errors
    );

    // Send the transfer transaction (signed by the sender/from_wallet)
    create_and_send_tx(rpc_client, &instructions, from_wallet, &[]).await
}


// --- Jupiter API Helpers ---
// (Copied and adapted from pumper/src/main.rs, using OriginalSimError)

pub(crate) async fn get_jupiter_quote(
    http_client: &ReqwestClient,
    output_mint: &str,
    amount_lamports: u64,
    slippage_bps: u16,
) -> Result<QuoteResponse, OriginalSimError> {
    let params = [
        ("inputMint", SOL_MINT_ADDRESS),
        ("outputMint", output_mint),
        ("amount", &amount_lamports.to_string()),
        ("slippageBps", &slippage_bps.to_string()),
        ("onlyDirectRoutes", "false"),
    ];
    debug!("Fetching quote with params: {:?}", params);
    let response = http_client
        .get(JUPITER_QUOTE_API_URL)
        .query(&params)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        error!("Quote API Error Response: Status {}, Body: {}", status, text);
        return Err(OriginalSimError::ApiError(format!(
            "Quote API failed with status {}: {}", status, text
        )));
    }
    Ok(response.json::<QuoteResponse>().await?)
}

pub(crate) async fn get_jupiter_quote_token_to_sol(
    http_client: &ReqwestClient,
    input_mint: &str, // Token mint
    amount_tokens: u64,
    slippage_bps: u16,
) -> Result<QuoteResponse, OriginalSimError> {
    let params = [
        ("inputMint", input_mint),
        ("outputMint", SOL_MINT_ADDRESS),
        ("amount", &amount_tokens.to_string()),
        ("slippageBps", &slippage_bps.to_string()),
        ("onlyDirectRoutes", "false"),
    ];
    debug!("Fetching quote (Token->SOL) with params: {:?}", params);
    let response = http_client
        .get(JUPITER_QUOTE_API_URL)
        .query(&params)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        error!("Quote API Error Response: Status {}, Body: {}", status, text);
        return Err(OriginalSimError::ApiError(format!(
            "Quote API failed with status {}: {}", status, text
        )));
    }
    Ok(response.json::<QuoteResponse>().await?)
}


pub(crate) async fn get_swap_transaction(
    http_client: &ReqwestClient,
    quote_response: &QuoteResponse,
    user_public_key: &Pubkey,
) -> Result<SwapResponse, OriginalSimError> {
    let payload = SwapRequestPayload {
        quote_response,
        user_public_key: user_public_key.to_string(),
        wrap_and_unwrap_sol: true,
        // prioritization_fee_lamports: "auto".to_string(), // Removed
        compute_unit_price: Some(PRIORITY_FEE_MICRO_LAMPORTS), // Use constant defined in create_and_send_tx
        compute_unit_limit: Some(SIMULATION_SWAP_COMPUTE_LIMIT), // Use swap-specific limit
    };
    debug!("Fetching swap transaction with explicit CU limit ({}) and price ({})...", SIMULATION_SWAP_COMPUTE_LIMIT, PRIORITY_FEE_MICRO_LAMPORTS);
    let response = http_client
        .post(JUPITER_SWAP_API_URL)
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        error!("Swap API Error Response: Status {}, Body: {}", status, text);
        return Err(OriginalSimError::ApiError(format!(
            "Swap API failed with status {}: {}", status, text
        )));
    }
    Ok(response.json::<SwapResponse>().await?)
}

// Simplified execute_swap (no Jito initially)
pub(crate) async fn execute_swap(
    rpc_client: &RpcClient,
    swap_tx_base64: &str,
    wallet_keypair: &Keypair,
) -> Result<Signature, OriginalSimError> {
    let swap_tx_bytes = BASE64_STANDARD.decode(swap_tx_base64)?;
    debug!("Transaction Decoded from Base64.");

    let versioned_tx: VersionedTransaction = bincode::deserialize(&swap_tx_bytes)?;
    debug!("Transaction Deserialized.");

    // We need the message to sign
    let message = versioned_tx.message;

    // Sign the transaction with the wallet's keypair ONLY
    // Jupiter swap tx should already be signed by Jupiter, we just need user signature
    let signed_tx = VersionedTransaction::try_new(message, &[wallet_keypair])
         .map_err(|e| OriginalSimError::ConfigError(format!("Failed to sign swap transaction: {}", e)))?; // Use ConfigError or similar
    debug!("Transaction Signed by user.");

    // Send via standard RPC
    // --- Send and Manually Confirm Swap ---
    debug!(" -> Sending swap transaction...");
    let signature = rpc_client.send_transaction(&signed_tx).await
        .map_err(OriginalSimError::SolanaClientError)?;
    info!(" -> Swap transaction sent. Signature: {}", signature);

    debug!(" -> Confirming swap transaction {}...", signature);
    let confirmation_start = Instant::now();
    let confirmation_timeout = Duration::from_secs(90); // Longer timeout for swaps (90s)

    loop {
        if confirmation_start.elapsed() > confirmation_timeout {
            error!(" -> Swap confirmation timed out for {}", signature);
            return Err(OriginalSimError::ConfirmationTimeout(signature));
        }

        match rpc_client.get_signature_statuses(&[signature]).await {
            Ok(statuses) => {
                if let Some(Some(status)) = statuses.value.get(0) {
                    if let Some(confirmation_status) = &status.confirmation_status {
                        match confirmation_status {
                            solana_transaction_status::TransactionConfirmationStatus::Processed => {
                                debug!(" -> {} status: Processed. Waiting for confirmation...", signature);
                            }
                            solana_transaction_status::TransactionConfirmationStatus::Confirmed |
                            solana_transaction_status::TransactionConfirmationStatus::Finalized => {
                                info!(" -> Swap transaction {} confirmed/finalized.", signature);
                                if let Some(err) = &status.err {
                                     error!("    -> On-chain execution failed! Error: {:?}", err);
                                     return Err(OriginalSimError::TransactionError(err.clone()));
                                }
                                return Ok(signature);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!(" -> Error fetching swap status for {}: {:?}. Retrying...", signature, e);
            }
        }
        sleep(Duration::from_secs(3)).await; // Wait 3 seconds before polling swap status
    }
    // --- End Send and Manually Confirm Swap ---
}


// --- Simulator Implementation ---
// (Copied and adapted from pumper/src/main.rs Simulator)

// Removed #[derive(Debug)] because RpcClient doesn't implement it
pub struct Simulator {
    rpc_client: Arc<RpcClient>,
    http_client: Arc<ReqwestClient>,
    wallets: Vec<Wallet>,
    token_mint: Pubkey,
    slippage_bps: u16,
    log: Vec<LogEntry>,
    step_count: usize,
    wallets_file_path: String, // Added to save log relative to wallets file
    target_sol_per_zombie: u64, // Target SOL amount for each zombie wallet
    // Removed Jito related fields
}

impl Simulator {
    // Updated constructor to take clients and config directly
    pub fn new(
        rpc_client: Arc<RpcClient>,
        http_client: Arc<ReqwestClient>,
        wallets: Vec<Wallet>,
        token_mint: Pubkey,
        slippage_bps: u16,
        wallets_file_path: String,
        initial_sol_dev: f64, // Total SOL in dev wallet meant for distribution
    ) -> Result<Self, OriginalSimError> { // Return Result
        if wallets.len() < 2 || wallets[0].id != 0 { // Need minter (ID 0) and at least one zombie
            // Return an error instead of panicking
            return Err(OriginalSimError::ConfigError(
                "Simulator requires at least two wallets, and Wallet ID 0 must be the MINTER.".to_string()
            ));
        }

        let num_zombies = wallets.len() - 1;
        let total_initial_lamports_dev = sol_to_lamports(initial_sol_dev);

        // Calculate SOL available for distribution from minter, leaving a buffer
        let lamports_available_for_distribution = total_initial_lamports_dev.saturating_sub(FEE_BUFFER_SWAP); // Leave buffer (e.g., 0.01 SOL)

        // Calculate target SOL per zombie directly
        let target_sol_per_zombie = if num_zombies > 0 {
            lamports_available_for_distribution / (num_zombies as u64)
        } else {
            0
        };

        // Check if the target is enough for rent + next fee buffer
        const RENT_EXEMPT_MINIMUM_LOCAL_SIM: u64 = 890_880; // Rent for system account
        let _required_for_rent_and_fee = RENT_EXEMPT_MINIMUM_LOCAL_SIM + FEE_BUFFER_TRANSFER; // Use existing constant

        

        info!("Target SOL per zombie wallet (after estimated fees): {} lamports ({} SOL)", target_sol_per_zombie, lamports_to_sol(target_sol_per_zombie));


        Ok(Self {
            rpc_client,
            http_client,
            wallets,
            token_mint,
            slippage_bps,
            log: Vec::new(),
            step_count: 0,
            wallets_file_path,
            target_sol_per_zombie, // Store calculated target
        })
    }

    fn add_log(&mut self, event_type: &str, details: serde_json::Value) {
        let entry = LogEntry {
            timestamp: Utc::now().to_rfc3339(), // Add timestamp
            step: self.step_count,
            event_type: event_type.to_string(),
            details: details.clone(),
        };
        // Use log crate
        info!("Step {}: {} - {}", self.step_count, event_type, details);
        self.log.push(entry);
    }

    // Helper to get a random wallet ID excluding specified IDs
    fn get_random_wallet_id_exclude(&self, exclude_ids: &HashSet<usize>) -> Option<usize> {
        let mut rng = rand::thread_rng();
        let available_wallets: Vec<usize> = self.wallets.iter()
            .map(|w| w.id)
            .filter(|id| !exclude_ids.contains(id))
            .collect();

        available_wallets.choose(&mut rng).copied()
    }

     // Helper to get a random wallet that holds the token, excluding specified IDs
     async fn get_random_wallet_with_token(
         &self,
         exclude_ids: Option<&HashSet<usize>>,
     ) -> Result<Option<&Wallet>, OriginalSimError> {
         let mut potential_holders: Vec<&Wallet> = Vec::new();

         for wallet in &self.wallets {
             // Skip excluded wallets if provided
             if let Some(excluded) = exclude_ids {
                 if excluded.contains(&wallet.id) {
                     continue;
                 }
             }
             // Skip minter (wallet 0) implicitly for selling/dispersing actions
             if wallet.id == 0 {
                 continue;
             }

             match get_token_balance(&self.rpc_client, &wallet.pubkey(), &self.token_mint).await {
                 Ok(balance) if balance > 0 => {
                     potential_holders.push(wallet);
                 }
                 Ok(_) => {} // Balance is 0, skip
                 Err(e) => {
                     warn!("Failed to get token balance for wallet {} during random selection: {:?}", wallet.id, e);
                     // Don't stop the whole process, just skip this wallet
                 }
             }
         }

         // Create RNG *after* the loop and awaits
         let mut rng = rand::thread_rng();
         Ok(potential_holders.choose(&mut rng).copied())
     }


    // Helper to get a specific wallet, excluding minter (wallet 0)
    fn get_wallet_exclude_minter(&self, wallet_id: usize) -> Option<&Wallet> {
        if wallet_id == 0 {
            return None;
        }
        self.wallets.get(wallet_id)
    }


    // Ensure a zombie wallet has the target SOL amount, transferring from/to minter if needed.
    async fn ensure_sol_balance(&self, wallet_id: usize) -> Result<(), OriginalSimError> {
        if wallet_id == 0 { // Minter doesn't need balance adjustment relative to itself
            return Ok(());
        }
        if self.target_sol_per_zombie == 0 { // If target is 0, no adjustment needed
             return Ok(());
        }

        let target_wallet = self.wallets.get(wallet_id)
            .ok_or_else(|| OriginalSimError::ConfigError(format!("Wallet ID {} not found for balance check", wallet_id)))?;
        let target_pubkey = target_wallet.pubkey();

        let current_balance = get_sol_balance(&self.rpc_client, &target_pubkey).await?;

        if current_balance != self.target_sol_per_zombie {
            let amount_to_transfer: i64 = self.target_sol_per_zombie as i64 - current_balance as i64;

            if amount_to_transfer == 0 {
                // Already has the exact amount
                return Ok(());
            }

            let (source_wallet, recipient_pubkey, transfer_lamports, action_label) = if amount_to_transfer > 0 {
                // Needs top-up from minter
                let minter_wallet = self.wallets.get(0).unwrap(); // Wallet 0 is minter
                // Add buffer to amount needed to cover the transfer fee
                let amount_to_send = (amount_to_transfer as u64) + FEE_BUFFER_TRANSFER;
                info!(
                    "Wallet {} needs {} SOL (has {}). Funding {} SOL from Minter.",
                    wallet_id, lamports_to_sol(amount_to_transfer as u64), lamports_to_sol(current_balance), lamports_to_sol(amount_to_send)
                );
                (minter_wallet, target_pubkey, amount_to_send, "Top-up")
            } else {
                // Has too much, needs to send back to minter
                let minter_pubkey = self.wallets[0].pubkey();
                let excess_amount = amount_to_transfer.abs() as u64;
                // Check if excess is enough to cover its own transfer fee
                if excess_amount <= FEE_BUFFER_TRANSFER {
                     warn!("Wallet {} excess SOL ({}) is not enough to cover transfer fee. Skipping return.", wallet_id, excess_amount);
                     return Ok(()); // Skip the transfer back
                }
                let amount_to_send_back = excess_amount.saturating_sub(FEE_BUFFER_TRANSFER);
                if amount_to_send_back == 0 {
                     warn!("Wallet {} excess SOL ({}) is exactly the transfer fee buffer. Skipping return.", wallet_id, excess_amount);
                     return Ok(()); // Skip the transfer back
                }
                 info!(
                    "Wallet {} has excess {} SOL (has {}). Returning {} SOL to Minter.",
                    wallet_id, lamports_to_sol(excess_amount), lamports_to_sol(current_balance), lamports_to_sol(amount_to_send_back)
                );
                (target_wallet, minter_pubkey, amount_to_send_back, "Return excess")
            };

            // Check source balance BEFORE attempting transfer
            let source_pubkey = source_wallet.pubkey();
            let source_balance = get_sol_balance(&self.rpc_client, &source_pubkey).await?;
            // Check if source has enough for the transfer amount itself (fee is handled by send_transaction)
            if source_balance < transfer_lamports {
                 let err_msg = format!(
                     "Source wallet {} has insufficient funds ({}) to {} {} lamports to {}.",
                     source_pubkey, lamports_to_sol(source_balance), action_label, transfer_lamports, recipient_pubkey
                 );
                 error!("{}", err_msg);
                 return Err(OriginalSimError::ConfigError(err_msg));
            }

            // Perform transfer
            match transfer_sol(&self.rpc_client, &source_wallet.keypair, &recipient_pubkey, transfer_lamports).await {
                Ok(sig) => {
                    info!(" -> {} successful for wallet {}: {}", action_label, wallet_id, sig);
                    // Add a small delay for balance update
                    sleep(Duration::from_secs(3)).await;
                    Ok(())
                }
                Err(e) => {
                    error!(" -> {} FAILED for wallet {}: {:?}", action_label, wallet_id, e);
                    Err(e) // Propagate the error
                }
            }
        } else {
            debug!("Wallet {} already has target SOL balance ({}).", wallet_id, lamports_to_sol(current_balance));
            Ok(())
        }
    }


    // --- Simulation Actions ---

    async fn buy_action(
        &mut self, // Need mutable self to log
        buyer_id: usize
    ) -> Result<(), OriginalSimError> {
        info!("==> BUY Action initiated by Wallet {}", buyer_id);
        let buyer_wallet = self.get_wallet_exclude_minter(buyer_id)
            .ok_or(OriginalSimError::NoSuitableWallet)?;
        let buyer_pubkey = buyer_wallet.pubkey();
        let _buyer_keypair = &buyer_wallet.keypair; // Borrow keypair

        // Ensure buyer has enough SOL for minimum buy + fee buffer
        let _required_sol = MIN_BUY_LAMPORTS + FEE_BUFFER_SWAP;
        self.ensure_sol_balance(buyer_id).await?; // Removed required_sol argument

        // Get actual balance after potential top-up
        let current_sol_buyer = get_sol_balance(&self.rpc_client, &buyer_pubkey).await?;
        let available_for_buy = current_sol_buyer.saturating_sub(FEE_BUFFER_SWAP);

        if available_for_buy < MIN_BUY_LAMPORTS {
            warn!(
                "Wallet {} still has insufficient SOL ({}) for minimum buy ({}) after top-up attempt. Skipping buy.",
                buyer_id, lamports_to_sol(current_sol_buyer), lamports_to_sol(MIN_BUY_LAMPORTS)
            );
            return Ok(()); // Not an error, just can't perform action
        }

        // Use the full available balance for the buy
        let buy_amount_lamports = available_for_buy; // Use the full available amount

        info!(
            "Wallet {} attempting to buy with {} SOL (full available balance)",
            buyer_id, lamports_to_sol(buy_amount_lamports)
        );

        // Get quote
        match get_jupiter_quote(&self.http_client, &self.token_mint.to_string(), buy_amount_lamports, self.slippage_bps).await {
            Ok(quote) => {
                let estimated_tokens_str = quote.out_amount.clone();
                info!(" -> Quote received: {} SOL for ~{} tokens", lamports_to_sol(buy_amount_lamports), estimated_tokens_str);

                // Get swap transaction
                match get_swap_transaction(&self.http_client, &quote, &buyer_pubkey).await {
                    Ok(swap_resp) => {
                        // Execute swap
                        // Need to borrow keypair again here
                        let buyer_keypair_ref = &self.wallets[buyer_id].keypair;
                        match execute_swap(&self.rpc_client, &swap_resp.swap_transaction, buyer_keypair_ref).await {
                            Ok(sig) => {
                                info!(" -> Wallet {} BUY successful: {}", buyer_id, sig);
                                self.add_log("buy", json!({ "wallet_id": buyer_id, "sol_spent": buy_amount_lamports, "tokens_quoted": estimated_tokens_str, "signature": sig.to_string() }));
                                // Omit price logging for now
                            }
                            Err(e) => {
                                error!(" -> Wallet {} BUY execution FAILED: {:?}", buyer_id, e);
                                self.add_log("buy_fail", json!({ "wallet_id": buyer_id, "sol_spent": buy_amount_lamports, "error": e.to_string() }));
                            }
                        }
                    }
                    Err(e) => {
                        error!(" -> Wallet {} get_swap_transaction FAILED: {:?}", buyer_id, e);
                        self.add_log("buy_fail", json!({ "wallet_id": buyer_id, "sol_spent": buy_amount_lamports, "error": format!("get_swap_tx failed: {}", e) }));
                    }
                }
            }
            Err(e) => {
                error!(" -> Wallet {} get_jupiter_quote FAILED: {:?}", buyer_id, e);
                self.add_log("buy_fail", json!({ "wallet_id": buyer_id, "sol_spent": buy_amount_lamports, "error": format!("get_quote failed: {}", e) }));
            }
        }
        Ok(())
    }


    async fn sell_action(
        &mut self, // Need mutable self to log
        seller_id: usize
    ) -> Result<(), OriginalSimError> {
        info!("==> SELL Action initiated by Wallet {}", seller_id);
        let seller_wallet = self.get_wallet_exclude_minter(seller_id)
             .ok_or(OriginalSimError::NoSuitableWallet)?;
        let seller_pubkey = seller_wallet.pubkey();
        let _seller_keypair = &seller_wallet.keypair; // Borrow keypair

        // Ensure seller has enough SOL for the swap fee buffer
        self.ensure_sol_balance(seller_id).await?; // Removed FEE_BUFFER_SWAP argument

        // Get token balance
        let token_balance = get_token_balance(&self.rpc_client, &seller_pubkey, &self.token_mint).await?;

        if token_balance < MIN_SELL_TOKENS {
            info!("Wallet {} has insufficient tokens ({}) to sell (min {}). Skipping sell.", seller_id, token_balance, MIN_SELL_TOKENS);
            return Ok(());
        }
// --- gather_and_sell_action moved before step function ---
        // Decide amount to sell (all tokens for now)
        let sell_amount_tokens = token_balance;
        info!("Wallet {} attempting to sell {} tokens", seller_id, sell_amount_tokens);

        // Get quote (Token -> SOL)
        match get_jupiter_quote_token_to_sol(&self.http_client, &self.token_mint.to_string(), sell_amount_tokens, self.slippage_bps).await {
            Ok(quote) => {
                let estimated_sol_str = quote.out_amount.clone();
                let parsed_sol_estimate = estimated_sol_str.parse::<u64>().unwrap_or(0);
                info!(" -> Quote received: {} tokens for ~{} SOL", sell_amount_tokens, lamports_to_sol(parsed_sol_estimate));

                // Get swap transaction
                match get_swap_transaction(&self.http_client, &quote, &seller_pubkey).await {
                    Ok(swap_resp) => {
                        // Execute swap
                        // Need to borrow keypair again
                        let seller_keypair_ref = &self.wallets[seller_id].keypair;
                        match execute_swap(&self.rpc_client, &swap_resp.swap_transaction, seller_keypair_ref).await {
                            Ok(sig) => {
                                info!(" -> Wallet {} SELL successful: {}", seller_id, sig);
                                self.add_log("sell", json!({ "wallet_id": seller_id, "tokens_sold": sell_amount_tokens, "sol_quoted": parsed_sol_estimate, "signature": sig.to_string() }));
                                // Omit price logging for now
                            }
                            Err(e) => {
                                error!(" -> Wallet {} SELL execution FAILED: {:?}", seller_id, e);
                                self.add_log("sell_fail", json!({ "wallet_id": seller_id, "tokens_sold": sell_amount_tokens, "error": e.to_string() }));
                            }
                        }
                    }
                    Err(e) => {
                        error!(" -> Wallet {} get_swap_transaction FAILED: {:?}", seller_id, e);
                        self.add_log("sell_fail", json!({ "wallet_id": seller_id, "tokens_sold": sell_amount_tokens, "error": format!("get_swap_tx failed: {}", e) }));
                    }
                }
            }
            // Handle specific Jupiter error for no route found
            Err(OriginalSimError::ApiError(msg)) if msg.contains("Could not find any route") || msg.contains("COULD_NOT_FIND_ANY_ROUTE") => {
                 info!(" -> Skipping SELL for Wallet {}: Jupiter could not find a route for {} tokens.", seller_id, sell_amount_tokens);
                 self.add_log("sell_skip", json!({ "wallet_id": seller_id, "reason": "Jupiter route not found", "token_amount": sell_amount_tokens }));
            }
            Err(e) => {
                error!(" -> Wallet {} get_jupiter_quote (Token->SOL) FAILED: {:?}", seller_id, e);
                self.add_log("sell_fail", json!({ "wallet_id": seller_id, "tokens_sold": sell_amount_tokens, "error": format!("get_quote failed: {}", e) }));
            }
        }
        Ok(())
    }

    async fn disperse_action(
        &mut self, // Need mutable self to log
        // disperser_id: usize // Removed: Always disperse from Wallet 0
    ) -> Result<(), OriginalSimError> {
        info!("==> DISPERSE Action initiated from Minter (Wallet 0)");
        let disperser_wallet = self.wallets.get(0)
            .ok_or_else(|| OriginalSimError::ConfigError("Minter wallet (ID 0) not found for dispersal".to_string()))?;
        let disperser_pubkey = disperser_wallet.pubkey();
        // Create an owned Keypair from bytes to avoid borrowing `self` across `.await`
        let disperser_keypair_owned = Keypair::from_bytes(&disperser_wallet.keypair.to_bytes())
             // Map the specific error from from_bytes to a more general one
             // Explicitly handle the ed25519_dalek error type
             .map_err(|e: ed25519_dalek::ed25519::Error| OriginalSimError::ConfigError(format!("Failed to recreate minter keypair from bytes: {}", e)))?;

        // Get minter's SOL balance
        let sol_balance = get_sol_balance(&self.rpc_client, &disperser_pubkey).await?;

        let num_zombies = self.wallets.len().saturating_sub(1);
        if num_zombies == 0 {
            info!(" -> No zombie wallets to disperse SOL to. Skipping dispersal.");
            return Ok(());
        }

        // Calculate total SOL available for dispersal, leaving a buffer
        let dispersible_sol_total = sol_balance.saturating_sub(FEE_BUFFER_SWAP); // Leave a buffer like 0.01 SOL

        if dispersible_sol_total == 0 {
            info!(" -> Minter has no dispersible SOL (after buffer). Skipping dispersal.");
            return Ok(());
        }

        // Calculate SOL per zombie
        let sol_per_zombie = dispersible_sol_total / (num_zombies as u64);

        if sol_per_zombie == 0 {
            info!(" -> Calculated SOL per zombie is 0. Skipping dispersal.");
            return Ok(());
        }

        info!(
            " -> Dispersing {} SOL total ({} SOL each) from Minter to {} zombies.",
            lamports_to_sol(sol_per_zombie * num_zombies as u64), lamports_to_sol(sol_per_zombie), num_zombies
        );

        // Loop through zombies and transfer
        for zombie_id in 1..=num_zombies { // Iterate from 1 up to num_zombies
            let recipient_wallet = &self.wallets[zombie_id];
            let recipient_pubkey = recipient_wallet.pubkey();

            info!("    -> Sending {} SOL to Wallet {} ({})", lamports_to_sol(sol_per_zombie), zombie_id, recipient_pubkey);
            // Use the owned keypair here
            match transfer_sol(&self.rpc_client, &disperser_keypair_owned, &recipient_pubkey, sol_per_zombie).await {
                Ok(sig) => {
                    info!("    -> SOL dispersal successful: {}", sig);
                    self.add_log("disperse_sol", json!({ "from_id": 0, "to_id": zombie_id, "sol_amount": sol_per_zombie, "signature": sig.to_string() }));
                }
                Err(e) => {
                    error!("    -> SOL dispersal FAILED: {:?}", e);
                    self.add_log("disperse_sol_fail", json!({ "from_id": 0, "to_id": zombie_id, "sol_amount": sol_per_zombie, "error": e.to_string() }));
                    // Continue dispersing to other wallets even if one fails
                }
            }
            sleep(Duration::from_millis(200)).await; // Small delay between transfers
        }

        // Removed token dispersal logic
        Ok(())
    }

    // --- Simulation Step Logic ---

async fn gather_and_sell_action(&mut self) -> Result<(), OriginalSimError> {
        info!("==> GATHER & SELL Action initiated...");
        self.add_log("gather_and_sell_start", json!({}));

        let minter_wallet = &self.wallets[0]; // Wallet 0 is minter/recipient
        let minter_pubkey = minter_wallet.pubkey();
        let minter_keypair_bytes = minter_wallet.keypair.to_bytes(); // Get bytes (Copy)

        let mut gather_success_count = 0;
        let mut gather_failure_count = 0;
        let num_wallets = self.wallets.len();
        let num_zombies = num_wallets.saturating_sub(1);

        info!(" -> Gathering tokens from {} zombie wallets to Minter {}", num_zombies, minter_pubkey);

        // --- Gather Phase ---
        // Iterate by index (1 to num_wallets - 1)
        for zombie_id in 1..num_wallets {
            // Get references locally within the loop iteration
            let zombie_wallet = &self.wallets[zombie_id];
            let zombie_pubkey = zombie_wallet.pubkey();
            let zombie_keypair_ref = &zombie_wallet.keypair;

            let token_balance = match get_token_balance(&self.rpc_client, &zombie_pubkey, &self.token_mint).await {
                Ok(bal) => bal,
                Err(e) => {
                    warn!("Failed to get token balance for zombie {}: {:?}. Skipping gather.", zombie_id, e);
                    gather_failure_count += 1;
                    continue;
                }
            };

            if token_balance == 0 {
                debug!("Zombie {} has 0 tokens. Skipping gather.", zombie_id);
                continue; // Skip if no tokens
            }

            info!(" -> Attempting to gather {} tokens from Wallet {} ({})", token_balance, zombie_id, zombie_pubkey);

            // Ensure zombie has enough SOL for the transfer fee
            let zombie_sol = get_sol_balance(&self.rpc_client, &zombie_pubkey).await?;
            if zombie_sol < FEE_BUFFER_TRANSFER { // Use the transfer buffer constant
                 warn!(" -> Zombie {} has insufficient SOL ({}) for token transfer fee. Skipping gather.", zombie_id, zombie_sol);
                 gather_failure_count += 1;
                 continue;
            }

            // Minter pays for potential ATA creation on their side
            // Recreate minter keypair from bytes inside the loop to avoid borrow issues
            let minter_keypair_for_transfer = Keypair::from_bytes(&minter_keypair_bytes)
                .map_err(|e| OriginalSimError::ConfigError(format!("Failed to recreate minter keypair for transfer: {}", e)))?; // Handle potential error

            match transfer_spl_tokens(&self.rpc_client, &self.token_mint, zombie_keypair_ref, &minter_pubkey, token_balance, &minter_keypair_for_transfer).await { // Pass reference
                Ok(sig) => {
                    info!("    -> Gather successful from Wallet {}: {}", zombie_id, sig);
                    self.add_log("gather_token", json!({ "from_id": zombie_id, "to_id": 0, "token_amount": token_balance, "signature": sig.to_string() }));
                    gather_success_count += 1;
                }
                Err(e) => {
                    error!("    -> Gather FAILED from Wallet {}: {:?}", zombie_id, e);
                    self.add_log("gather_token_fail", json!({ "from_id": zombie_id, "to_id": 0, "token_amount": token_balance, "error": e.to_string() }));
                    gather_failure_count += 1;
                    // Continue gathering from others even if one fails
                }
            }
             // Optional delay between gathers?
             // sleep(Duration::from_millis(100)).await;
        }
        info!(" -> Gather phase complete. Success: {}, Failed: {}", gather_success_count, gather_failure_count);

        // --- Sell Phase (from Minter Wallet 0) ---
        info!(" -> Proceeding to sell gathered tokens from Minter Wallet 0...");
        // Call sell_action for wallet 0
        match self.sell_action(0).await {
             Ok(_) => {
                 info!(" -> Final sell action from Minter completed (check logs for success/failure).");
                 // Log entry for sell is handled within sell_action
             }
             Err(e) => {
                 error!(" -> Final sell action from Minter FAILED: {:?}", e);
                 self.add_log("gather_and_sell_final_sell_fail", json!({ "error": e.to_string() }));
                 // Return the error from the sell action
                 return Err(e);
             }
        }

        self.add_log("gather_and_sell_finish", json!({ "gathered_success": gather_success_count, "gathered_failed": gather_failure_count }));
        Ok(())
    }
    async fn step(&mut self) -> Result<(), OriginalSimError> {
        self.step_count += 1;
        info!("--- Simulation Step {} ---", self.step_count);

        // Choose a random action: Buy, Sell, Disperse
        let action = rand::thread_rng().gen_range(0..100);

        // Choose a random wallet (excluding minter 0)
        let wallet_id = rand::thread_rng().gen_range(1..self.wallets.len());

        // Choose between Buy (40%), Sell (40%), Gather & Sell (20%)
        let result = if action < 40 { // 40% chance Buy
            self.buy_action(wallet_id).await
        } else if action < 80 { // 40% chance Sell (from a random token holder)
            // For selling, we need a wallet that *has* tokens.
            // Let's try to find one randomly. If none found, skip sell for this step.
            match self.get_random_wallet_with_token(None).await? {
                Some(seller_wallet) => self.sell_action(seller_wallet.id).await,
                None => {
                    info!("No wallets with tokens found for random selling. Skipping sell action for step {}.", self.step_count);
                    Ok(()) // Return Ok(()) to indicate a skipped action, not an error
                }
            }
        } else { // 20% chance Disperse SOL from Minter
            self.disperse_action().await // Removed wallet_id argument
        };

        // Log errors from actions but don't stop the simulation loop unless it's critical
        match result {
            Ok(_) => debug!("Step {} action completed.", self.step_count),
            Err(e) => {
                error!("Error during step {}: {:?}", self.step_count, e);
                // Decide if the error is fatal (e.g., config error) or recoverable
                match e {
                    OriginalSimError::ConfigError(_) | OriginalSimError::InvalidPrivateKey(_) => return Err(e), // Fatal errors
                    _ => {} // Continue simulation for other errors
                }
            }
        }

        // Small delay between steps
        sleep(Duration::from_millis(500)).await;
        Ok(())
    }

    // --- Main Run Loop ---
    // Updated signature to accept parameters
    pub async fn run(&mut self, max_steps: usize) -> Result<(), OriginalSimError> {
        info!("Starting Original Simulation for {} steps...", max_steps);
        for _ in 0..max_steps {
            match self.step().await {
                Ok(_) => {}
                Err(OriginalSimError::SimulationFinished) => {
                    info!("Simulation finished early.");
                    break;
                }
                Err(e) => {
                    error!("Simulation run failed: {:?}", e);
                    // Attempt to save log before returning error
                    if let Err(log_err) = self.save_log() {
                         error!("Failed to save log after simulation error: {:?}", log_err);
                    }
                    return Err(e); // Propagate fatal errors
                }
            }
        }

        info!("Simulation completed {} steps.", self.step_count);

        // Consolidate funds at the end (optional, adapted from pumper)
        info!("Attempting final consolidation...");
        if let Err(e) = self.consolidate_funds().await {
             error!("Error during final consolidation: {:?}", e);
             // Don't return error, just log it
        }

        self.save_log()?; // Save log at the end
        Ok(())
    }

    // --- Consolidation Logic ---
    // (Adapted from pumper/src/main.rs consolidate_funds)
     async fn consolidate_funds(&self) -> Result<(), OriginalSimError> {
         info!("--- Consolidating Funds to Minter (Wallet 0) ---");
         let minter_wallet = self.wallets.get(0)
             .ok_or_else(|| OriginalSimError::ConfigError("Minter wallet (ID 0) not found for consolidation".to_string()))?;
         let minter_pubkey = minter_wallet.pubkey();
         let _minter_keypair_ref = &minter_wallet.keypair; // Borrow minter keypair

         for wallet in self.wallets.iter().skip(1) { // Skip minter (wallet 0)
             let wallet_id = wallet.id;
             let wallet_pubkey = wallet.pubkey();
             let _wallet_keypair = &wallet.keypair; // Borrow current wallet keypair
             info!("Consolidating Wallet {}: {}", wallet_id, wallet_pubkey);

             // --- Consolidate Tokens ---
             let token_balance = match get_token_balance(&self.rpc_client, &wallet_pubkey, &self.token_mint).await {
                 Ok(bal) => bal,
                 Err(e) => {
                     error!(" -> Failed to get token balance for wallet {}: {:?}. Skipping token consolidation.", wallet_id, e);
                     0 // Treat as 0 balance
                 }
             };

             if token_balance > 0 {
                 info!(" -> Transferring {} tokens to minter...", token_balance);
                 // Need wallet and minter keypair refs
                 let wallet_kp_ref = &self.wallets[wallet_id].keypair;
                 let minter_kp_ref_for_ata = &self.wallets[0].keypair; // Minter pays for potential ATA creation
                 match transfer_spl_tokens(&self.rpc_client, &self.token_mint, wallet_kp_ref, &minter_pubkey, token_balance, minter_kp_ref_for_ata).await {
                     Ok(sig) => info!("    -> Token consolidation successful: {}", sig),
                     Err(e) => error!("    -> Token consolidation FAILED: {:?}", e),
                 }
                 sleep(Duration::from_secs(1)).await; // Delay after token transfer
             } else {
                 info!(" -> No tokens to consolidate.");
             }

             // --- Close Token Account (Optional but Recommended) ---
             // Check if ATA exists and has 0 balance after potential transfer
             let ata_pubkey = get_associated_token_address(&wallet_pubkey, &self.token_mint);
             match get_token_balance(&self.rpc_client, &wallet_pubkey, &self.token_mint).await {
                  Ok(0) => {
                      info!(" -> Attempting to close empty ATA: {}", ata_pubkey);
                      // Ensure wallet has SOL for close fee
                      let _sol_for_close = FEE_BUFFER_TRANSFER; // Use transfer buffer
                      if let Err(e) = self.ensure_sol_balance(wallet_id).await { // Removed sol_for_close argument
                          warn!(" -> Cannot ensure SOL balance for closing ATA: {:?}. Skipping close.", e);
                      } else {
                          let close_ata_ix = match close_account(
                              &TOKEN_PROGRAM_ID,
                              &ata_pubkey,      // Account to close
                              &wallet_pubkey,   // Destination for remaining SOL (rent)
                              &wallet_pubkey,   // Owner of the ATA
                              &[],              // Signers
                          ) {
                              Ok(ix) => ix,
                              Err(e) => {
                                  error!(" -> Error creating close_account instruction: {:?}", e);
                                  continue; // Skip to next wallet
                              }
                          };

                          // Send close transaction, signed by the wallet owner
                          let wallet_kp_ref = &self.wallets[wallet_id].keypair;
                          match create_and_send_tx(&self.rpc_client, &[close_ata_ix], wallet_kp_ref, &[]).await {
                              Ok(sig) => info!("    -> ATA close successful: {}", sig),
                              Err(e) => error!("    -> ATA close FAILED: {:?}", e),
                          }
                          sleep(Duration::from_secs(1)).await;
                      }
                  }
                  Ok(bal) => info!(" -> ATA {} not empty ({} tokens). Skipping close.", ata_pubkey, bal),
                  Err(OriginalSimError::SolanaClientError(ref cli_err)) if cli_err.to_string().contains("AccountNotFound") => {
                       info!(" -> ATA {} does not exist. Skipping close.", ata_pubkey);
                  }
                  Err(e) => warn!(" -> Error checking token balance for closing ATA {}: {:?}. Skipping close.", ata_pubkey, e),
             }


             // --- Consolidate SOL ---
             // Get balance *after* potential token transfer/ATA close
             let final_sol_balance = match get_sol_balance(&self.rpc_client, &wallet_pubkey).await {
                 Ok(bal) => bal,
                 Err(e) => {
                     error!(" -> Failed to get final SOL balance for wallet {}: {:?}. Skipping SOL consolidation.", wallet_id, e);
                     continue; // Skip SOL consolidation for this wallet
                 }
             };

             let sol_to_transfer = final_sol_balance.saturating_sub(FEE_BUFFER_TRANSFER); // Leave buffer for fee

             if sol_to_transfer > 0 {
                 info!(" -> Transferring {} SOL to minter...", lamports_to_sol(sol_to_transfer));
                 // Need wallet keypair ref
                 let wallet_kp_ref = &self.wallets[wallet_id].keypair;
                 match transfer_sol(&self.rpc_client, wallet_kp_ref, &minter_pubkey, sol_to_transfer).await {
                     Ok(sig) => info!("    -> SOL consolidation successful: {}", sig),
                     Err(e) => error!("    -> SOL consolidation FAILED: {:?}", e),
                 }
             } else {
                 info!(" -> Not enough SOL to consolidate ({} lamports).", final_sol_balance);
             }
             sleep(Duration::from_secs(1)).await; // Delay before next wallet
         } // End loop through wallets

         info!("--- Consolidation Complete ---");
         Ok(())
     }


    // --- Save Log ---
    // (Adapted from pumper/src/main.rs save_log)
    fn save_log(&self) -> Result<(), OriginalSimError> {
        let wallets_path = Path::new(&self.wallets_file_path);
        let log_file_name = wallets_path.file_stem()
            .map(|stem| format!("{}_orig_simulation_log.json", stem.to_string_lossy())) // Add _orig_
            .unwrap_or_else(|| "orig_simulation_log.json".to_string());

        let log_path = if let Some(parent) = wallets_path.parent() {
            parent.join(&log_file_name)
        } else {
            Path::new(&log_file_name).to_path_buf()
        };

        info!("Saving original simulation log to {}...", log_path.display());
        match OpenOptions::new().write(true).create(true).truncate(true).open(&log_path) {
            Ok(file) => {
                let writer = BufWriter::new(file);
                if let Err(e) = serde_json::to_writer_pretty(writer, &self.log) {
                    error!("Error writing simulation log: {}", e);
                    Err(OriginalSimError::JsonError(e))
                } else {
                    info!("Log saved successfully.");
                    Ok(())
                }
            }
            Err(e) => {
                error!("Error creating/opening log file '{}': {}", log_path.display(), e);
                Err(OriginalSimError::IoError(e))
            }
        }
    }
} // End impl Simulator


// --- Public Runner Function ---
// This function will be called by the GUI
pub async fn run_original_simulation(
    rpc_url: String, // Get RPC URL from GUI settings
    wallets_file_path: String,
    token_mint_str: String,
    num_wallets: usize,
    initial_sol_minter: f64, // Initial SOL for Wallet 0
    max_steps: usize,
    slippage_bps: u16,
    // Updated sender type to match GUI
    status_sender: Option<tokio::sync::mpsc::UnboundedSender<Result<String, String>>>,
) -> Result<(), anyhow::Error> { // Use anyhow::Error for easy error propagation to GUI
    info!("Initializing Original Simulation...");

    // Send initial status
    if let Some(sender) = &status_sender {
        // Send Ok result, no await for unbounded
        let _ = sender.send(Ok("Initializing simulation...".to_string()));
    }

    // Setup clients
    let http_client = Arc::new(ReqwestClient::new());
    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        rpc_url,
        CommitmentConfig::confirmed(), // Use confirmed commitment
    ));

    // Check RPC connection
    if let Err(e) = rpc_client.get_latest_blockhash().await {
         error!("RPC connection failed: {}", e);
         if let Some(sender) = &status_sender {
             // Send Err result, no await
             let _ = sender.send(Err(format!("RPC connection failed: {}", e)));
         }
         return Err(anyhow!("RPC connection failed: {}", e));
    }
    info!("RPC connection successful.");
    if let Some(sender) = &status_sender {
        // Send Ok result, no await
        let _ = sender.send(Ok("RPC connection successful.".to_string()));
    }


    let token_mint = Pubkey::from_str(&token_mint_str)
        .context(format!("Invalid CA: {}", token_mint_str))?;

    // Load or generate wallets
    let wallets = load_or_generate_wallets(&wallets_file_path, num_wallets)
        .map_err(anyhow::Error::from)?; // Convert OriginalSimError to anyhow::Error

    if wallets.is_empty() {
         return Err(anyhow!("Failed to load or generate any wallets."));
    }

    // Fund Minter Wallet (Wallet 0) if needed
    let minter_wallet = &wallets[0];
    let minter_pubkey = minter_wallet.pubkey();
    let required_minter_sol = sol_to_lamports(initial_sol_minter);
    let current_minter_sol = get_sol_balance(&rpc_client, &minter_pubkey).await
         .map_err(anyhow::Error::from)?;

    if current_minter_sol < required_minter_sol {
        warn!(
            "Minter wallet {} has {} SOL, less than the target initial amount of {} SOL. Manual funding might be required.",
            minter_pubkey, lamports_to_sol(current_minter_sol), initial_sol_minter
        );
         if let Some(sender) = &status_sender {
             // Send Ok result (warning), no await
             let _ = sender.send(Ok(format!("Warning: Minter needs manual funding (has {} SOL, needs {} SOL).", lamports_to_sol(current_minter_sol), initial_sol_minter)));
         }
        // NOTE: This simulation version doesn't automatically fund the minter from an external source.
    } else {
         info!("Minter wallet {} has sufficient initial SOL ({} >= {}).", minter_pubkey, lamports_to_sol(current_minter_sol), initial_sol_minter);
    }


    // Initialize simulator
    let mut simulator = Simulator::new(
        rpc_client,
        http_client,
        wallets,
        token_mint,
        slippage_bps,
        wallets_file_path.clone(), // Pass path for logging
        initial_sol_minter, // Pass initial SOL amount for distribution calculation
    ).map_err(anyhow::Error::from)?; // Convert OriginalSimError to anyhow::Error


    // --- Initial Fund Distribution ---
    info!("Distributing initial SOL to zombie wallets...");
    if let Some(sender) = &status_sender {
        let _ = sender.send(Ok("Distributing initial SOL...".to_string()));
    }
    // Iterate through zombie wallets (skip wallet 0)
    for wallet_id in 1..simulator.wallets.len() {
        if let Err(e) = simulator.ensure_sol_balance(wallet_id).await {
            // Log error but continue trying to fund others
            error!("Failed initial funding for wallet {}: {}", wallet_id, e);
            if let Some(sender) = &status_sender {
                 let _ = sender.send(Err(format!("Failed initial funding for wallet {}: {}", wallet_id, e)));
            }
            // Optionally, decide if simulation should halt if initial funding fails for any wallet
            // For now, we log and continue.
        }
    }
    info!("Initial SOL distribution attempt complete.");
    if let Some(sender) = &status_sender {
        let _ = sender.send(Ok("Initial SOL distribution attempt complete.".to_string()));
    }

    // --- Run simulation loop ---
    info!("Starting simulation run (max {} steps)...", max_steps);
     if let Some(sender) = &status_sender {
         let _ = sender.send(Ok("Starting simulation loop...".to_string()));
     }
    // Modify Simulator::run to remove the random disperse action if it's still there
    // For now, assume run just executes buy/sell steps
    let run_result = simulator.run(max_steps).await;

    // --- Consolidate Funds (Optional - can be triggered by GUI instead) ---
    // info!("Consolidating funds back to dev wallet...");
    // if let Err(e) = simulator.consolidate_funds().await {
    //     error!("Fund consolidation failed: {}", e);
    //     // Log error but don't necessarily fail the whole simulation run
    // }

    // --- Save Log ---
    info!("Saving simulation log...");
    if let Err(e) = simulator.save_log() {
        error!("Failed to save simulation log: {}", e);
        // Don't overwrite the main run error if it exists
        if run_result.is_ok() {
             return Err(anyhow::Error::from(e));
        }
    }

    // --- Handle Final Result ---
    match run_result {
        Ok(_) => {
            info!("Simulation completed successfully.");
             if let Some(sender) = status_sender {
                 let _ = sender.send(Ok("Simulation finished.".to_string()));
             }
            Ok(())
        }
        Err(e) => {
            error!("Simulation failed: {}", e);
             if let Some(sender) = status_sender {
                 let _ = sender.send(Err(format!("Simulation failed: {}", e)));
             }
            Err(anyhow::Error::from(e))
        }
    }
}