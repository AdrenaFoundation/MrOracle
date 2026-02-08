use {
    crate::{
        adrena_ix::{BatchPricesWithProvider, MultiBatchPrices, SwitchboardFeedMapEntry},
        handlers::{create_update_oracle_multi_ix, create_update_oracle_switchboard_ix},
    },
    adrena_abi::oracle::ChaosLabsBatchPrices,
    anchor_client::Program,
    anyhow::Context,
    solana_client::rpc_config::RpcSendTransactionConfig,
    solana_sdk::{
        compute_budget::ComputeBudgetInstruction, instruction::Instruction, pubkey::Pubkey,
        signature::Keypair,
    },
    std::{sync::Arc, time::Duration},
    switchboard_on_demand::client::surge::SurgeUpdate,
    tokio::time::timeout,
};

pub async fn update_oracle_with_multi_batch(
    program: &Program<Arc<Keypair>>,
    median_priority_fee: u64,
    update_oracle_cu_limit: u32,
    provider: u8,
    batch: ChaosLabsBatchPrices,
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
    queue_pubkey: Pubkey,
    max_age_slots: u64,
    feed_map: Vec<SwitchboardFeedMapEntry>,
    updates: Vec<SurgeUpdate>,
) -> Result<(), anyhow::Error> {
    if updates.is_empty() {
        return Err(anyhow::anyhow!("missing switchboard updates"));
    }

    let mut ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_price(median_priority_fee),
        ComputeBudgetInstruction::set_compute_unit_limit(update_oracle_cu_limit),
    ];

    let mut quote_accounts = Vec::with_capacity(updates.len());
    let mut quote_instruction_indices = Vec::with_capacity(updates.len());

    for update in updates {
        let ed25519_ix_idx = u16::try_from(ixs.len())
            .context("too many instructions before switchboard ed25519 verification")?;

        let quote_ixs = update
            .to_quote_ix(queue_pubkey, program.payer(), ed25519_ix_idx)
            .map_err(|e| anyhow::anyhow!("failed to convert surge update to quote ixs: {e}"))?;

        if quote_ixs.len() != 2 {
            return Err(anyhow::anyhow!(
                "expected 2 quote instructions (ed25519 + quote update), got {}",
                quote_ixs.len()
            ));
        }

        let quote_ix = quote_ixs
            .get(1)
            .context("missing quote-program instruction in generated quote ixs")?;

        let quote_account = quote_ix
            .accounts
            .get(1)
            .map(|meta| meta.pubkey)
            .context("missing quote account meta in quote-program instruction")?;

        let quote_ix_index = usize::from(ed25519_ix_idx)
            .checked_add(1)
            .context("quote instruction index overflow")?;
        let quote_ix_index =
            u8::try_from(quote_ix_index).context("quote instruction index exceeds u8")?;

        quote_accounts.push(quote_account);
        quote_instruction_indices.push(quote_ix_index);
        ixs.extend(quote_ixs);
    }

    let update_oracle_ix = create_update_oracle_switchboard_ix(
        &quote_accounts,
        quote_instruction_indices,
        max_age_slots,
        feed_map,
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
