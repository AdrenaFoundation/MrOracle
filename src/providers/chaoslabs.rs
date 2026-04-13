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

/// Default TTL for forwarded oracle batches (seconds). Override with
/// `ORACLE_BATCH_MAX_AGE_SECONDS`. Timestamp-based safety rule: reject any
/// batch whose latest_timestamp or per-price timestamp is older than this.
/// Independent of adrena-data's ingestion-side TTL — we do not rely on
/// upstream filtering.
fn oracle_batch_max_age_seconds() -> i64 {
    std::env::var("ORACLE_BATCH_MAX_AGE_SECONDS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30)
}

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

            // Timestamp TTL check (independent of adrena-data). Reject stale
            // batches before building a ProviderUpdate so we don't dispatch
            // old signed data to the chain.
            let ttl = oracle_batch_max_age_seconds();
            let now = chrono::Utc::now();
            let batch_age = (now - batch.latest_timestamp).num_seconds();
            if batch_age > ttl {
                log::warn!(
                    "chaoslabs oracle_batch {} stale: latest_timestamp age={}s > ttl={}s — skipping cycle",
                    batch.oracle_batch_id, batch_age, ttl
                );
                return Ok(None);
            }
            for p in &batch.prices {
                let p_age = (now - p.price_timestamp).num_seconds();
                if p_age > ttl {
                    log::warn!(
                        "chaoslabs oracle_batch {} stale: feed_id={} price_timestamp age={}s > ttl={}s — skipping cycle",
                        batch.oracle_batch_id, p.feed_id, p_age, ttl
                    );
                    return Ok(None);
                }
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

/// Convert a `Decimal` to a `u64` with sanity bounds.
/// Prices are at 1e10 scale (PRICE_DECIMALS=10).
/// Rejects zero, negative, and prices outside a wide sanity window.
///
/// Bounds (at 1e10 scale):
///   min: 1 (effectively $0.0000000001 — catches zero/corrupt)
///   max: 10_000_000 * 1e10 = 1e17 (covers assets up to $10M per unit)
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
