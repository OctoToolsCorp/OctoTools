use serde::{Deserialize, Serialize};
use std::{
    fs::{File},
    io::{Read, Write},
    path::{PathBuf},
};
use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signer::{Signer, keypair::Keypair}
};
use std::str::FromStr;
use bs58;

use crate::models::settings; // Added self import for constant usage

// Define the available Jito endpoints
pub const JITO_BLOCK_ENGINE_ENDPOINTS: [&str; 6] = [
    "https://amsterdam.mainnet.block-engine.jito.wtf",
    "https://frankfurt.mainnet.block-engine.jito.wtf",
    "https://london.mainnet.block-engine.jito.wtf",
    "https://ny.mainnet.block-engine.jito.wtf",
    "https://tokyo.mainnet.block-engine.jito.wtf",
    "https://slc.mainnet.block-engine.jito.wtf",
];

// Define the path for the settings file (relative to executable or config dir)
const SETTINGS_FILENAME: &str = "app_settings.json";

fn get_settings_path() -> PathBuf {
    // For simplicity, using the current directory. Consider dirs crate later.
    PathBuf::from(SETTINGS_FILENAME)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AppSettings {
    // Solana configuration
    pub solana_rpc_url: String,
    pub solana_ws_url: String,
    pub commitment: String,

    // Pump.fun configuration
    pub pumpfun_api_url: String,
    pub pumpfun_portal_api_url: String,
    pub ipfs_api_url: String,

    // Jito configuration (Updated)
    pub selected_jito_block_engine_url: String, // User selected endpoint
    pub jito_auth_keypair_path: String, // Added: Path to Jito auth keypair
    pub jito_bundle_submission_url: String, // Added: Jito bundle submission URL
    pub jito_tip_account: String, // General tip account for other Jito features (e.g., launch)
    pub jito_tip_amount_sol: f64, // General tip amount for other Jito features

    // Volume Bot Jito sendTransaction Specific Settings (Updated)
    pub use_jito_for_volume_bot: bool,
    pub jito_tip_account_pk_str_volume_bot: String, // Jito tip account to use for volume bot tips
    pub jito_tip_lamports_volume_bot: u64,          // Tip amount in lamports for volume bot transactions sent via Jito

    // Wallet/Keys Path
    pub keys_path: String,

    // Default Behavior
    pub parent_wallet_private_key: String,
    pub dev_wallet_private_key: String,

    // Default Behavior (cont.)
    pub default_slippage_percent: f64,
    pub default_priority_fee_sol: f64,
    #[serde(default = "crate::models::settings::default_main_tx_priority_fee")]
    pub default_main_tx_priority_fee_micro_lamports: u64,
    #[serde(default = "crate::models::settings::default_jito_tip_sol")]
    pub default_jito_actual_tip_sol: f64,
    pub max_priority_fee_lamports: u64, // Added: Max priority fee in lamports
    pub pump_fun_fee_basis_points: u64,
    pub default_decimals: u8, // Added: Default token decimals
    pub instruction_per_tx: usize, // Added: Default instructions per transaction
    pub max_retry: usize, // Added: Default max retries for transactions

    // Mix Wallets specific
    #[serde(default = "default_mix_sol_per_new_wallet")]
    pub mix_sol_per_new_wallet: f64,

    // Optional proxy list for zombie buys
    pub zombie_proxies: Option<String>,
}

fn default_mix_sol_per_new_wallet() -> f64 {
    0.001 // Default SOL amount to send to each new mixed wallet
}

// Private functions to provide default values for serde
fn default_main_tx_priority_fee() -> u64 {
    500_000 // Default value from AppSettings::default()
}

fn default_jito_tip_sol() -> f64 {
    0.001 // Default value from AppSettings::default()
}
impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            solana_rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            solana_ws_url: "wss://api.mainnet-beta.solana.com".to_string(),
            commitment: "confirmed".to_string(),
            pumpfun_api_url: "https://client-api-2-74b1891ee9f9.herokuapp.com".to_string(),
            pumpfun_portal_api_url: "https://pumpportal.fun/api".to_string(),
            ipfs_api_url: "https://pump.fun/api/ipfs".to_string(),
            selected_jito_block_engine_url: settings::JITO_BLOCK_ENGINE_ENDPOINTS[3].to_string(), // Default to NY
            jito_auth_keypair_path: "".to_string(), // Added default
            jito_bundle_submission_url: "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles".to_string(), // Added default
            jito_tip_account: "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5".to_string(), // General tip account
            jito_tip_amount_sol: 0.0001, // General tip amount

            use_jito_for_volume_bot: false,
            jito_tip_account_pk_str_volume_bot: "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5".to_string(), // Default Jito tip account
            jito_tip_lamports_volume_bot: 10000, // Default tip for volume bot Jito transactions (0.00001 SOL)

            keys_path: "./keys.json".to_string(),
            default_slippage_percent: 10.0,
            parent_wallet_private_key: "".to_string(),
            dev_wallet_private_key: "".to_string(),
            default_priority_fee_sol: 0.0001,
            default_main_tx_priority_fee_micro_lamports: 500_000,
            default_jito_actual_tip_sol: 0.001,
            max_priority_fee_lamports: 100_000, // Added default
            pump_fun_fee_basis_points: 100,
            default_decimals: 6, // Added default
            instruction_per_tx: 6, // Added default
            max_retry: 4, // Added default
            mix_sol_per_new_wallet: default_mix_sol_per_new_wallet(), // Added default
            zombie_proxies: None,
        }
    }
}

impl AppSettings {
    /// Loads settings from the JSON file, or returns default if it doesn't exist or fails.
    pub fn load() -> Self {
        let path = get_settings_path();
        info!("Attempting to load settings from: {}", path.display());
        match File::open(&path) {
            Ok(mut file) => {
                let mut contents = String::new();
                if let Err(e) = file.read_to_string(&mut contents) {
                    warn!("Failed to read settings file '{}': {}. Using default settings.", path.display(), e);
                    return AppSettings::default();
                }
                match serde_json::from_str(&contents) {
                    Ok(settings) => {
                        info!("Successfully loaded settings from {}", path.display());
                        settings
                    }
                    Err(e) => {
                        warn!("Failed to parse settings file '{}': {}. Using default settings.", path.display(), e);
                        AppSettings::default()
                    }
                }
            }
            Err(_) => {
                info!("Settings file '{}' not found. Using default settings.", path.display());
                AppSettings::default()
            }
        }
    }

    /// Saves the current settings to the JSON file.
    pub fn save(&self) -> Result<()> {
        let path = get_settings_path();
        info!("Attempting to save settings to: {}", path.display());
        let json_string = serde_json::to_string_pretty(self)
            .context("Failed to serialize settings to JSON")?;

        let mut file = File::create(&path)
            .with_context(|| format!("Failed to create or open settings file for writing: {}", path.display()))?;

        file.write_all(json_string.as_bytes())
            .with_context(|| format!("Failed to write settings to file: {}", path.display()))?;

        info!("Successfully saved settings to {}", path.display());
        Ok(())
    }

    // Helper to get commitment config
    pub fn get_commitment_config(&self) -> Result<CommitmentConfig> {
         match self.commitment.to_lowercase().as_str() {
            "processed" => Ok(CommitmentConfig::processed()),
            "confirmed" => Ok(CommitmentConfig::confirmed()),
            "finalized" => Ok(CommitmentConfig::finalized()),
            _ => Err(anyhow!("Invalid commitment level: {}", self.commitment)),
        }
    }

     // Helper to get Jito tip account Pubkey
    pub fn get_jito_tip_account_pubkey(&self) -> Result<Pubkey> {
        Pubkey::from_str(&self.jito_tip_account)
            .map_err(|e| anyhow!("Invalid Jito Tip Account Pubkey '{}': {}", self.jito_tip_account, e))
    }

    // Helper to get Jito tip lamports
    pub fn get_jito_tip_lamports(&self) -> u64 {
        (self.jito_tip_amount_sol * 1_000_000_000.0) as u64
    }

    // Helper to get default priority fee lamports
    pub fn get_default_priority_fee_lamports(&self) -> u64 {
        (self.default_priority_fee_sol * 1_000_000_000.0) as u64
    }

    // Helper to safely get pubkey string from a private key string
    pub fn get_pubkey_from_privkey_str(pk_str: &str) -> Option<String> {
        if pk_str.is_empty() {
            return None;
        }
        bs58::decode(pk_str)
            .into_vec()
            .ok()
            .and_then(|bytes| Keypair::from_bytes(&bytes).ok())
            .map(|kp| kp.pubkey().to_string())
    }
}