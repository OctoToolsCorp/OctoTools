use crate::errors::{PumpfunError, Result};
use dotenv::dotenv;
use lazy_static::lazy_static;
use log::{debug, info, warn};
use serde::Deserialize;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

lazy_static! {
    pub static ref CONFIG: Arc<Config> = Arc::new(Config::load().unwrap_or_else(|e| {
        // Prefix unused `e`
        eprintln!("Failed to load configuration: {}. Using default settings.", e);
        Config::default()
    }));
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    // Solana configuration
    pub solana_rpc_url: String,
    pub solana_ws_url: String,
    pub commitment: String,

    // Pump.fun configuration
    pub pumpfun_api_url: String,
    pub pumpfun_portal_api_url: String,
    pub ipfs_api_url: String,

    // Jito configuration
    pub jito_mainnet_url: String,
    pub jito_block_engine_url: String,
    pub jito_auth_keypair_path: String,
    pub jito_json_rpc_url: String,

    // Token configuration constants
    pub default_decimals: u8,
    
    // Program IDs
    pub pumpfun_program_id: Pubkey,
    pub event_auth: Pubkey,
    pub fee_recipient: Pubkey,
    
    // Other constants
    pub instruction_per_tx: usize,
    pub max_retry: usize,
    pub lamports_per_sol: u64,
    pub max_priority_fee_lamports: u64,

    // Jito tip fields
    #[serde(default = "default_jito_tip_account")]
    pub jito_tip_account: Option<Pubkey>,
    #[serde(default = "default_jito_tip_lamports")]
    pub jito_tip_lamports: u64,

    // Pump.fun Fee Basis Points
    #[serde(default = "default_pump_fun_fee_basis_points")]
    pub pump_fun_fee_basis_points: u64,

    // Optional proxy list for zombie buys
    pub zombie_proxies: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenv().ok();
        info!("Loading configuration from environment variables.");

        let app_settings = AppSettings::load(); // Load app settings
        let config = Config {
            solana_rpc_url: app_settings.solana_rpc_url.clone(), // Use from AppSettings
            solana_ws_url: app_settings.solana_ws_url.clone(),
            commitment: app_settings.commitment.clone(),
            pumpfun_api_url: app_settings.pumpfun_api_url.clone(),
            pumpfun_portal_api_url: app_settings.pumpfun_portal_api_url.clone(),
            ipfs_api_url: app_settings.ipfs_api_url.clone(),
            jito_mainnet_url: app_settings.selected_jito_block_engine_url.clone(), // Assuming selected_jito_block_engine_url is the replacement for jito_mainnet_url
            jito_block_engine_url: app_settings.selected_jito_block_engine_url.clone(), // And also for jito_block_engine_url for now
            jito_auth_keypair_path: app_settings.jito_auth_keypair_path.clone(),
            jito_json_rpc_url: app_settings.jito_bundle_submission_url.clone(),
            default_decimals: app_settings.default_decimals,
            pumpfun_program_id: Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")?,
            event_auth: Pubkey::from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1")?,
            fee_recipient: Pubkey::from_str("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM")?,
            instruction_per_tx: app_settings.instruction_per_tx,
            max_retry: app_settings.max_retry,
            max_priority_fee_lamports: app_settings.max_priority_fee_lamports,
            lamports_per_sol: 1_000_000_000, // This is a constant, not typically in settings
            jito_tip_account: Pubkey::from_str(&app_settings.jito_tip_account).ok(),
            jito_tip_lamports: (app_settings.jito_tip_amount_sol * 1_000_000_000.0) as u64,
            pump_fun_fee_basis_points: app_settings.pump_fun_fee_basis_points,
            zombie_proxies: app_settings.zombie_proxies.clone(),
        };

        // Print the loaded RPC URL for debugging
        println!("*** Debug: Loaded RPC URL: {}", config.solana_rpc_url);

        debug!("Configuration loaded: {:?}", config);
        Ok(config)
    }

    pub fn get_commitment_config(&self) -> Result<CommitmentConfig> {
        match self.commitment.as_str() {
            "processed" => Ok(CommitmentConfig::processed()),
            "confirmed" => Ok(CommitmentConfig::confirmed()),
            "finalized" => Ok(CommitmentConfig::finalized()),
            _ => Err(PumpfunError::Config(format!("Invalid commitment level: {}", self.commitment))),
        }
    }
}

// Default implementation for Config
impl Default for Config {
    fn default() -> Self {
        Config {
            solana_rpc_url: "https://solana-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_API_KEY".to_string(),
            solana_ws_url: "wss://api.mainnet-beta.solana.com".to_string(),
            commitment: "confirmed".to_string(),
            pumpfun_api_url: "https://client-api-2-74b1891ee9f9.herokuapp.com".to_string(),
            pumpfun_portal_api_url: "https://pumpportal.fun/api".to_string(),
            ipfs_api_url: "https://pump.fun/api/ipfs".to_string(),
            jito_mainnet_url: "frankfurt.mainnet.block-engine.jito.wtf".to_string(),
            jito_block_engine_url: "https://dallas.testnet.block-engine.jito.wtf".to_string(),
            jito_auth_keypair_path: "".to_string(),
            jito_json_rpc_url: "https://dallas.testnet.block-engine.jito.wtf/api/v1/bundles".to_string(),
            default_decimals: 6,
            pumpfun_program_id: Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")
                .expect("Failed to parse PUMPFUN_PROGRAM_ID in default config"),
            event_auth: Pubkey::from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1")
                .expect("Failed to parse EVENT_AUTH in default config"),
            fee_recipient: Pubkey::from_str("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM")
                .expect("Failed to parse FEE_RECIPIENT in default config"),
            instruction_per_tx: 6,
            max_retry: 4,
            max_priority_fee_lamports: 100000,
            lamports_per_sol: 1_000_000_000,
            jito_tip_account: None,
            jito_tip_lamports: 10000,
            pump_fun_fee_basis_points: 100, // Added default
            zombie_proxies: None,
        }
    }
}

// Import AppSettings
use crate::models::settings::AppSettings;

/// Get the RPC URL from AppSettings (app_settings.json)
pub fn get_rpc_url() -> String {
    // Load AppSettings directly here to get the most current value
    // This avoids relying on the potentially outdated global static CONFIG for this specific value.
    let settings = AppSettings::load();
    settings.solana_rpc_url
}

/// Get the commitment config from the CONFIG
pub fn get_commitment_config() -> CommitmentConfig {
    CONFIG.get_commitment_config().unwrap_or_else(|_e| {
        warn!("Invalid commitment level in config, defaulting to confirmed.");
        CommitmentConfig::confirmed()
    })
}

/// Get the Jito Block Engine URL from the CONFIG
pub fn get_jito_block_engine_url() -> Result<String> {
    // Hardcoded Jito Block Engine URL for sending bundles
    Ok("https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles".to_string())
}

/// Get the configuration directory path
pub fn get_config_dir() -> PathBuf {
    let home_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home_dir.join(".pumpfun")
}

fn default_jito_tip_account() -> Option<Pubkey> {
    None
}

fn default_jito_tip_lamports() -> u64 {
    10000
}

// Added default for pump fun fee bps
fn default_pump_fun_fee_basis_points() -> u64 {
    100 // Default 1%
} 