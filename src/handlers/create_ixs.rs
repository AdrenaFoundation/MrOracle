//! Thin wrapper around `adrena_ix::build_update_pool_aum_ix`. Kept as its own
//! module to mirror main's layout, making the release/39 diff easier to read.

use {
    crate::adrena_ix::{
        build_update_pool_aum_ix, BatchPrices, MultiBatchPrices, SwitchboardUpdateParams,
    },
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
};

#[allow(clippy::too_many_arguments)]
pub fn create_update_pool_aum_ix(
    payer: &Pubkey,
    pool_pubkey: Pubkey,
    oracle_prices: Option<BatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
    quote_account: Option<Pubkey>,
    custody_accounts: &[AccountMeta],
) -> anyhow::Result<Instruction> {
    build_update_pool_aum_ix(
        payer,
        pool_pubkey,
        oracle_prices,
        multi_oracle_prices,
        switchboard_oracle_prices,
        quote_account,
        custody_accounts,
    )
}
