use {
    crate::{handlers::create_update_pool_aum_ix, rpc_fallback::RpcFallback, UPDATE_AUM_CU_LIMIT},
    adrena_abi::oracle::ChaosLabsBatchPrices,
    anchor_lang::{InstructionData, ToAccountMetas},
    solana_client::rpc_config::RpcSendTransactionConfig,
    solana_sdk::{
        compute_budget::ComputeBudgetInstruction,
        instruction::{AccountMeta, Instruction},
    },
};

pub async fn update_pool_aum(
    rpc_fallback: &RpcFallback,
    median_priority_fee: u64,
    last_trading_prices: ChaosLabsBatchPrices,
    remaining_accounts: Vec<AccountMeta>,
) -> Result<(), anyhow::Error> {
    log::info!("  <*> Updating AUM");

    let (params, accounts) =
        create_update_pool_aum_ix(&rpc_fallback.payer_pubkey(), Some(last_trading_prices));

    let mut account_metas = accounts.to_account_metas(None);
    account_metas.extend(remaining_accounts);

    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_price(median_priority_fee),
        ComputeBudgetInstruction::set_compute_unit_limit(UPDATE_AUM_CU_LIMIT),
        Instruction {
            program_id: adrena_abi::ID,
            accounts: account_metas,
            data: InstructionData::data(&params),
        },
    ];

    let tx_hash = rpc_fallback
        .sign_and_send(
            instructions,
            RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            },
            "Price Update",
        )
        .await?;

    log::info!("   <> TX sent: {:#?}", tx_hash.to_string());

    Ok(())
}
