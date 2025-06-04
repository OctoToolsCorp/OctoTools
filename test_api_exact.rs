use reqwest::{Client, header};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up the client
    let client = Client::new();
    
    // This is the exact format from the Python example
    let payload = json!({
        "publicKey": "2nX5kG96kLhvPA74VY497bYyVyAhYdWZSq9xk2e3ourT", // Your actual public key
        "action": "sell",
        "mint": "57BTcUAH7KuaZVVSGuz8XJeXYacUozc6KB92TcNkpump",
        "amount": 100,
        "denominatedInSol": "false",
        "slippage": 10,
        "priorityFee": 0.0005,
        "pool": "pump"
    });
    
    println!("Request payload: {}", serde_json::to_string_pretty(&payload)?);
    
    // Send request
    let response = client.post("https://pumpportal.fun/api/trade-local")
        .header(header::CONTENT_TYPE, "application/json")
        .json(&payload)
        .send()
        .await?;
    
    println!("Status: {}", response.status());
    println!("Response: {}", response.text().await?);
    
    Ok(())
} 