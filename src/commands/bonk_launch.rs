use anyhow::{anyhow, Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient as AsyncRpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
    commitment_config::CommitmentConfig,
};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use log::{info, error};
use std::str::FromStr;
use std::fs; // Added for reading file content

use crate::models::LaunchStatus; // Assuming LaunchStatus can be reused or adapted
use crate::models::LaunchParams; // Updated path
use crate::commands::utils; // For load_keypair_from_string
use crate::config; // For get_rpc_url
use spl_token::ID as TOKEN_PROGRAM_ID;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::{system_program, sysvar};
use borsh::{BorshDeserialize, BorshSerialize};
use sha2::{Sha256, Digest}; // Added for instruction discriminator

// Constants from Solscan and common knowledge
const RAYDIUM_LAUNCHPAD_PROGRAM_ID_STR: &str = "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj";
pub const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";
const METAPLEX_TOKEN_METADATA_PROGRAM_ID_STR: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";
// These might need to be fetched or are PDAs
const RAYDIUM_GLOBAL_CONFIG_STR: &str = "6s1xP3hpbAfFoNtUNF8mfHsjr2Bd97JxFJRWLbL6aHuX";
const RAYDIUM_PLATFORM_CONFIG_STR: &str = "FfYek5vEz23cMkWsdJwG2oa6EphsvXSHrGpdALN4g6W1";
const RAYDIUM_EVENT_AUTHORITY_STR: &str = "2DPAtwB8L12vrMRExbLuyGnC7n2J5LNoZQSejeQGpwkr"; // From Solscan logs and buy_exact_in


#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct MintParams {
    pub decimals: u8,
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct ConstantCurveData {
    pub supply: u64, // Total supply of base tokens for the curve
    pub total_base_sell: u64, // Amount of base tokens allocated for selling in the curve
    pub total_quote_fund_raising: u64, // Target amount of quote tokens to raise
    pub migrate_type: u8, // Type of migration, if any
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub enum CurveParamsData {
    Constant(ConstantCurveData),
    // Other curve types could be added here if supported
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct CurveParams {
    // This structure might need to match how Raydium expects it.
    // The Solscan shows `data: { Constant: { data: { ... } } }`
    // So, it might be an enum with a variant that holds the data.
    // For now, let's assume a direct struct or an enum wrapper.
    // Re-evaluating based on Solscan: it's an enum like structure.
    pub curve_type_data: CurveParamsData,
}


#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct VestingParams {
    pub total_locked_amount: u64,
    pub cliff_period: u64,
    pub unlock_period: u64,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct InitializeLaunchpadInstructionData {
    pub base_mint_param: MintParams,
    pub curve_param: CurveParams, // This was curve_param in Solscan, containing CurveParamsData
    pub vesting_param: VestingParams,
}

// RAYDIUM_LAUNCHPAD_PROGRAM_ID_STR is already defined at the top of the file (line 25).
// Removing this duplicate definition.

// Placeholder for Raydium Launchpad specific instruction building logic
// This will be expanded based on the Solscan transaction details.

pub async fn launch_on_raydium(
    params: LaunchParams,
    status_sender: UnboundedSender<LaunchStatus>,
) -> Result<(), anyhow::Error> {
    info!("Attempting to launch token on Raydium Launchpad (Bonk style): {}", params.name);
    let _ = status_sender.send(LaunchStatus::Log(format!("Raydium Launch: Starting for {}...", params.name)));

    let rpc_url = config::get_rpc_url();
    let rpc_client = Arc::new(AsyncRpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed()));

    let minter_keypair = match utils::load_keypair_from_string(&params.minter_private_key_str, "MinterKeypair") {
        Ok(kp) => kp,
        Err(e) => {
            let err_msg = format!("Failed to load minter keypair: {}", e);
            error!("{}", err_msg);
            let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
            return Err(anyhow!(err_msg));
        }
    };
    let _ = status_sender.send(LaunchStatus::Log(format!("Minter: {}", minter_keypair.pubkey())));

    let raydium_launchpad_program_id = Pubkey::from_str(RAYDIUM_LAUNCHPAD_PROGRAM_ID_STR)
        .map_err(|e| anyhow!("Invalid Raydium Launchpad Program ID: {}", e))?;
    let _ = status_sender.send(LaunchStatus::Log(format!("Raydium Launchpad Program ID: {}", raydium_launchpad_program_id)));

    let wsol_mint_pubkey = Pubkey::from_str(WSOL_MINT_STR).unwrap();
    let metaplex_metadata_program_id = Pubkey::from_str(METAPLEX_TOKEN_METADATA_PROGRAM_ID_STR).unwrap();
    let global_config_pubkey = Pubkey::from_str(RAYDIUM_GLOBAL_CONFIG_STR).unwrap();
    let platform_config_pubkey = Pubkey::from_str(RAYDIUM_PLATFORM_CONFIG_STR).unwrap();
    let event_authority_pubkey = Pubkey::from_str(RAYDIUM_EVENT_AUTHORITY_STR).unwrap();

    // 1. Determine Base Mint
    let base_mint_keypair = if params.mint_keypair_path_str.is_empty() {
        info!("Generating new keypair for base mint.");
        let _ = status_sender.send(LaunchStatus::Log("Generating new base mint keypair...".to_string()));
        Keypair::new()
    } else {
        info!("Loading base mint keypair from path: {}", params.mint_keypair_path_str);
        let _ = status_sender.send(LaunchStatus::Log(format!("Loading base mint keypair from {}...", params.mint_keypair_path_str)));
        match fs::read_to_string(&params.mint_keypair_path_str) {
            Ok(pk_str) => {
                match utils::load_keypair_from_string(pk_str.trim(), "BaseMintKeypairFromFile") {
                    Ok(kp) => kp,
                    Err(e) => {
                        let err_msg = format!("Failed to parse keypair from file {}: {}", params.mint_keypair_path_str, e);
                        error!("{}", err_msg);
                        let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
                        return Err(anyhow!(err_msg));
                    }
                }
            }
            Err(e) => {
                let err_msg = format!("Failed to read base mint keypair file {}: {}", params.mint_keypair_path_str, e);
                error!("{}", err_msg);
                let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
                return Err(anyhow!(err_msg));
            }
        }
    };
    let base_mint_pubkey = base_mint_keypair.pubkey();
    let _ = status_sender.send(LaunchStatus::Log(format!("Base Mint Pubkey: {}", base_mint_pubkey)));

    // Populate MintParams
    let mint_params = MintParams {
        decimals: 6, // Assuming 6 decimals for now, like HEDGE example. Could be from params.
        name: params.name.clone(),
        symbol: params.symbol.clone(),
        uri: params.image_url.clone(), // Using image_url as URI for metadata
    };

    // TODO: Populate CurveParams - This needs careful calculation based on params.dev_buy_sol, params.zombie_buy_sol, desired total supply etc.
    // For now, using placeholder values similar to HEDGE example.
    // Total supply of 1 billion tokens with 6 decimals = 1_000_000_000 * 10^6 = 1_000_000_000_000_000
    let example_total_supply_lamports = 1_000_000_000_000_000; // 1B tokens
    // Example: 80% for pool, 20% for something else (or all for pool initially)
    let example_total_base_sell_lamports = (example_total_supply_lamports as f64 * 0.8) as u64; // 80%
    // Example: Raise 10 SOL (10 * 10^9 lamports)
    let example_total_quote_fund_raising_lamports = solana_sdk::native_token::sol_to_lamports(10.0);


    let curve_params = CurveParams {
        curve_type_data: CurveParamsData::Constant(ConstantCurveData {
            supply: example_total_supply_lamports, // Placeholder
            total_base_sell: example_total_base_sell_lamports, // Placeholder
            total_quote_fund_raising: example_total_quote_fund_raising_lamports, // Placeholder
            migrate_type: 1, // As per HEDGE example
        })
    };

    // Populate VestingParams (all zeros in HEDGE example)
    let vesting_params = VestingParams {
        total_locked_amount: 0,
        cliff_period: 0,
        unlock_period: 0,
    };

    let initialize_instruction_data = InitializeLaunchpadInstructionData {
        base_mint_param: mint_params,
        curve_param: curve_params,
        vesting_param: vesting_params,
    };

    let mut instruction_data_args = match initialize_instruction_data.try_to_vec() {
        Ok(data) => data,
        Err(e) => {
            let err_msg = format!("Failed to serialize initialize instruction args: {}", e);
            error!("{}", err_msg);
            let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
            return Err(anyhow!(err_msg));
        }
    };
    let _ = status_sender.send(LaunchStatus::Log("Successfully serialized instruction arguments.".to_string()));

    // Use the exact 8-byte discriminator from Solscan's raw instruction data for "initialize"
    // Raw data starts with: afaf6d1f0d989bed...
    let discriminator: [u8; 8] = [0xaf, 0xaf, 0x6d, 0x1f, 0x0d, 0x98, 0x9b, 0xed];
    
    // The Sha256 hasher and related lines are no longer needed as we have the direct discriminator.
    // let mut hasher = Sha256::new();
    // hasher.update(b"initialize");
    // let sighash_bytes = hasher.finalize();
    // let old_discriminator = &sighash_bytes[0..8];

    let mut serialized_data = Vec::with_capacity(discriminator.len() + instruction_data_args.len());
    serialized_data.extend_from_slice(&discriminator); // Use the hardcoded discriminator
    serialized_data.extend_from_slice(&instruction_data_args);
    let _ = status_sender.send(LaunchStatus::Log(format!("Serialized instruction data with discriminator (len {}): {:?}", serialized_data.len(), serialized_data)));


    // 2. Build and send the `initialize` instruction for Raydium Launchpad.
    //    This involves:
    //    - Finding/deriving PDAs for pool state, base vault, quote vault, and Raydium Launchpad Authority.
    // Seeds from the provided IDL:
    let (launchpad_authority_pda, _authority_bump) = Pubkey::find_program_address(
        &[b"vault_auth_seed"], // Seed from IDL: [118,97,117,108,116,95,97,117,116,104,95,115,101,101,100]
        &raydium_launchpad_program_id
    );
    let _ = status_sender.send(LaunchStatus::Log(format!("Launchpad Authority PDA (from IDL): {}", launchpad_authority_pda)));

    let (pool_state_pda, _pool_state_bump) = Pubkey::find_program_address(
        &[b"pool", base_mint_pubkey.as_ref(), wsol_mint_pubkey.as_ref()], // Seeds from IDL
        &raydium_launchpad_program_id
    );
    let _ = status_sender.send(LaunchStatus::Log(format!("Pool State PDA (from IDL): {}", pool_state_pda)));
    
    let (base_vault_pda, _base_vault_bump) = Pubkey::find_program_address(
        &[b"pool_vault", pool_state_pda.as_ref(), base_mint_pubkey.as_ref()], // Seeds from IDL
        &raydium_launchpad_program_id
    );
    let _ = status_sender.send(LaunchStatus::Log(format!("Base Vault PDA (from IDL): {}", base_vault_pda)));

    let (quote_vault_pda, _quote_vault_bump) = Pubkey::find_program_address(
        &[b"pool_vault", pool_state_pda.as_ref(), wsol_mint_pubkey.as_ref()], // Seeds from IDL
        &raydium_launchpad_program_id
    );
     let _ = status_sender.send(LaunchStatus::Log(format!("Quote Vault PDA (from IDL): {}", quote_vault_pda)));

    // Derive Metaplex Metadata PDA for the base mint (this derivation is standard and confirmed by IDL structure for metadata_account)
    let (metadata_account_pda, _metadata_bump) = Pubkey::find_program_address(
        &[
            b"metadata".as_ref(),
            metaplex_metadata_program_id.as_ref(),
            base_mint_pubkey.as_ref(),
        ],
        &metaplex_metadata_program_id
    );
    let _ = status_sender.send(LaunchStatus::Log(format!("Metadata Account PDA (standard derivation): {}", metadata_account_pda)));


    // Assembling accounts for the `initialize` instruction based on Solscan and IDL:
    let accounts = vec![
        solana_sdk::instruction::AccountMeta::new(minter_keypair.pubkey(), true), // #1 Payer (also Creator)
        solana_sdk::instruction::AccountMeta::new(minter_keypair.pubkey(), true), // #2 Creator
        solana_sdk::instruction::AccountMeta::new_readonly(global_config_pubkey, false), // #3 Global Config
        solana_sdk::instruction::AccountMeta::new_readonly(platform_config_pubkey, false),// #4 Platform Config
        solana_sdk::instruction::AccountMeta::new_readonly(launchpad_authority_pda, false), // #5 Authority (PDA)
        solana_sdk::instruction::AccountMeta::new(pool_state_pda, false), // #6 Pool State (PDA, writable)
        solana_sdk::instruction::AccountMeta::new(base_mint_pubkey, true), // #7 Base Mint (Signer - our new mint)
        solana_sdk::instruction::AccountMeta::new_readonly(wsol_mint_pubkey, false), // #8 Quote Mint (WSOL)
        solana_sdk::instruction::AccountMeta::new(base_vault_pda, false),  // #9 Base Vault (PDA, writable)
        solana_sdk::instruction::AccountMeta::new(quote_vault_pda, false), // #10 Quote Vault (PDA, writable)
        solana_sdk::instruction::AccountMeta::new(metadata_account_pda, false), // #11 Metadata Account (PDA, writable)
        solana_sdk::instruction::AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),    // #12 Base Token Program
        solana_sdk::instruction::AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),    // #13 Quote Token Program
        solana_sdk::instruction::AccountMeta::new_readonly(metaplex_metadata_program_id, false), // #14 Metadata Program
        solana_sdk::instruction::AccountMeta::new_readonly(system_program::ID, false), // #15 System Program
        solana_sdk::instruction::AccountMeta::new_readonly(sysvar::rent::ID, false), // #16 Sysvar Rent
        solana_sdk::instruction::AccountMeta::new_readonly(event_authority_pubkey, false), // #17 Event Authority
        solana_sdk::instruction::AccountMeta::new_readonly(raydium_launchpad_program_id, false), // #18 The program itself (for potential self-CPI)
    ];
     let initialize_ix = solana_sdk::instruction::Instruction {
        program_id: raydium_launchpad_program_id,
        // Now passing 18 accounts
        accounts: accounts.to_vec(),
        data: serialized_data,
    };

    let mut instructions = vec![
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_000_000), // Generous limit for launch
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(params.priority_fee), // Use priority fee from params
        initialize_ix,
    ];

    let latest_blockhash = match rpc_client.get_latest_blockhash().await {
        Ok(bh) => bh,
        Err(e) => {
            let err_msg = format!("Failed to get latest blockhash: {}", e);
            error!("{}", err_msg);
            let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
            return Err(anyhow!(err_msg));
        }
    };
    let _ = status_sender.send(LaunchStatus::Log(format!("Latest blockhash: {}", latest_blockhash)));

    // Signers: minter_keypair (payer and creator) and base_mint_keypair (new mint being initialized)
    // The other new accounts (pool_state_keypair.pubkey(), etc.) are created by the program, paid for by minter.
    let signers = vec![&minter_keypair, &base_mint_keypair];

    let message: solana_sdk::message::Message = solana_sdk::message::Message::new_with_blockhash(
        &instructions,
        Some(&minter_keypair.pubkey()), // Payer
        &latest_blockhash,
    );
    let versioned_message = solana_sdk::message::VersionedMessage::Legacy(message);
    let versioned_tx = VersionedTransaction::try_new(versioned_message, &signers)
        .map_err(|e| anyhow!("Failed to create versioned transaction: {}", e))?;
    let _ = status_sender.send(LaunchStatus::Log("Initialize transaction created and signed.".to_string()));

    if params.simulate_only {
        let _ = status_sender.send(LaunchStatus::SimulatingTx("Initialize Transaction".to_string()));
        match rpc_client.simulate_transaction(&versioned_tx).await {
            Ok(sim_response) => {
                if sim_response.value.err.is_some() {
                    let sim_err = sim_response.value.err.unwrap().to_string();
                    let sim_logs = sim_response.value.logs.unwrap_or_default().join("\n");
                    error!("Simulation failed for initialize tx: {}\nLogs:\n{}", sim_err, sim_logs);
                    let _ = status_sender.send(LaunchStatus::SimulationFailed("Initialize Transaction".to_string(), format!("Error: {}. Logs: {}", sim_err, sim_logs)));
                    // For simulation, we might not want to hard return error, but report it.
                    // However, if init fails, subsequent steps are pointless.
                    return Err(anyhow!("Simulation failed for initialize tx: {}", sim_err));
                } else {
                    let sim_logs = sim_response.value.logs.unwrap_or_default().join("\n");
                    info!("Simulation successful for initialize tx.\nLogs:\n{}", sim_logs);
                    let _ = status_sender.send(LaunchStatus::SimulationSuccess("Initialize Transaction".to_string()));
                    // Continue with placeholder for dev buy simulation if applicable
                }
            }
            Err(e) => {
                error!("Failed to simulate initialize tx: {}", e);
                let _ = status_sender.send(LaunchStatus::Failure(format!("Failed to simulate initialize tx: {}", e)));
                return Err(anyhow!("Failed to simulate initialize tx: {}", e));
            }
        }
    } else {
        // Send and confirm the transaction
        let _ = status_sender.send(LaunchStatus::SubmittingBundle(1)); // Using SubmittingBundle for single tx too
        match utils::sign_and_send_versioned_transaction_with_config(
            rpc_client.as_ref(),
            versioned_tx,
            &signers,
            CommitmentConfig::confirmed(), // Or use a higher commitment if needed
            status_sender.clone(),
            "Initialize Raydium Pool".to_string()
        ).await {
            Ok(signature) => {
                info!("Initialize transaction successful. Signature: {}", signature);
                let _ = status_sender.send(LaunchStatus::BundleLanded(signature.to_string(), "Confirmed".to_string()));
                // Proceed to dev buy if applicable
            }
            Err(e) => {
                let err_msg = format!("Initialize transaction failed: {}", e);
                error!("{}", err_msg);
                let _ = status_sender.send(LaunchStatus::Failure(err_msg.clone()));
                return Err(anyhow!(err_msg));
            }
        }
    }


    // TODO: Implement dev buy (`buy_exact_in`) if params.dev_buy_sol > 0
    // TODO: Implement zombie buys if params.zombie_buy_sol > 0 and params.loaded_wallet_data is not empty

    // Final status update
    if params.simulate_only {
        let _ = status_sender.send(LaunchStatus::SimulatedOnly("Raydium launch (Initialize part) simulation complete.".to_string()));
        info!("Raydium Launch (Initialize part) SIMULATED for token: {}", params.name);
    } else {
        // This success message might be premature if dev/zombie buys are pending.
        // For now, it indicates the `initialize` part is done.
        let success_message = format!(
            "Raydium launch initialize for {} successful. Pool State: {}, Base Mint: {}. Link: TODO-SolscanLinkForPool",
            params.name, pool_state_pda, base_mint_pubkey // Changed pool_state_keypair.pubkey() to pool_state_pda
        );
        let _ = status_sender.send(LaunchStatus::Success(success_message));
        info!("Raydium Launch initialize for token: {} successful.", params.name);
    }

    Ok(())
}