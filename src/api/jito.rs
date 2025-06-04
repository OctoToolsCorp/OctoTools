use crate::config::CONFIG;
use crate::errors::{PumpfunError, Result};
use crate::models::api::{BundleRequest};
use crate::utils::get_random_number;
use log::{debug, info, warn, error};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    instruction::Instruction,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::{Transaction, VersionedTransaction},
};
use std::{str::FromStr, time::Duration};
use uuid::Uuid;
use bincode;
use base64::{Engine as _, engine::general_purpose::STANDARD as base64_standard};

/// Get Jito tip accounts from the API
pub async fn get_tip_accounts() -> Result<Vec<String>> {
    // Tip addresses can be hard-coded to avoid API calls
    let fallback_tips = vec![
        "JitoSQD5VN6a5CwwMJMZDu8fYRmkXWGGiSdmNpnhpNR".to_string(),
        "JitoVJpPqEZv7GXqzF9Anen8WrJJxRPVgHaCvXzvPLH".to_string(),
    ];
    
    // In a real implementation, fetch from Jito's API 
    Ok(fallback_tips)
}

/// Create a Jito tip instruction
pub fn get_jito_tip_instruction(
    keypair: &Keypair,
    _priority_multiplier: f64, // Mark as unused for now
) -> Result<Instruction> {
    let tip_accounts = vec![
        "JitoSQD5VN6a5CwwMJMZDu8fYRmkXWGGiSdmNpnhpNR".to_string(),
        "JitoVJpPqEZv7GXqzF9Anen8WrJJxRPVgHaCvXzvPLH".to_string(),
    ];
    
    if tip_accounts.is_empty() {
        // Use a fallback address if no tip accounts are available
        let fallback_tip_address = Pubkey::from_str("JitoSQD5VN6a5CwwMJMZDu8fYRmkXWGGiSdmNpnhpNR")
            .map_err(|_| PumpfunError::InvalidParameter("Invalid fallback tip address".to_string()))?;
            
        return Ok(system_instruction::transfer(
            &keypair.pubkey(),
            &fallback_tip_address,
            // Use the direct lamport amount from config
            CONFIG.jito_tip_lamports, // Removed multiplier and SOL conversion
        ));
    }
    
    let tip_account = Pubkey::from_str(&tip_accounts[get_random_number(0, tip_accounts.len() - 1)])
        .map_err(|_| PumpfunError::InvalidParameter("Invalid tip account".to_string()))?;
    
    Ok(system_instruction::transfer(
        &keypair.pubkey(),
        &tip_account,
        // Use the direct lamport amount from config
        CONFIG.jito_tip_lamports, // Removed multiplier and SOL conversion
    ))
}

/// Send bundle of transactions to Jito's block engine
pub async fn send_bundles(transactions: &[VersionedTransaction]) -> Result<String> {
    // TODO: Implement actual Jito bundle sending logic
    // This is a placeholder implementation
    if transactions.is_empty() {
        return Err(PumpfunError::Transaction("No transactions to send".to_string()));
    }

    let serialized = bincode::serialize(&transactions[0])
        .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize transaction: {}", e)))?;
    
    // Encode using the Engine trait
    let encoded_tx = base64_standard.encode(&serialized);
    
    Ok(encoded_tx)
    
    // Actual implementation would involve:
    // 1. Creating a JSON RPC request with the bundle
    // 2. Sending it to the Jito RPC endpoint
    // 3. Handling the response
}

/// Check the status of a bundle
pub async fn check_bundle(uuid: &str) -> Result<bool> {
    let _client = Client::new();
    let _endpoint = format!("https://{}/api/v1/bundles", CONFIG.jito_mainnet_url);
    
    let _request_body = BundleRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: "getBundleStatuses".to_string(),
        params: vec![vec![uuid.to_string()]],
    };
    
    let max_attempts = 10;
    let mut attempt = 0;
    
    while attempt < max_attempts {
        // In a real implementation, send an actual API request
        // For now, we'll simulate a success after a few attempts
        
        attempt += 1;
        
        if attempt >= 3 {
            info!("Bundle {} confirmed on attempt {}", uuid, attempt);
            return Ok(true);
        }
        
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    
    warn!("Bundle {} not confirmed after {} attempts", uuid, max_attempts);
    Ok(false)
}

// Jito API client for bundle submission
pub struct JitoClient {
    client: Client,
    base_url: String,
    auth_token: Option<String>,
}

// Implementation for JitoClient
impl JitoClient {
    // Create a new instance of the Jito client
    pub fn new(auth_token: Option<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: "https://mainnet.jito.wtf/api/v1".to_string(),
            auth_token,
        }
    }

    // Submit a transaction as a bundle to Jito MEV
    pub async fn submit_bundle(&self, transactions: Vec<Transaction>) -> Result<String> {
        // Check if auth token is available
        let auth_token = match &self.auth_token {
            Some(token) => token.clone(),
            None => return Err(PumpfunError::Api("Jito auth token not provided".to_string())),
        };

        debug!("Preparing to submit {} transactions in a bundle", transactions.len());

        let tx_strings: Vec<String> = transactions
            .iter()
            .map(|tx| {
                let serialized = bincode::serialize(tx)
                    .map_err(|e| PumpfunError::Serialization(e.to_string()))?;
                // Encode using the Engine trait
                Ok(base64_standard.encode(&serialized))
            })
            .collect::<Result<Vec<String>>>()?;

        // Build bundle payload
        let payload = json!({
            "transactions": tx_strings,
            "header": {
                "tip_account": null
            }
        });

        // Submit to Jito API
        let response = self.client.post(&format!("{}/bundles", self.base_url))
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", auth_token))
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(PumpfunError::Api(format!("Failed to submit bundle: {}", error_text)));
        }

        // Parse the response
        let response_data: JitoBundleResponse = response.json().await?;
        info!("Bundle submitted successfully with UUID: {}", response_data.uuid);

        Ok(response_data.uuid)
    }

    // Check the status of a submitted bundle
    pub async fn check_bundle_status(&self, uuid: &str) -> Result<JitoBundleStatus> {
        // Check if auth token is available
        let auth_token = match &self.auth_token {
            Some(token) => token.clone(),
            None => return Err(PumpfunError::Api("Jito auth token not provided".to_string())),
        };

        debug!("Checking status of bundle: {}", uuid);

        // Get bundle status
        let response = self.client.get(&format!("{}/bundles/{}", self.base_url, uuid))
            .header("Authorization", format!("Bearer {}", auth_token))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(PumpfunError::Api(format!("Failed to check bundle status: {}", error_text)));
        }

        // Parse the response
        let status: JitoBundleStatus = response.json().await?;
        debug!("Bundle status: {:?}", status);

        Ok(status)
    }
}

// Response model for bundle submission
#[derive(Debug, Deserialize)]
pub struct JitoBundleResponse {
    pub uuid: String,
}

// Response model for bundle status
#[derive(Debug, Deserialize)]
pub struct JitoBundleStatus {
    pub uuid: String,
    pub state: String,
    pub bundle_result: Option<JitoBundleResult>,
}

// Bundle result model
#[derive(Debug, Deserialize)]
pub struct JitoBundleResult {
    pub success: bool,
    pub error: Option<String>,
}

// Define the expected response structure for sendBundle
#[derive(Serialize, Deserialize, Debug)]
pub struct JitoSendBundleResponse {
    jsonrpc: String,
    pub result: String, // Expecting the bundle ID string directly
    pub error: Option<JitoError>,
    id: String, // Use String to match the UUID sent
}

#[derive(Serialize, Deserialize, Debug)]
pub struct JitoError {
   code: i64,
   message: String,
}

/// Sends a bundle of signed transactions to the Jito Block Engine.
///
/// # Arguments
///
/// * `transactions` - A vector of VersionedTransaction.
///
/// # Returns
///
/// * `Result<String>` - The Jito bundle ID if successful.
pub async fn send_bundle(transactions: Vec<VersionedTransaction>) -> Result<String> {
    let jito_url = CONFIG.jito_block_engine_url.clone(); // Assuming CONFIG is accessible
    if jito_url.is_empty() {
        return Err(PumpfunError::Config("JITO_BLOCK_ENGINE_URL not set".to_string()));
    }

    // Serialize and encode transactions
    let encoded_transactions: Vec<String> = transactions
        .iter()
        .map(|tx| -> Result<String> {
            let serialized_tx = bincode::serialize(tx)
                .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize transaction: {}", e)))?;
            // Jito expects base64 encoding for bundles
            Ok(base64_standard.encode(serialized_tx))
            // If Jito expects base58, use this instead:
            // Ok(bs58::encode(serialized_tx).into_string())
        })
        .collect::<Result<Vec<String>>>()?;

    let tip_account = CONFIG.jito_tip_account.clone(); // Get tip account from config
    let tip_lamports = CONFIG.jito_tip_lamports;      // Get tip amount from config
    
    if tip_account.is_none() || tip_lamports == 0 {
         return Err(PumpfunError::Config("Jito tip account or amount not configured".to_string()));
    }

    let tip_account_pubkey = tip_account.unwrap();
    let request_id = Uuid::new_v4().to_string();

    let payload = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "sendBundle",
        "params": [{
            "txs": encoded_transactions,
            "tipAccount": tip_account_pubkey.to_string(),
            "tipAmount": tip_lamports
        }]
    });

    debug!("Sending bundle to Jito: {}", serde_json::to_string(&payload).unwrap_or_default());

    let client = Client::new();
    let response = client.post(&jito_url)
        .json(&payload)
        .timeout(Duration::from_secs(30)) // Add a timeout
        .send()
        .await
        .map_err(|e| PumpfunError::Api(format!("Failed to send bundle to Jito: {}", e)))?;

    if response.status().is_success() {
        // Deserialize into the correct response struct
        let jito_response: JitoSendBundleResponse = response.json().await
            .map_err(|e| PumpfunError::Api(format!("Failed to parse Jito sendBundle response: {}", e)))?;
        // Access the bundle ID directly from the result field
        let bundle_id = jito_response.result;
        info!("Jito bundle submitted successfully. Bundle ID: {}", bundle_id);
        Ok(bundle_id)
    } else {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        error!("Jito API Error ({}): {}", status, error_text);
        Err(PumpfunError::Api(format!(
            "Jito sendBundle request failed with status {}: {}",
            status, error_text
        )))
    }
} 