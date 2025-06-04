use crate::errors::{PumpfunError, Result};
use log::{debug};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::native_token::LAMPORTS_PER_SOL;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use spl_token::state::{Account as TokenAccount, Mint};
use solana_sdk::program_pack::Pack;

/// Get SOL balance for a wallet
pub async fn get_sol_balance(rpc_client: &RpcClient, wallet_address: &Pubkey) -> Result<u64> {
    debug!("Getting SOL balance for address: {}", wallet_address);
    
    let balance = rpc_client.get_balance(wallet_address)
        .await
        .map_err(|e| PumpfunError::SolanaClient(e))?;
    
    debug!("SOL balance: {} lamports ({} SOL)", 
           balance, 
           balance as f64 / LAMPORTS_PER_SOL as f64);
    
    Ok(balance)
}

/// Get token balance for a wallet and CA
pub async fn get_token_balance(rpc_client: &RpcClient, wallet_address: &Pubkey, token_mint: &Pubkey) -> Result<u64> {
    debug!("Getting token balance for wallet: {} and mint: {}", wallet_address, token_mint);
    
    // Get the associated token account
    let token_account = get_associated_token_address(wallet_address, token_mint);
    
    // Check if the token account exists
    match rpc_client.get_account(&token_account).await {
        Ok(account) => {
            // Deserialize the token account data
            let token_account_data = TokenAccount::unpack(&account.data)
                .map_err(|e| PumpfunError::Token(format!("Failed to deserialize token account data: {}", e)))?;
            
            debug!("Token balance: {} tokens", token_account_data.amount);
            Ok(token_account_data.amount)
        },
        Err(_) => {
            // If the token account doesn't exist, the balance is 0
            debug!("Token account does not exist for this wallet. Balance is 0.");
            Ok(0)
        }
    }
}

/// Get the token decimals for a CA
pub async fn get_token_decimals(rpc_client: &RpcClient, token_mint: &Pubkey) -> Result<u8> {
    debug!("Getting token decimals for mint: {}", token_mint);
    
    let mint_account = rpc_client.get_account(token_mint)
        .await
        .map_err(|e| PumpfunError::SolanaClient(e))?;
    
    let mint_data = Mint::unpack(&mint_account.data)
        .map_err(|e| PumpfunError::Token(format!("Failed to deserialize mint data: {}", e)))?;
    
    debug!("Token decimals: {}", mint_data.decimals);
    Ok(mint_data.decimals)
}

/// Format a token amount with its decimals
pub fn format_token_amount(amount: u64, decimals: u8) -> String {
    if decimals == 0 {
        return amount.to_string();
    }
    
    let divisor = 10u64.pow(decimals as u32);
    let whole_part = amount / divisor;
    let fractional_part = amount % divisor;
    
    if fractional_part == 0 {
        return whole_part.to_string();
    }
    
    let fractional_str = format!("{:0width$}", fractional_part, width = decimals as usize);
    // Trim trailing zeros
    let trimmed = fractional_str.trim_end_matches('0');
    
    if trimmed.is_empty() {
        whole_part.to_string()
    } else {
        format!("{}.{}", whole_part, trimmed)
    }
}

/// Get token account for a wallet and CA
pub async fn get_token_account(rpc_client: &RpcClient, wallet_address: &Pubkey, token_mint: &Pubkey) -> Result<Pubkey> {
    let token_account = get_associated_token_address(wallet_address, token_mint);
    
    // Check if the token account exists
    if !check_token_account_exists(rpc_client, &token_account).await? {
        return Err(PumpfunError::Token(format!(
            "Token account does not exist for wallet: {} and mint: {}", 
            wallet_address, token_mint
        )));
    }
    
    Ok(token_account)
}

/// Check if a token account exists
pub async fn check_token_account_exists(rpc_client: &RpcClient, token_account: &Pubkey) -> Result<bool> {
    match rpc_client.get_account(token_account).await {
        Ok(_) => Ok(true),
        Err(_) => Ok(false)
    }
}

/// Check if a token exists on-chain
pub async fn check_token_exists(rpc_client: &RpcClient, token_mint: &Pubkey) -> Result<bool> {
    match rpc_client.get_account(token_mint).await {
        Ok(account) => {
            // Verify it's a CA account
            match Mint::unpack(&account.data) {
                Ok(_) => Ok(true),
                Err(_) => Ok(false) // Not a valid mint account
            }
        },
        Err(_) => Ok(false) // Account doesn't exist
    }
} 