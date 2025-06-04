use crate::errors::{PumpfunError, Result};
use log::{debug, warn, error, info};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signature},
    transaction::{VersionedTransaction},
    pubkey::Pubkey,
    // account::Account, // Removed unused import
    // commitment_config::CommitmentConfig, // Removed unused import
};
use std::time::Duration;
use std::thread;
use std::sync::Arc;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct BundleStatusError {
    #[serde(rename = "Ok")]
    pub ok: Option<serde_json::Value>, 
}

#[derive(Debug, Deserialize, Clone)]
pub struct BundleStatusInfo {
    pub bundle_id: String,
    pub transactions: Vec<String>,
    pub slot: u64,
    pub confirmation_status: String,
    pub err: Option<BundleStatusError>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BundleStatusesContext {
    pub slot: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GetBundleStatusesResult {
    pub context: BundleStatusesContext,
    pub value: Vec<BundleStatusInfo>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct JitoRpcResponse<T> {
    pub jsonrpc: String,
    pub result: Option<T>,
    pub id: u64,
    pub error: Option<JsonValue>,
}

use reqwest::Client as ReqwestClient;
use serde_json::{json, Value as JsonValue};
use base64::{engine::general_purpose::{STANDARD as BASE64_STANDARD}, Engine as _};
use bincode;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;

// Sign and send a versioned transaction (standard RPC)
pub async fn sign_and_send_versioned_transaction(
    rpc_client: &RpcClient,
    transaction: VersionedTransaction,
    signers: &[&Keypair],
) -> Result<Signature> {
    let mut retries = 0;
    
    debug!("Sending versioned transaction...");

    while retries < MAX_RETRIES {
        match rpc_client.get_latest_blockhash().await {
            Ok(blockhash) => {
                let mut message = transaction.message.clone();
                
                if let solana_sdk::message::VersionedMessage::V0(ref mut v0_message) = message {
                    debug!("Updating blockhash for versioned transaction. Old: {}, New: {}", 
                           v0_message.recent_blockhash, blockhash);
                    v0_message.recent_blockhash = blockhash;
                } else {
                    debug!("Transaction message is not V0, cannot update blockhash");
                }
                
                let signed_tx = match VersionedTransaction::try_new(message, signers) {
                    Ok(tx) => tx,
                    Err(err) => {
                        warn!("Failed to sign transaction: {:?}", err);
                        return Err(PumpfunError::Transaction(
                            format!("Failed to sign transaction: {:?}", err),
                        ));
                    }
                };
                
                debug!("Sending transaction with {} signatures", signed_tx.signatures.len());
                if !signed_tx.signatures.is_empty() {
                    debug!("First signature: {}", signed_tx.signatures[0]);
                }
                
                match rpc_client.send_transaction(&signed_tx).await {
                    Ok(sig) => {
                        debug!("Transaction sent successfully: {:?}", sig);
                        return Ok(sig);
                    }
                    Err(err) => {
                        warn!("Failed to send versioned transaction: {:?}", err);
                        if err.to_string().contains("Blockhash not found") {
                            warn!("The blockhash {} is no longer valid", blockhash);
                        }
                        if err.to_string().contains("insufficient funds") {
                            warn!("Insufficient funds for transaction");
                        }
                        retries += 1;
                        if retries >= MAX_RETRIES {
                            break;
                        }
                        thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
                    }
                }
            }
            Err(err) => {
                warn!(
                    "Failed to get latest blockhash: {:?}, retrying ({}/{})",
                    err,
                    retries + 1,
                    MAX_RETRIES
                );
                retries += 1;
                if retries >= MAX_RETRIES {
                    break;
                }
                thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
            }
        }
    }

    Err(PumpfunError::Transaction(
        "Max retries exceeded".to_string(),
    ))
}

// --- Jito Bundle Sending via JSON-RPC --- //
pub async fn send_jito_bundle_via_json_rpc(
    transactions: Vec<VersionedTransaction>,
    jito_block_engine_url: &str,
) -> Result<String> {
    if transactions.is_empty() {
        return Err(PumpfunError::Bundling("Cannot send an empty bundle".to_string()));
    }
    debug!("Sending {} transactions as a Jito bundle via JSON-RPC...", transactions.len());

    let bundles_endpoint = if jito_block_engine_url.ends_with("/api/v1/bundles") {
        jito_block_engine_url.to_string()
    } else {
        let base = jito_block_engine_url.trim_end_matches('/');
        format!("{}/api/v1/bundles", base)
    };

    let mut encoded_transactions: Vec<String> = Vec::with_capacity(transactions.len());
    for (i, transaction) in transactions.iter().enumerate() {
        let tx_bytes = bincode::serialize(transaction)
            .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize transaction #{} in bundle: {}", i, e)))?;
        debug!("Serialized transaction #{} size: {} bytes", i, tx_bytes.len());

        if tx_bytes.len() > 1232 {
            error!("Transaction #{} exceeds max size ({} bytes > 1232 bytes)!", i, tx_bytes.len());
            if !transaction.signatures.is_empty() {
                error!("  Failing Transaction Signature (first): {}", transaction.signatures[0]);
            }
            return Err(PumpfunError::Transaction(format!(
                "Transaction #{} exceeds max size ({} bytes > 1232 bytes)", 
                i, tx_bytes.len()
            )));
        }
        
        let tx_base64 = BASE64_STANDARD.encode(&tx_bytes);
        encoded_transactions.push(tx_base64);
    }

    let encoding_param = json!({ "encoding": "base64" }); 
    let request_payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [encoded_transactions, encoding_param]
    });
    debug!("Constructed sendBundle JSON-RPC payload with {} transactions and explicit encoding.", transactions.len());

    let http_client = ReqwestClient::new();
    match http_client.post(&bundles_endpoint)
        .json(&request_payload)
        .send()
        .await {
        Ok(response) => {
            let status = response.status();
            let response_text = response.text().await.unwrap_or_else(|_| "Failed to read Jito response text".to_string());
            debug!("Jito response status: {}", status);
            debug!("Jito response body: {}\n", response_text);

            if status.is_success() {
                match serde_json::from_str::<JsonValue>(&response_text) {
                    Ok(json_response) => {
                        if let Some(bundle_id) = json_response.get("result").and_then(|v| v.as_str()) {
                            info!("Bundle submitted successfully via JSON-RPC. Bundle ID: {}", bundle_id);
                            Ok(bundle_id.to_string())
                        } else if let Some(error) = json_response.get("error") {
                            error!("Jito JSON-RPC Error: {}", error);
                            Err(PumpfunError::Bundling(format!("Jito API error: {}", error)))
                        } else {
                            error!("Unexpected Jito JSON response format (Success Status): {}", response_text);
                            Err(PumpfunError::Bundling("Unexpected Jito JSON response format (Success Status)".to_string()))
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse Jito JSON response (Success Status): {} - Body: {}", e, response_text);
                        Err(PumpfunError::Bundling(format!("Failed to parse Jito JSON response (Success Status): {}", e)))
                    }
                }
            } else {
                match serde_json::from_str::<JsonValue>(&response_text) {
                     Ok(json_response) => {
                         if let Some(error) = json_response.get("error") {
                             error!("Jito API Error (Status {}): {}", status, error);
                             Err(PumpfunError::Bundling(format!("Jito API error (Status {}): {}", status, error)))
                         } else {
                             error!("Jito request failed with status {} and non-standard error body: {}", status, response_text);
                             Err(PumpfunError::Bundling(format!("Jito request failed with status {} and non-standard error body", status)))
                         }
                     }
                     Err(_) => {
                         error!("Jito request failed with status {}: {}", status, response_text);
                         Err(PumpfunError::Bundling(format!("Jito request failed with status {}: {}", status, response_text)))
                     }
                 }
            }
        }
        Err(e) => {
            error!("Failed to send HTTP request to Jito: {}", e);
            Err(PumpfunError::Http(e))
        }
    }
}

// --- Transaction Simulation --- //
use solana_client::{
    rpc_response::RpcSimulateTransactionResult,
    rpc_config::{RpcSimulateTransactionConfig, RpcSimulateTransactionAccountsConfig} // Added for config
};
// use solana_sdk::account::Account; // Not used if replace_accounts is unavailable
use solana_account_decoder::UiAccountEncoding; // Added for encoding
// use solana_transaction_status::UiTransactionEncoding; // Not used in basic sim

pub async fn simulate_and_check(
    rpc_client: &Arc<RpcClient>,
    transaction: &VersionedTransaction,
    tx_description: &str,
    accounts_to_fetch: Option<Vec<Pubkey>>, // Parameter for specific accounts
    // replace_accounts_for_sim: Option<Vec<(Pubkey, Account)>>, // Removed as field is not available
) -> Result<RpcSimulateTransactionResult> {
    info!("  Simulating {} with config...", tx_description);

    let accounts_config = accounts_to_fetch.map(|keys| {
        RpcSimulateTransactionAccountsConfig {
            encoding: Some(UiAccountEncoding::Base64), // Specify Base64 encoding
            addresses: keys.into_iter().map(|pk| pk.to_string()).collect(),
        }
    });
 
    // Assuming RpcSimulateTransactionConfig does not have 'replace_accounts'
    // but might have 'inner_instructions' or other fields based on the error.
    // For now, construct with fields known to be common or indicated by error.
    let sim_config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(rpc_client.commitment()),
        encoding: None, // This is for the transaction encoding, not account data.
        accounts: accounts_config,
        min_context_slot: None,
        inner_instructions: false, // Add inner_instructions field as indicated by compiler
    };
    
    match rpc_client.simulate_transaction_with_config(transaction, sim_config).await {
        Ok(simulation_result) => {
            if let Some(ref err) = simulation_result.value.err {
                error!("{} Simulation Error: {:?}", tx_description, err);
                if let Some(ref logs) = simulation_result.value.logs {
                    error!("Simulation Logs:");
                    for log in logs {
                        error!("- {}", log);
                    }
                }
                Err(PumpfunError::TransactionSimulation(format!("Simulation failed for {}: {:?}", tx_description, err)))
            } else {
                info!("  ✅ {} Simulation Successful (RPC reported no errors).", tx_description);
                let logs = simulation_result.value.logs.as_deref().unwrap_or(&[]);
                let units_consumed = simulation_result.value.units_consumed.unwrap_or(0);
                debug!("  {} Simulated Compute Units: {}", tx_description, units_consumed);

                if !logs.is_empty() {
                     info!("Simulation Logs:");
                     for log in logs {
                         info!("- {}", log);
                     }
                 } else {
                     info!("No simulation logs available.");
                 }
                Ok(simulation_result.value)
            }
        }
        Err(client_error) => {
            error!("{} Simulation RPC Error: {:?}", tx_description, client_error);
            Err(PumpfunError::SolanaClient(client_error))
        }
    }
}
// --- Jito Bundle Status Check --- //
pub async fn get_jito_bundle_statuses(
    bundle_ids: Vec<String>,
    jito_block_engine_url: &str,
) -> Result<Option<GetBundleStatusesResult>> {
    if bundle_ids.is_empty() {
        return Err(PumpfunError::Bundling("Cannot get status for empty bundle ID list".to_string()));
    }
    if bundle_ids.len() > 5 {
        return Err(PumpfunError::InvalidInput("Too many bundle IDs for getBundleStatuses, max 5".to_string()));
    }
    debug!("Getting Jito bundle statuses for IDs: {:?}", bundle_ids);

    // Jito's getBundleStatuses is also sent to the /api/v1/bundles endpoint
    let bundles_endpoint = if jito_block_engine_url.ends_with("/api/v1/bundles") {
        jito_block_engine_url.to_string()
    } else {
        let base = jito_block_engine_url.trim_end_matches('/');
        format!("{}/api/v1/bundles", base)
    };
    
    let request_payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getBundleStatuses",
        "params": [bundle_ids] 
    });
    debug!("Constructed getBundleStatuses JSON-RPC payload: {}", serde_json::to_string_pretty(&request_payload).unwrap_or_default());

    let http_client = ReqwestClient::new();
    match http_client.post(&bundles_endpoint)
        .json(&request_payload)
        .send()
        .await {
        Ok(response) => {
            let status = response.status();
            let response_text = response.text().await.unwrap_or_else(|_| "Failed to read Jito response text for getBundleStatuses".to_string());
            debug!("Jito getBundleStatuses response status: {}", status);
            debug!("Jito getBundleStatuses response body: {}\n", response_text);

            if status.is_success() {
                match serde_json::from_str::<JitoRpcResponse<GetBundleStatusesResult>>(&response_text) {
                    Ok(json_response) => {
                        if let Some(rpc_error) = json_response.error {
                            error!("Jito RPC Error in getBundleStatuses: {}", rpc_error);
                            Err(PumpfunError::Bundling(format!("Jito API error in getBundleStatuses: {}", rpc_error)))
                        } else {
                            Ok(json_response.result)
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse Jito getBundleStatuses JSON response (Success Status): {} - Body: {}", e, response_text);
                        Err(PumpfunError::Bundling(format!("Failed to parse Jito getBundleStatuses JSON response (Success Status): {}", e)))
                    }
                }
            } else {
                 match serde_json::from_str::<JitoRpcResponse<GetBundleStatusesResult>>(&response_text) {
                     Ok(json_response) => {
                         if let Some(rpc_error) = json_response.error {
                            error!("Jito API Error in getBundleStatuses (Status {}): {}", status, rpc_error);
                            Err(PumpfunError::Bundling(format!("Jito API error in getBundleStatuses (Status {}): {}", status, rpc_error)))
                         } else {
                            error!("Jito getBundleStatuses request failed with status {} and non-standard error body: {}", status, response_text);
                            Err(PumpfunError::Bundling(format!("Jito getBundleStatuses request failed with status {} and non-standard error body", status)))
                         }
                     }
                     Err(_) => {
                        error!("Jito getBundleStatuses request failed with status {}: {}", status, response_text);
                        Err(PumpfunError::Bundling(format!("Jito getBundleStatuses request failed with status {}: {}", status, response_text)))
                     }
                 }
            }
        }
        Err(e) => {
            error!("Failed to send HTTP request to Jito for getBundleStatuses: {}", e);
            Err(PumpfunError::Http(e))
        }
    }
}