use {
    adrena_abi::{oracle::ChaosLabsBatchPrices, ALP_MINT, CORTEX_ID, MAIN_POOL_ID},
    solana_sdk::pubkey::Pubkey,
};

pub fn create_update_pool_aum_ix(
    payer: &Pubkey,
    last_trading_prices: Option<ChaosLabsBatchPrices>,
) -> (
    adrena_abi::instruction::UpdatePoolAum,
    adrena_abi::accounts::UpdatePoolAum,
) {
    let oracle_pda = adrena_abi::pda::get_oracle_pda().0;

    let args = adrena_abi::instruction::UpdatePoolAum {
        params: adrena_abi::types::UpdatePoolAumParams {
            oracle_prices: last_trading_prices,
        },
    };

    let accounts = adrena_abi::accounts::UpdatePoolAum {
        payer: *payer,
        cortex: CORTEX_ID,
        pool: MAIN_POOL_ID,
        oracle: oracle_pda,
        lp_token_mint: ALP_MINT,
    };

    (args, accounts)
}
