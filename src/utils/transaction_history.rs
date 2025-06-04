// src/utils/transaction_history.rs
use solana_client::nonblocking::rpc_client::RpcClient;
// Removed problematic import: use solana_client::rpc_request::RpcGetSignaturesForAddressConfig;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionEncoding,
    // TransactionDetails, UiTransactionTokenBalance, // Removed unused imports
};
use solana_rpc_client_api::request::RpcRequest;
use solana_rpc_client_api::response::RpcConfirmedTransactionStatusWithSignature;
// Removed: use solana_account_decoder::parse_token::OptionValue;


use serde::{Deserialize, Serialize};
use serde_json::json; // For building JSON params
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolTransferInfo {
    pub from_account: Option<Pubkey>,
    pub to_account: Option<Pubkey>,
    pub amount_lamports: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenTransferInfo {
    pub mint: Pubkey,
    pub from_account: Option<Pubkey>, // Token account
    pub to_account: Option<Pubkey>,   // Token account
    pub from_owner: Option<Pubkey>,   // Owner of the from_account
    pub to_owner: Option<Pubkey>,     // Owner of the to_account
    pub amount: u64,                  // Raw token amount
    pub decimals: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransactionType {
    Unknown,
    SolTransfer,
    TokenTransfer,
    Swap, // Generic swap type
    MintTo,
    CreateAccount,
    InitializeMint,
    // Add other relevant types
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PnlAction {
    Buy {
        token_mint: Pubkey,
        token_amount: u64,
        token_decimals: u8,
        cost_sol: u64,
    },
    Sell {
        token_mint: Pubkey,
        token_amount: u64,
        token_decimals: u8,
        proceeds_sol: u64,
    },
    Receive { // e.g. airdrop, transfer in from another of user's wallets
        token_mint: Pubkey,
        token_amount: u64,
        token_decimals: u8,
    },
    Send { // e.g. transfer out to another wallet/exchange
        token_mint: Pubkey,
        token_amount: u64,
        token_decimals: u8,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedTransactionInfo {
    pub signature: Signature,
    pub block_time: Option<i64>, // Unix timestamp
    pub fee_lamports: u64,
    pub involved_accounts: Vec<Pubkey>, // All accounts involved
    pub transaction_type: TransactionType, // General type of the transaction
    pub sol_transfers: Vec<SolTransferInfo>,
    pub token_transfers: Vec<TokenTransferInfo>,
    pub success: bool,
    pub pnl_relevant_action: Option<PnlAction>, // Specific action for PnL tracking
}

// We'll add functions here to fetch and parse transactions.
pub async fn get_transaction_history_for_token(
    rpc_client: &RpcClient,
    user_wallet: &Pubkey,
    token_mint_to_track: &Pubkey, // Renamed for clarity
    limit: Option<usize>,
    before_signature: Option<Signature>, // For pagination
    until_signature: Option<Signature>,  // For pagination
) -> Result<Vec<ParsedTransactionInfo>, Box<dyn Error>> {
    let mut signatures_to_fetch: Vec<solana_sdk::signature::Signature> = Vec::new();
    let mut all_parsed_transactions: Vec<ParsedTransactionInfo> = Vec::new();

    // 1. Fetch transaction signatures for the user_wallet
    // It's important that user_wallet is the main wallet address (owner), not a token account.
    let mut current_before_signature_option = before_signature;
    let rpc_batch_size: usize = 100; // Standard batch size for RPC calls

    loop {
        let actual_batch_limit = if let Some(total_limit) = limit {
            std::cmp::min(rpc_batch_size, total_limit.saturating_sub(signatures_to_fetch.len()))
        } else {
            rpc_batch_size
        };

        if actual_batch_limit == 0 && limit.is_some() {
            log::debug!("Total signature limit reached.");
            break;
        }

        let commitment_config = solana_sdk::commitment_config::CommitmentConfig::confirmed();
        let mut config_map = serde_json::Map::new();
        config_map.insert("commitment".to_string(), json!(commitment_config.commitment.to_string()));
        config_map.insert("limit".to_string(), json!(actual_batch_limit));

        if let Some(before_sig) = current_before_signature_option {
            config_map.insert("before".to_string(), json!(before_sig.to_string()));
        }
        if let Some(until_sig) = until_signature { // Pass this through if provided
            config_map.insert("until".to_string(), json!(until_sig.to_string()));
        }
        
        let params = json!([
            user_wallet.to_string(),
            config_map // Automatically becomes Value::Object(config_map)
        ]);

        log::debug!(
            "Sending RpcRequest::GetSignaturesForAddress for wallet {} with params: {:?}",
            user_wallet, params
        );
        
        // Use RpcClient::send<T>() which handles JSON-RPC request and deserialization
        let response_tx_statuses: Vec<RpcConfirmedTransactionStatusWithSignature> = rpc_client
            .send(RpcRequest::GetSignaturesForAddress, params) // Pass params directly
            .await?;

        if response_tx_statuses.is_empty() {
            log::debug!("No more signatures found for wallet {}.", user_wallet);
            break;
        }
        
        let mut last_fetched_sig_for_pagination: Option<solana_sdk::signature::Signature> = None;

        for tx_status in response_tx_statuses {
            match solana_sdk::signature::Signature::from_str(&tx_status.signature) {
                Ok(sdk_signature) => {
                    signatures_to_fetch.push(sdk_signature);
                    last_fetched_sig_for_pagination = Some(sdk_signature); // Update for next 'before'
                }
                Err(e) => {
                    log::error!("Failed to parse signature string '{}': {}", tx_status.signature, e);
                    continue;
                }
            }
        }
        
        current_before_signature_option = last_fetched_sig_for_pagination;

        log::debug!("Fetched {} signatures in this batch. Total so far: {}", signatures_to_fetch.len(), signatures_to_fetch.len());

        // If a total limit was provided by the caller
        if let Some(total_call_limit) = limit {
            if signatures_to_fetch.len() >= total_call_limit {
                log::debug!("Reached total fetch limit of {}.", total_call_limit);
                break;
            }
        }
        
        if current_before_signature_option.is_none() {
            log::debug!("No 'before' signature to continue pagination (last batch was likely the end or empty).");
            break;
        }
    }
    log::debug!("Finished fetching signatures. Total {} signatures for wallet {}", signatures_to_fetch.len(), user_wallet);

    // 2. For each signature, fetch the full transaction details
    for signature in signatures_to_fetch {
        let tx_config = solana_client::rpc_config::RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::JsonParsed),
            commitment: Some(solana_sdk::commitment_config::CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0), // Fetch version 0, can be adjusted
        };

        log::debug!("Fetching transaction details for signature: {}", signature);
        match rpc_client.get_transaction_with_config(&signature, tx_config).await {
            Ok(encoded_tx_with_meta) => {
                // 3. Parse the transaction into ParsedTransactionInfo
                match parse_encoded_transaction(encoded_tx_with_meta, &signature, token_mint_to_track, user_wallet) {
                    Ok(parsed_info) => {
                        // The involves_token check is now more critical with PnlAction
                        if parsed_info.success && (parsed_info.pnl_relevant_action.is_some() || involves_token(&parsed_info, token_mint_to_track)) {
                            all_parsed_transactions.push(parsed_info);
                        } else if !parsed_info.success {
                            log::debug!("Skipping failed transaction: {}", signature);
                        } else {
                            log::debug!("Skipping transaction {} as it does not involve token {} in a PnL relevant way or via involves_token check.", signature, token_mint_to_track);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to parse transaction {}: {}", signature, e);
                    }
                }
            }
            Err(e) => {
                log::error!("Failed to get transaction details for signature {}: {}", signature, e);
            }
        }
    }

    log::info!("Finished fetching and parsing transactions. Found {} relevant transactions for wallet {} and token {}.", all_parsed_transactions.len(), user_wallet, token_mint_to_track);
    Ok(all_parsed_transactions)
}

fn parse_encoded_transaction(
    encoded_tx_with_meta: EncodedConfirmedTransactionWithStatusMeta,
    signature: &Signature,
    token_mint_to_track: &Pubkey,
    user_wallet_address: &Pubkey,
) -> Result<ParsedTransactionInfo, Box<dyn Error>> {
    let meta = encoded_tx_with_meta.transaction.meta.as_ref();
    let success = meta.map_or(false, |m| m.err.is_none());
    let fee_lamports = meta.map_or(0, |m| m.fee);

    let involved_accounts: Vec<Pubkey> = match &encoded_tx_with_meta.transaction.transaction {
        solana_transaction_status::EncodedTransaction::Json(ui_transaction) => {
            match &ui_transaction.message {
                solana_transaction_status::UiMessage::Parsed(parsed_msg) => {
                    parsed_msg.account_keys.iter()
                        .map(|acc_key| Pubkey::from_str(&acc_key.pubkey))
                        .collect::<Result<Vec<Pubkey>, _>>()
                        .map_err(|e| format!("Failed to parse an account key in parsed message for tx {}: {}", signature, e))?
                }
                solana_transaction_status::UiMessage::Raw(raw_msg) => {
                    raw_msg.account_keys.iter()
                        .map(|key_str| Pubkey::from_str(key_str))
                        .collect::<Result<Vec<Pubkey>, _>>()
                        .map_err(|e| format!("Failed to parse an account key in raw message for tx {}: {}", signature, e))?
                }
            }
        }
        solana_transaction_status::EncodedTransaction::Binary(_encoded_data, _encoding) => {
            log::warn!("Received Binary encoded transaction for {}, account key parsing might be incomplete.", signature);
            Vec::new()
        }
        _ => {
            log::warn!("Received transaction with unexpected encoding for {}, account key parsing might be incomplete.", signature);
            Vec::new()
        }
    };

    let mut sol_transfers: Vec<SolTransferInfo> = Vec::new();
    if let Some(m) = meta {
        if involved_accounts.len() == m.pre_balances.len() && involved_accounts.len() == m.post_balances.len() {
            for i in 0..involved_accounts.len() {
                let pre_balance = m.pre_balances[i];
                let post_balance = m.post_balances[i];
                let account = involved_accounts[i];
                if post_balance < pre_balance {
                    sol_transfers.push(SolTransferInfo {
                        from_account: Some(account), to_account: None, amount_lamports: pre_balance - post_balance,
                    });
                } else if post_balance > pre_balance {
                    sol_transfers.push(SolTransferInfo {
                        from_account: None, to_account: Some(account), amount_lamports: post_balance - pre_balance,
                    });
                }
            }
        } else {
            log::warn!("Mismatch in account_keys ({}) and pre/post_balances ({}/{}) lengths for tx {}, cannot accurately parse SOL transfers from balances.",
                involved_accounts.len(), m.pre_balances.len(), m.post_balances.len(), signature);
        }
    }

    let mut token_transfers: Vec<TokenTransferInfo> = Vec::new();
    if let Some(m) = meta {
        // Directly use the OptionSerializer variants Some/None
        if let (solana_transaction_status::option_serializer::OptionSerializer::Some(pre_token_balances), solana_transaction_status::option_serializer::OptionSerializer::Some(post_token_balances)) = (&m.pre_token_balances, &m.post_token_balances) {
            let mut pre_map: HashMap<(Pubkey, Pubkey), (u64, u8, Option<Pubkey>)> = HashMap::new();
            for ptb in pre_token_balances.iter() {
                if ptb.account_index as usize >= involved_accounts.len() {
                    log::warn!("Tx {}: pre_token_balance account_index {} out of bounds for involved_accounts len {}", signature, ptb.account_index, involved_accounts.len());
                    continue;
                }
                let account_pubkey = involved_accounts[ptb.account_index as usize];
                let mint_pubkey = Pubkey::from_str(&ptb.mint).map_err(|e| format!("Tx {}: Invalid mint pubkey string '{}': {}", signature, ptb.mint, e))?;
                let amount = ptb.ui_token_amount.amount.parse::<u64>().map_err(|e| format!("Tx {}: Invalid token amount string '{}': {}", signature, ptb.ui_token_amount.amount, e))?;
                let decimals = ptb.ui_token_amount.decimals;
                let owner_pubkey_opt = ptb.owner.as_ref()
                    .map(|s| Pubkey::from_str(s).map_err(|e| format!("Tx {}: Invalid owner pubkey string '{}' in pre_token_balances: {}", signature, s, e)))
                    .transpose()?;
                pre_map.insert((account_pubkey, mint_pubkey), (amount, decimals, owner_pubkey_opt));
            }

            let mut post_map: HashMap<(Pubkey, Pubkey), (u64, u8, Option<Pubkey>)> = HashMap::new();
            for ptb in post_token_balances.iter() { // Iterate over &Vec
                if ptb.account_index as usize >= involved_accounts.len() {
                    log::warn!("Tx {}: post_token_balance account_index {} out of bounds for involved_accounts len {}", signature, ptb.account_index, involved_accounts.len());
                    continue;
                }
                let account_pubkey = involved_accounts[ptb.account_index as usize];
                let mint_pubkey = Pubkey::from_str(&ptb.mint).map_err(|e| format!("Tx {}: Invalid mint pubkey string '{}': {}", signature, ptb.mint, e))?;
                let amount = ptb.ui_token_amount.amount.parse::<u64>().map_err(|e| format!("Tx {}: Invalid token amount string '{}': {}", signature, ptb.ui_token_amount.amount, e))?;
                let decimals = ptb.ui_token_amount.decimals;
                let owner_pubkey_opt = ptb.owner.as_ref()
                    .map(|s| Pubkey::from_str(s).map_err(|e| format!("Tx {}: Invalid owner pubkey string '{}' in post_token_balances: {}", signature, s, e)))
                    .transpose()?;
                post_map.insert((account_pubkey, mint_pubkey), (amount, decimals, owner_pubkey_opt));
            }

            let mut all_token_accounts_mints: HashSet<(Pubkey, Pubkey)> = pre_map.keys().cloned().collect();
            all_token_accounts_mints.extend(post_map.keys().cloned());

            for (account, mint) in all_token_accounts_mints {
                let pre_b_info = pre_map.get(&(account, mint));
                let post_b_info = post_map.get(&(account, mint));
                let pre_amount = pre_b_info.map_or(0, |(amt, _, _)| *amt);
                let post_amount = post_b_info.map_or(0, |(amt, _, _)| *amt);

                if pre_amount == post_amount { continue; }

                let decimals = post_b_info.map(|(_, dec, _)| *dec).or_else(|| pre_b_info.map(|(_, dec, _)| *dec)).unwrap_or(0);
                let owner = post_b_info.and_then(|(_, _, own_opt)| *own_opt).or_else(|| pre_b_info.and_then(|(_, _, own_opt)| *own_opt));

                if post_amount < pre_amount {
                    token_transfers.push(TokenTransferInfo { mint, from_account: Some(account), to_account: None, from_owner: owner, to_owner: None, amount: pre_amount - post_amount, decimals });
                } else {
                    token_transfers.push(TokenTransferInfo { mint, from_account: None, to_account: Some(account), from_owner: None, to_owner: owner, amount: post_amount - pre_amount, decimals });
                }
            }
            if !token_transfers.is_empty() {
                log::debug!("Tx {}: Parsed {} token balance changes.", signature, token_transfers.len());
            }
        } else {
            log::debug!("Tx {}: No pre_token_balances or post_token_balances found in metadata.", signature);
        }
    } else {
        log::warn!("Tx {}: No metadata found, cannot parse token transfers.", signature);
    }

    let mut pnl_action: Option<PnlAction> = None;
    let mut main_transaction_type = TransactionType::Unknown;

    let mut user_received_tracked_token_flag = false;
    let mut user_sent_tracked_token_flag = false;
    let mut pnl_token_amount: u64 = 0;
    let mut pnl_token_decimals: u8 = 0;

    for tt_info in &token_transfers {
        if tt_info.mint == *token_mint_to_track {
            if tt_info.to_owner == Some(*user_wallet_address) && tt_info.to_account.is_some() {
                user_received_tracked_token_flag = true;
                pnl_token_amount = tt_info.amount;
                pnl_token_decimals = tt_info.decimals;
                break; // Assume one primary action for the tracked token per user per tx for PnlAction
            }
            if tt_info.from_owner == Some(*user_wallet_address) && tt_info.from_account.is_some() {
                user_sent_tracked_token_flag = true;
                pnl_token_amount = tt_info.amount;
                pnl_token_decimals = tt_info.decimals;
                break;
            }
        }
    }
    
    let net_sol_sent_by_user: u64 = sol_transfers.iter()
        .filter(|st| st.from_account == Some(*user_wallet_address))
        .map(|st| st.amount_lamports)
        .sum();
    let net_sol_received_by_user: u64 = sol_transfers.iter()
        .filter(|st| st.to_account == Some(*user_wallet_address))
        .map(|st| st.amount_lamports)
        .sum();

    if user_received_tracked_token_flag && net_sol_sent_by_user > 0 {
        pnl_action = Some(PnlAction::Buy {
            token_mint: *token_mint_to_track, token_amount: pnl_token_amount, token_decimals: pnl_token_decimals, cost_sol: net_sol_sent_by_user,
        });
        main_transaction_type = TransactionType::Swap;
    } else if user_sent_tracked_token_flag && net_sol_received_by_user > 0 {
        pnl_action = Some(PnlAction::Sell {
            token_mint: *token_mint_to_track, token_amount: pnl_token_amount, token_decimals: pnl_token_decimals, proceeds_sol: net_sol_received_by_user,
        });
        main_transaction_type = TransactionType::Swap;
    } else if user_received_tracked_token_flag {
        pnl_action = Some(PnlAction::Receive {
            token_mint: *token_mint_to_track, token_amount: pnl_token_amount, token_decimals: pnl_token_decimals,
        });
        main_transaction_type = TransactionType::TokenTransfer;
    } else if user_sent_tracked_token_flag {
        pnl_action = Some(PnlAction::Send {
            token_mint: *token_mint_to_track, token_amount: pnl_token_amount, token_decimals: pnl_token_decimals,
        });
        main_transaction_type = TransactionType::TokenTransfer;
    } else if net_sol_sent_by_user > 0 || net_sol_received_by_user > 0 {
        main_transaction_type = TransactionType::SolTransfer;
    } else if !token_transfers.is_empty() {
        main_transaction_type = TransactionType::TokenTransfer; // Generic token activity not directly PnL for user/tracked_token
    }
    // Instruction parsing would be needed for MintTo, CreateAccount etc.

    log::debug!("Finished parsing transaction {}: success={}, fee={}, type={:?}, pnl_action={:?}", signature, success, fee_lamports, main_transaction_type, pnl_action);

    Ok(ParsedTransactionInfo {
        signature: *signature,
        block_time: encoded_tx_with_meta.block_time,
        fee_lamports,
        involved_accounts,
        transaction_type: main_transaction_type,
        sol_transfers,
        token_transfers,
        success,
        pnl_relevant_action: pnl_action,
    })
}

// Helper function to check if the parsed transaction involves the token we are tracking
fn involves_token(parsed_info: &ParsedTransactionInfo, token_mint_to_track: &Pubkey) -> bool {
    // Primary check: Does any parsed token transfer involve the tracked mint?
    if parsed_info.token_transfers.iter().any(|t| t.mint == *token_mint_to_track) {
        log::debug!("Transaction {} involves token {} via token_transfers.", parsed_info.signature, token_mint_to_track);
        return true;
    }

    // Fallback: Check if the token_mint_to_track itself is an account key.
    // This might be relevant for minting operations or if the mint itself is an account in the tx.
    if parsed_info.involved_accounts.contains(token_mint_to_track) {
        log::debug!("Transaction {} involves token {} directly in its account keys (fallback check).", parsed_info.signature, token_mint_to_track);
        return true;
    }
    
    log::trace!("Transaction {} does not seem to involve token {} based on available parsed info.", parsed_info.signature, token_mint_to_track);
    false
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PnlSummary {
    pub token_mint: Option<Pubkey>,
    pub realized_pnl_sol: f64, // Can be negative
    pub remaining_tokens_held: u64,
    pub cost_basis_of_remaining_tokens_sol: f64,
    pub average_cost_of_remaining_tokens_sol_per_token: f64, // cost_basis / remaining_tokens
    pub total_sol_invested_in_buys: u64,
    pub total_sol_received_from_sells: u64,
    pub total_fees_paid_lamports: u64, // Sum of fees for PnL relevant transactions
    pub total_tokens_bought: u64,
    pub total_tokens_sold: u64,
    pub total_tokens_received_other: u64, // e.g. airdrops, transfers in
    pub total_tokens_sent_other: u64,     // e.g. transfers out
}

pub fn calculate_pnl(
    transactions: &[ParsedTransactionInfo],
    token_mint_to_track: &Pubkey,
) -> Result<PnlSummary, Box<dyn Error>> {
    if transactions.is_empty() {
        log::info!("No transactions provided for PnL calculation for token {}.", token_mint_to_track);
        return Ok(PnlSummary { token_mint: Some(*token_mint_to_track), ..Default::default() });
    }

    let mut sorted_transactions = transactions.to_vec();
    // Sort by block_time, oldest first. Transactions without block_time are problematic;
    // we might place them at the beginning or end, or filter them. For now, let's assume most have it.
    // Transactions with None block_time will be grouped together.
    sorted_transactions.sort_by_key(|t| t.block_time);

    let mut summary = PnlSummary { token_mint: Some(*token_mint_to_track), ..Default::default() };

    let mut current_tokens_held: u64 = 0;
    let mut current_total_cost_basis_sol: f64 = 0.0; // Use f64 for precision with division

    for tx_info in sorted_transactions.iter().filter(|t| t.success) {
        if let Some(action) = &tx_info.pnl_relevant_action {
            // Only add fee if the transaction was PNL relevant for the tracked token
            summary.total_fees_paid_lamports += tx_info.fee_lamports;

            match action {
                PnlAction::Buy { token_mint, token_amount, token_decimals: _, cost_sol } => {
                    if token_mint == token_mint_to_track {
                        summary.total_sol_invested_in_buys += *cost_sol;
                        summary.total_tokens_bought += *token_amount;

                        current_tokens_held += *token_amount;
                        current_total_cost_basis_sol += *cost_sol as f64;
                        log::debug!(
                            "PnL Calc (Buy): Tx {}, +{} tokens, +{} SOL cost. Held: {}, Cost Basis: {:.4}",
                            tx_info.signature, token_amount, cost_sol, current_tokens_held, current_total_cost_basis_sol
                        );
                    }
                }
                PnlAction::Sell { token_mint, token_amount, token_decimals: _, proceeds_sol } => {
                    if token_mint == token_mint_to_track {
                        summary.total_sol_received_from_sells += *proceeds_sol;
                        summary.total_tokens_sold += *token_amount;

                        if current_tokens_held > 0 && *token_amount > 0 {
                            let average_cost_per_token_at_sale = if current_tokens_held > 0 {
                                current_total_cost_basis_sol / current_tokens_held as f64
                            } else {
                                0.0 // Should not happen if selling tokens we hold
                            };
                            
                            let tokens_to_sell = std::cmp::min(*token_amount, current_tokens_held);
                            let cost_of_goods_sold = tokens_to_sell as f64 * average_cost_per_token_at_sale;
                            let pnl_for_this_sale = *proceeds_sol as f64 - cost_of_goods_sold;
                            summary.realized_pnl_sol += pnl_for_this_sale;

                            current_total_cost_basis_sol -= cost_of_goods_sold;
                            current_tokens_held -= tokens_to_sell;

                            if *token_amount > tokens_to_sell {
                                log::warn!(
                                    "PnL Calc (Sell): Tx {}, Attempted to sell {} tokens, but only held {}. PnL calculated on {}.",
                                    tx_info.signature, token_amount, tokens_to_sell, tokens_to_sell
                                );
                            }
                             log::debug!(
                                "PnL Calc (Sell): Tx {}, -{} tokens (cost {:.4}), +{} SOL proceeds. PnL: {:.4}. Held: {}, Remaining Cost Basis: {:.4}",
                                tx_info.signature, tokens_to_sell, cost_of_goods_sold, proceeds_sol, pnl_for_this_sale, current_tokens_held, current_total_cost_basis_sol
                            );
                        } else {
                            // Selling tokens we don't have a record of buying (or short selling - not handled)
                            // For now, treat proceeds as pure profit if no cost basis.
                            summary.realized_pnl_sol += *proceeds_sol as f64;
                            log::warn!(
                                "PnL Calc (Sell): Tx {}, Sold {} tokens with no prior recorded holdings or cost basis. Proceeds {} treated as PnL.",
                                tx_info.signature, token_amount, proceeds_sol
                            );
                        }
                    }
                }
                PnlAction::Receive { token_mint, token_amount, token_decimals: _ } => {
                    if token_mint == token_mint_to_track {
                        summary.total_tokens_received_other += *token_amount;
                        current_tokens_held += *token_amount;
                        // For 'Receive' (e.g., airdrop), cost basis impact is typically zero unless market value at receipt is used.
                        // For simplicity, we assume zero cost for received tokens here.
                        // If market value at receipt was known, current_total_cost_basis_sol would increase.
                        log::debug!(
                            "PnL Calc (Receive): Tx {}, +{} tokens. Held: {}, Cost Basis: {:.4} (unchanged by receive)",
                            tx_info.signature, token_amount, current_tokens_held, current_total_cost_basis_sol
                        );
                    }
                }
                PnlAction::Send { token_mint, token_amount, token_decimals: _ } => {
                     if token_mint == token_mint_to_track {
                        summary.total_tokens_sent_other += *token_amount;
                        if current_tokens_held > 0 && *token_amount > 0 {
                            let average_cost_per_token_at_send = if current_tokens_held > 0 {
                                current_total_cost_basis_sol / current_tokens_held as f64
                            } else {
                                0.0
                            };
                            let tokens_to_send = std::cmp::min(*token_amount, current_tokens_held);
                            let cost_basis_reduction = tokens_to_send as f64 * average_cost_per_token_at_send;
                            
                            current_total_cost_basis_sol -= cost_basis_reduction;
                            current_tokens_held -= tokens_to_send;
                            // Sending tokens is a disposition. It might be a non-taxable event (like moving to another own wallet)
                            // or could realize a loss/gain if considered a gift/donation at fair market value.
                            // For this PnL, we just reduce holdings and their cost basis. No PnL is realized here.
                            log::debug!(
                                "PnL Calc (Send): Tx {}, -{} tokens (cost basis reduction {:.4}). Held: {}, Remaining Cost Basis: {:.4}",
                                tx_info.signature, tokens_to_send, cost_basis_reduction, current_tokens_held, current_total_cost_basis_sol
                            );
                        } else {
                            log::warn!(
                                "PnL Calc (Send): Tx {}, Attempted to send {} tokens with no prior recorded holdings.",
                                tx_info.signature, token_amount
                            );
                        }
                    }
                }
            }
        }
    }

    summary.remaining_tokens_held = current_tokens_held;
    summary.cost_basis_of_remaining_tokens_sol = if current_total_cost_basis_sol > 0.0 { current_total_cost_basis_sol } else { 0.0 };

    if summary.remaining_tokens_held > 0 && summary.cost_basis_of_remaining_tokens_sol > 0.0 {
        summary.average_cost_of_remaining_tokens_sol_per_token =
            summary.cost_basis_of_remaining_tokens_sol / summary.remaining_tokens_held as f64;
    } else {
        summary.average_cost_of_remaining_tokens_sol_per_token = 0.0;
    }
    
    // Adjust realized PnL by total fees paid for PNL relevant transactions (fees are in lamports)
    // summary.realized_pnl_sol -= summary.total_fees_paid_lamports as f64 / 1_000_000_000.0; // Convert lamports to SOL

    log::info!("PnL Calculation Summary for token {}: {:?}", token_mint_to_track, summary);
    Ok(summary)
}