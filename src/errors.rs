use thiserror::Error;
// use anchor_lang::error::AnchorError;
// use anchor_client::ClientError as AnchorClientError; // Keep commented if unused
use solana_client::client_error::ClientError;
use solana_sdk::pubkey::ParsePubkeyError;
use solana_sdk::program_error::ProgramError;
use solana_sdk::signature::SignerError;
use solana_sdk::message::CompileError; // Added for MessageV0::try_compile
// Remove unused http import
// use http;

#[derive(Error, Debug)]
pub enum PumpfunError {
    #[error("Environment error: {0}")]
    Environment(String),

    #[error("Wallet error: {0}")]
    Wallet(String),
    
    #[error("Solana client error: {0}")]
    SolanaClient(#[from] ClientError),
    
    #[error("Token error: {0}")]
    Token(String),
    
    #[error("API error: {0}")]
    Api(String),
    
    #[error("Insufficient balance: {0}")]
    InsufficientBalance(String),
    
    #[error("Transaction error: {0}")]
    Transaction(String),
    
    #[error("IO error: {0}")]
    Io(String),
    
    #[error("Serialization error: {0}")]
    Serialization(String),
    
    #[error("Network error: {0}")]
    Network(String),
    
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    
    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),
    
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Pump.fun API specific error: {0}")]
    PumpApi(String),

    #[error("Timeout: {0}")]
    Timeout(String),
    
    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Pubkey parse error: {0}")]
    PubkeyParseError(#[from] ParsePubkeyError),
    
    #[error("BS58 decode error: {0}")]
    Bs58DecodeError(#[from] bs58::decode::Error),
    
    #[error("Signature error: {0}")]
    SignatureError(#[from] ed25519_dalek::SignatureError),
    
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
    
    #[error("Unknown error: {0}")]
    Unknown(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Solana program error: {0}")]
    SolanaProgram(#[from] ProgramError),

    #[error("Insufficient funds: {0}")]
    InsufficientFunds(String),
    
    #[error("Metadata upload error: {0}")]
    MetadataUpload(String),

    #[error("Bundle creation/send error: {0}")]
    Bundling(String),
    
    #[error("Signing error: {0}")]
    Signing(String),
    
    #[error("Generic error: {0}")]
    Generic(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),
    
    #[error("Calculation error: {0}")]
    Calculation(String),

    // Comment out AnchorClientError if not used
    // #[error("Anchor client error: {0}")]
    // AnchorClient(#[from] AnchorClientError),

    #[error("Jito SDK/Client error: {0}")]
    Jito(String),

    #[error("Token Metadata error: {0}")]
    Metadata(String),

    #[error("Instruction building error: {0}")]
    Build(String),

    #[error("Signer error: {0}")]
    Signer(#[from] SignerError),

    #[error("Transaction simulation error: {0}")]
    TransactionSimulation(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Message compile error: {0}")]
    MessageCompile(#[from] CompileError),

    #[error("Invariant condition violated: {0}")]
    Invariant(String),
}

pub type Result<T> = std::result::Result<T, PumpfunError>;

// Add From<anyhow::Error> implementation
impl From<anyhow::Error> for PumpfunError {
    fn from(err: anyhow::Error) -> Self {
        // Attempt to downcast to specific PumpfunError types if needed,
        // otherwise wrap in a generic error.
        // For simple cases, just converting to string is fine.
        PumpfunError::Generic(err.to_string())
    }
}

// Remove the From impl for http::uri::InvalidUri
// impl From<http::uri::InvalidUri> for PumpfunError {
//     fn from(err: http::uri::InvalidUri) -> Self {
//         PumpfunError::Config(format!("Invalid URI for Jito endpoint: {}", err))
//     }
// } 