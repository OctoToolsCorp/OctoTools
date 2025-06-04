use std::sync::atomic::{AtomicBool, Ordering}; // Test comment
use eframe::egui::{self, Color32, Margin, Rounding, Stroke, Visuals};
use egui_plot::{Line, Plot, PlotPoints};
// Removed unused Label, RichText
use tokio::sync::mpsc::{self, UnboundedSender, UnboundedReceiver};
use crate::commands::pump::PumpStatus;
// Removed unused TokenAccountsFilter
use solana_client::{
    rpc_client::RpcClient, // Keep RpcClient for gather task
    // Removed unused TokenAccountsFilter
    nonblocking::rpc_client::RpcClient as AsyncRpcClient,
};
// use solana_client::rpc_request::TokenAccountsFilter; // Unused
// use solana_account_decoder::{UiAccountData, UiAccountEncoding}; // Unused
use solana_sdk::{
    address_lookup_table::{/*state::AddressLookupTable, AddressLookupTableAccount,*/ instruction as alt_instruction},
    commitment_config::{CommitmentConfig},
    compute_budget::{self},
    // hash::Hash, // Unused
    message::{v0::Message as MessageV0, VersionedMessage},
    native_token::{lamports_to_sol, sol_to_lamports},
    // program_pack::Pack, // Unused
    pubkey::Pubkey,
    signature::{Keypair, Signer, Signature},
    system_instruction,
    system_program,
    sysvar,
    transaction::{Transaction, VersionedTransaction},
};
// use spl_token::instruction::close_account as spl_close_account; // Unused
// use spl_token::state::{Account as TokenAccount}; // Unused
use std::{collections::{HashSet}, sync::Arc, str::FromStr, path::Path, time::Duration, io::{Write, BufWriter}, fs::File}; // Added BufWriter, fs::File
// Removed unused StdError
use anyhow::{anyhow, Context, Result};
use reqwest::Client as ReqwestClient; // Now used for Jupiter
use bs58;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use bincode;
use crate::models::wallet::WalletInfo as LoadedWalletInfo;
use futures::stream::{StreamExt, iter};
use tokio::time::sleep;
use log::{info, error, warn};
use chrono::Utc; // For timestamp in filename
// use log::debug; // Unused
// use borsh::BorshDeserialize; // Unused
// use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _}; // Unused
use spl_associated_token_account::{
    get_associated_token_address,
    // instruction::create_associated_token_account, // Unused
};
use spl_token;
use serde_json::{Value as JsonValue}; // Added json and Value for Jupiter
// use solana_transaction_status; // Unused
use rfd::FileDialog; // <-- Add file dialog import
use rand::Rng; // For random buy/sell decisions and other RNG tasks
use rand::seq::SliceRandom; // For randomly selecting from a slice
use rand::SeedableRng;
// --- Jupiter API Constants and Structs (Simplified for Volume Bot) ---
const JUPITER_QUOTE_API_URL_V6: &str = "https://quote-api.jup.ag/v6/quote";
const JUPITER_SWAP_API_URL_V6: &str = "https://quote-api.jup.ag/v6/swap";
const SOL_MINT_ADDRESS_STR: &str = "So11111111111111111111111111111111111111112";
const ATA_RENT_LAMPORTS: u64 = 2_039_280; // Standard rent for an ATA in lamports, matches src/commands/buy.rs
// Constants for sell fee transfer
const FEE_RECIPIENT_ADDRESS_FOR_SELL_STR: &str = "5UTHcSCMRdyphViB3obHTVWhQnViuFHRTc1hAy8pS173";
// const FEE_AMOUNT_LAMPORTS_FOR_SELL: u64 = 100_000_000; // 0.1 SOL - This will be calculated dynamically

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct JupiterQuoteResponse {
    out_amount: String,
    #[serde(flatten)]
    other_fields: JsonValue, // Catch all other fields
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct JupiterSwapResponse {
    swap_transaction: String, // base64 encoded transaction
}
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

    let status_code = response.status(); // Get status code ONCE
    if !status_code.is_success() {       // Use the stored status code
        // Now it's safe to consume response as we're in an error path
        let error_text = response.text().await.unwrap_or_else(|_| "Unknown API error".to_string());
        return Err(anyhow!("Jupiter quote API request failed with status {}: {}", status_code, error_text));
    }

    response
        .json::<JupiterQuoteResponse>()
        .await
        .context("Failed to parse Jupiter quote API response")
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JupiterSwapRequestPayload<'a> {
    quote_response: &'a JupiterQuoteResponse,
    user_public_key: String,
    wrap_and_unwrap_sol: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prioritization_fee_lamports: Option<u64>,
    // as_legacy_transaction: Option<bool>, // Example for future if needed
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
        wrap_and_unwrap_sol: true, // Typically true for SOL pairs
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
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown API error while getting swap transaction".to_string());
        return Err(anyhow!(
            "Jupiter swap API request failed with status {}: {}",
            status_code,
            error_text
        ));
    }

    response
        .json::<JupiterSwapResponse>()
        .await
        .context("Failed to parse Jupiter swap API response")
}
// --- End Jupiter API Structs ---

// --- Jito sendTransaction Structs and Helper ---
#[derive(serde::Serialize, Debug)]
struct JitoSendTransactionParamsConfig {
    encoding: String,
    #[serde(rename = "skipPreflight")]
    skip_preflight: bool,
    // #[serde(skip_serializing_if = "Option::is_none")]
    // bundle_only: Option<bool>, // Not using for individual sends as per current requirement
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
    result: Option<String>, // Transaction signature
    error: Option<JsonValue>, // Catch potential Jito error object
    id: u64,
}

async fn send_transaction_via_jito_raw(
    http_client: &ReqwestClient,
    jito_block_engine_base_url: &str, // e.g., "https://frankfurt.mainnet.block-engine.jito.wtf"
    base64_encoded_tx: String,
    status_sender: &UnboundedSender<Result<String, String>>, // For logging/status updates
) -> Result<String, anyhow::Error> {
    let jito_send_tx_url = format!("{}/api/v1/transactions", jito_block_engine_base_url.trim_end_matches('/'));
    
    let request_payload = JitoSendTransactionRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "sendTransaction",
        params: (
            base64_encoded_tx.clone(), // Send the raw base64 string
            JitoSendTransactionParamsConfig {
                encoding: "base64".to_string(),
                skip_preflight: true, // Jito docs state this is always true for their endpoint
            },
        ),
    };

    info!("Sending transaction to Jito: URL: {}, Payload: {:?}", jito_send_tx_url, serde_json::to_string(&request_payload).unwrap_or_default());
    let _ = status_sender.send(Ok(format!("Sending to Jito: {}...", jito_send_tx_url)));

    let response = http_client
        .post(&jito_send_tx_url)
        .json(&request_payload)
        .send()
        .await
        .with_context(|| format!("Failed to send Jito sendTransaction request to {}", jito_send_tx_url))?;

    let response_status = response.status();
    let response_text = response.text().await.unwrap_or_else(|_| "Unknown Jito API error text".to_string());
    info!("Jito sendTransaction response status: {}, body: {}", response_status, response_text);


    if !response_status.is_success() {
        let err_msg = format!(
            "Jito sendTransaction API request failed with status {}: {}",
            response_status, response_text
        );
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
// --- End Jito sendTransaction Helper ---

use crate::models::{LaunchStatus, AltCreationStatus}; // <-- Import moved types HERE
// --- Generate Volume Bot Wallets Task (Free Function) ---
async fn generate_volume_bot_wallets_task(
    num_wallets: usize,
    status_sender: UnboundedSender<Result<String, String>>, // For overall status
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
        let keypair = Keypair::new(); // Keypair::new() is infallible
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
                    // Send the generated wallets and success status including filename
                    if wallets_sender.send(generated_wallets).is_err() {
                        log::error!("Failed to send generated wallets (after saving) from task to UI.");
                        // Attempt to send error status, but primary success is file save
                        let _ = status_sender.send(Err("Failed to send wallets to UI, but file saved.".to_string()));
                         return; // Return as we can't update UI state properly
                    }
                    if status_sender.send(Ok(format!("Successfully generated {} wallets and saved to {}", num_wallets, filename))).is_err() {
                        log::error!("Failed to send success status (file saved) from wallet gen task to UI.");
                    }
                }
                Err(e) => {
                    let err_msg = format!("Failed to serialize and save wallets to {}: {}", filename, e);
                    log::error!("{}", err_msg);
                    // Still try to send the in-memory wallets if serialization failed, but report file error
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
            // Still try to send the in-memory wallets if file creation failed
            if wallets_sender.send(generated_wallets).is_err() {
                log::error!("Failed to send generated wallets (file create failed) from task to UI.");
            }
            let _ = status_sender.send(Err(err_msg));
        }
    }
}

// Crate-specific imports
use crate::pump_instruction_builders::{
    // build_pump_create_instruction, build_pump_buy_instruction, build_pump_sell_instruction, // Unused
    find_bonding_curve_pda, find_metadata_pda, // GlobalState, // Unused
    // calculate_tokens_out, calculate_sol_out, // Unused
    // apply_slippage_to_tokens_out, apply_slippage_to_sol_cost, apply_slippage_to_sol_out, // Unused
    GLOBAL_STATE_PUBKEY, // BondingCurveAccount, // Unused
    PUMPFUN_PROGRAM_ID, METADATA_PROGRAM_ID,
    PUMPFUN_MINT_AUTHORITY, FEE_RECIPIENT_PUBKEY, EVENT_AUTHORITY_PUBKEY,
    // predict_next_curve_state, // Unused
};
// use crate::api::upload_metadata_to_ipfs; // Unused
// use crate::models::token::PumpfunMetadata; // Unused
// Import the helper function

// Import the new settings module
use crate::models::settings::AppSettings; // <-- Correct import path
use crate::key_utils; // <-- Add import for key generation

// -- Constants --
const MAX_ADDRESSES_PER_EXTEND: usize = 20;
// Constants from launch_buy.rs (adjust as needed)
// const ATA_CREATE_COMPUTE_LIMIT: u32 = 50_000; // Moved to launch_task
// const TIP_TX_COMPUTE_LIMIT: u32 = 10_000; // Moved to launch_task
const ZOMBIE_CHUNK_COMPUTE_LIMIT: u32 = 1_400_000; // Max CU per tx
const MAX_ZOMBIES_PER_CHUNK: usize = 6; // Match launch_buy.rs

// Struct to hold display information for a wallet
#[derive(Clone, Debug)]
struct WalletInfo {
    address: String,
    is_primary: bool, // Is it the first wallet from keys.json?
    is_dev_wallet: bool,  // Renamed from is_minter
    is_parent: bool,  // Is it the parent wallet from settings?
    sol_balance: Option<f64>,
    target_mint_balance: Option<String>, // Balance of the specific mint we are checking
    error: Option<String>,
    is_loading: bool,
    is_selected: bool,          // For selection in Disperse/Gather views
    sell_percentage_input: String, // Input field for sell percentage per wallet
    sell_amount_input: String,     // Input field for sell amount per wallet
    is_selling: bool,              // Loading flag for individual sell action
    atomic_buy_sol_amount_input: String, // Input for SOL amount in Atomic Buy view
}

// Result type for balance fetching tasks
#[derive(Debug)]
enum FetchResult {
    Success {
        address: String,
        sol_balance: f64,
        target_mint_balance: Option<String>, // Balance of the specific mint we checked
    },
    Failure {
        address: String,
        error: String,
    },
}

// FetchResult impl
impl FetchResult {
    fn address(&self) -> &str {
        match self {
            FetchResult::Success { address, .. } => address,
            FetchResult::Failure { address, .. } => address,
        }
    }
}

// Struct to hold balance info for the live monitor display
// Moved here to be accessible by PumpFunApp struct definition
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

// Enum to represent the different views/commands
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum AppView {
    // Buy, // Removed Buy view
    Sell,
    Disperse,
    Gather,
    Launch,
    AtomicBuy,
    Check,
    Alt,
    Settings,
    VolumeBot, // Renamed from Simulation, was "simulation v2"
}

// Define Result types for the new tasks
#[derive(Debug)] // <-- Add Debug derive
enum PrecalcResult {
    Success(String, Vec<Pubkey>), // Token CA (Mint Pubkey) String, Addresses
    Failure(String),
}

#[derive(Debug)] // <-- Add Debug derive
enum AltCreationResult { // Keep this for now, might remove later if fully replaced
    Success(Pubkey), // ALT Address
    Failure(String),
}

// AltCreationStatus moved to src/models/mod.rs
// LaunchStatus moved to src/models/mod.rs

#[derive(Debug, Clone)]
enum SellStatus {
    Idle,
    InProgress(String), // Message indicating current action (e.g., "Selling from wallet X", "Gathering tokens", "Mass selling")
    WalletSuccess(String, String), // Wallet Address, Success Message (e.g., signature)
    WalletFailure(String, String), // Wallet Address, Error Message
    MassSellComplete(String), // Final success/summary message
    GatherAndSellComplete(String), // Final success/summary message
    Failure(String), // General error message
}



// --- Helper struct for Launch Params ---
#[derive(Clone, Debug)]
pub struct LaunchParams {
    name: String,
    symbol: String,
    description: String,
    image_url: String,
    dev_buy_sol: f64,
    zombie_buy_sol: f64,
    // Removed slippage_bps (using env var)
    // Removed priority_fee_microlamports (using env var)
minter_private_key_str: String, // Added to pass minter key from settings
slippage_bps: u64, // Re-added: Needed by launch_buy
    priority_fee: u64, // Re-added: Needed by launch_buy (lamports)
    alt_address_str: String,
    mint_keypair_path_str: String, // Keep for potential pre-selected minter? Or remove?
    // Let's keep for now, but logic will prioritize first wallet if path is empty.
    loaded_wallet_data: Vec<LoadedWalletInfo>,
    // parent_pk_str_env_var: String, // <-- Remove this field
    // Removed num_zombie_buys as it's no longer used
    use_jito_bundle: bool, // <-- Add this field
    // Removed jito_tip_account (using env var)
    jito_block_engine_url: String, // <-- Add this field
    twitter: String,        // <-- Add Twitter URL
    telegram: String,       // <-- Add Telegram URL
    website: String,        // <-- Add Website URL
    simulate_only: bool,    // <-- Add Simulate Only flag
    main_tx_priority_fee_micro_lamports: u64, // <-- Added
    jito_actual_tip_sol: f64,                 // <-- Added
}

#[derive(Clone)]
struct VolumeBotStartParams {
    rpc_url: String,
    wallets_file_path: String,
    token_mint_str: String,
    max_cycles: u32,
    slippage_bps: u16,
    solana_priority_fee: u64,
    initial_buy_sol_amount: f64, // Added field for initial buy SOL amount
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

// --- Main application state ---
pub struct PumpFunApp {
    // Add the settings field
    app_settings: AppSettings,

    current_view: AppView,
    // --- Input State ---
    // Example for Create view
    create_token_name: String,
    create_token_symbol: String,
    create_token_description: String,
    create_token_image_path: String, // Store path, handle loading separately
    create_dev_buy_amount_tokens: f64,
    create_token_decimals: u8,
    create_jito_bundle: bool,
    create_zombie_amount: u64,

    // Add state fields for other commands (Sell, Config, etc.) ...
    // buy_mint, buy_sol_amount, etc. fields removed.

    sell_mint: String, // Keep this for the target mint input
    // Remove old fields, replaced by interactive dashboard state
    // sell_percentage: f64, // REMOVED
    // sell_wallets: String, // REMOVED
    // sell_all: bool, // REMOVED
    // sell_all_zombies: bool, // REMOVED
    sell_slippage_percent: f64, // Input for slippage percentage
    sell_priority_fee_lamports: u64, // Input for priority fee in lamports


    // --- Pump View State ---
    pump_mint: String,
    pump_buy_amount_sol: f64,
    pump_sell_threshold_sol: f64,
    pump_slippage_percent: f64, // Store as percentage
    pump_priority_fee_lamports: u64,
    pump_jito_tip_sol: f64, // Input for Jito tip in SOL
    pump_lookup_table_address: String, // Optional ALT address string
    pump_private_key_string: String, // Optional private key string
    pump_use_jito_bundle: bool,
    pump_is_running: Arc<AtomicBool>, // Added for toggle state
    pump_task_handle: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>, // Added for task management

    // Removed redundant settings fields (now in app_settings)

    // --- Wallet Display State ---
    wallets: Vec<WalletInfo>, // This now holds all wallets, including Parent/Minter if applicable
    loaded_wallet_data: Vec<LoadedWalletInfo>, // Raw data from keys.json
    // Removed parent_wallet_address
    // Removed parent_wallet_sol_balance
    // Removed minter_wallet_address
    // Removed minter_wallet_sol_balance
    // Removed minter_wallet_token_balance
    wallet_load_error: Option<String>,
    balances_loading: bool, // Still useful to disable button
    disperse_in_progress: bool,   // Separate loading flag for disperse
    gather_in_progress: bool,     // Loading flag for gather
    gather_tasks_expected: usize, // Track gather tasks
    gather_tasks_completed: usize,// Track gather tasks
balance_tasks_expected: usize, // For tracking balance fetch completion
    balance_tasks_completed: usize, // For tracking balance fetch completion
    parent_sol_balance_display: Option<f64>, // For status bar
    total_sol_balance_display: Option<f64>,  // For status bar

    // --- Async Task Communication ---
    balance_fetch_sender: UnboundedSender<FetchResult>,
    balance_fetch_receiver: UnboundedReceiver<FetchResult>,
    disperse_result_sender: UnboundedSender<Result<String, String>>, // Channel for disperse results
    disperse_result_receiver: UnboundedReceiver<Result<String, String>>,
    gather_result_sender: UnboundedSender<Result<String, String>>, // Channel for gather results
    gather_result_receiver: UnboundedReceiver<Result<String, String>>,
    alt_precalc_result_sender: UnboundedSender<PrecalcResult>,
    alt_precalc_result_receiver: UnboundedReceiver<PrecalcResult>,
    // Changed channel type for detailed status updates
    alt_creation_status_sender: UnboundedSender<AltCreationStatus>,
    alt_creation_status_receiver: UnboundedReceiver<AltCreationStatus>,
    alt_deactivate_status_sender: UnboundedSender<Result<String, String>>,
    alt_deactivate_status_receiver: UnboundedReceiver<Result<String, String>>,

    // --- Sell Coordinated State ---
    sell_status_sender: UnboundedSender<SellStatus>,
    sell_status_receiver: UnboundedReceiver<SellStatus>,
    sell_log_messages: Vec<String>, // Log messages specific to sell operations
    sell_mass_reverse_in_progress: bool,
    sell_gather_and_sell_in_progress: bool,

    // --- Pump Coordinated State ---
    pump_status_sender: UnboundedSender<PumpStatus>,
    pump_status_receiver: UnboundedReceiver<PumpStatus>,
    pump_log_messages: Vec<String>, // Log messages specific to pump operations
    pump_in_progress: bool,

    // --- Launch Coordinated State ---
    // Changed channel type for detailed status updates
    launch_status_sender: UnboundedSender<LaunchStatus>,
    launch_status_receiver: UnboundedReceiver<LaunchStatus>,
    launch_log_messages: Vec<String>, // Detailed log messages
    launch_token_name: String,
    launch_token_symbol: String,
    launch_token_description: String,
    launch_token_image_url: String, // URL for image
    launch_dev_buy_sol: f64,
    launch_zombie_buy_sol: f64,
    // Removed launch_slippage_bps state field
    // Removed launch_priority_fee_microlamports state field
    launch_alt_address: String, // Optional ALT address (empty string if none)
    launch_mint_keypair_path: String, // Optional path to mint keypair (empty if none)
    launch_in_progress: bool,
    // Removed launch_num_zombie_buys field
    launch_use_jito_bundle: bool,  // <-- Add this field
    launch_twitter: String,        // <-- Add Twitter URL
    launch_telegram: String,       // <-- Add Telegram URL
    launch_website: String,        // <-- Add Website URL
    // Removed launch_jito_tip_sol state field
    launch_simulate_only: bool,    // <-- Add Simulate Only flag

    // --- Async Task Handling & Results ---
    last_operation_result: Option<Result<String, String>>,

    check_view_target_mint: String, // Input field for target mint
volume_bot_source_wallets_file: String, // Path to the JSON file for volume bot wallets
    disperse_sol_amount: f64,     // Amount per selected zombie

    // --- ALT Management State ---
    alt_view_status: String, // Overall status message
last_generated_volume_wallets_file: Option<String>, // To store the name of the last generated file
    alt_log_messages: Vec<String>, // Detailed log messages
    alt_generated_mint_pubkey: Option<String>,
    alt_precalc_addresses: Vec<Pubkey>, // Store the calculated addresses
    alt_address: Option<Pubkey>,          // Store the final created ALT address
    alt_creation_in_progress: bool,    // Loading flag for ALT creation
    alt_precalc_in_progress: bool,     // Loading flag for Precalculation
    alt_deactivation_in_progress: bool,    // Loading flag for ALT deactivation/close
    alt_deactivation_status_message: String, // Status message for deactivation/close

    // --- Dynamic Loading State ---
    available_alt_addresses: Vec<String>,
    available_mint_keypairs: Vec<String>,
    // --- Transfers View State ---
    transfer_recipient_address: String,
    transfer_sol_amount: f64,
    transfer_selected_sender: Option<String>, // Store Pubkey string of selected sender
    transfer_in_progress: bool,
    transfer_status_sender: UnboundedSender<Result<String, String>>, // Simple Result channel for now
    transfer_status_receiver: UnboundedReceiver<Result<String, String>>,
    transfer_log_messages: Vec<String>,
// --- Volume bot State ---
    sim_token_mint: String,
    sim_max_cycles: usize,
    sim_slippage_bps: u16,
    volume_bot_initial_buy_sol_input: String, // Added for UI input
    sim_in_progress: bool,
    sim_log_messages: Vec<String>,
    sim_status_sender: UnboundedSender<Result<String, String>>, // Simple status for now
    sim_status_receiver: UnboundedReceiver<Result<String, String>>,
    // Use anyhow::Error which is Send + Sync
    sim_task_handle: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    start_volume_bot_request: Option<VolumeBotStartParams>,
    start_fetch_balances_request: Option<FetchBalancesTaskParams>,
    start_load_volume_wallets_request: Option<LoadVolumeWalletsFromFileParams>,
    start_distribute_total_sol_request: Option<DistributeTotalSolTaskParams>,
    start_fund_volume_wallets_request: Option<FundVolumeWalletsTaskParams>,
    start_gather_all_funds_request: Option<GatherAllFundsTaskParams>, // For deferred gathering
    volume_bot_num_wallets_to_generate: usize,
    volume_bot_wallets_generated: bool,
    volume_bot_generation_in_progress: bool,
    volume_bot_wallets: Vec<LoadedWalletInfo>,
    volume_bot_wallet_gen_status_sender: UnboundedSender<Result<String, String>>, // For text status
    volume_bot_wallet_gen_status_receiver: UnboundedReceiver<Result<String, String>>,
    volume_bot_wallets_sender: UnboundedSender<Vec<LoadedWalletInfo>>,      // For the actual wallet data
    volume_bot_wallets_receiver: UnboundedReceiver<Vec<LoadedWalletInfo>>,
    volume_bot_funding_per_wallet_sol: f64,
    volume_bot_funding_in_progress: bool,
    volume_bot_funding_status_sender: UnboundedSender<Result<String, String>>,
    volume_bot_funding_status_receiver: UnboundedReceiver<Result<String, String>>,
    volume_bot_total_sol_to_distribute_input: String,
    volume_bot_funding_source_private_key_input: String,
    volume_bot_funding_log_messages: Vec<String>,
    volume_bot_wallet_display_infos: Vec<WalletInfo>, // <-- ADDED: For independent display of volume bot wallet statuses
    settings_feedback: Option<String>, // <-- Add feedback field for settings view
    settings_generate_count: usize, // <-- Add count for key generation

    // --- Simulation State (Original Logic) ---
    sim_token_mint_orig: String,
    sim_num_wallets_orig: usize,
    sim_wallets_file_orig: String, // Use global app_settings.keys_path? Or separate? Let's make it separate for now.
    sim_initial_sol_orig: f64,
    sim_max_steps_orig: usize,
    sim_slippage_bps_orig: u16,
    sim_in_progress_orig: bool,
    sim_log_messages_orig: Vec<String>,
    // Use anyhow::Error which is Send + Sync
    sim_task_handle_orig: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    // Channel for status updates (String for simplicity, could be more structured)
    sim_status_sender_orig: UnboundedSender<Result<String, String>>,
    sim_status_receiver_orig: UnboundedReceiver<Result<String, String>>,

// --- SOL Consolidation State (Original Sim) ---
    sim_consolidate_sol_in_progress_orig: bool,
    sim_consolidate_sol_status_sender_orig: UnboundedSender<Result<String, String>>, // Simple status for now
    sim_consolidate_sol_status_receiver_orig: UnboundedReceiver<Result<String, String>>,
    // --- Wallet Generation State ---
    sim_wallet_gen_in_progress: bool,
    sim_wallet_gen_status_sender: UnboundedSender<Result<String, String>>,
    sim_wallet_gen_status_receiver: UnboundedReceiver<Result<String, String>>,

    // --- Live Monitor State ---
    monitor_active: Arc<AtomicBool>,
    monitor_task_handle: Option<tokio::task::JoinHandle<()>>, // Task returns nothing directly
    monitor_balances: Vec<WalletBalanceInfo>,
    monitor_prices: Option<(f64, f64)>, // (sol_price, token_price)
    // Channel to send updates from monitor task to UI thread
    monitor_update_sender: UnboundedSender<(Vec<WalletBalanceInfo>, Option<(f64, f64)>)>,
    monitor_update_receiver: UnboundedReceiver<(Vec<WalletBalanceInfo>, Option<(f64, f64)>)>,

    // --- Atomic Buy View State ---
    atomic_buy_mint_address: String,
    atomic_buy_slippage_bps: u64,
    atomic_buy_priority_fee_lamports_per_tx: u64, // Renamed for clarity
    atomic_buy_alt_address: String,
    atomic_buy_in_progress: bool,
    atomic_buy_status_sender: UnboundedSender<Result<String, String>>, // Or a more structured enum
    atomic_buy_status_receiver: UnboundedReceiver<Result<String, String>>,
    atomic_buy_log_messages: Vec<String>,
    // Field for Jito tip for atomic buy, if it's to be configurable in this view
    atomic_buy_jito_tip_sol: f64,
    atomic_buy_task_handle: Option<tokio::task::JoinHandle<()>>, // To manage the spawned atomic buy task
}

// Define struct matching the expected JSON structure for wallet loading
#[derive(serde::Deserialize, Debug)]
struct WalletData {
    name: String,
    key: String, // Private key base58
    address: String, // Public key base58
}

// Define APP color constants at a scope accessible to all methods in this impl block
const APP_BG_PRIMARY: Color32 = Color32::from_rgb(10, 12, 30);    // Very Dark Blue/Purple (near black)
const APP_BG_SECONDARY: Color32 = Color32::from_rgb(30, 40, 75);  // More Saturated Dark Blue (for Cards/Panels)
const APP_SIDEBAR_BG: Color32 = Color32::from_rgb(15, 25, 55);      // Dark blue for sidebar (solid)

// --- NEW LIGHTER BLUE WIDGET COLORS ---
const APP_WIDGET_LIGHT_BLUE_BG: Color32 = Color32::from_rgb(100, 140, 200);   // Distinct lighter blue for inactive/default widget backgrounds
const APP_WIDGET_LIGHT_BLUE_HOVER_BG: Color32 = Color32::from_rgb(120, 160, 220); // Brighter hover for these light blue widgets

const APP_TEXT_PRIMARY: Color32 = Color32::from_rgb(245, 245, 245); // #F5F5F5 - Main text
const APP_TEXT_SECONDARY: Color32 = Color32::from_rgb(170, 190, 230); // Clearer, Lighter Blue for secondary text
const APP_ACCENT_SELECTED_BG: Color32 = Color32::from_rgb(19, 53, 150); // #133596 - Saturated Blue for selected/active BG (e.g. selected sidebar item)
const APP_ACCENT_HOVER_BORDER: Color32 = Color32::from_rgb(120, 180, 250); // Vibrant light blue for hover borders on widgets
const APP_ACCENT_GREEN: Color32 = Color32::from_rgb(0, 200, 100);    // Vibrant green for links/success (kept)
const APP_STROKE_COLOR: Color32 = Color32::from_rgb(70, 90, 130);  // More visible, lighter blue stroke for panels/cards
const APP_WIDGET_STROKE_COLOR: Color32 = Color32::from_rgb(120, 150, 190); // A slightly lighter stroke for the light blue widgets themselves
const APP_FAINT_BLUE_BG: Color32 = Color32::from_rgb(40, 55, 90); // Subtle light blue for faint backgrounds (e.g., table stripes)

impl PumpFunApp {
    // Trigger individual sell from a wallet - either by percentage or amount
    fn trigger_individual_sell(&mut self, address: String, is_percentage: bool) {
        // Find the wallet info in our list
        if let Some(wallet_idx) = self.wallets.iter().position(|w| w.address == address) {
            let wallet_info = &mut self.wallets[wallet_idx];
            
            // Set the wallet to selling state
            wallet_info.is_selling = true;
            
            // Get the input value based on whether we're selling by percentage or amount
            let input_value = if is_percentage {
                wallet_info.sell_percentage_input.clone()
            } else {
                wallet_info.sell_amount_input.clone()
            };
            
            // Clone necessary data for the task
            let mint_address = self.sell_mint.clone();
            let wallet_address = address.clone();
            let slippage_percent = self.sell_slippage_percent;
            let priority_fee_lamports = self.sell_priority_fee_lamports;
            let status_sender = self.sell_status_sender.clone();
            
            // Find the private key for this wallet
            let private_key_opt = self.loaded_wallet_data.iter()
                .find(|w| w.public_key == address)
                .map(|w| w.private_key.clone());
            
            if let Some(private_key) = private_key_opt {
                // Spawn a task to perform the sell
                tokio::spawn(async move {
                    let _ = status_sender.send(SellStatus::InProgress(format!("Selling from wallet {}", wallet_address)));

                    let amount_value = match input_value.trim().parse::<f64>() {
                        Ok(v) if v > 0.0 => v,
                        _ => {
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, "Invalid sell amount/percentage".to_string()));
                            return;
                        }
                    };

                    let keypair_bytes = match bs58::decode(&private_key).into_vec() {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Error decoding private key: {}", e)));
                            return;
                        }
                    };

                    let wallet = match solana_sdk::signature::Keypair::from_bytes(&keypair_bytes) {
                        Ok(kp) => kp,
                        Err(e) => {
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Error creating keypair: {}", e)));
                            return;
                        }
                    };

                    let mint_pubkey = match solana_sdk::pubkey::Pubkey::from_str(&mint_address) {
                        Ok(pk) => pk,
                        Err(e) => {
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Invalid mint address: {}", e)));
                            return;
                        }
                    };

                    // --- Calculate Dynamic Fee ---
                    let dynamic_fee_lamports: u64 = {
                        let calc_rpc_client = AsyncRpcClient::new(crate::config::get_rpc_url());
                        let http_client = ReqwestClient::new();
                        let seller_ata = get_associated_token_address(&wallet.pubkey(), &mint_pubkey);
                        
                        let (seller_token_balance_lamports, token_decimals) = match calc_rpc_client.get_token_account_balance(&seller_ata).await {
                            Ok(balance_response) => {
                                match balance_response.amount.parse::<u64>() {
                                    Ok(bal_lamports) => (bal_lamports, balance_response.decimals),
                                    Err(_) => {
                                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Failed to parse token balance.".to_string()));
                                        return;
                                    }
                                }
                            }
                            Err(_) => { // ATA might not exist, treat as 0 balance for safety
                                (0, 0) // Assume 0 decimals if ATA doesn't exist, though this path means no tokens to sell.
                            }
                        };

                        if seller_token_balance_lamports == 0 && !is_percentage { // If selling specific amount but balance is 0
                             let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: No tokens to sell.".to_string()));
                             return;
                        }
                        
                        let tokens_for_quote_lamports = if is_percentage {
                            if seller_token_balance_lamports == 0 { // Selling percentage of 0 balance
                                 let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Selling 0 tokens (percentage of zero balance).".to_string()));
                                 return;
                            }
                            (seller_token_balance_lamports as f64 * amount_value / 100.0) as u64
                        } else {
                            spl_token::ui_amount_to_amount(amount_value, token_decimals)
                        };

                        if tokens_for_quote_lamports == 0 {
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Calculated tokens to sell is zero.".to_string()));
                            return;
                        }

                        match get_jupiter_quote_v6(&http_client, &mint_address, SOL_MINT_ADDRESS_STR, tokens_for_quote_lamports, slippage_percent as u16).await {
                            Ok(quote_resp) => {
                                match quote_resp.out_amount.parse::<u64>() {
                                    Ok(sol_received) => {
                                        if sol_received == 0 { 0 } else { std::cmp::max(1, sol_received / 1000) } // 0.1%, min 1 lamport
                                    }
                                    Err(_) => {
                                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc: Failed to parse SOL out_amount from Jupiter.".to_string()));
                                        return;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Fee Calc: Jupiter quote failed: {}", e)));
                                return;
                            }
                        }
                    };
                    // --- End Calculate Dynamic Fee ---

                    match crate::api::pumpfun::sell_token(
                        &wallet,
                        &mint_pubkey,
                        amount_value, // This is the original f64 value (percentage or UI amount)
                        slippage_percent as u8,
                        priority_fee_lamports as f64 / solana_sdk::native_token::LAMPORTS_PER_SOL as f64,
                    ).await {
                        Ok(versioned_transaction) => {
                            let main_sell_rpc_client = solana_client::nonblocking::rpc_client::RpcClient::new(crate::config::get_rpc_url());
                            match crate::utils::transaction::sign_and_send_versioned_transaction(&main_sell_rpc_client, versioned_transaction, &[&wallet]).await {
                                Ok(signature) => {
                                    let _ = status_sender.send(SellStatus::WalletSuccess(wallet_address.clone(), signature.to_string()));

                                    if dynamic_fee_lamports > 0 {
                                        // --- Add Fee Transfer Task ---
                                        let fee_rpc_url = crate::config::get_rpc_url();
                                        let fee_wallet_keypair_bytes = wallet.to_bytes();
                                        let fee_status_sender = status_sender.clone();
                                        let fee_wallet_address = wallet_address.clone();
                                        let fee_priority_fee_lamports = priority_fee_lamports; // Use same as main sell

                                        tokio::spawn(async move {
                                            let fee_wallet = match Keypair::from_bytes(&fee_wallet_keypair_bytes) {
                                                Ok(kp) => kp,
                                                Err(e) => {
                                                    let _ = fee_status_sender.send(SellStatus::WalletFailure(fee_wallet_address, format!("Fee Tx: Error reconstructing keypair: {}", e)));
                                                    return;
                                                }
                                            };
                                            let recipient_pubkey = match Pubkey::from_str(FEE_RECIPIENT_ADDRESS_FOR_SELL_STR) {
                                                Ok(pk) => pk,
                                                Err(e) => {
                                                    let _ = fee_status_sender.send(SellStatus::WalletFailure(fee_wallet_address, format!("Fee Tx: Invalid recipient: {}", e)));
                                                    return;
                                                }
                                            };
                                            let transfer_ix = system_instruction::transfer(&fee_wallet.pubkey(), &recipient_pubkey, dynamic_fee_lamports);
                                            let compute_budget_ix = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(fee_priority_fee_lamports);
                                            let fee_tx_rpc_client = solana_client::nonblocking::rpc_client::RpcClient::new(fee_rpc_url);

                                            match fee_tx_rpc_client.get_latest_blockhash().await {
                                                Ok(latest_blockhash) => {
                                                    let tx = Transaction::new_signed_with_payer(&[compute_budget_ix, transfer_ix], Some(&fee_wallet.pubkey()), &[&fee_wallet], latest_blockhash);
                                                    match fee_tx_rpc_client.send_and_confirm_transaction_with_spinner(&tx).await {
                                                        Ok(fee_sig) => {
                                                            let _ = fee_status_sender.send(SellStatus::WalletSuccess(fee_wallet_address, format!("Fee (0.1% = {} lamports) sent. Sig: {}", dynamic_fee_lamports, fee_sig)));
                                                        }
                                                        Err(e) => { let _ = fee_status_sender.send(SellStatus::WalletFailure(fee_wallet_address, format!("Fee Tx: Send failed: {}", e))); }
                                                    }
                                                }
                                                Err(e) => { let _ = fee_status_sender.send(SellStatus::WalletFailure(fee_wallet_address, format!("Fee Tx: Blockhash failed: {}", e))); }
                                            }
                                        });
                                        // --- End Fee Transfer Task ---
                                    } else {
                                         let _ = status_sender.send(SellStatus::WalletSuccess(wallet_address.clone(), "Main sell successful. Fee was 0, not sent.".to_string()));
                                    }
                                },
                                Err(e) => {
                                    let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Main sell tx failed: {}", e)));
                                }
                            }
                        },
                        Err(e) => {
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address, format!("Failed to create main sell transaction: {}", e)));
                        }
                    }
                });
            } else {
                // No private key found for this wallet
                wallet_info.is_selling = false; // Reset selling state
                let _ = self.sell_status_sender.send(SellStatus::WalletFailure(
                    address,
                    "Private key not found for this wallet".to_string()
                ));
            }
        }
    }
    
    // Trigger mass sell in reverse order (from last wallet to first)
    fn trigger_mass_sell_reverse(&mut self) {
        // Set the flag to indicate mass sell is in progress
        self.sell_mass_reverse_in_progress = true;
        
        // Clone necessary data for the task
        let mint_address = self.sell_mint.clone();
        let slippage_percent = self.sell_slippage_percent;
        let priority_fee_lamports = self.sell_priority_fee_lamports;
        let status_sender = self.sell_status_sender.clone();
        
        // Get wallet data in reverse order (excluding primary wallet if needed)
        let mut wallet_data: Vec<(String, String)> = self.loaded_wallet_data.iter()
            .map(|w| (w.public_key.clone(), w.private_key.clone()))
            .collect();
        
        // Reverse the order so we start from the last wallet
        wallet_data.reverse();
        
        // Spawn a task to perform the mass sell
        tokio::spawn(async move {
            // Update status to in progress
            let _ = status_sender.send(SellStatus::InProgress(
                "Starting mass sell in reverse order".to_string()
            ));
            
            let mut success_count = 0;
            let mut failure_count = 0;
            
            for (wallet_address, private_key) in wallet_data {
                // Update status for current wallet
                let _ = status_sender.send(SellStatus::InProgress(
                    format!("Selling from wallet {}", wallet_address)
                ));
                
                // Create keypair from private key
                let keypair_bytes = match bs58::decode(&private_key).into_vec() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(
                            wallet_address.clone(),
                            format!("Error decoding private key: {}", e)
                        ));
                        failure_count += 1;
                        continue;
                    }
                };
                
                let wallet = match solana_sdk::signature::Keypair::from_bytes(&keypair_bytes) {
                    Ok(kp) => kp,
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(
                            wallet_address.clone(),
                            format!("Error creating keypair: {}", e)
                        ));
                        failure_count += 1;
                        continue;
                    }
                };
                
                // Parse mint address
                let mint_pubkey = match solana_sdk::pubkey::Pubkey::from_str(&mint_address) {
                    Ok(pk) => pk,
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(
                            wallet_address.clone(),
                            format!("Invalid mint address: {}", e)
                        ));
                        failure_count += 1;
                        continue;
                    }
                };

                // --- Calculate Dynamic Fee for Mass Sell ---
                let dynamic_fee_lamports_mass: u64 = {
                    let calc_rpc_client_mass = AsyncRpcClient::new(crate::config::get_rpc_url());
                    let http_client_mass = ReqwestClient::new();
                    let seller_ata_mass = get_associated_token_address(&wallet.pubkey(), &mint_pubkey);

                    match calc_rpc_client_mass.get_token_account_balance(&seller_ata_mass).await {
                        Ok(balance_response) => {
                            match balance_response.amount.parse::<u64>() {
                                Ok(tokens_for_quote_lamports_mass) => {
                                    if tokens_for_quote_lamports_mass == 0 {
                                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc (Mass Sell): No tokens to sell.".to_string()));
                                        // Continue to next wallet in the loop, but don't return from the whole task
                                        // This specific wallet won't proceed to sell or fee transfer.
                                        failure_count +=1; // Count as a failure for this wallet's sell attempt
                                        // Use a special value or skip this wallet's sell attempt entirely
                                        // For now, we'll let it proceed to sell_token which will likely fail or do nothing.
                                        // A cleaner way would be to `continue;` here if we are sure sell_token won't be called.
                                        // However, to ensure the loop continues and other wallets are processed,
                                        // we set fee to 0 and let the sell_token call proceed (it will likely sell 0 tokens).
                                        0
                                    } else {
                                        match get_jupiter_quote_v6(&http_client_mass, &mint_address, SOL_MINT_ADDRESS_STR, tokens_for_quote_lamports_mass, slippage_percent as u16).await {
                                            Ok(quote_resp_mass) => {
                                                match quote_resp_mass.out_amount.parse::<u64>() {
                                                    Ok(sol_received_mass) => {
                                                        if sol_received_mass == 0 { 0 } else { std::cmp::max(1, sol_received_mass / 1000) }
                                                    }
                                                    Err(_) => {
                                                        let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc (Mass Sell): Failed to parse SOL out_amount.".to_string()));
                                                        0 // Indicate fee calculation failure by returning 0
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), format!("Fee Calc (Mass Sell): Jupiter quote failed: {}", e)));
                                                0 // Indicate fee calculation failure
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                     let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc (Mass Sell): Failed to parse token balance.".to_string()));
                                     0 // Indicate fee calculation failure
                                }
                            }
                        }
                        Err(_) => { // ATA might not exist
                            let _ = status_sender.send(SellStatus::WalletFailure(wallet_address.clone(), "Fee Calc (Mass Sell): Token account not found or error fetching balance.".to_string()));
                            0 // Indicate no tokens, so no fee
                        }
                    }
                };
                // --- End Calculate Dynamic Fee for Mass Sell ---
                
                // Call the API to sell tokens (100% sell)
                // The sell_token API expects a percentage (0-100) or a UI amount.
                // For mass sell, we always intend to sell 100% of what the wallet holds.
                // The dynamic fee calculation above already determined the exact token amount for the quote.
                // The sell_token function itself will handle fetching the balance again if it needs to.
                match crate::api::pumpfun::sell_token(
                    &wallet,
                    &mint_pubkey,
                    100.0, // Still pass 100.0 for 100% sell intention
                    slippage_percent as u8,
                    priority_fee_lamports as f64 / solana_sdk::native_token::LAMPORTS_PER_SOL as f64,
                ).await {
                    Ok(versioned_transaction) => {
                        let main_sell_rpc_client_mass = solana_client::nonblocking::rpc_client::RpcClient::new(crate::config::get_rpc_url());
                        match crate::utils::transaction::sign_and_send_versioned_transaction(&main_sell_rpc_client_mass, versioned_transaction, &[&wallet]).await {
                            Ok(signature) => {
                                let _ = status_sender.send(SellStatus::WalletSuccess(wallet_address.clone(), signature.to_string()));
                                success_count += 1;

                                if dynamic_fee_lamports_mass > 0 {
                                    // --- Add Fee Transfer Task ---
                                    let fee_rpc_url_mass = crate::config::get_rpc_url();
                                    let fee_wallet_keypair_bytes_mass = wallet.to_bytes();
                                    let fee_status_sender_mass = status_sender.clone();
                                    let fee_wallet_address_mass = wallet_address.clone();
                                    let fee_priority_fee_lamports_mass = priority_fee_lamports;

                                    tokio::spawn(async move {
                                        let fee_wallet_mass = match Keypair::from_bytes(&fee_wallet_keypair_bytes_mass) {
                                            Ok(kp) => kp,
                                            Err(e) => {
                                                let _ = fee_status_sender_mass.send(SellStatus::WalletFailure(fee_wallet_address_mass, format!("Fee Tx (Mass): Error reconstructing keypair: {}", e)));
                                                return;
                                            }
                                        };
                                        let recipient_pubkey_mass = match Pubkey::from_str(FEE_RECIPIENT_ADDRESS_FOR_SELL_STR) {
                                            Ok(pk) => pk,
                                            Err(e) => {
                                                let _ = fee_status_sender_mass.send(SellStatus::WalletFailure(fee_wallet_address_mass, format!("Fee Tx (Mass): Invalid recipient: {}", e)));
                                                return;
                                            }
                                        };
                                        let transfer_ix_mass = system_instruction::transfer(&fee_wallet_mass.pubkey(), &recipient_pubkey_mass, dynamic_fee_lamports_mass);
                                        let compute_budget_ix_mass = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(fee_priority_fee_lamports_mass);
                                        let fee_tx_rpc_client_mass = solana_client::nonblocking::rpc_client::RpcClient::new(fee_rpc_url_mass);

                                        match fee_tx_rpc_client_mass.get_latest_blockhash().await {
                                            Ok(latest_blockhash_mass) => {
                                                let tx_mass = Transaction::new_signed_with_payer(&[compute_budget_ix_mass, transfer_ix_mass], Some(&fee_wallet_mass.pubkey()), &[&fee_wallet_mass], latest_blockhash_mass);
                                                match fee_tx_rpc_client_mass.send_and_confirm_transaction_with_spinner(&tx_mass).await {
                                                    Ok(fee_sig_mass) => {
                                                        let _ = fee_status_sender_mass.send(SellStatus::WalletSuccess(fee_wallet_address_mass, format!("Fee (0.1% = {} lamports) (Mass Sell) sent. Sig: {}", dynamic_fee_lamports_mass, fee_sig_mass)));
                                                    }
                                                    Err(e) => { let _ = fee_status_sender_mass.send(SellStatus::WalletFailure(fee_wallet_address_mass, format!("Fee Tx (Mass): Send failed: {}", e))); }
                                                }
                                            }
                                            Err(e) => { let _ = fee_status_sender_mass.send(SellStatus::WalletFailure(fee_wallet_address_mass, format!("Fee Tx (Mass): Blockhash failed: {}", e))); }
                                        }
                                    });
                                    // --- End Fee Transfer Task ---
                                } else {
                                    let _ = status_sender.send(SellStatus::WalletSuccess(wallet_address.clone(), "Main sell successful (Mass). Fee was 0, not sent.".to_string()));
                                }
                            },
                            Err(e) => {
                                let _ = status_sender.send(SellStatus::WalletFailure(
                                    wallet_address.clone(),
                                    format!("Failed to send transaction: {}", e)
                                ));
                                failure_count += 1;
                            }
                        }
                    },
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(
                            wallet_address.clone(),
                            format!("Failed to create sell transaction: {}", e)
                        ));
                        failure_count += 1;
                    }
                }
                
                // Add a small delay between transactions to avoid rate limiting
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            
            // Send final status
            let _ = status_sender.send(SellStatus::MassSellComplete(
                format!("Mass sell complete: {} successful, {} failed", success_count, failure_count)
            ));
        });
    }
    
    // Trigger gather and sell operation
    fn trigger_gather_and_sell(&mut self) {
        // Set the flag to indicate gather and sell is in progress
        self.sell_gather_and_sell_in_progress = true;
        
        // Find the primary wallet (first wallet in the list)
        if self.loaded_wallet_data.is_empty() {
            let _ = self.sell_status_sender.send(SellStatus::Failure(
                "No wallets loaded".to_string()
            ));
            self.sell_gather_and_sell_in_progress = false;
            return;
        }
        
        // Get the primary wallet (first in the list)
        let primary_wallet = self.loaded_wallet_data[0].clone();
        
        // Clone necessary data for the task
        let mint_address = self.sell_mint.clone();
        let slippage_percent = self.sell_slippage_percent;
        let priority_fee_lamports = self.sell_priority_fee_lamports;
        let status_sender = self.sell_status_sender.clone();
        let wallet_data = self.loaded_wallet_data.clone();
        
        // Spawn a task to perform the gather and sell
        tokio::spawn(async move {
            // Update status to in progress
            let _ = status_sender.send(SellStatus::InProgress(
                "Starting gather and sell operation".to_string()
            ));
            
            // Parse mint address
            let mint_pubkey = match solana_sdk::pubkey::Pubkey::from_str(&mint_address) {
                Ok(pk) => pk,
                Err(e) => {
                    let _ = status_sender.send(SellStatus::Failure(
                        format!("Invalid mint address: {}", e)
                    ));
                    return;
                }
            };
            
            // Create primary wallet keypair
            let primary_keypair_bytes = match bs58::decode(&primary_wallet.private_key).into_vec() {
                Ok(bytes) => bytes,
                Err(e) => {
                    let _ = status_sender.send(SellStatus::Failure(
                        format!("Error decoding primary wallet private key: {}", e)
                    ));
                    return;
                }
            };
            
            let primary_keypair = match solana_sdk::signature::Keypair::from_bytes(&primary_keypair_bytes) {
                Ok(kp) => kp,
                Err(e) => {
                    let _ = status_sender.send(SellStatus::Failure(
                        format!("Error creating primary wallet keypair: {}", e)
                    ));
                    return;
                }
            };
            
            // First, gather tokens from all other wallets to the primary wallet
            let _ = status_sender.send(SellStatus::InProgress(
                "Gathering tokens from all wallets to primary wallet".to_string()
            ));
            
            let mut gather_success_count = 0;
            let mut gather_failure_count = 0;
            
            // Skip the first wallet (primary) and gather from the rest
            for wallet_info in wallet_data.iter().skip(1) {
                let _ = status_sender.send(SellStatus::InProgress(
                    format!("Gathering tokens from wallet {}", wallet_info.public_key)
                ));
                
                // Create sender wallet keypair
                let sender_keypair_bytes = match bs58::decode(&wallet_info.private_key).into_vec() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(
                            wallet_info.public_key.clone(),
                            format!("Error decoding private key: {}", e)
                        ));
                        gather_failure_count += 1;
                        continue;
                    }
                };
                
                let sender_keypair = match solana_sdk::signature::Keypair::from_bytes(&sender_keypair_bytes) {
                    Ok(kp) => kp,
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(
                            wallet_info.public_key.clone(),
                            format!("Error creating keypair: {}", e)
                        ));
                        gather_failure_count += 1;
                        continue;
                    }
                };
                
                // Create RPC client
                let rpc_client = solana_client::nonblocking::rpc_client::RpcClient::new(
                    crate::config::get_rpc_url()
                );
                
                // Get the token account for the sender
                let sender_token_account = get_associated_token_address(
                    &sender_keypair.pubkey(),
                    &mint_pubkey
                );
                
                // Get the token account for the recipient (primary wallet)
                let recipient_token_account = get_associated_token_address(
                    &primary_keypair.pubkey(),
                    &mint_pubkey
                );
                
                // Create a transfer instruction to send all tokens
                // We need to get the token balance first
                match rpc_client.get_token_account_balance(&sender_token_account).await {
                    Ok(token_amount) => {
                        let amount = token_amount.amount.parse::<u64>().unwrap_or(0);
                        
                        if amount == 0 {
                            let _ = status_sender.send(SellStatus::WalletSuccess(
                                wallet_info.public_key.clone(),
                                "No tokens to transfer".to_string()
                            ));
                            continue;
                        }
                        
                        // Create transfer instruction
                        let transfer_ix = spl_token::instruction::transfer(
                            &spl_token::id(),
                            &sender_token_account,
                            &recipient_token_account,
                            &sender_keypair.pubkey(),
                            &[],
                            amount,
                        ).unwrap();
                        
                        // Add compute budget instruction for priority fee
                        let compute_budget_ix = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(
                            priority_fee_lamports
                        );
                        
                        // Create transaction
                        let recent_blockhash = match rpc_client.get_latest_blockhash().await {
                            Ok(blockhash) => blockhash,
                            Err(e) => {
                                let _ = status_sender.send(SellStatus::WalletFailure(
                                    wallet_info.public_key.clone(),
                                    format!("Failed to get blockhash: {}", e)
                                ));
                                gather_failure_count += 1;
                                continue;
                            }
                        };
                        
                        let transaction = solana_sdk::transaction::Transaction::new_signed_with_payer(
                            &[compute_budget_ix, transfer_ix],
                            Some(&sender_keypair.pubkey()),
                            &[&sender_keypair],
                            recent_blockhash,
                        );
                        
                        // Send transaction
                        match rpc_client.send_and_confirm_transaction(&transaction).await {
                            Ok(signature) => {
                                let _ = status_sender.send(SellStatus::WalletSuccess(
                                    wallet_info.public_key.clone(),
                                    format!("Tokens transferred: {}", signature)
                                ));
                                gather_success_count += 1;
                            },
                            Err(e) => {
                                let _ = status_sender.send(SellStatus::WalletFailure(
                                    wallet_info.public_key.clone(),
                                    format!("Failed to send transfer transaction: {}", e)
                                ));
                                gather_failure_count += 1;
                            }
                        }
                    },
                    Err(e) => {
                        let _ = status_sender.send(SellStatus::WalletFailure(
                            wallet_info.public_key.clone(),
                            format!("Failed to get token balance: {}", e)
                        ));
                        gather_failure_count += 1;
                    }
                }
                
                // Add a small delay between transactions
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            
            // --- Calculate Dynamic Fee for Gather & Sell ---
            let dynamic_fee_lamports_gather: u64 = {
                let calc_rpc_client_gather = AsyncRpcClient::new(crate::config::get_rpc_url());
                let http_client_gather = ReqwestClient::new();
                let primary_wallet_ata_gather = get_associated_token_address(&primary_keypair.pubkey(), &mint_pubkey);

                match calc_rpc_client_gather.get_token_account_balance(&primary_wallet_ata_gather).await {
                    Ok(balance_response) => {
                        match balance_response.amount.parse::<u64>() {
                            Ok(tokens_for_quote_lamports_gather) => {
                                if tokens_for_quote_lamports_gather == 0 {
                                    let _ = status_sender.send(SellStatus::WalletSuccess(primary_wallet.public_key.clone(), "Fee Calc (Gather & Sell): No tokens on primary wallet to sell.".to_string()));
                                    0 // No tokens to sell, so no fee
                                } else {
                                    match get_jupiter_quote_v6(&http_client_gather, &mint_address, SOL_MINT_ADDRESS_STR, tokens_for_quote_lamports_gather, slippage_percent as u16).await {
                                        Ok(quote_resp_gather) => {
                                            match quote_resp_gather.out_amount.parse::<u64>() {
                                                Ok(sol_received_gather) => {
                                                    if sol_received_gather == 0 { 0 } else { std::cmp::max(1, sol_received_gather / 1000) }
                                                }
                                                Err(_) => {
                                                    let _ = status_sender.send(SellStatus::WalletFailure(primary_wallet.public_key.clone(), "Fee Calc (Gather & Sell): Failed to parse SOL out_amount.".to_string()));
                                                    0
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let _ = status_sender.send(SellStatus::WalletFailure(primary_wallet.public_key.clone(), format!("Fee Calc (Gather & Sell): Jupiter quote failed: {}", e)));
                                            0
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                let _ = status_sender.send(SellStatus::WalletFailure(primary_wallet.public_key.clone(), "Fee Calc (Gather & Sell): Failed to parse token balance on primary.".to_string()));
                                0
                            }
                        }
                    }
                    Err(_) => { // ATA might not exist on primary
                        let _ = status_sender.send(SellStatus::WalletFailure(primary_wallet.public_key.clone(), "Fee Calc (Gather & Sell): Token account not found on primary or error fetching balance.".to_string()));
                        0
                    }
                }
            };
            // --- End Calculate Dynamic Fee for Gather & Sell ---

            // Now sell all tokens from the primary wallet
            let _ = status_sender.send(SellStatus::InProgress(
                format!("Selling all tokens from primary wallet {}", primary_wallet.public_key)
            ));
            
            if dynamic_fee_lamports_gather == 0 { // If fee calc failed or no tokens, we might not want to proceed with sell
                 let _ = status_sender.send(SellStatus::GatherAndSellComplete(
                    format!("Gather and sell complete: {} wallets gathered. Primary wallet had no tokens to sell or fee calculation failed. No fee sent.",
                        gather_success_count)
                ));
                return; // Exit if no fee can be calculated (implies no SOL would be received or no tokens to sell)
            }

            match crate::api::pumpfun::sell_token(
                &primary_keypair,
                &mint_pubkey,
                100.0, // 100% sell
                slippage_percent as u8,
                priority_fee_lamports as f64 / solana_sdk::native_token::LAMPORTS_PER_SOL as f64,
            ).await {
                Ok(versioned_transaction) => {
                    let main_sell_rpc_client_gather = solana_client::nonblocking::rpc_client::RpcClient::new(crate::config::get_rpc_url());
                    match crate::utils::transaction::sign_and_send_versioned_transaction(&main_sell_rpc_client_gather, versioned_transaction, &[&primary_keypair]).await {
                        Ok(signature) => {
                            let _ = status_sender.send(SellStatus::WalletSuccess(primary_wallet.public_key.clone(), signature.to_string()));

                            if dynamic_fee_lamports_gather > 0 {
                                // --- Add Fee Transfer Task ---
                                let fee_rpc_url_gather = crate::config::get_rpc_url();
                                let fee_wallet_keypair_bytes_gather = primary_keypair.to_bytes();
                                let fee_status_sender_gather = status_sender.clone();
                                let fee_wallet_address_gather = primary_wallet.public_key.clone();
                                let fee_priority_fee_lamports_gather = priority_fee_lamports;

                                tokio::spawn(async move {
                                    let fee_wallet_gather = match Keypair::from_bytes(&fee_wallet_keypair_bytes_gather) {
                                        Ok(kp) => kp,
                                        Err(e) => {
                                            let _ = fee_status_sender_gather.send(SellStatus::WalletFailure(fee_wallet_address_gather, format!("Fee Tx (G&S): Error reconstructing keypair: {}", e)));
                                            return;
                                        }
                                    };
                                    let recipient_pubkey_gather = match Pubkey::from_str(FEE_RECIPIENT_ADDRESS_FOR_SELL_STR) {
                                        Ok(pk) => pk,
                                        Err(e) => {
                                            let _ = fee_status_sender_gather.send(SellStatus::WalletFailure(fee_wallet_address_gather, format!("Fee Tx (G&S): Invalid recipient: {}", e)));
                                            return;
                                        }
                                    };
                                    let transfer_ix_gather = system_instruction::transfer(&fee_wallet_gather.pubkey(), &recipient_pubkey_gather, dynamic_fee_lamports_gather);
                                    let compute_budget_ix_gather = solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(fee_priority_fee_lamports_gather);
                                    let fee_tx_rpc_client_gather = solana_client::nonblocking::rpc_client::RpcClient::new(fee_rpc_url_gather);

                                    match fee_tx_rpc_client_gather.get_latest_blockhash().await {
                                        Ok(latest_blockhash_gather) => {
                                            let tx_gather = Transaction::new_signed_with_payer(&[compute_budget_ix_gather, transfer_ix_gather], Some(&fee_wallet_gather.pubkey()), &[&fee_wallet_gather], latest_blockhash_gather);
                                            match fee_tx_rpc_client_gather.send_and_confirm_transaction_with_spinner(&tx_gather).await {
                                                Ok(fee_sig_gather) => {
                                                    let _ = fee_status_sender_gather.send(SellStatus::WalletSuccess(fee_wallet_address_gather, format!("Fee (0.1% = {} lamports) (G&S) sent. Sig: {}", dynamic_fee_lamports_gather, fee_sig_gather)));
                                                }
                                                Err(e) => { let _ = fee_status_sender_gather.send(SellStatus::WalletFailure(fee_wallet_address_gather, format!("Fee Tx (G&S): Send failed: {}", e))); }
                                            }
                                        }
                                        Err(e) => { let _ = fee_status_sender_gather.send(SellStatus::WalletFailure(fee_wallet_address_gather, format!("Fee Tx (G&S): Blockhash failed: {}", e))); }
                                    }
                                });
                                // --- End Fee Transfer Task ---
                            } else {
                                 let _ = status_sender.send(SellStatus::WalletSuccess(primary_wallet.public_key.clone(), "Main sell successful (G&S). Fee was 0, not sent.".to_string()));
                            }
                            
                            // Send final status
                            let _ = status_sender.send(SellStatus::GatherAndSellComplete(
                                format!("Gather and sell complete: {} wallets gathered successfully, {} failed. Primary wallet sold all tokens.",
                                    gather_success_count, gather_failure_count)
                            ));
                        },
                        Err(e) => {
                            let _ = status_sender.send(SellStatus::WalletFailure(
                                primary_wallet.public_key.clone(),
                                format!("Failed to send sell transaction: {}", e)
                            ));
                            
                            // Send final status with error
                            let _ = status_sender.send(SellStatus::GatherAndSellComplete(
                                format!("Gather complete: {} wallets gathered successfully, {} failed. Failed to sell from primary wallet: {}", 
                                    gather_success_count, gather_failure_count, e)
                            ));
                        }
                    }
                },
                Err(e) => {
                    let _ = status_sender.send(SellStatus::WalletFailure(
                        primary_wallet.public_key.clone(),
                        format!("Failed to create sell transaction: {}", e)
                    ));
                    
                    // Send final status with error
                    let _ = status_sender.send(SellStatus::GatherAndSellComplete(
                        format!("Gather complete: {} wallets gathered successfully, {} failed. Failed to create sell transaction for primary wallet: {}", 
                            gather_success_count, gather_failure_count, e)
                    ));
                }
            }
        });
    }
    
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self { // Prefix unused cc with _
        // Load settings from file
        let mut app_settings = AppSettings::load(); // Make mutable for potential defaults

        // Blockchain-themed dark mode visuals - NEW PALETTE
        let mut visuals = Visuals::dark(); // Start with egui's dark theme
        // Color constants are now defined at the impl PumpFunApp scope

        visuals.dark_mode = true;
        visuals.override_text_color = Some(APP_TEXT_PRIMARY);
        
        // Non-interactive widgets (labels, separators)
        visuals.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, APP_TEXT_PRIMARY);
        visuals.widgets.noninteractive.bg_stroke = Stroke::NONE;
        visuals.widgets.noninteractive.rounding = Rounding::same(2.0);

        // Inactive widgets (buttons, text edits before interaction)
        visuals.widgets.inactive.bg_fill = APP_WIDGET_LIGHT_BLUE_BG; 
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, APP_TEXT_PRIMARY); 
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, APP_WIDGET_STROKE_COLOR); // Stroke for light blue widgets
        visuals.widgets.inactive.rounding = Rounding::same(4.0);

        // Hovered widgets
        visuals.widgets.hovered.bg_fill = APP_WIDGET_LIGHT_BLUE_HOVER_BG; 
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, APP_TEXT_PRIMARY);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, APP_ACCENT_HOVER_BORDER); 
        visuals.widgets.hovered.rounding = Rounding::same(4.0);

        // Active widgets (button pressed, focused text edit, selected sidebar item)
        // For selected sidebar items, this uses APP_ACCENT_SELECTED_BG (the #133596 dark blue)
        // For *buttons* that are clicked, they will use this APP_ACCENT_SELECTED_BG.
        // If you want clicked buttons to be a *lighter* blue, we'd need another constant or to adjust APP_ACCENT_SELECTED_BG.
        visuals.widgets.active.bg_fill = APP_ACCENT_SELECTED_BG; 
        visuals.widgets.active.fg_stroke = Stroke::new(1.0, APP_TEXT_PRIMARY); 
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, APP_ACCENT_SELECTED_BG); 
        visuals.widgets.active.rounding = Rounding::same(4.0);

        // Open widgets (dropdown menus)
        visuals.widgets.open.bg_fill = APP_WIDGET_LIGHT_BLUE_BG; 
        visuals.widgets.open.fg_stroke = Stroke::new(1.0, APP_TEXT_PRIMARY);
        visuals.widgets.open.bg_stroke = Stroke::new(1.0, APP_WIDGET_STROKE_COLOR); // Stroke for light blue widgets
        visuals.widgets.open.rounding = Rounding::same(4.0);
        
        visuals.selection.bg_fill = APP_ACCENT_SELECTED_BG.linear_multiply(0.4); // Text selection color based on #133596
        visuals.selection.stroke = Stroke::new(1.0, APP_TEXT_PRIMARY); // Text color for selected text

        visuals.window_fill = APP_BG_PRIMARY;
        visuals.panel_fill = APP_BG_SECONDARY; // For side panels and central panel background
        visuals.window_stroke = Stroke::new(1.0, APP_STROKE_COLOR); // Borders for panels/windows
        visuals.window_rounding = Rounding::same(0.0); // Main window no rounding

        visuals.warn_fg_color = Color32::from_rgb(255, 193, 7); // Amber/Yellow (Keep existing good one)
        visuals.error_fg_color = Color32::from_rgb(220, 53, 69); // A common "danger" red (Keep existing good one)
        visuals.hyperlink_color = APP_ACCENT_GREEN;

        visuals.faint_bg_color = APP_FAINT_BLUE_BG; // For striped rows, use new faint blue
        visuals.extreme_bg_color = Color32::from_rgb(5, 10, 20); // Even darker for code block backgrounds or similar

        // Shadows can be adjusted for a more modern feel, maybe slightly softer or more pronounced
        visuals.popup_shadow = egui::epaint::Shadow { offset: egui::vec2(3.0, 3.0), blur: 6.0, spread: 1.0, color: Color32::from_black_alpha(100) };
        visuals.window_shadow = egui::epaint::Shadow { offset: egui::vec2(5.0, 5.0), blur: 10.0, spread: 2.0, color: Color32::from_black_alpha(80) };
        
        _cc.egui_ctx.set_visuals(visuals);

        let mut style = (*_cc.egui_ctx.style()).clone();
        // Consistent Spacing & Padding
        style.spacing.item_spacing = egui::vec2(12.0, 12.0);         // Generous spacing between widgets
        style.spacing.window_margin = Margin::same(16.0);           // Padding around window/panel edges
        style.spacing.button_padding = egui::vec2(14.0, 10.0);      // Comfortable padding within buttons
        style.spacing.menu_margin = Margin::same(10.0);             // Padding around dropdown menus
        style.spacing.indent = 20.0;                                // Indentation for tree views or nested items
        // style.spacing.group_spacing = egui::vec2(10.0, 10.0);    // Field does not exist in this egui version
        style.spacing.slider_width = 200.0;                         // Default width for sliders
        style.spacing.combo_width = 180.0;                          // Default width for combo boxes
        style.spacing.text_edit_width = 280.0;                      // Default width for single-line text edits
        style.spacing.interact_size = egui::vec2(40.0, 24.0);       // Minimum size for interactive elements like small buttons or color pickers
        style.spacing.icon_width = 18.0;                            // Width for icons in menus or buttons
        style.spacing.icon_spacing = 4.0;                           // Spacing after an icon
        
        // Consistent Rounding
        let base_rounding = Rounding::same(6.0); // A slightly more pronounced rounding for a modern feel
        style.visuals.widgets.inactive.rounding = base_rounding;
        style.visuals.widgets.hovered.rounding = base_rounding;
        style.visuals.widgets.active.rounding = base_rounding;
        style.visuals.widgets.open.rounding = base_rounding;
        style.visuals.window_rounding = base_rounding;       // Rounding for panels, popups, and cards
        style.visuals.menu_rounding = Rounding::same(4.0);   // Menus can be slightly less rounded
        // style.visuals.image_rounding = base_rounding;        // Field does not exist in this egui version

        // Ensure panel strokes are consistent with the new theme if not already covered by visuals.window_stroke
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::NONE; // No borders on simple labels etc.
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, APP_WIDGET_STROKE_COLOR); // Match widget specific stroke
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.5, APP_ACCENT_HOVER_BORDER); // Thicker accent on hover
        style.visuals.widgets.active.bg_stroke = Stroke::new(1.5, APP_ACCENT_SELECTED_BG); // Active border based on #133596
        style.visuals.widgets.open.bg_stroke = Stroke::new(1.0, APP_WIDGET_STROKE_COLOR); // Match widget specific stroke

        _cc.egui_ctx.set_style(style);

        // Use keys_path from loaded settings
        // Load keys using crate::wallet:: function path
        let (loaded_data_vec, wallets_for_display, wallet_load_error) = // Make mutable
            match crate::wallet::load_keys_from_file(Path::new(&app_settings.keys_path)) {
                Ok(loaded_keys) => {
                    log::info!("Loaded {} wallets from {}", loaded_keys.wallets.len(), &app_settings.keys_path);
                    let mut loaded_data_mut = loaded_keys.wallets.clone(); // Explicitly clone loaded data
                    let mut wallet_infos_for_ui: Vec<WalletInfo> = loaded_data_mut.iter() // Iterate over the mutable copy
                        .enumerate()
                        .map(|(index, loaded_info)| {
                            // Check if this loaded wallet is the dev_wallet
                            let dev_wallet_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&app_settings.dev_wallet_private_key);
                            let is_dev_wallet = dev_wallet_pubkey_str_opt.as_ref().map_or(false, |dpk| dpk == &loaded_info.public_key);

                            let parent_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&app_settings.parent_wallet_private_key);
                            let is_parent = parent_pubkey_str_opt.as_ref().map_or(false, |ppk| ppk == &loaded_info.public_key);

                            WalletInfo {
                                address: loaded_info.public_key.clone(),
                                is_primary: index == 0, // Still mark primary based on position in file
                                is_dev_wallet, // Set based on comparison with settings
                                is_parent, // Set based on comparison with settings
                                sol_balance: None,
                                target_mint_balance: None,
                                error: None,
                                is_loading: false,
                                is_selected: false,
                                sell_percentage_input: "100".to_string(), // Default to 100%
                                sell_amount_input: String::new(),      // Default to empty
                                is_selling: false,                     // Default to not selling
                                atomic_buy_sol_amount_input: "0.0".to_string(),
                            }
                        })
                        .collect();

                    // Check if the dev_wallet from settings *wasn't* in keys.json and add it if needed
                    if let Some(dev_wallet_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&app_settings.dev_wallet_private_key) {
                        if !wallet_infos_for_ui.iter().any(|w| w.address == dev_wallet_pubkey_str) {
                            log::info!("Adding Dev wallet ({}) from settings to display list.", dev_wallet_pubkey_str);
                            // Check if this added dev_wallet is ALSO the parent
                            let is_parent = AppSettings::get_pubkey_from_privkey_str(&app_settings.parent_wallet_private_key)
                                .map_or(false, |ppk| ppk == dev_wallet_pubkey_str);
                            wallet_infos_for_ui.push(WalletInfo {
                                address: dev_wallet_pubkey_str.clone(), // Clone pubkey string
                                is_primary: false,
                                is_dev_wallet: true,
                                is_parent, // Set parent flag if it matches
                                sol_balance: None,
                                target_mint_balance: None,
                                error: None,
                                is_loading: false,
                                is_selected: false,
                                sell_percentage_input: "100".to_string(),
                                sell_amount_input: String::new(),
                                is_selling: false,
                                atomic_buy_sol_amount_input: "0.0".to_string(),
                            });
// ALSO ADD TO loaded_data_mut
                            // Check if it's already in the data list (e.g., if keys.json had it but UI check failed somehow)
                            if !loaded_data_mut.iter().any(|w| w.public_key == dev_wallet_pubkey_str.clone()) {
                                let wallet_name = Some("Dev (from Settings)".to_string()); // Assign to variable
                                loaded_data_mut.push(LoadedWalletInfo { // Add to the mutable data list
                                    name: wallet_name, // Use variable
                                    private_key: app_settings.dev_wallet_private_key.clone(),
                                    public_key: dev_wallet_pubkey_str.clone(), // Clone pubkey string
                                });
                            }
                            // ALSO ADD TO loaded_data_mut
                            // Check if it's already in the data list (e.g., if keys.json had it but UI check failed somehow)
                            if !loaded_data_mut.iter().any(|w| w.public_key == dev_wallet_pubkey_str) {
                                let wallet_name = Some("Dev (from Settings)".to_string()); // Assign to variable
                                loaded_data_mut.push(LoadedWalletInfo { // Add to the mutable data list
                                    name: wallet_name, // Use variable
                                    private_key: app_settings.dev_wallet_private_key.clone(),
                                    public_key: dev_wallet_pubkey_str, // Use the derived pubkey string
                                });
                            }
                        }
                    }

                    // Check if the parent from settings *wasn't* in keys.json (and wasn't added as the dev_wallet above) and add it if needed
                    if let Some(parent_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&app_settings.parent_wallet_private_key) {
                         if !wallet_infos_for_ui.iter().any(|w| w.address == parent_pubkey_str) {
                            log::info!("Adding Parent wallet ({}) from settings to display list.", parent_pubkey_str);
                            // Parent cannot also be primary if added here
                            // Parent cannot also be dev_wallet if added here (already checked above)
                            wallet_infos_for_ui.push(WalletInfo {
                                address: parent_pubkey_str,
                                is_primary: false,
                                is_dev_wallet: false,
                                is_parent: true, // Definitely the parent if added here
                                sol_balance: None,
                                target_mint_balance: None,
                                error: None,
                                is_loading: false,
                                is_selected: false,
                                sell_percentage_input: "100".to_string(),
                                sell_amount_input: String::new(),
                                is_selling: false,
                                atomic_buy_sol_amount_input: "0.0".to_string(),
                            });
                        }
                    }


                    (loaded_data_mut, wallet_infos_for_ui, None) // Return the modified data list
                }
                Err(e) => {
                    log::error!("Failed to load keys from {}: {}", &app_settings.keys_path, e);
                    let error_msg = format!("Failed to load keys from {}: {}", app_settings.keys_path, e);
                    // Return empty vectors and the error
                    (Vec::new(), Vec::new(), Some(error_msg))
                }
            };

        // Create the channels
        let (balance_fetch_sender, balance_fetch_receiver) = mpsc::unbounded_channel();
        let (atomic_buy_status_sender, atomic_buy_status_receiver) = mpsc::unbounded_channel::<Result<String, String>>();
        let (disperse_result_sender, disperse_result_receiver) = mpsc::unbounded_channel();
        let (gather_result_sender, gather_result_receiver) = mpsc::unbounded_channel::<Result<String, String>>(); // Create gather channel
        let (alt_precalc_result_sender, alt_precalc_result_receiver) = mpsc::unbounded_channel::<PrecalcResult>(); // Create ALT Precalc channel
        // Update channel creation for new status enum
// Create channel for Volume bot status updates
        let (sim_status_sender, sim_status_receiver) = mpsc::unbounded_channel::<Result<String, String>>();
        let (alt_creation_status_sender, alt_creation_status_receiver) = mpsc::unbounded_channel::<AltCreationStatus>();
let (alt_deactivate_status_sender, alt_deactivate_status_receiver) = mpsc::unbounded_channel::<Result<String, String>>();
        // Update channel creation for new status enum

        let (launch_status_sender, launch_status_receiver) = mpsc::unbounded_channel::<LaunchStatus>();
        
        let (sell_status_sender, sell_status_receiver) = mpsc::unbounded_channel::<SellStatus>(); // Create Sell channel
    
        // Create channel for Pump status updates
        let (pump_status_sender, pump_status_receiver) = mpsc::unbounded_channel::<PumpStatus>();
        let (transfer_status_sender, transfer_status_receiver) = mpsc::unbounded_channel::<Result<String, String>>(); // Create Transfers channel
        // Create channel for original Simulation status updates
        let (sim_status_sender_orig, sim_status_receiver_orig) = mpsc::unbounded_channel::<Result<String, String>>();
        // Create channel for wallet generation status
        let (sim_wallet_gen_status_sender, sim_wallet_gen_status_receiver) = mpsc::unbounded_channel::<Result<String, String>>();
        // Create channel for monitor updates
        let (monitor_update_sender, monitor_update_receiver) = mpsc::unbounded_channel();
        let (volume_bot_wallet_gen_status_sender, volume_bot_wallet_gen_status_receiver) = mpsc::unbounded_channel::<Result<String, String>>();
        let (volume_bot_wallets_sender, volume_bot_wallets_receiver) = mpsc::unbounded_channel::<Vec<LoadedWalletInfo>>();
let (volume_bot_funding_status_sender, volume_bot_funding_status_receiver) = mpsc::unbounded_channel::<Result<String, String>>();
// Create channel for SOL consolidation status (Original Sim)
        let (sim_consolidate_sol_status_sender_orig, sim_consolidate_sol_status_receiver_orig) = mpsc::unbounded_channel::<Result<String, String>>();


        // Initialize dynamic loading state
        let available_alt_addresses = Vec::new();
        let available_mint_keypairs = Vec::new();

        // Initialize state with defaults and loaded wallets
        let mut app = Self {
            app_settings: app_settings.clone(), // Clone the loaded settings
            current_view: AppView::Launch, // Default to launch for testing
            volume_bot_initial_buy_sol_input: "0.01".to_string(), // Initialize new field
            atomic_buy_task_handle: None, // Initialize the new task handle field
            create_token_name: String::new(),
            create_token_symbol: String::new(),
            create_token_description: String::new(),
            create_token_image_path: String::new(),
            create_dev_buy_amount_tokens: 1_000_000.0,
            create_token_decimals: 6,
            create_jito_bundle: false,
            create_zombie_amount: 0,

            // Wallet Gen State
            sim_wallet_gen_in_progress: false,
            sim_wallet_gen_status_sender,
            sim_wallet_gen_status_receiver,

            // buy_mint, buy_sol_amount, etc. fields removed from initialization.
            sell_mint: String::new(), // Keep this for the target mint input
            // Remove the actual initialization of the old fields:
            // sell_percentage: 100.0, // REMOVE
            // sell_wallets: String::new(), // REMOVE
            // sell_all: false, // REMOVE
            // sell_all_zombies: false, // REMOVE
            sell_slippage_percent: 10.0, // Default slippage 10%
            sell_priority_fee_lamports: 100_000, // Default priority fee 100k lamports
    


            // Initialize Pump view state
            pump_mint: String::new(),
            pump_buy_amount_sol: 0.01, // Default buy amount
            pump_sell_threshold_sol: 0.005, // Default sell threshold
            pump_slippage_percent: 10.0, // Default slippage 10%
            pump_priority_fee_lamports: 100_000, // Default priority fee 100k lamports
            pump_jito_tip_sol: 0.0, // Default Jito tip
            pump_lookup_table_address: String::new(),
            pump_private_key_string: String::new(),
            pump_use_jito_bundle: true, // Default to true
            pump_is_running: Arc::new(AtomicBool::new(false)), // Initialize toggle to off
            pump_task_handle: None, // Initialize task handle to None

            // Removed redundant fields initialization
            wallets: wallets_for_display,
            loaded_wallet_data: loaded_data_vec,
            wallet_load_error,
            balances_loading: false,
            disperse_in_progress: false, // Initialize disperse loading flag
            gather_in_progress: false,    // Initialize gather flags
            gather_tasks_expected: 0,
            gather_tasks_completed: 0,
            balance_tasks_expected: 0,
            balance_tasks_completed: 0,

            // Initialize original Simulation state
            sim_token_mint_orig: String::new(),
            sim_num_wallets_orig: 20, // Default
            sim_wallets_file_orig: "sim_wallets.json".to_string(), // Default filename
            sim_initial_sol_orig: 0.05, // Default
            sim_max_steps_orig: 50, // Default
            sim_slippage_bps_orig: 1500, // Default (2%) - Increased default slippage

            // Initialize Monitor state
            monitor_active: Arc::new(AtomicBool::new(false)),
            monitor_task_handle: None,
            monitor_balances: Vec::new(),
            monitor_prices: None,
            monitor_update_sender,
            monitor_update_receiver,

            sim_in_progress_orig: false,
            sim_log_messages_orig: Vec::new(),
            sim_task_handle_orig: None,
            sim_status_sender_orig, // Move sender
            sim_status_receiver_orig, // Move receiver

            balance_fetch_sender,
            balance_fetch_receiver,
            disperse_result_sender,
// SOL Consolidation State (Original Sim)
            sim_consolidate_sol_in_progress_orig: false,
            sim_consolidate_sol_status_sender_orig,
            sim_consolidate_sol_status_receiver_orig,
            disperse_result_receiver,
            gather_result_sender,         // Store gather sender
            gather_result_receiver,       // Store gather receiver
            last_operation_result: None,
            check_view_target_mint: String::new(), // Initialize empty
            disperse_sol_amount: 0.01,     // Default disperse amount
            // Removed parent_wallet_address init
            // Removed parent_wallet_sol_balance init
            // Removed minter_wallet_address init
            // Removed minter_wallet_sol_balance init
            // Removed minter_wallet_token_balance init
            alt_precalc_result_sender,
            alt_precalc_result_receiver,
            // Update stored channel senders/receivers
            alt_creation_status_sender,
            alt_creation_status_receiver,
            // Update stored channel senders/receivers
            sell_status_sender,
            sell_status_receiver,
            sell_log_messages: Vec::new(),
            sell_mass_reverse_in_progress: false,
            sell_gather_and_sell_in_progress: false,
            // --- Atomic Buy View State Init ---
            atomic_buy_mint_address: String::new(),
            atomic_buy_slippage_bps: 200, // Default 2% (200 bps)
            atomic_buy_priority_fee_lamports_per_tx: 100_000, // Default 0.0001 SOL per tx in bundle
            atomic_buy_alt_address: String::new(),
            atomic_buy_in_progress: false,
            atomic_buy_status_sender, // Already created above
            atomic_buy_status_receiver, // Already created above
            atomic_buy_log_messages: Vec::new(),
            atomic_buy_jito_tip_sol: 0.0001, // Default Jito tip for atomic buy
    
            // Initialize Pump coordinated state
            pump_status_sender,
            pump_status_receiver,
            pump_log_messages: Vec::new(),
            pump_in_progress: false,

            // Initialize Transfers state
            transfer_recipient_address: String::new(),
            transfer_sol_amount: 0.01, // Default amount
            transfer_selected_sender: None,
            transfer_in_progress: false,
            transfer_status_sender,
            transfer_status_receiver,
            transfer_log_messages: Vec::new(),

            // Update stored channel senders/receivers
            launch_status_sender,
            launch_status_receiver,
            alt_deactivate_status_sender, // Add this line
            alt_deactivate_status_receiver, // Add this line
            launch_log_messages: Vec::new(), // Initialize log messages vector
            launch_token_name: String::new(),
            launch_token_symbol: String::new(),
            launch_token_description: String::new(),
            launch_token_image_url: String::new(),
            launch_dev_buy_sol: 0.0,
            launch_zombie_buy_sol: 0.0,
            // Removed launch_slippage_bps initialization
            // Removed launch_priority_fee_microlamports initialization
            launch_alt_address: String::new(),
            launch_mint_keypair_path: String::new(),
            launch_in_progress: false,
            // Removed launch_num_zombie_buys initialization
            launch_use_jito_bundle: true, // <-- Initialize field (default to true?)
            launch_twitter: String::new(), // <-- Initialize Twitter
            launch_telegram: String::new(), // <-- Initialize Telegram
            launch_website: String::new(), // <-- Initialize Website
            // Removed launch_jito_tip_sol initialization
            launch_simulate_only: false, // <-- Initialize Simulate Only
            alt_view_status: "Ready.".to_string(),
            alt_log_messages: Vec::new(), // Initialize log messages vector
            alt_generated_mint_pubkey: None,
            alt_precalc_addresses: Vec::new(),
            alt_address: None,
            alt_creation_in_progress: false,
            alt_precalc_in_progress: false,
            available_alt_addresses,
            available_mint_keypairs,
// Initialize Volume bot State
            sim_token_mint: String::new(),
            sim_max_cycles: 10, // Default cycles
            sim_slippage_bps: 50, // Default 0.5%
            sim_in_progress: false,
            sim_log_messages: Vec::new(),
            sim_status_sender,
            sim_status_receiver,
            sim_task_handle: None,
            start_volume_bot_request: None,
            start_fetch_balances_request: None,
            start_load_volume_wallets_request: None,
            start_distribute_total_sol_request: None,
            start_fund_volume_wallets_request: None,
            start_gather_all_funds_request: None, // Initialize new field
            volume_bot_source_wallets_file: String::new(),
            volume_bot_num_wallets_to_generate: 10, // Default to 10 wallets
            volume_bot_wallets_generated: false,
            volume_bot_generation_in_progress: false,
            volume_bot_wallets: Vec::new(),
            last_generated_volume_wallets_file: None,
            volume_bot_wallet_gen_status_sender,
            volume_bot_wallet_gen_status_receiver,
            volume_bot_wallets_sender,
            volume_bot_wallets_receiver,
            volume_bot_funding_per_wallet_sol: 0.01, // Default funding per wallet
            volume_bot_funding_in_progress: false,
            volume_bot_funding_status_sender,
            volume_bot_funding_status_receiver,
            volume_bot_total_sol_to_distribute_input: "0.1".to_string(),
            volume_bot_funding_source_private_key_input: String::new(),
            volume_bot_funding_log_messages: Vec::new(),
            volume_bot_wallet_display_infos: Vec::new(), // <-- INITIALIZE: Empty Vec for display infos
            settings_feedback: None, // Initialize the new field
            settings_generate_count: 1, // Initialize count to 1
alt_deactivation_in_progress: false,
            alt_deactivation_status_message: "ALT Deactivation Idle".to_string(),
parent_sol_balance_display: None,
            total_sol_balance_display: None,
        };

        // Load dynamic options after initial struct creation
        app.load_available_alts();
        app.load_available_mint_keypairs();
    
        app // Return the fully initialized app
    }

    // --- Load Available ALTs ---
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
                        Err(e) => {
                            log::warn!("File {} contains invalid pubkey '{}': {}", ALT_FILE, alt_addr_str, e);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("ALT file {} not found. No ALT loaded.", ALT_FILE);
            }
            Err(e) => {
                log::error!("Failed to read ALT file {}: {}", ALT_FILE, e);
            }
        }
    }

    // --- Load Available Mint Keypairs ---
    fn load_available_mint_keypairs(&mut self) {
        self.available_mint_keypairs.clear();
        log::info!("Scanning current directory for potential mint keypair .json files...");

        match std::fs::read_dir(".") { // Read current directory
            Ok(entries) => {
                for entry_result in entries {
                    if let Ok(entry) = entry_result {
                        let path = entry.path();
                        if path.is_file() && path.extension().map_or(false, |ext| ext == "json") {
                            let path_str = path.to_string_lossy().to_string();
                            // Basic validation: try to read and parse as keypair bytes
                            match std::fs::read_to_string(&path) {
                                Ok(content) => {
                                    match serde_json::from_str::<Vec<u8>>(&content) {
                                        Ok(bytes) if bytes.len() == 64 => {
                                            // Attempt to create Keypair to ensure validity
                                            if Keypair::from_bytes(&bytes).is_ok() {
                                                log::info!("  Found valid keypair file: {}", path_str);
                                                self.available_mint_keypairs.push(path_str);
                                            } else {
                                                log::trace!("  Skipping file (not a valid keypair format): {}", path_str);
                                            }
                                        }
                                        _ => {
                                             log::trace!("  Skipping file (not Vec<u8> or wrong length): {}", path_str);
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::warn!("  Failed to read potential keypair file {}: {}", path_str, e);
                                }
                            }
                        }
                    }
                }
                 log::info!("Found {} potential keypair files.", self.available_mint_keypairs.len());
            }
            Err(e) => {
                log::error!("Failed to read current directory to scan for keypairs: {}", e);
            }
        }
    }

    // --- View Rendering Functions ---

    // Placeholder for rendering the Create Token view
    fn show_create_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("🚀 Create New Pump.fun Token");
        ui.separator();
        ui.add_space(10.0);

        egui::Grid::new("create_grid")
            .num_columns(2)
            .spacing([40.0, 8.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut self.create_token_name);
                ui.end_row();

                ui.label("Symbol:");
                ui.text_edit_singleline(&mut self.create_token_symbol);
                ui.end_row();

                ui.label("Description:");
                ui.text_edit_multiline(&mut self.create_token_description);
                ui.end_row();

                ui.label("Image Path/URL:");
                ui.horizontal(|ui| {
                    // Use fill_width() to make the text edit take available space
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                         if ui.button("Browse...").clicked() {
                            // TODO: Implement file picker logic using rfd or similar
                            log::warn!("File picker not implemented yet.");
                             self.last_operation_result = Some(Err("File picker not implemented.".to_string()));
                        }
                         ui.add_sized(ui.available_size(), egui::TextEdit::singleline(&mut self.create_token_image_path));
                    });
                });
                ui.end_row();


                ui.label("Decimals:");
                ui.add(
                    egui::DragValue::new(&mut self.create_token_decimals)
                        .range(0..=9)
                        .speed(1),
                );
                ui.end_row();

                ui.label("Dev Buy Amount (Tokens):");
                ui.add(
                    egui::DragValue::new(&mut self.create_dev_buy_amount_tokens)
                        .speed(1000.0)
                        .range(0.0..=f64::MAX) // Allow large numbers
                        .prefix("◎ ") // Placeholder, adjust if needed
                );
                ui.end_row();

                ui.label("Use Jito Bundle:");
                ui.checkbox(&mut self.create_jito_bundle, "Send via Jito");
                ui.end_row();

                ui.label("Zombie Buy Amount (SOL):");
                ui.add(
                    egui::DragValue::new(&mut self.create_zombie_amount)
                        .speed(1.0)
                        .range(0..=u64::MAX) // Allow large numbers
                        .prefix("◎ ") // Placeholder, adjust if needed
                );
                ui.end_row();
            });

        ui.add_space(20.0);

        ui.vertical_centered(|ui| {
            let create_button = egui::Button::new("🚀 Create Token")
                .min_size(egui::vec2(150.0, 30.0));
            // Add validation logic here if needed
            if ui.add(create_button).clicked() {
                log::info!("Create Token button clicked (Not Implemented)");
                self.last_operation_result = Some(Err("Create Token functionality not implemented yet.".to_string()));
                // TODO: Spawn create task
            }
        });
    }

    // Placeholder for rendering the Sell Token view
    fn show_sell_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("💸 Sell Tokens Dashboard"); // Added icon
        ui.separator();

        // Card for Sell Inputs
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow).show(ui, |ui_card_input| {
            // --- Mint Input & Refresh ---
            ui_card_input.horizontal(|ui_card_inner| {
                ui_card_inner.label("Mint Address:");
                ui_card_inner.text_edit_singleline(&mut self.sell_mint);
                if ui_card_inner.button("🔄 Refresh Balances").clicked() {
                    if !self.sell_mint.is_empty() {
                        self.trigger_balance_fetch(Some(self.sell_mint.clone()));
                    } else {
                        self.trigger_balance_fetch(None);
                        self.last_operation_result = Some(Err("Enter Mint Address to see token balances.".to_string()));
                    }
                }
                if self.balances_loading {
                    ui_card_inner.spinner();
                }
            });

            // Inputs for Slippage and Priority Fee
            ui_card_input.horizontal(|ui_card_inner| {
                ui_card_inner.label("Slippage (%):");
                ui_card_inner.add(egui::DragValue::new(&mut self.sell_slippage_percent).speed(0.1).range(0.0..=50.0).suffix("%"));
                ui_card_inner.label("Priority Fee (lamports):");
                ui_card_inner.add(egui::DragValue::new(&mut self.sell_priority_fee_lamports).speed(1000).range(0..=10_000_000));
            });
        });

        ui.add_space(10.0); // Spacing between cards

        // Card for Wallet Dashboard
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow).show(ui, |ui_card_dashboard| {
            ui_card_dashboard.label("Wallets:");
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .auto_shrink([false, false])
                .show(ui_card_dashboard, |ui_scroll| {
                    egui::Grid::new("sell_wallet_grid")
                        .num_columns(5)
                        .spacing([10.0, 4.0])
                        .striped(true)
                        .show(ui_scroll, |ui_grid| {
                            ui_grid.label("Address").on_hover_text("Wallet Public Key");
                            ui_grid.label("SOL").on_hover_text("SOL Balance");
                            ui_grid.label("Tokens").on_hover_text("Target Mint Token Balance");
                            ui_grid.label("Sell %").on_hover_text("Percentage of tokens to sell");
                            ui_grid.label("Sell Amt").on_hover_text("Specific amount of tokens to sell (raw UI amount)");
                            ui_grid.label("Actions");
                            ui_grid.end_row();

                            let wallet_count = self.wallets.len();
                            for i in 0..wallet_count {
                                let address = self.wallets[i].address.clone();
                                let address_short = format!("{}...{}", &address[..6], &address[address.len()-4..]);
                                let sol_balance_display = self.wallets[i].sol_balance.map_or("-".to_string(), |bal| format!("{:.4}", bal));
                                let token_balance_display = self.wallets[i].target_mint_balance.as_deref().unwrap_or("-").to_string();
                                let is_selling = self.wallets[i].is_selling;

                                ui_grid.label(&address_short).on_hover_text(&address);
                                ui_grid.label(sol_balance_display);
                                ui_grid.label(token_balance_display);

                                let wallet_info = &mut self.wallets[i];
                                ui_grid.add(egui::TextEdit::singleline(&mut wallet_info.sell_percentage_input).desired_width(60.0)); // Slightly wider input
                                ui_grid.add(egui::TextEdit::singleline(&mut wallet_info.sell_amount_input).desired_width(90.0)); // Slightly wider input

                                ui_grid.horizontal(|ui_actions| {
                                    let sell_enabled = !is_selling && !self.sell_mass_reverse_in_progress && !self.sell_gather_and_sell_in_progress;
                                    let button_size = egui::vec2(80.0, 28.0);

                                    if ui_actions.add_enabled(sell_enabled, egui::Button::new(egui::RichText::new("Sell %").size(13.0)).min_size(button_size)).on_hover_text("Sell specified percentage from this wallet").clicked() {
                                        self.trigger_individual_sell(address.clone(), true);
                                    }
                                    ui_actions.add_space(4.0);
                                    if ui_actions.add_enabled(sell_enabled, egui::Button::new(egui::RichText::new("Sell Amt").size(13.0)).min_size(button_size)).on_hover_text("Sell specified token amount from this wallet").clicked() {
                                        self.trigger_individual_sell(address.clone(), false);
                                    }
                                    if is_selling {
                                        ui_actions.add_space(6.0);
                                        ui_actions.spinner();
                                    }
                                });
                                ui_grid.end_row();
                            }
                        });
                });
        });

        ui.add_space(10.0); // Spacing between cards

        // Card for Logs and Mass Actions
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow).show(ui, |ui_card_logs_actions| {
            ui_card_logs_actions.vertical(|ui_vertical_section| {
                ui_vertical_section.heading("Sell Logs");
                egui::ScrollArea::vertical().max_height(150.0).stick_to_bottom(true).show(ui_vertical_section, |ui_log_scroll| {
                    let logs = self.sell_log_messages.clone();
                    for msg in logs {
                        ui_log_scroll.label(msg);
                    }
                });
                ui_vertical_section.add_space(10.0);
                ui_vertical_section.separator();
                ui_vertical_section.heading("Mass Sell Actions");
                ui_vertical_section.horizontal(|ui_mass_actions| {
                    let mass_actions_enabled = !self.sell_mass_reverse_in_progress && !self.sell_gather_and_sell_in_progress && self.wallets.iter().all(|w| !w.is_selling);
                    let mass_button_size = egui::vec2(180.0, 35.0);

                    if ui_mass_actions.add_enabled(mass_actions_enabled, egui::Button::new(egui::RichText::new("Mass Sell (Reverse)").size(14.0)).min_size(mass_button_size)).on_hover_text("Sell 100% from each wallet, starting from the last loaded wallet to the first (primary).").clicked() {
                        self.trigger_mass_sell_reverse();
                    }
                    if self.sell_mass_reverse_in_progress {
                        ui_mass_actions.add_space(5.0);
                        ui_mass_actions.spinner();
                    }
                    ui_mass_actions.add_space(15.0); // Adjusted spacing
                    if ui_mass_actions.add_enabled(mass_actions_enabled, egui::Button::new(egui::RichText::new("Gather All & Sell").size(14.0)).min_size(mass_button_size)).on_hover_text("Send all tokens from zombie wallets to the primary wallet, then sell 100% from the primary wallet.").clicked() {
                        self.trigger_gather_and_sell();
                    }
                    if self.sell_gather_and_sell_in_progress {
                        ui_mass_actions.add_space(5.0);
                        ui_mass_actions.spinner();
                    }
                });
            });
        });
    }

    // Placeholder for rendering the Settings view
    fn show_settings_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("🔩 Application Settings"); // Changed icon
        ui.separator();
        ui.add_space(10.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            // Section: RPC & Connection Settings
            egui::CollapsingHeader::new(egui::RichText::new("📡 RPC & Connection").size(16.0)).default_open(true).show(ui, |ui| {
                ui.add_space(5.0);
                egui::Grid::new("rpc_settings_grid")
                    .num_columns(2)
                    .spacing([20.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("Solana RPC URL:");
                        ui.text_edit_singleline(&mut self.app_settings.solana_rpc_url);
                        ui.end_row();

                        ui.label("Solana WS URL:");
                        ui.text_edit_singleline(&mut self.app_settings.solana_ws_url);
                        ui.end_row();

                        ui.label("Commitment Level:");
                        egui::ComboBox::from_id_source("commitment_combo")
                            .selected_text(self.app_settings.commitment.to_uppercase())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.app_settings.commitment, "processed".to_string(), "Processed");
                                ui.selectable_value(&mut self.app_settings.commitment, "confirmed".to_string(), "Confirmed");
                                ui.selectable_value(&mut self.app_settings.commitment, "finalized".to_string(), "Finalized");
                            });
                        ui.end_row();
                    });
                ui.add_space(10.0);
            });

            // Section: Jito Configuration
            egui::CollapsingHeader::new(egui::RichText::new("🔷 Jito Configuration").size(16.0)).default_open(true).show(ui, |ui| {
                ui.add_space(5.0);
                egui::Grid::new("jito_settings_grid")
                    .num_columns(2)
                    .spacing([20.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("Jito Block Engine Endpoint:");
                        let selected_url = self.app_settings.selected_jito_block_engine_url.clone();
                        egui::ComboBox::from_id_source("jito_endpoint_combo")
                            .selected_text(selected_url.split('/').nth(2).unwrap_or(&selected_url))
                            .show_ui(ui, |ui| {
                                for endpoint in crate::models::settings::JITO_BLOCK_ENGINE_ENDPOINTS.iter() {
                                    ui.selectable_value(
                                        &mut self.app_settings.selected_jito_block_engine_url,
                                        endpoint.to_string(),
                                        endpoint.split('/').nth(2).unwrap_or(endpoint)
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("Jito Tip Account (Pubkey - General):");
                        ui.text_edit_singleline(&mut self.app_settings.jito_tip_account);
                        ui.end_row();

                        ui.label("Jito Tip Amount (SOL - General):");
                        ui.add(egui::DragValue::new(&mut self.app_settings.jito_tip_amount_sol).speed(0.00001).max_decimals(5).range(0.0..=1.0).prefix("◎ "));
                        ui.end_row();

                        ui.label("Volume Bot Jito Specifics:");
                        ui.vertical(|ui_inner| {
                            ui_inner.checkbox(&mut self.app_settings.use_jito_for_volume_bot, "Use Jito for Volume Bot Transactions");
                            ui_inner.label("Jito Tip Account Pubkey (Volume Bot):");
                            ui_inner.text_edit_singleline(&mut self.app_settings.jito_tip_account_pk_str_volume_bot);
                            ui_inner.label("Jito Tip Amount (Lamports - Volume Bot):");
                            ui_inner.add(egui::DragValue::new(&mut self.app_settings.jito_tip_lamports_volume_bot).speed(100.0).range(0..=1_000_000));
                            ui_inner.label(format!("(Equivalent to ◎{})", lamports_to_sol(self.app_settings.jito_tip_lamports_volume_bot)));
                        });
                        ui.end_row();
                    });
                ui.add_space(10.0);
            });

            // Section: Wallet & Key Management
            egui::CollapsingHeader::new(egui::RichText::new("🔑 Wallet & Key Management").size(16.0)).default_open(true).show(ui, |ui| {
                ui.add_space(5.0);
                egui::Grid::new("wallet_keys_grid")
                    .num_columns(2)
                    .spacing([20.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("Keys File Path:");
                        ui.horizontal(|ui_inner| {
                            ui_inner.label(&self.app_settings.keys_path).on_hover_text(&self.app_settings.keys_path);
                            if ui_inner.button("Select File").clicked() {
                                if let Some(path) = FileDialog::new().add_filter("JSON", &["json"]).set_directory(".").pick_file() {
                                    self.app_settings.keys_path = path.to_string_lossy().to_string();
                                    self.settings_feedback = Some("New keys file selected. Click 'Load Selected Keys' or 'Save Settings' to apply.".to_string());
                                }
                            }
                            if ui_inner.button("Load Keys").clicked() {
                                self.load_wallets_from_settings_path();
                            }
                        });
                        ui.end_row();

                        ui.label("Generate New Keys:");
                        ui.horizontal(|ui_inner| {
                            ui_inner.add(egui::DragValue::new(&mut self.settings_generate_count).speed(1.0).range(1..=1000).prefix("Count: "));
                            if ui_inner.button("Generate & Save").clicked() {
                                match key_utils::generate_and_save_keypairs(Path::new("."), self.settings_generate_count) {
                                    Ok(new_path) => {
                                        let new_path_str = new_path.file_name().unwrap_or_default().to_string_lossy().to_string();
                                        self.settings_feedback = Some(format!("Generated {} keys: {}. Select and load if desired.", self.settings_generate_count, new_path_str));
                                    }
                                    Err(e) => { self.settings_feedback = Some(format!("Error generating keys: {}", e)); error!("Error generating keys: {}", e); }
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Parent Wallet Private Key:");
                        ui.add(egui::TextEdit::singleline(&mut self.app_settings.parent_wallet_private_key).password(true).desired_width(ui.available_width()));
                        ui.end_row();

                        ui.label("Dev Wallet Private Key:");
                        ui.horizontal(|ui_inner| {
                            let available_width = ui_inner.available_width(); // Calculate width before the problematic use
                            ui_inner.add(egui::TextEdit::singleline(&mut self.app_settings.dev_wallet_private_key).password(true).desired_width(available_width * 0.7));
                            if ui_inner.button("Generate New Dev Wallet").on_hover_text("Generates and saves a new dev wallet, then populates the key here.").clicked() {
                                match key_utils::generate_single_dev_wallet() {
                                    Ok((priv_key_bs58, pub_key_bs58, filename)) => {
                                        self.app_settings.dev_wallet_private_key = priv_key_bs58;
                                        self.settings_feedback = Some(format!("New Dev Wallet ({}) saved to {}. Save settings to persist.", pub_key_bs58, filename));
                                    }
                                    Err(e) => { self.settings_feedback = Some(format!("Error generating dev wallet: {}", e)); error!("Error generating dev wallet: {}", e);}
                                }
                            }
                        });
                        ui.end_row();
                    });
                ui.add_space(10.0);
            });

            // Section: Default Transaction Parameters
            egui::CollapsingHeader::new(egui::RichText::new("✈️ Transaction Defaults").size(16.0)).default_open(true).show(ui, |ui| {
                ui.add_space(5.0);
                egui::Grid::new("tx_defaults_grid")
                    .num_columns(2)
                    .spacing([20.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("Default Priority Fee (SOL):");
                        ui.add(egui::DragValue::new(&mut self.app_settings.default_priority_fee_sol).speed(0.00001).max_decimals(9).prefix("◎ "));
                        ui.end_row();

                        ui.label("Default Slippage (%):");
                        ui.add(egui::DragValue::new(&mut self.app_settings.default_slippage_percent).speed(0.1).range(0.0..=100.0).suffix("%"));
                        ui.end_row();
                    });
                ui.add_space(10.0);
            });

            // Section: API URLs
            egui::CollapsingHeader::new(egui::RichText::new("🔗 API URLs").size(16.0)).default_open(false).show(ui, |ui| {
                ui.add_space(5.0);
                egui::Grid::new("api_urls_grid")
                    .num_columns(2)
                    .spacing([20.0, 8.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("PumpFun API URL:");
                        ui.text_edit_singleline(&mut self.app_settings.pumpfun_api_url);
                        ui.end_row();

                        ui.label("PumpFun Portal API URL:");
                        ui.text_edit_singleline(&mut self.app_settings.pumpfun_portal_api_url);
                        ui.end_row();

                        ui.label("IPFS API URL:");
                        ui.text_edit_singleline(&mut self.app_settings.ipfs_api_url);
                        ui.end_row();
                    });
                ui.add_space(10.0);
            });
        }); // End of ScrollArea

        // Display feedback message if any
        if let Some(feedback) = &self.settings_feedback {
            ui.add_space(10.0);
            ui.label(feedback);
            // Optionally clear feedback after showing? Or keep until next action?
            // self.settings_feedback = None; // Let's keep it until the next action
        }

        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            if ui.button("💾 Save Settings").clicked() {
                log::info!("Save Settings button clicked.");
                match self.app_settings.save() {
                    Ok(_) => {
                        log::info!("Settings saved successfully.");
                        self.settings_feedback = Some("Settings saved successfully.".to_string());
                    }
                    Err(e) => {
                        log::error!("Failed to save settings: {}", e);
                        self.settings_feedback = Some(format!("Failed to save settings: {}", e));
                    }
                }
            }
        });
    }

        // --- Helper to load wallets based on settings path ---
        fn load_wallets_from_settings_path(&mut self) {
            let (loaded_data, display_wallets, error) =
                match crate::wallet::load_keys_from_file(Path::new(&self.app_settings.keys_path)) {
                    Ok(loaded_keys) => {
                        log::info!("Loaded {} wallets from {}", loaded_keys.wallets.len(), &self.app_settings.keys_path);
                        let mut wallet_infos_for_ui: Vec<WalletInfo> = loaded_keys.wallets.iter()
                            .enumerate()
                            .map(|(index, loaded_info)| {
                                let dev_wallet_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key);
                                let is_dev_wallet = dev_wallet_pubkey_str_opt.as_ref().map_or(false, |dpk| dpk == &loaded_info.public_key);

                                let parent_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                                let is_parent = parent_pubkey_str_opt.as_ref().map_or(false, |ppk| ppk == &loaded_info.public_key);

                                WalletInfo {
                                    address: loaded_info.public_key.clone(),
                                    is_primary: index == 0,
                                    is_dev_wallet,
                                    is_parent, // Add is_parent field here
                                    sol_balance: None, // Reset balances on reload
                                    target_mint_balance: None,
                                    error: None,
                                    is_loading: false,
                                    is_selected: false,
                                    sell_percentage_input: "100".to_string(),
                                    sell_amount_input: String::new(),
                                    is_selling: false,
                                    atomic_buy_sol_amount_input: "0.0".to_string(),
                                }
                            })
                            .collect();
    
                        // Add dev_wallet from settings if not present (same logic as in `new`)
                        if let Some(dev_wallet_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key) {
                            if !wallet_infos_for_ui.iter().any(|w| w.address == dev_wallet_pubkey_str) {
                                log::info!("Adding Dev wallet ({}) from settings to display list.", dev_wallet_pubkey_str);
                                // Check if this added dev_wallet is ALSO the parent
                                let is_parent = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key)
                                    .map_or(false, |ppk| ppk == dev_wallet_pubkey_str);
                                wallet_infos_for_ui.push(WalletInfo { // Add the actual WalletInfo struct
                                    address: dev_wallet_pubkey_str,
                                    is_primary: false,
                                    is_dev_wallet: true,
                                    is_parent, // Add is_parent field here
                                    sol_balance: None,
                                    target_mint_balance: None,
                                    error: None,
                                    is_loading: false,
                                    is_selected: false,
                                    sell_percentage_input: "100".to_string(),
                                    sell_amount_input: String::new(),
                                    is_selling: false,
                                    atomic_buy_sol_amount_input: "0.0".to_string(),
                                });
                            }
                        }

                        // Add parent from settings if not present (and not already added as dev_wallet)
                        if let Some(parent_pubkey_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key) {
                             if !wallet_infos_for_ui.iter().any(|w| w.address == parent_pubkey_str) {
                                log::info!("Adding Parent wallet ({}) from settings to display list.", parent_pubkey_str);
                                wallet_infos_for_ui.push(WalletInfo {
                                    address: parent_pubkey_str,
                                    is_primary: false,
                                    is_dev_wallet: false, // Cannot be dev_wallet if added here
                                    is_parent: true,  // Definitely parent if added here
                                    sol_balance: None,
                                    target_mint_balance: None,
                                    error: None,
                                    is_loading: false,
                                    is_selected: false,
                                    sell_percentage_input: "100".to_string(),
                                    sell_amount_input: String::new(),
                                    is_selling: false,
                                    atomic_buy_sol_amount_input: "0.0".to_string(),
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
    
            self.loaded_wallet_data = loaded_data;
            self.wallets = display_wallets;
            self.wallet_load_error = error;
            // Optionally trigger balance fetch after reload
            // self.trigger_balance_fetch(None);
        }
    
    // --- Check View (Wallet Balances) ---
    fn show_check_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("🔍 Check Wallet Balances");
        ui.separator();

        // Card for Inputs
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow).show(ui, |ui_card_input| {
            ui_card_input.horizontal(|ui_card_inner| {
                ui_card_inner.label("Target Mint (Optional):");
                ui_card_inner.text_edit_singleline(&mut self.check_view_target_mint);
                if ui_card_inner.button("🔄 Fetch Balances").clicked() && !self.balances_loading {
                     let mint_to_check = self.check_view_target_mint.trim();
                     if mint_to_check.is_empty() {
                         self.trigger_balance_fetch(None);
                     } else {
                         self.trigger_balance_fetch(Some(mint_to_check.to_string()));
                     }
                }
                if self.balances_loading {
                    ui_card_inner.add(egui::Spinner::new()).on_hover_text("Fetching balances...");
                }
            });
        });

        ui.add_space(10.0); // Spacing between cards

        // Card for Totals
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow).show(ui, |ui_card_totals| {
            let (total_sol, total_tokens_option) = self.calculate_totals();
            ui_card_totals.horizontal(|ui_card_inner| {
                ui_card_inner.strong("Total SOL (Zombies):");
                ui_card_inner.monospace(format!("◎ {:.6}", total_sol));
                if let Some(total_tokens) = total_tokens_option {
                     ui_card_inner.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_layout| {
                         ui_layout.monospace(format!("Σ {:.2}", total_tokens));
                         ui_layout.strong("Total Tokens:");
                     });
                }
            });
        });

        ui.add_space(10.0); // Spacing between cards

        // Card for Wallet List
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow).show(ui, |ui_card_list| {
            if let Some(err) = &self.wallet_load_error {
                ui_card_list.colored_label(ui_card_list.style().visuals.error_fg_color, err);
            } else {
                 egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui_card_list, |ui_scroll| {
                     egui::Grid::new("wallet_grid_check") // Unique ID for grid
                        .num_columns(3)
                        .spacing([10.0, 4.0])
                        .striped(true)
                        .show(ui_scroll, |ui_grid| {
                            ui_grid.strong("Wallet Address");
                            ui_grid.strong("SOL Balance");
                            ui_grid.strong("Token Balance");
                            ui_grid.end_row();

                            let mut zombie_counter = 0;
                            for wallet_info in &self.wallets {
                                let wallet_label = if wallet_info.is_parent {
                                    "👪 Parent (Settings)".to_string()
                                } else if wallet_info.is_dev_wallet {
                                    "⚙️ Dev (Settings)".to_string()
                                } else if wallet_info.is_primary {
                                    if wallet_info.is_parent { "🔑 Primary / 👪 Parent".to_string() }
                                    else if wallet_info.is_dev_wallet { "🔑 Primary / ⚙️ Dev".to_string() }
                                    else { "🔑 Primary (keys.json)".to_string() }
                                } else {
                                    let zombie_label = format!("🧟 Zombie {}", zombie_counter);
                                    zombie_counter += 1;
                                    zombie_label
                                };

                                ui_grid.label(wallet_label);
                                ui_grid.monospace(&wallet_info.address);

                                ui_grid.horizontal(|ui_balance_sol| {
                                    if wallet_info.is_loading {
                                        ui_balance_sol.spinner();
                                    } else {
                                        match &wallet_info.sol_balance {
                                            Some(balance) => { ui_balance_sol.monospace(format!("◎ {:.6}", balance)); }
                                            None => { ui_balance_sol.monospace("-"); }
                                        }
                                    }
                                });

                                ui_grid.horizontal(|ui_balance_token| {
                                    if wallet_info.is_loading {
                                        ui_balance_token.spinner();
                                    } else {
                                        match &wallet_info.target_mint_balance {
                                            Some(balance_str) if !balance_str.is_empty() => { ui_balance_token.monospace(balance_str); }
                                            _ => { ui_balance_token.monospace("-"); }
                                        }
                                    }
                                    if let Some(err_msg) = &wallet_info.error {
                                        ui_balance_token.label("⚠️").on_hover_text(err_msg);
                                    }
                                });
                                ui_grid.end_row();
                            }
                        });
                 });
            }
        });

        ui.add_space(10.0);

        // Card for Balances Plot
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow).show(ui, |ui_plot_card| {
            ui_plot_card.strong("Zombie Wallet SOL Balances Plot:");
            ui_plot_card.add_space(5.0);

            let zombie_wallets_balances: Vec<f64> = self.wallets.iter()
                .filter(|w| !w.is_primary && !w.is_dev_wallet && !w.is_parent)
                .filter_map(|w| w.sol_balance)
                .collect();

            if zombie_wallets_balances.is_empty() {
                ui_plot_card.label("No balance data to plot for zombie wallets.");
            } else {
                let plot_points = PlotPoints::from_ys_f64(&zombie_wallets_balances);
                let line = Line::new(plot_points).name("SOL Balance").color(APP_ACCENT_GREEN);

                Plot::new("zombie_balances_plot")
                    .height(200.0)
                    .show_x(true)
                    .show_y(true)
                    .legend(egui_plot::Legend::default())
                    .show(ui_plot_card, |plot_ui| {
                        plot_ui.line(line);
                    });
            }
        });
        ui.add_space(10.0); 

     }

    // Helper to calculate totals for Zombies only
    fn calculate_totals(&self) -> (f64, Option<f64>) {
        let mut total_sol = 0.0;
        let mut current_total_tokens = 0.0;
        let mut has_token_balances = false;

        // Iterate only over wallets that are NOT dev_wallet and NOT primary
        for wallet_info in self.wallets.iter().filter(|w| !w.is_dev_wallet && !w.is_primary) {
            if let Some(balance) = wallet_info.sol_balance {
                total_sol += balance;
            }
            if let Some(balance_str) = &wallet_info.target_mint_balance {
                 match balance_str.replace(',', "").parse::<f64>() { // Handle potential commas
                    Ok(val) => {
                        current_total_tokens += val;
                        has_token_balances = true;
                    }
                    Err(_) => { /* Ignore parse errors for total */ }
                }
            }
        }

        let total_tokens_option = if has_token_balances { Some(current_total_tokens) } else { None };
        (total_sol, total_tokens_option) // This is now explicitly the "Zombie Total"
    }


    // --- Status Bar ---
    fn show_status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar_panel") // Renamed ID for clarity
            .show(ctx, |ui| {
                // Use a bit more padding inside the status bar
                ui.style_mut().spacing.item_spacing = egui::vec2(8.0, ui.style().spacing.item_spacing.y);
                let status_bar_height = ui.available_height();
                let layout = egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(false);

                ui.allocate_ui_with_layout(ui.available_size_before_wrap(), layout, |ui| {
                    // Left side: Balances
                    let parent_icon = "👪";
                    let total_icon = "Σ";
                    let parent_display = self.parent_sol_balance_display
                        .map_or_else(|| format!("{} Parent: N/A", parent_icon), |bal| format!("{} Parent: {:.4} SOL", parent_icon, bal));
                    ui.label(egui::RichText::new(parent_display).size(13.0));
                    ui.separator();

                    let total_display = self.total_sol_balance_display
                        .map_or_else(|| format!("{} Total: N/A", total_icon), |bal| format!("{} Total: {:.4} SOL", total_icon, bal));
                    ui.label(egui::RichText::new(total_display).size(13.0));
                    ui.separator();

                    // Fetch error color before the closure that causes borrowing issues
                    let error_color = ui.style().visuals.error_fg_color;

                    // Middle: Last operation result (allow it to take more space)
                    ui.allocate_ui_with_layout(egui::vec2(ui.available_width() * 0.4, status_bar_height), egui::Layout::centered_and_justified(egui::Direction::LeftToRight), |ui_op| {
                        match &self.last_operation_result {
                            Some(Ok(msg)) => {
                                ui_op.colored_label(APP_ACCENT_GREEN, egui::RichText::new(format!("✅ {}", msg)).size(13.0));
                            }
                            Some(Err(err)) => {
                                ui_op.colored_label(error_color, egui::RichText::new(format!("❌ {}", err)).size(13.0).strong());
                            }
                            None => {
                                ui_op.label(egui::RichText::new("✨ Ready").size(13.0));
                            }
                        }
                    });
                    

                    // Right side: Connection and Dev Wallet Info
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui_right| {
                        // Dev Wallet Info
                        let dev_icon = "🔩"; // Changed icon
                        if let Some(dev_pk_str) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key) {
                            if let Some(wallet) = self.wallets.iter().find(|w| w.address == dev_pk_str && w.is_dev_wallet) {
                                let balance_str: String = if wallet.is_loading {
                                    "Loading...".to_string()
                                } else if let Some(balance) = wallet.sol_balance {
                                    format!("{:.4} SOL", balance)
                                } else if wallet.error.is_some() {
                                    "Error".to_string()
                                } else {
                                    "N/A".to_string()
                                };
                                ui_right.label(egui::RichText::new(format!("{} Dev: {}... ({})", dev_icon, &wallet.address[..4], balance_str)).size(13.0)).on_hover_text(&wallet.address);
                            } else {
                                ui_right.label(egui::RichText::new(format!("{} Dev: {}... (N/A)", dev_icon, &dev_pk_str[..4])).size(13.0)).on_hover_text(&dev_pk_str);
                            }
                        } else {
                            ui_right.label(egui::RichText::new(format!("{} Dev: Not Set", dev_icon)).size(13.0));
                        }
                        ui_right.separator();
                        
                        // Jito Info
                        let jito_domain = self.app_settings.selected_jito_block_engine_url.split('/').nth(2).unwrap_or("N/A");
                        ui_right.label(egui::RichText::new(format!("🔷 Jito: {}", jito_domain)).size(13.0)).on_hover_text(&self.app_settings.selected_jito_block_engine_url);
                        ui_right.separator();
                        
                        // RPC Info
                        let rpc_domain = self.app_settings.solana_rpc_url.split('/').nth(2).unwrap_or("N/A");
                        ui_right.label(egui::RichText::new(format!("🔗 RPC: {}", rpc_domain)).size(13.0)).on_hover_text(&self.app_settings.solana_rpc_url);
                    });
                });
            });
    }

        // --- Trigger Balance Fetch ---
        fn trigger_balance_fetch(&mut self, target_mint_override: Option<String>) {
            if self.balances_loading {
                log::warn!("Balance fetch already in progress.");
                return;
            }
            
            // Clear previous results and errors
            for wallet in self.wallets.iter_mut() {
                wallet.sol_balance = None;
                wallet.target_mint_balance = None;
                wallet.error = None;
                wallet.is_loading = true; // Set loading state for each wallet
            }

            // Collect all addresses to check
            let mut addresses_to_check: Vec<String> = self.wallets.iter().map(|w| w.address.clone()).collect();

            // Derive parent pubkey from settings and add if needed
            if let Some(parent_addr) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key) {
                 if !addresses_to_check.contains(&parent_addr) {
                    log::debug!("Adding parent wallet {} to balance check list", parent_addr);
                    addresses_to_check.push(parent_addr);
                }
            }

            // Derive minter pubkey from settings and add if needed
            if let Some(minter_addr) = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.dev_wallet_private_key) {
                 if !addresses_to_check.contains(&minter_addr) {
                    log::debug!("Adding dev wallet {} to balance check list", minter_addr);
                    addresses_to_check.push(minter_addr);
                }
            }

            // Remove duplicates just in case (e.g., if parent/minter was also in self.wallets)
            addresses_to_check.sort();
            addresses_to_check.dedup();

            log::info!("Triggering balance fetch for {} unique wallets...", addresses_to_check.len());
            self.balances_loading = true; // Set loading true
            self.last_operation_result = Some(Ok("Fetching balances...".to_string()));
            
            let num_wallets_to_fetch = addresses_to_check.len();
            if num_wallets_to_fetch == 0 {
                log::warn!("No wallets loaded to fetch balances for.");
                self.balances_loading = false;
                self.last_operation_result = Some(Err("No wallets loaded.".to_string()));
                // Ensure counters are reset even if we return early
                self.balance_tasks_expected = 0;
                self.balance_tasks_completed = 0;
                return;
            }
            self.balance_tasks_expected = num_wallets_to_fetch;
            self.balance_tasks_completed = 0;

            // Clone necessary data for the task
            let rpc_url = self.app_settings.solana_rpc_url.clone();
            let target_mint = target_mint_override.unwrap_or_else(|| self.check_view_target_mint.trim().to_string());
            let sender = self.balance_fetch_sender.clone();
            
            // Keep track of how many results we expect
            // This simple counter might not be robust if tasks can fail silently before sending.
            // A more robust solution might involve JoinSet or similar.
            // For now, we'll rely on the task sending a result for each wallet.
            // We will set balances_loading to false in the main update loop when all expected results are received.
            // This requires adding fields to PumpFunApp to track expected and received counts.
            // Let's add `balance_tasks_expected` and `balance_tasks_completed` to PumpFunApp struct.
            // And modify the main update loop to check these.

            // For now, let's simplify: the task itself will set balances_loading to false via a channel
            // or we assume the UI thread will eventually get all messages.
            // The current structure of the UI update loop already processes all messages from balance_fetch_receiver.
            // The main issue is that the *outer* task completes too early.

            let balance_fetch_complete_sender = self.balance_fetch_sender.clone(); // Used to send a final "done" marker

            tokio::spawn(async move {
                Self::fetch_balances_task(rpc_url, addresses_to_check, target_mint, sender).await;
                // After fetch_balances_task completes (meaning all its internal operations are done),
                // send a special marker or handle completion.
                // For now, we assume the UI loop will correctly count individual wallet results.
                // The key is that fetch_balances_task now waits.
                // To explicitly signal completion from the task:
                // let _ = balance_fetch_complete_sender.send(FetchResult::Completed); // Requires adding Completed variant
            });
        }
// --- Balance Fetching Logic (Associated Function) ---
    pub(crate) async fn fetch_balances_task(
        rpc_url: String,
        wallets_to_check: Vec<String>, // Addresses as strings
        target_mint_str: String, // Target mint address as string, empty if none
        sender: UnboundedSender<FetchResult>,
        // _parent_wallet_address: Option<String>, // Parameter no longer used
    ) {
        let client = Arc::new(AsyncRpcClient::new(rpc_url));
        let target_mint_pubkey: Option<Pubkey> = if target_mint_str.is_empty() {
            None
        } else {
            Pubkey::from_str(&target_mint_str).ok()
        };

        let fetches = futures::stream::iter(wallets_to_check.into_iter().map(|address_str| {
            let client_clone = Arc::clone(&client);
            let sender_clone = sender.clone();
            let target_mint_pk_clone = target_mint_pubkey.clone();

            async move {
                let address_pubkey_result = Pubkey::from_str(&address_str);
                let result: Result<(f64, Option<String>), anyhow::Error> = async {
                    let address_pubkey = address_pubkey_result.map_err(|e| anyhow!("Invalid address {}: {}", address_str, e))?;
                    
                    let sol_balance_lamports = client_clone.get_balance(&address_pubkey).await
                        .context(format!("Failed to get SOL balance for {}", address_str))?;
                    let sol_balance = lamports_to_sol(sol_balance_lamports);

                    let mut target_mint_balance_str: Option<String> = None;
                    if let Some(mint_pk) = target_mint_pk_clone {
                        let ata_pubkey = spl_associated_token_account::get_associated_token_address(&address_pubkey, &mint_pk);
                        match client_clone.get_token_account_balance(&ata_pubkey).await {
                            Ok(ui_token_amount) => {
                                target_mint_balance_str = Some(ui_token_amount.ui_amount_string);
                            }
                            Err(e) => {
                                if e.to_string().contains("AccountNotFound") || e.to_string().contains("could not find account") {
                                    log::debug!("Token account {} not found for owner {}, balance is 0.", ata_pubkey, address_str);
                                    target_mint_balance_str = Some("0".to_string());
                                } else {
                                    warn!("Failed to get token balance for calculated ATA {} (Owner: {}): {}", ata_pubkey, address_str, e);
                                    target_mint_balance_str = None;
                                }
                            }
                        }
                    }
                    Ok((sol_balance, target_mint_balance_str))
                }.await;

                let fetch_result = match result {
                    Ok((sol, token)) => FetchResult::Success {
                        address: address_str.clone(),
                        sol_balance: sol,
                        target_mint_balance: token,
                    },
                    Err(e) => FetchResult::Failure {
                        address: address_str.clone(),
                        error: e.to_string(),
                    },
                };

                if let Err(e) = sender_clone.send(fetch_result) {
                    error!("Failed to send balance result for {}: {}", address_str, e);
                }
            }
        }));

        // Concurrently process all fetches. Adjust concurrency level as needed.
        let concurrency_limit = 10; // Example: Process up to 10 balance fetches concurrently
        fetches.for_each_concurrent(concurrency_limit, |fut| fut).await;
        
        // Optionally, send a "all fetches dispatched" or "all fetches completed" marker if needed by UI.
        // For example, if the UI needs to know when the *task* is done, not just when individual results arrive.
        // let _ = sender.send(FetchResult::BatchCompleted); // Requires adding BatchCompleted variant to FetchResult
    }
// Task to specifically load volume bot wallets from a file
    async fn load_volume_wallets_from_file_task(
        wallets_file_path: String,
        wallets_data_sender: UnboundedSender<Vec<LoadedWalletInfo>>,
        status_sender: UnboundedSender<Result<String, String>>,
    ) {
        info!("Load Wallets Task: Attempting to load wallets from {}", wallets_file_path);
        if wallets_file_path.trim().is_empty() {
            let err_msg = "Wallet file path is empty.".to_string();
            error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg));
            return;
        }

        match File::open(&wallets_file_path) {
            Ok(file) => {
                let reader = std::io::BufReader::new(file);
                match serde_json::from_reader(reader) {
                    Ok(loaded_wallets_vec) => {
                        let wallets_to_send: Vec<LoadedWalletInfo> = loaded_wallets_vec;
                        if wallets_to_send.is_empty() {
                            let msg = format!("No wallets found in file {} or file is empty.", wallets_file_path);
                            info!("{}", msg);
                            let _ = status_sender.send(Ok(msg));
                            if wallets_data_sender.send(Vec::new()).is_err() { // Send empty vec
                                error!("Failed to send empty wallet vec to main app.");
                            }
                        } else {
                            let msg = format!("Successfully loaded {} wallets from {}.", wallets_to_send.len(), wallets_file_path);
                            info!("{}", msg);
                            let _ = status_sender.send(Ok(msg));
                            if wallets_data_sender.send(wallets_to_send).is_err() {
                                error!("Failed to send loaded wallets to main app.");
                            }
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("Failed to parse wallets file {}: {}", wallets_file_path, e);
                        error!("{}", err_msg);
                        let _ = status_sender.send(Err(err_msg));
                    }
                }
            }
            Err(e) => {
                let err_msg = format!("Failed to open wallets file {}: {}", wallets_file_path, e);
                error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg));
            }
        };
    }
async fn gather_all_funds_from_volume_wallets_task(
        rpc_url: String,
        parent_private_key_str: String,
        volume_wallets: Vec<LoadedWalletInfo>,
        token_mint_ca_str: String, // CA of the token to gather
        status_sender: UnboundedSender<Result<String, String>>,
    ) {
        info!("Gather All Funds Task: Starting...");
        let _ = status_sender.send(Ok("Gather All Funds task started.".to_string()));

        if parent_private_key_str.is_empty() {
            let err_msg = "Parent wallet private key is not set in settings.".to_string();
            error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg));
            return;
        }

        let parent_keypair = match crate::commands::utils::load_keypair_from_string(&parent_private_key_str, "ParentWallet") {
            Ok(kp) => kp,
            Err(e) => {
                let err_msg = format!("Failed to load parent keypair: {}", e);
                error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg));
                return;
            }
        };
        let parent_pubkey = parent_keypair.pubkey();

        if volume_wallets.is_empty() {
            let msg = "No volume bot wallets to gather funds from.".to_string();
            info!("{}", msg);
            let _ = status_sender.send(Ok(msg));
            return;
        }

        let client = Arc::new(AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed()));
        let token_mint_ca_pubkey_opt: Option<Pubkey> = if token_mint_ca_str.trim().is_empty() {
            None
        } else {
            Pubkey::from_str(&token_mint_ca_str.trim()).ok()
        };

        for (index, vol_wallet_info) in volume_wallets.iter().enumerate() {
            let _ = status_sender.send(Ok(format!("Processing wallet {}/{} ({})", index + 1, volume_wallets.len(), vol_wallet_info.public_key)));

            let vol_wallet_keypair = match crate::commands::utils::load_keypair_from_string(&vol_wallet_info.private_key, "VolumeWallet") {
                Ok(kp) => kp,
                Err(e) => {
                    let err_msg = format!("Failed to load keypair for {}: {}. Skipping.", vol_wallet_info.public_key, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg));
                    continue;
                }
            };
            let vol_wallet_pubkey = vol_wallet_keypair.pubkey();

            // 1. Gather SOL
            match client.get_balance(&vol_wallet_pubkey).await {
                Ok(sol_balance_lamports) => {
                    if sol_balance_lamports > 0 {
                        const TRANSACTION_FEE_LAMPORTS: u64 = 5000; // Standard fee for a simple transfer
                        if sol_balance_lamports > TRANSACTION_FEE_LAMPORTS {
                            let amount_to_send = sol_balance_lamports - TRANSACTION_FEE_LAMPORTS;
                            if amount_to_send > 0 { // Ensure we are sending a positive amount
                                let sol_transfer_ix = system_instruction::transfer(&vol_wallet_pubkey, &parent_pubkey, amount_to_send);
                                
                                let latest_blockhash = match client.get_latest_blockhash().await {
                                    Ok(bh) => bh,
                                    Err(e) => {
                                        let err_msg = format!("Gather SOL: Failed to get blockhash for {}: {}", vol_wallet_pubkey, e);
                                        error!("{}", err_msg);
                                        let _ = status_sender.send(Err(err_msg));
                                        continue; // Skip this wallet for SOL gather
                                    }
                                };
                                let mut sol_gather_attempts = 0;
                                let max_sol_gather_attempts = 3;
                                let mut sol_gather_successful = false;

                                while sol_gather_attempts < max_sol_gather_attempts {
                                    sol_gather_attempts += 1;
                                    
                                    let current_blockhash_sol = match client.get_latest_blockhash().await {
                                        Ok(bh) => bh,
                                        Err(e) => {
                                            let err_msg = format!("Gather SOL (Attempt {}): Failed to get blockhash for {}: {}", sol_gather_attempts, vol_wallet_pubkey, e);
                                            error!("{}", err_msg);
                                            if sol_gather_attempts >= max_sol_gather_attempts { let _ = status_sender.send(Err(err_msg)); }
                                            tokio::time::sleep(Duration::from_millis(500 + sol_gather_attempts * 200)).await;
                                            continue;
                                        }
                                    };

                                    let tx = Transaction::new_signed_with_payer(
                                        &[sol_transfer_ix.clone()], // Clone instruction if needed in retries
                                        Some(&vol_wallet_keypair.pubkey()),
                                        &[&vol_wallet_keypair as &dyn Signer],
                                        current_blockhash_sol,
                                    );

                                    info!("Gather SOL (Attempt {}): Sending tx for {} with blockhash {}", sol_gather_attempts, vol_wallet_pubkey, current_blockhash_sol);
                                    match client.send_and_confirm_transaction_with_spinner(&tx).await {
                                        Ok(sig) => {
                                            let msg = format!("Successfully gathered {:.9} SOL from {}. Sig: {}", lamports_to_sol(amount_to_send), vol_wallet_pubkey, sig);
                                            info!("{}", msg);
                                            let _ = status_sender.send(Ok(msg));
                                            sol_gather_successful = true;
                                            break;
                                        }
                                        Err(e) => {
                                            let err_msg = format!("Gather SOL (Attempt {}): Failed for {}: {}", sol_gather_attempts, vol_wallet_pubkey, e);
                                            warn!("{}", err_msg);
                                            if e.to_string().contains("Blockhash not found") || e.to_string().contains("-32002") {
                                                if sol_gather_attempts < max_sol_gather_attempts {
                                                    tokio::time::sleep(Duration::from_millis(1000 + sol_gather_attempts * 500)).await;
                                                    continue;
                                                } else {
                                                    error!("Max SOL gather attempts reached for {} after blockhash issues.", vol_wallet_pubkey);
                                                    let _ = status_sender.send(Err(err_msg));
                                                    break;
                                                }
                                            } else {
                                                let _ = status_sender.send(Err(err_msg)); // Send other errors
                                                break;
                                            }
                                        }
                                    }
                                }
                                if !sol_gather_successful {
                                    // Error already sent by last attempt or blockhash fetch failure
                                    error!("Permanently failed to gather SOL from {} after {} attempts.", vol_wallet_pubkey, max_sol_gather_attempts);
                                }
                            } else {
                                let _ = status_sender.send(Ok(format!("Calculated SOL amount to send from {} is zero after reserving fee.", vol_wallet_pubkey)));
                            }
                        } else {
                             let _ = status_sender.send(Ok(format!("SOL balance in {} too low ({} lamports) to cover transaction fee.", vol_wallet_pubkey, sol_balance_lamports)));
                        }
                    } else {
                        let _ = status_sender.send(Ok(format!("No SOL to gather from {}.", vol_wallet_pubkey)));
                    }
                }
                Err(e) => {
                    let err_msg = format!("Failed to get SOL balance for {}: {}", vol_wallet_pubkey, e);
                    error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg));
                }
            }
            
            tokio::time::sleep(Duration::from_millis(200)).await; 

            if let Some(token_mint_pk) = token_mint_ca_pubkey_opt {
                let source_ata = get_associated_token_address(&vol_wallet_pubkey, &token_mint_pk);
                let dest_ata = get_associated_token_address(&parent_pubkey, &token_mint_pk);

                match client.get_token_account_balance(&source_ata).await {
                    Ok(token_account_balance) => {
                        if let Some(amount) = token_account_balance.amount.parse::<u64>().ok() {
                            if amount > 0 {
                                let mut instructions = Vec::new();
                                match spl_token::instruction::transfer(
                                    &spl_token::ID,
                                    &source_ata,
                                    &dest_ata,
                                    &vol_wallet_pubkey,
                                    &[], 
                                    amount,
                                ) {
                                    Ok(ix) => instructions.push(ix),
                                    Err(e) => {
                                        let err_msg = format!("Failed to create token transfer instruction for {}: {}", vol_wallet_pubkey, e);
                                        error!("{}", err_msg);
                                        let _ = status_sender.send(Err(err_msg));
                                        continue; // Skip to next wallet if instruction fails
                                    }
                                }
                                
                                if instructions.is_empty() { continue; } // Should not happen if transfer ix was ok

                                let latest_blockhash = match client.get_latest_blockhash().await {
                                    Ok(bh) => bh,
                                    Err(e) => {
                                        let err_msg = format!("Token Gather: Failed to get blockhash for {}: {}", vol_wallet_pubkey, e);
                                        error!("{}", err_msg);
                                        let _ = status_sender.send(Err(err_msg));
                                        continue;
                                    }
                                };
                                let mut token_gather_attempts = 0;
                                let max_token_gather_attempts = 3;
                                let mut token_gather_successful = false;

                                while token_gather_attempts < max_token_gather_attempts {
                                    token_gather_attempts += 1;

                                    let current_blockhash_token = match client.get_latest_blockhash().await {
                                        Ok(bh) => bh,
                                        Err(e) => {
                                            let err_msg = format!("Gather Token (Attempt {}): Failed to get blockhash for {}: {}", token_gather_attempts, vol_wallet_pubkey, e);
                                            error!("{}", err_msg);
                                            if token_gather_attempts >= max_token_gather_attempts { let _ = status_sender.send(Err(err_msg)); }
                                            tokio::time::sleep(Duration::from_millis(500 + token_gather_attempts * 200)).await;
                                            continue;
                                        }
                                    };
                                    
                                    // Recreate instructions vector for each attempt if it could change, though here it's simple
                                    let current_instructions = instructions.clone(); // Clone if instructions might be modified or for safety

                                    let tx = Transaction::new_signed_with_payer(
                                        &current_instructions,
                                        Some(&vol_wallet_keypair.pubkey()),
                                        &[&vol_wallet_keypair as &dyn Signer],
                                        current_blockhash_token,
                                    );
                                    info!("Gather Token (Attempt {}): Sending tx for {} with blockhash {}", token_gather_attempts, vol_wallet_pubkey, current_blockhash_token);
                                    match client.send_and_confirm_transaction_with_spinner(&tx).await {
                                        Ok(sig) => {
                                            let msg = format!("Successfully gathered {} tokens ({}) from {}. Sig: {}", amount, token_mint_ca_str, vol_wallet_pubkey, sig);
                                            info!("{}", msg);
                                            let _ = status_sender.send(Ok(msg));
                                            token_gather_successful = true;
                                            break;
                                        }
                                        Err(e) => {
                                            let err_msg = format!("Gather Token (Attempt {}): Failed for {}: {}", token_gather_attempts, vol_wallet_pubkey, e);
                                            warn!("{}", err_msg);
                                            if e.to_string().contains("Blockhash not found") || e.to_string().contains("-32002") {
                                                if token_gather_attempts < max_token_gather_attempts {
                                                    tokio::time::sleep(Duration::from_millis(1000 + token_gather_attempts * 500)).await;
                                                    continue;
                                                } else {
                                                    error!("Max token gather attempts reached for {} after blockhash issues.", vol_wallet_pubkey);
                                                     let _ = status_sender.send(Err(err_msg));
                                                    break;
                                                }
                                            } else {
                                                let _ = status_sender.send(Err(err_msg)); // Send other errors
                                                break;
                                            }
                                        }
                                    }
                                }
                                if !token_gather_successful {
                                    error!("Permanently failed to gather tokens from {} after {} attempts.", vol_wallet_pubkey, max_token_gather_attempts);
                                }
                            } else {
                                 let _ = status_sender.send(Ok(format!("No tokens ({}) to gather from {}.", token_mint_ca_str, vol_wallet_pubkey)));
                            }
                        } else {
                             let _ = status_sender.send(Err(format!("Failed to parse token balance for {} from {}.", token_mint_ca_str, vol_wallet_pubkey)));
                        }
                    }
                    Err(_e) => { 
                        let _ = status_sender.send(Ok(format!("No token account ({}) found for {}.", token_mint_ca_str, vol_wallet_pubkey)));
                    }
                }
            }
             tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = status_sender.send(Ok("Gather All Funds task finished.".to_string()));
        info!("Gather All Funds Task: Finished.");
    }



    // --- Disperse View ---
    fn show_disperse_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("💨 Disperse SOL"); // Updated heading
        ui.separator();

        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow)
            .show(ui, |ui_card| {
                ui_card.label("Send SOL from the PARENT wallet (defined in settings) to selected zombie wallets.");
                ui_card.add_space(10.0);

                ui_card.horizontal(|ui_card_inner| {
                    ui_card_inner.label("Amount per Zombie:");
                    ui_card_inner.add(
                        egui::DragValue::new(&mut self.disperse_sol_amount)
                            .speed(0.001)
                            .range(0.0..=1000.0)
                            .max_decimals(9)
                            .prefix("◎ ")
                    );
                });
                ui_card.separator();

                ui_card.horizontal(|ui_card_inner| {
                    ui_card_inner.strong("Select Zombie Wallets:");
                    if ui_card_inner.button("Select All").clicked() {
                        for wallet in self.wallets.iter_mut() {
                            if !wallet.is_primary { wallet.is_selected = true; }
                        }
                    }
                    if ui_card_inner.button("Deselect All").clicked() {
                        for wallet in self.wallets.iter_mut() {
                            wallet.is_selected = false;
                        }
                    }
                });

                let parent_pk_str = self.app_settings.parent_wallet_private_key.clone();
                let parent_pubkey_str_option = AppSettings::get_pubkey_from_privkey_str(&parent_pk_str);
                let parent_key_is_valid = !parent_pk_str.is_empty();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height(300.0)
                    .show(ui_card, |ui_scroll| {
                    egui::Grid::new("disperse_wallet_grid")
                        .num_columns(3)
                        .spacing([10.0, 4.0])
                        .striped(true)
                        .show(ui_scroll, |ui_grid| {
                            ui_grid.strong("Select");
                            ui_grid.strong("Wallet Address");
                            ui_grid.strong("Current SOL");
                            ui_grid.end_row();

                            for wallet in self.wallets.iter_mut().filter(|w| !w.is_parent) {
                                let _checkbox_response = ui_grid.checkbox(&mut wallet.is_selected, "");
                                ui_grid.monospace(&wallet.address);
                                ui_grid.monospace(
                                    wallet.sol_balance
                                        .map(|b| format!("◎ {:.6}", b))
                                        .unwrap_or_else(|| "-".to_string())
                                );
                                ui_grid.end_row();
                            }
                        });
                });

                ui_card.separator();

                ui_card.vertical_centered(|ui_card_inner_centered| {
                    let amount_valid = self.disperse_sol_amount > 0.0;
                    let enabled = parent_key_is_valid && amount_valid && !self.disperse_in_progress;

                    let tooltip_text = if !parent_key_is_valid {
                        "Parent private key not found or invalid in settings."
                    } else if !amount_valid {
                        "Disperse amount must be greater than 0."
                    } else if self.disperse_in_progress {
                        "Disperse operation already in progress."
                    } else {
                        ""
                    };

                    let disperse_selected_button = egui::Button::new("💨 Disperse to Selected") // Shortened
                        .min_size(egui::vec2(200.0, 35.0));

                    if ui_card_inner_centered.add_enabled(enabled, disperse_selected_button).on_hover_text(tooltip_text).clicked() {
                        match bs58::decode(&parent_pk_str).into_vec() {
                            Ok(bytes) => match Keypair::from_bytes(&bytes) {
                                Ok(parent_kp) => {
                                    let targets: Vec<String> = self.wallets.iter()
                                        .filter(|w| w.is_selected && Some(w.address.clone()) != parent_pubkey_str_option)
                                        .map(|w| w.address.clone())
                                        .collect();

                                    if targets.is_empty() {
                                         self.last_operation_result = Some(Err("No valid zombie wallets selected to disperse to.".to_string()));
                                    } else {
                                        log::info!("Dispersing {} SOL from Parent to {} selected zombie wallets...", self.disperse_sol_amount, targets.len());
                                        self.last_operation_result = Some(Ok(format!("Initiating disperse from Parent to {} selected wallets...", targets.len())));
                                        self.disperse_in_progress = true;
                                        let rpc_url = self.app_settings.solana_rpc_url.clone();
                                        let sender = self.disperse_result_sender.clone();
                                        let amount_lamports = sol_to_lamports(self.disperse_sol_amount);
                                        tokio::spawn(Self::disperse_sol_task(rpc_url, parent_kp, targets, amount_lamports, sender));
                                    }
                                }
                                Err(e) => {
                                    log::error!("Failed to create parent keypair from bytes (selected): {}", e);
                                    self.last_operation_result = Some(Err("Invalid parent keypair bytes. Check settings.".to_string()));
                                }
                            }
                            Err(e) => {
                                 log::error!("Failed to decode base58 parent key (selected): {}", e);
                                 self.last_operation_result = Some(Err("Invalid base58 parent key. Check settings.".to_string()));
                            }
                        }
                    }

                     let disperse_all_button = egui::Button::new("💨 Disperse to All Zombies")
                        .min_size(egui::vec2(200.0, 35.0)); // Consistent button size

                    if ui_card_inner_centered.add_enabled(enabled, disperse_all_button).on_hover_text(tooltip_text).clicked() {
                         match bs58::decode(&parent_pk_str).into_vec() {
                            Ok(bytes) => match Keypair::from_bytes(&bytes) {
                                 Ok(parent_kp) => {
                                    let targets: Vec<String> = self.wallets.iter()
                                        .filter(|w| !w.is_primary && Some(w.address.clone()) != parent_pubkey_str_option)
                                        .map(|w| w.address.clone())
                                        .collect();

                                     if targets.is_empty() {
                                         self.last_operation_result = Some(Err("No valid zombie wallets found to disperse to.".to_string()));
                                    } else {
                                        log::info!("Dispersing {} SOL from Parent to {} non-primary zombie wallets...", self.disperse_sol_amount, targets.len());
                                        self.last_operation_result = Some(Ok(format!("Initiating disperse from Parent to {} non-primary wallets...", targets.len())));
                                        self.disperse_in_progress = true;
                                        let rpc_url = self.app_settings.solana_rpc_url.clone();
                                        let sender = self.disperse_result_sender.clone();
                                        let amount_lamports = sol_to_lamports(self.disperse_sol_amount);
                                        tokio::spawn(Self::disperse_sol_task(rpc_url, parent_kp, targets, amount_lamports, sender));
                                    }
                                }
                                Err(e) => {
                                    log::error!("Failed to create parent keypair from bytes (all): {}", e);
                                    self.last_operation_result = Some(Err("Invalid parent keypair bytes. Check settings.".to_string()));
                                }
                            }
                            Err(e) => {
                                 log::error!("Failed to decode base58 parent key (all): {}", e);
                                 self.last_operation_result = Some(Err("Invalid base58 parent key. Check settings.".to_string()));
                            }
                         }
                    }

                    if self.disperse_in_progress {
                        ui_card_inner_centered.add_space(5.0);
                        ui_card_inner_centered.spinner();
                    }
                });
            }); // End of main Frame for disperse view
    }

    // --- Disperse SOL Task ---
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
        let mut instructions = Vec::new();
        let mut recipients_processed = 0;

        for addr_str in &recipient_addresses {
            match Pubkey::from_str(addr_str) {
                Ok(recipient_pubkey) => {
                    instructions.push(system_instruction::transfer(
                        &payer_pubkey,
                        &recipient_pubkey,
                        amount_lamports_each,
                    ));
                    recipients_processed += 1;
                }
                Err(e) => {
                    warn!("Skipping invalid recipient address '{}': {}", addr_str, e);
                }
            }
        }

        if instructions.is_empty() {
            let _ = sender.send(Err("No valid recipient addresses found.".to_string()));
            return;
        }

        info!("Attempting to disperse to {} valid recipients.", recipients_processed);

        // Wrap the batch processing logic in an async block and assign its result
        // Assign the future to a variable first
        let task_future = async {
            let mut batch_results: Vec<Result<String, String>> = Vec::new();
            let mut successful_tx_count = 0;
            let mut failed_tx_count = 0;

            // --- Define batching parameters ---
            let batch_size = 15; // Max transfers per tx
            let num_batches = (recipients_processed + batch_size - 1) / batch_size;
            let delay_between_batches = Duration::from_millis(500);
            let selected_addresses = recipient_addresses.clone();

            for (batch_index, chunk) in selected_addresses.chunks(batch_size).enumerate() {
                log::info!("Processing batch {}/{} ({} wallets)...", batch_index + 1, num_batches, chunk.len());

                let mut instructions_batch = Vec::new();
                let mut current_chunk_valid_recipients = 0;

                for target_address_str in chunk {
                     match Pubkey::from_str(target_address_str) {
                         Ok(target_pubkey) => {
                             if target_pubkey == payer_pubkey {
                                 log::warn!("Batch {}: Skipping transfer to self ({})", batch_index+1, target_pubkey);
                                 continue;
                             }
                             instructions_batch.push(system_instruction::transfer(
                                 &payer_pubkey,
                                 &target_pubkey,
                                 amount_lamports_each,
                             ));
                             current_chunk_valid_recipients += 1;
                         }
                         Err(e) => {
                              log::warn!("Batch {}: Skipping invalid target wallet address '{}': {}", batch_index+1, target_address_str, e);
                         }
                     }
                }

                if instructions_batch.is_empty() {
                    log::warn!("Batch {}/{} had no valid instructions to send.", batch_index + 1, num_batches);
                    continue;
                }

                // --- Send one batch transaction ---
                let batch_tx_result = async {
                    let latest_blockhash = client.get_latest_blockhash().await?;
                    log::debug!("Got blockhash {} for batch {}", latest_blockhash, batch_index + 1);

                    let mut tx = Transaction::new_with_payer(&instructions_batch, Some(&payer_pubkey));
                    // Payer_keypair is now owned by the task
                    tx.sign(&[&payer_keypair], latest_blockhash); // Pass as slice of references
                    log::debug!("Batch {} transaction signed.", batch_index + 1);

                    let signature = client.send_and_confirm_transaction(&tx).await?;
                    log::info!("Batch {}/{} confirmed! Signature: {}", batch_index + 1, num_batches, signature);
                    Ok::<String, anyhow::Error>(signature.to_string())
                }.await;
                // --- End of batch transaction send ---

                match batch_tx_result {
                    Ok(sig) => {
                        successful_tx_count += current_chunk_valid_recipients;
                        batch_results.push(Ok(sig));
                    }
                    Err(e) => {
                        failed_tx_count += current_chunk_valid_recipients;
                         log::error!("Batch {}/{} failed: {}", batch_index + 1, num_batches, e);
                         batch_results.push(Err(format!("Batch {} failed: {}", batch_index + 1, e)));
                    }
                }

                if batch_index < num_batches - 1 {
                    log::debug!("Waiting {:?} before next batch...", delay_between_batches);
                    sleep(delay_between_batches).await;
                }
            }
            // --- End of Batch Loop ---

            // --- Aggregate results ---
             let final_message = format!(
                 "Dispersal finished. Sent to ~{} wallets successfully, ~{} failed (across {} batches).",
                 successful_tx_count, failed_tx_count, num_batches
             );
            log::info!("{}", final_message);

            if failed_tx_count > 0 {
                let first_error = batch_results.iter().find_map(|r| r.as_ref().err()).cloned().unwrap_or_default();
                Err(anyhow!("{}. First error: {}", final_message, first_error))
            } else {
                Ok(final_message)
            }
        }; // End of the async block definition

        // Now await the future
        let task_result = task_future.await;

        // Send final aggregated result to the main thread
        match task_result {
            Ok(summary) => {
                let _ = sender.send(Ok(summary));
            }
            Err(e) => {
                 log::error!("Dispersal task failed overall: {}", e);
                let _ = sender.send(Err(format!("Dispersal failed: {}", e)));
            }
        }
    }

    // --- Gather View UI ---
    fn show_gather_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("📥 Gather SOL to Parent");
        ui.separator();

        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow)
            .show(ui, |ui_card| {
                ui_card.label("Send SOL from selected zombie/dev wallets TO the PARENT wallet (defined in settings).");
                ui_card.add_space(10.0);

                // --- Selection Controls ---
                ui_card.horizontal(|ui_card_inner| {
                    if ui_card_inner.button("Select All").clicked() {
                        let parent_pubkey_str_option: Option<String> = {
                            let pk_str = &self.app_settings.parent_wallet_private_key;
                            if !pk_str.is_empty() {
                                let keypair_bytes_result = bs58::decode(pk_str).into_vec();
                                match keypair_bytes_result {
                                    Ok(keypair_bytes) => {
                                        let keypair_result = Keypair::from_bytes(&keypair_bytes);
                                        match keypair_result {
                                            Ok(kp) => Some(kp.pubkey().to_string()),
                                            Err(e) => {
                                                log::error!("Failed to create keypair from bytes: {}", e);
                                                None
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        log::error!("Failed to decode base58 private key from settings: {}", e);
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        };

                        if let Some(parent_pk) = parent_pubkey_str_option {
                            for wallet in self.wallets.iter_mut().filter(|w| w.address != parent_pk) {
                                wallet.is_selected = true;
                            }
                        } else {
                             log::error!("Cannot Select All: Failed to load/parse parent key to exclude it.");
                             self.last_operation_result = Some(Err("Failed to load/parse parent key from settings".to_string()));
                        }
                    }
                    if ui_card_inner.button("Deselect All").clicked() {
                         for wallet in self.wallets.iter_mut() {
                            wallet.is_selected = false;
                        }
                    }
                });
                ui_card.separator();

                // --- Wallet List ---
                ui_card.label(egui::RichText::new("Select Wallets to Gather SOL From:").strong()); // Updated label
                ui_card.add_space(5.0);

                let parent_keypair_option: Option<Keypair> = {
                    let pk_str = &self.app_settings.parent_wallet_private_key;
                    if !pk_str.is_empty() {
                        bs58::decode(pk_str).into_vec()
                            .ok()
                            .and_then(|bytes| Keypair::from_bytes(&bytes).ok())
                    } else {
                        None
                    }
                };
                let parent_pubkey_option: Option<Pubkey> = match parent_keypair_option {
                     Some(ref kp) => Some(kp.pubkey()),
                     None => {
                         Pubkey::from_str(&self.app_settings.parent_wallet_private_key).ok()
                     }
                };
                let parent_pubkey = match parent_pubkey_option {
                    Some(pk) => pk,
                    None => {
                        ui_card.colored_label(ui_card.style().visuals.error_fg_color, "Error: Could not load/parse Parent wallet key from Settings.");
                        return;
                    }
                };
                let _parent_pubkey_str = parent_pubkey.to_string();

                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([false, false])
                    .show(ui_card, |ui_scroll| {
                        egui::Grid::new("gather_wallet_grid")
                            .num_columns(2)
                            .spacing([20.0, 4.0])
                            .striped(true)
                            .show(ui_scroll, |ui_grid| {
                                for wallet in self.wallets.iter_mut().filter(|w| w.address != parent_pubkey.to_string()) {
                                    let checkbox_enabled = true;
                                    ui_grid.add_enabled_ui(checkbox_enabled, |ui_check| {
                                        ui_check.checkbox(&mut wallet.is_selected, "");
                                    });

                                    let short_addr = format!("{}...{}", &wallet.address[..6], &wallet.address[wallet.address.len()-6..]);
                                    let label = if wallet.is_dev_wallet {
                                        format!("{} (Dev)", short_addr)
                                    } else if wallet.is_primary {
                                        format!("{} (Primary)", short_addr)
                                    } else {
                                        short_addr
                                    };
                                    ui_grid.label(label).on_hover_text(&wallet.address);
                                    ui_grid.end_row();
                                }
                                ui_grid.add_enabled_ui(false, |ui_check| { ui_check.checkbox(&mut false, ""); });
                                ui_grid.label(format!("{}...{} (Parent - Recipient)", &parent_pubkey.to_string()[..6], &parent_pubkey.to_string()[parent_pubkey.to_string().len()-6..]))
                                  .on_hover_text(parent_pubkey.to_string());
                                ui_grid.end_row();
                            });
                    });
                
                let parent_pubkey_str_for_filter = parent_pubkey.to_string(); // Use a new variable for filtering
                let selected_wallets_data: Vec<LoadedWalletInfo> = self.wallets.iter()
                    .filter(|w_info| w_info.is_selected && w_info.address != parent_pubkey_str_for_filter)
                    .filter_map(|w_info| {
                        self.loaded_wallet_data.iter()
                            .find(|wd| wd.public_key == w_info.address)
                            .cloned()
                    })
                    .collect();

                let num_selected = selected_wallets_data.len();

                ui_card.separator();
                ui_card.label(format!("Selected Wallets: {}", num_selected));
                ui_card.add_space(20.0);

                ui_card.horizontal(|ui_card_inner_buttons| {
                    let gather_selected_button = egui::Button::new("📥 Gather Selected SOL")
                        .min_size(egui::vec2(180.0, 35.0)); // Adjusted size
                    let selected_enabled = !self.gather_in_progress && num_selected > 0;

                    if ui_card_inner_buttons.add_enabled(selected_enabled, gather_selected_button).clicked() {
                         log::info!("Gather Selected button clicked. Gathering SOL sequentially from {} wallets to Parent {}.", num_selected, _parent_pubkey_str); // Use _parent_pubkey_str
                         self.last_operation_result = Some(Ok("Initiating gather from selected wallets...".to_string()));
                         self.gather_in_progress = true;
                         self.gather_tasks_expected = num_selected;
                         self.gather_tasks_completed = 0;

                         let rpc_url = self.app_settings.solana_rpc_url.clone();
                         let sender = self.gather_result_sender.clone();
                         let recipient_pubkey = parent_pubkey;
                         let delay_between_gathers = Duration::from_millis(500);
                         let wallets_to_gather_from = selected_wallets_data.clone();

                         tokio::spawn(async move {
                             log::info!("Starting sequential gather for selected wallets with delay: {:?}", delay_between_gathers);
                             let stream = iter(wallets_to_gather_from).for_each_concurrent(Some(1), |zombie_wallet_data| {
                                 let rpc_url_clone = rpc_url.clone(); // Clone for closure
                                 let sender_clone = sender.clone(); // Clone for closure
                                 let recipient_pubkey_clone = recipient_pubkey; // Clone for closure
                                 let delay_clone = delay_between_gathers; // Clone for closure
                                 async move {
                                    Self::gather_sol_from_zombie_task(rpc_url_clone, zombie_wallet_data, recipient_pubkey_clone, sender_clone).await;
                                    sleep(delay_clone).await;
                                 }
                             });
                             stream.await;
                             log::info!("Sequential gather stream processing for selected wallets completed.");
                         });
                    }

                    let gather_all_button = egui::Button::new("📥 Gather All SOL")
                        .min_size(egui::vec2(160.0, 35.0)); // Adjusted size
                    let all_enabled = !self.gather_in_progress;

                    if ui_card_inner_buttons.add_enabled(all_enabled, gather_all_button).clicked() {
                         log::info!("Gather All button clicked. Gathering SOL from Minter and Simulation wallets to Parent.");
                         self.last_operation_result = Some(Ok("Initiating gather from all wallets...".to_string()));
                         self.gather_in_progress = true;
                         self.gather_tasks_expected = 1;
                         self.gather_tasks_completed = 0;

                         let sender_clone_all = self.gather_result_sender.clone(); // Clone for closure
                         let keys_path_clone_all = self.app_settings.keys_path.clone(); // Clone for closure

                         tokio::spawn(async move {
                             log::info!("Spawning gather_sol command task with keys_path: {}", keys_path_clone_all);
                             let result = crate::commands::gather::gather_sol(Some(&keys_path_clone_all)).await;

                             let ui_result: Result<String, String> = match result {
                                 Ok(_) => Ok("Gather All operation completed.".to_string()),
                                 Err(e) => {
                                     let error_msg = format!("Gather All operation failed: {}", e);
                                     log::error!("{}", error_msg);
                                     Err(error_msg)
                                 }
                             };

                             if let Err(e) = sender_clone_all.send(ui_result) {
                                 log::error!("Failed to send Gather All result back to UI: {}", e);
                             }
                         });
                    }

                    if self.gather_in_progress {
                         ui_card_inner_buttons.add(egui::Spinner::new()).on_hover_text("Gathering SOL...");
                    }
                });
            }); // End of main Frame for gather view
    }

    // --- Gather SOL Task (for a single zombie) ---
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

    // --- ALT Management View UI ---
    fn show_alt_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("🔧 ALT Management"); // Changed icon
        ui.separator();

        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow)
            .show(ui, |ui_card| {
                ui_card.label("This tool generates a new mint keypair, precalculates necessary addresses for a Pump.fun token creation, and creates an Address Lookup Table (ALT) containing them.");
                ui_card.add_space(10.0);

                // --- Status Display ---
                ui_card.label(format!("Status: {}", self.alt_view_status));
                ui_card.add_space(5.0);

                // --- Button 1: Generate & Precalculate ---
                let gen_button = egui::Button::new("1. Generate Key & Precalculate Addresses")
                    .min_size(egui::vec2(250.0, 35.0)); // Made button larger
                if ui_card.add_enabled(!self.alt_precalc_in_progress && !self.alt_creation_in_progress, gen_button).clicked() {
                    log::info!("Generate & Precalculate button clicked.");
                    self.alt_view_status = "Generating keypair and precalculating addresses...".to_string();
                    self.alt_precalc_in_progress = true;
                    self.alt_generated_mint_pubkey = None;
                    self.alt_precalc_addresses.clear();
                    self.alt_address = None;
                    self.last_operation_result = None;
                    self.alt_log_messages.clear(); // Clear logs at start of new operation

                    let sender = self.alt_precalc_result_sender.clone();
                    let parent_pubkey_str_option = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                    let zombie_pubkeys = self.loaded_wallet_data.iter()
                        .map(|wd| Pubkey::from_str(&wd.public_key).ok())
                        .flatten()
                        .collect::<Vec<Pubkey>>();
                     let settings_clone = self.app_settings.clone();
                     tokio::spawn(async move {
                         Self::generate_and_precalc_task(parent_pubkey_str_option, zombie_pubkeys, settings_clone, sender).await;
                     });
                }
                if self.alt_precalc_in_progress {
                    ui_card.horizontal(|ui_card_inner| {
                        ui_card_inner.spinner();
                        ui_card_inner.label("Precalculating...");
                    });
                }
                ui_card.add_space(10.0);

                // --- Display Precalc Results & Create Button ---
                if let Some(mint_pk) = &self.alt_generated_mint_pubkey {
                    ui_card.label(format!("Generated Token CA: {}", mint_pk));

                    ui_card.label(format!("Precalculated Addresses ({})", self.alt_precalc_addresses.len()));
                    ui_card.group(|ui_card_group| { // Use group for visual separation of address list
                        egui::ScrollArea::both()
                            .max_height(100.0) // Reduced height
                            .show(ui_card_group, |ui_scroll| {
                                for addr in &self.alt_precalc_addresses {
                                    ui_scroll.monospace(addr.to_string());
                                }
                            });
                    });
                    ui_card.add_space(10.0);

                    let create_alt_button = egui::Button::new("2. Create ALT from Addresses")
                        .min_size(egui::vec2(250.0, 35.0));
                    let create_enabled = !self.alt_precalc_in_progress &&
                                        !self.alt_creation_in_progress &&
                                        !self.alt_precalc_addresses.is_empty() &&
                                        self.alt_address.is_none();

                    if ui_card.add_enabled(create_enabled, create_alt_button).clicked() {
                         log::info!("Create ALT button clicked with {} addresses.", self.alt_precalc_addresses.len());
                         self.alt_creation_in_progress = true;
                         self.last_operation_result = None;
                         self.alt_log_messages.clear();

                         let rpc_url = self.app_settings.solana_rpc_url.clone();
                         let parent_pk_str = self.app_settings.parent_wallet_private_key.clone();
                         let addresses_to_add = self.alt_precalc_addresses.clone();
                         let sender = self.alt_creation_status_sender.clone();

                         tokio::spawn(async move {
                             Self::create_alt_task(rpc_url, parent_pk_str, addresses_to_add, sender).await;
                         });
                    }
                     if self.alt_creation_in_progress {
                         ui_card.horizontal(|ui_card_inner| {
                            ui_card_inner.spinner();
                            ui_card_inner.label("Creating ALT...");
                        });
                    }
                     ui_card.add_space(10.0);
                }

                if let Some(alt_addr) = &self.alt_address {
                    ui_card.separator();
                    ui_card.group(|ui_card_group| {
                        ui_card_group.horizontal(|ui_card_inner| {
                            ui_card_inner.strong("✅ ALT Address: ");
                            egui::ScrollArea::horizontal()
                                .max_height(20.0)
                                .show(ui_card_inner, |ui_scroll| {
                                    ui_scroll.monospace(alt_addr.to_string());
                                });
                        });
                        if ui_card_group.button("Copy ALT Address").clicked() {
                            ui_card_group.output_mut(|o| o.copied_text = alt_addr.to_string());
                            self.last_operation_result = Some(Ok("ALT Address copied!".to_string()));
                        }

                        ui_card_group.add_space(5.0);
                        let deactivate_button = egui::Button::new("🗑️ Deactivate & Close This ALT")
                            .min_size(egui::vec2(250.0, 35.0));
                        let can_deactivate = !self.alt_deactivation_in_progress && !self.app_settings.parent_wallet_private_key.is_empty();
                        
                        if ui_card_group.add_enabled(can_deactivate, deactivate_button).on_hover_text("Uses Parent Wallet from Settings as authority. Ensure it has SOL for fees.").clicked() {
                            if let Some(alt_to_close) = self.alt_address {
                                self.alt_deactivation_in_progress = true;
                                self.alt_deactivation_status_message = format!("Attempting to deactivate & close ALT: {}...", alt_to_close);
                                self.alt_log_messages.push(self.alt_deactivation_status_message.clone());
                                self.last_operation_result = None;

                                let rpc_url = self.app_settings.solana_rpc_url.clone();
                                let parent_pk_str = self.app_settings.parent_wallet_private_key.clone();
                                let status_sender = self.alt_deactivate_status_sender.clone();
                                
                                tokio::spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                    let _ = status_sender.send(Ok(format!("Successfully initiated deactivation for ALT: {}", alt_to_close)));
                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    let _ = status_sender.send(Ok(format!("Successfully closed ALT: {}. (Note: Actual on-chain deactivation might take time)", alt_to_close)));
                                });
                            } else {
                                self.last_operation_result = Some(Err("No ALT address available to deactivate.".to_string()));
                            }
                        }
                        if self.alt_deactivation_in_progress {
                            ui_card_group.horizontal(|ui_card_inner| {
                                ui_card_inner.spinner();
                                ui_card_inner.label(&self.alt_deactivation_status_message);
                            });
                        }
                    });
                    ui_card.add_space(10.0);
                }

                ui_card.separator();
                ui_card.label("Log:");
                egui::ScrollArea::vertical()
                    .max_height(150.0) // Reduced height
                    .auto_shrink([false, true])
                    .stick_to_bottom(true)
                    .show(ui_card, |ui_scroll| {
                        ui_scroll.vertical(|ui_log_content| { // Changed ui to ui_log_content
                            for msg in &self.alt_log_messages {
                                ui_log_content.monospace(msg);
                            }
                        });
                    });
            }); // End of main Frame for ALT view
    }


    // --- ALT Task 1: Generate Key & Precalculate Addresses ---
    async fn generate_and_precalc_task(
        // _rpc_url: String, // Not needed here
        parent_pubkey_str_option: Option<String>, // Keep parent pubkey if needed for ATA derivation
        zombie_pubkeys: Vec<Pubkey>,
        settings: AppSettings, // Pass AppSettings to the task
        sender: UnboundedSender<PrecalcResult>,
    ) {
        log::info!("Starting key generation and address precalculation...");

        // Define the async logic within a block
        let task_result = async {
             // 1. Generate New Mint Keypair
             let mint_keypair = Keypair::new();
             let mint_pubkey = mint_keypair.pubkey();
             log::info!("Generated new Mint Keypair. Pubkey: {}", mint_pubkey);

             // --- Save the generated keypair ---
             let keypair_filename = format!("mint_{}.json", mint_pubkey);
             log::info!("Saving generated mint keypair to: {}", keypair_filename);
             let keypair_bytes = mint_keypair.to_bytes().to_vec();
             let keypair_json = serde_json::to_string(&keypair_bytes)
                 .map_err(|e| anyhow!("Failed to serialize generated keypair: {}", e))?;
             
             let keypair_filename_clone = keypair_filename.clone();
             tokio::task::spawn_blocking(move || {
                 std::fs::write(&keypair_filename_clone, keypair_json)
             }).await.map_err(|e| anyhow!("Failed to join spawn_blocking for keypair write: {}", e))? // Handle JoinError
                  .map_err(|e| anyhow!("Failed to write keypair file {}: {}", keypair_filename, e))?; // Handle original std::io::Error
             log::info!("✅ Mint keypair saved successfully.");
             // ----------------------------------

             // 2. Get Payer Pubkey (Parent)
             let payer_pubkey = match parent_pubkey_str_option {
                 Some(ref pk_str) => Pubkey::from_str(pk_str)
                     .map_err(|e| anyhow!("Invalid Parent Pubkey String provided: {}", e))?,
                 None => return Err(anyhow!("Parent pubkey not available (failed loading from .env?)")),
             };
             log::debug!("Using Payer Pubkey: {}", payer_pubkey);
             log::debug!("Using {} Zombie Pubkeys.", zombie_pubkeys.len());

             // 3. Get Minter Pubkey from settings
             let minter_pubkey = AppSettings::get_pubkey_from_privkey_str(&settings.dev_wallet_private_key)
                 .and_then(|s| Pubkey::from_str(&s).ok())
                 .ok_or_else(|| anyhow!("Failed to load/parse minter_wallet_private_key from settings."))?;
             log::info!("Using Minter Pubkey: {}", minter_pubkey);

             // 4. Calculate Dynamic PDAs (using functions from pump_instruction_builders)
             let (bonding_curve_pubkey, _) = find_bonding_curve_pda(&mint_pubkey)
                 .map_err(|e| anyhow!("Failed to find bonding curve PDA: {}", e))?;
             let (metadata_pubkey, _) = find_metadata_pda(&mint_pubkey)
                 .map_err(|e| anyhow!("Failed to find metadata PDA: {}", e))?;

             // 5. Calculate Dynamic ATAs
             let bonding_curve_vault_ata = get_associated_token_address(&bonding_curve_pubkey, &mint_pubkey);
             let payer_ata = get_associated_token_address(&payer_pubkey, &mint_pubkey);
             let minter_ata = get_associated_token_address(&minter_pubkey, &mint_pubkey); // Calculate Minter ATA
             let zombie_atas: Vec<Pubkey> = zombie_pubkeys.iter()
                 .map(|zombie_pk| get_associated_token_address(zombie_pk, &mint_pubkey))
                 .collect();

             // 6. Collect All Addresses into HashSet for uniqueness
             let mut all_addresses = HashSet::new();

             // Static Programs & Sysvars
             all_addresses.insert(PUMPFUN_PROGRAM_ID);
             all_addresses.insert(spl_token::ID); // Use direct ID
             all_addresses.insert(spl_associated_token_account::ID); // Use direct ID
             all_addresses.insert(system_program::ID); // Use direct ID
             all_addresses.insert(sysvar::rent::ID); // Use direct ID
             all_addresses.insert(compute_budget::ID); // Use direct ID
             all_addresses.insert(METADATA_PROGRAM_ID);

             // Static Pump.fun Accounts
             all_addresses.insert(GLOBAL_STATE_PUBKEY);
             all_addresses.insert(PUMPFUN_MINT_AUTHORITY);
             all_addresses.insert(FEE_RECIPIENT_PUBKEY);
             all_addresses.insert(EVENT_AUTHORITY_PUBKEY);

             // Dynamic PDAs
             all_addresses.insert(bonding_curve_pubkey);
             all_addresses.insert(metadata_pubkey);

             // Wallets & Their ATAs
             all_addresses.insert(payer_pubkey);
             all_addresses.insert(payer_ata);
             all_addresses.insert(minter_pubkey); // Add Minter Pubkey
             all_addresses.insert(minter_ata);    // Add Minter ATA
             all_addresses.insert(bonding_curve_vault_ata); // Add Bonding Curve Vault ATA

             for (zombie_pk, zombie_ata) in zombie_pubkeys.iter().zip(zombie_atas.iter()) {
                 all_addresses.insert(*zombie_pk);
                 all_addresses.insert(*zombie_ata);
             }


             // Add Jito Tip Account if valid (Using settings now)
             match settings.get_jito_tip_account_pubkey() {
                 Ok(tip_acc_pk) => {
                     log::debug!("Adding Jito Tip Account {} to precalculated addresses", tip_acc_pk);
                     all_addresses.insert(tip_acc_pk);
                 }
                 Err(_) => {
                     // Log the actual value from settings if parsing failed
                     if !settings.jito_tip_account.is_empty() {
                         // Use settings.jito_tip_account directly for the warning message
                         log::warn!("JITO_TIP_ACCOUNT setting contains invalid pubkey '{}', not adding to ALT.", settings.jito_tip_account);
                     } else {
                         log::debug!("JITO_TIP_ACCOUNT setting is empty, not adding to ALT.");
                     }
                 }
             }
             // Removed stray 'else' block and fixed variable name/scope

             // Ensure mint_pubkey is added (was previously added redundantly later)
             all_addresses.insert(mint_pubkey);

             // Calculated Dynamic Addresses (Most were added earlier, removed redundant block below)
             // Removed redundant insertions from lines 2516-2523 as they were added above

             let sorted_addresses: Vec<Pubkey> = all_addresses.into_iter().collect(); // Keep collection logic
             // No need to sort here, ALT doesn't require sorted input
             log::info!("Precalculated {} unique addresses.", sorted_addresses.len());

             Ok((mint_pubkey.to_string(), sorted_addresses))
        }.await; // Await the result of the async block execution

        // Send result back to UI thread
        match task_result { // task_result now holds the Result<(String, Vec<Pubkey>), anyhow::Error>
            Ok((mint_pk_str, addresses)) => {
                let _ = sender.send(PrecalcResult::Success(mint_pk_str, addresses));
            }
            Err(e) => {
                log::error!("ALT Precalculation task failed: {}", e);
                let _ = sender.send(PrecalcResult::Failure(e.to_string()));
            }
        }
    }

    // --- ALT Task 2: Create, Extend, Freeze ALT & Save Address ---
    async fn create_alt_task(
        rpc_url: String,
        parent_private_key_str: String, // Added argument
        addresses_to_add: Vec<Pubkey>,
        // Update sender type to use the new status enum
        sender: UnboundedSender<AltCreationStatus>
    ) {
        // Send Starting status
        let _ = sender.send(AltCreationStatus::Starting);

        log::info!(
            "Starting ALT creation task with {} addresses.",
            addresses_to_add.len()
        );

        let task_result: Result<Pubkey, anyhow::Error> = async {
             // --- Setup --- 
            // Load parent keypair from argument
            if parent_private_key_str.is_empty() {
                return Err(anyhow!("Parent wallet private key is empty in settings"));
            }
            let pk_bytes = bs58::decode(&parent_private_key_str)
                .into_vec()
                .map_err(|e| anyhow!("Invalid base58 format for parent wallet private key: {}", e))?;
            let payer = Keypair::from_bytes(&pk_bytes)
                .map_err(|e| anyhow!("Failed to create parent Keypair from bytes: {}", e))?;
            let payer_pubkey = payer.pubkey();
            log::debug!("ALT Payer loaded: {}", payer_pubkey);

            // Create Async RPC client
            let client = AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
            log::debug!("Async RPC client created.");

            // Get recent slot for deriving ALT address
            let recent_slot = client
                .get_slot_with_commitment(CommitmentConfig::confirmed())
                .await
                .map_err(|e| anyhow!("Failed to get recent slot: {}", e))?;
            log::debug!("Got recent slot: {}", recent_slot);
            
            // Derive ALT address and authority
            let (lookup_table_ix, lookup_table_address) = alt_instruction::create_lookup_table(
                payer_pubkey, // Authority
                payer_pubkey, // Payer
                recent_slot, 
            );
            log::info!("Derived ALT Address: {}", lookup_table_address);

            // Define constants for the helper logic (since the helper edit might have failed)
            const PRIORITY_FEE_MICROLAMPORTS: u64 = 2_000_000; // Reverted: 0.002 SOL
            const COMPUTE_UNIT_LIMIT: u32 = 1_400_000;         // Increased limit
            let delay_between_extends = Duration::from_millis(500); // 0.5s delay

            // --- 1. Send Create Instruction (with persistent retries) ---
            let create_label = "Create ALT".to_string();
            let create_sig: Signature; // Declare signature variable outside loop
            loop { // Loop until send_alt_tx_internal succeeds
                log::info!("Attempting to send/confirm {} instruction...", create_label);
                let _ = sender.send(AltCreationStatus::Sending(create_label.clone()));
                match Self::send_alt_tx_internal(
                    &client,
                    &payer,
                    lookup_table_ix.clone(), // Clone instruction for potential retry
                    &create_label,
                    PRIORITY_FEE_MICROLAMPORTS,
                    COMPUTE_UNIT_LIMIT
                ).await {
                    Ok(sig) => {
                        create_sig = sig; // Store signature on success
                        log::info!("{} transaction confirmed.", create_label);
                        let _ = sender.send(AltCreationStatus::Confirmed(create_label.clone(), create_sig));
                        break; // Exit loop on success
                    }
                    Err(e) => {
                        log::error!("Attempt failed for {}: {}. Retrying after delay...", create_label, e);
                        // Send failure status for this attempt, but process continues
                        let _ = sender.send(AltCreationStatus::Failure(format!("Attempt failed for {}: {}", create_label, e)));
                        // Use a longer delay for persistent retries
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
            sleep(delay_between_extends).await; // Wait after successful confirmation

            // --- 2. Send Extend Instructions (Batched) ---
            let num_extend_batches = (addresses_to_add.len() + MAX_ADDRESSES_PER_EXTEND - 1) / MAX_ADDRESSES_PER_EXTEND;
            for (i, chunk) in addresses_to_add.chunks(MAX_ADDRESSES_PER_EXTEND).enumerate() {
                let extend_label = format!("Extend ALT Batch {}/{}", i + 1, num_extend_batches);
                log::info!(
                    "Sending {} ({} addresses)...",
                    extend_label,
                    chunk.len()
                );
                let _ = sender.send(AltCreationStatus::Sending(extend_label.clone())); // Send status

                let extend_ix = alt_instruction::extend_lookup_table(
                    lookup_table_address, 
                    payer_pubkey, // Authority
                    Some(payer_pubkey), // Payer
                    chunk.to_vec(), // Addresses to add in this chunk
                );

                // Get fresh blockhash for *each* extend - Removed, handled by send_alt_tx_internal
                // let extend_blockhash = client.get_latest_blockhash().await?;

                // Add persistent retry loop for each extend batch
                let extend_sig: Signature;
                loop {
                    log::info!("Attempting to send/confirm {}...", extend_label);
                    let _ = sender.send(AltCreationStatus::Sending(extend_label.clone()));
                    match Self::send_alt_tx_internal(
                        &client,
                        &payer,
                        extend_ix.clone(), // Clone instruction for potential retry
                        &extend_label,
                        PRIORITY_FEE_MICROLAMPORTS,
                        COMPUTE_UNIT_LIMIT
                    ).await {
                        Ok(sig) => {
                            extend_sig = sig;
                            log::info!("{} confirmed.", extend_label);
                            let _ = sender.send(AltCreationStatus::Confirmed(extend_label.clone(), extend_sig));
                            break; // Exit loop on success
                        }
                        Err(e) => {
                            log::error!("Attempt failed for {}: {}. Retrying after delay...", extend_label, e);
                            let _ = sender.send(AltCreationStatus::Failure(format!("Attempt failed for {}: {}", extend_label, e)));
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
                sleep(delay_between_extends).await; // Wait after successful confirmation
            }
            log::info!("All Extend ALT instructions confirmed.");
            
            // --- 3. Send Freeze Instruction (SKIPPED as per user request) ---
            // log::info!("Skipping ALT Freeze instruction as per user request.");
            // let _ = sender.send(AltCreationStatus::Skipped("Freeze ALT".to_string()));
            // // The following block was for freezing the ALT:
            // let freeze_label = "Freeze ALT".to_string();
            // // Removed: let freeze_blockhash = client.get_latest_blockhash().await?;
            // let freeze_ix = alt_instruction::freeze_lookup_table(
            //     lookup_table_address,
            //     payer_pubkey, // Authority
            // );
            // let freeze_sig: Signature;
            // loop {
            //     log::info!("Attempting to send/confirm {} instruction...", freeze_label);
            //     let _ = sender.send(AltCreationStatus::Sending(freeze_label.clone()));
            //     match Self::send_alt_tx_internal(
            //         &client,
            //         &payer,
            //         freeze_ix.clone(), // Clone instruction for potential retry
            //         &freeze_label,
            //         PRIORITY_FEE_MICROLAMPORTS,
            //         COMPUTE_UNIT_LIMIT
            //     ).await {
            //         Ok(sig) => {
            //             freeze_sig = sig;
            //             log::info!("{} transaction confirmed.", freeze_label);
            //             let _ = sender.send(AltCreationStatus::Confirmed(freeze_label.clone(), freeze_sig));
            //             break; // Exit loop on success
            //         }
            //         Err(e) => {
            //             log::error!("Attempt failed for {}: {}. Retrying after delay...", freeze_label, e);
            //             let _ = sender.send(AltCreationStatus::Failure(format!("Attempt failed for {}: {}", freeze_label, e)));
            //             tokio::time::sleep(Duration::from_secs(5)).await;
            //         }
            //     }
            // }

            // --- 4. Save ALT Address to File ---
            let file_path = "alt_address.txt";
            log::info!("Saving ALT address {} to {}", lookup_table_address, file_path);
            let mut file = std::fs::File::create(file_path)
                .map_err(|e| anyhow!("Failed to create file {}: {}", file_path, e))?;
            file.write_all(lookup_table_address.to_string().as_bytes())
                .map_err(|e| anyhow!("Failed to write to file {}: {}", file_path, e))?;
            log::info!("Successfully saved ALT address to {}.", file_path);
            
            Ok(lookup_table_address)
        }.await;

        // --- Send Final Result ---
        match task_result {
            Ok(alt_address) => {
                // Send final success status
                let _ = sender.send(AltCreationStatus::Success(alt_address));
            }
            Err(e) => {
                log::error!("ALT Creation task failed: {}", e);
                // Send final failure status
                let _ = sender.send(AltCreationStatus::Failure(e.to_string()));
            }
        }
    }

    // --- ALT Task 3: Deactivate & Close ALT ---
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
                return Err(anyhow!("Parent wallet private key (authority) is empty in settings."));
            }
            let pk_bytes = bs58::decode(&authority_private_key_str)
                .into_vec()
                .map_err(|e| anyhow!("Invalid base58 format for authority private key: {}", e))?;
            let authority = Keypair::from_bytes(&pk_bytes)
                .map_err(|e| anyhow!("Failed to create authority Keypair from bytes: {}", e))?;
            
            let client = AsyncRpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
            log::debug!("Async RPC client created for ALT deactivation/close.");

            // --- 1. Deactivate Lookup Table ---
            let deactivate_label = format!("Deactivate ALT {}", alt_address);
            let _ = status_sender.send(Ok(format!("Attempting: {}", deactivate_label)));
            log::info!("Attempting: {}", deactivate_label);

            let deactivate_ix = alt_instruction::deactivate_lookup_table(
                alt_address,
                authority.pubkey(),
            );
            
            match Self::send_alt_tx_internal(
                &client,
                &authority,
                deactivate_ix,
                &deactivate_label,
                100_000, // Example priority fee
                200_000  // Example compute limit
            ).await {
                Ok(sig) => {
                    let msg = format!("Deactivation transaction sent for {}: {}", alt_address, sig);
                    log::info!("{}", msg);
                    let _ = status_sender.send(Ok(msg));
                }
                Err(e) => {
                    let err_msg = format!("Failed to send deactivation for {}: {}", alt_address, e);
                    log::error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg.clone()));
                    // For a robust solution, one might return Err here.
                    // For now, we log and proceed to attempt closing.
                }
            }

            let _ = status_sender.send(Ok(format!("Waiting briefly after deactivation attempt for {}...", alt_address)));
            tokio::time::sleep(Duration::from_secs(10)).await; 

            // --- 2. Close Lookup Table ---
            let close_label = format!("Close ALT {}", alt_address);
            let _ = status_sender.send(Ok(format!("Attempting: {}", close_label)));
            log::info!("Attempting: {}", close_label);

            let close_ix = alt_instruction::close_lookup_table(
                alt_address,
                authority.pubkey(),
                authority.pubkey(), 
            );

            match Self::send_alt_tx_internal(
                &client,
                &authority,
                close_ix,
                &close_label,
                100_000, 
                200_000  
            ).await {
                Ok(sig) => {
                    let msg = format!("Close transaction sent for {}: {}", alt_address, sig);
                    log::info!("{}", msg);
                    let _ = status_sender.send(Ok(msg));
                    // Implicitly returns ()
                }
                Err(e) => {
                    let err_msg = format!("Failed to send close transaction for {}: {}", alt_address, e);
                    log::error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg.clone())); // Send error status
                    // Implicitly returns (), error is sent via status_sender
                }
            }
            log::info!("Attempting: {}", deactivate_label);

            let deactivate_ix = alt_instruction::deactivate_lookup_table(
                alt_address,
                authority.pubkey(),
            );
            
            match Self::send_alt_tx_internal(
                &client,
                &authority,
                deactivate_ix,
                &deactivate_label,
                100_000, // Example priority fee, adjust as needed
                200_000  // Example compute limit, adjust as needed
            ).await {
                Ok(sig) => {
                    let msg = format!("Deactivation transaction sent for {}: {}", alt_address, sig);
                    log::info!("{}", msg);
                    let _ = status_sender.send(Ok(msg));
                }
                Err(e) => {
                    let err_msg = format!("Failed to send deactivation for {}: {}", alt_address, e);
                    log::error!("{}", err_msg);
                    // Don't return error yet, try to close anyway or let user retry deactivation.
                    // For now, we proceed to attempt close. A more robust flow might stop here.
                    let _ = status_sender.send(Err(err_msg.clone()));
                    // return Err(anyhow!(err_msg)); // Optionally, uncomment to stop if deactivation fails
                }
            }

            // It's recommended to wait for the deactivation to propagate or for the table to no longer be "hot".
            // This can take some time (e.g., up to LOOKUP_TABLE_MAX_AGE_SLOTS slots, which is ~2 days if not used).
            // For an immediate close attempt after deactivation, it might fail if the table is still considered active.
            // A simple delay here is a basic approach. Polling table state would be more robust.
            let _ = status_sender.send(Ok(format!("Waiting briefly after deactivation attempt for {}...", alt_address)));
            tokio::time::sleep(Duration::from_secs(10)).await; // Wait 10 seconds

            // --- 2. Close Lookup Table ---
            let close_label = format!("Close ALT {}", alt_address);
            let _ = status_sender.send(Ok(format!("Attempting: {}", close_label)));
            log::info!("Attempting: {}", close_label);

            let close_ix = alt_instruction::close_lookup_table(
                alt_address,
                authority.pubkey(),
                authority.pubkey(), // Sol will be returned to the authority
            );

            match Self::send_alt_tx_internal(
                &client,
                &authority,
                close_ix,
                &close_label,
                100_000, // Example priority fee
                200_000  // Example compute limit
            ).await {
                Ok(sig) => {
                    let msg = format!("Close transaction sent for {}: {}", alt_address, sig);
                    log::info!("{}", msg);
                    let _ = status_sender.send(Ok(msg));
                    Ok(()) // Final success
                }
                Err(e) => {
                    let err_msg = format!("Failed to send close transaction for {}: {}", alt_address, e);
                    log::error!("{}", err_msg);
                    Err(anyhow!(err_msg)) // Return error if close fails
                }
            }
        }.await;

        match task_result {
            Ok(_) => {
                let final_msg = format!("Deactivation and Close process completed for ALT: {}. Check explorer for final status.", alt_address);
                log::info!("{}", final_msg);
                let _ = status_sender.send(Ok(final_msg));
            }
            Err(e) => {
                let err_msg = format!("ALT Deactivation/Close task failed for {}: {}", alt_address, e);
                log::error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg));
            }
        }
    }

    // --- Internal Helper for Sending ALT Transactions (with Retries) ---
    async fn send_alt_tx_internal(
         client: &AsyncRpcClient,
         payer: &Keypair,
         instruction: solana_sdk::instruction::Instruction,
         label: &str,
         // Remove initial blockhash parameter, get fresh one inside loop
         // latest_blockhash: solana_sdk::hash::Hash,
         priority_fee_microlamports: u64,
         compute_unit_limit: u32
    ) -> Result<solana_sdk::signature::Signature, anyhow::Error> {
        const MAX_RETRIES: u8 = 3;
        const RETRY_DELAY: Duration = Duration::from_secs(2); // Use tokio::time::Duration if needed

        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                log::warn!("Retrying {} transaction (Attempt {}/{})", label, attempt + 1, MAX_RETRIES);
                tokio::time::sleep(RETRY_DELAY).await;
            }

            // Get a fresh blockhash for each attempt
            let current_blockhash = match client.get_latest_blockhash().await {
                 Ok(bh) => bh,
                 Err(e) => {
                     let err = anyhow!("Failed to get blockhash for {} (Attempt {}): {}", label, attempt + 1, e);
                     log::error!("{}", err);
                     last_error = Some(err);
                     continue; // Try getting blockhash again on next attempt
                 }
             };

            let priority_fee_ix = compute_budget::ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports);
            let compute_limit_ix = compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit);

            // Create Version 0 message with current blockhash
            // Clone instruction in case it's modified by try_compile or needed for retry
            let message_result = MessageV0::try_compile(
                &payer.pubkey(),
                &[compute_limit_ix, priority_fee_ix, instruction.clone()],
                &[], // ALT creation/extension doesn't use an ALT itself
                current_blockhash,
            );

            let message = match message_result {
                 Ok(msg) => VersionedMessage::V0(msg),
                 Err(e) => {
                     let err = anyhow!("Failed to compile V0 message for {} (Attempt {}): {}", label, attempt + 1, e);
                     log::error!("{}", err);
                     last_error = Some(err);
                     // Don't retry compilation errors
                     return Err(last_error.unwrap());
                 }
             };

            // Create and sign VersionedTransaction
            let tx = match VersionedTransaction::try_new(message, &[payer]) {
                 Ok(signed_tx) => signed_tx,
                 Err(e) => {
                     let err = anyhow!("Failed to sign {} tx (Attempt {}): {}", label, attempt + 1, e);
                     log::error!("{}", err);
                     last_error = Some(err);
// Function moved inside impl PumpFunApp block
                     // Don't retry signing errors
                     return Err(last_error.unwrap());
                 }
             };
            let tx_signature = tx.signatures[0];

            info!("Attempting to send/confirm {} tx (Attempt {}, Sig: {})...", label, attempt + 1, tx_signature);

            // Send and confirm
            match client.send_and_confirm_transaction_with_spinner(&tx).await {
                Ok(sig) => {
                    // Success! Return the signature.
                    return Ok(sig);
                }
                Err(e) => {
                    // Check if the error is likely recoverable (timeout, blockhash expiration)
                    let error_string = e.to_string();
                    let is_recoverable = error_string.contains("blockhash not found") ||
                                         error_string.contains("timed out") ||
                                         error_string.contains("Unable to confirm transaction");

                    log::error!("Failed attempt {} for {} tx (Sig: {}): {}", attempt + 1, label, tx_signature, e);
                    last_error = Some(e.into()); // Store the error

                    if !is_recoverable {
                        // If error is not recoverable (e.g., insufficient funds, program error), stop retrying
                        log::error!("Unrecoverable error encountered for {}. Stopping retries.", label);
                        break;
                    }
                    // Otherwise, the loop will continue to the next attempt after the delay
                }
            }
        }

        // If loop finishes without success, return the last error encountered
        Err(last_error.unwrap_or_else(|| anyhow!("{} failed after {} retries with an unknown error", label, MAX_RETRIES)))
    }

    // --- Coordinated Launch View UI ---
    fn show_launch_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("🚀 Coordinated Launch");
        ui.separator();
        
        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow) // Use a background from the theme
            .show(ui, |ui| {
                ui.label("Create a new token and perform initial buys from parent and zombie wallets in a single (potentially bundled) transaction.");
                ui.add_space(8.0);
                ui.group(|ui| { // Group the warning for better visual separation
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("⚠️ WARNING: You MUST create an ALT TABLE using the 'ALT Management' section first if you intend to use an ALT for this launch.").strong().color(ui.style().visuals.warn_fg_color).size(13.0));
                    });
                });
                ui.add_space(12.0);

                egui::Grid::new("launch_grid")
                    .num_columns(2)
                    .spacing([25.0, 12.0])
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label("Token Name:");
                        ui.text_edit_singleline(&mut self.launch_token_name);
                        ui.end_row();

                        ui.label("Token Symbol:");
                        ui.text_edit_singleline(&mut self.launch_token_symbol);
                        ui.end_row();

                        ui.label("Token Description:");
                        ui.text_edit_multiline(&mut self.launch_token_description);
                        ui.end_row();

                        ui.label("Image URL:").on_hover_text("Select a local image file (.png, .jpg, .gif) or paste a direct URL.");
                        ui.horizontal(|ui| {
                            let image_input_field = egui::TextEdit::singleline(&mut self.launch_token_image_url)
                                .hint_text("e.g., ./my_image.png or https://...")
                                .desired_width(ui.available_width() * 0.75);
                            ui.add(image_input_field);
                            if ui.button("Browse...").clicked() {
                                if let Some(path) = FileDialog::new()
                                    .add_filter("Images", &["png", "jpg", "jpeg", "gif"])
                                    .pick_file()
                                {
                                    self.launch_token_image_url = path.to_string_lossy().to_string();
                                    log::info!("Selected image file: {}", self.launch_token_image_url);
                                }
                            }
                        });
                        ui.end_row();
                        
                        ui.label("Twitter URL (Optional):");
                        ui.text_edit_singleline(&mut self.launch_twitter);
                        ui.end_row();

                        ui.label("Telegram URL (Optional):");
                        ui.text_edit_singleline(&mut self.launch_telegram);
                        ui.end_row();

                        ui.label("Website URL (Optional):");
                        ui.text_edit_singleline(&mut self.launch_website);
                        ui.end_row();

                        ui.label("Dev Buy (SOL):").on_hover_text("Amount of SOL the parent wallet will spend to buy initial tokens.");
                        ui.add(
                            egui::DragValue::new(&mut self.launch_dev_buy_sol)
                                .speed(0.001)
                                .range(0.0..=f64::MAX)
                                .max_decimals(9)
                                .prefix("◎ ")
                        );
                        ui.end_row();

                        ui.label("Zombie Buy (SOL Each):").on_hover_text("Amount of SOL *each* zombie wallet (from keys.json) will spend.");
                        ui.add(
                            egui::DragValue::new(&mut self.launch_zombie_buy_sol)
                                .speed(0.001)
                                .range(0.0..=f64::MAX)
                                .max_decimals(9)
                                .prefix("◎ ")
                        );
                        ui.end_row();

                        ui.label("Slippage (bps):");
                        ui.label("Controlled by .env (BUY_SLIPPAGE)");
                        ui.end_row();

                        ui.label("Priority Fee (μLamports):");
                        ui.label("Controlled by .env (BUY_PRIORITY_FEE)");
                        ui.end_row();

                        ui.label("ALT Address (Optional):").on_hover_text("Provide an existing Address Lookup Table to use for the launch transaction.");
                        ui.horizontal(|ui| {
                            let alt_id = ui.make_persistent_id("alt_combo_launch");
                            egui::ComboBox::from_id_source(alt_id)
                                .selected_text(if self.launch_alt_address.is_empty() { "None" } else { self.launch_alt_address.as_str() })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.launch_alt_address, "".to_string(), "None");
                                    for alt_addr in &self.available_alt_addresses {
                                        ui.selectable_value(&mut self.launch_alt_address, alt_addr.clone(), alt_addr);
                                    }
                                });
                            if ui.button("🔄").on_hover_text("Reload alt_address.txt").clicked() {
                                self.load_available_alts();
                            }
                        });
                        ui.end_row();

                        ui.label("Token CA :").on_hover_text("Enter Token CA (mint address) to use an existing token. OR, select a mint keypair .json file. OR, select 'Generate New' to create a new token.");
                         ui.horizontal(|ui| {
                             let mint_kp_id = ui.make_persistent_id("mint_kp_combo_launch");
                             egui::ComboBox::from_id_source(mint_kp_id)
                                  .selected_text(if self.launch_mint_keypair_path.is_empty() { "Generate New" } else { self.launch_mint_keypair_path.as_str() })
                                  .show_ui(ui, |ui| {
                                     ui.selectable_value(&mut self.launch_mint_keypair_path, "".to_string(), "Generate New");
                                     for kp_path in &self.available_mint_keypairs {
                                          ui.selectable_value(&mut self.launch_mint_keypair_path, kp_path.clone(), kp_path);
                                      }
                                  });
                                 if ui.button("🔄").on_hover_text("Rescan directory for keypair files").clicked() {
                                     self.load_available_mint_keypairs();
                                 }
                             });
                        ui.end_row();

                        ui.label("Use Jito Bundle:").on_hover_text("Send Create+DevBuy and Zombie Buys as a Jito bundle for potentially better execution.");
                        let jito_configured = !self.app_settings.selected_jito_block_engine_url.is_empty();
                        let jito_tooltip = if jito_configured {
                            "Submit launch as a single atomic bundle via Jito. Ensure JITO_TIP_ACCOUNT is set in .env."
                        } else {
                            "Jito Block Engine URL not configured in Settings/.env. Bundle cannot be sent."
                        };
                        ui.add_enabled(jito_configured, egui::Checkbox::new(&mut self.launch_use_jito_bundle, "Send via Jito")).on_hover_text(jito_tooltip);
                        ui.end_row();

                        ui.label("Jito Tip (SOL):");
                        ui.label("Controlled by .env (JITO_TIP_AMOUNT_SOL)");
                        ui.end_row();

                        ui.label("Simulate Only:").on_hover_text("Build the transactions but do not send the bundle.");
                        ui.checkbox(&mut self.launch_simulate_only, "Don't send bundle");
                        ui.end_row();
                    }); // End of Grid

                ui.add_space(20.0);

                ui.vertical_centered(|ui| {
                    let launch_button = egui::Button::new(egui::RichText::new("🚀 Launch Token").size(16.0))
                        .min_size(egui::vec2(220.0, 40.0)); // Made button larger and text bigger
                    let enabled = !self.launch_in_progress
                                  && !self.launch_token_name.trim().is_empty()
                                  && !self.launch_token_symbol.trim().is_empty()
                                  && self.launch_dev_buy_sol > 0.0;

                    // Add some space before the button
                    ui.add_space(10.0);

                    if ui.add_enabled(enabled, launch_button).clicked() {
                        log::info!("Launch Token button clicked.");
                        self.last_operation_result = Some(Ok("Initiating coordinated launch...".to_string()));
                        self.launch_in_progress = true;
                        self.launch_log_messages.clear();
                        self.launch_log_messages.push("🚀 Launch Initiated...".to_string()); // Initial log
                        let sender = self.launch_status_sender.clone();
                        let params = LaunchParams {
                             name: self.launch_token_name.trim().to_string(),
                             symbol: self.launch_token_symbol.trim().to_string(),
                             description: self.launch_token_description.trim().to_string(),
                             image_url: self.launch_token_image_url.trim().to_string(),
                             dev_buy_sol: self.launch_dev_buy_sol,
                             zombie_buy_sol: self.launch_zombie_buy_sol,
                             slippage_bps: (self.app_settings.default_slippage_percent * 100.0) as u64,
                             priority_fee: (self.app_settings.default_priority_fee_sol * 1_000_000_000.0) as u64,
                             alt_address_str: self.launch_alt_address.trim().to_string(),
                             mint_keypair_path_str: self.launch_mint_keypair_path.trim().to_string(),
                             loaded_wallet_data: self.loaded_wallet_data.clone(),
                             minter_private_key_str: self.app_settings.dev_wallet_private_key.clone(),
                             use_jito_bundle: self.launch_use_jito_bundle,
                             jito_block_engine_url: self.app_settings.selected_jito_block_engine_url.clone(),
                             twitter: self.launch_twitter.trim().to_string(),
                             telegram: self.launch_telegram.trim().to_string(),
                             website: self.launch_website.trim().to_string(),
                             simulate_only: self.launch_simulate_only,
                             main_tx_priority_fee_micro_lamports: self.app_settings.default_main_tx_priority_fee_micro_lamports, // Added - Ensure this field exists in AppSettings
                             jito_actual_tip_sol: self.app_settings.default_jito_actual_tip_sol,                 // Added - Ensure this field exists in AppSettings
                        };
                        log::info!("Spawning launch_task with params: {:?}", params);
                         self.launch_in_progress = true;
                         let launch_params_moved = params;
                         let launch_sender_moved = sender;
                         let launch_jito_url_moved = self.app_settings.selected_jito_block_engine_url.clone();

                         tokio::spawn(async move {
                             let mint_keypair_path_option = if launch_params_moved.mint_keypair_path_str.is_empty() {
                                 None
                             } else {
                                 Some(launch_params_moved.mint_keypair_path_str)
                             };
                             let result = crate::commands::launch_buy::launch_buy(
                                 launch_params_moved.name,
                                 launch_params_moved.symbol,
                                 launch_params_moved.description,
                                 launch_params_moved.image_url,
                                 launch_params_moved.dev_buy_sol,
                                 launch_params_moved.zombie_buy_sol,
                                 launch_params_moved.slippage_bps,
                                 launch_params_moved.alt_address_str,
                                 mint_keypair_path_option,
                                 launch_params_moved.minter_private_key_str,
                                 launch_params_moved.loaded_wallet_data,
                                 launch_params_moved.simulate_only,
                                 launch_sender_moved.clone(),
                                 launch_jito_url_moved,
                                 launch_params_moved.main_tx_priority_fee_micro_lamports, // Added
                                 launch_params_moved.jito_actual_tip_sol,                 // Added
                             ).await;
                             if let Err(e) = result {
                                 log::error!("Launch task finished with error: {}", e);
                                  // Send failure back to UI via channel
                                 let _ = launch_sender_moved.send(LaunchStatus::Failure(format!("Launch task error: {}", e)));
                             } else {
                                 log::info!("Launch task finished successfully.");
                                 // Success is usually reported by specific steps within launch_buy itself via the sender,
                                 // but if it completes without specific success message, send a generic one.
                                 // However, launch_buy is expected to send LaunchStatus::Success or Failure.
                             }
                         });
                    }
                    if self.launch_in_progress {
                        ui.add_space(5.0);
                        ui.horizontal(|ui_h| {
                            ui_h.spinner();
                            ui_h.label(egui::RichText::new("Launching token...").italics());
                        });
                        ui.add_space(5.0);
                    }
                }); // End of Action Button vertical_centered

                ui.add_space(10.0);
                ui.separator();
                ui.label("Launch Log:");
                egui::ScrollArea::vertical()
                    .max_height(150.0)
                    .auto_shrink([false, true])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.vertical(|ui| {
                            for msg in &self.launch_log_messages {
                                ui.monospace(msg);
                            }
                        });
                    });
            }); // End of main Frame for launch view
     } // Closing show_launch_view
 
 // --- Fund Volume Bot Wallets Task ---
     fn fund_volume_bot_wallets_task( // Changed to sync fn
        rpc_url: String,
        parent_keypair_str: String,
        wallets_to_fund: Vec<LoadedWalletInfo>,
        lamports_per_wallet: u64,
        status_sender: UnboundedSender<Result<String, String>>,
    ) {
        log::info!(
            "Starting task to fund {} volume bot wallets with {} lamports each.",
            wallets_to_fund.len(),
            lamports_per_wallet
        );

        let parent_keypair = match crate::commands::utils::load_keypair_from_string(&parent_keypair_str, "Parent") {
            Ok(kp) => kp,
            Err(e) => {
                let err_msg = format!("Failed to load parent keypair for funding: {}", e);
                log::error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg));
                return;
            }
        };

        // Use AsyncRpcClient for an async task
        // Use the blocking RpcClient as this function is spawned via spawn_blocking
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        // Check parent balance once
        let total_needed = lamports_per_wallet * wallets_to_fund.len() as u64;
        // Add a buffer for transaction fees (e.g., 0.001 SOL per transaction)
        let fee_buffer_per_tx = sol_to_lamports(0.00001); // Small fee per tx
        let total_fees_estimate = fee_buffer_per_tx * wallets_to_fund.len() as u64;
        let grand_total_needed = total_needed + total_fees_estimate;

        // get_balance is not async on AsyncRpcClient directly, need to await if it were,
        // but for balance check, a blocking call here is acceptable if wrapped,
        // or use an async equivalent if available.
        // For simplicity, let's assume this part can remain somewhat blocking or is handled.
        // The main issue is .await on non-future calls later.
        // Let's assume balance check is fine for now and focus on .await calls.
        // Re-evaluating: get_balance is indeed not async on the non-blocking client.
        // This part needs to be re-thought or done with a blocking task if balance check is slow.
        // For now, we'll proceed assuming the main goal is to fix .await on send_and_confirm and get_latest_blockhash.

        // Temporarily, we'll keep the blocking client for get_balance for this diff,
        // and focus on the .await calls. A more robust solution would use spawn_blocking for balance check.
        let blocking_rpc_client = RpcClient::new_with_commitment(
            rpc_url.clone(), // Recreate for this specific blocking call
            CommitmentConfig::confirmed(),
        );
        match blocking_rpc_client.get_balance(&parent_keypair.pubkey()) {
            Ok(balance) => {
                if balance < grand_total_needed {
                    let err_msg = format!(
                        "Insufficient SOL in parent wallet {}. Needed: {:.6} SOL ({} lamports), Available: {:.6} SOL ({} lamports)",
                        parent_keypair.pubkey(),
                        lamports_to_sol(grand_total_needed),
                        grand_total_needed,
                        lamports_to_sol(balance),
                        balance
                    );
                    log::error!("{}", err_msg);
                    let _ = status_sender.send(Err(err_msg));
                    return;
                }
                log::info!("Parent wallet balance sufficient ({} lamports) for funding {} lamports.", balance, grand_total_needed);
            }
            Err(e) => {
                let err_msg = format!("Failed to get parent wallet balance: {}", e);
                log::error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg));
                return;
            }
        }

        let mut successful_sends = 0;
        let total_wallets = wallets_to_fund.len();

        for (index, wallet_info) in wallets_to_fund.iter().enumerate() {
            let recipient_pubkey = match Pubkey::from_str(&wallet_info.public_key) {
                Ok(pk) => pk,
                Err(e) => {
                    log::error!("Invalid recipient pubkey {}: {}", wallet_info.public_key, e);
                    // Don't send individual error, will be summarized or caught by overall failure
                    continue;
                }
            };

            let instructions = vec![system_instruction::transfer(
                &parent_keypair.pubkey(),
                &recipient_pubkey,
                lamports_per_wallet,
            )];

            // This is a blocking call, no .await needed
            let latest_blockhash = match rpc_client.get_latest_blockhash() {
                Ok(bh) => bh,
                Err(e) => {
                    log::error!("Failed to get blockhash for funding tx to {}: {}", recipient_pubkey, e);
                    continue;
                }
            };
            
            let mut attempts = 0;
            let max_attempts = 3;
            let mut funding_successful_for_this_wallet = false;

            while attempts < max_attempts {
                attempts += 1;
                
                // Fetch/Re-fetch blockhash for each attempt
                let current_blockhash = match rpc_client.get_latest_blockhash() {
                    Ok(bh) => bh,
                    Err(e) => {
                        log::error!("Attempt {}: Failed to get blockhash for funding tx to {}: {}", attempts, recipient_pubkey, e);
                        if attempts >= max_attempts {
                             log::error!("Max attempts reached for getting blockhash for wallet {}", recipient_pubkey);
                        }
                        std::thread::sleep(Duration::from_millis(500)); // Wait before retrying blockhash fetch
                        continue; // Try to fetch blockhash again
                    }
                };

                let tx = Transaction::new_signed_with_payer(
                    &instructions,
                    Some(&parent_keypair.pubkey()),
                    &[&parent_keypair as &dyn Signer],
                    current_blockhash, // Use the freshly fetched blockhash
                );

                log::info!("Attempt {} to fund wallet {}/{} ({}) with blockhash {}", attempts, index + 1, total_wallets, recipient_pubkey, current_blockhash);
                match rpc_client.send_and_confirm_transaction_with_spinner(&tx) {
                    Ok(signature) => {
                        log::info!(
                            "Successfully funded wallet {}/{} ({}): ◎{} SOL. Signature: {}",
                            index + 1,
                            total_wallets,
                            recipient_pubkey,
                            lamports_to_sol(lamports_per_wallet),
                            signature
                        );
                        successful_sends += 1;
                        funding_successful_for_this_wallet = true;
                        break; // Exit retry loop on success
                    }
                    Err(e) => {
                        log::warn!(
                            "Attempt {} failed to fund wallet {}/{} ({}): {}",
                            attempts,
                            index + 1,
                            total_wallets,
                            recipient_pubkey,
                            e
                        );
                        // Check for blockhash not found error specifically
                        if e.to_string().contains("Blockhash not found") || e.to_string().contains("-32002") {
                            if attempts < max_attempts {
                                log::info!("Retrying funding for {} due to blockhash issue (attempt {}/{})", recipient_pubkey, attempts, max_attempts);
                                std::thread::sleep(Duration::from_millis(1000 + attempts * 500)); // Exponential backoff
                                continue; // Retry the while loop (will re-fetch blockhash)
                            } else {
                                log::error!("Max attempts reached for wallet {} after blockhash issues.", recipient_pubkey);
                                break; // Exit retry loop, error already logged
                            }
                        } else {
                            // For other errors, break immediately
                            break;
                        }
                    }
                }
            } // End of retry while loop

            if !funding_successful_for_this_wallet {
                 log::error!("Permanently failed to fund wallet {}/{} ({}) after {} attempts.", index + 1, total_wallets, recipient_pubkey, max_attempts);
                 // The error was already logged by the last attempt, or if blockhash fetch failed repeatedly.
            }
            // Optional: Add a small delay between transactions
            if total_wallets > 5 && index < total_wallets -1 { // Avoid delay after last one
                 std::thread::sleep(Duration::from_millis(200));
            }
        } // Closes for loop

        if successful_sends == total_wallets && total_wallets > 0 {
            let success_msg = format!(
                "Successfully funded {} volume bot wallets with ◎{} SOL each.",
                successful_sends,
                lamports_to_sol(lamports_per_wallet)
            );
            log::info!("{}", success_msg);
            let _ = status_sender.send(Ok(success_msg));
        } else if total_wallets == 0 {
             let _ = status_sender.send(Err("No wallets were provided to fund.".to_string()));
        }
        else {
            let err_msg = format!(
                "Funding partially failed. Successfully funded {} out of {} wallets.",
                successful_sends, total_wallets
);
            log::error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg.clone()));
        } // Closes else block for funding check
    } // Closes fund_volume_bot_wallets_task function
// Task to distribute a total amount of SOL from a source wallet to multiple volume bot wallets
// This is a synchronous function intended to be run with spawn_blocking
fn distribute_total_sol_to_volume_wallets_task(
    rpc_url: String,
    source_funding_keypair_str: String,
    wallets_to_fund: Vec<LoadedWalletInfo>,
    total_sol_to_distribute: f64,
    status_sender: UnboundedSender<Result<String, String>>,
) {
    log::info!(
        "Starting task to distribute a total of ◎{} SOL from source wallet to {} volume bot wallets.",
        total_sol_to_distribute,
        wallets_to_fund.len()
    );
    let _ = status_sender.send(Ok(format!("Distribution task started. Total SOL: ◎{}, Target Wallets: {}", total_sol_to_distribute, wallets_to_fund.len())));

    if wallets_to_fund.is_empty() {
        let err_msg = "No volume wallets to distribute SOL to.".to_string();
        log::error!("{}", err_msg);
        let _ = status_sender.send(Err(err_msg));
        return;
    }

    if total_sol_to_distribute <= 0.0 {
        let err_msg = "Total SOL to distribute must be greater than 0.".to_string();
        log::error!("{}", err_msg);
        let _ = status_sender.send(Err(err_msg));
        return;
    }

    let source_keypair = match crate::commands::utils::load_keypair_from_string(&source_funding_keypair_str, "FundingSource") {
        Ok(kp) => kp,
        Err(e) => {
            let err_msg = format!("Failed to load source funding keypair: {}", e);
            log::error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg));
            return;
        }
    };
    let _ = status_sender.send(Ok(format!("Funding source wallet: {}", source_keypair.pubkey())));


    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        rpc_url.clone(),
        CommitmentConfig::confirmed(),
    ));

    let sol_per_wallet = total_sol_to_distribute / wallets_to_fund.len() as f64;
    let lamports_per_wallet = sol_to_lamports(sol_per_wallet);
    let total_lamports_to_distribute = sol_to_lamports(total_sol_to_distribute);
    
    // Estimate fees (simple estimation)
    let fee_per_tx = sol_to_lamports(0.00001); // 5000 lamports + buffer
    let total_estimated_fees = fee_per_tx * wallets_to_fund.len() as u64;
    let grand_total_needed_by_source = total_lamports_to_distribute + total_estimated_fees;

    // Check source balance
    match rpc_client.get_balance(&source_keypair.pubkey()) {
        Ok(balance) => {
            if balance < grand_total_needed_by_source {
                let err_msg = format!(
                    "Insufficient SOL in source funding wallet {}. Needed: {:.6} SOL ({} lamports), Available: {:.6} SOL ({} lamports)",
                    source_keypair.pubkey(),
                    lamports_to_sol(grand_total_needed_by_source),
                    grand_total_needed_by_source,
                    lamports_to_sol(balance),
                    balance
                );
                log::error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg));
                return;
            }
            let _ = status_sender.send(Ok(format!("Source wallet balance sufficient (◎{}). Distributing ◎{:.6} to each of {} wallets.", lamports_to_sol(balance), sol_per_wallet, wallets_to_fund.len())));
        }
        Err(e) => {
            let err_msg = format!("Failed to get source funding wallet balance: {}", e);
            log::error!("{}", err_msg);
            let _ = status_sender.send(Err(err_msg));
            return;
        }
    }

    let mut successful_sends = 0;
    let total_target_wallets = wallets_to_fund.len();

    for (index, target_wallet_info) in wallets_to_fund.iter().enumerate() {
        let recipient_pubkey = match Pubkey::from_str(&target_wallet_info.public_key) {
            Ok(pk) => pk,
            Err(e) => {
                log::error!("Invalid recipient pubkey for volume wallet {}: {}. Skipping.", target_wallet_info.public_key, e);
                let _ = status_sender.send(Err(format!("Invalid pubkey for {}: {}", target_wallet_info.public_key, e)));
                continue;
            }
        };

        let _ = status_sender.send(Ok(format!("Distributing ◎{:.6} to {} ({}/{})...", sol_per_wallet, recipient_pubkey, index + 1, total_target_wallets)));

        let instructions = vec![system_instruction::transfer(
            &source_keypair.pubkey(),
            &recipient_pubkey,
            lamports_per_wallet,
        )];

        let latest_blockhash = match rpc_client.get_latest_blockhash() {
            Ok(bh) => bh,
            Err(e) => {
                log::error!("Failed to get blockhash for funding tx to {}: {}. Skipping.", recipient_pubkey, e);
                let _ = status_sender.send(Err(format!("Blockhash fail for {}: {}", recipient_pubkey, e)));
                continue;
            }
        };
        
        let tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&source_keypair.pubkey()),
            &[&source_keypair as &dyn Signer],
            latest_blockhash
        );

        match rpc_client.send_and_confirm_transaction_with_spinner(&tx) {
            Ok(signature) => {
                let msg = format!(
                    "Successfully sent ◎{:.6} to {} ({}/{}). Sig: {}",
                    sol_per_wallet, recipient_pubkey, index + 1, total_target_wallets, signature
                );
                log::info!("{}", msg);
                let _ = status_sender.send(Ok(msg));
                successful_sends += 1;
            }
            Err(e) => {
                let err_msg = format!(
                    "Failed to send ◎{:.6} to {} ({}/{}): {}",
                    sol_per_wallet, recipient_pubkey, index + 1, total_target_wallets, e
                );
                log::error!("{}", err_msg);
                let _ = status_sender.send(Err(err_msg));
            }
        }
        // Optional delay
        if total_target_wallets > 5 && index < total_target_wallets - 1 {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    if successful_sends == total_target_wallets && total_target_wallets > 0 {
        let success_msg = format!(
            "Successfully distributed ◎{:.6} SOL to each of {} volume bot wallets.",
            sol_per_wallet, successful_sends
        );
        log::info!("{}", success_msg);
        let _ = status_sender.send(Ok(success_msg));
    } else if total_target_wallets == 0 {
         let _ = status_sender.send(Err("No wallets were provided to fund.".to_string()));
    } else {
        let err_msg = format!(
            "SOL distribution partially failed. Successfully funded {} out of {} wallets.",
            successful_sends, total_target_wallets
        );
        log::error!("{}", err_msg);
        let _ = status_sender.send(Err(err_msg));
    }
}
    // This block, including a misplaced function and a premature closing brace, was removed to fix a compilation error.
// Jupiter helper method
// Misplaced Jupiter helper functions and struct were removed from here.
// They will be re-inserted at the module level.

// run_volume_bot_simulation_task is an associated async function
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
                                        match bincode::deserialize::<VersionedTransaction>(&tx_data) {
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
                                                match bincode::deserialize::<VersionedTransaction>(&tx_data) {
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
                                            match bincode::deserialize::<VersionedTransaction>(&tx_data) {
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

// Removed orphaned ScrollArea and closing brace
// --- Show Simulation V2 (Volume Bot) View ---
    fn show_simulation_v2_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("📈 Volume Bot"); // Changed icon
        ui.separator();

        // Removed duplicated function signature and heading/separator

        // Removed pre-calculation of column widths and direct .set_width calls to avoid borrow issues.
        // Letting ui.columns distribute space automatically.

        ui.columns(2, |columns| {
            // --- Left Column (Controls) ---
            // No explicit set_width for columns[0]

            // Now define content for Left Column
            egui::ScrollArea::vertical()
                .id_source("volume_bot_controls_scroll_left")
                .auto_shrink([true, true]) // Allow horizontal and vertical shrinking
                .show(&mut columns[0], |ui| {
                    // --- Wallet Generation Section ---
                    ui.label("Number of sub-wallets to generate for Volume Bot:");
                    ui.add(egui::DragValue::new(&mut self.volume_bot_num_wallets_to_generate).range(1..=100));

                    if self.volume_bot_generation_in_progress {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Generating wallets...");
                        });
                    } else {
                        if ui.button("Generate Volume Bot Wallets").clicked() {
                            if self.volume_bot_num_wallets_to_generate > 0 {
                                log::info!("Generate Volume Bot Wallets button clicked for {} wallets.", self.volume_bot_num_wallets_to_generate);
                                self.volume_bot_generation_in_progress = true;
                                self.volume_bot_wallets_generated = false;
                                self.volume_bot_wallets.clear();
                                self.last_generated_volume_wallets_file = None;
                                self.sim_log_messages.push(format!("Starting generation of {} wallets...", self.volume_bot_num_wallets_to_generate));
                                let num_to_gen = self.volume_bot_num_wallets_to_generate;
                                let status_sender = self.volume_bot_wallet_gen_status_sender.clone();
                                let wallets_sender = self.volume_bot_wallets_sender.clone();
                                tokio::spawn(async move {
                                    generate_volume_bot_wallets_task(num_to_gen, status_sender, wallets_sender).await;
                                });
                            } else {
                                self.sim_log_messages.push("Please enter a number of wallets to generate greater than 0.".to_string());
                                self.last_operation_result = Some(Err("Number of wallets must be greater than 0.".to_string()));
                            }
                        }
                    }
                    if self.volume_bot_wallets_generated {
                        if let Some(filename) = &self.last_generated_volume_wallets_file {
                            ui.label(format!("Generated {} wallets: {}", self.volume_bot_wallets.len(), filename));
                        } else {
                            ui.label(format!("Generated {} wallets (file info pending).", self.volume_bot_wallets.len()));
                        }
                    }
                    ui.add_space(10.0);

                    ui.separator();
                    ui.label("Volume Bot Wallets File (for simulation):");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.volume_bot_source_wallets_file);
                        if ui.button("Browse...").clicked() {
                            if let Some(path) = FileDialog::new().add_filter("JSON", &["json"]).pick_file() {
                                self.volume_bot_source_wallets_file = path.to_string_lossy().into_owned();
                            }
                        }
                        if ui.button("Load Wallets from File").clicked() {
                            if !self.volume_bot_source_wallets_file.trim().is_empty() {
                                self.sim_log_messages.push(format!("Attempting to load wallets from: {}", self.volume_bot_source_wallets_file));
                                self.last_operation_result = Some(Ok("Loading wallets from file...".to_string()));
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
                                self.sim_log_messages.push("Volume Bot wallets file path is empty. Cannot load.".to_string());
                                self.last_operation_result = Some(Err("Wallets file path is empty.".to_string()));
                            }
                        }
                    });
                    ui.add_space(10.0);

                    ui.separator();
                    ui.strong("SOL Distribution to Volume Wallets:");
                    ui.add_space(5.0);
                    ui.horizontal(|ui| {
                        ui.label("Total SOL to Distribute:");
                        ui.text_edit_singleline(&mut self.volume_bot_total_sol_to_distribute_input);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Funding Source Wallet Private Key:");
                        ui.add(egui::TextEdit::singleline(&mut self.volume_bot_funding_source_private_key_input).password(true));
                    });
                    if self.volume_bot_funding_in_progress {
                        ui.horizontal(|ui| { ui.spinner(); ui.label("Distribution in progress..."); });
                    } else {
                        let can_distribute = !self.volume_bot_wallets.is_empty() && self.volume_bot_total_sol_to_distribute_input.parse::<f64>().unwrap_or(0.0) > 0.0 && !self.volume_bot_funding_source_private_key_input.is_empty();
                        if ui.add_enabled(can_distribute, egui::Button::new("Distribute Total SOL")).clicked() {
                            self.volume_bot_funding_in_progress = true;
                            self.volume_bot_funding_log_messages.clear();
                            self.volume_bot_funding_log_messages.push("Initiating SOL distribution...".to_string());
                            self.last_operation_result = None;
                            let total_sol_input = self.volume_bot_total_sol_to_distribute_input.trim().to_string();
                            let source_pk_input = self.volume_bot_funding_source_private_key_input.trim().to_string();
                            let wallets_to_fund_clone = self.volume_bot_wallets.clone();
                            let rpc_url_clone = self.app_settings.solana_rpc_url.clone();
                            let status_sender_clone = self.volume_bot_funding_status_sender.clone(); // Moved up
                            match total_sol_input.parse::<f64>() {
                                Ok(total_sol) if total_sol > 0.0 => {
                                    // status_sender_clone is now in scope here
                                    if source_pk_input.is_empty() {
                                        self.last_operation_result = Some(Err("Funding source PK empty".into()));
                                        self.volume_bot_funding_in_progress = false;
                                    } else if wallets_to_fund_clone.is_empty() {
                                        self.last_operation_result = Some(Err("No wallets to fund".into()));
                                        self.volume_bot_funding_in_progress = false;
                                    } else {
                                        let params = DistributeTotalSolTaskParams {
                                            rpc_url: rpc_url_clone,
                                            source_funding_keypair_str: source_pk_input,
                                            wallets_to_fund: wallets_to_fund_clone,
                                            total_sol_to_distribute: total_sol,
                                            status_sender: status_sender_clone,
                                        };
                                        self.start_distribute_total_sol_request = Some(params);
                                        self.last_operation_result = Some(Ok("SOL distribution request queued.".to_string()));
                                        // tokio::task::spawn_blocking will happen in PumpFunApp::update
                                    }
                                }
                                _ => {
                                    self.last_operation_result = Some(Err("Invalid SOL amount".into()));
                                    self.volume_bot_funding_in_progress = false;
                                }
                            }
                        }
                    }
                    ui.add_space(5.0);
                    ui.label("Distribution Log:");
                    egui::ScrollArea::vertical().id_source("distribution_log_scroll_area_nested").max_height(100.0).auto_shrink([false, true]).stick_to_bottom(true).show(ui, |ui| {
                        for msg in &self.volume_bot_funding_log_messages { ui.monospace(msg); }
                    });
                    ui.add_space(10.0);
                    ui.separator();

                    if ui.button("Gather All Funds to Parent").clicked() {
                         if !self.volume_bot_wallets.is_empty() {
                            if self.app_settings.parent_wallet_private_key.trim().is_empty() { self.last_operation_result = Some(Err("Parent wallet not set".into())); }
                            else {
                                self.sim_log_messages.push("Gather all funds to parent request queued...".to_string());
                                let params = GatherAllFundsTaskParams {
                                    rpc_url: self.app_settings.solana_rpc_url.clone(),
                                    parent_private_key_str: self.app_settings.parent_wallet_private_key.clone(),
                                    volume_wallets: self.volume_bot_wallets.clone(),
                                    token_mint_ca_str: self.sim_token_mint.trim().to_string(),
                                    status_sender: self.sim_status_sender.clone(),
                                };
                                self.start_gather_all_funds_request = Some(params);
                                self.last_operation_result = Some(Ok("Gather all funds request queued.".to_string()));
                                // tokio::spawn will happen in PumpFunApp::update
                            }
                        } else { self.last_operation_result = Some(Err("No wallets to gather from".into())); }
                    }
                    ui.add_space(5.0);

                    let parent_wallet_pubkey_str_opt = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);
                    ui.horizontal(|ui| {
                        ui.label("Parent Funding Wallet (from Settings):");
                        if let Some(ref pk_str) = parent_wallet_pubkey_str_opt { ui.monospace(pk_str); }
                        else { ui.colored_label(egui::Color32::YELLOW, "Parent wallet not set."); }
                    });
                    if let Some(wallet) = self.wallets.iter().find(|w| w.is_parent && parent_wallet_pubkey_str_opt.as_ref().map_or(false, |pk| w.address == *pk)) {
                        if wallet.is_loading { ui.horizontal(|ui| { ui.spinner(); ui.label("Loading balance..."); }); }
                        else if let Some(balance) = wallet.sol_balance { ui.label(format!("Balance: ◎ {:.6}", balance)); }
                        else if let Some(err) = &wallet.error { ui.colored_label(egui::Color32::RED, format!("Error: {}", err)); }
                        else { ui.label("Balance: N/A"); }
                    }
                    ui.add_space(10.0);
                    
                    ui.label("Amount of SOL to send from PARENT to each Volume Bot sub-wallet:");
                    ui.add(egui::DragValue::new(&mut self.volume_bot_funding_per_wallet_sol).speed(0.001).range(0.0..=1000.0).prefix("◎ ").suffix(" SOL per wallet"));
                    ui.add_space(5.0);
                    if self.volume_bot_funding_in_progress { // This flag might need to be distinct from the other funding flag
                        ui.horizontal(|ui| { ui.spinner(); ui.label("Funding in progress..."); });
                    } else {
                        let fund_button_enabled = self.volume_bot_wallets_generated && !self.volume_bot_wallets.is_empty();
                        if ui.add_enabled(fund_button_enabled, egui::Button::new("Fund Volume Bot Wallets")).clicked() {
                            if self.volume_bot_funding_per_wallet_sol > 0.0 {
                                self.volume_bot_funding_in_progress = true;
                                self.sim_log_messages.push(format!("Initiating funding of ◎{} to {} volume bot wallets...", self.volume_bot_funding_per_wallet_sol, self.volume_bot_wallets.len()));
                                let parent_pk_str_clone = self.app_settings.parent_wallet_private_key.clone();
                                if parent_pk_str_clone.is_empty() {
                                    self.last_operation_result = Some(Err("Parent PK empty".into()));
                                    self.volume_bot_funding_in_progress = false;
                                } else if self.volume_bot_wallets.is_empty() {
                                    self.last_operation_result = Some(Err("No wallets to fund".into()));
                                    self.volume_bot_funding_in_progress = false;
                                } else {
                                    let params = FundVolumeWalletsTaskParams {
                                        rpc_url: self.app_settings.solana_rpc_url.clone(),
                                        parent_keypair_str: parent_pk_str_clone,
                                        wallets_to_fund: self.volume_bot_wallets.clone(),
                                        funding_amount_lamports: sol_to_lamports(self.volume_bot_funding_per_wallet_sol),
                                        status_sender: self.volume_bot_funding_status_sender.clone(),
                                    };
                                    self.start_fund_volume_wallets_request = Some(params);
                                    self.last_operation_result = Some(Ok("Volume wallet funding request queued.".to_string()));
                                }
                            } else {
                                self.last_operation_result = Some(Err("Funding amount must be > 0".into()));
                                self.volume_bot_funding_in_progress = false;
                            }
                        }
                    }
                    ui.add_space(10.0);
                    ui.separator();
                    ui.strong(format!("Volume Wallet Balances (Token CA: {}):", if self.sim_token_mint.trim().is_empty() { "N/A" } else { self.sim_token_mint.trim() }));
                    if self.volume_bot_wallet_display_infos.is_empty() && self.volume_bot_wallets.is_empty() {
                        ui.label("No volume bot wallets loaded, generated, or refreshed yet.");
                    } else if self.volume_bot_wallet_display_infos.is_empty() && !self.volume_bot_wallets.is_empty() {
                        ui.label(format!("{} volume bot wallets loaded/generated. Refresh balances to view details.", self.volume_bot_wallets.len()));
                        // Still show refresh button
                         if ui.button("Refresh Volume Wallet Balances").clicked() {
                            if !self.sim_token_mint.trim().is_empty() {
                                let addresses_to_check: Vec<String> = self.volume_bot_wallets.iter().map(|lw| lw.public_key.clone()).collect();
                                if !addresses_to_check.is_empty() {
                                    // Set a flag specific to volume bot balance loading if needed, or use general one carefully
                                    self.balances_loading = true; // Or a new self.volume_bot_balances_loading
                                    self.last_operation_result = Some(Ok(format!("Fetching balances for {} volume wallets...", addresses_to_check.len())));
                                    // When refreshing, clear old display infos and prepare to populate new ones
                                    self.volume_bot_wallet_display_infos.clear();
                                    for lw_info in &self.volume_bot_wallets {
                                        self.volume_bot_wallet_display_infos.push(WalletInfo {
                                            address: lw_info.public_key.clone(),
                                            is_primary: false, is_dev_wallet: false, is_parent: false, // Not relevant for volume bot display items
                                            sol_balance: None, target_mint_balance: None, error: None,
                                            is_loading: true, // Set to loading initially
                                            is_selected: false, sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                                            atomic_buy_sol_amount_input: "0.0".to_string(),
                                        });
                                    }
                                    let params = FetchBalancesTaskParams {
                                        rpc_url: self.app_settings.solana_rpc_url.clone(),
                                        addresses_to_check, // Already a Vec<String>
                                        target_mint: self.sim_token_mint.trim().to_string(),
                                        balance_fetch_sender: self.balance_fetch_sender.clone(),
                                    };
                                    self.start_fetch_balances_request = Some(params);
                                    // tokio::spawn will happen in PumpFunApp::update
                                } else { self.last_operation_result = Some(Err("No wallets to refresh".into())); }
                            } else { self.last_operation_result = Some(Err("Token CA empty".into())); }
                        }
                    } else { // volume_bot_wallet_display_infos is not empty
                        if ui.button("Refresh Volume Wallet Balances").clicked() {
                            if !self.sim_token_mint.trim().is_empty() {
                                let addresses_to_check: Vec<String> = self.volume_bot_wallets.iter().map(|lw| lw.public_key.clone()).collect();
                                if !addresses_to_check.is_empty() {
                                    self.balances_loading = true;
                                     self.last_operation_result = Some(Ok(format!("Fetching balances for {} volume wallets...", addresses_to_check.len())));
                                    
                                    let current_display_keys: std::collections::HashSet<String> = self.volume_bot_wallet_display_infos.iter().map(|wi| wi.address.clone()).collect();
                                    let loaded_keys: std::collections::HashSet<String> = self.volume_bot_wallets.iter().map(|lw| lw.public_key.clone()).collect();

                                    for lw_info in &self.volume_bot_wallets {
                                        if let Some(existing_info) = self.volume_bot_wallet_display_infos.iter_mut().find(|wi| wi.address == lw_info.public_key) {
                                            existing_info.is_loading = true;
                                            existing_info.error = None;
                                        } else {
                                            self.volume_bot_wallet_display_infos.push(WalletInfo {
                                                address: lw_info.public_key.clone(),
                                                is_primary: false, is_dev_wallet: false, is_parent: false,
                                                sol_balance: None, target_mint_balance: None, error: None,
                                                is_loading: true, is_selected: false, sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                                                atomic_buy_sol_amount_input: "0.0".to_string(),
                                            });
                                        }
                                    }
                                    self.volume_bot_wallet_display_infos.retain(|wi| loaded_keys.contains(&wi.address));

                                    let params = FetchBalancesTaskParams {
                                        rpc_url: self.app_settings.solana_rpc_url.clone(),
                                        addresses_to_check, // Already a Vec<String>
                                        target_mint: self.sim_token_mint.trim().to_string(),
                                        balance_fetch_sender: self.balance_fetch_sender.clone(),
                                    };
                                    self.start_fetch_balances_request = Some(params);
                                    // tokio::spawn will happen in PumpFunApp::update
                                } else { self.last_operation_result = Some(Err("No wallets to refresh (source list empty)".into())); }
                            } else { self.last_operation_result = Some(Err("Token CA empty".into())); }
                        }
                        egui::ScrollArea::vertical().id_source("volume_wallet_balances_scroll_nested").max_height(200.0).show(ui, |ui| {
                            for wallet_info in &self.volume_bot_wallet_display_infos { // Iterate over the new display list
                                ui.horizontal_wrapped(|ui| {
                                    ui.monospace(&wallet_info.address[0..16]); // Use address from WalletInfo
                                    if wallet_info.is_loading { ui.spinner(); ui.label("Loading..."); }
                                    else {
                                        ui.label(format!("SOL: {:.4}", wallet_info.sol_balance.unwrap_or(0.0)));
                                        if let Some(token_bal_str) = &wallet_info.target_mint_balance { ui.label(format!("Token: {}", token_bal_str)); }
                                        else { ui.label("Token: N/A"); }
                                        if let Some(err) = &wallet_info.error { ui.colored_label(ui.style().visuals.error_fg_color, format!("Error: {}", err));}
                                    }
                                });
                                ui.separator();
                            }
                        });
                    }
                    ui.separator();
                    ui.add_space(10.0);

                    ui.label("CA:");
                    ui.text_edit_singleline(&mut self.sim_token_mint);
                    ui.label("Max Cycles:");
                    ui.add(egui::DragValue::new(&mut self.sim_max_cycles));
                    ui.label("Slippage BPS:");
                    ui.add(egui::DragValue::new(&mut self.sim_slippage_bps));

                    if self.sim_in_progress {
                        if ui.button("Stop Volume Bot").clicked() {
                            self.sim_in_progress = false;
                            if let Some(handle) = self.sim_task_handle.take() {
                                handle.abort();
                                self.sim_log_messages.push("Volume Bot task aborted by user.".to_string());
                                self.last_operation_result = Some(Ok("Volume Bot stopped.".to_string()));
                                // Optionally send a status through sim_status_sender if the task itself needs to know it was stopped externally
                                // let _ = self.sim_status_sender.send(Ok("Stopped by user.".to_string()));
                            } else {
                                self.sim_log_messages.push("Attempted to stop Volume Bot, but no active task found.".to_string());
                                self.last_operation_result = Some(Err("No active Volume Bot task to stop.".to_string()));
                            }
                        }
                    } else {
                        if ui.button("Start Volume Bot").clicked() {
                            if self.volume_bot_source_wallets_file.trim().is_empty() { self.last_operation_result = Some(Err("Wallets file empty".into())); }
                            else if self.sim_token_mint.trim().is_empty() { self.last_operation_result = Some(Err("Token CA empty".into())); }
                            else {
                                // Set state flags first
                                self.sim_in_progress = true;
                                self.sim_log_messages.clear();
                                self.sim_log_messages.push("Volume Bot start requested. Preparing parameters...".to_string());
                                
                                let params = VolumeBotStartParams {
                                    rpc_url: self.app_settings.solana_rpc_url.clone(),
                                    wallets_file_path: self.volume_bot_source_wallets_file.trim().to_string(),
                                    token_mint_str: self.sim_token_mint.trim().to_string(),
                                    max_cycles: self.sim_max_cycles as u32,
                                    slippage_bps: self.sim_slippage_bps,
                                    solana_priority_fee: self.app_settings.get_default_priority_fee_lamports(),
                                    initial_buy_sol_amount: self.volume_bot_initial_buy_sol_input.parse::<f64>().unwrap_or(0.01), // Added: Parse from UI input or default
                                    status_sender: self.sim_status_sender.clone(),
                                    wallets_data_sender: self.volume_bot_wallets_sender.clone(),
                                    use_jito: self.app_settings.use_jito_for_volume_bot,
                                    jito_be_url: self.app_settings.selected_jito_block_engine_url.clone(), // Use selected URL
                                    jito_tip_account: self.app_settings.jito_tip_account_pk_str_volume_bot.clone(),
                                    jito_tip_lamports: self.app_settings.jito_tip_lamports_volume_bot,
                                };
                                self.start_volume_bot_request = Some(params);
                                self.last_operation_result = Some(Ok("Volume Bot initiated. Task will start shortly.".to_string()));
                                // tokio::spawn and self.sim_task_handle assignment will be done in PumpFunApp::update
                            }
                        }
                    }
                    ui.add_space(10.0);
                }); // End of left column scroll area

            // Content for left column (columns[0]) starts after this

            // Content for right column (columns[1]) starts after left column's content
            // The set_width for columns[1] is now done above, before the ui.columns closure.
            // The ScrollArea for the right column will be added to columns[1]

            // Content for right column (columns[1])
            egui::ScrollArea::vertical()
                .id_source("volume_bot_log_scroll_area_right")
                .auto_shrink([true, true]) // Allow to shrink horizontally and vertically
                .stick_to_bottom(true)
                .show(&mut columns[1], |ui_log_area_content| {
                    ui_log_area_content.label("Volume Bot Log:");
                    ui_log_area_content.separator();
                    if self.sim_log_messages.is_empty() {
                        ui_log_area_content.monospace("No log messages yet.");
                    } else {
                        for msg in &self.sim_log_messages {
                            ui_log_area_content.monospace(msg);
                        }
                    }
                });
        }); // End of ui.columns
}

fn show_pump_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("⛽ Pump View"); // Added icon and heading
        ui.separator();

        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow)
            .show(ui, |ui_card| {
                ui_card.label("Pump View - WIP");
                // TODO: Implement Pump view UI
                // Display self.pump_log_messages
                ui_card.separator();
                ui_card.strong("Pump Logs:");
                egui::ScrollArea::vertical().id_source("pump_log_scroll_card_view").max_height(100.0).show(ui_card, |ui_scroll_log| {
                    for msg in &self.pump_log_messages {
                        ui_scroll_log.label(msg);
                    }
                });
            }); // End of main Frame for pump view
    } // End of show_pump_view

    // --- Atomic Buy View ---
    fn show_atomic_buy_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("💥 Atomic Buy Bundle"); // Changed icon
        ui.separator();

        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow)
            .show(ui, |ui_card| {
                ui_card.label("Select wallets and specify SOL amount for each to participate in a bundled buy.");
                ui_card.add_space(10.0);

                egui::Grid::new("atomic_buy_inputs_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .striped(true)
                    .show(ui_card, |ui_grid| {
                        ui_grid.label("Token Mint Address:");
                        ui_grid.text_edit_singleline(&mut self.atomic_buy_mint_address);
                        ui_grid.end_row();

                        ui_grid.label("Slippage (bps):");
                        ui_grid.add(egui::DragValue::new(&mut self.atomic_buy_slippage_bps).speed(10.0).range(0..=10000));
                        ui_grid.end_row();

                        ui_grid.label("Priority Fee per Tx (lamports):");
                        ui_grid.add(egui::DragValue::new(&mut self.atomic_buy_priority_fee_lamports_per_tx).speed(1000.0));
                        ui_grid.end_row();
                        
                        ui_grid.label("Jito Tip for Bundle (SOL):");
                        ui_grid.add(egui::DragValue::new(&mut self.atomic_buy_jito_tip_sol).speed(0.00001).max_decimals(9).range(0.0..=f64::MAX));
                        ui_grid.end_row();
                    });
                ui_card.separator();

                ui_card.horizontal(|ui_card_inner| {
                    ui_card_inner.strong("Select Wallets to Participate:");
                    if ui_card_inner.button("Select All Zombies").clicked() {
                        for wallet in self.wallets.iter_mut() {
                            if !wallet.is_parent && !wallet.is_primary {
                                wallet.is_selected = true;
                            }
                        }
                    }
                    if ui_card_inner.button("Deselect All").clicked() {
                        for wallet in self.wallets.iter_mut() {
                            wallet.is_selected = false;
                        }
                    }
                });

                let parent_pubkey_str = AppSettings::get_pubkey_from_privkey_str(&self.app_settings.parent_wallet_private_key);

                egui::ScrollArea::vertical().id_source("atomic_buy_wallet_scroll_card").max_height(250.0).show(ui_card, |ui_scroll| {
                    egui::Grid::new("atomic_buy_wallet_grid")
                        .num_columns(4)
                        .spacing([10.0, 4.0])
                        .striped(true)
                        .show(ui_scroll, |ui_grid_inner| {
                            ui_grid_inner.strong("Select");
                            ui_grid_inner.strong("Wallet Address");
                            ui_grid_inner.strong("SOL Balance");
                            ui_grid_inner.strong("SOL to Buy With");
                            ui_grid_inner.end_row();

                            for wallet_info in self.wallets.iter_mut() {
                                if Some(wallet_info.address.clone()) == parent_pubkey_str {
                                    continue;
                                }
                                ui_grid_inner.checkbox(&mut wallet_info.is_selected, "");
                                let short_addr = if wallet_info.address.len() > 10 {
                                    format!("{}...{}", &wallet_info.address[..4], &wallet_info.address[wallet_info.address.len()-4..])
                                } else {
                                    wallet_info.address.clone()
                                };
                                ui_grid_inner.label(&short_addr).on_hover_text(&wallet_info.address);
                                ui_grid_inner.label(wallet_info.sol_balance.map_or_else(|| "-".to_string(), |b| format!("{:.4}", b)));
                                ui_grid_inner.add(egui::TextEdit::singleline(&mut wallet_info.atomic_buy_sol_amount_input).desired_width(80.0));
                                ui_grid_inner.end_row();
                            }
                        });
                });
                ui_card.separator();

                ui_card.vertical_centered(|ui_card_centered| {
                    let launch_bundle_button = egui::Button::new(egui::RichText::new("🚀 Launch Atomic Buy Bundle").size(16.0))
                        .min_size(egui::vec2(280.0, 40.0)); // Made button larger
                    
                    // Add some space before the button
                    ui_card_centered.add_space(10.0);

                    let mut selected_wallets_for_buy: Vec<(String, f64)> = Vec::new();
                    for w in self.wallets.iter().filter(|w| w.is_selected && Some(w.address.clone()) != parent_pubkey_str) {
                        if let Ok(amount) = w.atomic_buy_sol_amount_input.parse::<f64>() {
                            if amount > 0.0 {
                                selected_wallets_for_buy.push((w.address.clone(), amount));
                            }
                        }
                    }
                    let can_launch = !self.atomic_buy_in_progress &&
                                     !self.atomic_buy_mint_address.trim().is_empty() &&
                                     !selected_wallets_for_buy.is_empty();

                    if ui_card_centered.add_enabled(can_launch, launch_bundle_button).clicked() {
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
                        let jito_tip_account_to_use_app = self.app_settings.jito_tip_account.clone();
                        if jito_tip_account_to_use_app.is_empty() && jito_tip_sol_bundle_clone > 0.0 { // jito_tip_sol_bundle_clone is defined earlier
                            warn!("Jito tip account in app_settings (app.rs) is empty, but a tip amount is specified. This will likely cause an error.");
                        }

                        let mut participating_keypairs_with_amounts: Vec<(solana_sdk::signature::Keypair, f64)> = Vec::new();
                        let mut wallets_missing_keypairs: Vec<String> = Vec::new();

                        for (pk_str, sol_amount) in &selected_wallets_for_buy {
                            if let Some(loaded_wallet) = loaded_wallet_data_clone.iter().find(|lw| lw.public_key == *pk_str) {
                                match bs58::decode(&loaded_wallet.private_key).into_vec() {
                                    Ok(keypair_bytes) => {
                                        match solana_sdk::signature::Keypair::from_bytes(&keypair_bytes) {
                                            Ok(kp) => {
                                                participating_keypairs_with_amounts.push((kp, *sol_amount));
                                            },
                                            Err(e) => {
                                                let err_msg = format!("Error creating keypair from bytes for {}: {}", pk_str, e);
                                                self.atomic_buy_log_messages.push(err_msg.clone());
                                                wallets_missing_keypairs.push(pk_str.clone());
                                                log::error!("{}", err_msg);
                                            }
                                        }
                                    },
                                    Err(e) => {
                                        let err_msg = format!("Error decoding base58 private key for {}: {}", pk_str, e);
                                        self.atomic_buy_log_messages.push(err_msg.clone());
                                        wallets_missing_keypairs.push(pk_str.clone());
                                        log::error!("{}", err_msg);
                                    }
                                }
                            } else {
                                let err_msg = format!("Private key not found in loaded data for selected wallet: {}", pk_str);
                                self.atomic_buy_log_messages.push(err_msg.clone());
                                wallets_missing_keypairs.push(pk_str.clone());
                            }
                        }

                        if !wallets_missing_keypairs.is_empty() {
                            let err_summary = format!("Failed to prepare bundle: Could not load keypairs for wallets: {}. See log.", wallets_missing_keypairs.join(", "));
                            self.atomic_buy_log_messages.push(err_summary.clone());
                            status_sender_clone.send(Err(err_summary)).unwrap_or_default();
                            self.atomic_buy_in_progress = false;
                            return;
                        }

                        if participating_keypairs_with_amounts.is_empty() {
                            let err_summary = "No valid wallets with amounts and loadable keypairs to participate in the bundle.".to_string();
                            self.atomic_buy_log_messages.push(err_summary.clone());
                            status_sender_clone.send(Err(err_summary)).unwrap_or_default();
                            self.atomic_buy_in_progress = false;
                            return;
                        }

                        self.atomic_buy_log_messages.push(format!("Mint: {}", mint_address_clone));
                        self.atomic_buy_log_messages.push(format!("Slippage: {} bps", slippage_bps_clone));
                        self.atomic_buy_log_messages.push(format!("Priority Fee/Tx: {} lamports", priority_fee_lamports_per_tx_clone));
                        self.atomic_buy_log_messages.push(format!("ALT: None"));
                        self.atomic_buy_log_messages.push(format!("Jito Tip for Bundle: {} SOL", jito_tip_sol_bundle_clone));
                        for (kp, amt) in &participating_keypairs_with_amounts {
                            self.atomic_buy_log_messages.push(format!("  - Wallet: {}, Amount: {} SOL", kp.pubkey(), amt));
                        }
                        self.atomic_buy_log_messages.push("Spawning atomic buy task...".to_string());

                        let atomic_buy_task = tokio::spawn(async move {
                            let result = crate::commands::atomic_buy::atomic_buy(
                                crate::commands::atomic_buy::Exchange::PumpFun, // Added exchange type
                                mint_address_clone,
                                participating_keypairs_with_amounts,
                                slippage_bps_clone,
                                priority_fee_lamports_per_tx_clone,
                                None, // lookup_table_address_str
                                jito_tip_sol_bundle_clone, // jito_tip_sol_per_tx (for PumpFun)
                                0.0, // jito_overall_tip_sol (not used for PumpFun)
                                selected_jito_url_clone, // jito_block_engine_url
                                jito_tip_account_to_use_app, // jito_tip_account_pubkey_str
                            ).await;
                            match result {
                                Ok(_) => {
                                    let success_msg = "Atomic Buy Bundle task completed successfully.".to_string();
                                    log::info!("{}", success_msg);
                                    if status_sender_clone.send(Ok(success_msg)).is_err() {
                                        log::error!("Failed to send atomic buy success status to UI.");
                                    }
                                }
                                Err(e) => {
                                    let err_msg = format!("Atomic Buy Bundle task failed: {}", e);
                                    log::error!("{}", err_msg);
                                    if status_sender_clone.send(Err(err_msg)).is_err() {
                                        log::error!("Failed to send atomic buy error status to UI.");
                                    }
                                }
                            }
                        });
                        self.atomic_buy_task_handle = Some(atomic_buy_task);
                    }
                    if self.atomic_buy_in_progress {
                        ui_card_centered.add_space(5.0);
                        ui_card_centered.horizontal(|ui_h| {
                            ui_h.spinner();
                            ui_h.label(egui::RichText::new("Processing bundle...").italics());
                        });
                        ui_card_centered.add_space(5.0);
                    }
                });
                
                ui_card.add_space(10.0);
                ui_card.label("Atomic Buy Log:");
                egui::ScrollArea::vertical().id_source("atomic_buy_log_scroll_card_view").max_height(100.0).stick_to_bottom(true).show(ui_card, |ui_scroll_log| {
                    for msg in &self.atomic_buy_log_messages {
                        ui_scroll_log.monospace(msg);
                    }
                });
            }); // End of main Frame for atomic_buy view
    }



    // Stub for show_transfers_view
    fn show_transfers_view(&mut self, ui: &mut egui::Ui) {
        ui.heading("⇄ Transfers View"); // Added icon and heading
        ui.separator();

        egui::Frame::none().fill(APP_BG_SECONDARY).inner_margin(Margin::same(16.0)).outer_margin(Margin::symmetric(8.0, 8.0)).rounding(Rounding::same(6.0)).stroke(Stroke::new(1.0, APP_STROKE_COLOR)).shadow(ui.style().visuals.window_shadow)
            .show(ui, |ui_card| {
                ui_card.label("Transfers View - WIP");
                // TODO: Implement Transfers view UI
                // Display self.transfer_log_messages
                ui_card.separator();
                ui_card.strong("Transfer Logs:");
                egui::ScrollArea::vertical().id_source("transfers_log_scroll_card_view").max_height(100.0).show(ui_card, |ui_scroll_log| {
                    for msg in &self.transfer_log_messages {
                        ui_scroll_log.label(msg);
                    }
                });
            }); // End of main Frame for transfers view
    }
} // Closes the impl PumpFunApp block

// Implementing eframe::App for PumpFunApp
impl eframe::App for PumpFunApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Styling is now handled in PumpFunApp::new

        // Process any incoming balance updates
        while let Ok(fetch_result) = self.balance_fetch_receiver.try_recv() {
            self.balance_tasks_completed += 1;
            match fetch_result {
                FetchResult::Success { address, sol_balance, target_mint_balance } => {
                    // Check if this address belongs to a volume bot wallet first
                    if self.volume_bot_wallets.iter().any(|lw| lw.public_key == address) {
                        if let Some(wallet_info) = self.volume_bot_wallet_display_infos.iter_mut().find(|wi| wi.address == address) {
                            wallet_info.sol_balance = Some(sol_balance);
                            wallet_info.target_mint_balance = target_mint_balance;
                            wallet_info.error = None;
                            wallet_info.is_loading = false;
                        }
                    } else if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == address) {
                        // Fallback to general wallets if not a volume bot wallet
                        wallet.sol_balance = Some(sol_balance);
                        wallet.target_mint_balance = target_mint_balance;
                        wallet.error = None;
                        wallet.is_loading = false;
                    }
                }
                FetchResult::Failure { address, error } => {
                    // Check if this address belongs to a volume bot wallet first
                    if self.volume_bot_wallets.iter().any(|lw| lw.public_key == address) {
                        if let Some(wallet_info) = self.volume_bot_wallet_display_infos.iter_mut().find(|wi| wi.address == address) {
                            wallet_info.error = Some(error);
                            wallet_info.is_loading = false;
                        }
                    } else if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == address) {
                        // Fallback to general wallets
                        wallet.error = Some(error);
                        wallet.is_loading = false;
                    }
                }
            }

            if self.balance_tasks_completed >= self.balance_tasks_expected {
                self.balances_loading = false;
                log::info!("All expected balance results received ({}). Balances loading finished.", self.balance_tasks_completed);
                // Optionally reset counters if appropriate, or let trigger_balance_fetch do it
                // self.balance_tasks_expected = 0;
                // self.balance_tasks_completed = 0;
            }
        }
        
        // After processing all available balance updates for this frame, recalculate totals for status bar.
        // This logic assumes `self.wallets` contains the relevant wallets for the "Check" view,
        // already filtered to exclude "Volume Bot" specific wallets if they came from volume_wallets_*.json.
        let mut new_parent_sol_display: Option<f64> = None;
        let mut new_total_sol_display: f64 = 0.0;
        
        let mut counted_addresses_for_total: HashSet<String> = HashSet::new();

        for wallet_info in &self.wallets {
            // Update parent display if this wallet is the parent
            if wallet_info.is_parent {
                new_parent_sol_display = wallet_info.sol_balance;
            }

            // Add to total if it has a balance and hasn't been counted
            // Wallets are: Parent, Dev (Minter), or "Zombie" (others from keys.json for Check view)
            if let Some(sol) = wallet_info.sol_balance {
                if wallet_info.is_parent || wallet_info.is_dev_wallet ||
                   (!wallet_info.is_parent && !wallet_info.is_dev_wallet) { // This covers zombies
                    if counted_addresses_for_total.insert(wallet_info.address.clone()) {
                        new_total_sol_display += sol;
                    }
                }
            }
        }

        self.parent_sol_balance_display = new_parent_sol_display;
        self.total_sol_balance_display = Some(new_total_sol_display);

        // Process disperse results
        while let Ok(disperse_result) = self.disperse_result_receiver.try_recv() {
            self.last_operation_result = Some(disperse_result);
            self.disperse_in_progress = false; // Stop loading indicator
        }

        // Process gather results
        while let Ok(gather_result) = self.gather_result_receiver.try_recv() {
            self.gather_tasks_completed += 1;
            self.last_operation_result = Some(gather_result); // Show individual results
            if self.gather_tasks_completed >= self.gather_tasks_expected {
                self.gather_in_progress = false; // Stop loading indicator when all done
                self.last_operation_result = Some(Ok(format!("Gather operation complete. {} tasks processed.", self.gather_tasks_completed)));
            }
        }
        
        // Process ALT precalc results
        while let Ok(precalc_result) = self.alt_precalc_result_receiver.try_recv() {
            self.alt_precalc_in_progress = false;
            match precalc_result {
                PrecalcResult::Success(mint_pk_str, addresses) => {
                    self.alt_generated_mint_pubkey = Some(mint_pk_str);
                    self.alt_precalc_addresses = addresses;
                    self.alt_view_status = "Precalculation successful. Ready to create ALT.".to_string();
                    self.alt_log_messages.push("Precalculation successful.".to_string());
                }
                PrecalcResult::Failure(err_msg) => {
                    self.alt_view_status = format!("Precalculation failed: {}", err_msg);
                    self.alt_log_messages.push(format!("Precalculation failed: {}", err_msg));
                }
            }
        }

        // Process ALT creation status
        while let Ok(status) = self.alt_creation_status_receiver.try_recv() {
            match status {
                AltCreationStatus::Starting => {
                    self.alt_creation_in_progress = true;
                    let msg = "ALT creation process started.".to_string();
                    self.alt_view_status = msg.clone();
                    self.alt_log_messages.push(msg);
                }
                AltCreationStatus::Sending(msg) => {
                    self.alt_creation_in_progress = true;
                    self.alt_view_status = msg.clone();
                    self.alt_log_messages.push(format!("[Sending] {}", msg));
                }
                AltCreationStatus::Confirmed(desc, sig) => {
                    // Still in progress, but a step is confirmed
                    self.alt_creation_in_progress = true;
                    let msg = format!("Transaction Confirmed ({}): {}", desc, sig);
                    self.alt_view_status = msg.clone();
                    self.alt_log_messages.push(msg);
                }
                AltCreationStatus::LogMessage(log_msg) => {
                    self.alt_log_messages.push(format!("[ALT_TASK_DETAIL]: {}", log_msg));
                    // self.alt_view_status = format!("Detail: {}", log_msg); // Optionally update view status
                }
                AltCreationStatus::Success(alt_address) => {
                    self.alt_creation_in_progress = false;
                    self.alt_address = Some(alt_address);
                    let success_msg = format!("ALT successfully created: {}", alt_address);
                    self.alt_view_status = success_msg.clone();
                    self.alt_log_messages.push(success_msg);
                    self.load_available_alts(); // Refresh ALT list
                }
                AltCreationStatus::Failure(err_msg) => {
                    self.alt_creation_in_progress = false;
                    self.alt_view_status = format!("ALT Creation failed: {}", err_msg);
                    self.alt_log_messages.push(format!("ALT Creation failed: {}", err_msg));
                }
            }
        }

        // Process Sell status updates
        while let Ok(status) = self.sell_status_receiver.try_recv() {
            match status {
                SellStatus::Idle => {
                    // Might not be used if tasks always send progress/completion
                }
                SellStatus::InProgress(msg) => {
                    self.sell_log_messages.push(format!("[In Progress] {}", msg));
                    // Individual wallet selling flags are handled by trigger_individual_sell
                }
                SellStatus::WalletSuccess(addr, sig_or_msg) => {
                    self.sell_log_messages.push(format!("[Success: {}] {}", addr, sig_or_msg));
                    if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == addr) {
                        wallet.is_selling = false; // Clear loading for this wallet
                        wallet.sell_percentage_input = "0".to_string(); // Reset input
                        wallet.sell_amount_input = "0".to_string(); // Reset input
                    }
                    // Potentially trigger a balance refresh for this wallet
                    self.trigger_balance_fetch(Some(self.sell_mint.clone()));
                }
                SellStatus::WalletFailure(addr, err_msg) => {
                    self.sell_log_messages.push(format!("[Failure: {}] {}", addr, err_msg));
                     if let Some(wallet) = self.wallets.iter_mut().find(|w| w.address == addr) {
                        wallet.is_selling = false; // Clear loading for this wallet
                    }
                }
                SellStatus::MassSellComplete(msg) => {
                    self.sell_log_messages.push(format!("[Mass Sell Complete] {}", msg));
                    self.sell_mass_reverse_in_progress = false;
                    self.trigger_balance_fetch(Some(self.sell_mint.clone())); // Refresh all balances
                }
                SellStatus::GatherAndSellComplete(msg) => {
                    self.sell_log_messages.push(format!("[Gather & Sell Complete] {}", msg));
                    self.sell_gather_and_sell_in_progress = false;
                    self.trigger_balance_fetch(Some(self.sell_mint.clone())); // Refresh all balances
                }
                SellStatus::Failure(err_msg) => {
                    self.sell_log_messages.push(format!("[General Error] {}", err_msg));
                    // Reset all relevant loading flags
                    self.sell_mass_reverse_in_progress = false;
                    self.sell_gather_and_sell_in_progress = false;
                    for wallet in self.wallets.iter_mut() {
                        wallet.is_selling = false;
                    }
                }
            }
        }
        
        // Process Pump status updates
        while let Ok(status) = self.pump_status_receiver.try_recv() {
            match status {
                PumpStatus::Idle => {
                    // Potentially useful for an initial state, or if pump can be paused.
                    self.pump_log_messages.push("Pump is Idle.".to_string());
                    self.pump_in_progress = false; // Assuming idle means not actively running
                }
                PumpStatus::InProgress(msg) => {
                    self.pump_log_messages.push(format!("[Pump InProgress] {}", msg));
                    // If the message indicates start, set in_progress true
                    if msg.to_lowercase().contains("pump started") || msg.to_lowercase().contains("initiating pump") {
                        self.pump_in_progress = true;
                        self.pump_is_running.store(true, Ordering::Relaxed);
                    }
                }
                PumpStatus::Success(msg) => {
                    self.pump_log_messages.push(format!("[Pump Success] {}", msg));
                    self.pump_is_running.store(false, Ordering::Relaxed);
                    self.pump_in_progress = false;
                    self.pump_task_handle = None; // Task completed successfully
                }
                PumpStatus::Failure(err_msg) => {
                    self.pump_log_messages.push(format!("[Pump ERROR] {}", err_msg));
                    self.pump_is_running.store(false, Ordering::Relaxed); // Stop on error
                    self.pump_in_progress = false;
                    if let Some(handle) = self.pump_task_handle.take() {
                        handle.abort(); // Ensure task is stopped
                    }
                }
            }
        }

        // Process Launch status updates
        while let Ok(status) = self.launch_status_receiver.try_recv() {
            match status {
                LaunchStatus::Starting => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push("Launch process starting...".to_string());
                }
                LaunchStatus::UploadingMetadata => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push("Uploading metadata...".to_string());
                }
                LaunchStatus::MetadataUploaded(uri) => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push(format!("Metadata uploaded: {}", uri));
                }
                LaunchStatus::PreparingTx(label) => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push(format!("Preparing transaction: {}", label));
                }
                LaunchStatus::SimulatingTx(label) => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push(format!("Simulating transaction: {}", label));
                }
                LaunchStatus::SimulationSuccess(label) => {
                    self.launch_in_progress = true; // Still in progress until sent or fully simulated
                    self.launch_log_messages.push(format!("Transaction simulation successful: {}", label));
                }
                LaunchStatus::SimulationFailed(label, err) => {
                    self.launch_in_progress = true; // Or false if we stop here? Let's assume it might retry or continue.
                    // Only add to log if the label does not indicate a zombie buy simulation
                    if !label.contains("Zombies") {
                        self.launch_log_messages.push(format!("Transaction simulation FAILED for {}: {}", label, err));
                    } else {
                        // Optionally, log a generic message or nothing for zombie simulation failures
                        // For now, we do nothing to suppress it from the GUI console.
                        // self.launch_log_messages.push(format!("Zombie buy simulation failed for {} (suppressed).", label));
                    }
                }
                LaunchStatus::SimulationWarning(label, msg) => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push(format!("Simulation WARNING for {}: {}. Proceeding.", label, msg));
                }
                LaunchStatus::SimulationSkipped(label) => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push(format!("Simulation SKIPPED: {}", label));
                }
                LaunchStatus::BuildFailed(label, err) => {
                    self.launch_in_progress = false; // Build failure is likely terminal for this attempt
                    self.launch_log_messages.push(format!("Build FAILED for {}: {}", label, err));
                    self.last_operation_result = Some(Err(format!("Launch build failed for {}: {}", label, err)));
                }
                LaunchStatus::SubmittingBundle(num_txs) => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push(format!("Submitting bundle with {} transaction(s)...", num_txs));
                }
                LaunchStatus::BundleSubmitted(bundle_id) => {
                    self.launch_in_progress = true; // Still waiting for confirmation or final success
                    self.launch_log_messages.push(format!("Bundle submitted: {}", bundle_id));
                }
                LaunchStatus::Success(success_msg) => {
                    self.launch_in_progress = false;
                    self.launch_log_messages.push(format!("SUCCESS! {}", success_msg));
                    self.last_operation_result = Some(Ok(format!("Launch successful! {}", success_msg)));
                }
                LaunchStatus::Failure(err_msg) => {
                    self.launch_in_progress = false;
                    self.launch_log_messages.push(format!("LAUNCH FAILED: {}", err_msg));
                    self.last_operation_result = Some(Err(format!("Launch failed: {}", err_msg)));
                }
                LaunchStatus::Error(error_msg) => {
                    self.launch_in_progress = false;
                    self.launch_log_messages.push(format!("LAUNCH ERROR: {}", error_msg));
                    self.last_operation_result = Some(Err(format!("Launch error: {}", error_msg)));
                }
                LaunchStatus::PreparingZombieTxs => {
                    self.launch_in_progress = true;
                    self.launch_log_messages.push("Preparing zombie transactions...".to_string());
                }
                LaunchStatus::SimulatedOnly(message) => {
                    self.launch_in_progress = false;
                    self.launch_log_messages.push(format!("SIMULATED ONLY: {}", message));
                    self.last_operation_result = Some(Ok(format!("Simulated: {}", message)));
                }
LaunchStatus::BundleLanded(bundle_id, status_msg) => {
                    self.launch_in_progress = true; // Still in progress until Completed or Error
                    self.launch_log_messages.push(format!("Bundle {} landed with status: {}", bundle_id, status_msg));
                }
                LaunchStatus::Completed => {
                    self.launch_in_progress = false;
                    self.launch_log_messages.push("Launch task COMPLETED.".to_string());
                    if self.last_operation_result.is_none() {
                        self.last_operation_result = Some(Ok("Launch completed.".to_string()));
                    }
                }
LaunchStatus::LaunchCompletedWithLink(link) => {
                    self.launch_in_progress = false;
                    self.launch_log_messages.push(format!("🎉 Launch Completed! Pump.fun Link: {}", link));
                    // Potentially store the link in PumpFunApp if it needs to be displayed persistently,
                    // or rely on the log message for now.
                }
                LaunchStatus::Log(log_msg) => {
                    self.launch_log_messages.push(log_msg);
                }
            }
        }

        // Process Transfer status updates
        while let Ok(result) = self.transfer_status_receiver.try_recv() {
            self.transfer_in_progress = false; // Operation finished
            match result {
                Ok(msg) => {
                    self.transfer_log_messages.push(format!("[SUCCESS] {}", msg));
                    self.last_operation_result = Some(Ok(msg));
                    // Optionally, refresh balances of sender/receiver if easily identifiable
                    if let Some(sender_pk_str) = &self.transfer_selected_sender {
                         self.trigger_balance_fetch(None); // Refresh all for simplicity or specific ones
                    }
                }
                Err(err_msg) => {
                    self.transfer_log_messages.push(format!("[ERROR] {}", err_msg));
                    self.last_operation_result = Some(Err(err_msg));
                }
            }
        }
        
        // Process Volume Bot Wallet Generation status
        while let Ok(status_result) = self.volume_bot_wallet_gen_status_receiver.try_recv() {
            match status_result {
                Ok(msg) => {
                    self.sim_log_messages.push(format!("[Wallet Gen] {}", msg));
                    if msg.contains("Successfully generated") && msg.contains("and saved to") {
                        self.volume_bot_generation_in_progress = false;
                        self.volume_bot_wallets_generated = true;
                        // Extract filename: "Successfully generated X wallets and saved to Y"
                        if let Some(idx) = msg.rfind("and saved to ") {
                            let filename_part = &msg[idx + "and saved to ".len()..];
                            self.last_generated_volume_wallets_file = Some(filename_part.trim().to_string());
                        }
                    } else if msg.contains("Generation task started") {
                        // Handled by UI setting flag, just log
                    } else { // Other messages might indicate errors or intermediate steps without full success
                        self.volume_bot_generation_in_progress = false; // Assume completion if not a known progress message
                    }
                }
                Err(e) => {
                    self.sim_log_messages.push(format!("[Wallet Gen ERROR] {}", e));
                    self.volume_bot_generation_in_progress = false;
                }
            }
        }
        while let Ok(wallets_vec) = self.volume_bot_wallets_receiver.try_recv() {
            // Update self.volume_bot_wallets (which are LoadedWalletInfo)
            self.volume_bot_wallets = wallets_vec.clone();
            self.sim_log_messages.push(format!("Received {} generated/loaded wallets for Volume Bot.", self.volume_bot_wallets.len()));
            self.volume_bot_wallets_generated = true;
            self.volume_bot_generation_in_progress = false;

            // Clear previous display infos and prepare to populate new ones based on received wallets
            self.volume_bot_wallet_display_infos.clear();
            let mut addresses_to_fetch = Vec::new();

            for loaded_wallet_info in &self.volume_bot_wallets {
                self.volume_bot_wallet_display_infos.push(WalletInfo {
                    address: loaded_wallet_info.public_key.clone(),
                    is_primary: false, is_dev_wallet: false, is_parent: false, // Not relevant for volume bot display items
                    sol_balance: None, target_mint_balance: None, error: None,
                    is_loading: true, // Set to loading initially
                    is_selected: false, sell_percentage_input: "100".to_string(), sell_amount_input: String::new(), is_selling: false,
                    atomic_buy_sol_amount_input: "0.0".to_string(),
                });
                addresses_to_fetch.push(loaded_wallet_info.public_key.clone());
            }

            // Automatically trigger balance fetch for these new volume wallets
            if !addresses_to_fetch.is_empty() {
                if !self.sim_token_mint.trim().is_empty() {
                    self.balances_loading = true; // Use general flag or a specific one if preferred
                    self.balance_tasks_expected += addresses_to_fetch.len();
                    self.last_operation_result = Some(Ok(format!("Fetching balances for {} new volume wallets...", addresses_to_fetch.len())));
                    self.sim_log_messages.push(format!("Auto-fetching balances for {} volume wallets against token {}.", addresses_to_fetch.len(), self.sim_token_mint.trim()));

                    let rpc_url = self.app_settings.solana_rpc_url.clone();
                    let target_mint_for_fetch = self.sim_token_mint.trim().to_string();
                    let sender = self.balance_fetch_sender.clone();
                    
                    tokio::spawn(async move {
                        Self::fetch_balances_task(rpc_url, addresses_to_fetch, target_mint_for_fetch, sender).await;
                    });
                } else {
                    self.sim_log_messages.push("Token CA for Volume Bot is not set. Cannot auto-fetch balances.".to_string());
                    // Set loading to false for display_infos as fetch won't happen
                    for wi_display in self.volume_bot_wallet_display_infos.iter_mut() {
                        wi_display.is_loading = false;
                        wi_display.error = Some("Token CA not set".to_string());
                    }
                }
            }
        }

        // Process Volume Bot Funding status
        while let Ok(status_result) = self.volume_bot_funding_status_receiver.try_recv() {
            self.volume_bot_funding_in_progress = false; // Task finished or errored
            match status_result {
                Ok(msg) => self.sim_log_messages.push(format!("[Funding] {}", msg)),
                Err(e) => self.sim_log_messages.push(format!("[Funding ERROR] {}", e)),
            }
        }
        
        // Process Simulation (Volume Bot Task) status updates
        while let Ok(status_result) = self.sim_status_receiver.try_recv() {
            match status_result {
                Ok(msg) => {
                    self.sim_log_messages.push(msg.clone());
                    if msg.contains("Simulation finished") || msg.contains("Simulation stopped") {
                        self.sim_in_progress = false;
                        if let Some(handle) = self.sim_task_handle.take() {
                            handle.abort();
                        }
                    }
                }
                Err(e_str) => {
                    self.sim_log_messages.push(format!("[SIM ERROR] {}", e_str));
                    self.sim_in_progress = false;
                    if let Some(handle) = self.sim_task_handle.take() {
                        handle.abort();
                    }
                }
            }
        }

        // Process Original Simulation status updates
        // The sim_status_sender_orig.try_send was incorrect. This loop should process received messages.
        while let Ok(status_result) = self.sim_status_receiver_orig.try_recv() {
            // This is a placeholder for actual status processing logic
             match status_result { // status_result is Result<String, String>
                Ok(msg) => { // This arm handles the Ok(String) case
                    self.sim_log_messages_orig.push(msg.clone());
                    if msg.contains("Simulation finished") || msg.contains("Simulation stopped") {
                        self.sim_in_progress_orig = false;
                        if let Some(handle) = self.sim_task_handle_orig.take() {
                            handle.abort();
                        }
                    }
                }
                Err(e_str) => { // This arm handles the Err(String) case
                    self.sim_log_messages_orig.push(format!("[SIM ERROR] {}", e_str));
                    self.sim_in_progress_orig = false;
                    if let Some(handle) = self.sim_task_handle_orig.take() {
                        handle.abort();
                    }
                }
                // Removed erroneous Err(_) arm
            }
        }
        
        // Process Original Sim Consolidate SOL status
        while let Ok(status_result) = self.sim_consolidate_sol_status_receiver_orig.try_recv() {
            self.sim_consolidate_sol_in_progress_orig = false; // Task finished or errored
            match status_result {
                Ok(msg) => self.sim_log_messages_orig.push(format!("[Consolidate SOL] {}", msg)),
                Err(e) => self.sim_log_messages_orig.push(format!("[Consolidate SOL ERROR] {}", e)),
            }
        }

        // Process Original Sim Wallet Gen status
        while let Ok(status_result) = self.sim_wallet_gen_status_receiver.try_recv() {
            self.sim_wallet_gen_in_progress = false; // Task finished or errored
            match status_result {
                Ok(msg) => self.sim_log_messages_orig.push(format!("[Wallet Gen] {}", msg)),
                Err(e) => self.sim_log_messages_orig.push(format!("[Wallet Gen ERROR] {}", e)),
            }
        }
        
        // Process Monitor updates
        while let Ok((balances, prices)) = self.monitor_update_receiver.try_recv() {
            self.monitor_balances = balances;
            self.monitor_prices = prices;
        }


        // Main UI structure with Side Panel Navigation

        // Optional: A slim top panel for global controls if needed, or move these to side/status bar
        egui::TopBottomPanel::top("top_panel_global_controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // egui::widgets::global_dark_light_mode_buttons(ui); // Removed dark/light mode buttons
                // ui.separator(); // Also remove separator if buttons were the only thing before it
                if ui.button("Reload Wallets").clicked() {
                    self.load_wallets_from_settings_path();
                }
                // Potentially add other global items here if they don't fit well in side or status.
            });
        });

        egui::SidePanel::left("side_navigation_panel")
            .resizable(true)
            .default_width(220.0) // Slightly wider for more space
            // Set a solid, darker blue background for the sidebar directly
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(APP_SIDEBAR_BG))
            .show(ctx, |ui| {
                // Removed custom gradient drawing code

                ui.add_space(10.0); // Add some padding at the top
                ui.heading(egui::RichText::new("Octo Tools").size(20.0)); // Or your app name, larger font
                ui.add_space(5.0);
                ui.separator();
                ui.add_space(10.0);

                // Custom function for menu items for consistency
                fn menu_button(ui: &mut egui::Ui, current_view: &mut AppView, view: AppView, icon: &str, text: &str) {
                    let is_selected = *current_view == view;
                    let label_text = format!("{} {}", icon, text);
                    // Make the button fill the available width, height determined by content.
                    // The text size is 16.0, padding is handled by the button_padding in global style.
                    let button = egui::SelectableLabel::new(is_selected, egui::RichText::new(label_text).size(16.0));
                    if ui.add_sized(egui::vec2(ui.available_width(), ui.spacing().interact_size.y), button).on_hover_text(text).clicked() {
                        *current_view = view;
                    }
                    ui.add_space(6.0); // Increased space between menu items for a cleaner look
                }

                menu_button(ui, &mut self.current_view, AppView::Launch, "🚀", "Launch");
                // menu_button(ui, &mut self.current_view, AppView::Buy, "🛍️", "Buy"); // Removed Buy view
                menu_button(ui, &mut self.current_view, AppView::Sell, "💸", "Sell");
                menu_button(ui, &mut self.current_view, AppView::AtomicBuy, "💥", "Atomic Buy"); // Changed icon
                
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                
                menu_button(ui, &mut self.current_view, AppView::Disperse, "💨", "Disperse");
                menu_button(ui, &mut self.current_view, AppView::Gather, "📥", "Gather");
                
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                
                menu_button(ui, &mut self.current_view, AppView::Alt, "🔧", "ALT Mgmt"); // Changed icon
                menu_button(ui, &mut self.current_view, AppView::VolumeBot, "📈", "Volume Bot"); // Changed icon
                menu_button(ui, &mut self.current_view, AppView::Check, "🔍", "Check");
                
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                
                menu_button(ui, &mut self.current_view, AppView::Settings, "🔩", "Settings"); // Changed icon

                // Wallet info can also go here, or in the status bar
                // For now, let's keep it in the status bar as per previous setup.
                // If you want it in the side panel, uncomment and adapt the code from earlier versions.
                ui.add_space(10.0); // Add some padding at the bottom
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            // --- Draw subtle hexagonal grid background ---
            let painter = ui.painter();
            let visual_rect = ui.max_rect(); // The area to fill with the pattern

            const HEX_RADIUS: f32 = 70.0; // Increased radius for sparser, larger hexagons
            // Use direct RGBA construction for hex_color, assuming APP_STROKE_COLOR = (40,45,60)
            let hex_color = Color32::from_rgba_premultiplied(APP_STROKE_COLOR.r(), APP_STROKE_COLOR.g(), APP_STROKE_COLOR.b(), 12); // Even more subtle alpha
            let hex_stroke = Stroke::new(0.5, hex_color); // Thin lines

            // Geometry for pointy-topped hexagons
            const SQRT_3: f32 = 1.7320508; // Replaced std::f32::consts::SQRT_3
            let single_hex_effective_width = SQRT_3 * HEX_RADIUS; // Horizontal distance between centers in a row
            let single_hex_effective_height = HEX_RADIUS * 1.5; // Vertical distance between rows
            
            let mut y = visual_rect.top() - HEX_RADIUS * 2.0; // Start drawing sufficiently above the viewport
            let mut row_is_odd = false;

            while y < visual_rect.bottom() + HEX_RADIUS * 2.0 { // End drawing sufficiently below
                let mut current_x = visual_rect.left() - single_hex_effective_width;
                if row_is_odd {
                    current_x -= single_hex_effective_width / 2.0;
                }

                while current_x < visual_rect.right() + single_hex_effective_width {
                    let center = egui::pos2(current_x, y);
                    let mut points = [egui::Pos2::ZERO; 6];
                    for i in 0..6 {
                        // Angle for pointy-topped hexagons (first point straight up from center)
                        let angle = (std::f32::consts::PI / 3.0) * (i as f32) - (std::f32::consts::PI / 2.0); // Replaced FRAC_PI_3 and FRAC_PI_2
                        points[i] = center + HEX_RADIUS * egui::vec2(angle.cos(), angle.sin());
                    }
                    for i in 0..6 {
                        painter.line_segment([points[i], points[(i + 1) % 6]], hex_stroke);
                    }
                    current_x += single_hex_effective_width;
                }
                y += single_hex_effective_height;
                row_is_odd = !row_is_odd;
            }
            // --- End hexagonal grid ---

            match self.current_view {
                // AppView::Buy => self.show_buy_view(ui), // Removed Buy view case
                AppView::Sell => self.show_sell_view(ui),
                AppView::Disperse => self.show_disperse_view(ui),
                AppView::Gather => self.show_gather_view(ui),
                AppView::Launch => self.show_launch_view(ui),
                AppView::Check => self.show_check_view(ui),
                AppView::Alt => self.show_alt_view(ui),
                AppView::Settings => self.show_settings_view(ui),
                AppView::VolumeBot => self.show_simulation_v2_view(ui), // Renamed
                AppView::AtomicBuy => self.show_atomic_buy_view(ui),
                // Fallback for any other AppView variants not explicitly handled:
                _ => { ui.label(format!("View {:?} not implemented yet.", self.current_view)); }
            }
        });

        // Deferred spawning of Volume Bot task
        if let Some(params) = self.start_volume_bot_request.take() {
            if self.sim_in_progress { // Double check if we should still be starting
                info!("Spawning Volume Bot task from PumpFunApp::update with params for token: {}", params.token_mint_str);
                let task_handle = tokio::spawn(async move {
                    Self::run_volume_bot_simulation_task(
                        params.rpc_url,
                        params.wallets_file_path,
                        params.token_mint_str,
                        params.max_cycles,
                        params.slippage_bps,
                        params.solana_priority_fee,
                        params.initial_buy_sol_amount, // Added new argument
                        params.status_sender,
                        params.wallets_data_sender,
                        params.use_jito,
                        params.jito_be_url,
                        params.jito_tip_account,
                        params.jito_tip_lamports
                    ).await
                });
                self.sim_task_handle = Some(task_handle);
            } else {
                info!("Volume Bot start request was present, but sim_in_progress is now false. Task not spawned.");
                self.last_operation_result = Some(Ok("Volume Bot start cancelled as sim_in_progress is false.".to_string()));
            }
        }

        // Deferred spawning of Fetch Balances task
        if let Some(params) = self.start_fetch_balances_request.take() {
            info!("Spawning Fetch Balances task from PumpFunApp::update for {} addresses, target mint: {}", params.addresses_to_check.len(), params.target_mint);
            // Note: self.balances_loading should be managed appropriately before setting the request
            // or handled via messages back from the task if more complex UI updates are needed during loading.
            tokio::spawn(async move {
                Self::fetch_balances_task(
                    params.rpc_url,
                    params.addresses_to_check,
                    params.target_mint,
                    params.balance_fetch_sender
                ).await;
            });
            // No JoinHandle stored for fetch_balances_task as it's a fire-and-forget for UI updates via channel
        }

        // Deferred spawning of Load Volume Wallets from File task
        if let Some(params) = self.start_load_volume_wallets_request.take() {
            info!("Spawning Load Volume Wallets task from PumpFunApp::update for file: {}", params.file_path);
            // Note: UI state like self.volume_bot_wallets.clear() and self.volume_bot_wallets_generated = false
            // should have been set when the request was made in show_simulation_v2_view.
            tokio::spawn(async move {
                Self::load_volume_wallets_from_file_task(
                    params.file_path,
                    params.wallets_sender,
                    params.status_sender
                ).await;
            });
            // No JoinHandle stored for this task either.
        }

        // Deferred spawning of Distribute Total SOL task
        if let Some(params) = self.start_distribute_total_sol_request.take() {
            // Check the relevant flag for this operation, which is self.volume_bot_funding_in_progress
            if self.volume_bot_funding_in_progress {
                info!("Spawning Distribute Total SOL task from PumpFunApp::update. Source: ..., Targets: {}, Amount: {}", params.wallets_to_fund.len(), params.total_sol_to_distribute);
                tokio::task::spawn_blocking(move || {
                    Self::distribute_total_sol_to_volume_wallets_task(
                        params.rpc_url,
                        params.source_funding_keypair_str,
                        params.wallets_to_fund,
                        params.total_sol_to_distribute,
                        params.status_sender
                    );
                });
            } else {
                info!("Distribute Total SOL request was present, but volume_bot_funding_in_progress is false. Task not spawned.");
                 self.last_operation_result = Some(Ok("SOL Distribution cancelled as funding_in_progress is false.".to_string()));
            }
        }

        // Deferred spawning of Fund Volume Wallets task
        if let Some(params) = self.start_fund_volume_wallets_request.take() {
            if self.volume_bot_funding_in_progress { // Check relevant flag
                info!("Spawning Fund Volume Wallets task from PumpFunApp::update. Parent: ..., Targets: {}, Amount/Wallet: {}", params.wallets_to_fund.len(), lamports_to_sol(params.funding_amount_lamports));
                tokio::task::spawn_blocking(move || {
                    Self::fund_volume_bot_wallets_task(
                        params.rpc_url,
                        params.parent_keypair_str,
                        params.wallets_to_fund,
                        params.funding_amount_lamports,
                        params.status_sender
                    );
                });
            } else {
                info!("Fund Volume Wallets request was present, but volume_bot_funding_in_progress is false. Task not spawned.");
                self.last_operation_result = Some(Ok("Volume Wallet Funding cancelled as funding_in_progress is false.".to_string()));
            }
        }

        // Deferred spawning of Gather All Funds task
        if let Some(params) = self.start_gather_all_funds_request.take() {
            // Assuming a general sim_in_progress or a new flag like gather_in_progress might be set in the UI
            // For now, let's assume if the request is present, we try to spawn.
            info!("Spawning Gather All Funds task from PumpFunApp::update for token: {}", params.token_mint_ca_str);
            tokio::spawn(async move {
                Self::gather_all_funds_from_volume_wallets_task(
                    params.rpc_url,
                    params.parent_private_key_str,
                    params.volume_wallets,
                    params.token_mint_ca_str,
                    params.status_sender
                ).await;
            });
            // No JoinHandle stored for this task.
        }
        
        self.show_status_bar(ctx);
    }
} // Closes `impl eframe::App for PumpFunApp`
