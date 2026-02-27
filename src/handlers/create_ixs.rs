use {
    crate::adrena_ix::{self, MultiBatchPrices, SwitchboardFeedMapEntry, SwitchboardUpdateParams},
    adrena_abi::oracle::ChaosLabsBatchPrices,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
};

pub fn create_update_pool_aum_ix(
    payer: &Pubkey,
    pool_pubkey: Pubkey,
    oracle_prices: Option<ChaosLabsBatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
    pull_feed_pubkeys: &[Pubkey],
    custody_accounts: &[AccountMeta],
) -> Result<Instruction, anyhow::Error> {
    adrena_ix::build_update_pool_aum_ix(
        payer,
        pool_pubkey,
        oracle_prices,
        multi_oracle_prices,
        switchboard_oracle_prices,
        pull_feed_pubkeys,
        custody_accounts,
    )
}

pub fn create_update_oracle_switchboard_ix(
    pull_feed_pubkeys: &[Pubkey],
    submit_instruction_index: u8,
    max_age_slots: u64,
    feed_map: Vec<SwitchboardFeedMapEntry>,
) -> Result<Instruction, anyhow::Error> {
    adrena_ix::build_update_oracle_switchboard_ix(
        pull_feed_pubkeys,
        submit_instruction_index,
        max_age_slots,
        feed_map,
    )
}

pub fn create_update_oracle_multi_ix(
    multi_oracle_prices: MultiBatchPrices,
) -> Result<Instruction, anyhow::Error> {
    adrena_ix::build_update_oracle_multi_ix(multi_oracle_prices)
}
