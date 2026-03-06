use {
    crate::{
        db::{self, OracleBatch},
        oracle_poller::PollerPayload,
        provider_updates::ProviderUpdate,
    },
    adrena_abi::oracle::{BatchPrices, PriceData},
    anyhow::Context,
    deadpool_postgres::Pool as DbPool,
    rust_decimal::prelude::ToPrimitive,
    serde::Deserialize,
    std::{
        collections::{HashMap, HashSet},
        fs,
        future::Future,
        pin::Pin,
    },
};

const AUTONOM_PROVIDER: &str = "autonom";
const AUTONOM_MIN_FEED_ID: u8 = 30;
const AUTONOM_MAX_FEED_ID: u8 = 141;


#[derive(Debug, Clone)]
struct AutonomAliasMapping {
    alias_by_canonical: HashMap<u64, u8>,
}

#[derive(Debug, Deserialize)]
struct AutonomFeedMapEntryRaw {
    adrena_feed_id: u8,
    autonom_feed_id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AutonomFeedMapFile {
    Bare(Vec<AutonomFeedMapEntryRaw>),
    Versioned {
        _schema_version: Option<String>,
        autonom_feed_map: Vec<AutonomFeedMapEntryRaw>,
    },
}

pub fn make_autonom_cycle(
    db_pool: DbPool,
    feed_map_path: &str,
) -> Result<
    impl Fn() -> Pin<Box<dyn Future<Output = Result<Option<PollerPayload>, anyhow::Error>> + Send>>
        + Send
        + Sync,
    anyhow::Error,
> {
    let alias_mapping = load_feed_map(feed_map_path)?;

    let mut required_alias_feed_ids: Vec<u8> = alias_mapping
        .alias_by_canonical
        .values()
        .copied()
        .collect();
    required_alias_feed_ids.sort_unstable();

    log::info!(
        "Autonom: required alias feed_ids from config: {:?}",
        required_alias_feed_ids
    );

    Ok(move || -> Pin<Box<dyn Future<Output = Result<Option<PollerPayload>, anyhow::Error>> + Send>> {
        let db_pool = db_pool.clone();
        let alias_mapping = alias_mapping.clone();
        let required = required_alias_feed_ids.clone();
        Box::pin(async move {
            let batch =
                match db::get_latest_oracle_batch_by_provider(&db_pool, AUTONOM_PROVIDER).await? {
                    Some(b) => b,
                    None => return Ok(None),
                };

            let batch_id = batch.oracle_batch_id;
            let result =
                build_autonom_batch_prices_from_db(batch, &required, &alias_mapping)?;

            Ok(Some(PollerPayload {
                dedup_key: batch_id.to_string(),
                update: ProviderUpdate::Autonom(result),
            }))
        })
    })
}

fn build_autonom_batch_prices_from_db(
    db_batch: OracleBatch,
    required_alias_feed_ids: &[u8],
    alias_mapping: &AutonomAliasMapping,
) -> Result<BatchPrices, anyhow::Error> {
    if db_batch.provider != AUTONOM_PROVIDER {
        return Err(anyhow::anyhow!(
            "received non-autonom batch from DB provider={} batch_id={}",
            db_batch.provider,
            db_batch.oracle_batch_id
        ));
    }

    let (signature, recovery_id) = db_batch.decode_signature()?;

    // CRITICAL: Preserve the DB insertion order (oracle_batch_price_id ASC).
    // The API signs the batch in its response order. The on-chain
    // build_message_hash() reconstructs the hash in the order prices appear.
    // Reordering would break signature verification (InvalidOracleSignature).
    let mut entries_by_alias = HashMap::<u8, PriceData>::new();
    let mut insertion_order: Vec<u8> = Vec::new();

    for price in db_batch.prices {
        if price.exponent != -10 {
            return Err(anyhow::anyhow!(
                "unsupported autonom exponent {} for feed_id {} in batch_id {}",
                price.exponent,
                price.feed_id,
                db_batch.oracle_batch_id
            ));
        }

        let Some(alias_feed_id) = resolve_alias_feed_id(
            price.feed_id,
            price.source_feed_id.as_deref(),
            alias_mapping,
        ) else {
            log::warn!(
                "Skipping autonom DB price feed_id={} source_feed_id={:?} symbol={:?} (no alias mapping)",
                price.feed_id,
                price.source_feed_id,
                price.symbol
            );
            continue;
        };

        let price_value = price.price.to_u64().ok_or_else(|| {
            anyhow::anyhow!(
                "failed to convert autonom price {} to u64 for alias feed_id {}",
                price.price,
                alias_feed_id
            )
        })?;

        let new_price = PriceData {
            feed_id: alias_feed_id,
            price: price_value,
            timestamp: price.price_timestamp.timestamp(),
        };

        match entries_by_alias.get_mut(&alias_feed_id) {
            Some(existing) => {
                if new_price.timestamp > existing.timestamp {
                    *existing = new_price;
                }
            }
            None => {
                insertion_order.push(alias_feed_id);
                entries_by_alias.insert(alias_feed_id, new_price);
            }
        }
    }

    let required: HashSet<u8> = required_alias_feed_ids.iter().copied().collect();
    let provided: HashSet<u8> = entries_by_alias.keys().copied().collect();

    let mut missing: Vec<u8> = required.difference(&provided).copied().collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        return Err(anyhow::anyhow!(
            "autonom DB batch {} missing required alias feed_ids {:?}",
            db_batch.oracle_batch_id,
            missing
        ));
    }

    // Build prices in DB insertion order (preserved via insertion_order vec)
    let prices: Vec<PriceData> = insertion_order
        .iter()
        .filter_map(|alias_id| entries_by_alias.get(alias_id).copied())
        .collect();

    Ok(BatchPrices {
        prices,
        signature,
        recovery_id,
    })
}

fn resolve_alias_feed_id(
    feed_id: i32,
    source_feed_id: Option<&str>,
    alias_mapping: &AutonomAliasMapping,
) -> Option<u8> {
    if let Ok(alias_feed_id) = u8::try_from(feed_id) {
        if (AUTONOM_MIN_FEED_ID..=AUTONOM_MAX_FEED_ID).contains(&alias_feed_id) {
            return Some(alias_feed_id);
        }
    }

    if feed_id >= 0 {
        let canonical = feed_id as u64;
        if let Some(alias_feed_id) = alias_mapping.alias_by_canonical.get(&canonical).copied() {
            return Some(alias_feed_id);
        }
    }

    if let Some(raw_source_feed_id) = source_feed_id {
        if let Ok(canonical) = raw_source_feed_id.parse::<u64>() {
            if let Some(alias_feed_id) = alias_mapping.alias_by_canonical.get(&canonical).copied() {
                return Some(alias_feed_id);
            }
        }
    }

    None
}

fn load_feed_map(feed_map_path: &str) -> Result<AutonomAliasMapping, anyhow::Error> {
    let file_contents = fs::read_to_string(feed_map_path)
        .with_context(|| format!("failed to read autonom feed map file `{feed_map_path}`"))?;

    let parsed: AutonomFeedMapFile = serde_json::from_str(&file_contents)
        .with_context(|| format!("failed to parse autonom feed map json from `{feed_map_path}`"))?;

    let raw_entries: Vec<(u8, u64)> = match parsed {
        AutonomFeedMapFile::Bare(entries) => entries
            .into_iter()
            .map(|entry| (entry.adrena_feed_id, entry.autonom_feed_id))
            .collect(),
        AutonomFeedMapFile::Versioned {
            _schema_version: _,
            autonom_feed_map,
        } => autonom_feed_map
            .into_iter()
            .map(|entry| (entry.adrena_feed_id, entry.autonom_feed_id))
            .collect(),
    };

    if raw_entries.is_empty() {
        return Err(anyhow::anyhow!(
            "autonom feed map is empty: `{feed_map_path}`"
        ));
    }

    let mut alias_by_canonical = HashMap::new();
    let mut seen_adrena_feed_ids = HashSet::new();
    let mut seen_autonom_feed_ids = HashSet::new();

    for (adrena_feed_id, autonom_feed_id) in raw_entries {
        if !(AUTONOM_MIN_FEED_ID..=AUTONOM_MAX_FEED_ID).contains(&adrena_feed_id) {
            return Err(anyhow::anyhow!(
                "autonom adrena_feed_id {} outside expected range {}..={}",
                adrena_feed_id,
                AUTONOM_MIN_FEED_ID,
                AUTONOM_MAX_FEED_ID
            ));
        }

        if !seen_adrena_feed_ids.insert(adrena_feed_id) {
            return Err(anyhow::anyhow!(
                "duplicate autonom adrena_feed_id {} in feed map",
                adrena_feed_id
            ));
        }

        if !seen_autonom_feed_ids.insert(autonom_feed_id) {
            return Err(anyhow::anyhow!(
                "duplicate autonom_feed_id {} in feed map",
                autonom_feed_id
            ));
        }

        alias_by_canonical.insert(autonom_feed_id, adrena_feed_id);
    }

    Ok(AutonomAliasMapping { alias_by_canonical })
}
