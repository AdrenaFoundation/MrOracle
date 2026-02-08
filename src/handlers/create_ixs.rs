use {
    crate::adrena_ix::{self, MultiBatchPrices, SwitchboardFeedMapEntry},
    adrena_abi::oracle::ChaosLabsBatchPrices,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
};

pub fn create_update_pool_aum_ix(
    payer: &Pubkey,
    last_trading_prices: Option<ChaosLabsBatchPrices>,
    custody_accounts: &[AccountMeta],
) -> Result<Instruction, anyhow::Error> {
    adrena_ix::build_update_pool_aum_ix(payer, last_trading_prices, custody_accounts)
}

pub fn create_update_oracle_switchboard_ix(
    quote_accounts: &[Pubkey],
    quote_instruction_indices: Vec<u8>,
    max_age_slots: u64,
    feed_map: Vec<SwitchboardFeedMapEntry>,
) -> Result<Instruction, anyhow::Error> {
    adrena_ix::build_update_oracle_switchboard_ix(
        quote_accounts,
        quote_instruction_indices,
        max_age_slots,
        feed_map,
    )
}

pub fn create_update_oracle_multi_ix(
    multi_oracle_prices: MultiBatchPrices,
) -> Result<Instruction, anyhow::Error> {
    adrena_ix::build_update_oracle_multi_ix(multi_oracle_prices)
}
