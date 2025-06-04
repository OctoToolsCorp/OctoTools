pub mod jito;
pub mod pumpfun;
pub mod jupiter; // Add jupiter module
pub mod raydium;
pub mod raydium_launchpad; // Added new module
pub mod raydium_launchpad_layouts; // Added for Raydium Launchpad layout structs
// Add ipfs module if it's not already declared
// pub mod ipfs; // Removed for now

// Add this new function
use crate::errors::{Result, PumpfunError};
use reqwest::Client;
use serde_json::Value;
use log::{debug, error};

// Consider moving URL to config
const PUMP_TRADE_API_URL: &str = "https://pumpportal.fun/api/trade-local";

pub async fn call_pump_trade_local(payload: Value) -> Result<Vec<u8>> {
    let client = Client::new();
    debug!("Calling Pump.fun trade API: {}", PUMP_TRADE_API_URL);
    debug!("Payload: {}", serde_json::to_string(&payload).unwrap_or_default());

    let response = client.post(PUMP_TRADE_API_URL)
        .json(&payload) // Send payload as JSON body
        .send()
        .await
        .map_err(|e| PumpfunError::Api(format!("Failed to send request to pump API: {}", e)))?;

    if response.status().is_success() {
        let bytes = response.bytes().await
            .map_err(|e| PumpfunError::Api(format!("Failed to read pump API response bytes: {}", e)))?;
        debug!("Received {} bytes from pump API", bytes.len());
        Ok(bytes.to_vec())
    } else {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_else(|_| "Failed to read error body".to_string());
        error!("Pump API Error ({}): {}", status, error_text);
        Err(PumpfunError::Api(format!(
            "Pump API request failed with status {}: {}",
            status, error_text
        )))
    }
}

// Make sure existing functions are kept
pub use pumpfun::*;
pub use jupiter::*; // Re-export jupiter functions
pub use raydium::*;
pub use raydium_launchpad::*; // Re-export new module
pub use raydium_launchpad_layouts::*; // Re-export new layout module
// pub use jito::*; // Removed unused import
// pub use ipfs::*; // Removed for now