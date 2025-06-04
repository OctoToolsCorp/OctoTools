use crate::errors::{Result, PumpfunError};
use crate::models::wallet::WalletKeys;
use crate::models::settings::AppSettings; // Import AppSettings
use log::{info, warn};
use solana_sdk::signature::{Keypair, SeedDerivable}; // Added SeedDerivable
// use std::env; // No longer needed for MINTER env var
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;
use std::path::PathBuf;
// use std::str::FromStr; // Unused
use serde::Deserialize;
use bs58;

// Publicly export necessary functions
// Comment out missing submodules
// pub mod generate;
// pub mod check;
// pub mod gather;

// pub use generate::wallet_generate_key;
// pub use check::check_wallets;
// pub use gather::gather_sol;

/// Gets the primary wallet keypair (minter/parent) from application settings.
pub fn get_wallet_keypair() -> Result<Keypair> {
    let app_settings = AppSettings::load(); // Load settings from app_settings.json
    let private_key_base58 = &app_settings.parent_wallet_private_key;

    if !private_key_base58.is_empty() {
        info!("Loading parent/minter keypair from application settings (parent_wallet_private_key)...");
        let decoded_bytes = bs58::decode(private_key_base58)
            .into_vec()
            .map_err(|e| PumpfunError::Wallet(format!("Failed to decode base58 private key from settings: {}", e)))?;
        
        if decoded_bytes.len() == 32 {
            // If it's 32 bytes, assume it's a seed / secret key
            let seed_array: [u8; 32] = match decoded_bytes.try_into() {
                Ok(arr) => arr,
                Err(_original_vec) => { // The error type is the Vec itself if lengths don't match
                    return Err(PumpfunError::Wallet(
                        "Decoded private key (seed) from settings is not 32 bytes long after check."
                            .to_string(),
                    ));
                }
            };
            Keypair::from_seed(&seed_array)
                .map_err(|e| PumpfunError::Wallet(format!("Failed to create keypair from 32-byte seed from settings: {}", e)))
        } else if decoded_bytes.len() == 64 {
            // If it's 64 bytes, assume it's a full keypair byte array [secret_key, public_key]
            Keypair::from_bytes(&decoded_bytes)
                .map_err(|e| PumpfunError::Wallet(format!("Failed to create keypair from 64-byte array from settings: {}", e)))
        } else {
            Err(PumpfunError::Wallet(format!(
                "Decoded private key from settings has unexpected length: {}. Expected 32 or 64 bytes.",
                decoded_bytes.len()
            )))
        }
    } else {
        warn!("Parent/minter private key not found or empty in application settings (app_settings.json).");
        warn!("Falling back to keys.json (first entry) as parent_wallet_private_key was empty in settings.");
        warn!("It is STRONGLY recommended to set 'parent_wallet_private_key' in app_settings.json for consistent behavior.");
        
        let keys_path_str = if !app_settings.keys_path.is_empty() {
            app_settings.keys_path.clone()
        } else {
            "./keys.json".to_string() // Default if keys_path in settings is also empty
        };
        let keys_path = get_keys_path(Some(&keys_path_str))?;
        
        let keys = load_keys_from_file(&keys_path)?;
        if keys.wallets.is_empty() {
            return Err(PumpfunError::Wallet(format!(
                "No parent_wallet_private_key in settings and no wallets found in {}",
                keys_path.display()
            )));
        }
        info!("Using first wallet from {} as fallback parent/minter.", keys_path.display());
        let decoded_bytes_fallback = bs58::decode(&keys.wallets[0].private_key)
            .into_vec()
            .map_err(|e| PumpfunError::Wallet(format!("Failed to decode base58 private key from keys.json (fallback): {}", e)))?;

        if decoded_bytes_fallback.len() == 32 {
            let seed_array_fallback: [u8; 32] = match decoded_bytes_fallback.try_into() {
                Ok(arr) => arr,
                Err(_original_vec) => { // The error type is the Vec itself if lengths don't match
                    return Err(PumpfunError::Wallet(
                        "Decoded private key (seed) from keys.json fallback is not 32 bytes long after check."
                            .to_string(),
                    ));
                }
            };
            Keypair::from_seed(&seed_array_fallback)
                .map_err(|e| PumpfunError::Wallet(format!("Failed to create keypair from 32-byte seed from keys.json (fallback): {}", e)))
        } else if decoded_bytes_fallback.len() == 64 {
            Keypair::from_bytes(&decoded_bytes_fallback)
                .map_err(|e| PumpfunError::Wallet(format!("Failed to create keypair from 64-byte array from keys.json (fallback): {}", e)))
        } else {
            Err(PumpfunError::Wallet(format!(
                "Decoded private key from keys.json (fallback) has unexpected length: {}. Expected 32 or 64 bytes.",
                decoded_bytes_fallback.len()
            )))
        }
    }
}

// Struct for the direct array format: [...] with fields "address", "key"
#[derive(Deserialize, Debug, Clone)] // Add Clone
struct SimWalletJson {
    name: String,
    address: String,
    key: String,
}

// Struct for the inner objects in the nested format: {"wallets": [...]} with fields "public_key", "private_key"
#[derive(Deserialize, Debug, Clone)] // Add Clone
struct KeysWalletJson {
    name: String,
    public_key: String,
    private_key: String,
}

// Struct to deserialize the top-level nested JSON object {"wallets": [...]}
#[derive(Deserialize, Debug)]
struct TopLevelKeysJson {
    wallets: Vec<KeysWalletJson>, // Use KeysWalletJson here
}
// Removed TopLevelWalletsJson struct

/// Loads zombie wallets from the specified file.
/// Assumes the parent/creator wallet is loaded from ENV.
/// Reads the JSON structure which is a direct array:
/// [ { "public_key": "...", "private_key": "..." }, ... ]
pub fn load_zombie_wallets_from_file(keys_path_arg: Option<&str>) -> Result<Vec<Keypair>> {
    let keys_path = get_keys_path(keys_path_arg)?;
    info!("Loading zombie wallets from file: {}", keys_path.display());
// If the filename starts with "volume_wallets_", don't load any zombies from it for the check list.
    // This is to prevent them from appearing as generic "Zombie X" wallets in the main check interface,
    // as they are likely managed or displayed elsewhere (e.g., Volume Bot tab).
    if let Some(filename_osstr) = keys_path.file_name() {
        if let Some(filename_str) = filename_osstr.to_str() {
            if filename_str.to_lowercase().starts_with("volume_wallets_") {
                warn!("Skipping zombie wallet loading from '{}' for the 'Check Wallets' display because it appears to be a volume-specific wallet file.", keys_path.display());
                return Ok(Vec::new());
            }
        }
    }

    let file = File::open(&keys_path)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to open keys file '{}': {}", keys_path.display(), e)))?;
    let _reader = BufReader::new(file); // Prefixed with underscore as it's not directly used after file_content read

    // Attempt to deserialize directly into a Vec<ZombieWalletJson> (format: [...])
    // Need to read the file content into a string first to allow re-reading if the first parse fails
    let file_content = std::fs::read_to_string(&keys_path)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to read keys file '{}': {}", keys_path.display(), e)))?;

    // Enum to hold the parsed wallet data, regardless of format
    #[derive(Clone)] // Add Clone
    enum ParsedWalletData {
        Sim(SimWalletJson),
        Keys(KeysWalletJson),
    }

    let parsed_wallets: Vec<ParsedWalletData> = match serde_json::from_str::<Vec<SimWalletJson>>(&file_content) {
        Ok(sim_wallets_vec) => {
            info!("Successfully parsed keys file '{}' as direct array format '[...]'.", keys_path.display());
            // Convert SimWalletJson to ParsedWalletData::Sim
            sim_wallets_vec.into_iter().map(ParsedWalletData::Sim).collect()
        }
        Err(e) => {
            // If direct array parsing failed, try parsing as {"wallets": [...]}
            warn!("Failed to parse keys file '{}' as direct array: {}. Trying '{{\"wallets\": [...]}}' format...", keys_path.display(), e);
            match serde_json::from_str::<TopLevelKeysJson>(&file_content) { // Use TopLevelKeysJson
                Ok(top_level) => {
                    info!("Successfully parsed keys file '{}' as '{{\"wallets\": [...]}}' format.", keys_path.display());
                    // Convert KeysWalletJson to ParsedWalletData::Keys
                    top_level.wallets.into_iter().map(ParsedWalletData::Keys).collect()
                }
                Err(e2) => {
                    // If both parsing attempts fail, return a combined error
                    return Err(PumpfunError::Wallet(format!(
                        "Failed to parse keys file '{}'. Tried direct array ('[...]') and nested '{{\"wallets\": [...]}}' formats. Error 1: {}. Error 2: {}",
                        keys_path.display(), e, e2
                    )));
                }
            }
        }
    };

    // Filter out wallets whose name starts with "Volume" (case-insensitive)
    let parsed_wallets: Vec<ParsedWalletData> = parsed_wallets.into_iter().filter(|wallet_data| {
        let name = match wallet_data {
            ParsedWalletData::Sim(sim_info) => &sim_info.name,
            ParsedWalletData::Keys(keys_info) => &keys_info.name,
        };
        !name.to_lowercase().starts_with("volume")
    }).collect();

    if parsed_wallets.is_empty() {
        warn!("No zombie wallets (after filtering 'Volume' wallets) found in '{}'.", keys_path.display());
        return Ok(Vec::new());
    }

    // Convert each ParsedWalletData entry into a Keypair
    parsed_wallets.iter().map(|parsed_info| {
        // Extract the correct fields based on the enum variant
        let (priv_key_str, pub_key_str_for_error) = match parsed_info {
            ParsedWalletData::Sim(sim_info) => (&sim_info.key, &sim_info.address),
            ParsedWalletData::Keys(keys_info) => (&keys_info.private_key, &keys_info.public_key),
        };

        // Decode the base58 private key string
        let secret_key_bytes = bs58::decode(priv_key_str)
            .into_vec()
            .map_err(|e| PumpfunError::Wallet(format!(
                "Failed to decode base58 private key for pubkey {}: {}",
                pub_key_str_for_error, e
            )))?;

        // Create Keypair from decoded bytes
        Keypair::from_bytes(&secret_key_bytes)
             .map_err(|e| PumpfunError::Wallet(format!(
                "Failed to create keypair from bytes for pubkey {}: {}",
                pub_key_str_for_error, e
            )))
    }).collect::<Result<Vec<Keypair>>>()
}

/// Helper to load all keys (including creator) without skipping.
pub fn load_keys_from_file(keys_path: &Path) -> Result<WalletKeys> {
    let file = File::open(keys_path)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to open keys file '{}': {}", keys_path.display(), e)))?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to parse keys file '{}': {}", keys_path.display(), e)))
}

/// Helper to determine the keys.json path.
fn get_keys_path(keys_path_arg: Option<&str>) -> Result<std::path::PathBuf> {
    Ok(keys_path_arg.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("./keys.json")))
}

// Helper to save keys
fn save_keys_to_file(keys: &WalletKeys, keys_path: &Path) -> Result<()> {
    let json_data = serde_json::to_string_pretty(keys)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to serialize keys: {}", e)))?;
    let mut file = File::create(keys_path)
        .map_err(|e| PumpfunError::Wallet(format!("Failed to create/open keys file for writing '{}': {}", keys_path.display(), e)))?;
    file.write_all(json_data.as_bytes())
        .map_err(|e| PumpfunError::Wallet(format!("Failed to write keys to file '{}': {}", keys_path.display(), e)))?;
    Ok(())
} 