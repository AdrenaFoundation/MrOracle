use {
    crate::{db::OracleBatch, provider_updates::ProviderUpdate},
    adrena_abi::oracle::{ChaosLabsBatchPrices, PriceData},
    anchor_client::Program,
    anyhow::Context,
    deadpool_postgres::Pool as DbPool,
    rust_decimal::prelude::ToPrimitive,
    serde::Deserialize,
    solana_sdk::signature::Keypair,
    std::{
        collections::{HashMap, HashSet},
        fs,
        sync::Arc,
        time::Duration,
    },
    tokio::sync::mpsc,
};

const AUTONOM_PROVIDER: &str = "autonom";
const AUTONOM_MIN_FEED_ID: u8 = 30;
const AUTONOM_MAX_FEED_ID: u8 = 141;

const ADRENA_ORACLE_DISCRIMINATOR_LEN: usize = 8;
const ADRENA_ORACLE_HEADER_LEN: usize = 16;
const ADRENA_ORACLE_PRICE_SLOT_LEN: usize = 64;
const ADRENA_ORACLE_PRICE_FEED_ID_OFFSET: usize = 28;
const ADRENA_ORACLE_PRICE_NAME_LEN_OFFSET: usize = 63;

#[derive(Debug, Clone)]
pub struct AutonomRuntimeConfig {
    pub db_pool: DbPool,
    pub feed_map_path: String,
    pub poll_interval: Duration,
}

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

pub async fn run_autonom_keeper(
    program: Program<Arc<Keypair>>,
    config: AutonomRuntimeConfig,
    update_sender: mpsc::Sender<ProviderUpdate>,
) -> Result<(), anyhow::Error> {
    let alias_mapping = load_feed_map(&config.feed_map_path)?;
    let mut last_emitted_batch_id: Option<i64> = None;

    loop {
        if let Err(err) = run_autonom_cycle(
            &program,
            &config,
            &alias_mapping,
            &update_sender,
            &mut last_emitted_batch_id,
        )
        .await
        {
            log::error!("Autonom cycle failed: {:?}", err);
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}

async fn run_autonom_cycle(
    program: &Program<Arc<Keypair>>,
    config: &AutonomRuntimeConfig,
    alias_mapping: &AutonomAliasMapping,
    update_sender: &mpsc::Sender<ProviderUpdate>,
    last_emitted_batch_id: &mut Option<i64>,
) -> Result<(), anyhow::Error> {
    let required_alias_feed_ids = fetch_required_autonom_alias_feed_ids(program).await?;
    if required_alias_feed_ids.is_empty() {
        log::warn!("No registered Autonom feed ids found in oracle account; skipping cycle");
        return Ok(());
    }

    let db_batch = match fetch_latest_autonom_batch_from_db(&config.db_pool).await? {
        Some(batch) => batch,
        None => {
            log::warn!("No autonom batch found in DB; skipping cycle");
            return Ok(());
        }
    };

    if *last_emitted_batch_id == Some(db_batch.oracle_batch_id) {
        log::debug!(
            "Skipping autonom batch {} (already emitted)",
            db_batch.oracle_batch_id
        );
        return Ok(());
    }

    let current_batch_id = db_batch.oracle_batch_id;
    let batch =
        build_autonom_batch_prices_from_db(db_batch, &required_alias_feed_ids, alias_mapping)?;

    update_sender
        .send(ProviderUpdate::Autonom(batch))
        .await
        .map_err(|_| anyhow::anyhow!("autonom provider update channel closed"))?;
    *last_emitted_batch_id = Some(current_batch_id);

    Ok(())
}

async fn fetch_required_autonom_alias_feed_ids(
    program: &Program<Arc<Keypair>>,
) -> Result<Vec<u8>, anyhow::Error> {
    let oracle_pda = adrena_abi::pda::get_oracle_pda().0;
    let account_data = program
        .rpc()
        .get_account_data(&oracle_pda)
        .await
        .with_context(|| format!("failed to fetch oracle account data at {oracle_pda}"))?;

    Ok(parse_registered_autonom_feed_ids(&account_data))
}

async fn fetch_latest_autonom_batch_from_db(
    db_pool: &DbPool,
) -> Result<Option<OracleBatch>, anyhow::Error> {
    crate::db::get_latest_oracle_batch_by_provider(db_pool, AUTONOM_PROVIDER).await
}

fn parse_registered_autonom_feed_ids(account_data: &[u8]) -> Vec<u8> {
    if account_data.len() <= ADRENA_ORACLE_DISCRIMINATOR_LEN + ADRENA_ORACLE_HEADER_LEN {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let mut offset = ADRENA_ORACLE_DISCRIMINATOR_LEN + ADRENA_ORACLE_HEADER_LEN;
    while offset + ADRENA_ORACLE_PRICE_SLOT_LEN <= account_data.len() {
        let feed_id = account_data[offset + ADRENA_ORACLE_PRICE_FEED_ID_OFFSET];
        let name_len = account_data[offset + ADRENA_ORACLE_PRICE_NAME_LEN_OFFSET];

        if name_len > 0
            && (AUTONOM_MIN_FEED_ID..=AUTONOM_MAX_FEED_ID).contains(&feed_id)
            && seen.insert(feed_id)
        {
            out.push(feed_id);
        }

        offset += ADRENA_ORACLE_PRICE_SLOT_LEN;
    }

    out.sort_unstable();
    out
}

fn build_autonom_batch_prices_from_db(
    db_batch: OracleBatch,
    required_alias_feed_ids: &[u8],
    alias_mapping: &AutonomAliasMapping,
) -> Result<ChaosLabsBatchPrices, anyhow::Error> {
    if db_batch.provider != AUTONOM_PROVIDER {
        return Err(anyhow::anyhow!(
            "received non-autonom batch from DB provider={} batch_id={}",
            db_batch.provider,
            db_batch.oracle_batch_id
        ));
    }

    let mut entries_by_alias = HashMap::<u8, PriceData>::new();

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

    let signature_hex = db_batch
        .signature
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "autonom DB batch {} has no signature",
                db_batch.oracle_batch_id
            )
        })?
        .trim_start_matches("0x");
    let signature_bytes =
        hex::decode(signature_hex).context("failed to decode autonom signature hex")?;
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("autonom signature must be 64 bytes"))?;

    let recovery_id = db_batch
        .recovery_id
        .ok_or_else(|| {
            anyhow::anyhow!(
                "autonom DB batch {} has no recovery_id",
                db_batch.oracle_batch_id
            )
        })
        .and_then(|id| {
            u8::try_from(id).map_err(|_| anyhow::anyhow!("invalid autonom recovery_id {}", id))
        })?;

    let mut prices: Vec<PriceData> = entries_by_alias.into_values().collect();
    prices.sort_by_key(|price| price.feed_id);

    Ok(ChaosLabsBatchPrices {
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
