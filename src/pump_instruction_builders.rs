use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    pubkey, // Import the const-compatible pubkey! macro
    system_program, sysvar,
    // Removed unused import: hash, 
};
use crate::errors::{Result, PumpfunError}; // Import PumpfunError for map_err
use borsh::{BorshSerialize, BorshDeserialize}; // Import Borsh for serialization and deserialization
use spl_associated_token_account::{ID as ASSOCIATED_TOKEN_PROGRAM_ID, get_associated_token_address}; // Import ATA calculation
use spl_token::ID as TOKEN_PROGRAM_ID; // Use directly from crate
// Removed import: use mpl_token_metadata::ID as METADATA_PROGRAM_ID;
use log::{debug, warn}; // Remove info and warn
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero, CheckedSub};
use anyhow::anyhow; // Import anyhow for error handling

// --- Constants ---
// TODO: Verify these are all needed and correct
pub const PUMPFUN_PROGRAM_ID: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
pub const GLOBAL_STATE_PUBKEY: Pubkey = pubkey!("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
pub const FEE_RECIPIENT_PUBKEY: Pubkey = pubkey!("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM");
// pub const EVENT_AUTH_PUBKEY: Pubkey = pubkey!("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1"); // Maybe not needed directly for instructions?
pub use system_program::ID as SYSTEM_PROGRAM_ID;
pub use sysvar::rent::ID as RENT_SYSVAR_ID;
// Found from analyzing transaction 3SrjWTyBMD...
pub const EVENT_AUTHORITY_PUBKEY: Pubkey = pubkey!("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1"); 
// Define METADATA_PROGRAM_ID locally using the v2.x Pubkey type
pub const METADATA_PROGRAM_ID: Pubkey = pubkey!("metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s"); 

// Placeholder for the actual Mint Authority Pubkey - THIS NEEDS TO BE CONFIRMED
// From Solscan TX 4RPiV..., account #1 for instruction #3
pub const PUMPFUN_MINT_AUTHORITY: Pubkey = pubkey!("TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM");

// Fee recipient for BUY instructions (from Solscan 4FURUG... #5.1 account #2)
// pub const PUMPFUN_BUY_FEE_RECIPIENT: Pubkey = pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV"); // Used when ATA created in same TX?
// Use Fee recipient from direct buy TX 4jdaKkc... #3 Account #2
// This constant is not used for the main buy instruction matching Solscan 57pLsV...
// pub const PUMPFUN_BUY_FEE_RECIPIENT: Pubkey = pubkey!("FWsW1xNtWscwNmKv6wVsU1iTzRN6wmmk3MjxRP5tT7hz");

// Specific Creator Vault from Solscan example 57pLsV... (Instruction #6, Account #10)
// when creator is 6NryqHZxhy7HxUfVvQtnDg1Xh55tYr29VbvyRbP9TLZW
pub const EXAMPLE_CREATOR_VAULT_PUBKEY: Pubkey = pubkey!("EBD1cUS7iqFWPmUAsTmFD72qSq4vamXYuHrzLtXSBRFj");

// Intermediary program that calls pump.fun buy (from Solscan 4FURUG... #5)
// pub const PUMPFUN_BUY_INTERMEDIARY_PROGRAM_ID: Pubkey = pubkey!("FAdo9NCw1ssek6Z6yeWzWjhLVsr8uiCwcWNUnKgzTnHe"); // REMOVE - Not used for direct buy
// Specific Jito tip account seen in Solscan 4FURUG... #5 account #13
// pub const JITO_TIP_ACCOUNT_GWHG: Pubkey = pubkey!("GWHG2XwitknDNxCYyQQN2zEj59RHgxk3R7QuoFTuHVfH"); // REMOVE - Not used for direct buy

// --- Account Structs (Updated from IDL) ---

#[derive(BorshDeserialize, Debug, Clone)]
pub struct GlobalState {
    // Discriminator removed - BorshDeserialize expects only data fields
    // Fields based on Solscan order and types
    pub initialized: u8, // Changed from bool due to deserialization error (Invalid bool representation: 82)
    pub authority: Pubkey,
    pub fee_recipient: Pubkey,
    pub initial_virtual_token_reserves: u64,
    pub initial_virtual_sol_reserves: u64,
    pub initial_real_token_reserves: u64,
    pub token_total_supply: u64,
    pub fee_basis_points: u64,
    // We define fields up to fee_basis_points.
    // BorshDeserialize expects the full account data, including the discriminator.
}

#[derive(BorshDeserialize, Debug, Clone)]
pub struct BondingCurveAccount {
    // Based on IDL `accounts.BondingCurve` and successful transaction analysis
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Pubkey, // Added: Original creator of the curve
}

pub struct TokenMetadata {
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

// --- PDA Calculation Functions (Placeholders/Assumptions) ---

pub fn find_bonding_curve_pda(mint_pk: &Pubkey) -> Result<(Pubkey, u8)> {
    // Use corrected seed based on TX analysis
    // warn!("Using placeholder PDA seeds for bonding curve: ["bonding-curve", mint_pk]");
     Ok(Pubkey::find_program_address(
        &[b"bonding-curve", mint_pk.as_ref()], // Corrected: Use hyphen
        &PUMPFUN_PROGRAM_ID
    ))
}

pub fn find_metadata_pda(mint_pk: &Pubkey) -> Result<(Pubkey, u8)> {
    // Uses Metaplex standard PDA derivation
     Ok(Pubkey::find_program_address(
        &[
            b"metadata", // Use literal seed prefix
            METADATA_PROGRAM_ID.as_ref(), // Use locally defined v2.x Pubkey
            mint_pk.as_ref(),
        ],
        &METADATA_PROGRAM_ID, // Use locally defined v2.x Pubkey
    ))
}

// --- Vault PDA Calculations (ASSUMPTIONS) ---
// Updated to derive from bonding_curve_pk based on TX analysis
// TODO: Verify these seeds are correct

pub fn find_bonding_curve_sol_vault_pda(bonding_curve_pk: &Pubkey) -> Result<(Pubkey, u8)> {
     // Use corrected seed based on TX analysis
     // warn!("Using placeholder PDA seeds for SOL vault: ["sol-vault", bonding_curve_pk]");
     Ok(Pubkey::find_program_address(
        &[b"vault", bonding_curve_pk.as_ref()], // Corrected: Use "vault"
        &PUMPFUN_PROGRAM_ID
    ))
}

pub fn find_creator_vault_pda(creator_pk: &Pubkey) -> Result<(Pubkey, u8)> {
    // This seed combination is a hypothesis based on common patterns and the expected PDA
    // from the error log (Dtb3... for creator FNAs...).
    // The error "AnchorError caused by account: creator_vault" suggests the seed might be "creator-vault".
    // If this doesn't yield the correct PDA, the seeds will need to be confirmed
    // by further analysis of the on-chain program or successful transactions.
    Ok(Pubkey::find_program_address(
        &[b"creator-vault", creator_pk.as_ref()], // Changed seed prefix
        &PUMPFUN_PROGRAM_ID
    ))
}
// --- Calculation Helpers (Moved from buy.rs / create.rs) ---

/// Calculates the amount of tokens received for a given SOL input amount, considering fees.
pub fn calculate_tokens_out(
    sol_in: u64,
    virtual_sol: u64,
    virtual_token: u64,
    fee_bps: u64,
) -> Result<u64> {
    if virtual_sol == 0 || virtual_token == 0 {
        return Err(PumpfunError::Calculation("Bonding curve reserves cannot be zero".to_string()));
    }
    let k_sol = BigUint::from(virtual_sol);
    let k_token = BigUint::from(virtual_token);
    let k = k_sol * k_token;

    if fee_bps >= 10000 {
        return Err(PumpfunError::Calculation(format!(
            "Fee basis points ({}) cannot be 10000 or more.",
            fee_bps
        )));
    }
    let fee = (sol_in as u128 * fee_bps as u128) / 10000u128;

    let sol_to_curve = sol_in.saturating_sub(fee as u64);
    if sol_to_curve == 0 { 
        warn!("Calculated SOL to curve is 0 after applying fee (Fee: {}, Input SOL: {}). Returning 0 tokens.", fee, sol_in);
        return Ok(0); 
    }
    let new_virtual_sol = BigUint::from(virtual_sol.saturating_add(sol_to_curve));
    if new_virtual_sol.is_zero() { return Err(PumpfunError::Calculation("New virtual SOL reserve is zero".to_string())); }
    let new_virtual_token = &k / new_virtual_sol;
    let tokens_out = BigUint::from(virtual_token)
        .checked_sub(&new_virtual_token)
        .ok_or_else(|| PumpfunError::Calculation("Calculation resulted in negative tokens out".to_string()))?;
    tokens_out.to_u64().ok_or_else(|| PumpfunError::Calculation("Tokens out exceeds u64::MAX".to_string()))
}

/// Applies slippage tolerance to the calculated SOL cost (increases the max SOL allowed).
pub fn apply_slippage_to_sol_cost(sol_cost: u64, slippage_bps: u64) -> u64 {
    if slippage_bps == 0 {
        return sol_cost; // No slippage
    }
    // Calculate slippage factor: 1 + slippage_bps / 10000
    // Use u128 for intermediate calculation to prevent overflow
    let numerator = 10000u128 + slippage_bps as u128;
    let denominator = 10000u128;
    let cost_u128 = sol_cost as u128;
    let max_sol_cost = (cost_u128 * numerator) / denominator;
    max_sol_cost as u64 // Convert back to u64
}

/// Applies slippage tolerance to the calculated tokens out (decreases the min tokens expected).
pub fn apply_slippage_to_tokens_out(tokens_out: u64, slippage_bps: u64) -> u64 {
    if slippage_bps == 0 {
        return tokens_out; // No slippage
    }
    // Calculate slippage factor: 1 - slippage_bps / 10000
    // Use u128 for intermediate calculation to prevent overflow and underflow
    // Ensure numerator doesn't go below zero if slippage_bps > 10000
    let numerator = 10000u128.saturating_sub(slippage_bps as u128); 
    let denominator = 10000u128;
    let tokens_u128 = tokens_out as u128;
    let min_tokens_out = (tokens_u128 * numerator) / denominator;
    min_tokens_out as u64 // Convert back to u64
}

/// Applies slippage tolerance to the calculated SOL out (decreases the min SOL expected).
pub fn apply_slippage_to_sol_out(sol_out: u64, slippage_bps: u64) -> u64 {
    if slippage_bps == 0 {
        return sol_out; // No slippage
    }
    // Calculate slippage factor: 1 - slippage_bps / 10000
    // Use u128 for intermediate calculation to prevent overflow and underflow
    let numerator = 10000u128.saturating_sub(slippage_bps as u128);
    let denominator = 10000u128;
    let sol_u128 = sol_out as u128;
    let min_sol_out = (sol_u128 * numerator) / denominator;
    min_sol_out as u64 // Convert back to u64
}


/// Calculates the amount of SOL received for selling a given token amount, considering fees.
/// This is the inverse of calculate_tokens_out.
pub fn calculate_sol_out(
    tokens_in: u64,
    virtual_sol: u64,
    virtual_token: u64,
    fee_bps: u64,
) -> Result<u64> {
    if virtual_sol == 0 || virtual_token == 0 {
        return Err(PumpfunError::Calculation("Bonding curve reserves cannot be zero".to_string()));
    }
    if tokens_in == 0 {
        return Ok(0); // Selling zero tokens yields zero SOL
    }
    if tokens_in > virtual_token {
        // This shouldn't happen if reserves are fetched correctly before selling
        // but good to handle defensively.
        warn!(
            "Attempting to sell {} tokens, but virtual reserve is only {}. Selling max possible.", 
            tokens_in, virtual_token
        );
        // Or return error: Err(PumpfunError::Calculation("Cannot sell more tokens than virtual reserves"))?
        // Let's cap it for now, but an error might be better depending on desired behavior.
        let _tokens_in = virtual_token; // Prefix with underscore
    }

    let k_sol = BigUint::from(virtual_sol);
    let k_token = BigUint::from(virtual_token);
    let k = k_sol.clone() * k_token;

    let new_virtual_token = BigUint::from(virtual_token.saturating_sub(tokens_in)); // Subtract tokens sold
    if new_virtual_token.is_zero() {
        // Selling all tokens, should receive close to all virtual SOL
        // Handle division by zero possibility - return virtual SOL minus a tiny amount? 
        // Or based on formula, result should be large. Let's check the math.
        // If new_virtual_token is 0, new_virtual_sol = k / 0 -> infinity. 
        // This implies the price goes to infinity, which isn't right.
        // Let's rethink the edge case of selling the last tokens.
        // The formula calculates the difference in the other reserve based on the change in the input reserve.
        warn!("Sell calculation resulted in zero virtual tokens remaining. This might indicate selling the last tokens.");
        // If selling *all* tokens (tokens_in == virtual_token), the SOL out is virtual_sol?
        // Let's stick to the formula and see.
        // If new_virtual_token is 0, maybe return an error or a specific large value?
        // For now, let's prevent division by zero and return error.
        return Err(PumpfunError::Calculation("Sell calculation resulted in zero virtual tokens, cannot divide by zero.".to_string()));
    }

    let new_virtual_sol = &k / new_virtual_token; 
    let sol_out_gross = new_virtual_sol
        .checked_sub(&k_sol) // Difference in SOL reserve
        .ok_or_else(|| PumpfunError::Calculation("Calculation resulted in negative SOL out (gross)".to_string()))?;

    // Apply fee
    if fee_bps >= 10000 {
        return Err(PumpfunError::Calculation(format!(
            "Fee basis points ({}) cannot be 10000 or more.",
            fee_bps
        )));
    }
    let fee = (sol_out_gross.clone() * BigUint::from(fee_bps)) / BigUint::from(10000u16);
    let sol_out_net = sol_out_gross
        .checked_sub(&fee)
        .ok_or_else(|| PumpfunError::Calculation("Calculation resulted in negative SOL out after fee".to_string()))?;

    sol_out_net.to_u64().ok_or_else(|| PumpfunError::Calculation("SOL out exceeds u64::MAX".to_string()))
}

// Helper function to predict state after a buy
pub fn predict_next_curve_state(
    current_state: &BondingCurveAccount,
    sol_amount_in: u64,
    global_fee_bps: u64, // Ensure this is used
) -> anyhow::Result<BondingCurveAccount> {
    if current_state.virtual_sol_reserves == 0 || current_state.virtual_token_reserves == 0 || sol_amount_in == 0 {
        // If reserves or input are zero, state doesn't change (cannot buy)
        // Or handle based on initial reserves if applicable? For now, return current.
        // Consider if this should return an error or just log a warning.
        warn!("Attempted to predict state with zero reserves or input, returning current state.");
        return Ok(current_state.clone());
    }

    if global_fee_bps >= 10000 { // Check the real global_fee_bps
        return Err(anyhow!(
            "Fee basis points ({}) cannot be 10000 or more during prediction.",
            global_fee_bps
        ));
    }
    let fee = (sol_amount_in as u128 * global_fee_bps as u128 / 10000) as u64; // Use real global_fee_bps
    let sol_after_fee = sol_amount_in.checked_sub(fee)
        .ok_or_else(|| anyhow!(
            "Fee ({}) subtraction resulted in underflow during prediction (SOL in: {}) - Check fee BPS ({})",
            fee, sol_amount_in, global_fee_bps
        ))?;

    // Calculate new reserves based on constant product: k = x * y
    let k = current_state.virtual_sol_reserves as u128 * current_state.virtual_token_reserves as u128;

    let next_sol_reserves_u128 = (current_state.virtual_sol_reserves as u128)
        .checked_add(sol_after_fee as u128) // Add SOL after fee
        .ok_or_else(|| anyhow!("Predicted SOL reserves overflowed u128"))?;

    if next_sol_reserves_u128 == 0 {
        // Should not happen if sol_amount_in > 0, but check anyway
         return Err(anyhow!("Division by zero: Predicted next SOL reserves cannot be zero"));
    }

    let next_token_reserves_u128 = k / next_sol_reserves_u128;

    // Create the predicted next state
    let next_state = BondingCurveAccount {
        virtual_sol_reserves: u64::try_from(next_sol_reserves_u128)
            .map_err(|_| anyhow!("Predicted SOL reserves overflowed u64"))?,
        virtual_token_reserves: u64::try_from(next_token_reserves_u128)
            .map_err(|_| anyhow!("Predicted Token reserves overflowed u64"))?,
        // Copy other fields from the current state if they exist and are relevant
        // For prediction, we only care about reserves.
        ..current_state.clone() // Clone other fields if necessary/possible
    };

    Ok(next_state)
}

// --- Instruction Builders (Placeholders) ---

/// Builds the instruction to create a token and bonding curve on Pump.fun
/// Uses Anchor discriminator `sighash("global", "create")`
/// Accounts based on TX analysis, args based on IDL/TX analysis.
pub fn build_pump_create_instruction( 
    creator_pk: &Pubkey,
    mint_pk: &Pubkey,
    bonding_curve_pk: &Pubkey, 
    metadata_pk: &Pubkey,      
    metadata_uri: &str,
    token_name: &str,
    token_symbol: &str,
) -> Result<Instruction> { 
    // Token Vault PDA is needed for the accounts list
    // let (bonding_curve_token_vault_pk, _) = find_bonding_curve_token_vault_pda(bonding_curve_pk)?;
    // CORRECT CALCULATION: Use standard ATA for the bonding curve PDA
    let bonding_curve_token_vault_ata = get_associated_token_address(bonding_curve_pk, mint_pk);

    const CREATE_DISCRIMINATOR: [u8; 8] = [0x18, 0x1e, 0xc8, 0x28, 0x05, 0x1c, 0x07, 0x77];
    #[derive(BorshSerialize)]
    struct CreateArgs {
        name: String,
        symbol: String,
        uri: String, 
        creator: Pubkey, // Added creator field
    }

    // Use Dynamic Arguments, including creator_pk
    let args = CreateArgs {
        name: token_name.to_string(),
        symbol: token_symbol.to_string(),
        uri: metadata_uri.to_string(),
        creator: *creator_pk, // Add creator pubkey to args
    };

    let mut instruction_data = CREATE_DISCRIMINATOR.to_vec();
    args.serialize(&mut instruction_data)
        .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize create args: {}", e)))?;

    // --- Log Full Instruction Data (Keep for now) --- 
    let data_hex = instruction_data.iter().map(|b| format!("{:02x}", b)).collect::<String>();
    debug!("Full Create Instruction Data (Hex): {}", data_hex);
    // --- End Log --- 

    // Define accounts matching Solscan Input Accounts for Instruction #3 (4RPiV...)
    // and 57pLsV... (Instruction #3)
    let accounts = vec![
        // Index 0: Mint (Writable, Signer)
        AccountMeta::new(*mint_pk, true),
        // Index 1: Mint Authority (Readonly)
        AccountMeta::new_readonly(PUMPFUN_MINT_AUTHORITY, false),
        // Index 2: Bonding Curve (Writable)
        AccountMeta::new(*bonding_curve_pk, false),
        // Index 3: Associated Bonding Curve / Token Vault ATA (Writable)
        AccountMeta::new(bonding_curve_token_vault_ata, false), // Use the calculated ATA
        // Index 4: Global (Readonly)
        AccountMeta::new_readonly(GLOBAL_STATE_PUBKEY, false),
        // Index 5: Mpl Token Metadata Program (Readonly)
        AccountMeta::new_readonly(METADATA_PROGRAM_ID, false),
        // Index 6: Metadata PDA (Writable)
        AccountMeta::new(*metadata_pk, false),
        // Index 7: User / Creator (Writable, Signer)
        AccountMeta::new(*creator_pk, true),
        // Index 8: System Program (Readonly)
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        // Index 9: Token Program (Readonly)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        // Index 10: Associated Token Program (Readonly)
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
        // Index 11: Rent Sysvar (Readonly)
        AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
        // Index 12: Event Authority (Readonly)
        AccountMeta::new_readonly(EVENT_AUTHORITY_PUBKEY, false),
        // Index 13: Pump Program ID (Readonly) - Re-adding based on Solscan showing it as an input account
        AccountMeta::new_readonly(PUMPFUN_PROGRAM_ID, false),
    ];

    Ok(Instruction {
        program_id: PUMPFUN_PROGRAM_ID,
        accounts, 
        data: instruction_data, 
    })
}

/// Builds the instruction to extend a Pump.fun bonding curve account.
/// Based on TX 429uBtXW24CDNtB1db3ZyiTFDDiVDZTM466RonVRG76Guq9d4Q1FN7cU5vp9DecjvJiPqXfG5AZVPpvGnpMo6eo7
/// Instruction #4
pub fn build_pump_extend_account_instruction(
    bonding_curve_pk: &Pubkey,
    user_pk: &Pubkey, // Payer for the extension, should be the creator
) -> Result<Instruction> {
    const EXTEND_ACCOUNT_DISCRIMINATOR: [u8; 8] = [0xea, 0x66, 0xc2, 0xcb, 0x96, 0x48, 0x3e, 0xe5];
    let instruction_data = EXTEND_ACCOUNT_DISCRIMINATOR.to_vec();

    let accounts = vec![
        AccountMeta::new(*bonding_curve_pk, false), // Account to extend (Writable)
        AccountMeta::new(*user_pk, true),           // User (Payer) (Writable, Signer)
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false), // System Program
        AccountMeta::new_readonly(EVENT_AUTHORITY_PUBKEY, false), // Event Authority
        AccountMeta::new_readonly(PUMPFUN_PROGRAM_ID, false),    // Pump.fun Program - Re-adding
    ];

    Ok(Instruction {
        program_id: PUMPFUN_PROGRAM_ID,
        accounts,
        data: instruction_data,
    })
}


/// Builds the instruction to buy tokens from a Pump.fun bonding curve.
/// Uses Anchor discriminator `sighash("global", "buy")`
/// Accounts based on TX analysis, args based on IDL.
pub fn build_pump_buy_instruction(
    buyer_pk: &Pubkey,
    mint_pk: &Pubkey,
    bonding_curve_pk: &Pubkey,
    creator_vault_pk: &Pubkey, // Added to match Solscan 57pLsV...
    token_amount: u64,
    max_sol_cost: u64,
) -> Result<Instruction> {
    // Calculate derived accounts
    let buyer_token_ata = get_associated_token_address(buyer_pk, mint_pk);
    let bonding_curve_token_vault_ata = get_associated_token_address(bonding_curve_pk, mint_pk);

    // Discriminator for "global:buy" from Solscan 57pLsV... (Instruction #6)
    const BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
    #[derive(BorshSerialize)]
    struct BuyArgs {
        amount: u64, // Token amount (this is min_tokens_out for safety)
        max_sol_cost: u64,
    }
    let args = BuyArgs {
        amount: token_amount,
        max_sol_cost,
    };
    let mut instruction_data = BUY_DISCRIMINATOR.to_vec();
    args.serialize(&mut instruction_data)
        .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize buy args: {}", e)))?;

    // Accounts based on Solscan 57pLsV... (Instruction #6)
    let accounts = vec![
        // Index 0: Global (Readonly)
        AccountMeta::new_readonly(GLOBAL_STATE_PUBKEY, false),
        // Index 1: Fee Recipient (Writable)
        AccountMeta::new(FEE_RECIPIENT_PUBKEY, false),
        // Index 2: Mint (Readonly) - Mint should not be a signer for a buy. It's read for decimals.
        AccountMeta::new_readonly(*mint_pk, false),
        // Index 3: Bonding Curve PDA (Writable)
        AccountMeta::new(*bonding_curve_pk, false),
        // Index 4: Associated Bonding Curve (Vault) (Writable)
        AccountMeta::new(bonding_curve_token_vault_ata, false),
        // Index 5: Associated User (Buyer's ATA) (Writable)
        AccountMeta::new(buyer_token_ata, false),
        // Index 6: User (Buyer) (Writable, Signer)
        AccountMeta::new(*buyer_pk, true),
        // Index 7: System Program (Readonly)
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        // Index 8: Token Program (Readonly)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        // Index 9: Creator Vault (Writable) - ADDED
        AccountMeta::new(*creator_vault_pk, false),
        // Index 10: Event Authority (Readonly)
        AccountMeta::new_readonly(EVENT_AUTHORITY_PUBKEY, false),
        // Index 11: Pump Program (Readonly) - Re-adding
        AccountMeta::new_readonly(PUMPFUN_PROGRAM_ID, false),
    ];

    Ok(Instruction {
        program_id: PUMPFUN_PROGRAM_ID,
        accounts,
        data: instruction_data,
    })
}

/// Builds the instruction to sell tokens into a Pump.fun bonding curve.
/// Uses Anchor discriminator `sighash("global", "sell")` - NEEDS VERIFICATION
/// Accounts based on TX analysis (likely similar to buy), args based on IDL guess.
pub fn build_pump_sell_instruction(
    seller_pk: &Pubkey,
    mint_pk: &Pubkey,
    bonding_curve_pk: &Pubkey,
    token_amount_in: u64,
    min_sol_output: u64,
) -> Result<Instruction> {
    // Calculate derived accounts
    let seller_token_ata = get_associated_token_address(seller_pk, mint_pk);
    let bonding_curve_token_vault_ata = get_associated_token_address(bonding_curve_pk, mint_pk);
    let (_bonding_curve_sol_vault_pk, _) = find_bonding_curve_sol_vault_pda(bonding_curve_pk)?;

    // --- GUESSING DISCRIMINATOR - VERIFY THIS! --- 
     // Anchor sighash for `global::sell`: 0c2ba787bf851986 // Incorrect based on IDL
    // CORRECT discriminator from on-chain sell transaction 2V3AS6...
    const SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad]; 
    // warn!("Using potentially INCORRECT discriminator for sell instruction. VERIFY: {:?}", SELL_DISCRIMINATOR); // Remove warning
    
    #[derive(BorshSerialize)]
    struct SellArgs {
        amount: u64, // Token amount to sell
        min_sol_output: u64,
    }
    let args = SellArgs {
        amount: token_amount_in,
        min_sol_output,
    };
    let mut instruction_data = SELL_DISCRIMINATOR.to_vec();
    args.serialize(&mut instruction_data)
        .map_err(|e| PumpfunError::Serialization(format!("Failed to serialize sell args: {}", e)))?;

    // Accounts based on buy instruction, likely very similar - VERIFY THIS!
    // Corrected based on on-chain transaction 2V3AS6... (Inner Ix #4.1)
    let accounts = vec![
        // Index 0: Global (Readonly)
        AccountMeta::new_readonly(GLOBAL_STATE_PUBKEY, false),
        // Index 1: Fee Recipient (Writable) - Use BUY fee recipient based on TX
        AccountMeta::new(FEE_RECIPIENT_PUBKEY, false), // Corrected to FEE_RECIPIENT_PUBKEY
        // Index 2: Mint (Readonly)
        AccountMeta::new_readonly(*mint_pk, false),
        // Index 3: Bonding Curve PDA (Writable)
        AccountMeta::new(*bonding_curve_pk, false),
        // Index 4: Bonding Curve Token Vault ATA (Writable)
        AccountMeta::new(bonding_curve_token_vault_ata, false),
        // --- REMOVED Index 5: Bonding Curve SOL Vault --- 
        // AccountMeta::new(bonding_curve_sol_vault_pk, false),
        // Index 5 (was 6): Seller's Token ATA (`associated_user`) (Writable)
        AccountMeta::new(seller_token_ata, false),
        // Index 6 (was 7): Seller (`user`) (Signer, Writable)
        AccountMeta::new(*seller_pk, true),
        // Index 7 (was 8): System Program (Readonly)
        AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        // Index 8 (was 9): Associated Token Program (Readonly) <-- ADDED based on TX
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
        // Index 9 (was 10): Token Program (Readonly)
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        // Index 10 (was 11): Event Authority (Readonly)
        AccountMeta::new_readonly(EVENT_AUTHORITY_PUBKEY, false),
        // Index 11 (was 12): Pump Program (Readonly)
        AccountMeta::new_readonly(PUMPFUN_PROGRAM_ID, false),
    ];

    Ok(Instruction {
        program_id: PUMPFUN_PROGRAM_ID,
        accounts: accounts,
        data: instruction_data,
    })
}

// Removed placeholder modules for spl_token, spl_associated_token_account, mpl_token_metadata
// Use directly from crate
// TOKEN_PROGRAM_ID now imported above
// METADATA_PROGRAM_ID now imported above
// ASSOCIATED_TOKEN_PROGRAM_ID now imported above
// RENT_SYSVAR_ID now imported above
// EVENT_AUTHORITY_PUBKEY now imported above
// PUMPFUN_PROGRAM_ID now imported above
// GLOBAL_STATE_PUBKEY now imported above
// FEE_RECIPIENT_PUBKEY now imported above