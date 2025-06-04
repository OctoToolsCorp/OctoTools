use anyhow::{anyhow, Context, Result};
use bs58;
use solana_sdk::signature::Keypair;
// use console::{Style};
// use crate::errors::Result;
// use crate::wallet::{get_wallet_keypair};
// use crate::wallet::{WalletInfo};
// use solana_sdk::signature::{Keypair, Signer};

// Prefix unused styles

// Remove unused function (now handled in wallets.rs)
// pub async fn generate_key(vanity: Option<String>, keys_path_arg: Option<&str>) -> Result<()> { ... } 
/// Loads a Solana Keypair from a base58 encoded private key string.
pub fn load_keypair_from_string(pk_str: &str, wallet_name: &str) -> Result<Keypair> {
    if pk_str.is_empty() {
        return Err(anyhow!("Private key for {} is empty.", wallet_name));
    }
    let bytes = bs58::decode(pk_str)
        .into_vec()
        .with_context(|| format!("Failed to decode private key for {}: invalid base58 string", wallet_name))?;
    Keypair::from_bytes(&bytes)
        .with_context(|| format!("Failed to create keypair from bytes for {}", wallet_name))
}

use solana_client::nonblocking::rpc_client::RpcClient as AsyncRpcClient;
use solana_sdk::{
    transaction::VersionedTransaction,
    signature::{Signature},
    commitment_config::CommitmentConfig,
};
use tokio::sync::mpsc::UnboundedSender; // For status sending
use crate::models::LaunchStatus; // Or a more generic status enum if preferred
use log::{info, error, warn as log_warn}; // For logging within the function -> Aliased warn

// Function to sign and send a versioned transaction with specific commitment and status updates
pub async fn sign_and_send_versioned_transaction_with_config(
    rpc_client: &AsyncRpcClient,
    mut transaction: VersionedTransaction, // Made mutable to update blockhash
    signers: &[&Keypair],
    commitment_config: CommitmentConfig,
    status_sender: UnboundedSender<LaunchStatus>, // For sending status updates
    tx_label: String, // For identifying the transaction in logs/status
) -> anyhow::Result<Signature> {
    info!("[{}] Attempting to send transaction...", tx_label);
    let _ = status_sender.send(LaunchStatus::Log(format!("[{}] Fetching latest blockhash...", tx_label)));

    let latest_blockhash = rpc_client.get_latest_blockhash_with_commitment(commitment_config).await
        .map_err(|e| {
            error!("[{}] Failed to get latest blockhash: {}", tx_label, e);
            anyhow!("[{}] Failed to get latest blockhash: {}", tx_label, e)
        })?;
    
    transaction.message.set_recent_blockhash(latest_blockhash.0);
    let _ = status_sender.send(LaunchStatus::Log(format!("[{}] Blockhash updated: {}", tx_label, latest_blockhash.0)));

    // Sign the transaction
    // Note: VersionedTransaction::try_new creates a new tx. If we already have one and just updated blockhash,
    // we might need to re-sign it or ensure it was signed with a placeholder blockhash initially.
    // For simplicity, assuming `transaction` was constructed with a placeholder and needs signing here,
    // or that `try_sign` is appropriate.
    // However, `VersionedTransaction` is typically signed upon creation with `try_new`.
    // If `transaction` is already signed but blockhash changed, we need to re-sign.
    // Let's assume it needs to be signed now.
    // This is tricky because `VersionedTransaction` doesn't have a simple `sign` method like legacy `Transaction`.
    // It's signed by `try_new`. So, we must reconstruct it if blockhash changes *after* initial signing.

    // The `transaction` parameter is already a `VersionedTransaction`.
    // If its blockhash was updated, its signatures are now invalid.
    // It needs to be re-signed. The `signers` are passed, so we can re-create it.
    
    let signed_transaction = VersionedTransaction::try_new(transaction.message.clone(), signers)
        .map_err(|e| {
            error!("[{}] Failed to re-sign transaction after blockhash update: {}", tx_label, e);
            anyhow!("[{}] Failed to re-sign transaction: {}", tx_label, e)
        })?;
    
    let _ = status_sender.send(LaunchStatus::Log(format!("[{}] Transaction signed. Sending...", tx_label)));

    // Send and confirm
    // Using send_and_confirm_transaction_with_spinner_and_config for better UX if available,
    // or simulate + send_transaction + confirm_transaction loop.
    // For simplicity, let's use send_transaction and then poll for confirmation.
    // RpcClient has `send_and_confirm_transaction_with_spinner_and_config` but it's blocking.
    // AsyncRpcClient has `send_transaction_with_config`.

    match rpc_client.send_transaction_with_config(&signed_transaction, solana_client::rpc_config::RpcSendTransactionConfig {
        skip_preflight: false, // Default, can be true if confident
        preflight_commitment: Some(commitment_config.commitment),
        encoding: None, // Default
        max_retries: Some(5), // Example retry count
        min_context_slot: None,
    }).await {
        Ok(signature) => {
            info!("[{}] Transaction sent. Signature: {}. Confirming...", tx_label, signature);
            let _ = status_sender.send(LaunchStatus::Log(format!("[{}] Tx sent: {}. Confirming...", tx_label, signature)));
            
            // Confirmation loop
            let mut attempts = 0;
            loop {
                attempts += 1;
                if attempts > 60 { // Approx 30 seconds timeout
                    let err_msg = format!("[{}] Confirmation timeout for signature: {}", tx_label, signature);
                    error!("{}", err_msg);
                    let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
                    return Err(anyhow!(err_msg));
                }
                match rpc_client.get_signature_statuses(&[signature]).await {
                    Ok(statuses) => {
                        if let Some(Some(status)) = statuses.value.get(0) {
                            if status.err.is_some() {
                                let tx_err = status.err.as_ref().unwrap().to_string();
                                let err_msg = format!("[{}] Transaction failed confirmation: {}. Sig: {}", tx_label, tx_err, signature);
                                error!("{}", err_msg);
                                let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
                                return Err(anyhow!(err_msg));
                            } else if status.confirmation_status.as_ref().map_or(false, |cs| *cs == solana_transaction_status::TransactionConfirmationStatus::Finalized || *cs == solana_transaction_status::TransactionConfirmationStatus::Confirmed) {
                                info!("[{}] Transaction confirmed: {}", tx_label, signature);
                                let _ = status_sender.send(LaunchStatus::Log(format!("[{}] Tx confirmed: {}", tx_label, signature)));
                                return Ok(signature);
                            } else {
                                // Still processing
                                let _ = status_sender.send(LaunchStatus::Log(format!("[{}] Tx status: {:?}. Attempt {}/60", tx_label, status.confirmation_status, attempts)));
                            }
                        } else {
                             let _ = status_sender.send(LaunchStatus::Log(format!("[{}] Tx status pending. Attempt {}/60", tx_label, attempts)));
                        }
                    }
                    Err(e) => {
                        log_warn!("[{}] Error fetching signature status for {}: {}. Retrying...", tx_label, signature, e);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        Err(e) => {
            let err_msg = format!("[{}] Failed to send transaction: {}", tx_label, e);
            error!("{}", err_msg);
            let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
            Err(anyhow!(err_msg))
        }
    }
}