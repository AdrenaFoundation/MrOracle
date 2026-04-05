use {
    crate::rpc_fallback::RpcFallback,
    serde_json,
    solana_client::{
        rpc_request::RpcRequest,
        rpc_response::RpcPrioritizationFee,
    },
    solana_sdk::pubkey::Pubkey,
    std::error::Error,
};

pub struct GetRecentPrioritizationFeesByPercentileConfig {
    pub percentile: Option<u64>,
    pub fallback: bool,
    pub locked_writable_accounts: Vec<Pubkey>,
}

pub async fn fetch_mean_priority_fee(
    rpc_fallback: &RpcFallback,
    percentile: u64,
) -> Result<u64, anyhow::Error> {
    let config = GetRecentPrioritizationFeesByPercentileConfig {
        percentile: Some(percentile),
        fallback: false,
        locked_writable_accounts: vec![], //adrena_abi::MAIN_POOL_ID, adrena_abi::CORTEX_ID],
    };
    get_mean_prioritization_fee_by_percentile(rpc_fallback, &config, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch mean priority fee: {:?}", e))
}

pub async fn get_recent_prioritization_fees_by_percentile(
    rpc_fallback: &RpcFallback,
    config: &GetRecentPrioritizationFeesByPercentileConfig,
    slots_to_return: Option<usize>,
) -> Result<Vec<RpcPrioritizationFee>, Box<dyn Error>> {
    let accounts: Vec<String> = config
        .locked_writable_accounts
        .iter()
        .map(|key| key.to_string())
        .collect();
    // Only send the accounts array — the percentile param is a Triton/rpcpool
    // extension and not supported by standard RPCs (e.g. api.mainnet-beta.solana.com)
    let args = vec![serde_json::to_value(accounts)?];

    let response: Vec<RpcPrioritizationFee> = rpc_fallback
        .send_rpc_request(
            RpcRequest::GetRecentPrioritizationFees,
            serde_json::Value::from(args),
            "Priority Fees",
        )
        .await?;

    let mut recent_prioritization_fees: Vec<RpcPrioritizationFee> = response;

    recent_prioritization_fees.sort_by_key(|fee| fee.slot);

    if let Some(slots) = slots_to_return {
        recent_prioritization_fees.truncate(slots);
    }

    Ok(recent_prioritization_fees)
}

pub async fn get_mean_prioritization_fee_by_percentile(
    rpc_fallback: &RpcFallback,
    config: &GetRecentPrioritizationFeesByPercentileConfig,
    slots_to_return: Option<usize>,
) -> Result<u64, Box<dyn Error>> {
    let recent_prioritization_fees =
        get_recent_prioritization_fees_by_percentile(rpc_fallback, config, slots_to_return).await?;

    if recent_prioritization_fees.is_empty() {
        return Err("No prioritization fees retrieved".into());
    }

    // Client-side percentile computation (the server-side percentile param is a
    // Triton-only extension that doesn't work on public RPCs). We take the fee
    // value at the given percentile (basis points, 10000 = 100%) from a sorted
    // ascending list. Higher percentile = higher fee = higher priority.
    let mut fees: Vec<u64> = recent_prioritization_fees
        .iter()
        .map(|fee| fee.prioritization_fee)
        .collect();
    fees.sort_unstable();

    let percentile_bps = config.percentile.unwrap_or(5000);
    let idx = ((fees.len() as u64).saturating_sub(1) * percentile_bps / 10_000) as usize;
    Ok(fees[idx.min(fees.len() - 1)])
}
