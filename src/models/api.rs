use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct IpfsResponse {
    #[serde(rename = "metadataUri")]
    pub metadata_uri: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JitoTipAccount {
    pub address: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JitoResponse {
    pub result: JitoResult,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JitoResult {
    #[serde(rename = "jsonrpc")]
    pub json_rpc: String,
    pub id: u64,
    #[serde(rename = "method")]
    pub method_name: String,
    pub value: Option<Vec<JitoBundleStatus>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JitoBundleStatus {
    pub bundle_id: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BundleRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: Vec<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PumpQuoteResponse {
    pub quote: PumpQuote,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PumpQuote {
    pub mint: String,
    pub bonding_curve: String,
    pub quote_type: String,
    pub in_amount: String,
    pub in_amount_ui: f64,
    pub in_token_address: String,
    pub out_amount: String,
    pub out_amount_ui: f64,
    pub out_token_address: String,
    pub meta: PumpQuoteMeta,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PumpQuoteMeta {
    pub is_completed: bool,
    pub out_decimals: u8,
    pub in_decimals: u8,
    pub total_supply: String,
    pub current_market_cap_in_sol: f64,
} 