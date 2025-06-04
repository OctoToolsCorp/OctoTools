use anyhow::{Result, Context};
use log::{info, error, warn, debug};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    // compute_budget::ComputeBudgetInstruction, // Marked as unused
    message::{v0::Message as MessageV0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account,
};
use spl_token::{self, native_mint::ID as NATIVE_SOL_MINT};
use std::{
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::time::sleep;

// Define the constant here as it's used in this file
const MAX_TRANSACTIONS_PER_BUNDLE: usize = 5; // Max total transactions Jito allows in a bundle

use crate::{
    config::get_commitment_config,
    errors::PumpfunError,
    models::settings::AppSettings as Settings,
    utils::transaction::{send_jito_bundle_via_json_rpc, sign_and_send_versioned_transaction},
};

// Placeholder for exchange type, similar to atomic_buy
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellExchange {
    PumpFun,
    Raydium, // This could be AMM/CLMM or a specific launchpad if applicable
}

// Parameters for each wallet's sell operation
#[derive(Debug, Clone)]
pub struct WalletSellParams {
    pub token_mint_to_sell: Pubkey,
    pub amount_tokens_to_sell: u64, // Specific amount of tokens
    // pub percentage_to_sell: Option<f64>, // Alternative: sell a percentage
    // pub sell_all: bool, // Alternative: sell all tokens
}

#[allow(clippy::too_many_arguments)]
pub async fn atomic_sell(
    rpc_client: Arc<RpcClient>,
    settings: &Settings, // Assuming settings are passed
    exchange: SellExchange,
    participating_wallets_with_params: Vec<(Keypair, WalletSellParams)>,
    slippage_bps: u64,
    priority_fee_lamports_per_tx: u64,
    jito_tip_sol_per_tx: f64,     // For Pump.fun style per-tx tip
    jito_overall_tip_sol: f64,    // For Raydium style bundle tip
    jito_block_engine_url: String,
    jito_tip_account_pubkey_str: String,
    // lookup_table_address_str: Option<String>, // May need this later
) -> Result<(), PumpfunError> {
    info!("⚛️ Preparing Multi-Wallet Atomic Sell Jito Bundle for {:?}...", exchange);

    if participating_wallets_with_params.is_empty() {
        error!("No participating wallets provided for the sell operation.");
        return Err(PumpfunError::Wallet("No participating wallets for sell.".into()));
    }

    let tip_account_pubkey = Pubkey::from_str(&jito_tip_account_pubkey_str)
        .map_err(|e| PumpfunError::Config(format!("Invalid Jito Tip Account Pubkey from settings '{}': {}", jito_tip_account_pubkey_str, e)))?;

    let commitment_config: CommitmentConfig = get_commitment_config(); // Ensure get_commitment_config returns CommitmentConfig
    // let mut bundle_transactions: Vec<VersionedTransaction> = Vec::new(); // No longer bundling for Raydium Jito
    let mut total_successful_sell_preparations = 0;
 
    let launchpad_program_id = Pubkey::from_str("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj")
        .map_err(|e| {
            error!("Failed to parse Raydium Launchpad Program ID: {}", e);
            PumpfunError::Config("Invalid Raydium Launchpad Program ID".to_string())
        })?;

    match exchange {
        SellExchange::PumpFun => {
            info!("Handling Pump.fun sell logic (Not yet implemented)...");
            // TODO: Implement Pump.fun sell logic
        }
        SellExchange::Raydium => {
            info!("Attempting Raydium Launchpad sell logic...");

            // Jito bundle tip removed for Raydium sells; transactions will be sent individually.
            // let overall_jito_tip_lamports = (jito_overall_tip_sol * 1_000_000_000.0) as u64;
            // if overall_jito_tip_lamports > 0 { ... } // Tip logic removed
 
            for (i, (wallet_keypair, sell_params)) in participating_wallets_with_params.iter().enumerate() {
                // Bundle limit checks removed as we are sending individually.
                // let max_slots_for_sell_ops = MAX_TRANSACTIONS_PER_BUNDLE;
                // if bundle_transactions.len() >= max_slots_for_sell_ops.saturating_sub(1) { ... } // Limit check removed
 
                let seller_pk = wallet_keypair.pubkey();
                info!("Processing Raydium Launchpad sell for wallet #{} ({})", i + 1, seller_pk);
                info!("  Token to sell: {}, Amount: {}", sell_params.token_mint_to_sell, sell_params.amount_tokens_to_sell);

                if sell_params.amount_tokens_to_sell == 0 {
                    warn!("  Skipping wallet {} due to zero token amount to sell.", seller_pk);
                    continue;
                }

                match crate::api::raydium_launchpad::get_launchpad_market_info(
                    &rpc_client,
                    &launchpad_program_id,
                    &sell_params.token_mint_to_sell,
                    &NATIVE_SOL_MINT,
                ).await {
                    Ok(Some(market_keys)) => {
                        info!("  Successfully fetched dynamic Raydium Launchpad market keys for token {}.", sell_params.token_mint_to_sell);

                        let source_token_ata = get_associated_token_address(&seller_pk, &sell_params.token_mint_to_sell);
                        if rpc_client.get_account(&source_token_ata).await.is_err() {
                            error!("  Seller's source token ATA {} for {} not found. Cannot sell. Skipping.", source_token_ata, sell_params.token_mint_to_sell);
                            continue;
                        }
                        info!("  Seller's source token ATA {} for {} confirmed to exist.", source_token_ata, sell_params.token_mint_to_sell);
                        
                        let dest_wsol_ata = get_associated_token_address(&seller_pk, &NATIVE_SOL_MINT);
                        if rpc_client.get_account(&dest_wsol_ata).await.is_err() {
                            info!("    Seller's destination WSOL ATA {} not found. Creating...", dest_wsol_ata);
                            let create_wsol_ata_ix = create_associated_token_account(&seller_pk, &seller_pk, &NATIVE_SOL_MINT, &spl_token::id());
                            let latest_blockhash_ata = rpc_client.get_latest_blockhash().await?;
                            let ata_tx_msg = MessageV0::try_compile(&seller_pk, &[create_wsol_ata_ix], &[], latest_blockhash_ata)?;
                            let ata_tx = VersionedTransaction::try_new(VersionedMessage::V0(ata_tx_msg), &[wallet_keypair])?;
                            
                            match sign_and_send_versioned_transaction(&rpc_client, ata_tx, &[wallet_keypair]).await {
                                Ok(sig) => { info!("    ✅ Destination WSOL ATA {} created. Sig: {}", dest_wsol_ata, sig); sleep(Duration::from_secs(5)).await; },
                                Err(e) => { error!("    ❌ Destination WSOL ATA creation failed for {}: {}. Skipping.", dest_wsol_ata, e); continue; }
                            }
                        } else {
                            info!("    Seller's destination WSOL ATA {} already exists.", dest_wsol_ata);
                        }

                        let min_sol_out_placeholder: u64 = 1;

                        info!("    Attempting to build Raydium Launchpad sell transaction for wallet {}...", seller_pk);
                        match crate::api::raydium_launchpad::build_raydium_launchpad_sell_transaction(
                            wallet_keypair,
                            &market_keys,
                            &sell_params.token_mint_to_sell,
                            &NATIVE_SOL_MINT,
                            sell_params.amount_tokens_to_sell,
                            min_sol_out_placeholder,
                            Some(priority_fee_lamports_per_tx)
                        ).await {
                            Ok(instructions) => {
                                info!("    Successfully built Raydium Launchpad sell instructions for wallet {}.", seller_pk);
                                let latest_blockhash = rpc_client.get_latest_blockhash().await
                                    .context(format!("Failed to get latest blockhash for Raydium sell (wallet {})", seller_pk))?;
                                info!("    Got latest blockhash for Raydium sell (wallet {}): {}", seller_pk, latest_blockhash);
                                
                                let message = MessageV0::try_compile(&seller_pk, &instructions, &[], latest_blockhash)?;
                                info!("    Compiled Raydium sell message for wallet {}.", seller_pk);
                                
                                let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[wallet_keypair])?;
                                info!("    Signed Raydium sell transaction for wallet {}.", seller_pk);

                                info!("    Simulating Raydium Launchpad sell transaction for wallet {}...", seller_pk);
                                let base_sim_config = solana_client::rpc_config::RpcSimulateTransactionConfig {
                                    sig_verify: false, replace_recent_blockhash: true, commitment: Some(commitment_config),
                                    encoding: Some(solana_transaction_status::UiTransactionEncoding::Base64),
                                    accounts: None, min_context_slot: None, inner_instructions: false,
                                };
                                let sim_config_for_sell = base_sim_config.clone();
                                let sim_config_for_unwrap = base_sim_config.clone();
                                match rpc_client.simulate_transaction_with_config(&tx, sim_config_for_sell).await {
                                    Ok(response) => {
                                        if let Some(err_val) = response.value.err { // Renamed err to err_val to avoid conflict
                                            error!("Raydium Launchpad Sell TX for {} Simulation FAILED: {:?}", seller_pk, err_val);
                                            if let Some(logs) = response.value.logs { logs.iter().for_each(|log| error!("- {}", log)); }
                                            continue;
                                        } else {
                                            info!("    ✅ Raydium Launchpad Sell TX for {} Simulation Successful.", seller_pk);
                                            if let Some(units) = response.value.units_consumed { debug!("Simulated CU for sell: {}", units); }

                                            // Now, prepare to unwrap the WSOL received
                                            info!("    Preparing to unwrap WSOL for wallet {} from ATA {}", seller_pk, dest_wsol_ata);
                                            let unwrap_ix = spl_token::instruction::close_account(
                                                &spl_token::id(),
                                                &dest_wsol_ata, // Source WSOL account to close
                                                &seller_pk,     // Destination for unwrapped SOL (owner of WSOL ATA)
                                                &seller_pk,     // Authority to close WSOL ATA
                                                &[],
                                            ).map_err(|e| {
                                                error!("Failed to create close_account instruction for {}: {}", seller_pk, e);
                                                PumpfunError::Build(format!("Failed to create WSOL unwrap instruction: {}", e))
                                            })?;
                                            
                                            let latest_blockhash_unwrap = rpc_client.get_latest_blockhash().await
                                                .context(format!("Failed to get blockhash for WSOL unwrap (wallet {})", seller_pk))?;
                                            
                                            let unwrap_message = MessageV0::try_compile(
                                                &seller_pk,
                                                &[unwrap_ix], // Potentially add ComputeBudget if needed, though close_account is usually low
                                                &[], // No ALTs needed for simple close
                                                latest_blockhash_unwrap,
                                            )?;
                                            let unwrap_tx = VersionedTransaction::try_new(VersionedMessage::V0(unwrap_message), &[wallet_keypair])?;

                                            info!("    Simulating WSOL unwrap transaction for wallet {}...", seller_pk);
                                            // Use the same sim_config as the sell transaction
                                            match rpc_client.simulate_transaction_with_config(&unwrap_tx, sim_config_for_unwrap).await {
                                                Ok(unwrap_response) => {
                                                    if let Some(unwrap_err_val) = unwrap_response.value.err {
                                                        error!("WSOL Unwrap TX for {} Simulation FAILED: {:?}", seller_pk, unwrap_err_val);
                                                        if let Some(logs) = unwrap_response.value.logs { logs.iter().for_each(|log| error!("- {}", log)); }
                                                        continue; // Skip this wallet if unwrap fails
                                                    } else {
                                                        info!("    ✅ WSOL Unwrap TX for {} Simulation Successful.", seller_pk);
                                                        if let Some(units) = unwrap_response.value.units_consumed { debug!("Simulated CU for unwrap: {}", units); }
                                                        
                                                        // Both sell and unwrap simulations successful. Send them individually.
                                                        info!("    Sending Raydium Launchpad Sell TX for {}...", seller_pk);
                                                        match sign_and_send_versioned_transaction(&rpc_client, tx, &[wallet_keypair]).await {
                                                            Ok(sell_sig) => {
                                                                info!("    ✅ Raydium Sell TX for {} successful. Signature: {}", seller_pk, sell_sig);
                                                                
                                                                info!("    Sending WSOL Unwrap TX for {}...", seller_pk);
                                                                match sign_and_send_versioned_transaction(&rpc_client, unwrap_tx, &[wallet_keypair]).await {
                                                                    Ok(unwrap_sig) => {
                                                                        info!("    ✅ WSOL Unwrap TX for {} successful. Signature: {}", seller_pk, unwrap_sig);
                                                                        total_successful_sell_preparations += 1;
                                                                    }
                                                                    Err(e_unwrap_send) => {
                                                                        error!("    ❌ Failed to send WSOL Unwrap TX for {}: {}", seller_pk, e_unwrap_send);
                                                                        // Decide if we should continue with other wallets or count this as partial success
                                                                    }
                                                                }
                                                            }
                                                            Err(e_sell_send) => {
                                                                error!("    ❌ Failed to send Raydium Sell TX for {}: {}", seller_pk, e_sell_send);
                                                                // Skip unwrap if sell failed
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(e_unwrap_sim) => {
                                                    error!("Error during WSOL unwrap simulation for {}: {}", seller_pk, e_unwrap_sim);
                                                    continue; // Skip this wallet if unwrap simulation request fails
                                                }
                                            }
                                        }
                                    }
                                    Err(e_sim) => { error!("Error during Raydium sell simulation for {}: {}", seller_pk, e_sim); continue; }
                                }
                            }
                            Err(e_build) => {
                                error!("    Failed to build Raydium Launchpad sell transaction for wallet {}: {}. Skipping.", seller_pk, e_build);
                                continue;
                            }
                        }
                    }
                    Ok(None) => {
                        warn!("  Could not find Raydium Launchpad market info for token {}. Skipping wallet {}.", sell_params.token_mint_to_sell, seller_pk);
                        // This path would typically mean it's not a launchpad token, so a generic Raydium AMM sell might be attempted.
                        // For now, the logic is specific to launchpad. If generic AMM sell is added, WSOL unwrap would apply there too.
                        continue;
                    }
                    Err(e_fetch) => {
                        error!("  Error fetching Raydium Launchpad market info for token {}: {}. Skipping wallet {}.", sell_params.token_mint_to_sell, e_fetch, seller_pk);
                        continue;
                    }
                }
            }
        }
    }

    // Logic for sending Jito bundle is removed for Raydium sells.
    // Transactions are now sent individually within the loop.
    if total_successful_sell_preparations > 0 {
        info!("✅ Completed processing Raydium sells for {} wallets individually.", total_successful_sell_preparations);
    } else {
        info!("No Raydium sell transactions were successfully processed individually.");
        if !participating_wallets_with_params.is_empty() { // If there were wallets but none succeeded
            // Consider if an error should be returned if no operations succeed.
            // For now, just logging.
            warn!("Although wallets were provided for Raydium sell, no transactions were successfully sent.");
        }
    }
    
    info!("Atomic sell for {:?} completed.", exchange);
    Ok(())
}