use solana_program::pubkey::Pubkey;
use borsh::{BorshDeserialize, BorshSerialize};

// Based on raydium-sdk-V2/src/raydium/launchpad/layout.ts

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub struct LaunchpadVestingScheduleLayout {
    pub total_locked_amount: u64,
    pub cliff_period: u64,
    pub unlock_period: u64,
    pub start_time: u64,
    pub total_allocated_share: u64,
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
pub struct LaunchpadPoolLayout {
    pub _padding0: u64, // First u64() in SDK, seems to be padding or discriminator space
    pub epoch: u64,
    pub bump: u8,
    pub status: u8,
    pub mint_decimals_a: u8,
    pub mint_decimals_b: u8,
    pub migrate_type: u8,

    pub supply: u64,
    pub total_sell_a: u64,
    pub virtual_a: u64,
    pub virtual_b: u64,
    pub real_a: u64,
    pub real_b: u64,

    pub total_fund_raising_b: u64,
    pub protocol_fee: u64,
    pub platform_fee: u64,
    pub migrate_fee: u64,

    pub vesting_schedule: LaunchpadVestingScheduleLayout,

    pub config_id: Pubkey,    // This is the "Global Config" from Solscan
    pub platform_id: Pubkey,  // This is the "Platform Config" from Solscan
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,

    pub creator: Pubkey,

    pub _padding1: [u64; 8], // seq(u64(), 8) in SDK
}

// Placeholder for PlatformConfig if needed later
// #[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
// pub struct PlatformConfigLayout {
//     pub _padding0: u64,
//     pub epoch: u64,
//     pub platform_claim_fee_wallet: Pubkey,
//     pub platform_lock_nft_wallet: Pubkey,
//     pub platform_scale: u64,
//     pub creator_scale: u64,
//     pub burn_scale: u64,
//     pub fee_rate: u64,
//     pub name: [u8; 64],
//     pub web: [u8; 256],
//     pub img: [u8; 256],
//     pub cp_config_id: Pubkey,
//     pub _padding1: [u8; 224],
// }

// Placeholder for LaunchpadConfig (Global Config) if needed later
// #[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
// pub struct LaunchpadConfigLayout {
//     pub _padding0: u64,
//     pub epoch: u64,
//     pub curve_type: u8,
//     pub index: u16,
//     pub migrate_fee: u64,
//     pub trade_fee_rate: u64,
//     pub max_share_fee_rate: u64,
//     pub min_supply_a: u64,
//     pub max_lock_rate: u64,
//     pub min_sell_rate_a: u64,
//     pub min_migrate_rate_a: u64,
//     pub min_fund_raising_b: u64,
//     pub mint_b: Pubkey,
//     pub protocol_fee_owner: Pubkey,
//     pub migrate_fee_owner: Pubkey,
//     pub migrate_to_amm_wallet: Pubkey,
//     pub migrate_to_cpmm_wallet: Pubkey,
//     pub _padding1: [u64; 16],
// }