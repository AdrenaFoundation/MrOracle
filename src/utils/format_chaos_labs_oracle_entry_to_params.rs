use {
    adrena_abi::oracle::{ChaosLabsBatchPrices, PriceData},
    hex::FromHex,
};

use crate::db::get_assets_prices::AssetsPrices;
use rust_decimal::prelude::ToPrimitive;

pub fn format_chaos_labs_oracle_entry_to_params(
    chaos_labs_oracle_entry: &AssetsPrices,
) -> Result<ChaosLabsBatchPrices, anyhow::Error> {
    let signature_vec: [u8; 64] = {
        let vec = Vec::from_hex(&chaos_labs_oracle_entry.signature)
            .map_err(|e| anyhow::anyhow!("Failed to parse signature hex: {}", e))?;
        vec.try_into()
            .map_err(|_| anyhow::anyhow!("Signature hex string has incorrect length"))?
    };

    let mut prices = Vec::new();

    // JitoSOL/USD - feed_id 1
    prices.push(PriceData {
        // symbol: "JITOSOLUSD".to_string(),
        feed_id: 1,
        price: chaos_labs_oracle_entry
            .jitosolusd_price
            .to_u64()
            .ok_or_else(|| anyhow::anyhow!("Failed to convert JitoSOL price to u64"))?,
        timestamp: chaos_labs_oracle_entry.jitosolusd_price_ts.timestamp(),
        // exponent: -10 as i8,
    });

    // SOL/USD - feed_id 0
    prices.push(PriceData {
        // symbol: "SOLUSD".to_string(),
        feed_id: 0,
        price: chaos_labs_oracle_entry
            .solusd_price
            .to_u64()
            .ok_or_else(|| anyhow::anyhow!("Failed to convert SOL price to u64"))?,
        timestamp: chaos_labs_oracle_entry.solusd_price_ts.timestamp(),
        // exponent: -10 as i8,
    });

    // WBTC/USD - feed_id 3
    prices.push(PriceData {
        // symbol: "WBTCUSD".to_string(),
        feed_id: 3,
        price: chaos_labs_oracle_entry
            .wbtcusd_price
            .to_u64()
            .ok_or_else(|| anyhow::anyhow!("Failed to convert WBTC price to u64"))?,
        timestamp: chaos_labs_oracle_entry.wbtcusd_price_ts.timestamp(),
        // exponent: -10 as i8,
    });

    // BTC/USD - feed_id 2
    prices.push(PriceData {
        // symbol: "BTCUSD".to_string(),
        feed_id: 2,
        price: chaos_labs_oracle_entry
            .btcusd_price
            .to_u64()
            .ok_or_else(|| anyhow::anyhow!("Failed to convert BTC price to u64"))?,
        timestamp: chaos_labs_oracle_entry.btcusd_price_ts.timestamp(),
        // exponent: -10 as i8,
    });

    // BONK/USD - feed_id 4
    prices.push(PriceData {
        // symbol: "BONKUSD".to_string(),
        feed_id: 4,
        price: chaos_labs_oracle_entry
            .bonkusd_price
            .to_u64()
            .ok_or_else(|| anyhow::anyhow!("Failed to convert BONK price to u64"))?,
        timestamp: chaos_labs_oracle_entry.bonkusd_price_ts.timestamp(),
        // exponent: -10 as i8,
    });

    // USDC/USD - feed_id 5
    prices.push(PriceData {
        // symbol: "USDCUSD".to_string(),
        feed_id: 5,
        price: chaos_labs_oracle_entry
            .usdcusd_price
            .to_u64()
            .ok_or_else(|| anyhow::anyhow!("Failed to convert USDC price to u64"))?,
        timestamp: chaos_labs_oracle_entry.usdcusd_price_ts.timestamp(),
        // exponent: -10 as i8,
    });

    let batch_prices = ChaosLabsBatchPrices {
        prices,
        signature: signature_vec,
        recovery_id: chaos_labs_oracle_entry.recovery_id as u8,
    };

    Ok(batch_prices)
}
