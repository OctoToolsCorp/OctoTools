use crate::config::CONFIG;
use crate::errors::{Result, PumpfunError};
use crate::models::token::{PumpfunMetadata};
use crate::models::api::{IpfsResponse};
use log::{debug, info, error};
use reqwest::{
    Client,
    multipart::{Form, Part},
};
use reqwest::header::{CONTENT_TYPE};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::{VersionedTransaction},
};
use std::{
    fs::File,
    io::Read,
    path::Path,
    time::Duration,
};
use bincode;
use serde_json;
use hex;

/// Upload metadata to IPFS via Pump.fun
pub async fn upload_metadata_to_ipfs(token_config: &PumpfunMetadata) -> Result<String> {
    let client = Client::new();
    let mut form = Form::new()
        .text("name", token_config.name.clone())
        .text("symbol", token_config.symbol.clone())
        .text("description", token_config.description.clone())
        .text("showName", token_config.show_name.to_string());
    
    // Add optional fields if present
    if let Some(twitter) = &token_config.twitter {
        form = form.text("twitter", twitter.clone());
    }
    
    if let Some(telegram) = &token_config.telegram {
        form = form.text("telegram", telegram.clone());
    }
    
    if let Some(website) = &token_config.website {
        form = form.text("website", website.clone());
    }
    
    // Add image if present
    if let Some(image) = &token_config.image {
        if Path::new(image).exists() {
            let mut file = File::open(image)
                .map_err(|e| PumpfunError::Io(e.to_string()))?;
            
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)
                .map_err(|e| PumpfunError::Io(e.to_string()))?;
            
            let file_part = Part::bytes(buffer)
                .file_name(Path::new(image).file_name().unwrap().to_string_lossy().to_string());
            
            form = form.part("file", file_part);
        } else {
            // If not a local file, assume it's a URL
            form = form.text("image", image.clone());
        }
    }
    
    // Use the dedicated IPFS API URL from config
    let ipfs_url = &CONFIG.ipfs_api_url;
    debug!("Uploading metadata to IPFS via endpoint: {}", ipfs_url);

    // Try uploading to the configured IPFS endpoint
    let response = client.post(ipfs_url)
        .multipart(form) // Use the original form
        .timeout(Duration::from_secs(30)) // Increased timeout slightly
        .send()
        .await;

    match response {
        Ok(res) => {
            if res.status().is_success() {
                let ipfs_response: IpfsResponse = res.json().await
                    .map_err(|e| PumpfunError::Api(format!("Failed to parse IPFS response: {}", e)))?;

                info!("Metadata uploaded successfully. URI: {}", ipfs_response.metadata_uri);
                Ok(ipfs_response.metadata_uri)
            } else {
                let status = res.status();
                let error_text = res.text().await.unwrap_or_default();
                error!(
                    "IPFS endpoint failed with status: {}, error: {}",
                    status, error_text
                );
                Err(PumpfunError::Api(format!(
                    "IPFS upload failed. Status: {}, error: {}",
                    status, error_text
                )))
            }
        },
        Err(e) => {
            error!("IPFS endpoint request failed: {}", e);
            Err(PumpfunError::Api(format!(
                "IPFS upload request failed: {}", e
            )))
        }
    }
}

/// Check if a mint address already exists on Pump.fun
pub async fn check_pumpfun_address(rpc_client: &RpcClient, mint_pubkey: &Pubkey) -> Result<bool> {
    debug!("Checking if mint exists on Pump.fun: {}", mint_pubkey);
    
    // Simply check if the account exists on-chain
    match rpc_client.get_account(mint_pubkey) {
        Ok(_) => {
            debug!("Token mint already exists: {}", mint_pubkey);
            Ok(true)
        },
        Err(_) => {
            debug!("Token mint does not exist yet: {}", mint_pubkey);
            Ok(false)
        }
    }
}

/// Create a token on Pump.fun
pub async fn create_token(
    wallet_keypair: &Keypair,
    token_keypair: &Keypair,
    metadata_uri: &str,
    token_name: &str,
    token_symbol: &str,
    initial_buy_amount: f64,
) -> Result<VersionedTransaction> {
    let client = Client::new();
    
    // Create token request with direct JSON format
    let payload = serde_json::json!({
        "publicKey": wallet_keypair.pubkey().to_string(),
        "action": "create",
        "mint": token_keypair.pubkey().to_string(),
        "amount": initial_buy_amount * 0.9, // Reduce amount slightly to account for fees
        "denominatedInSol": "true",
        "slippage": 10,
        "priorityFee": 0.00005,
        "pool": "auto",
        "tokenMetadata": {
            "name": token_name,
            "symbol": token_symbol,
            "uri": metadata_uri
        }
    });
    
    debug!("Sending create token request to PumpPortal: {:?}", payload);
    
    // Simplified request - just setting Content-Type header
    let response = client.post(format!("{}/trade-local", CONFIG.pumpfun_portal_api_url))
        .header(CONTENT_TYPE, "application/json")
        .json(&payload)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| PumpfunError::Api(format!("Failed to send create token request: {}", e)))?;
        
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        return Err(PumpfunError::Api(format!(
            "Create token request failed with status {}: {}",
            status, error_text
        )));
    }
    
    // Get transaction data
    let tx_data = response.bytes().await
        .map_err(|e| PumpfunError::Api(format!("Failed to get transaction data: {}", e)))?;
    
    debug!("Raw transaction data (hex): {}", hex::encode(&tx_data));
    debug!("Received transaction data of size: {} bytes", tx_data.len());
        
    // Explicitly deserialize into the SDK's VersionedTransaction
    let tx: solana_sdk::transaction::VersionedTransaction = bincode::deserialize(&tx_data.to_vec())
        .map_err(|e| PumpfunError::Serialization(format!("Failed to deserialize transaction: {}", e)))?;
        
    debug!("Deserialized transaction: {:?}", tx);

    Ok(tx)
}

/// Buy tokens from Pump.fun
pub async fn buy_token(
    wallet_keypair: &Keypair,
    mint: &Pubkey,
    amount: f64,
    slippage: u8,
    priority_fee: f64,
) -> Result<VersionedTransaction> {
    let client = Client::new();
    
    // Create buy request with the exact format expected by the API
    let payload = serde_json::json!({
        "publicKey": wallet_keypair.pubkey().to_string(),
        "action": "buy",
        "mint": mint.to_string(),
        "amount": amount,
        "denominatedInSol": "true",
        "slippage": slippage,
        "priorityFee": priority_fee,
        "pool": "auto"
    });
    
    debug!("Sending buy request to PumpPortal: {:?}", payload);
    
    // Simplified request - just setting Content-Type header
    let response = client.post(format!("{}/trade-local", CONFIG.pumpfun_portal_api_url))
        .header(CONTENT_TYPE, "application/json")
        .json(&payload)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| PumpfunError::Api(format!("Failed to send buy token request: {}", e)))?;
        
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        return Err(PumpfunError::Api(format!(
            "Buy token request failed with status {}: {}",
            status, error_text
        )));
    }
    
    // Get transaction data
    let tx_data = response.bytes().await
        .map_err(|e| PumpfunError::Api(format!("Failed to get transaction data: {}", e)))?;
    
    debug!("Received transaction data of size: {} bytes", tx_data.len());
        
    // Deserialize transaction
    let tx = bincode::deserialize(&tx_data.to_vec())
        .map_err(|e| PumpfunError::Serialization(format!("Failed to deserialize transaction: {}", e)))?;
        
    Ok(tx)
}

/// Sell tokens from Pump.fun
///
/// The percentage parameter should be a value between 0-100, representing the percentage of
/// tokens to sell. For example, to sell 50% of tokens, pass 50.0.
/// 
/// If is_percentage is false, the amount parameter represents a direct token amount to sell.
pub async fn sell_token(
    wallet_keypair: &Keypair,
    mint: &Pubkey,
    amount: f64, // Amount can be percentage or token amount
    slippage: u8, // Slippage percentage (u8)
    priority_fee: f64, // Priority fee in SOL (f64)
) -> Result<VersionedTransaction> {
    let client = Client::new();

    // Validate RPC client and get balance
    let rpc_client = solana_client::nonblocking::rpc_client::RpcClient::new(
        CONFIG.solana_rpc_url.clone()
    );
    let token_balance = match crate::utils::balance::get_token_balance(&rpc_client, &wallet_keypair.pubkey(), mint).await {
        Ok(balance) => balance,
        Err(e) => {
            error!("Failed to get token balance: {}", e);
            return Err(PumpfunError::Api(format!("Failed to get token balance: {}", e)));
        }
    };

    if token_balance == 0 {
        info!("Token balance is zero for mint {}, cannot sell.", mint);
        return Err(PumpfunError::Api("Cannot sell token with zero balance".to_string()));
    }

    // Format amount string with proper percentage symbol
    let amount_string: String;

    // We're going to assume this is a percentage sale
    // The GUI will have already converted token amounts to percentages
    if amount >= 100.0 {
        // Format as "100.0%" string for 100% sells
        amount_string = "100.0%".to_string();
        info!("Selling 100% (sending \"{}\") for mint {}", amount_string, mint);
    } else if amount > 0.0 {
        // Format the percentage as a string like "95.5%"
        amount_string = format!("{:.1}%", amount); // Use one decimal place for consistency
        info!("Selling {} (sending \"{}\") for mint {}", amount_string, amount_string, mint);
    } else {
        info!("Sell percentage is zero or negative, nothing to sell for mint {}", mint);
        return Err(PumpfunError::Api("Sell percentage must be positive".to_string()));
    }

    // Construct the payload using the percentage string
    let payload = serde_json::json!({
        "publicKey": wallet_keypair.pubkey().to_string(),
        "action": "sell",
        "mint": mint.to_string(),
        "amount": amount_string,     // Send the percentage string
        "denominatedInSol": "false",       // Always use string "false"
        "slippage": slippage, // Send slippage percentage (u8)
        "priorityFee": priority_fee, // Send priority fee in SOL (f64)
        "pool": "auto"
    });

    debug!("Sell request payload: {:?}", payload);

    // Send request with the constructed payload
    let response = client.post(format!("{}/trade-local", CONFIG.pumpfun_portal_api_url))
        .header(CONTENT_TYPE, "application/json")
        .json(&payload)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| PumpfunError::Api(format!("Failed to send sell token request: {}", e)))?;
        
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        return Err(PumpfunError::Api(format!(
            "Sell token request failed with status {}: {}",
            status, error_text
        )));
    }
    
    // Get transaction data
    let tx_data = response.bytes().await
        .map_err(|e| PumpfunError::Api(format!("Failed to get transaction data: {}", e)))?;
    
    debug!("Received transaction data of size: {} bytes", tx_data.len());
    
    // Try to extract and log any readable parts of the response
    if let Ok(json_str) = String::from_utf8(tx_data.to_vec()) {
        if json_str.starts_with('{') {
            debug!("Response appears to be JSON: {}", json_str);
        }
    }
    
    // Deserialize transaction
    let tx = bincode::deserialize(&tx_data.to_vec())
        .map_err(|e| PumpfunError::Serialization(format!("Failed to deserialize transaction: {}", e)))?;
        
    Ok(tx)
}

/// Fetches a list of unsigned, base58-encoded transactions from the Pump.fun API 
/// for a bundled request (e.g., create + multiple buys).
///
/// # Arguments
///
/// * `transaction_args` - A vector of JSON values, where each value represents 
///                      the arguments for a single action (create, buy, sell) 
///                      as expected by the /trade-local endpoint.
///
/// # Returns
///
/// * `Result<Vec<String>>` - A list of base58-encoded transaction strings if successful.
pub async fn get_token_bundle_transactions(
    client: &Client,
    transaction_args: Vec<serde_json::Value>,
) -> Result<Vec<String>> {
    // Use the dedicated Pump Portal API URL from config
    let portal_api_url = &CONFIG.pumpfun_portal_api_url;
    let endpoint = format!("{}/trade-local", portal_api_url);

    debug!("Sending bundle request with {} actions to Pump.fun: {}", transaction_args.len(), endpoint);
    // debug!("Payload: {:?}", transaction_args); // Maybe too verbose

    // // Creates its own client internally - REMOVED
    // let client = Client::builder()
    //     .timeout(Duration::from_secs(60)) // Increased timeout
    //     .build()
    //     .map_err(|e| PumpfunError::Api(format!("Failed to build client for bundle request: {}", e)))?;

    // Use the passed-in client
    let response = client.post(&endpoint)
        .json(&transaction_args)
        .send()
        .await;
    match response {
        Ok(res) => {
            if res.status().is_success() {
                // Expecting a JSON array of strings
                let tx_strings: Vec<String> = res.json().await
                    .map_err(|e| PumpfunError::Api(format!("Failed to parse bundle transaction response: {}", e)))?;
                
                info!("Received {} encoded transactions from Pump.fun for the bundle.", tx_strings.len());
                Ok(tx_strings)
            } else {
                let status = res.status();
                let error_text = res.text().await.unwrap_or_default();
                error!(
                    "Pump.fun bundle request failed with status: {}, error: {}",
                    status, error_text
                );
                Err(PumpfunError::Api(format!(
                    "Pump.fun bundle request failed. Status: {}, error: {}",
                    status, error_text
                )))
            }
        },
        Err(e) => {
            error!("Pump.fun bundle request failed: {}", e);
            Err(PumpfunError::Api(format!(
                "Pump.fun bundle request failed: {}", e
            )))
        }
    }
}

pub async fn get_tx_from_account(client: &Client, mint_str: &str) -> Result<VersionedTransaction> {
    let url = format!("{}/tokens/{}/transactions/account", CONFIG.pumpfun_api_url, mint_str);
    let response = client.get(&url).send().await?;
    let response_text = response.text().await?;
    
    serde_json::from_str(&response_text)
        .map_err(|e| PumpfunError::Serialization(format!("Failed to deserialize transaction from account: {}", e)))
}

pub async fn get_tx_from_user(client: &Client, mint_str: &str) -> Result<VersionedTransaction> {
    let url = format!("{}/tokens/{}/transactions/user", CONFIG.pumpfun_api_url, mint_str);
    let response = client.get(&url).send().await?;
    let response_text = response.text().await?;

    serde_json::from_str(&response_text)
        .map_err(|e| PumpfunError::Serialization(format!("Failed to deserialize transaction from user: {}", e)))
}

pub async fn get_tx_from_metadata(client: &Client, mint_str: &str) -> Result<VersionedTransaction> {
    let url = format!("{}/tokens/{}/transactions/metadata", CONFIG.pumpfun_api_url, mint_str);
    let response = client.get(&url).send().await?;
    let response_text = response.text().await?;

    serde_json::from_str(&response_text)
        .map_err(|e| PumpfunError::Serialization(format!("Failed to deserialize transaction: {}", e)))
} 