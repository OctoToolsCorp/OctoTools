//! Simulation logic adapted from the 'pumper' project.

// Standard library imports
use std::{
    collections::HashSet,
    // error::Error, // Removed
    fs::{File, OpenOptions},
    io::{BufReader, BufWriter}, // Removed Write
    path::{Path}, // Removed PathBuf
    str::FromStr,
    sync::Arc,
    time::Duration,
};

// External Crate Imports (ensure these are in the main Cargo.toml)
use anyhow::Context as _; // Import the Context trait for .context() method
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
// use borsh::{BorshDeserialize, BorshSerialize}; // Removed
use bs58;
use chrono::Utc;
use log::{debug, error, info, warn};
use rand::seq::SliceRandom;
use rand::Rng;
use rand::rngs::StdRng;
use rand::SeedableRng;
use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use serde_json::{json}; // Removed Value
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    // Removed RpcSendTransactionConfig, RpcSimulateTransactionConfig
    // Removed RpcSimulateTransactionResult, RpcTokenAccountBalance
};
// Note: Solana SDK version mismatch (2.x vs 1.18) will require significant refactoring later.
use solana_program::{
    instruction::Instruction,
    native_token::lamports_to_sol,
    program_error::ProgramError, // Use this for program errors
    program_pack::Pack,
    pubkey::Pubkey,
    system_instruction,
    // system_program, // Removed
};
use solana_sdk::{
    // commitment_config::CommitmentConfig, // Removed
    compute_budget::ComputeBudgetInstruction,
    signature::{Keypair, Signature, Signer},
    transaction::{Transaction, TransactionError, VersionedTransaction}, // Keep both for now
};
use spl_associated_token_account::{
    get_associated_token_address, instruction::create_associated_token_account,
};
use spl_token::{
    error::TokenError, // Use this for token errors
    instruction::{transfer as spl_transfer}, // Removed close_account
    state::Account as TokenAccount, // For Account::LEN
    ID as TOKEN_PROGRAM_ID, // Use constant for token program ID
};
use thiserror::Error;
use tokio::time::sleep;

// --- Modules ---
pub mod original; // Make the module public
pub use original::run_original_simulation; // Re-export the runner function

// --- Constants ---
// const SOL_MINT_ADDRESS: &str = "So11111111111111111111111111111111111111112"; // Defined in original.rs
const JUPITER_QUOTE_API_URL: &str = "https://quote-api.jup.ag/v6/quote";
const JUPITER_SWAP_API_URL: &str = "https://quote-api.jup.ag/v6/swap";
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
// Simulation V2 specific constants
const BUY_MIN_LAMPORTS_V2: u64 = 8_000_000; // 0.008 SOL
const FEE_BUFFER_SWAP_V2: u64 = 1_000_000; // 0.001 SOL
const CONSOLIDATION_FEE_ESTIMATE_V2: u64 = 2_005_000; // Fee for token/SOL transfers
const RENT_EXEMPT_MINIMUM: u64 = 890_880; // Minimum lamports for rent exemption (system account)

// --- Error Enum ---
#[derive(Debug, Error)]
pub enum SimulationError {
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
    #[error("Simulation finished")]
    SimulationFinished,
    #[error("Simulation configuration error: {0}")]
    ConfigError(String),
}

// --- Data Structs ---

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WalletData {
    name: String,
    address: String,
    key: String, // Private key as base58 string
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

// Debug implementation for Wallet as Keypair doesn't implement Debug
impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("id", &self.id)
            .field("pubkey", &self.keypair.pubkey().to_string())
            // DO NOT include the private key here
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
    prioritization_fee_lamports: String, // "auto" or lamports as string
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    swap_transaction: String, // base64 encoded transaction
}

// --- Wallet Loading ---

pub fn load_or_generate_wallets(file_path: &str, num_wallets: usize) -> Result<Vec<Wallet>, SimulationError> {
    let path = Path::new(file_path);
    let mut wallets = Vec::new();
    let mut wallets_data: Vec<WalletData> = Vec::new();

    if path.exists() {
        info!("Loading wallets from {}...", file_path);
        match File::open(path) {
            Ok(file) => {
                let reader = BufReader::new(file);
                match serde_json::from_reader(reader) {
                    Ok(loaded_data) => {
                        wallets_data = loaded_data;
                        info!("Successfully loaded {} wallet entries.", wallets_data.len());
                        for data in &wallets_data {
                            let parsed_id = data.name.trim_start_matches("wallet").parse::<usize>();
                            let wallet_id = match parsed_id {
                                Ok(id) => id,
                                Err(_) => {
                                    warn!("Could not parse ID from wallet name '{}'. Skipping.", data.name);
                                    continue;
                                }
                            };

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
                        if wallets.len() < num_wallets {
                            info!("Loaded {} valid wallets, need {}. Generating {} more...", wallets.len(), num_wallets, num_wallets - wallets.len());
                        } else if wallets.len() > num_wallets {
                            info!("Loaded {} wallets, using the first {}.", wallets.len(), num_wallets);
                            wallets.truncate(num_wallets);
                            wallets_data.truncate(num_wallets);
                        } else {
                            info!("Loaded exactly {} wallets.", num_wallets);
                            return Ok(wallets);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to parse wallets file '{}': {}. Generating new wallets.", file_path, e);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to open wallets file '{}': {}. Generating new wallets.", file_path, e);
            }
        }
    } else {
        info!("Wallets file '{}' not found. Generating new wallets.", file_path);
    }

    // --- Generation ---
    let start_index = wallets.len();
    if wallets_data.capacity() < num_wallets {
        wallets_data.reserve(num_wallets - wallets_data.capacity());
    }
    for i in start_index..num_wallets {
        let keypair = Keypair::new();
        let id = i;
        info!("Generated Wallet ID: {}, Pubkey: {}", id, keypair.pubkey());
        let wallet_data_entry = WalletData {
            name: format!("wallet{}", id),
            address: keypair.pubkey().to_string(),
            key: keypair.to_base58_string(),
        };
        wallets.push(Wallet { id, keypair });
        if i < wallets_data.len() {
            wallets_data[i] = wallet_data_entry;
        } else {
            wallets_data.push(wallet_data_entry);
        }
    }

    // --- Save ---
    info!("Saving {} wallet entries to {}...", wallets_data.len(), file_path);
    match File::create(path) {
        Ok(file) => {
            let writer = BufWriter::new(file);
            if let Err(e) = serde_json::to_writer_pretty(writer, &wallets_data) {
                error!("Error saving wallets file: {}", e);
            } else {
                info!("Wallets saved successfully.");
            }
        }
        Err(e) => {
            error!("Error creating wallets file for saving: {}", e);
        }
    }

    wallets.truncate(num_wallets);
    Ok(wallets)
}

// --- Balance & Transfer Helpers ---

pub(crate) async fn get_sol_balance(rpc_client: &RpcClient, pubkey: &Pubkey) -> Result<u64, SimulationError> {
    Ok(rpc_client.get_balance(pubkey).await?)
}

pub(crate) async fn get_token_balance(
    rpc_client: &RpcClient,
    owner_pubkey: &Pubkey,
    mint_pubkey: &Pubkey,
) -> Result<u64, SimulationError> {
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
                Err(SimulationError::SolanaClientError(e))
            }
        }
    }
}

// Helper to create and send a simple (legacy) transaction
// Note: Jito sending logic is removed for initial integration.
pub(crate) async fn create_and_send_tx(
    rpc_client: &RpcClient,
    instructions: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
) -> Result<Signature, SimulationError> {
    let recent_blockhash = rpc_client.get_latest_blockhash().await?;

    // Add priority fee instruction
    const PRIORITY_FEE_MICRO_LAMPORTS: u64 = 100_000; // Example fee
    debug!("Using priority fee: {} micro-lamports", PRIORITY_FEE_MICRO_LAMPORTS);
    let priority_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(PRIORITY_FEE_MICRO_LAMPORTS);

    let mut final_instructions = vec![priority_fee_ix];
    final_instructions.extend_from_slice(instructions);

    let mut tx = Transaction::new_with_payer(&final_instructions, Some(&payer.pubkey()));

    let mut all_signers = vec![payer];
    all_signers.extend_from_slice(signers);

    tx.sign(&all_signers, recent_blockhash);

    debug!(" -> Sending transaction via standard RPC...");
    match rpc_client.send_and_confirm_transaction_with_spinner(&tx).await {
        Ok(sig) => {
            info!(" -> Transaction confirmed via standard RPC. Signature: {}", sig);
            Ok(sig)
        }
        Err(e) => {
            error!(" -> Standard RPC send/confirm failed: {:?}", e);
            if let solana_client::client_error::ClientErrorKind::TransactionError(tx_err) = e.kind {
                error!("    -> On-chain execution failed!");
                Err(SimulationError::TransactionError(tx_err))
            } else {
                Err(SimulationError::SolanaClientError(e))
            }
        }
    }
}

// Transfer SOL between wallets
pub(crate) async fn transfer_sol(
    rpc_client: &RpcClient,
    from: &Keypair,
    to: &Pubkey,
    amount_lamports: u64,
) -> Result<Signature, SimulationError> {
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

// Transfer SPL Tokens between wallets, handles ATA creation funded by MINTER
pub(crate) async fn transfer_spl_tokens(
    rpc_client: &RpcClient,
    token_mint: &Pubkey,
    from_wallet: &Keypair,
    to_pubkey: &Pubkey,
    amount_tokens: u64,
    minter_keypair: &Keypair, // Minter pays for ATA creation if needed
) -> Result<Signature, SimulationError> {
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

    match rpc_client.get_account(&dest_ata).await {
        Ok(_) => {
            debug!("Destination ATA {} already exists.", dest_ata);
        }
        Err(e) => {
            if e.to_string().contains("AccountNotFound") || e.to_string().contains("could not find account") {
                debug!("Destination ATA {} does not exist.", dest_ata);
                ata_creation_needed = true;
            } else {
                error!("Error checking destination ATA {}: {}", dest_ata, e);
                return Err(SimulationError::SolanaClientError(e));
            }
        }
    }

    if ata_creation_needed {
        let rent_exempt_minimum_ata = rpc_client.get_minimum_balance_for_rent_exemption(TokenAccount::LEN).await?;
        info!("Rent-exempt minimum for ATA: {} lamports", rent_exempt_minimum_ata);

        // Check MINTER balance before attempting ATA creation tx
        const ATA_CREATION_FEE_BUFFER: u64 = 1_000_000; // Example buffer
        let minter_balance = get_sol_balance(rpc_client, &minter_keypair.pubkey()).await?;
        if minter_balance < rent_exempt_minimum_ata + ATA_CREATION_FEE_BUFFER {
            error!("MINTER has insufficient SOL ({}) to pay for ATA creation fee + rent (needs ~{}).", minter_balance, rent_exempt_minimum_ata + ATA_CREATION_FEE_BUFFER);
            return Err(SimulationError::ApiError("MINTER insufficient funds for ATA creation".to_string()));
        }

        info!("Creating destination ATA {} (paid by MINTER)...", dest_ata);
        let create_ata_ix = create_associated_token_account(
            &minter_keypair.pubkey(), // Payer is MINTER
            to_pubkey,                // Owner of the new ATA
            token_mint,
            &TOKEN_PROGRAM_ID,
        );
        // Send ATA creation transaction separately
        match create_and_send_tx(rpc_client, &[create_ata_ix], minter_keypair, &[]).await {
            Ok(sig) => {
                info!("ATA creation transaction successful: {}", sig);
                sleep(Duration::from_secs(5)).await; // Allow confirmation
            }
            Err(e) => {
                error!("Failed to create destination ATA {} paid by MINTER: {:?}", dest_ata, e);
                return Err(e);
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
            &[],          // Signer pubkeys
            amount_tokens,
        )?,
    );

    // Send the transfer transaction (signed by the sender)
    create_and_send_tx(rpc_client, &instructions, from_wallet, &[]).await
}

// --- Jupiter API Helpers ---

pub(crate) async fn get_jupiter_quote(
    http_client: &ReqwestClient,
    output_mint: &str,
    amount_lamports: u64,
    slippage_bps: u16,
) -> Result<QuoteResponse, SimulationError> {
    let params = [
        ("inputMint", original::SOL_MINT_ADDRESS),
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
        return Err(SimulationError::ApiError(format!(
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
) -> Result<QuoteResponse, SimulationError> {
    let params = [
        ("inputMint", input_mint),
        ("outputMint", original::SOL_MINT_ADDRESS),
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
        return Err(SimulationError::ApiError(format!(
            "Quote API failed with status {}: {}", status, text
        )));
    }
    Ok(response.json::<QuoteResponse>().await?)
}

pub(crate) async fn get_swap_transaction(
    http_client: &ReqwestClient,
    quote_response: &QuoteResponse,
    user_public_key: &Pubkey,
) -> Result<SwapResponse, SimulationError> {
    let payload = SwapRequestPayload {
        quote_response,
        user_public_key: user_public_key.to_string(),
        wrap_and_unwrap_sol: true,
        prioritization_fee_lamports: "auto".to_string(),
    };
    debug!("Fetching swap transaction...");
    let response = http_client
        .post(JUPITER_SWAP_API_URL)
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        error!("Swap API Error Response: Status {}, Body: {}", status, text);
        return Err(SimulationError::ApiError(format!(
            "Swap API failed with status {}: {}", status, text
        )));
    }
    Ok(response.json::<SwapResponse>().await?)
}

// Executes a Jupiter swap transaction (Versioned)
// Note: Jito sending logic removed for initial integration.
pub(crate) async fn execute_swap(
    rpc_client: &RpcClient,
    swap_tx_base64: &str,
    wallet_keypair: &Keypair,
) -> Result<Signature, SimulationError> {
    let swap_tx_bytes = BASE64_STANDARD.decode(swap_tx_base64)?;
    debug!("Transaction Decoded from Base64.");

    let mut versioned_tx: VersionedTransaction = bincode::deserialize(&swap_tx_bytes)?;
    debug!("Transaction Deserialized.");

    debug!("Fetching latest blockhash...");
    let (recent_blockhash, _last_valid_block_height) = rpc_client
        .get_latest_blockhash_with_commitment(rpc_client.commitment())
        .await?;
    debug!("Got latest blockhash: {}", recent_blockhash);
    versioned_tx.message.set_recent_blockhash(recent_blockhash);

    // Sign the transaction
    // Need to handle potential signing errors
    let signed_tx = VersionedTransaction::try_new(versioned_tx.message, &[wallet_keypair])
        .map_err(|e| SimulationError::ApiError(format!("Failed to sign transaction: {}", e)))?;
    debug!("Transaction Signed.");

    // Send via standard RPC
    info!("Sending swap transaction via standard RPC...");
    match rpc_client.send_and_confirm_transaction_with_spinner(&signed_tx).await {
        Ok(sig) => {
            info!(" -> Swap transaction confirmed via standard RPC. Signature: {}", sig);
            Ok(sig)
        }
        Err(e) => {
            error!(" -> Standard RPC send/confirm swap failed: {:?}", e);
            if let solana_client::client_error::ClientErrorKind::TransactionError(tx_err) = e.kind {
                error!("    -> On-chain execution failed!");
                Err(SimulationError::TransactionError(tx_err))
            } else {
                Err(SimulationError::SolanaClientError(e))
            }
        }
    }
}

// --- SimulatorV2 Implementation ---

pub struct SimulatorV2 {
    rpc_client: Arc<RpcClient>,
    http_client: Arc<ReqwestClient>,
    wallets: Vec<Wallet>,
    token_mint: Pubkey,
    slippage_bps: u16,
    log: Vec<LogEntry>,
    step_count: usize,
    wallets_file_path: String,
}

impl SimulatorV2 {
    pub fn new(
        rpc_client: Arc<RpcClient>,
        http_client: Arc<ReqwestClient>,
        wallets: Vec<Wallet>,
        token_mint: Pubkey,
        slippage_bps: u16,
        wallets_file_path: String,
    ) -> Self {
        if wallets.is_empty() || wallets[0].id != 0 {
            // Use panic for unrecoverable setup errors in `new`
            panic!("SimulatorV2 requires at least one wallet, and Wallet ID 0 must be the MINTER.");
        }
        Self {
            rpc_client,
            http_client,
            wallets,
            token_mint,
            slippage_bps,
            log: Vec::new(),
            step_count: 0,
            wallets_file_path,
        }
    }

    fn add_log(&mut self, event_type: &str, details: serde_json::Value) {
        let entry = LogEntry {
            timestamp: Utc::now().to_rfc3339(), // Add timestamp
            step: self.step_count,
            event_type: event_type.to_string(),
            details: details.clone(), // Clone details for logging
        };
        info!("Step {}: {} - {}", self.step_count, event_type, details); // Use info level
        self.log.push(entry);
    }

    // --- Phase 1: Initial Funding ---
    async fn initial_funding(&mut self) -> Result<Vec<usize>, SimulationError> {
        info!("--> Phase 1: Initial Funding <--");
        // let mut funded_indices: Vec<usize> = Vec::new(); // Unused
        const TOTAL_DISTRIBUTION_LAMPORTS: u64 = 50_000_000; // 0.05 SOL
        const NUM_WALLETS_TO_FUND: usize = 4;
        const MIN_REQUIRED_MINTER_BALANCE: u64 = 60_000_000; // Need slightly more than distribution + fees
        const FEE_BUFFER_PER_TRANSFER: u64 = 1_000_000;

        // Get Minter Wallet FIRST
        let minter_wallet = self.wallets.get(0).ok_or(SimulationError::NoSuitableWallet)?;
        let minter_keypair = &minter_wallet.keypair;
        let minter_pubkey = minter_wallet.pubkey();
        let minter_balance = get_sol_balance(&self.rpc_client, &minter_pubkey).await?;

        if minter_balance < MIN_REQUIRED_MINTER_BALANCE {
            error!("FATAL: Minter balance ({}) too low for initial funding (needs {}).", lamports_to_sol(minter_balance), lamports_to_sol(MIN_REQUIRED_MINTER_BALANCE));
            return Err(SimulationError::ConfigError("Minter insufficient funds for V2 initial funding".to_string()));
        }

        // RNG creation moved down
        let potential_recipient_indices: Vec<usize> = (1..self.wallets.len()).collect();
        if potential_recipient_indices.len() < NUM_WALLETS_TO_FUND {
            error!("FATAL: Not enough non-dev wallets ({}) to fund the required {}.", potential_recipient_indices.len(), NUM_WALLETS_TO_FUND);
            return Err(SimulationError::ConfigError("Not enough wallets for V2 funding".to_string()));
        }
        // Create thread-safe RNG just before use
        let mut rng = StdRng::from_entropy();
        let funded_indices: Vec<usize> = potential_recipient_indices
            .choose_multiple(&mut rng, NUM_WALLETS_TO_FUND)
            .cloned()
            .collect();

        let amount_per_wallet = TOTAL_DISTRIBUTION_LAMPORTS / NUM_WALLETS_TO_FUND as u64;
        let total_needed = TOTAL_DISTRIBUTION_LAMPORTS + (NUM_WALLETS_TO_FUND as u64 * FEE_BUFFER_PER_TRANSFER);
        if minter_balance < total_needed {
            warn!("Minter balance ({}) might be insufficient for {} funding transfers needing ~{} SOL total.", lamports_to_sol(minter_balance), funded_indices.len(), lamports_to_sol(total_needed));
        }

        info!("Distributing {} SOL total to {} randomly selected wallets ({} SOL each)...",
                 lamports_to_sol(TOTAL_DISTRIBUTION_LAMPORTS), funded_indices.len(), lamports_to_sol(amount_per_wallet));

        for &index in &funded_indices {
            if let Some(recipient_wallet) = self.wallets.get(index) {
                let recipient_pubkey: Pubkey = recipient_wallet.pubkey();
                info!("Funding Wallet {} ({}) with {} SOL...", index, recipient_pubkey, lamports_to_sol(amount_per_wallet));
                if let Err(e) = transfer_sol(&self.rpc_client, minter_keypair, &recipient_pubkey, amount_per_wallet).await {
                    error!(" -> Funding FAILED for Wallet {}: {:?}", index, e);
                } else {
                    info!(" -> Funding successful for Wallet {}", index);
                    sleep(Duration::from_millis(200)).await;
                }
            } else {
                warn!("Could not find wallet with ID {} during initial funding.", index);
            }
        }
        info!("--> Phase 1 Complete <--");
        Ok(funded_indices)
    }

    // --- Phase 2: Buying ---
    async fn buying_phase(&mut self, funded_indices: &[usize]) -> Result<HashSet<usize>, SimulationError> {
        info!("--> Phase 2: Buying <--");
        let mut successful_buyer_ids = HashSet::new();
        let mut rng = StdRng::from_entropy();

        if funded_indices.is_empty() {
            info!("No wallets were funded in Phase 1. Skipping buying phase.");
            return Ok(successful_buyer_ids);
        }
        info!("Attempting buys from {} funded wallets...", funded_indices.len());

        for &wallet_id in funded_indices {
            let buyer_wallet = match self.wallets.get(wallet_id) {
                Some(w) => w,
                None => {
                    warn!("Buying Phase: Wallet ID {} not found. Skipping.", wallet_id);
                    continue;
                }
            };
            let buyer_pubkey = buyer_wallet.pubkey();
            let buyer_keypair = &buyer_wallet.keypair;
            info!("Processing Buy for Wallet {}: {}", wallet_id, buyer_pubkey);

            let current_sol_buyer = match get_sol_balance(&self.rpc_client, &buyer_pubkey).await {
                Ok(bal) => bal,
                Err(e) => {
                    error!(" -> Failed to get SOL balance: {:?}. Skipping buy.", e);
                    continue;
                }
            };
            let available_for_buy = current_sol_buyer.saturating_sub(FEE_BUFFER_SWAP_V2);

            if available_for_buy < BUY_MIN_LAMPORTS_V2 {
                info!(
                    " -> Wallet {} has insufficient SOL ({}) for minimum buy (needs {} + {} fee). Skipping.",
                    wallet_id, lamports_to_sol(current_sol_buyer),
                    lamports_to_sol(BUY_MIN_LAMPORTS_V2), lamports_to_sol(FEE_BUFFER_SWAP_V2)
                );
                continue;
            }

            let amount_to_buy_lamports = rng.gen_range(BUY_MIN_LAMPORTS_V2..=available_for_buy);
            info!(" -> Attempting to buy with {} SOL", lamports_to_sol(amount_to_buy_lamports));

            match get_jupiter_quote(&self.http_client, &self.token_mint.to_string(), amount_to_buy_lamports, self.slippage_bps).await {
                Ok(quote) => {
                    let estimated_tokens_str = quote.out_amount.clone();
                    info!("    -> Quote received: {} SOL for ~{} tokens", lamports_to_sol(amount_to_buy_lamports), estimated_tokens_str);
                    match get_swap_transaction(&self.http_client, &quote, &buyer_pubkey).await {
                        Ok(swap_resp) => {
                            match execute_swap(&self.rpc_client, &swap_resp.swap_transaction, buyer_keypair).await {
                                Ok(sig) => {
                                    info!("    -> Wallet {} buy successful: {}", wallet_id, sig);
                                    successful_buyer_ids.insert(wallet_id);
                                    self.add_log("buy_v2", json!({ "wallet_id": wallet_id, "sol_spent": amount_to_buy_lamports, "tokens_quoted": estimated_tokens_str, "signature": sig.to_string() }));
                                }
                                Err(e) => {
                                    error!("    -> Wallet {} buy execution FAILED: {:?}", wallet_id, e);
                                    self.add_log("buy_v2_fail", json!({ "wallet_id": wallet_id, "sol_spent": amount_to_buy_lamports, "error": e.to_string() }));
                                }
                            }
                        }
                        Err(e) => {
                            error!("    -> Wallet {} get_swap_transaction FAILED: {:?}", wallet_id, e);
                            self.add_log("buy_v2_fail", json!({ "wallet_id": wallet_id, "sol_spent": amount_to_buy_lamports, "error": format!("get_swap_tx failed: {}", e) }));
                        }
                    }
                }
                Err(e) => {
                    error!("    -> Wallet {} get_jupiter_quote FAILED: {:?}", wallet_id, e);
                    self.add_log("buy_v2_fail", json!({ "wallet_id": wallet_id, "sol_spent": amount_to_buy_lamports, "error": format!("get_quote failed: {}", e) }));
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
        info!("--> Phase 2 Complete ({} successful buys) <--", successful_buyer_ids.len());
        Ok(successful_buyer_ids)
    }

    // --- Phase 3: Consolidate Tokens ---
    async fn consolidation_phase(&mut self, buyer_ids: &HashSet<usize>) -> Result<Option<usize>, SimulationError> {
        info!("--> Phase 3: Consolidate Tokens <--");
        if buyer_ids.is_empty() {
            info!("No successful buyers in Phase 2. Skipping token consolidation.");
            return Ok(None);
        }

        let potential_seller_indices: Vec<usize> = (1..self.wallets.len())
            .filter(|id| !buyer_ids.contains(id))
            .collect();
        if potential_seller_indices.is_empty() {
            info!("No potential wallets available to act as seller. Skipping consolidation.");
            return Ok(None);
        }

        let mut rng = StdRng::from_entropy();
        let chosen_seller_index = *potential_seller_indices.choose(&mut rng).unwrap();
        let seller_wallet = match self.wallets.get(chosen_seller_index) {
            Some(w) => w,
            None => {
                warn!("Consolidation Phase: Chosen seller ID {} not found. Skipping.", chosen_seller_index);
                return Ok(None);
            }
        };
        let seller_pubkey = seller_wallet.pubkey();
        info!("Chosen Seller Wallet: {} ({})", chosen_seller_index, seller_pubkey);

        info!("Transferring tokens from {} buyers to seller {}...", buyer_ids.len(), chosen_seller_index);
        // let minter_keypair_ref = &self.wallets[0].keypair; // Unused

        for &buyer_id in buyer_ids {
            let buyer_wallet = match self.wallets.get(buyer_id) {
                Some(w) => w,
                None => {
                    warn!("Consolidation: Buyer ID {} not found. Skipping transfer.", buyer_id);
                    continue;
                }
            };
            let buyer_pubkey = buyer_wallet.pubkey();
            let buyer_keypair = &buyer_wallet.keypair; // Keep this borrow short
            info!("Processing transfer from Buyer {}: {}", buyer_id, buyer_pubkey);

            // Clone/copy data needed for transfer *before* the call
            let rpc_client_clone = Arc::clone(&self.rpc_client);
            let token_mint_clone = self.token_mint; // Pubkey is Copy
            let seller_pubkey_clone = seller_pubkey; // Pubkey is Copy
            // Recreate minter keypair inside the loop when needed
            let minter_kp_b58 = self.wallets[0].keypair.to_base58_string();
            
            // First decode the base58 string to bytes
            let minter_keypair_bytes = match bs58::decode(&minter_kp_b58).into_vec() {
                Ok(bytes) => bytes,
                Err(e) => {
                    // Return error from the outer function if Err
                    return Err(SimulationError::Base58DecodeError(e));
                }
            };
            
            // Then create the keypair from the bytes
            let minter_keypair_ref_for_transfer = match Keypair::from_bytes(&minter_keypair_bytes) {
                Ok(kp) => kp,
                Err(e) => {
                    // Return error from the outer function if Err
                    // Convert the std::error::Error to a string to avoid type mismatch
                    return Err(SimulationError::ConfigError(format!("Invalid private key: {}", e)));
                }
            };
            // If we reach here, minter_keypair_ref_for_transfer is assigned

            let token_balance = match get_token_balance(&rpc_client_clone, &buyer_pubkey, &token_mint_clone).await { // Use clones
                Ok(bal) => bal,
                Err(e) => {
                    error!(" -> Failed to get token balance for buyer {}: {:?}. Skipping transfer.", buyer_id, e);
                    continue;
                }
            };
            if token_balance == 0 {
                info!(" -> Buyer {} has 0 tokens. Skipping transfer.", buyer_id);
                continue;
            }

            let buyer_sol_balance = match get_sol_balance(&self.rpc_client, &buyer_pubkey).await {
                Ok(bal) => bal,
                Err(e) => {
                    error!(" -> Failed to get SOL balance for buyer {}: {:?}. Skipping transfer.", buyer_id, e);
                    continue;
                }
            };
            if buyer_sol_balance < CONSOLIDATION_FEE_ESTIMATE_V2 {
                // info!(" -> Buyer {} has insufficient SOL ({}) for token transfer fee (needs {}). Skipping.", // Commented out for borrow check
                //          buyer_id, lamports_to_sol(buyer_sol_balance), lamports_to_sol(CONSOLIDATION_FEE_ESTIMATE_V2));
                warn!(" -> Buyer {} has insufficient SOL ({}) for token transfer fee (needs {}). Skipping.", // Use warn instead
                         buyer_id, lamports_to_sol(buyer_sol_balance), lamports_to_sol(CONSOLIDATION_FEE_ESTIMATE_V2));
                continue;
            }

            // info!(" -> Transferring {} tokens from buyer {} to seller {}", token_balance, buyer_id, chosen_seller_index); // Commented out for borrow check
            debug!(" -> Preparing to transfer {} tokens from buyer {} to seller {}", token_balance, buyer_id, chosen_seller_index); // Use debug instead
            // Call transfer_spl_tokens using the clones/copies
            let transfer_result = transfer_spl_tokens(
                &rpc_client_clone,
                &token_mint_clone,
                buyer_keypair, // This borrow ends when transfer_spl_tokens returns
                &seller_pubkey_clone,
                token_balance,
                &minter_keypair_ref_for_transfer // Use recreated keypair ref
            ).await;

            // Now handle the result and log *after* the await and transfer_spl_tokens borrows are finished
            match transfer_result {
                 Ok(sig) => {
                     info!("    -> Token transfer successful: {}", sig);
                     // Log after await, self is mutably borrowed here
                     self.add_log("consolidate_v2", json!({ "from_id": buyer_id, "to_id": chosen_seller_index, "token_amount": token_balance, "signature": sig.to_string() }));
                 }
                 Err(e) => {
                     error!("    -> Token transfer FAILED for buyer {}: {:?}", buyer_id, e);
                     // Log after await, self is mutably borrowed here
                     self.add_log("consolidate_v2_fail", json!({ "from_id": buyer_id, "to_id": chosen_seller_index, "token_amount": token_balance, "error": e.to_string() }));
                 }
             }

            sleep(Duration::from_millis(500)).await;
        } // End of loop iteration, all local borrows drop here
        info!("--> Phase 3 Complete <--");
        Ok(Some(chosen_seller_index))
    }

    // --- Phase 4: Selling ---
    async fn selling_phase(&mut self, seller_id_option: Option<usize>) -> Result<(), SimulationError> {
        info!("--> Phase 4: Selling <--");
        let seller_id = match seller_id_option {
            Some(id) => id,
            None => {
                info!("No seller designated from Phase 3. Skipping selling phase.");
                return Ok(());
            }
        };

        let seller_wallet = match self.wallets.get(seller_id) {
            Some(w) => w,
            None => {
                warn!("Selling Phase: Seller ID {} not found. Skipping.", seller_id);
                return Ok(());
            }
        };
        let seller_pubkey = seller_wallet.pubkey();
        let seller_keypair = &seller_wallet.keypair;
        info!("Processing Sell for Wallet {}: {}", seller_id, seller_pubkey);

        let tokens_to_sell = match get_token_balance(&self.rpc_client, &seller_pubkey, &self.token_mint).await {
            Ok(bal) => bal,
            Err(e) => {
                error!(" -> Failed to get token balance for seller {}: {:?}. Skipping sell.", seller_id, e);
                return Ok(());
            }
        };
        if tokens_to_sell == 0 {
            info!(" -> Seller {} has 0 tokens. Skipping sell.", seller_id);
            return Ok(());
        }
        info!(" -> Seller {} attempting to sell {} tokens", seller_id, tokens_to_sell);

        let seller_sol_balance = match get_sol_balance(&self.rpc_client, &seller_pubkey).await {
            Ok(bal) => bal,
            Err(e) => {
                error!(" -> Failed to get SOL balance for seller {}: {:?}. Skipping sell.", seller_id, e);
                return Ok(());
            }
        };
        if seller_sol_balance < FEE_BUFFER_SWAP_V2 {
            info!(" -> Seller {} has insufficient SOL ({}) for swap fee (needs {}). Skipping.",
                     seller_id, lamports_to_sol(seller_sol_balance), lamports_to_sol(FEE_BUFFER_SWAP_V2));
            return Ok(());
        }

        // --- Pre-create WSOL ATA if needed ---
        // (Simplified: Assume Jupiter handles WSOL wrapping/unwrapping)

        // --- Perform Swap (Token -> SOL) ---
        match get_jupiter_quote_token_to_sol(&self.http_client, &self.token_mint.to_string(), tokens_to_sell, self.slippage_bps).await {
            Ok(quote) => {
                let estimated_sol_str = quote.out_amount.clone();
                let parsed_sol_estimate = estimated_sol_str.parse::<u64>().unwrap_or(0);
                info!("    -> Quote received: {} tokens for ~{} SOL", tokens_to_sell, lamports_to_sol(parsed_sol_estimate));

                match get_swap_transaction(&self.http_client, &quote, &seller_pubkey).await {
                    Ok(swap_resp) => {
                        match execute_swap(&self.rpc_client, &swap_resp.swap_transaction, seller_keypair).await {
                            Ok(sig) => {
                                info!("    -> Wallet {} sell successful: {}", seller_id, sig);
                                self.add_log("sell_v2", json!({ "wallet_id": seller_id, "tokens_sold": tokens_to_sell, "sol_quoted": parsed_sol_estimate, "signature": sig.to_string() }));
                            }
                            Err(e) => {
                                error!("    -> Wallet {} sell execution FAILED: {:?}", seller_id, e);
                                self.add_log("sell_v2_fail", json!({ "wallet_id": seller_id, "tokens_sold": tokens_to_sell, "error": e.to_string() }));
                            }
                        }
                    }
                    Err(e) => {
                        error!("    -> Wallet {} get_swap_transaction FAILED: {:?}", seller_id, e);
                        self.add_log("sell_v2_fail", json!({ "wallet_id": seller_id, "tokens_sold": tokens_to_sell, "error": format!("get_swap_tx failed: {}", e) }));
                    }
                }
            }
            Err(SimulationError::ApiError(msg)) if msg.contains("Could not find any route") || msg.contains("COULD_NOT_FIND_ANY_ROUTE") => {
                info!(" -> Skipping SELL for Wallet {}: Jupiter could not find a route for {} tokens.", seller_id, tokens_to_sell);
                self.add_log("sell_v2_skip", json!({ "wallet_id": seller_id, "reason": "Jupiter route not found", "token_amount": tokens_to_sell }));
            }
            Err(e) => {
                error!("    -> Wallet {} get_jupiter_quote FAILED: {:?}", seller_id, e);
                self.add_log("sell_v2_fail", json!({ "wallet_id": seller_id, "tokens_sold": tokens_to_sell, "error": format!("get_quote failed: {}", e) }));
                // Don't propagate quote errors, just log and finish phase
            }
        }
        info!("--> Phase 4 Complete <--");
        Ok(())
    }

    // --- Run Simulation V2 Loop ---
    pub async fn run(&mut self, max_cycles: usize) -> Result<(), SimulationError> {
        info!("Starting Simulation V2 for {} cycles...", max_cycles);
        for cycle in 1..=max_cycles {
            self.step_count = cycle; // Use cycle number as step count for logging
            info!("\n===== Cycle {} / {} =====", cycle, max_cycles);

            // Phase 1: Funding
            let funded_indices = match self.initial_funding().await {
                Ok(indices) => indices,
                Err(e) => {
                    error!("Cycle {} failed during initial funding: {:?}. Stopping simulation.", cycle, e);
                    self.save_log()?; // Attempt save before exiting
                    return Err(e);
                }
            };
            sleep(Duration::from_secs(1)).await; // Delay between phases

            // Phase 2: Buying
            let successful_buyers = match self.buying_phase(&funded_indices).await {
                Ok(ids) => ids,
                Err(e) => {
                    error!("Cycle {} failed during buying phase: {:?}. Stopping simulation.", cycle, e);
                    self.save_log()?;
                    return Err(e);
                }
            };
            sleep(Duration::from_secs(1)).await;

            // Phase 3: Consolidation
            let seller_id_option = match self.consolidation_phase(&successful_buyers).await {
                Ok(id_opt) => id_opt,
                Err(e) => {
                    error!("Cycle {} failed during consolidation phase: {:?}. Stopping simulation.", cycle, e);
                    self.save_log()?;
                    return Err(e);
                }
            };
            sleep(Duration::from_secs(1)).await;

            // Phase 4: Selling
            if let Err(e) = self.selling_phase(seller_id_option).await {
                error!("Cycle {} failed during selling phase: {:?}. Stopping simulation.", cycle, e);
                self.save_log()?;
                return Err(e);
            }
            sleep(Duration::from_secs(5)).await; // Longer delay after a full cycle
        }

        info!("Simulation V2 completed {} cycles.", max_cycles);
        self.save_log()?; // Save log at the end
        Ok(())
    }

    // --- Save Log ---
    fn save_log(&self) -> Result<(), SimulationError> {
        let wallets_path = Path::new(&self.wallets_file_path);
        let log_file_name = wallets_path.file_stem()
            .map(|stem| format!("{}_simulation_log.json", stem.to_string_lossy()))
            .unwrap_or_else(|| "simulation_log.json".to_string());

        let log_path = if let Some(parent) = wallets_path.parent() {
            parent.join(&log_file_name)
        } else {
            Path::new(&log_file_name).to_path_buf()
        };

        info!("Saving simulation log to {}...", log_path.display());
        // Use OpenOptions to create or truncate the file
        match OpenOptions::new().write(true).create(true).truncate(true).open(&log_path) {
            Ok(file) => {
                let writer = BufWriter::new(file);
                if let Err(e) = serde_json::to_writer_pretty(writer, &self.log) {
                    error!("Error writing simulation log: {}", e);
                    Err(SimulationError::JsonError(e)) // Return specific error
                } else {
                    info!("Log saved successfully.");
                    Ok(())
                }
            }
            Err(e) => {
                error!("Error creating/opening log file '{}': {}", log_path.display(), e);
                Err(SimulationError::IoError(e)) // Return specific error
            }
        }
    }
}

// --- Public Runner Function ---

/// Runs the V2 simulation.
pub async fn run_simulation_v2(
    // Accept clients as arguments
    rpc_client: Arc<RpcClient>, // Changed from rpc_url
    http_client: Arc<ReqwestClient>, // Added http_client
    wallets_file: String,
    token_mint_str: String,
    max_cycles: usize,
    slippage_bps: u16,
    // Add other config params as needed
) -> Result<(), anyhow::Error> { // Change return type to anyhow::Error
    info!("Initializing Simulation V2...");

    // Use anyhow context for better error messages
    let token_mint = Pubkey::from_str(&token_mint_str)
        .context(format!("Invalid CA: {}", token_mint_str))?;

    // Load wallets (use a reasonable number, e.g., 20, or make it configurable)
    const NUM_SIM_WALLETS: usize = 20;
    // Map SimulationError to anyhow::Error
    let wallets = load_or_generate_wallets(&wallets_file, NUM_SIM_WALLETS)
        .map_err(anyhow::Error::from)?;

    // Setup clients - REMOVED as they are now passed as arguments
    // let http_client = Arc::new(ReqwestClient::new());
    // let rpc_client = Arc::new(RpcClient::new_with_commitment(
    //     rpc_url.clone(), // This line caused the error
    //     CommitmentConfig::confirmed(),
    // ));

    // Clients are now passed in, no need to create them here or check connection again

    // Initialize and run simulator
    let mut simulator = SimulatorV2::new(
        rpc_client,
        http_client,
        wallets,
        token_mint,
        slippage_bps,
        wallets_file, // Pass the path for log saving
    );

    match simulator.run(max_cycles).await {
        Ok(_) => info!("Simulation V2 finished successfully."),
        Err(e) => {
            error!("Simulation V2 failed: {}", e);
            // Attempt to save log even on failure
            if let Err(log_err) = simulator.save_log() { // save_log returns SimulationError
                error!("Failed to save log after simulation error: {}", log_err);
            }
            // Propagate the original simulation error, converting it to anyhow::Error
            return Err(anyhow::Error::from(e));
        }
    }

    Ok(()) // Return Ok(()) on successful completion
}