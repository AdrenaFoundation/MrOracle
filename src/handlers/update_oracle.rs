use {
    crate::{
        adrena_ix::{BatchPricesWithProvider, MultiBatchPrices},
        handlers::{create_update_oracle_multi_ix, create_update_oracle_switchboard_ix},
        provider_updates::SwitchboardOraclePricesUpdate,
    },
    adrena_abi::oracle::BatchPrices,
    anchor_client::Program,
    solana_client::rpc_config::RpcSendTransactionConfig,
    solana_sdk::{
        compute_budget::ComputeBudgetInstruction, instruction::Instruction, pubkey::Pubkey,
        signature::Keypair,
    },
    std::{sync::Arc, time::Duration},
    tokio::time::timeout,
};

pub async fn update_oracle_with_multi_batch(
    program: &Program<Arc<Keypair>>,
    median_priority_fee: u64,
    update_oracle_cu_limit: u32,
    provider: u8,
    batch: BatchPrices,
) -> Result<(), anyhow::Error> {
    let multi_oracle_prices = MultiBatchPrices {
        batches: vec![BatchPricesWithProvider { provider, batch }],
    };

    let update_oracle_ix = create_update_oracle_multi_ix(multi_oracle_prices)?;

    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_price(median_priority_fee),
        ComputeBudgetInstruction::set_compute_unit_limit(update_oracle_cu_limit),
        update_oracle_ix,
    ];

    send_update_oracle_transaction(program, ixs, "multi-oracle").await
}

pub async fn update_oracle_with_switchboard(
    program: &Program<Arc<Keypair>>,
    median_priority_fee: u64,
    update_oracle_cu_limit: u32,
    switchboard_update: SwitchboardOraclePricesUpdate,
) -> Result<(), anyhow::Error> {
    let mut ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_price(median_priority_fee),
        ComputeBudgetInstruction::set_compute_unit_limit(update_oracle_cu_limit),
    ];

    // Append Ed25519 signature verification instruction.
    ixs.push(switchboard_update.ed25519_ix);

    // Append quote program store instruction.
    ixs.push(switchboard_update.quote_store_ix);

    let update_oracle_ix = create_update_oracle_switchboard_ix(
        switchboard_update.quote_account,
        switchboard_update.max_age_slots,
        switchboard_update.feed_map,
    )?;
    ixs.push(update_oracle_ix);

    send_update_oracle_transaction(program, ixs, "switchboard").await
}

async fn send_update_oracle_transaction(
    program: &Program<Arc<Keypair>>,
    ixs: Vec<Instruction>,
    flow_label: &str,
) -> Result<(), anyhow::Error> {
    let mut request = program.request();
    for ix in ixs {
        request = request.instruction(ix);
    }

    let tx = timeout(Duration::from_secs(2), request.signed_transaction())
        .await
        .map_err(|_| {
            log::error!(
                "   <> {} update_oracle transaction generation timed out after 2 seconds",
                flow_label
            );
            anyhow::anyhow!(
                "{} update_oracle transaction generation timed out",
                flow_label
            )
        })?
        .map_err(|e| {
            log::error!(
                "   <> {} update_oracle transaction generation failed with error: {:?}",
                flow_label,
                e
            );
            anyhow::anyhow!(
                "{} update_oracle transaction generation failed with error: {:?}",
                flow_label,
                e
            )
        })?;

    let tx_hash = program
        .rpc()
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
            log::error!(
                "   <> {} update_oracle transaction sending failed with error: {:?}",
                flow_label,
                e
            );
            anyhow::anyhow!(
                "{} update_oracle transaction sending failed with error: {:?}",
                flow_label,
                e
            )
        })?;

    log::info!(
        "   <> {} update_oracle TX sent: {:#?}",
        flow_label,
        tx_hash.to_string()
    );

    Ok(())
}
