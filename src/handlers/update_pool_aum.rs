use {
    crate::{
        adrena_ix::{
            BatchPricesWithProvider, MultiBatchPrices, SwitchboardUpdateParams,
        },
        handlers::create_update_pool_aum_ix,
        provider_updates::SwitchboardOraclePricesUpdate,
        UPDATE_AUM_CU_LIMIT,
    },
    adrena_abi::oracle::BatchPrices,
    anchor_client::Program,
    solana_client::rpc_config::RpcSendTransactionConfig,
    solana_sdk::{
        compute_budget::ComputeBudgetInstruction,
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
        signature::Keypair,
    },
    std::{sync::Arc, time::Duration},
    tokio::time::timeout,
};

pub async fn update_pool_aum_combined(
    program: &Program<Arc<Keypair>>,
    pool_pubkey: Pubkey,
    median_priority_fee: u64,
    update_pool_aum_cu_limit: u32,
    oracle_prices: Option<BatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardOraclePricesUpdate>,
    custody_accounts: Vec<AccountMeta>,
) -> Result<(), anyhow::Error> {
    let ixs = build_update_pool_aum_instruction_sequence(
        program,
        pool_pubkey,
        median_priority_fee,
        update_pool_aum_cu_limit,
        oracle_prices,
        multi_oracle_prices,
        switchboard_oracle_prices,
        custody_accounts,
    )?;

    send_update_pool_aum_transaction(program, ixs, "coordinated").await
}

pub async fn update_pool_aum(
    program: &Program<Arc<Keypair>>,
    pool_pubkey: Pubkey,
    median_priority_fee: u64,
    last_trading_prices: BatchPrices,
    custody_accounts: Vec<AccountMeta>,
) -> Result<(), anyhow::Error> {
    log::info!("  <*> Updating AUM");
    let ixs = build_update_pool_aum_instruction_sequence(
        program,
        pool_pubkey,
        median_priority_fee,
        UPDATE_AUM_CU_LIMIT,
        Some(last_trading_prices),
        None,
        None,
        custody_accounts,
    )?;
    send_update_pool_aum_transaction(program, ixs, "chaoslabs").await
}

pub async fn update_pool_aum_with_multi_batch(
    program: &Program<Arc<Keypair>>,
    pool_pubkey: Pubkey,
    median_priority_fee: u64,
    update_pool_aum_cu_limit: u32,
    provider: u8,
    batch: BatchPrices,
    custody_accounts: Vec<AccountMeta>,
) -> Result<(), anyhow::Error> {
    let multi_oracle_prices = MultiBatchPrices {
        batches: vec![BatchPricesWithProvider { provider, batch }],
    };
    let ixs = build_update_pool_aum_instruction_sequence(
        program,
        pool_pubkey,
        median_priority_fee,
        update_pool_aum_cu_limit,
        None,
        Some(multi_oracle_prices),
        None,
        custody_accounts,
    )?;
    send_update_pool_aum_transaction(program, ixs, "multi-oracle").await
}

pub async fn update_pool_aum_with_switchboard(
    program: &Program<Arc<Keypair>>,
    pool_pubkey: Pubkey,
    median_priority_fee: u64,
    update_pool_aum_cu_limit: u32,
    switchboard_update: SwitchboardOraclePricesUpdate,
    custody_accounts: Vec<AccountMeta>,
) -> Result<(), anyhow::Error> {
    let ixs = build_update_pool_aum_instruction_sequence(
        program,
        pool_pubkey,
        median_priority_fee,
        update_pool_aum_cu_limit,
        None,
        None,
        Some(switchboard_update),
        custody_accounts,
    )?;
    send_update_pool_aum_transaction(program, ixs, "switchboard").await
}

fn build_update_pool_aum_instruction_sequence(
    program: &Program<Arc<Keypair>>,
    pool_pubkey: Pubkey,
    median_priority_fee: u64,
    update_pool_aum_cu_limit: u32,
    oracle_prices: Option<BatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardOraclePricesUpdate>,
    custody_accounts: Vec<AccountMeta>,
) -> Result<Vec<Instruction>, anyhow::Error> {
    let mut ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_price(median_priority_fee),
        ComputeBudgetInstruction::set_compute_unit_limit(update_pool_aum_cu_limit),
    ];

    let (switchboard_params, quote_account) =
        if let Some(switchboard_update) = switchboard_oracle_prices {
            // Append Ed25519 signature verification instruction.
            ixs.push(switchboard_update.ed25519_ix);

            // Append quote program store instruction (updates canonical SwitchboardQuote PDA).
            ixs.push(switchboard_update.quote_store_ix);

            let params = SwitchboardUpdateParams {
                max_age_slots: switchboard_update.max_age_slots,
                feed_map: switchboard_update.feed_map,
            };

            (Some(params), Some(switchboard_update.quote_account))
        } else {
            (None, None)
        };

    let update_pool_aum_ix = create_update_pool_aum_ix(
        &program.payer(),
        pool_pubkey,
        oracle_prices,
        multi_oracle_prices,
        switchboard_params,
        quote_account,
        &custody_accounts,
    )?;
    ixs.push(update_pool_aum_ix);

    Ok(ixs)
}

async fn send_update_pool_aum_transaction(
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
                "   <> {} update_pool_aum transaction generation timed out after 2 seconds",
                flow_label
            );
            anyhow::anyhow!(
                "{} update_pool_aum transaction generation timed out",
                flow_label
            )
        })?
        .map_err(|e| {
            log::error!(
                "   <> {} update_pool_aum transaction generation failed with error: {:?}",
                flow_label,
                e
            );
            anyhow::anyhow!(
                "{} update_pool_aum transaction generation failed with error: {:?}",
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
                "   <> {} update_pool_aum transaction sending failed with error: {:?}",
                flow_label,
                e
            );
            anyhow::anyhow!(
                "{} update_pool_aum transaction sending failed with error: {:?}",
                flow_label,
                e
            )
        })?;

    log::info!(
        "   <> {} update_pool_aum TX sent: {:#?}",
        flow_label,
        tx_hash.to_string()
    );

    Ok(())
}
