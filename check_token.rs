use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use reqwest::Client;
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();
    
    let mint_address = "57BTcUAH7KuaZVVSGuz8XJeXYacUozc6KB92TcNkpump";
    println!("Checking token: {}", mint_address);
    
    // Check on-chain info
    let rpc_url = env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let rpc_client = RpcClient::new(&rpc_url);
    
    let mint_pubkey = Pubkey::from_str(mint_address)?;
    
    // Check if account exists
    match rpc_client.get_account(&mint_pubkey) {
        Ok(account) => {
            println!("Account exists on-chain: {}", account.lamports);
        },
        Err(e) => {
            println!("Account not found on-chain: {}", e);
        }
    }
    
    // Try to get token info from Pump.fun API
    let client = Client::new();
    
    // Try a GET request to see if the token exists
    println!("\nChecking token on Pump.fun API...");
    let response = client.get(format!("https://pump.fun/api/tokens/{}", mint_address))
        .send()
        .await?;
    
    println!("Status: {}", response.status());
    
    if response.status().is_success() {
        let token_info = response.text().await?;
        println!("Token info: {}", token_info);
    } else {
        println!("Token not found on Pump.fun API: {}", response.text().await?);
    }
    
    // Try a direct check with Pump.fun Portal API
    println!("\nChecking token on PumpPortal API...");
    let response = client.get(format!("https://pumpportal.fun/api/tokens/{}", mint_address))
        .send()
        .await?;
    
    println!("Status: {}", response.status());
    
    if response.status().is_success() {
        let token_info = response.text().await?;
        println!("Token info: {}", token_info);
    } else {
        println!("Token not found on PumpPortal API: {}", response.text().await?);
    }
    
    Ok(())
} 