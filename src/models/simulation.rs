use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationConfig {
    pub wallet_count: usize,
    pub dev_percent: f64,
    pub min_percent: f64,
    pub max_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationResults {
    pub token_name: String,
    pub token_symbol: String,
    pub token_decimals: u8,
    pub token_supply: u64,
    pub token_mint_address: String,
    pub initial_price: f64,
    pub initial_sol_reserve: f64,
    pub initial_token_reserve: u64,
    pub dev_fee_percent: f64,
    pub wallet_count: usize,
    pub min_buy_percent: f64,
    pub max_buy_percent: f64,
    pub sol_amounts: Vec<f64>,
    pub token_amounts: Vec<f64>,
    pub distribution: Vec<DistributionPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributionPlan {
    pub wallet: String,
    pub token_amount: String,
    pub token_percent: String,
    pub sol_amount: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationInfo {
    pub wallet: String,
    pub token_amount: f64,
    pub sol_amount: f64,
} 