// src/api/raydium_launchpad.rs

use anyhow::{Result, Context, anyhow};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer; // Added Signer trait
// Unused: use solana_sdk::transaction::VersionedTransaction;
// Unused: use reqwest::Client;
// Unused: use serde_json::Value as JsonValue;
use log::{info, error}; // Removed warn, debug
use solana_client::nonblocking::rpc_client::RpcClient; // Added
use std::sync::Arc; // Added
use borsh::BorshDeserialize; // Added

use crate::api::raydium_launchpad_layouts::LaunchpadPoolLayout; // Added
use crate::errors::PumpfunError; // Assuming PumpfunError can be used or a new error type

// NATIVE_SOL_MINT is already in scope via crate::api::raydium, but being explicit here if needed
// use crate::api::raydium::NATIVE_SOL_MINT;
use std::str::FromStr;


// Seeds from raydium-sdk-V2/src/raydium/launchpad/pda.ts
const LAUNCHPAD_AUTH_SEED: &[u8] = b"vault_auth_seed";
const LAUNCHPAD_POOL_SEED: &[u8] = b"pool";
// const LAUNCHPAD_POOL_VAULT_SEED: &[u8] = b"pool_vault"; // Vaults are read from pool state
const CPI_EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";


#[derive(Debug, Clone)]
pub struct RaydiumLaunchpadKeys {
    // Base
    pub program_id: Pubkey,         // Launchpad Program ID
    pub id: Pubkey,                 // Pool State (AMM ID)
    // mintA: Pubkey, // Base token mint (e.g., GROK) - will be target_token_mint
    // mintB: Pubkey, // Quote token mint (e.g., WSOL) - will be NATIVE_SOL_MINT
    pub vault_a: Pubkey,            // Base token vault for the pool
    pub vault_b: Pubkey,            // Quote token vault for the pool
    
    pub authority: Pubkey,          // Pool authority
    
    // Market specific (mimicking _Market from SDK for Launchpad, though not all might be used by Launchpad IX)
    // These are often for an associated OpenBook market if the Launchpad pool uses one.
    // The Solscan for GROK Launchpad buy_exact_in doesn't list all these as direct inputs to *that* instruction,
    // but they are part of a complete Raydium pool key structure.
    pub open_orders: Pubkey,
    pub target_orders: Pubkey,
    pub market_program_id: Pubkey,
    pub market_id: Pubkey,
    pub market_authority: Pubkey,
    pub market_base_vault: Pubkey,
    pub market_quote_vault: Pubkey,
    pub market_bids: Pubkey,
    pub market_asks: Pubkey,
    pub market_event_queue: Pubkey,

    // Additional accounts from Solscan for Launchpad program instruction
    pub global_config: Pubkey,
    pub platform_config: Pubkey,
    pub event_authority_lp: Pubkey, // Specific to Launchpad events, renamed to avoid clash
}


// Dynamically fetches Raydium Launchpad market info
pub async fn get_launchpad_market_info(
    rpc_client: &Arc<RpcClient>,
    launchpad_program_id: &Pubkey, // e.g., LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj
    target_token_mint: &Pubkey,    // This is mintA
    quote_token_mint: &Pubkey,     // This is mintB (e.g., WSOL)
) -> Result<Option<RaydiumLaunchpadKeys>> {
    info!(
        "Attempting to dynamically fetch Raydium Launchpad info for target_mint: {}, quote_mint: {}, program_id: {}",
        target_token_mint, quote_token_mint, launchpad_program_id
    );

    // 1. Derive PDAs
    let (pool_id_pda, _pool_bump) = Pubkey::find_program_address(
        &[
            LAUNCHPAD_POOL_SEED,
            &target_token_mint.to_bytes(),
            &quote_token_mint.to_bytes(),
        ],
        launchpad_program_id,
    );
    info!("Derived Pool ID PDA: {}", pool_id_pda);

    let (authority_pda, _auth_bump) = Pubkey::find_program_address(
        &[LAUNCHPAD_AUTH_SEED],
        launchpad_program_id,
    );
    info!("Derived Authority PDA: {}", authority_pda);
    
    let (event_authority_lp_pda, _event_auth_bump) = Pubkey::find_program_address(
        &[CPI_EVENT_AUTHORITY_SEED],
        launchpad_program_id,
    );
    info!("Derived Event Authority PDA: {}", event_authority_lp_pda);

    // 2. Fetch and Deserialize LaunchpadPool Account
    let pool_account_data = rpc_client.get_account_data(&pool_id_pda).await
        .context(format!("Failed to fetch LaunchpadPool account data for PDA: {}", pool_id_pda))?;
    
    let launchpad_pool_layout = LaunchpadPoolLayout::try_from_slice(&pool_account_data)
        .map_err(|e| PumpfunError::Deserialization(format!("Failed to deserialize LaunchpadPoolLayout for {}: {}", pool_id_pda, e)))?;
    info!("Successfully deserialized LaunchpadPoolLayout for {}: {:?}", pool_id_pda, launchpad_pool_layout);

    // Verify mints
    if launchpad_pool_layout.mint_a != *target_token_mint {
        return Err(anyhow!(PumpfunError::Invariant(format!(
            "Mismatch: Expected mint_a {} but found {} in fetched pool data for {}",
            target_token_mint, launchpad_pool_layout.mint_a, pool_id_pda
        ))).into());
    }
    if launchpad_pool_layout.mint_b != *quote_token_mint {
         return Err(anyhow!(PumpfunError::Invariant(format!(
            "Mismatch: Expected mint_b {} but found {} in fetched pool data for {}",
            quote_token_mint, launchpad_pool_layout.mint_b, pool_id_pda
        ))).into());
    }

    // 3. Populate RaydiumLaunchpadKeys
    // For OpenBook market keys, we'll use Pubkey::default() for now as they are not directly
    // used by the buy_exact_in instruction based on SDK and Solscan.
    // A more complete solution might involve fetching the LaunchpadConfig (using launchpad_pool_layout.config_id)
    // and checking if it references an OpenBook market.
    let dummy_pubkey = Pubkey::default();
    // Assuming srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX is the standard OpenBook DEX program ID
    let open_book_dex_program_id = Pubkey::from_str("srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX")
        .map_err(|e| anyhow!("Failed to parse OpenBook DEX program ID: {}", e))?;


    Ok(Some(RaydiumLaunchpadKeys {
        program_id: *launchpad_program_id,
        id: pool_id_pda, // This is the Pool State ID
        vault_a: launchpad_pool_layout.vault_a,
        vault_b: launchpad_pool_layout.vault_b,
        authority: authority_pda, // This is the Launchpad Auth PDA
        global_config: launchpad_pool_layout.config_id, // From deserialized pool state
        platform_config: launchpad_pool_layout.platform_id, // From deserialized pool state
        event_authority_lp: event_authority_lp_pda, // Derived CPI Event Authority

        // OpenBook market related keys - using defaults for now
        open_orders: dummy_pubkey,
        target_orders: dummy_pubkey,
        market_program_id: open_book_dex_program_id, // Standard OpenBook program
        market_id: dummy_pubkey,
        market_authority: dummy_pubkey,
        market_base_vault: dummy_pubkey,
        market_quote_vault: dummy_pubkey,
        market_bids: dummy_pubkey,
        market_asks: dummy_pubkey,
        market_event_queue: dummy_pubkey,
    }))
}

// function to build launchpad buy transaction
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    // message::{v0::Message as MessageV0, VersionedMessage}, // Unused
    compute_budget::ComputeBudgetInstruction,
    // system_program, // No longer adding System Program here based on SDK
    // sysvar::{rent::ID as SYSVAR_RENT_ID}, // No longer adding Rent Sysvar here based on SDK
};
// use borsh::{BorshSerialize}; // Unused

// Instruction data for Raydium Launchpad buy_exact_in
// Commenting out the struct as we will construct data manually
// #[derive(BorshSerialize, Debug)]
// pub struct BuyExactInInstructionData {
//     pub amount_in: u64,
//     pub minimum_amount_out: u64,
//     pub share_fee_rate: u64,
// }

pub async fn build_raydium_launchpad_buy_transaction(
    payer_keypair: &Keypair,
    market_keys: &RaydiumLaunchpadKeys,
    input_token_mint: &Pubkey, // e.g., WSOL
    output_token_mint: &Pubkey, // Token to buy (e.g., GROK)
    amount_in: u64,
    min_amount_out: u64,
    priority_fee_micro_lamports: Option<u64>,
) -> Result<Vec<Instruction>> {
    
    let payer_pubkey = Signer::pubkey(payer_keypair);

    // User's ATAs
    let user_quote_token_ata = spl_associated_token_account::get_associated_token_address(&payer_pubkey, input_token_mint); // WSOL ATA
    let user_base_token_ata = spl_associated_token_account::get_associated_token_address(&payer_pubkey, output_token_mint);  // GROK ATA

    // Discriminator for buy_exact_in from Solscan: faea0d7bd59c13ec
    let discriminator: [u8; 8] = [0xfa, 0xea, 0x0d, 0x7b, 0xd5, 0x9c, 0x13, 0xec];
    let amount_in_bytes = amount_in.to_le_bytes();
    let min_amount_out_bytes = min_amount_out.to_le_bytes();
    let share_fee_rate_bytes = (0u64).to_le_bytes(); // Assuming 0 as per Solscan

    let mut instruction_data_bytes = Vec::new();
    instruction_data_bytes.extend_from_slice(&discriminator);
    instruction_data_bytes.extend_from_slice(&amount_in_bytes);
    instruction_data_bytes.extend_from_slice(&min_amount_out_bytes);
    instruction_data_bytes.extend_from_slice(&share_fee_rate_bytes);

    let accounts = vec![
        AccountMeta::new(payer_pubkey, true),                                 // #1 Payer (signer)
        AccountMeta::new_readonly(market_keys.authority, false),              // #2 Authority (Launchpad Pool Authority)
        AccountMeta::new_readonly(market_keys.global_config, false),          // #3 Global Config
        AccountMeta::new_readonly(market_keys.platform_config, false),        // #4 Platform Config
        AccountMeta::new(market_keys.id, false),                              // #5 Pool State (writable, not signer by payer)
        AccountMeta::new(user_base_token_ata, false),                         // #6 User Base Token (GROK ATA, writable, not signer by payer)
        AccountMeta::new(user_quote_token_ata, false),                        // #7 User Quote Token (WSOL ATA, writable, not signer by payer)
        AccountMeta::new(market_keys.vault_a, false),                         // #8 Base Vault (GROK Pool Vault, writable, not signer by payer)
        AccountMeta::new(market_keys.vault_b, false),                         // #9 Quote Vault (WSOL Pool Vault, writable, not signer by payer)
        AccountMeta::new_readonly(*output_token_mint, false),                 // #10 Base Token Mint (GROK)
        AccountMeta::new_readonly(*input_token_mint, false),                  // #11 Quote Token Mint (WSOL)
        AccountMeta::new_readonly(spl_token::id(), false),                    // #12 Base Token Program
        AccountMeta::new_readonly(spl_token::id(), false),                    // #13 Quote Token Program
        AccountMeta::new_readonly(market_keys.event_authority_lp, false),     // #14 Event Authority (Launchpad specific)
        // Account #15: The Launchpad Program ID itself, as per SDK.
        AccountMeta::new_readonly(market_keys.program_id, false),             // #15 Launchpad Program ID
    ];

    // Ensure we have exactly 15 accounts as per SDK (excluding optional shareFeeReceiver)
    if accounts.len() != 15 {
        // This is a developer sanity check.
        error!("Developer Error: Expected 15 accounts for Raydium Launchpad buy_exact_in, got {}. Check implementation against SDK.", accounts.len());
        // Potentially return an error here or panic, as this indicates a mismatch with the SDK's known structure.
        // For now, will proceed, but this should be investigated if the error persists.
    }

    let mut instructions = Vec::new();
    if let Some(priority_fee) = priority_fee_micro_lamports {
        if priority_fee > 0 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(priority_fee));
        }
    }
    // Add a placeholder CU limit; actual limit might need to be determined by simulation or from Launchpad docs
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(400_000)); // Placeholder CU limit

    instructions.push(Instruction {
        program_id: market_keys.program_id, // Launchpad Program ID
        accounts,
        data: instruction_data_bytes, // Use manually constructed data
    });

    // The blockhash and ALTs would typically be passed in or fetched here.
    // For now, this function will just return the instruction set,
    // and atomic_buy will handle blockhash and transaction creation.
    // This is a temporary simplification.
    // To make this function fully testable standalone, it would need rpc_client and active_lookup_tables.
    // However, to integrate into atomic_buy, it's better if atomic_buy handles the final tx creation.

    // Return the collected instructions
    Ok(instructions)
}
// Based on SDK's sellExactInInstruction and Solscan transaction 4k6Bmk5jb78XhhB63neh4uyGHuywRwGfJQQLC5QaCG5RewU79HdxggrMfYLduYW8YU66ifnFA4CdZNVFpJ2DPrGr
pub async fn build_raydium_launchpad_sell_transaction(
    payer_keypair: &Keypair,
    market_keys: &RaydiumLaunchpadKeys,
    input_token_mint: &Pubkey,  // Token to sell (e.g., GROK) - This is Base Token
    output_token_mint: &Pubkey, // Token to receive (e.g., WSOL) - This is Quote Token
    amount_in_tokens: u64,      // Amount of input_token_mint (Base Token) to sell
    min_amount_out: u64,        // Minimum amount of output_token_mint (Quote Token) to receive
    priority_fee_micro_lamports: Option<u64>,
) -> Result<Vec<Instruction>> {
    let payer_pubkey = Signer::pubkey(payer_keypair);

    // User's ATAs
    // User is sending from their input_token_mint ATA (Base Token ATA)
    let user_base_token_ata = spl_associated_token_account::get_associated_token_address(&payer_pubkey, input_token_mint);
    // User is receiving to their output_token_mint ATA (Quote Token ATA, e.g., WSOL ATA)
    let user_quote_token_ata = spl_associated_token_account::get_associated_token_address(&payer_pubkey, output_token_mint);

    // Discriminator for sell_exact_in from SDK instrument.ts: anchorDataBuf.sellExactIn
    // export const anchorDataBuf = { sellExactIn: Buffer.from([149, 39, 222, 155, 211, 124, 152, 26]), ... }
    let discriminator: [u8; 8] = [149, 39, 222, 155, 211, 124, 152, 26];

    // Parameters for sell_exact_in: amount_in (of base token), minimum_amount_out (of quote token), share_fee_rate
    let amount_in_bytes = amount_in_tokens.to_le_bytes();
    let min_amount_out_bytes = min_amount_out.to_le_bytes();
    let share_fee_rate_bytes = (0u64).to_le_bytes(); // Assuming 0 for now, as per buy and Solscan sell example

    let mut instruction_data_bytes = Vec::new();
    instruction_data_bytes.extend_from_slice(&discriminator);
    instruction_data_bytes.extend_from_slice(&amount_in_bytes);
    instruction_data_bytes.extend_from_slice(&min_amount_out_bytes);
    instruction_data_bytes.extend_from_slice(&share_fee_rate_bytes);

    // Accounts are identical to buy_exact_in as per Solscan and SDK structure for this program
    let accounts = vec![
        AccountMeta::new(payer_pubkey, true),                                // #1 Payer (signer)
        AccountMeta::new_readonly(market_keys.authority, false),             // #2 Authority (Launchpad Pool Authority)
        AccountMeta::new_readonly(market_keys.global_config, false),         // #3 Global Config
        AccountMeta::new_readonly(market_keys.platform_config, false),       // #4 Platform Config
        AccountMeta::new(market_keys.id, false),                             // #5 Pool State (writable)
        AccountMeta::new(user_base_token_ata, false),                        // #6 User Base Token (Input Token ATA, e.g., GROK, writable)
        AccountMeta::new(user_quote_token_ata, false),                       // #7 User Quote Token (Output Token ATA, e.g., WSOL, writable)
        AccountMeta::new(market_keys.vault_a, false),                        // #8 Base Vault (GROK Pool Vault, writable)
        AccountMeta::new(market_keys.vault_b, false),                        // #9 Quote Vault (WSOL Pool Vault, writable)
        AccountMeta::new_readonly(*input_token_mint, false),                 // #10 Base Token Mint (e.g., GROK)
        AccountMeta::new_readonly(*output_token_mint, false),                // #11 Quote Token Mint (e.g., WSOL)
        AccountMeta::new_readonly(spl_token::id(), false),                   // #12 Base Token Program (for input_token_mint)
        AccountMeta::new_readonly(spl_token::id(), false),                   // #13 Quote Token Program (for output_token_mint)
        AccountMeta::new_readonly(market_keys.event_authority_lp, false),    // #14 Event Authority
        AccountMeta::new_readonly(market_keys.program_id, false),            // #15 Launchpad Program ID
    ];

    if accounts.len() != 15 {
        // This is a developer sanity check.
        error!("Developer Error: Expected 15 accounts for Raydium Launchpad sell_exact_in, got {}. Check implementation.", accounts.len());
        // Consider returning an error if strictness is required.
    }

    let mut instructions = Vec::new();
    if let Some(priority_fee) = priority_fee_micro_lamports {
        if priority_fee > 0 {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_price(priority_fee));
        }
    }
    // Placeholder CU limit, adjust as needed based on simulation/docs for sell
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(400_000)); 

    instructions.push(Instruction {
        program_id: market_keys.program_id,
        accounts,
        data: instruction_data_bytes,
    });

    Ok(instructions)
}