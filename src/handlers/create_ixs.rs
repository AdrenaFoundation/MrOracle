use {
    crate::adrena_ix::{self, MultiBatchPrices, SwitchboardUpdateParams},
    adrena_abi::oracle::BatchPrices,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
};

pub fn create_update_pool_aum_ix(
    payer: &Pubkey,
    pool_pubkey: Pubkey,
    oracle_prices: Option<BatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
    quote_account: Option<Pubkey>,
    custody_accounts: &[AccountMeta],
) -> Result<Instruction, anyhow::Error> {
    adrena_ix::build_update_pool_aum_ix(
        payer,
        pool_pubkey,
        oracle_prices,
        multi_oracle_prices,
        switchboard_oracle_prices,
        quote_account,
        custody_accounts,
    )
}
