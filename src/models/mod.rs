pub mod settings; // <-- Declare the new settings module
pub mod token;
pub mod wallet;
pub mod api;
pub mod simulation;
use crate::models::wallet::WalletInfo as LoadedWalletInfo; // Added for LaunchParams
use solana_sdk::signature::Signature; // Needed for LaunchStatus::Confirmed

// Enum for detailed Launch status updates
// Moved from gui/app.rs to break circular dependency
#[derive(Debug, Clone)]
pub enum LaunchStatus {
    Starting,
    Log(String), // Log message string
    Error(String), // General error reporting
    UploadingMetadata,
    MetadataUploaded(String), // URI
    PreparingTx(String), // Tx Label (e.g., "Tx1 (Create+DevBuy)", "Tx2 (Zombies 1-6)")
    PreparingZombieTxs, // Specifically for zombie transaction preparation phase
    SimulatingTx(String), // Tx Label
    SimulationSuccess(String), // Tx Label
    SimulationFailed(String, String), // Tx Label, Error
    SimulationWarning(String, String), // Tx Label, Warning Message (proceeding anyway)
    SimulationSkipped(String), // Tx Label for a skipped transaction
    BuildFailed(String, String), // Tx Label, Error
    SubmittingBundle(usize), // Number of transactions (was SendingBundle)
    BundleSubmitted(String), // Bundle ID (was BundleSent)
    SimulatedOnly(String), // Message for simulation-only mode completion
BundleLanded(String, String), // Bundle ID, Confirmation Status
    Completed, // Final success status without a message
    Success(String), // Final success message (including Bundle ID if applicable) - Kept for cases with specific success messages
    Failure(String), // Error message - Kept for general failures, though Error(String) might be preferred
    LaunchCompletedWithLink(String), // New: Final success with the pump.fun link
}

// Enum for detailed ALT creation status updates
// Moved from gui/app.rs to break circular dependency
#[derive(Debug, Clone)]
pub enum AltCreationStatus {
    Starting,
    Sending(String), // Description of tx being sent (e.g., "Create", "Extend Batch 1")
    Confirmed(String, Signature), // Description and Signature
    LogMessage(String), // New variant for detailed logs from send_alt_tx_internal
    Success(solana_sdk::pubkey::Pubkey), // Final ALT Address
    Failure(String), // Error message
}

#[derive(Clone, Debug)]
pub struct LaunchParams {
    pub name: String,
    pub symbol: String,
    pub description: String,
    pub image_url: String,
    pub dev_buy_sol: f64,
    pub zombie_buy_sol: f64,
    pub minter_private_key_str: String,
    pub slippage_bps: u64,
    pub priority_fee: u64,
    pub alt_address_str: String,
    pub mint_keypair_path_str: String,
    pub loaded_wallet_data: Vec<LoadedWalletInfo>,
    pub use_jito_bundle: bool,
    pub jito_block_engine_url: String,
    pub twitter: String,
    pub telegram: String,
    pub website: String,
    pub simulate_only: bool,
    pub main_tx_priority_fee_micro_lamports: u64,
    pub jito_actual_tip_sol: f64,
}