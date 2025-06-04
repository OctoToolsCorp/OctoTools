use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZombieWallet {
    pub name: String,
    pub address: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletBalance {
    pub address: Pubkey,
    pub balance: f64,
}

impl fmt::Display for WalletBalance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} SOL", self.address, self.balance)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedKey {
    pub timestamp: String,
    pub key_type: String,
    pub private_key: String,
    pub public_key: String,
    pub vanity_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletKeys {
    pub wallets: Vec<WalletInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletInfo {
    pub public_key: String,
    pub private_key: String,
    #[serde(default)]
    pub name: Option<String>,
} 