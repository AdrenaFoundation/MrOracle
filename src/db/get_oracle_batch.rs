//! Reads the latest `oracle_batches` row for a given provider, joined with
//! its per-feed price rows. Used by each provider's polling cycle to decide
//! whether a new batch has landed since the last push.
//!
//! Schema is defined in adrena-data/cron/db/migration/oracle-batches/migration-oracle-batches.sql.

use {
    anyhow::{anyhow, Context},
    chrono::{DateTime, Utc},
    deadpool_postgres::Pool as DbPool,
    rust_decimal::Decimal,
};

#[derive(Debug, Clone)]
pub struct OracleBatch {
    pub oracle_batch_id: i64,
    pub provider: String,
    pub signature: Option<String>,
    pub recovery_id: Option<i32>,
    pub latest_timestamp: DateTime<Utc>,
    pub metadata: Option<serde_json::Value>,
    pub prices: Vec<OracleBatchPrice>,
}

#[derive(Debug, Clone)]
pub struct OracleBatchPrice {
    pub feed_id: i32,
    pub price: Decimal,
    pub price_timestamp: DateTime<Utc>,
    pub exponent: i32,
    pub symbol: Option<String>,
    pub source_feed_id: Option<String>,
}

impl OracleBatch {
    /// Decode the hex-encoded signature + recovery_id into the wire format
    /// adrena's on-chain `BatchPrices` expects. Only valid for ChaosLabs +
    /// Autonom rows (Switchboard rows have `signature: None` because the
    /// "signature" lives inside the ed25519 ix bytes stored in metadata).
    pub fn decode_signature(&self) -> anyhow::Result<([u8; 64], u8)> {
        let sig_hex = self
            .signature
            .as_ref()
            .ok_or_else(|| anyhow!("oracle batch {} has no signature", self.oracle_batch_id))?;

        let sig_bytes = hex::decode(sig_hex.trim_start_matches("0x"))
            .with_context(|| format!("decode hex signature for batch {}", self.oracle_batch_id))?;

        if sig_bytes.len() != 64 {
            return Err(anyhow!(
                "oracle batch {} signature length is {} (expected 64)",
                self.oracle_batch_id,
                sig_bytes.len()
            ));
        }

        let mut sig = [0u8; 64];
        sig.copy_from_slice(&sig_bytes);

        let rec_id = self
            .recovery_id
            .ok_or_else(|| anyhow!("oracle batch {} has no recovery_id", self.oracle_batch_id))?;

        if !(0..=3).contains(&rec_id) {
            return Err(anyhow!(
                "oracle batch {} recovery_id {} out of range",
                self.oracle_batch_id,
                rec_id
            ));
        }

        Ok((sig, rec_id as u8))
    }
}

/// Fetch the latest batch row for a provider, along with its prices.
/// Prices are returned in `oracle_batch_price_id ASC` order — the order they
/// were inserted, which matches the provider API's response order, which is
/// what the provider signed. Reordering breaks on-chain signature verification.
///
/// Returns `None` if no batch has been inserted yet for this provider.
pub async fn get_latest_oracle_batch_by_provider(
    db_pool: &DbPool,
    provider: &str,
) -> anyhow::Result<Option<OracleBatch>> {
    let client = db_pool
        .get()
        .await
        .with_context(|| "acquire db client")?;

    let header_row = client
        .query_opt(
            r#"
                SELECT oracle_batch_id,
                       provider,
                       signature,
                       recovery_id,
                       latest_timestamp,
                       metadata
                FROM oracle_batches
                WHERE provider = $1
                ORDER BY latest_timestamp DESC
                LIMIT 1
            "#,
            &[&provider],
        )
        .await
        .with_context(|| format!("query latest oracle_batches row for provider={provider}"))?;

    let Some(row) = header_row else {
        return Ok(None);
    };

    let oracle_batch_id: i64 = row.get("oracle_batch_id");
    let provider: String = row.get("provider");
    let signature: Option<String> = row.get("signature");
    let recovery_id: Option<i32> = row.get("recovery_id");
    let latest_timestamp: DateTime<Utc> = row.get("latest_timestamp");
    let metadata: Option<serde_json::Value> = row.get("metadata");

    let price_rows = client
        .query(
            r#"
                SELECT feed_id,
                       price,
                       price_timestamp,
                       exponent,
                       symbol,
                       source_feed_id
                FROM oracle_batch_prices
                WHERE oracle_batch_id = $1
                ORDER BY oracle_batch_price_id ASC
            "#,
            &[&oracle_batch_id],
        )
        .await
        .with_context(|| format!("query oracle_batch_prices for batch_id={oracle_batch_id}"))?;

    let prices = price_rows
        .into_iter()
        .map(|r| OracleBatchPrice {
            feed_id: r.get("feed_id"),
            price: r.get("price"),
            price_timestamp: r.get("price_timestamp"),
            exponent: r.get("exponent"),
            symbol: r.get("symbol"),
            source_feed_id: r.get("source_feed_id"),
        })
        .collect();

    Ok(Some(OracleBatch {
        oracle_batch_id,
        provider,
        signature,
        recovery_id,
        latest_timestamp,
        metadata,
        prices,
    }))
}
