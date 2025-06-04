use anyhow::{Context, Result};
use chrono::Local;
use serde::Serialize;
use solana_sdk::{
    // pubkey::Pubkey, // Unused import
    signer::{keypair::Keypair, Signer},
};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use bs58;

#[derive(Serialize, Debug)]
struct WalletEntry {
    name: String,
    public_key: String,
    private_key: String,
}

#[derive(Serialize, Debug)]
struct KeysFile {
    wallets: Vec<WalletEntry>,
}

/// Generates a new Solana keypair and saves it to a timestamped JSON file.
///
/// The file format matches the structure provided by the user:
/// ```json
/// {
///   "wallets": [
///     {
///       "name": "generated_YYYYMMDD_HHMMSS",
///       "public_key": "...",
///       "private_key": "..."
///     }
///   ]
/// }
/// ```
///
/// # Arguments
/// * `output_dir` - The directory where the keys file should be saved.
///
/// * `count` - The number of keypairs to generate in the file.
/// # Returns
/// * `Ok(PathBuf)` containing the path to the newly created keys file.
/// * `Err(anyhow::Error)` if key generation or file saving fails.
pub fn generate_and_save_keypairs(output_dir: &Path, count: usize) -> Result<PathBuf> {
    // Ensure count is at least 1
    let count = if count == 0 { 1 } else { count };

    // 2. Create timestamp and filename
    let now = Local::now();
    let timestamp_str = now.format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("keys_{}.json", timestamp_str);
    let file_path = output_dir.join(&filename);

    // 3. Generate multiple wallet entries
    let mut wallet_entries = Vec::with_capacity(count);
    for i in 0..count {
        let keypair = Keypair::new();
        let public_key = keypair.pubkey().to_string();
        let private_key_bytes = keypair.to_bytes();
        let private_key_bs58 = bs58::encode(private_key_bytes).into_string();

        let wallet_name = format!("zombie{}", i + 1); // Use zombieN naming
        let wallet_entry = WalletEntry {
            name: wallet_name,
            public_key,
            private_key: private_key_bs58,
        };
        wallet_entries.push(wallet_entry);
    }
    let keys_data = KeysFile {
        wallets: wallet_entries,
    };

    // 4. Serialize to JSON
    let json_string = serde_json::to_string_pretty(&keys_data)
        .context("Failed to serialize key data to JSON")?;

    // 5. Save to file
    let mut file = File::create(&file_path)
        .with_context(|| format!("Failed to create keys file: {}", file_path.display()))?;
    file.write_all(json_string.as_bytes())
        .with_context(|| format!("Failed to write keys to file: {}", file_path.display()))?;

    log::info!("Successfully generated and saved keys to {}", file_path.display());
    Ok(file_path)
}

#[derive(Serialize, Debug)]
struct DevWalletFile {
    public_key: String,
    private_key: String,
}

/// Generates a new Solana keypair, saves it to a timestamped JSON file,
/// and returns the public and private keys.
pub fn generate_and_save_dev_wallet(output_dir: &Path) -> Result<(String, String), anyhow::Error> {
    let keypair = Keypair::new();
    let public_key_bs58 = keypair.pubkey().to_string();
    let private_key_bs58 = bs58::encode(keypair.to_bytes()).into_string();

    let now = Local::now();
    let timestamp_str = now.format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("dev_wallet_{}.json", timestamp_str);
    let file_path = output_dir.join(&filename);

    let dev_wallet_data = DevWalletFile {
        public_key: public_key_bs58.clone(),
        private_key: private_key_bs58.clone(),
    };

    let json_string = serde_json::to_string_pretty(&dev_wallet_data)
        .context("Failed to serialize dev wallet data to JSON")?;

    let mut file = File::create(&file_path)
        .with_context(|| format!("Failed to create dev wallet file: {}", file_path.display()))?;
    file.write_all(json_string.as_bytes())
        .with_context(|| format!("Failed to write dev wallet to file: {}", file_path.display()))?;

    log::info!("Successfully generated and saved dev wallet to {}", file_path.display());
    Ok((public_key_bs58, private_key_bs58))
}
/// Generates a new Solana keypair, saves it to "dev_wallet_[timestamp].json",
/// and returns the private key (bs58), public key (bs58), and the filename.
pub fn generate_single_dev_wallet() -> Result<(String, String, String), anyhow::Error> {
    let keypair = Keypair::new();
    let public_key_bs58 = keypair.pubkey().to_string();
    let private_key_bs58 = bs58::encode(keypair.to_bytes()).into_string();

    let now = Local::now();
    let timestamp_str = now.format("%Y%m%d_%H%M%S").to_string();
    let original_filename_str = format!("dev_wallet_{}.json", timestamp_str);
    let filename_to_return = original_filename_str.clone(); // Clone for returning before use in path ops

    // Save in the current directory.
    let current_dir = std::env::current_dir().context("Failed to get current directory")?;
    let file_path = current_dir.join(&original_filename_str); // Use the original string for path joining

    let dev_wallet_data = DevWalletFile {
        public_key: public_key_bs58.clone(),
        private_key: private_key_bs58.clone(),
    };

    let json_string = serde_json::to_string_pretty(&dev_wallet_data)
        .context("Failed to serialize dev wallet data to JSON")?;

    let mut file = File::create(&file_path)
        .with_context(|| format!("Failed to create dev wallet file: {}", file_path.display()))?;
    file.write_all(json_string.as_bytes())
        .with_context(|| format!("Failed to write dev wallet to file: {}", file_path.display()))?;

    log::info!("Successfully generated and saved dev wallet to {}", file_path.display());
    Ok((private_key_bs58, public_key_bs58, filename_to_return))
}
// load_keypair_from_string function moved to src/commands/utils.rs