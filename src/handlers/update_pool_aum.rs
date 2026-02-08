use {
    crate::{handlers::create_update_pool_aum_ix, UPDATE_AUM_CU_LIMIT},
    adrena_abi::oracle::ChaosLabsBatchPrices,
    anchor_client::Program,
    solana_client::rpc_config::RpcSendTransactionConfig,
    solana_sdk::{
        compute_budget::ComputeBudgetInstruction, instruction::AccountMeta, signature::Keypair,
    },
    std::{sync::Arc, time::Duration},
    tokio::time::timeout,
};

pub async fn update_pool_aum(
    program: &Program<Arc<Keypair>>,
    median_priority_fee: u64,
    last_trading_prices: ChaosLabsBatchPrices,
    custody_accounts: Vec<AccountMeta>,
) -> Result<(), anyhow::Error> {
    log::info!("  <*> Updating AUM");

    let update_pool_aum_ix = create_update_pool_aum_ix(
        &program.payer(),
        Some(last_trading_prices),
        &custody_accounts,
    )?;

    let tx = timeout(
        Duration::from_secs(2),
        program
            .request()
            .instruction(ComputeBudgetInstruction::set_compute_unit_price(
                median_priority_fee,
            ))
            .instruction(ComputeBudgetInstruction::set_compute_unit_limit(
                UPDATE_AUM_CU_LIMIT,
            ))
            .instruction(update_pool_aum_ix)
            .signed_transaction(),
    )
    .await
    .map_err(|_| {
        log::error!("   <> Transaction generation timed out after 2 seconds");
        anyhow::anyhow!("Transaction generation timed out")
    })?
    .map_err(|e| {
        log::error!("   <> Transaction generation failed with error: {:?}", e);
        anyhow::anyhow!("Transaction generation failed with error: {:?}", e)
    })?;

    let rpc_client = program.rpc();

    let tx_hash = rpc_client
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| {
            log::error!("   <> Transaction sending failed with error: {:?}", e);
            anyhow::anyhow!("Transaction sending failed with error: {:?}", e)
        })?;

    log::info!("   <> TX sent: {:#?}", tx_hash.to_string());

    Ok(())
}
