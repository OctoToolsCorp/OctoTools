// src/api/jupiter.rs
use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use std::error::Error;
use spl_token::native_mint; // Import the native_mint module
use solana_sdk::pubkey::Pubkey;
use base64::{engine::general_purpose, Engine as _};

pub const JUPITER_API_BASE_URL: &str = "https://quote-api.jup.ag/v6";

#[derive(Serialize, Deserialize, Debug)]
struct PriceData {
    price: f64,
}

#[derive(Serialize, Deserialize, Debug)]
struct PriceResponse {
    data: std::collections::HashMap<String, PriceData>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest {
    pub input_mint: String,
    pub output_mint: String,
    pub amount: u64,
    pub slippage_bps: u16,
    // pub platform_fee_bps: Option<u8>, // Example: 10 for 0.1%
    pub only_direct_routes: Option<bool>,
    pub user_public_key: Option<String>, // Required for some advanced features or token-gated APIs
    pub minimize_slippage: Option<bool>,
    // pub as_legacy_transaction: Option<bool>, // If you need legacy transactions
    // ... other optional fields as per Jupiter API docs
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlatformFee {
    pub amount: String,
    pub fee_bps: u8,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlanStep {
    pub swap_info: SwapInfo,
    pub percent: u8,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    pub label: String,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub fee_amount: String,
    pub fee_mint: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MarketInfo {
    pub id: String,
    pub label: String,
    pub input_mint: String,
    pub output_mint: String,
    pub not_enough_liquidity: bool,
    pub in_amount: String,
    pub out_amount: String,
    pub price_impact_pct: f64, // Can be number or string, use f64 and handle potential parsing if string
    pub lp_fee: Option<PlatformFee>, // Re-using PlatformFee for lp_fee structure
    pub platform_fee: Option<PlatformFee>,
}


// Added Serialize to QuoteResponse and more fields
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String, // lamports
    pub output_mint: String,
    pub out_amount: String, // lamports
    pub other_amount_threshold: String, // lamports, minimum out amount
    pub swap_mode: String, // "ExactIn" or "ExactOut"
    pub slippage_bps: u16,
    pub platform_fee: Option<PlatformFee>,
    pub price_impact_pct: String, // String representation of a number, e.g., "0.0007601600000000001"
    pub route_plan: Vec<RoutePlanStep>,
    pub context_slot: Option<u64>, // Made Option as it might not always be present
    pub time_taken: Option<f64>,   // Made Option
    // Additional fields that might be part of the response
    pub key: Option<String>, // Example, if there's a unique key for the quote
    pub market_info: Option<Vec<MarketInfo>>, // If detailed market info is provided per leg
    // The following are often part of the quote but might be under a different key or structure
    // For simplicity, adding them here. Adjust if they are nested.
    // pub amount: String, // This is usually `in_amount` or `out_amount` depending on swap_mode
    // pub out_amount_with_slippage: String,
    // pub min_out_amount: Option<String>, // alias for otherAmountThreshold
}


#[derive(Serialize, Debug)]
    #[serde(rename_all = "camelCase")]
    pub struct SwapInstructionsRequest {
    pub user_public_key: String,
    pub quote_response: QuoteResponse, // The full quote response from the /quote endpoint
    pub wrap_and_unwrap_sol: Option<bool>, // Default true
    // pub fee_account: Option<String>, // For platform fees
    pub compute_unit_price_micro_lamports: Option<u64>, // For priority fees
    pub as_legacy_transaction: Option<bool>, // If you need to build a legacy transaction
    // ... other optional fields
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BlockhashWithMetadata {
    pub blockhash: Vec<u8>, // Changed from String to Vec<u8> based on raw log
    // Temporarily removing last_valid_block_height to isolate deserialization of blockhash
    // pub last_valid_block_height: u64,
    // pub fetched_at: Option<FetchedAt>, // Optional, depending on if you need it
}

// #[derive(Deserialize, Debug)]
// #[serde(rename_all = "camelCase")]
// pub struct FetchedAt {
//     pub secs_since_epoch: u64,
//     pub nanos_since_epoch: u32,
// }


#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionsResponse {
    // pub token_ledger_instruction: Option<Instruction>, // If using token ledger
    pub compute_budget_instructions: Vec<InstructionJupiter>, // Compute budget instructions
    pub setup_instructions: Vec<InstructionJupiter>,          // Setup instructions (e.g., ATA creation)
    pub swap_instruction: InstructionJupiter,                 // The actual swap instruction
    pub cleanup_instruction: Option<InstructionJupiter>,      // Cleanup instruction (e.g., close ATA)
    pub address_lookup_table_addresses: Vec<String>,
    #[serde(rename = "blockhashWithMetadata")] // Ensure correct deserialization if field name differs
    pub blockhash_with_metadata: Option<BlockhashWithMetadata>, // Made Option in case it's not always present
    // pub error: Option<String>, // Jupiter might return an error message here
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct InstructionJupiter {
    pub program_id: String,
    pub accounts: Vec<AccountMetaJupiter>,
    pub data: String, // Base58 encoded
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccountMetaJupiter {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}


/// Fetches SOL and a specific token's price from Jupiter Price API v4.
pub async fn get_prices(
    http_client: &ReqwestClient,
    token_mint: &str,
) -> Result<(f64, f64), Box<dyn Error>> {
    let sol_mint_address = native_mint::ID.to_string();
    let ids = format!("{},{}", sol_mint_address, token_mint);
    let params = [("ids", ids.as_str()), ("vsToken", "USDC")]; // Example: price against USDC

    let price_api_url = "https://price.jup.ag/v4/price"; // Using v4 for price

    let response = http_client
        .get(price_api_url)
        .query(&params)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        return Err(format!("Price API v4 failed with status {}: {}", status, text).into());
    }

    let price_response: PriceResponse = response.json().await?;

    let sol_price = price_response
        .data
        .get(&sol_mint_address)
        .ok_or("SOL price data not found in response")?
        .price;

    let token_price = price_response
        .data
        .get(token_mint)
        .ok_or(format!("Token {} price data not found in response", token_mint))?
        .price;

    Ok((sol_price, token_price))
}

/// Fetches a quote from Jupiter API v6.
pub async fn get_jupiter_quote(
    http_client: &ReqwestClient,
    request: &QuoteRequest,
) -> Result<QuoteResponse, Box<dyn Error>> {
    let quote_url = format!("{}/quote", JUPITER_API_BASE_URL);
    // log::info!("Requesting Jupiter quote from: {}", quote_url);
    // log::info!("Quote request: {:?}", request);

    let response = http_client
        .get(&quote_url)
        .query(request) // GET request with query parameters
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        // log::error!("Jupiter quote API failed with status {}: {}", status, error_text);
        return Err(format!("Jupiter quote API failed with status {}: {}", status, error_text).into());
    }

    let quote_response: QuoteResponse = response.json().await?;
    // log::info!("Received Jupiter quote response: {:?}", quote_response);
    Ok(quote_response)
}

/// Fetches swap instructions from Jupiter API v6.
pub async fn get_jupiter_swap_instructions(
    http_client: &ReqwestClient,
    request: &SwapInstructionsRequest,
) -> Result<SwapInstructionsResponse, Box<dyn Error>> {
    let swap_instructions_url = format!("{}/swap-instructions", JUPITER_API_BASE_URL);
    // log::info!("Requesting Jupiter swap instructions from: {}", swap_instructions_url);
    // log::info!("Swap instructions request: {:?}", request);

    let response = http_client
        .post(&swap_instructions_url) // POST request
        .json(request) // Send request body as JSON
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        // log::error!("Jupiter swap-instructions API failed with status {}: {}", status, error_text);
        return Err(format!("Jupiter swap-instructions API failed with status {}: {}", status, error_text).into());
    }

    let swap_instructions_response: SwapInstructionsResponse = response.json().await?;
    // log::info!("Received Jupiter swap instructions response: {:?}", swap_instructions_response);
    Ok(swap_instructions_response)
}

// Helper to convert Jupiter instruction to Solana SDK Instruction
pub fn jupiter_instruction_to_sdk(
    jupiter_ix: &InstructionJupiter,
) -> Result<solana_sdk::instruction::Instruction, Box<dyn Error>> {
    let program_id = Pubkey::try_from(jupiter_ix.program_id.as_str())?;
    let accounts = jupiter_ix
        .accounts
        .iter()
        .map(|acc| solana_sdk::instruction::AccountMeta {
            pubkey: Pubkey::try_from(acc.pubkey.as_str()).unwrap(),
            is_signer: acc.is_signer,
            is_writable: acc.is_writable,
        })
        .collect();
    let data = general_purpose::STANDARD.decode(&jupiter_ix.data)?; // Use STANDARD engine

    Ok(solana_sdk::instruction::Instruction {
        program_id,
        accounts,
        data,
    })
}