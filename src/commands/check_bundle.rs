use anyhow::{Result, anyhow, Context};
use log::{info, error, debug};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

const JITO_API_URL: &str = "https://mainnet.block-engine.jito.wtf/api/v1";

// --- Structs for getInflightBundleStatuses ---

#[derive(Serialize, Debug)]
struct JitoRpcRequest<T> {
    jsonrpc: String,
    id: u64,
    method: String,
    params: T,
}

#[derive(Deserialize, Debug)]
struct JitoInflightResponse {
    jsonrpc: String,
    result: Option<JitoInflightResultValue>,
    id: u64,
    // Jito might return an error object here too
    error: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct JitoInflightResultValue {
    context: JitoContext,
    value: Option<Vec<BundleInflightStatus>>,
}

#[derive(Deserialize, Debug)]
struct BundleInflightStatus {
    bundle_id: String,
    status: String, // "Invalid", "Pending", "Failed", "Landed"
    landed_slot: Option<u64>,
}

// --- Structs for getBundleStatuses ---

#[derive(Deserialize, Debug)]
struct JitoHistoricalResponse {
    jsonrpc: String,
    result: Option<JitoHistoricalResultValue>,
    id: u64,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct JitoHistoricalResultValue {
    context: JitoContext,
    value: Option<Vec<Option<BundleHistoricalStatus>>>, // Note the Option<...>
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")] // Match Jito's camelCase field names
struct BundleHistoricalStatus {
    bundle_id: String,
    transactions: Vec<String>,
    slot: u64,
    confirmation_status: String, // "processed", "confirmed", "finalized"
    err: Option<HashMap<String, Option<serde_json::Value>>>, // Capture potential error details
}

#[derive(Deserialize, Debug)]
struct JitoContext {
    slot: u64,
}

// --- Command Handler ---

pub async fn check_bundle_status(bundle_ids: Vec<String>, historical: bool) -> Result<()> {
    if bundle_ids.is_empty() {
        return Err(anyhow!("No bundle IDs provided."));
    }
    if bundle_ids.len() > 5 {
         // Although clap handles this, good to double-check
        return Err(anyhow!("Cannot check more than 5 bundle IDs per request."));
    }

    info!("Checking status for {} bundle(s)...", bundle_ids.len());
    let client = Client::new();

    if historical {
        call_get_bundle_statuses(&client, bundle_ids).await?;
    } else {
        call_get_inflight_bundle_statuses(&client, bundle_ids).await?;
    }

    Ok(())
}

// --- Helper Functions for API Calls ---

async fn call_get_inflight_bundle_statuses(client: &Client, bundle_ids: Vec<String>) -> Result<()> {
    let endpoint = format!("{}/{}", JITO_API_URL, "getInflightBundleStatuses");
    let method = "getInflightBundleStatuses";

    let request_payload = JitoRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: method.to_string(),
        // Jito API expects params as an array containing an array of strings
        params: json!([&bundle_ids]),
    };

    info!("Sending request to Jito: {}", method);
    debug!("Payload: {:?}", serde_json::to_string(&request_payload)?);

    let response = client
        .post(&endpoint)
        .json(&request_payload)
        .send()
        .await
        .context(format!("Failed to send request to Jito endpoint: {}", endpoint))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
         return Err(anyhow!("Jito API request failed with status {}: {}", status, body));
    }

    let parsed_response: JitoInflightResponse = response
        .json()
        .await
        .context("Failed to parse JSON response from Jito")?;

    debug!("Parsed Response: {:?}", parsed_response);

    if let Some(err_val) = parsed_response.error {
        error!("Jito API returned an error: {}", err_val);
        return Err(anyhow!("Jito API error: {}", err_val));
    }

    match parsed_response.result {
        Some(result) => {
            info!("Current Jito Slot Context: {}", result.context.slot);
            match result.value {
                Some(statuses) if !statuses.is_empty() => {
                    for status in statuses {
                         println!("----------------------------------------");
                         println!("Bundle ID: {}", status.bundle_id);
                         println!("Status:    {}", status.status);
                         if let Some(slot) = status.landed_slot {
                             println!("Landed Slot: {}", slot);
                         }
                         println!("----------------------------------------");
                    }
                }
                _ => {
                    // This case handles if result.value is None or an empty Vec
                    println!("No status found for the provided bundle ID(s) in the recent (inflight) history.");
                    println!("Try using the --historical flag if the bundle is older than ~5 minutes.");
                }
            }
        }
        None => {
            println!("Jito API did not return a result object for inflight status.");
        }
    }

    Ok(())
}

async fn call_get_bundle_statuses(client: &Client, bundle_ids: Vec<String>) -> Result<()> {
    let endpoint = format!("{}/{}", JITO_API_URL, "getBundleStatuses");
    let method = "getBundleStatuses";

    let request_payload = JitoRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: 1,
        method: method.to_string(),
        params: json!([&bundle_ids]),
    };

    info!("Sending request to Jito: {}", method);
    debug!("Payload: {:?}", serde_json::to_string(&request_payload)?);

    let response = client
        .post(&endpoint)
        .json(&request_payload)
        .send()
        .await
        .context(format!("Failed to send request to Jito endpoint: {}", endpoint))?;

     if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
         return Err(anyhow!("Jito API request failed with status {}: {}", status, body));
    }

    let parsed_response: JitoHistoricalResponse = response
        .json()
        .await
        .context("Failed to parse JSON response from Jito")?;

     debug!("Parsed Response: {:?}", parsed_response);

    if let Some(err_val) = parsed_response.error {
        error!("Jito API returned an error: {}", err_val);
        return Err(anyhow!("Jito API error: {}", err_val));
    }

    match parsed_response.result {
        Some(result) => {
            info!("Current Jito Slot Context: {}", result.context.slot);
             match result.value {
                 Some(maybe_statuses) if !maybe_statuses.is_empty() => {
                    for (i, maybe_status) in maybe_statuses.into_iter().enumerate() {
                        let requested_bundle_id = bundle_ids.get(i).map_or("N/A", |s| s.as_str());
                        println!("----------------------------------------");
                        println!("Requested Bundle ID: {}", requested_bundle_id);
                        match maybe_status {
                            Some(status) => {
                                println!("Status Found (Historical):");
                                println!("  Bundle ID: {}", status.bundle_id); // Should match requested
                                println!("  Landed Slot: {}", status.slot);
                                println!("  Confirmation: {}", status.confirmation_status);
                                println!("  Transactions:");
                                for tx_sig in status.transactions {
                                    println!("    - {}", tx_sig);
                                }
                                if let Some(err_map) = status.err {
                                     println!("  Error Details: {:?}", err_map);
                                } else {
                                     println!("  Error Details: None");
                                }
                            }
                            None => {
                                println!("Status: Not Found (Historical)");
                                println!("  The bundle was not found or did not land successfully.");
                            }
                        }
                         println!("----------------------------------------");
                    }
                }
                _ => {
                     println!("No historical status found for the provided bundle ID(s).");
                }
             }
        }
        None => {
            println!("Jito API did not return a result object for historical status.");
        }
    }


    Ok(())
}
