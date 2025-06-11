use chrono::{DateTime, Utc};
use deadpool_postgres::Pool as DbPool;
use rust_decimal::Decimal;

pub struct AssetsPrices {
    pub solusd_price: Decimal,
    pub jitosolusd_price: Decimal,
    pub btcusd_price: Decimal,
    pub wbtcusd_price: Decimal,
    pub bonkusd_price: Decimal,
    pub usdcusd_price: Decimal,
    pub solusd_price_ts: DateTime<Utc>,
    pub jitosolusd_price_ts: DateTime<Utc>,
    pub btcusd_price_ts: DateTime<Utc>,
    pub wbtcusd_price_ts: DateTime<Utc>,
    pub bonkusd_price_ts: DateTime<Utc>,
    pub usdcusd_price_ts: DateTime<Utc>,
    pub signature: String,
    pub recovery_id: i32,
    pub latest_timestamp: DateTime<Utc>,
}

pub async fn get_assets_prices(db_pool: &DbPool) -> Result<Option<AssetsPrices>, anyhow::Error> {
    let client = db_pool.get().await.map_err(|e| {
        log::error!("Failed to get database connection: {:?}", e);
        anyhow::anyhow!("Failed to get database connection: {:?}", e)
    })?;

    let rows = client
        .query(
            "SELECT
                solusd_price,
                jitosolusd_price,
                btcusd_price,
                wbtcusd_price,
                bonkusd_price,
                usdcusd_price,
                solusd_price_ts,
                jitosolusd_price_ts,
                btcusd_price_ts,
                wbtcusd_price_ts,
                bonkusd_price_ts,
                usdcusd_price_ts,
                signature,
                recovery_id,
                latest_timestamp
            FROM assets_price
            ORDER BY latest_timestamp DESC
            LIMIT 1",
            &[],
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get oracle price entry from DB: {:?}", e))?;

    if let Some(row) = rows.first() {
        Ok(Some(AssetsPrices {
            solusd_price: row.get::<_, Decimal>(0),
            jitosolusd_price: row.get::<_, Decimal>(1),
            btcusd_price: row.get::<_, Decimal>(2),
            wbtcusd_price: row.get::<_, Decimal>(3),
            bonkusd_price: row.get::<_, Decimal>(4),
            usdcusd_price: row.get::<_, Decimal>(5),
            solusd_price_ts: row.get::<_, DateTime<Utc>>(6),
            jitosolusd_price_ts: row.get::<_, DateTime<Utc>>(7),
            btcusd_price_ts: row.get::<_, DateTime<Utc>>(8),
            wbtcusd_price_ts: row.get::<_, DateTime<Utc>>(9),
            bonkusd_price_ts: row.get::<_, DateTime<Utc>>(10),
            usdcusd_price_ts: row.get::<_, DateTime<Utc>>(11),
            signature: row.get::<_, String>(12),
            recovery_id: row.get::<_, i32>(13),
            latest_timestamp: row.get::<_, DateTime<Utc>>(14),
        }))
    } else {
        log::debug!("No oracle price entry found in DB");
        Ok(None)
    }
}
