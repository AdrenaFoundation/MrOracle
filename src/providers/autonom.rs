//! Autonom provider cycle.
//!
//! Same shape as chaoslabs.rs but enforces the 30..=141 feed_id range and
//! re-uses adrena-data's insertion order (which preserves the Autonom API
//! response order, which is what Autonom's `sign_adrena` hashes).
//!
//! Autonom's feed_id field on-chain is u8. The DB stores it as i32 (for
//! convenience) but we range-check on read.

use {
    crate::{
        adrena_ix::{BatchPrices, PriceData},
        db,
        oracle_poller::{CycleFn, PollerPayload},
        provider_updates::ProviderUpdate,
    },
    anyhow::{anyhow, Context},
    deadpool_postgres::Pool as DbPool,
};

const AUTONOM_PROVIDER: &str = "autonom";
const AUTONOM_MIN_FEED_ID: i32 = 30;
const AUTONOM_MAX_FEED_ID: i32 = 141;

pub fn make_autonom_cycle(db_pool: DbPool) -> CycleFn {
    Box::new(move || {
        let db_pool = db_pool.clone();
        Box::pin(async move {
            let batch = match db::get_latest_oracle_batch_by_provider(&db_pool, AUTONOM_PROVIDER)
                .await
                .context("autonom: read latest oracle batch")?
            {
                Some(b) => b,
                None => return Ok(None),
            };

            if batch.prices.is_empty() {
                return Err(anyhow!(
                    "autonom oracle_batch {} has zero prices",
                    batch.oracle_batch_id
                ));
            }

            let (signature, recovery_id) = batch.decode_signature()?;

            let mut prices = Vec::with_capacity(batch.prices.len());
            for p in &batch.prices {
                if !(AUTONOM_MIN_FEED_ID..=AUTONOM_MAX_FEED_ID).contains(&p.feed_id) {
                    return Err(anyhow!(
                        "autonom oracle_batch {} has feed_id {} outside range {AUTONOM_MIN_FEED_ID}..={AUTONOM_MAX_FEED_ID}",
                        batch.oracle_batch_id,
                        p.feed_id
                    ));
                }
                let feed_id: u8 = u8::try_from(p.feed_id).with_context(|| {
                    format!("autonom feed_id {} does not fit u8", p.feed_id)
                })?;

                let price: u64 = decimal_to_u64(&p.price).with_context(|| {
                    format!("autonom price for feed_id {} out of u64 range", p.feed_id)
                })?;

                let timestamp = p.price_timestamp.timestamp();

                prices.push(PriceData {
                    feed_id,
                    price,
                    timestamp,
                });
            }

            let batch_prices = BatchPrices {
                prices,
                signature,
                recovery_id,
            };

            Ok(Some(PollerPayload {
                dedup_key: batch.oracle_batch_id.to_string(),
                update: ProviderUpdate::Autonom(batch_prices),
            }))
        })
    })
}

/// Same sanity bounds as chaoslabs.rs — prices at 1e10 scale.
const PRICE_SANITY_MIN: u64 = 1;
const PRICE_SANITY_MAX: u64 = 100_000_000_000_000_000; // 1e17 = $10M at 1e10 scale

fn decimal_to_u64(d: &rust_decimal::Decimal) -> anyhow::Result<u64> {
    if d.is_sign_negative() {
        return Err(anyhow!("negative price"));
    }
    let trunc = d.trunc();
    use rust_decimal::prelude::ToPrimitive;
    let value = trunc
        .to_u64()
        .ok_or_else(|| anyhow!("price {} does not fit u64", d))?;

    if value < PRICE_SANITY_MIN || value > PRICE_SANITY_MAX {
        return Err(anyhow!(
            "price {} outside sanity bounds [{}, {}]",
            value, PRICE_SANITY_MIN, PRICE_SANITY_MAX
        ));
    }

    Ok(value)
}
