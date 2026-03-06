use anyhow::Context;
use chrono::{DateTime, Utc};
use deadpool_postgres::Pool as DbPool;
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct OracleBatchPrice {
    pub feed_id: i32,
    pub price: Decimal,
    pub price_timestamp: DateTime<Utc>,
    pub exponent: i32,
    pub symbol: Option<String>,
    pub source_feed_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OracleBatch {
    pub oracle_batch_id: i64,
    pub provider: String,
    pub signature: Option<String>,
    pub recovery_id: Option<i32>,
    pub latest_timestamp: DateTime<Utc>,
    pub prices: Vec<OracleBatchPrice>,
    pub metadata: Option<serde_json::Value>,
}

impl OracleBatch {
    /// Decode hex signature + recovery_id from a signed batch (ChaosLabs/Autonom).
    pub fn decode_signature(&self) -> Result<([u8; 64], u8), anyhow::Error> {
        let signature_hex = self
            .signature
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} DB batch {} has no signature",
                    self.provider,
                    self.oracle_batch_id
                )
            })?
            .trim_start_matches("0x");

        let signature_bytes = hex::decode(signature_hex).with_context(|| {
            format!(
                "failed to decode {} signature hex for batch {}",
                self.provider, self.oracle_batch_id
            )
        })?;

        let signature: [u8; 64] = signature_bytes.try_into().map_err(|_| {
            anyhow::anyhow!(
                "{} signature must be 64 bytes for batch {}",
                self.provider,
                self.oracle_batch_id
            )
        })?;

        let recovery_id = self
            .recovery_id
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} DB batch {} has no recovery_id",
                    self.provider,
                    self.oracle_batch_id
                )
            })
            .and_then(|id| {
                u8::try_from(id).map_err(|_| {
                    anyhow::anyhow!(
                        "invalid {} recovery_id {} for batch {}",
                        self.provider,
                        id,
                        self.oracle_batch_id
                    )
                })
            })?;

        Ok((signature, recovery_id))
    }
}

pub async fn get_latest_oracle_batch_by_provider(
    db_pool: &DbPool,
    provider: &str,
) -> Result<Option<OracleBatch>, anyhow::Error> {
    let client = db_pool.get().await.map_err(|e| {
        anyhow::anyhow!(
            "Failed to get database connection for oracle batch: {:?}",
            e
        )
    })?;

    let header_rows = client
        .query(
            "SELECT
                oracle_batch_id,
                provider,
                signature,
                recovery_id,
                latest_timestamp,
                metadata
            FROM oracle_batches
            WHERE provider = $1
            ORDER BY latest_timestamp DESC
            LIMIT 1",
            &[&provider],
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch oracle batch header: {:?}", e))?;

    let Some(header_row) = header_rows.first() else {
        return Ok(None);
    };

    let oracle_batch_id = header_row.get::<_, i64>(0);
    let provider = header_row.get::<_, String>(1);
    let signature = header_row.get::<_, Option<String>>(2);
    let recovery_id = header_row.get::<_, Option<i32>>(3);
    let latest_timestamp = header_row.get::<_, DateTime<Utc>>(4);
    let metadata = header_row.get::<_, Option<serde_json::Value>>(5);

    let price_rows = client
        .query(
            "SELECT
                feed_id,
                price,
                price_timestamp,
                exponent,
                symbol,
                source_feed_id
            FROM oracle_batch_prices
            WHERE oracle_batch_id = $1
            ORDER BY oracle_batch_price_id ASC",
            &[&oracle_batch_id],
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to fetch oracle batch prices: {:?}", e))?;

    let prices = price_rows
        .iter()
        .map(|row| OracleBatchPrice {
            feed_id: row.get::<_, i32>(0),
            price: row.get::<_, Decimal>(1),
            price_timestamp: row.get::<_, DateTime<Utc>>(2),
            exponent: row.get::<_, i32>(3),
            symbol: row.get::<_, Option<String>>(4),
            source_feed_id: row.get::<_, Option<String>>(5),
        })
        .collect();

    Ok(Some(OracleBatch {
        oracle_batch_id,
        provider,
        signature,
        recovery_id,
        latest_timestamp,
        prices,
        metadata,
    }))
}
