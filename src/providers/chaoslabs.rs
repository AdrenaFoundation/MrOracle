//! ChaosLabs provider cycle.
//!
//! Reads the latest `oracle_batches` row where provider='chaoslabs', rebuilds
//! it into a `BatchPrices` struct in the exact order the ChaosLabs API
//! originally signed (preserved by adrena-data via `ORDER BY oracle_batch_price_id ASC`),
//! and returns a `ProviderUpdate::ChaosLabs(...)` wrapped in a `PollerPayload`.
//!
//! ChaosLabs feed_id range is 0..=29 (enforced by adrena on-chain via
//! `OracleProvider::feed_id_range`).

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

const CHAOSLABS_PROVIDER: &str = "chaoslabs";
const CHAOSLABS_MIN_FEED_ID: i32 = 0;
const CHAOSLABS_MAX_FEED_ID: i32 = 29;

pub fn make_chaoslabs_cycle(db_pool: DbPool) -> CycleFn {
    Box::new(move || {
        let db_pool = db_pool.clone();
        Box::pin(async move {
            let batch = match db::get_latest_oracle_batch_by_provider(&db_pool, CHAOSLABS_PROVIDER)
                .await
                .context("chaoslabs: read latest oracle batch")?
            {
                Some(b) => b,
                None => return Ok(None),
            };

            if batch.prices.is_empty() {
                return Err(anyhow!(
                    "chaoslabs oracle_batch {} has zero prices",
                    batch.oracle_batch_id
                ));
            }

            let (signature, recovery_id) = batch.decode_signature()?;

            // Validate feed_id range + convert to wire types. Preserve
            // insertion order (ORDER BY oracle_batch_price_id ASC was applied
            // by the DB query) so the reconstructed hash matches ChaosLabs'
            // signature.
            let mut prices = Vec::with_capacity(batch.prices.len());
            for p in &batch.prices {
                if !(CHAOSLABS_MIN_FEED_ID..=CHAOSLABS_MAX_FEED_ID).contains(&p.feed_id) {
                    return Err(anyhow!(
                        "chaoslabs oracle_batch {} has feed_id {} outside range {CHAOSLABS_MIN_FEED_ID}..={CHAOSLABS_MAX_FEED_ID}",
                        batch.oracle_batch_id,
                        p.feed_id
                    ));
                }
                let feed_id: u8 = u8::try_from(p.feed_id).with_context(|| {
                    format!("chaoslabs feed_id {} does not fit u8", p.feed_id)
                })?;

                let price: u64 = decimal_to_u64(&p.price).with_context(|| {
                    format!("chaoslabs price for feed_id {} out of u64 range", p.feed_id)
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
                update: ProviderUpdate::ChaosLabs(batch_prices),
            }))
        })
    })
}

/// Convert a `Decimal` to a `u64`. The DB stores prices as NUMERIC without
/// a scaling factor — adrena-data writes the already-scaled 1e10 integer
/// directly as the decimal string. So we just need to check the value fits
/// u64 and round any stray fractional part.
fn decimal_to_u64(d: &rust_decimal::Decimal) -> anyhow::Result<u64> {
    if d.is_sign_negative() {
        return Err(anyhow!("negative price"));
    }
    // Truncate to integer. adrena-data always stores already-scaled u64-compatible
    // values so this never loses information in practice.
    let trunc = d.trunc();
    use rust_decimal::prelude::ToPrimitive;
    trunc
        .to_u64()
        .ok_or_else(|| anyhow!("price {} does not fit u64", d))
}
