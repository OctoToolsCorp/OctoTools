use crate::errors::{PumpfunError, Result};
use crate::models::wallet::{WalletBalance};
use bs58;
use log::{debug, error};
use rand::{distributions::Alphanumeric, Rng};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
};

const KEYS_FILENAME: &str = "keys.json";

/// Structure for storing wallet keypair info
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct WalletInfo {
    pub name: String,
    pub address: String,
    pub private_key: String,
}

/// Get keypair from base58 encoded private key
pub fn keypair_from_base58(base58_str: &str) -> Result<Keypair> {
    let bytes = bs58::decode(base58_str)
        .into_vec()
        .map_err(|e| PumpfunError::Wallet(format!("Invalid base58: {}", e)))?;

    if bytes.len() != 64 {
        return Err(PumpfunError::Wallet(format!(
            "Invalid private key length: {} bytes (expected 64)",
            bytes.len()
        )));
    }

    let keypair = Keypair::from_bytes(&bytes)
        .map_err(|e| PumpfunError::Wallet(format!("Invalid keypair: {}", e)))?;

    Ok(keypair)
}

/// Read keypair from a JSON file path
pub fn keypair_from_path<P: AsRef<Path>>(path: P) -> Result<Keypair> {
    let file = File::open(path.as_ref()).map_err(|e| {
        // Map to Io(String)
        PumpfunError::Io(format!("Failed to open keypair file {:?}: {}", path.as_ref(), e))
    })?;
    let reader = BufReader::new(file);
    let bytes: Vec<u8> = serde_json::from_reader(reader).map_err(|e| {
        PumpfunError::Serialization(format!("Failed to parse keypair file {:?}: {}", path.as_ref(), e))
    })?;
    Keypair::from_bytes(&bytes)
        .map_err(|e| PumpfunError::Wallet(format!("Invalid keypair data in file {:?}: {}", path.as_ref(), e)))
}

/// Get the keypair for the wallet specified in .env
pub fn get_wallet_keypair() -> Result<Keypair> {
    let private_key = std::env::var("WALLET_PRIVATE_KEY")
        .map_err(|_| PumpfunError::Wallet("Wallet private key not found in environment variables".to_string()))?;
    
    if private_key.is_empty() {
        return Err(PumpfunError::Wallet(
            "Wallet private key not found in environment variables".to_string(),
        ));
    }

    keypair_from_base58(&private_key)
}

/// Get the keypair for the dev wallet (same as main wallet for now)
pub fn get_dev_wallet() -> Result<Keypair> {
    get_wallet_keypair()
}

/// Get the keypair for the token (used for token creation)
pub fn get_token_keypair() -> Result<Keypair> {
    let private_key = std::env::var("TOKEN_PRIVATE_KEY")
        .map_err(|_| PumpfunError::Wallet("Token private key not found in environment variables".to_string()))?;
    
    if private_key.is_empty() {
        return Err(PumpfunError::Wallet(
            "Token private key not found in environment variables".to_string(),
        ));
    }

    keypair_from_base58(&private_key)
}

/// Get the path to the keys.json file
fn get_keys_path(keys_path_arg: Option<&str>) -> PathBuf {
    match keys_path_arg {
        Some(path_str) => PathBuf::from(path_str),
        None => {
            // Default to ./keys.json (current directory)
            let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            current_dir.join(KEYS_FILENAME)
            // Old logic for home directory:
            // let home_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            // home_dir.join(".pumpfun").join(KEYS_FILENAME)
        }
    }
}

/// Save wallet info to disk
fn save_wallets(wallets: &[WalletInfo], keys_path_arg: Option<&str>) -> Result<()> {
    let keys_path = get_keys_path(keys_path_arg);
    
    // Create parent directory if it doesn't exist (only if it's not the current dir)
    if let Some(parent) = keys_path.parent() {
        if parent != Path::new("") { 
             fs::create_dir_all(parent).map_err(|e| PumpfunError::Io(e.to_string()))?;
        }
    }
    
    let json = serde_json::to_string_pretty(wallets)?;
    fs::write(&keys_path, json).map_err(|e| PumpfunError::Io(e.to_string()))?;
    
    debug!("Saved {} wallets to {}", wallets.len(), keys_path.display());
    Ok(())
}

/// Load wallets from disk
pub fn load_zombie_wallets(keys_path_arg: Option<&str>) -> Result<Vec<WalletInfo>> {
    let keys_path = get_keys_path(keys_path_arg);
    
    if !keys_path.exists() {
        debug!("No existing keys.json found at {}", keys_path.display());
        return Ok(Vec::new());
    }
    
    let file = File::open(&keys_path).map_err(|e| PumpfunError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let wallets: Vec<WalletInfo> = serde_json::from_reader(reader)?;
    
    debug!("Loaded {} wallets from {}", wallets.len(), keys_path.display());
    Ok(wallets)
}

/// Create new zombie wallets
pub async fn create_zombie_wallets(count: usize, keys_path_arg: Option<&str>) -> Result<Vec<WalletInfo>> {
    let mut wallets = load_zombie_wallets(keys_path_arg)?;
    let mut new_wallets = Vec::with_capacity(count);
    
    for _ in 0..count {
        // Generate a random name for the wallet
        let name: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        
        // Create a new keypair
        let keypair = Keypair::new();
        let wallet_info = WalletInfo {
            name,
            address: keypair.pubkey().to_string(),
            private_key: bs58::encode(keypair.to_bytes()).into_string(),
        };
        
        new_wallets.push(wallet_info.clone());
        wallets.push(wallet_info);
    }
    
    // Save all wallets including the new ones
    save_wallets(&wallets, keys_path_arg)?;
    
    Ok(new_wallets)
}

/// Generate a new keypair, optionally with a vanity ending
pub async fn wallet_generate_key(vanity: Option<String>, keys_path_arg: Option<&str>) -> Result<(String, String)> {
    if let Some(vanity_text) = vanity {
        // Validate vanity text
        if vanity_text.len() > 5 {
            return Err(PumpfunError::Wallet(
                "Vanity text must be 5 characters or less".to_string(),
            ));
        }
        
        // Convert to lowercase for case-insensitive matching
        let vanity_lower = vanity_text.to_lowercase();
        
        // Search for a keypair with the specified ending
        let mut attempts = 0;
        const MAX_ATTEMPTS: usize = 10_000_000; // Reasonable limit to prevent infinite loop
        
        while attempts < MAX_ATTEMPTS {
            attempts += 1;
            
            let keypair = Keypair::new();
            let pubkey = keypair.pubkey().to_string();
            
            // Check if the public key ends with the vanity text (case insensitive)
            if pubkey.to_lowercase().ends_with(&vanity_lower) {
                debug!("Found vanity key after {} attempts", attempts);
                
                // Save to keys.json
                let name = format!("vanity_{}", vanity_text);
                let wallet_info = WalletInfo {
                    name,
                    address: pubkey.clone(),
                    private_key: bs58::encode(keypair.to_bytes()).into_string(),
                };
                
                let mut wallets = load_zombie_wallets(keys_path_arg)?;
                wallets.push(wallet_info);
                save_wallets(&wallets, keys_path_arg)?;
                
                return Ok((
                    pubkey,
                    bs58::encode(keypair.to_bytes()).into_string(),
                ));
            }
            
            // Show progress periodically
            if attempts % 100_000 == 0 {
                debug!("Searched {} keypairs for vanity ending", attempts);
            }
        }
        
        return Err(PumpfunError::Wallet(
            format!("Failed to find vanity key ending with '{}' after {} attempts", vanity_text, MAX_ATTEMPTS)
        ));
    } else {
        // Just generate a regular keypair
        let keypair = Keypair::new();
        let pubkey = keypair.pubkey().to_string();
        let private_key = bs58::encode(keypair.to_bytes()).into_string();
        
        // Save to keys.json
        let name = format!("generated_{}", 
            rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(6)
                .map(char::from)
                .collect::<String>());
        
        let wallet_info = WalletInfo {
            name,
            address: pubkey.clone(),
            private_key: private_key.clone(),
        };
        
        let mut wallets = load_zombie_wallets(keys_path_arg)?;
        wallets.push(wallet_info);
        save_wallets(&wallets, keys_path_arg)?;
        
        return Ok((pubkey, private_key));
    }
}

/// Get wallet keypair from a specific wallet info
pub fn get_keypair_from_wallet_info(wallet_info: &WalletInfo) -> Result<Keypair> {
    keypair_from_base58(&wallet_info.private_key)
}

/// Get the first zombie wallet or create one if none exist
pub async fn get_or_create_zombie_wallet() -> Result<WalletInfo> {
    let wallets = load_zombie_wallets(None)?;
    
    if wallets.is_empty() {
        debug!("No zombie wallets found, creating one");
        let new_wallets = create_zombie_wallets(1, None).await?;
        Ok(new_wallets[0].clone())
    } else {
        debug!("Using existing zombie wallet");
        Ok(wallets[0].clone())
    }
}

/// Get the parent zombie wallet (first wallet in the list)
pub fn get_parent_zombie() -> Result<WalletInfo> {
    let wallets = load_zombie_wallets(None)?;
    
    if wallets.is_empty() {
        return Err(PumpfunError::Wallet("No zombie wallets found".to_string()));
    }
    
    Ok(wallets[0].clone())
}

/// Mask a private key for display
pub fn mask_private_key(private_key: &str) -> String {
    if private_key.len() <= 8 {
        return "*".repeat(private_key.len());
    }
    
    let visible_prefix = &private_key[0..4];
    let visible_suffix = &private_key[private_key.len() - 4..];
    format!("{}...{}", visible_prefix, visible_suffix)
}

/// Gets the balance of a wallet in SOL
pub async fn get_wallet_balance(rpc_client: &RpcClient, wallet_pubkey: &Pubkey) -> Result<WalletBalance> {
    match rpc_client.get_balance(wallet_pubkey).await {
        Ok(balance) => {
            let sol_balance = balance as f64 / 1_000_000_000.0;
            Ok(WalletBalance { address: *wallet_pubkey, balance: sol_balance })
        }
        Err(e) => {
            error!("Failed to get wallet balance: {}", e);
            Err(PumpfunError::SolanaClient(solana_client::client_error::ClientError::from(e)))
        }
    }
}

/// Checks if a token account exists
pub async fn check_token_account_exists(rpc_client: &RpcClient, token_account: &Pubkey) -> Result<bool> {
    let account_info = rpc_client.get_account_with_commitment(token_account, CommitmentConfig::confirmed())
        .await
        .map_err(|e| PumpfunError::SolanaClient(e))?;
    
    Ok(account_info.value.is_some())
}

/// Makes sure the provided keypair is valid for use with Pump.fun
pub async fn check_pumpfun_address(rpc_client: &RpcClient, mint_pubkey: &Pubkey) -> Result<bool> {
    // Check if the address exists already
    let account_exists = check_token_account_exists(rpc_client, mint_pubkey).await?;
    
    if account_exists {
        return Err(PumpfunError::Token(
            "Token address already exists. Please select an unused address.".to_string()
        ));
    }
    
    Ok(true)
} 