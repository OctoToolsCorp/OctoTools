mod config;
mod errors;
mod models;
mod utils;
mod wallet;
mod api;
mod commands;
mod pump_instruction_builders;

use anyhow::{Result, Context, anyhow};
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use env_logger::Env;
use tokio::sync::mpsc::unbounded_channel;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use commands::pump::PumpStatus;
use models::token::TokenConfig;
use std::env;
use log::{info, error};
use solana_sdk::signature::Keypair;
use crate::wallet::load_zombie_wallets_from_file; // Use only this for now
use solana_sdk::native_token::lamports_to_sol;
// Import local models instead of from the crate
use crate::models::wallet::WalletInfo;
use crate::models::LaunchStatus;
use solana_sdk::signer::Signer;
use solana_sdk::signer::keypair::write_keypair_file;
use serde_json;
use bs58;

use solana_pumpfun_token::models::settings::AppSettings;
use crate::commands::{
    create::create_token,
    buy::buy_token,
    sell::sell_token,
    simulate::simulate_launch,
    gather::gather_sol,
    check::check_wallets,
    alt::create_lookup_table_command,
    setup_test::{setup_test_wallets_command, SetupTestWalletsArgs},
    atomic_buy::atomic_buy,
    launch_buy::launch_buy,
    precalc::precalc_alt_addresses,
    check_bundle::check_bundle_status,
    pump::pump_token,
};
// Comment out the missing import for now
// use crate::wallet::wallet_generate_key;

#[derive(Parser, Debug)]
#[command(
    name = "solana-pumpfun-token",
    author = "Your Name <your.email@example.com>",
    version,
    about = "Create and manage tokens on the Solana blockchain using pump.fun",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Optional: Path to the JSON file containing wallet keypairs.
    #[arg(long, global = true, default_value = "./keys.json")]
    keys_path: String,
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum CliExchange {
    Pumpfun,
    Raydium,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a new token on pump.fun
    Create {
        /// Token name
        #[arg(short, long)]
        name: String,
        /// Token symbol
        #[arg(short, long)]
        symbol: String,
        /// Token description
        #[arg(long)]
        description: Option<String>,
        /// Token image URL or path
        #[arg(long)]
        image: Option<String>,
        /// Initial dev buy amount (in TOKENS)
        #[arg(long, default_value_t = 1000000.0)]
        dev_amount: f64,
        /// Token decimals
        #[arg(short, long, default_value_t = 6)]
        decimals: u8,
        /// Submit as a *consolidated* bundle (create+buys+tip) to Jito
        #[arg(long)]
        jito_bundle: bool,
        /// Amount of TOKENS each zombie wallet should buy (Required for --jito-bundle)
        #[arg(long, requires = "jito_bundle")]
        zombie_amount: Option<u64>,
    },
    /// Buy tokens from pump.fun pool
    Buy {
        /// Token mint address
        #[arg(short, long)]
        mint: String,
        /// Amount of SOL to spend
        #[arg(short, long)]
        amount: f64,
        /// Slippage tolerance percentage
        #[arg(long, default_value_t = 5)]
        slippage: u8,
        /// Priority fee (SOL)
        #[arg(long, default_value_t = 0.00005)]
        priority_fee: f64,
        /// Submit as a consolidated bundle (buy+tip) to Jito
        #[arg(long)]
        jito_bundle: bool,
    },
    /// Sell tokens from your wallet
    Sell {
        /// Token mint address
        #[arg(short, long)]
        mint: String,
        /// Percentage of tokens to sell (0-100)
        #[arg(short, long, default_value_t = 100.0)]
        percentage: f64,
        /// Specific wallets to sell from (comma separated addresses)
        #[arg(long)]
        wallets: Option<String>,
        /// Sell all tokens from all wallets
        #[arg(long)]
        all: bool,
        /// Sell using all zombie wallets defined in keys.json
        #[arg(long, default_value = "false")]
        all_zombies: bool,
    },
    /// Launch a token with full zombie wallet coordination
    Launch {
        /// Number of zombie wallets to use
        #[arg(short, long, default_value_t = 15)]
        wallet_count: usize,
        /// Dev wallet percentage
        #[arg(long, default_value_t = 3.0)]
        dev_percent: f64,
        /// Minimum buy percentage per wallet
        #[arg(long, default_value_t = 20.0)]
        min_percent: f64,
        /// Maximum buy percentage per wallet
        #[arg(long, default_value_t = 20.0)]
        max_percent: f64,
        /// Jito tip amount (SOL)
        #[arg(long, default_value_t = 0.002)]
        jito_tip: f64,
    },
    /// Simulate a token launch without actually performing transactions
    Simulate {
        /// Number of zombie wallets to use
        #[arg(short, long, default_value_t = 15)]
        wallet_count: usize,
        /// Dev wallet percentage
        #[arg(long, default_value_t = 3.0)]
        dev_percent: f64,
        /// Minimum buy percentage per wallet
        #[arg(long, default_value_t = 20.0)]
        min_percent: f64,
        /// Maximum buy percentage per wallet
        #[arg(long, default_value_t = 20.0)]
        max_percent: f64,
    },
    /// Manage zombie wallets (DEPRECATED: Use generate/check/gather)
    Wallets {
        /// Create new zombie wallets
        #[arg(long)]
        create: Option<usize>,
        /// List all wallets
        #[arg(long)]
        list: bool,
    },
    /// [EXPERIMENTAL] Atomic Buy: Buy a token using all zombie wallets via Jito bundle and ALT
    AtomicBuy {
        /// Token mint address
        #[arg(short, long)]
        mint: String,
        /// Amount of SOL to spend per wallet
        #[arg(short, long)]
        amount: f64,
        /// Slippage tolerance in basis points (BPS)
        #[arg(long, default_value_t = 200)] // 2%
        slippage_bps: u64,
        /// Priority fee per transaction in lamports
        #[arg(long, default_value_t = 100000)] // 0.0001 SOL
        priority_fee: u64,
        /// Optional: Address Lookup Table (ALT) to use
        #[arg(long)]
        lookup_table: Option<String>,
        /// Optional: Jito tip for the entire bundle in SOL.
        /// For Pump.fun, this tip is applied per transaction.
        /// For Raydium, this is an overall tip for the bundle.
        #[arg(long)]
        jito_tip: Option<f64>,
        /// Exchange to use for the buy (pumpfun or raydium)
        #[arg(long, value_enum, default_value_t = CliExchange::Pumpfun)]
        exchange: CliExchange,
    },
    /// Gather SOL from zombie wallets to parent wallet
    Gather,
    /// Check wallet balances
    Check,
    /// Create or manage Address Lookup Tables (ALTs).
    #[clap(subcommand)]
    Alt(AltCommands),
    /// [TESTING] Setup wallets: create zombie wallets and request Testnet airdrops
    SetupTest(SetupTestWalletsArgs),
    /// [EXPERIMENTAL] Launch & Buy: Creates a token, performs a dev buy, then an atomic zombie buy bundle
    LaunchBuy {
        /// Token name
        #[arg(long)]
        name: String,
        /// Token symbol (ticker)
        #[arg(long)]
        symbol: String,
        /// Token description
        #[arg(long)]
        description: String,
        /// Token image URL (must be HTTPS)
        #[arg(long)]
        image_url: String,
        /// Amount of SOL the developer/payer wallet should buy initially
        #[arg(long)]
        dev_buy_sol: f64,
        /// Amount of SOL EACH zombie wallet should buy in the atomic bundle
        #[arg(long)]
        zombie_buy_sol: f64,
        /// Slippage tolerance in basis points (e.g., 500 for 5%) for ALL buys
        #[arg(long, default_value_t = 500)]
        slippage_bps: u64,
        /// Priority fee in lamports for the Jito bundle containing zombie buys
        #[arg(long, default_value_t = 100_000)]
        priority_fee: u64,
        /// Address of the Address Lookup Table (ALT) to use for the zombie bundle
        #[arg(long)]
        lookup_table: String, // Note: This is String, not Option<String> like in AtomicBuy
        /// Optional: Path to a pre-generated mint keypair file. If not provided, a new one is generated.
        #[arg(long)]
        mint_keypair_path: Option<String>,
        /// Optional: Run through build/simulation logic but do not send the final bundle.
        #[arg(long)]
        simulate_only: bool,
        /// Optional: Jito tip for the bundle in SOL (defaults to value from settings or 0.001 if not set)
        #[arg(long)]
        jito_tip_sol: Option<f64>,
    },
    /// Generate a new Solana keypair and save it to a JSON file (compatible with --mint-keypair-path)
    GenerateMintKeypair {
        /// Path to save the generated keypair file (defaults to ./mint_keypair.json)
        #[arg(long, short)]
        outfile: Option<String>,
    },
    /// Check the status of one or more Jito bundles
    CheckBundle(CheckBundleArgs),
    /// Buy a token and immediately create multiple sell orders bundled via Jito
    Pump {
        /// Token mint address
        #[arg(short, long)]
        mint: String,
        /// Amount of SOL to spend on the initial buy
        #[arg(long)]
        buy_amount: f64,
        /// Maximum SOL value desired per sell transaction (e.g., 0.01)
        #[arg(long, default_value_t = 0.01)]
        sell_threshold: f64,
        /// Slippage tolerance in basis points for the buy (e.g., 500 for 5%)
        #[arg(long, default_value_t = 500)]
        slippage_bps: u64,
        /// Priority fee per transaction in lamports for the Jito bundle
        #[arg(long, default_value_t = 10000)]
        priority_fee: u64,
        /// Optional: Jito tip amount in lamports
        #[arg(long)]
        jito_tip: Option<u64>,
        /// Optional: Address of the Address Lookup Table (ALT) to use if needed for bundling
        #[arg(long)]
        lookup_table: Option<String>,
        /// Optional: Path to a specific JSON keypair file to use for this pump operation.
        /// Uses the FIRST keypair in the file as the payer/operator.
        #[arg(long)]
        pump_keys_path: Option<String>,
        /// Optional: Provide the private key directly (e.g., base58 string).
        /// Takes precedence over --pump-keys-path and global --keys-path.
        /// Can also be set via PUMP_PRIVATE_KEY environment variable.
        #[arg(long, env = "PUMP_PRIVATE_KEY")]
        private_key: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AltCommands {
    /// Create a new ALT, add addresses from a specified file (or default ./addresesforalt.json), and freeze it.
    Create {
        /// Optional: Path to the JSON file containing an array of address strings to add.
        #[clap(long)]
        addresses_file: Option<String>,
    },
    /// Pre-calculate addresses needed for an ALT for a given mint and output to stdout or file.
    PrecalcAlt {
        /// Path to the mint keypair file (e.g., thecoin.json).
        #[clap(long)]
        mint_keypair_path: String,
        /// Optional: Path to the JSON file containing zombie wallets (defaults to global --keys-path).
        #[clap(long)]
        zombie_keys_path: Option<String>,
        /// Optional: Output file path. If not specified, prints JSON to stdout.
        #[clap(long)]
        outfile: Option<String>,
    },
}

// Define arguments for the new subcommand
#[derive(Parser, Debug)]
struct CheckBundleArgs {
    /// One or more Jito Bundle IDs to check status for (max 5 per request)
    #[clap(required = true, num_args = 1..=5)]
    bundle_ids: Vec<String>,

    /// Optional: Use getBundleStatuses (looks back further) instead of getInflightBundleStatuses (last 5 mins)
    #[clap(long)]
    historical: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize environment
    dotenv().ok();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    
    // Parse command line arguments
    let cli = Cli::parse();
    
    let keys_path_arg = &cli.keys_path;

    info!("Using keys file path: {}", keys_path_arg);
    
    info!("Starting solana-pumpfun-token");
    
    // Execute command
    match cli.command {
        Commands::Create { 
            name, 
            symbol, 
            description, 
            image, 
            dev_amount, 
            decimals, 
            jito_bundle, 
            zombie_amount,
        } => {
            let zombie_buy_token_amount = if jito_bundle {
                zombie_amount.context("--zombie-amount (token amount) is required when using --jito-bundle")?
            } else {
                0u64 // Use u64 zero
            };

            let token_config = TokenConfig {
                name,
                symbol,
                description: description.or_else(|| env::var("TOKEN_DESCRIPTION").ok()).unwrap_or_default(),
                image_url: image.or_else(|| env::var("TOKEN_IMAGE_PATH").ok()),
                initial_buy_amount: dev_amount, 
                decimals,
            };
            
            create_token(token_config, jito_bundle, zombie_buy_token_amount, Some(keys_path_arg)).await
                .context("Failed to create token")?;
        },
        
        Commands::Buy { mint, amount, slippage, priority_fee, jito_bundle } => {
            buy_token(&mint, amount, slippage, priority_fee, jito_bundle, Some(keys_path_arg)).await.context("Failed to buy token")?;
        },
        
        Commands::Sell { mint, percentage, wallets: _, all: _, all_zombies } => {
            sell_token(
                mint.clone(),
                Some(percentage),
                None,
                None,
                None,
                Some(keys_path_arg),
                all_zombies
            ).await.context("Failed to sell token")?;
        },
        
        Commands::Wallets { create, list } => {
            println!("Warning: The 'wallets' command is deprecated. Use 'generate', 'check', or 'gather'.");
            if create.is_some() {
                println!(" To create wallets, please use the 'generate' command.");
            } else if list {
                println!(" To list wallets, please use the 'check' command or view the keys file directly.");
            } else {
                 println!("Use --create <count> or --list with the wallets command (DEPRECATED).");
            }
        },
        
        Commands::Simulate { wallet_count, dev_percent, min_percent, max_percent } => {
            simulate_launch(wallet_count, dev_percent, min_percent, max_percent).await.context("Failed to simulate launch")?;
        },
        
        Commands::Launch { 
            wallet_count: _, // Prefix unused
            dev_percent: _, // Prefix unused
            min_percent: _, // Prefix unused
            max_percent: _, // Prefix unused
            jito_tip: _, // Prefix unused
         } => {
            // Placeholder for launch logic
            println!("Launch command received (implementation pending).");
            // launch::launch_token(wallet_count, dev_percent, min_percent, max_percent, jito_tip).await.context("Failed to launch token")?;
        },
        
        Commands::Gather => {
            gather_sol(Some(keys_path_arg)).await.context("Failed to gather SOL")?;
        },
        
        Commands::Check => {
            check_wallets(Some(keys_path_arg)).await.context("Failed to check wallets")?;
        },
        
        Commands::SetupTest(args) => {
            setup_test_wallets_command(args).await.context("Failed to setup test wallets")?;
        },

        Commands::AtomicBuy {
            mint,
            amount,
            slippage_bps,
            priority_fee,
            lookup_table,
            jito_tip,
            exchange, // Add the new exchange argument
        } => {
            info!("Executing AtomicBuy command for exchange: {:?}...", exchange);

            let keypairs = load_zombie_wallets_from_file(Some(&cli.keys_path))
                .with_context(|| format!("Failed to load zombie keypairs from {}", &cli.keys_path))?;

            if keypairs.is_empty() {
                return Err(anyhow!("No suitable keypairs found in {} for AtomicBuy (after filtering).", &cli.keys_path));
            }
            
            let participating_wallets_with_amounts: Vec<(Keypair, f64)> = keypairs
                .into_iter()
                .map(|kp| (kp, amount)) // `amount` is f64 from CLI args
                .collect();

            let jito_tip_sol_bundle = jito_tip.unwrap_or_else(|| lamports_to_sol(config::CONFIG.jito_tip_lamports));
            let jito_block_engine_url = config::CONFIG.jito_block_engine_url.clone();
            
            // For CLI, let's use the first known Jito tip account as a default.
            // This array is also defined in `atomic_buy.rs`. For consistency, it could be moved to `config.rs`.
            const DEFAULT_CLI_JITO_TIP_ACCOUNT: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";
            let jito_tip_account_cli = DEFAULT_CLI_JITO_TIP_ACCOUNT.to_string();

            info!("Calling atomic_buy with mint: {}, num_wallets: {}, slippage_bps: {}, priority_fee: {}, lookup_table: {:?}, jito_tip_sol: {}, jito_be: {}, jito_tip_acc: {}",
                mint, participating_wallets_with_amounts.len(), slippage_bps, priority_fee, lookup_table, jito_tip_sol_bundle, jito_block_engine_url, jito_tip_account_cli);

            let exchange_enum = match exchange {
                CliExchange::Raydium => commands::atomic_buy::Exchange::Raydium,
                CliExchange::Pumpfun => commands::atomic_buy::Exchange::PumpFun,
            };

            atomic_buy(
                exchange_enum, // Pass the parsed exchange
                mint,
                participating_wallets_with_amounts,
                slippage_bps,
                priority_fee,
                lookup_table,
                jito_tip_sol_bundle, // This will be used as jito_tip_sol_per_tx for PumpFun
                jito_tip_sol_bundle, // This will be used as jito_overall_tip_sol for Raydium (or 0.0 if specific logic is desired)
                jito_block_engine_url,
                jito_tip_account_cli,
            ).await?;
        },

        Commands::LaunchBuy {
            name,
            symbol,
            description,
            image_url,
            dev_buy_sol,
            zombie_buy_sol,
            slippage_bps,
            priority_fee, // This is main_tx_priority_fee_micro_lamports
            lookup_table,
            mint_keypair_path,
            simulate_only,
            jito_tip_sol, // Destructure the new optional arg
        } => {
            info!("Executing Launch & Buy command...");

            // Load settings to get minter key and default jito tip
            let settings = AppSettings::load();
            let minter_private_key_str = settings.dev_wallet_private_key.clone();
            if minter_private_key_str.is_empty() {
                 return Err(anyhow!("minter_wallet_private_key is not set in app_settings.json. Please set it via the GUI or manually."));
            }

            // Load zombie wallets using the provided or default path
            let zombie_wallet_data = load_zombie_wallets_from_file(Some(keys_path_arg))?;

            // Convert Vec<Keypair> to Vec<WalletInfo>
            let zombie_wallet_info_vec: Vec<models::wallet::WalletInfo> = zombie_wallet_data
                .iter()
                .map(|kp| models::wallet::WalletInfo {
                    public_key: kp.pubkey().to_string(),
                    private_key: bs58::encode(kp.to_bytes()).into_string(),
                    name: None,
                })
                .collect();

            // Create channel for status updates
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<models::LaunchStatus>();

            tokio::spawn(async move {
                while let Some(status) = receiver.recv().await {
                    info!("Launch Status: {:?}", status);
                }
            });

            // Determine the actual Jito tip to use
            // Use CLI arg if provided, otherwise use default from AppSettings
            let actual_jito_tip_sol = jito_tip_sol.unwrap_or(settings.default_jito_actual_tip_sol);

            launch_buy(
                name,
                symbol,
                description,
                image_url,
                dev_buy_sol,
                zombie_buy_sol,
                slippage_bps,
                lookup_table,
                mint_keypair_path,
                minter_private_key_str,
                zombie_wallet_info_vec,
                simulate_only,
                sender,
                settings.selected_jito_block_engine_url.clone(), // Use Jito URL from settings
                priority_fee, // This is main_tx_priority_fee_micro_lamports from CLI args
                actual_jito_tip_sol, // Use the determined actual_jito_tip_sol
            ).await?;
        },

        Commands::GenerateMintKeypair { outfile } => { 
            info!("Generating new keypair...");
            let new_keypair = Keypair::new();
            let pubkey = new_keypair.pubkey();
            
            let output_path = outfile.unwrap_or_else(|| "./mint_keypair.json".to_string());
            
            write_keypair_file(&new_keypair, &output_path)
                .map_err(|e| anyhow!("Failed to write keypair file '{}': {}", output_path, e))
                .context("Keypair generation failed during file write.")?;

            info!("✅ Successfully generated keypair!");
            info!("   Public Key: {}", pubkey);
            info!("   Saved to: {}", output_path);
            info!("   You can use this file with the --mint-keypair-path argument.");
        },

        Commands::CheckBundle(args) => {
            check_bundle_status(args.bundle_ids, args.historical).await?
        },

        Commands::Alt(alt_command) => match alt_command {
            AltCommands::Create { addresses_file } => {
                commands::alt::create_lookup_table_command(addresses_file).await?
            }
            AltCommands::PrecalcAlt { mint_keypair_path, zombie_keys_path, outfile } => {
                info!("Executing Precalc ALT Addresses command...");
                let actual_keys_path = zombie_keys_path.as_deref().unwrap_or(keys_path_arg);
                let addresses = commands::precalc::precalc_alt_addresses(&mint_keypair_path, Some(actual_keys_path))
                    .await
                    .context("Failed to pre-calculate ALT addresses")?;

                info!("Pre-calculation complete. Outputting addresses...");
                let output_string = serde_json::to_string_pretty(
                    &addresses.iter().map(|pk| pk.to_string()).collect::<Vec<_>>()
                )
                .context("Failed to serialize addresses to JSON")?;

                match outfile {
                    Some(path) => {
                        use std::io::Write;
                        let mut file = std::fs::File::create(&path)
                            .context(format!("Failed to create output file: {}", path))?;
                        file.write_all(output_string.as_bytes())
                            .context(format!("Failed to write to output file: {}", path))?;
                        info!("Addresses written to file: {}", path);
                    }
                    None => {
                        println!("{}", output_string);
                    }
                }
            }
        },

        Commands::Pump {
            mint,
            buy_amount,
            sell_threshold,
            slippage_bps,
            priority_fee,
            jito_tip,
            lookup_table,
            pump_keys_path,
            private_key,
        } => {
            info!("Executing Pump command...");
            // Setup status channel and running flag for pump_token
            let (status_sender, mut status_receiver) = unbounded_channel::<PumpStatus>();
            let is_running_flag = Arc::new(AtomicBool::new(true));
            // Spawn a task to print status updates
            tokio::spawn(async move {
                while let Some(status) = status_receiver.recv().await {
                    println!("Pump status: {:?}", status);
                }
            });
            info!("Starting pump loop (3s delay between attempts)...");
            loop {
                info!("Running pump iteration...");
                match commands::pump::pump_token(
                    mint.clone(),
                    buy_amount,
                    sell_threshold,
                    slippage_bps,
                    priority_fee,
                    jito_tip,
                    lookup_table.clone(),
                    Some(keys_path_arg),
                    pump_keys_path.clone(),
                    private_key.clone(),
                    is_running_flag.clone(),
                    status_sender.clone(),
                ).await {
                    Ok(_) => info!("Pump iteration completed successfully."),
                    Err(e) => error!("Pump iteration failed: {:?}", e),
                }

                info!("Waiting 3 seconds before next iteration...");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        },
    }
    
    info!("Command completed successfully.");
    Ok(())
}
