// Raydium API integration will go here.

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::VersionedTransaction;
use anyhow::{Result, Context, anyhow};
use log::{debug, error}; // Added error for logging
use reqwest::Client; // Added reqwest client
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use bincode;

// --- Constants ---
const RAYDIUM_API_BASE_URL: &str = "https://api.raydium.io/v2"; // For general API calls like priority fee
const RAYDIUM_SWAP_HOST: &str = "https://transaction-v1.raydium.io/"; // As per docs for trade/swap
pub const NATIVE_SOL_MINT: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");

// --- Request Structures ---

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumQuoteRequestParams {
    pub input_mint: String,
    pub output_mint: String,
    pub amount: u64,
    pub slippage_bps: u16,
    // pub tx_version: String, // "V0" or "LEGACY" - Seems this is for the /transaction endpoint, not quote
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumSwapRequest {
    pub compute_unit_price_micro_lamports: Option<String>, // String representation of u64
    pub swap_response: RaydiumQuoteResponse, // The full JSON object from quote
    #[serde(rename = "txVersion")]
    pub tx_version: String, // "V0" or "LEGACY"
    pub wallet: String,     // buyer pubkey as string
    pub wrap_sol: bool,
    pub unwrap_sol: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_account: Option<String>,
    // #[serde(skip_serializing_if = "Option::is_none")]
    // pub use_versioned_transaction: Option<bool>, // Covered by tx_version
    // #[serde(skip_serializing_if = "Option::is_none")]
    // pub only_direct_routes: Option<bool>, // For quote, not transaction
    // #[serde(skip_serializing_if = "Option::is_none")]
    // pub client_order_id: Option<String>, // Optional
}

// --- Response Structures ---

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RaydiumQuoteResponse(pub serde_json::Value); // Wrapper around serde_json::Value

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumFeeInfo {
    pub amount: String, // String representation of u64
    pub mint: String,
    pub pct: f64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumRoute {
    pub keys: Vec<String>, // Pool addresses
    // ... other route details
}


#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumSwapTransactionResponseData {
    pub transaction: String, // Base64 encoded transaction
    // The example shows `data: { transaction: string }[]`
    // So this should be a vec.
    // Let's adjust:
    // pub transactions: Vec<RaydiumSingleTransactionData>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumSingleTransactionData {
    pub transaction: String, // Base64 encoded transaction
    // pub id: Option<String>, // Example doesn't show id per transaction string
}


#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumSwapTransactionsResponse {
    // pub id: String, // Example shows these fields
    // pub version: String,
    // pub success: bool,
    // data: Vec<RaydiumSingleTransactionData>, // This matches the example
    // The example is: { data: { transaction: string }[] }
    // So, the outer structure is simpler:
    pub setup_transaction: Option<String>, // Base64 string for setup tx
    pub swap_transaction: String,        // Base64 string for swap tx
    // The API might return one or more transactions.
    // The example `data: { transaction: string }[]` suggests an array.
    // Let's use the structure from the example:
    // `await axios.post<{ id: string; version: string; success: boolean; data: { transaction: string }[] }>`
    // This means the top level response is an object with `id`, `version`, `success`, and `data`.
    // And `data` is an array of objects, each with a `transaction` field.
    // This is confusing. The Raydium SDK docs for /trade/v1 (POST) show:
    // "returns": { "setupTransaction": "string?", "swapTransaction": "string" }
    // Let's go with this simpler structure for now, as it's more direct.
    // If it's an array, we'll adjust.
    // The example `allTxBuf = swapTransactions.data.map((tx) => Buffer.from(tx.transaction, 'base64'))`
    // implies `swapTransactions.data` is an array of objects, each with a `transaction` field.
    // Let's reconcile.
    // The type hint in the example is: ` { data: { transaction: string }[] } `
    // This means the actual response for the transaction endpoint is an object,
    // which has a field `data` that is an array of objects, and each of those objects
    // has a `transaction` field (string).
    // So:
    // pub data: Vec<RaydiumSingleTransactionData>,
    // And the overall response:
    // pub id: String,
    // pub version: String,
    // pub success: bool,
    // pub data: Vec<RaydiumSingleTransactionData>,
    // This seems correct based on the example's type hint.
}

// Let's redefine based on the example's type hint for the POST response:
// `axios.post<{ id: string; version: string; success: boolean; data: { transaction: string }[] }>`
#[derive(Deserialize, Debug)]
pub struct RaydiumSerializedTransaction {
    pub transaction: String, // base64 encoded
}

#[derive(Deserialize, Debug)]
pub struct RaydiumSwapPostResponse {
    pub id: String,
    pub version: String,
    pub success: bool,
    pub data: Vec<RaydiumSerializedTransaction>,
}


// --- API Functions (Stubs) ---

pub async fn get_raydium_quote(
    client: &Client,
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount_lamports: u64,
    slippage_bps: u16,
    tx_version: &str, // "V0" or "LEGACY"
    swap_mode: &str, // "ExactIn" or "ExactOut"
) -> Result<RaydiumQuoteResponse> {
    // Updated endpoint path based on common Raydium SDK patterns.
    // Note: txVersion is often not part of the quote GET request itself.
    // swap_mode might be implied by the endpoint or a query param like 'mode'.
    // For now, assuming 'mode' is a query param if needed, or implied by amount type.
    let endpoint_path = match swap_mode {
        "ExactIn" => "compute/swap-base-in",
        "ExactOut" => "compute/swap-base-out",
        _ => return Err(anyhow!("Invalid swap_mode: {}. Must be 'ExactIn' or 'ExactOut'", swap_mode)),
    };

    let url = format!(
        "{}{}?inputMint={}&outputMint={}&amount={}&slippageBps={}&txVersion={}", // Re-added txVersion
        RAYDIUM_SWAP_HOST, // Use the specific swap host
        endpoint_path,
        input_mint.to_string(),
        output_mint.to_string(),
        amount_lamports,
        slippage_bps,
        tx_version // Pass the tx_version received by the function
    );
    debug!("Fetching Raydium quote from: {}", url);

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to send request to Raydium quote API")?;

    if response.status().is_success() {
        // Deserialize into serde_json::Value first to inspect it
        let response_value = response
            .json::<serde_json::Value>()
            .await
            .context("Failed to deserialize Raydium quote response into JSON Value")?;
        
        debug!("Raw Raydium quote response JSON: {:?}", response_value);

        // Check for success field or specific error messages
        if let Some(success_val) = response_value.get("success") {
            if success_val == false {
                let msg = response_value.get("msg").and_then(|v| v.as_str()).unwrap_or("Unknown error from Raydium quote API");
                error!("Raydium quote API indicated failure: success is false, msg: {}", msg);
                return Err(anyhow!("Raydium quote API failed: {}", msg));
            }
        }
        // Even if success field is not explicitly false, ROUTE_NOT_FOUND is a failure
        if let Some(msg_val) = response_value.get("msg") {
            if msg_val.as_str() == Some("ROUTE_NOT_FOUND") {
                error!("Raydium quote API reported ROUTE_NOT_FOUND.");
                return Err(anyhow!("Raydium quote API: ROUTE_NOT_FOUND for the given pair."));
            }
        }

        // If checks pass, wrap the value in our struct
        let quote_response = RaydiumQuoteResponse(response_value);
        debug!("Successfully processed Raydium quote: {:?}", quote_response);
        Ok(quote_response)
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read error body".to_string());
        error!(
            "Raydium quote API request failed with status {}: {}",
            status, error_text
        );
        Err(anyhow!(
            "Raydium quote API request failed with status {}: {}",
            status,
            error_text
        ))
    }
}

pub async fn get_raydium_swap_transactions(
    client: &Client,
    quote_response: RaydiumQuoteResponse,
    buyer_pk: Pubkey,
    priority_fee_micro_lamports: Option<u64>,
    tx_version: &str, // "V0" or "LEGACY"
    wrap_sol: bool,
    unwrap_sol: bool, // Added unwrap_sol
    input_account_opt: Option<Pubkey>,
    output_account_opt: Option<Pubkey>,
    // swap_mode is part of quote_response
) -> Result<Vec<VersionedTransaction>> {
    // Extract swap_mode from the serde_json::Value
    let swap_mode_str = quote_response.0
        .get("data") // Access the "data" object first
        .and_then(|data_obj| data_obj.get("swapType")) // Then get "swapType" from the "data" object
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Failed to extract swapType from RaydiumQuoteResponse JSON value (data.swapType)"))?;

    let endpoint_path = match swap_mode_str {
        "ExactIn" | "BaseIn" => "transaction/swap-base-in", // Accept "BaseIn" from quote
        "ExactOut" | "BaseOut" => "transaction/swap-base-out", // Accept "BaseOut" from quote
        _ => return Err(anyhow!("Invalid swapType ('{}') in quote_response. Expected 'ExactIn', 'BaseIn', 'ExactOut', or 'BaseOut'", swap_mode_str)),
    };

    let url = format!("{}{}", RAYDIUM_SWAP_HOST, endpoint_path);

    let request_payload = RaydiumSwapRequest {
        compute_unit_price_micro_lamports: priority_fee_micro_lamports.map(|fee| fee.to_string()),
        swap_response: quote_response, // Pass the wrapper directly (it contains the Value)
        tx_version: tx_version.to_string(),
        wallet: buyer_pk.to_string(),
        wrap_sol,
        unwrap_sol,
        input_account: input_account_opt.map(|pk| pk.to_string()),
        output_account: output_account_opt.map(|pk| pk.to_string()),
    };

    debug!("Sending Raydium swap transaction request to: {}", url);
    debug!("Request payload: {:?}", request_payload);

    let response = client
        .post(&url)
        .json(&request_payload)
        .send()
        .await
        .context("Failed to send request to Raydium swap transaction API")?;

    if response.status().is_success() {
        let swap_post_response = response
            .json::<RaydiumSwapPostResponse>()
            .await
            .context("Failed to deserialize Raydium swap transaction response")?;
        
        debug!("Successfully received Raydium swap transaction response: {:?}", swap_post_response);

        if !swap_post_response.success {
            return Err(anyhow!("Raydium swap transaction API indicated failure. ID: {}, Version: {}", swap_post_response.id, swap_post_response.version));
        }

        let mut versioned_transactions = Vec::new();
        for serialized_tx_data in swap_post_response.data {
            let tx_buf = BASE64_STANDARD.decode(&serialized_tx_data.transaction)
                .context("Failed to decode base64 transaction string")?;

            let versioned_tx = if tx_version == "V0" {
                // For VersionedTransaction, bincode::deserialize is typically used for the entire struct
                // when it's stored as raw bytes. If tx_buf is just the message bytes,
                // then VersionedTransaction::deserialize might expect a Serde deserializer.
                // However, Solana's VersionedTransaction itself implements Deserialize.
                // The issue might be how it's called.
                // Let's try bincode::deserialize directly on the buffer for V0 as well,
                // as VersionedTransaction should support this.
                bincode::deserialize(&tx_buf)
                    .context("Failed to deserialize V0 transaction using bincode")?
            } else if tx_version == "LEGACY" {
                // This part seems correct for legacy transactions.
                let legacy_tx: solana_sdk::transaction::Transaction = bincode::deserialize(&tx_buf)
                    .context("Failed to deserialize LEGACY transaction using bincode")?;
                VersionedTransaction::from(legacy_tx) // Convert legacy to VersionedTransaction
            } else {
                return Err(anyhow!("Unsupported tx_version for deserialization: {}", tx_version));
            };
            versioned_transactions.push(versioned_tx);
        }
        Ok(versioned_transactions)
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read error body".to_string());
        error!(
            "Raydium swap transaction API request failed with status {}: {}",
            status, error_text
        );
        Err(anyhow!(
            "Raydium swap transaction API request failed with status {}: {}",
            status,
            error_text
        ))
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumPriorityFeeData {
    pub vh: u64, // very high
    pub h: u64,  // high
    pub m: u64,  // medium
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumPriorityFeeDefault {
    #[serde(rename = "default")]
    pub default_fees: RaydiumPriorityFeeData,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RaydiumPriorityFeeResponse {
    // pub id: String, // Not always present or needed
    // pub success: bool,
    pub data: RaydiumPriorityFeeDefault,
}

pub async fn get_raydium_priority_fee(
    client: &Client,
) -> Result<RaydiumPriorityFeeResponse> {
    let url = format!("{}/priority/fee", RAYDIUM_API_BASE_URL);
    debug!("Fetching Raydium priority fee from: {}", url);

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to send request to Raydium priority fee API")?;

    if response.status().is_success() {
        let fee_response = response
            .json::<RaydiumPriorityFeeResponse>()
            .await
            .context("Failed to deserialize Raydium priority fee response")?;
        debug!("Successfully fetched Raydium priority fees: {:?}", fee_response);
        Ok(fee_response)
    } else {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Failed to read error body".to_string());
        error!(
            "Raydium priority fee API request failed with status {}: {}",
            status, error_text
        );
        Err(anyhow!(
            "Raydium priority fee API request failed with status {}: {}",
            status,
            error_text
        ))
    }
}