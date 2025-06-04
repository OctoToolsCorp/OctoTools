pub mod simulation; // Added simulation module
// src/lib.rs

use anchor_lang::prelude::*;

// Declare only the program ID. IDL will be loaded at runtime by clients.
declare_id!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

// NOTE: All the #[derive(Accounts)], #[account], #[event], etc. structs
// previously here have been removed as they belong in the on-chain program code,
// not in the client library. 

pub mod api;
pub mod commands;
pub mod config;
pub mod errors;
pub mod models;
pub mod utils;
pub mod wallet;
pub mod pump_instruction_builders;
pub mod gui;
pub mod key_utils;

// Re-export key types or functions if this library is meant to be used by others
// Example:
// pub use errors::PumpfunError;
// pub use models::token::TokenConfig;
// pub use commands::create::create_token; // etc.

// Optional: You might want a prelude for convenience
// pub mod prelude {
//     pub use super::errors::*;
//     // ... other common exports
// } 