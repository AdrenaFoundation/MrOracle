//! Builds the multi-provider `update_pool_aum` transaction from a
//! `PoolSnapshot` and sends it via `RpcFallback`.
//!
//! Instruction layout (matches providers-testing + adrena-data sidecar convention):
//!
//!   ix[0]   ComputeBudgetProgram::set_compute_unit_price
//!   ix[1]   ComputeBudgetProgram::set_compute_unit_limit
//!   ix[2]   ed25519_ix                   — only if snapshot has Switchboard
//!   ix[3]   quote_store_ix                — only if snapshot has Switchboard
//!   ix[last] update_pool_aum
//!
//! The `instruction_idx` in SwitchboardUpdateParams is 2 (ed25519 position).
//!
//! If the pool snapshot has nothing (all providers empty), we skip the tx.
//! This shouldn't normally happen because the dispatcher only calls us when
//! pending has some content, but we defend against it anyway.

use {
    crate::{
        adrena_ix::{
            BatchPricesWithProvider, MultiBatchPrices, SwitchboardUpdateParams,
            ORACLE_PROVIDER_AUTONOM, ORACLE_PROVIDER_CHAOSLABS,
        },
        handlers::create_update_pool_aum_ix,
        pool_config::{PoolRuntime, PoolSnapshot},
        rpc_fallback::RpcFallback,
    },
    solana_client::rpc_config::RpcSendTransactionConfig,
    solana_sdk::{compute_budget::ComputeBudgetInstruction, instruction::Instruction},
};

/// Build + send a `update_pool_aum` tx for one pool from a (possibly partial)
/// snapshot. Never panics; all errors are logged and propagated.
pub async fn fire_update_pool_aum(
    rpc_fallback: &RpcFallback,
    pool: &PoolRuntime,
    snapshot: PoolSnapshot,
    median_priority_fee: u64,
) -> Result<(), anyhow::Error> {
    if snapshot.is_empty() {
        log::warn!(
            "[{}] fire_update_pool_aum called with empty snapshot, skipping",
            pool.name
        );
        return Ok(());
    }

    // Build multi_oracle_prices from whatever non-Switchboard providers we
    // have in the snapshot. Switchboard goes via the dedicated
    // switchboard_oracle_prices field, not inside multi_oracle_prices
    // (adrena on-chain rejects Switchboard inside MultiBatchPrices).
    let mut batches: Vec<BatchPricesWithProvider> = Vec::new();
    let mut providers_summary = Vec::<&'static str>::new();

    if let Some(cl_batch) = snapshot.chaoslabs {
        batches.push(BatchPricesWithProvider {
            provider: ORACLE_PROVIDER_CHAOSLABS,
            batch: cl_batch,
        });
        providers_summary.push("chaoslabs");
    }
    if let Some(au_batch) = snapshot.autonom {
        batches.push(BatchPricesWithProvider {
            provider: ORACLE_PROVIDER_AUTONOM,
            batch: au_batch,
        });
        providers_summary.push("autonom");
    }

    let multi_oracle_prices = if batches.is_empty() {
        None
    } else {
        Some(MultiBatchPrices { batches })
    };

    // Switchboard branch. Extract the ed25519 + quote_store ixs from the
    // snapshot, build the SwitchboardUpdateParams for the program, and
    // capture the canonical quote PDA for remaining_accounts[0].
    let (switchboard_params, switchboard_ixs, quote_account) =
        if let Some(sb) = snapshot.switchboard {
            providers_summary.push("switchboard");
            let params = SwitchboardUpdateParams {
                max_age_slots: sb.max_age_slots,
                feed_map: sb.feed_map,
            };
            (
                Some(params),
                Some((sb.ed25519_ix, sb.quote_store_ix)),
                Some(sb.quote_account),
            )
        } else {
            (None, None, None)
        };

    let update_ix = create_update_pool_aum_ix(
        &rpc_fallback.payer_pubkey(),
        pool.address,
        None, // oracle_prices (legacy path) — never used on release/39
        multi_oracle_prices,
        switchboard_params,
        quote_account,
        &pool.custody_accounts,
    )?;

    // Assemble full instruction sequence.
    let mut instructions: Vec<Instruction> = Vec::with_capacity(5);
    instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
        median_priority_fee,
    ));
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
        pool.cu_limit,
    ));

    if let Some((ed25519_ix, quote_store_ix)) = switchboard_ixs {
        instructions.push(ed25519_ix);
        instructions.push(quote_store_ix);
    }

    instructions.push(update_ix);

    log::info!(
        "[{}] fire update_pool_aum providers=[{}] ixs={} cu_limit={} fee={}",
        pool.name,
        providers_summary.join(","),
        instructions.len(),
        pool.cu_limit,
        median_priority_fee
    );

    let op_label = format!("update_pool_aum:{}", pool.name);
    let tx_sig = rpc_fallback
        .sign_and_send(
            instructions,
            RpcSendTransactionConfig {
                skip_preflight: true,
                max_retries: Some(0),
                ..Default::default()
            },
            &op_label,
        )
        .await?;

    log::info!("[{}] TX sent: {}", pool.name, tx_sig);
    Ok(())
}
