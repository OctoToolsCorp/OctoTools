use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub name: String,
    pub symbol: String,
    pub description: String,
    pub image_url: Option<String>,
    pub initial_buy_amount: f64,
    pub decimals: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PumpfunMetadata {
    pub name: String,
    pub symbol: String,
    pub description: String,
    pub image: Option<String>,
    pub twitter: Option<String>,
    pub telegram: Option<String>,
    pub website: Option<String>,
    pub show_name: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PumpPortalRequest {
    pub publicKey: String,
    pub action: String, // "buy" or "sell" or "create"
    pub mint: String,
    pub amount: f64,
    pub denominatedInSol: String, // "true" or "false"
    pub slippage: u8,
    pub priorityFee: f64,
    pub pool: String, // "pump" or "raydium"
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "tokenMetadata")]
    pub token_metadata: Option<TokenMetadata>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TokenMetadata {
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenAccount {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
    pub decimals: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenDetails {
    pub mint: String,
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub current_price: f64,
    pub market_cap: f64,
    pub total_supply: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenBalance {
    pub wallet: String,
    pub amount: f64,
    pub value_in_sol: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenDistribution {
    pub wallet_name: String,
    pub wallet_address: String,
    pub token_amount: f64,
    pub token_percent: f64,
    pub sol_amount: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchResults {
    pub token_mint: String,
    pub status: String,
    pub wallet_results: Vec<WalletBuyResult>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletBuyResult {
    pub wallet: String,
    pub success: bool,
    pub method: String,
    pub txid: Option<String>,
    pub attempts: Option<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SwapPayload {
    pub action: String, // "create", "buy", "sell"
    #[serde(rename = "publicKey")] // Keep JSON name but use snake_case internally
    pub public_key: String,
    pub mint: Option<String>,
    #[serde(rename = "denominatedInSol")] // Keep JSON name
    pub denominated_in_sol: Option<String>, 
    pub amount: Option<f64>,
    pub slippage: Option<f64>,
    #[serde(rename = "priorityFee")] // Keep JSON name
    pub priority_fee: Option<f64>,
    pub pool: Option<String>,
    #[serde(rename = "tokenMetadata")] // Keep JSON name
    pub token_metadata: Option<TokenMetadata>,
} 
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct TokenPnL {
    pub mint: String, // Mint address of the token
    pub initial_investment_sol: f64,
    pub total_sol_received_from_sales: f64,
    pub total_usdc_received_from_sales: f64, // If sales can be in USDC
    pub total_fees_paid_sol: f64,
    pub cost_of_buybacks_sol: f64,         // Cost of any tokens repurchased
    pub current_held_token_amount: f64,    // Decimal-adjusted balance of tokens currently held
    pub current_token_price_in_sol: f64,
    pub current_token_price_in_usdc: f64,
    pub current_held_value_in_sol: f64,    // current_held_token_amount * current_token_price_in_sol
    pub current_held_value_in_usdc: f64,   // current_held_token_amount * current_token_price_in_usdc
    pub realized_pnl_sol: f64,
    pub realized_pnl_usdc: f64,
    pub unrealized_pnl_sol: f64,
    pub unrealized_pnl_usdc: f64,
    pub total_pnl_sol: f64,
    pub total_pnl_usdc: f64,
    pub last_updated_timestamp: i64,     // Unix timestamp of the last update
}