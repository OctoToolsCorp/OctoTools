use crate::errors::{PumpfunError, Result};
use log::{debug, error, info};
use reqwest::{Client, multipart};
use serde::{Deserialize, Serialize};
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    signature::Keypair,
    transaction::Transaction,
};
use std::str::FromStr;

// Pump.fun API client
pub struct PumpClient {
    client: Client,
    base_url: String,
}

// Implementation for PumpClient
impl PumpClient {
    // Create a new instance of the Pump client
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            base_url: "https://api.pump.fun/v1".to_string(),
        }
    }

    // Create a token on pump.fun (not available via API, returns instructions)
    pub async fn create_token(&self) -> Result<String> {
        info!("ℹ️ Creating token on Pump.fun is not currently available via API");
        info!("ℹ️ Please follow these steps:");
        info!("1. Go to https://pump.fun/create");
        info!("2. Connect wallet");
        info!("3. Fill out the form with token details");
        info!("4. Click 'Create Token' and approve the transaction");
        
        Ok("Manual creation required".to_string())
    }

    // Upload a token image to pump.fun
    pub async fn upload_image(&self, token_address: &str, image_path: &str) -> Result<String> {
        debug!("Uploading image for token {} from {}", token_address, image_path);

        // Check if the file exists
        if !std::path::Path::new(image_path).exists() {
            return Err(PumpfunError::InvalidInput(format!("Image file not found: {}", image_path)));
        }

        // Read the image file
        let file_data = std::fs::read(image_path)?;
        let file_part = multipart::Part::bytes(file_data)
            .file_name(std::path::Path::new(image_path).file_name().unwrap().to_string_lossy().to_string())
            .mime_str(match std::path::Path::new(image_path).extension().and_then(|ext| ext.to_str()) {
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                Some("gif") => "image/gif",
                _ => "application/octet-stream",
            })?;

        // Create multipart form
        let form = multipart::Form::new()
            .part("image", file_part)
            .text("token", token_address.to_string());

        // Upload the image
        let response = self.client.post(&format!("{}/token/upload-image", self.base_url))
            .multipart(form)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(PumpfunError::ApiError(format!("Failed to upload image: {}", error_text)));
        }

        Ok("Image uploaded successfully".to_string())
    }

    // Buy token from Pump.fun
    pub async fn buy_token(
        &self,
        token_mint: &str,
        amount: f64,
        slippage: f64,
        priority_fee: u64,
        wallet: &Keypair,
    ) -> Result<String> {
        debug!("Building buy transaction for {} tokens with slippage {}%", amount, slippage);

        // Build the request payload
        let payload = serde_json::json!({
            "mint": token_mint,
            "amount": amount,
            "slippage": slippage,
            "priority": priority_fee
        });

        // Request the buy transaction
        let response = self.client.post(&format!("{}/trade/buy", self.base_url))
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            return Err(PumpfunError::ApiError(format!("Failed to get buy transaction: {}", error_text)));
        }

        // Parse the transaction response
        let response_data: BuyTransactionResponse = response.json().await?;
        
        // Convert instructions to Solana instructions
        let mut instructions = Vec::new();
        for ix_data in response_data.instructions {
            let program_id = Pubkey::from_str(&ix_data.program_id)?;
            
            let accounts = ix_data.accounts.into_iter()
                .map(|acc| {
                    let pubkey = Pubkey::from_str(&acc.pubkey).map_err(|e| {
                        PumpfunError::InvalidInput(format!("Invalid pubkey: {}", e))
                    })?;
                    
                    Ok(solana_sdk::instruction::AccountMeta {
                        pubkey,
                        is_signer: acc.is_signer,
                        is_writable: acc.is_writable,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            
            let data = bs58::decode(&ix_data.data)
                .into_vec()
                .map_err(|e| PumpfunError::InvalidInput(format!("Invalid instruction data: {}", e)))?;
                
            instructions.push(Instruction {
                program_id,
                accounts,
                data,
            });
        }
        
        // Create and sign the transaction
        let (recent_blockhash, _) = response_data.blockhash.into();
        
        let mut transaction = Transaction::new_with_payer(&instructions, Some(&wallet.pubkey()));
        transaction.sign(&[wallet], recent_blockhash);
                
        let serialized_tx = bincode::serialize(&transaction)?;
        let tx_base64 = base64::encode(&serialized_tx);
        
        Ok(tx_base64)
    }
}

// Response model for buy transaction
#[derive(Debug, Deserialize)]
struct BuyTransactionResponse {
    blockhash: TransactionBlockhash,
    instructions: Vec<TransactionInstruction>,
}

#[derive(Debug, Deserialize)]
struct TransactionBlockhash {
    blockhash: String,
    last_valid_block_height: u64,
}

impl From<TransactionBlockhash> for (solana_sdk::hash::Hash, u64) {
    fn from(blockhash: TransactionBlockhash) -> Self {
        let hash = solana_sdk::hash::Hash::from_str(&blockhash.blockhash)
            .unwrap_or_else(|_| solana_sdk::hash::Hash::default());
        (hash, blockhash.last_valid_block_height)
    }
}

#[derive(Debug, Deserialize)]
struct TransactionInstruction {
    program_id: String,
    accounts: Vec<TransactionAccount>,
    data: String,
}

#[derive(Debug, Deserialize)]
struct TransactionAccount {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
} 