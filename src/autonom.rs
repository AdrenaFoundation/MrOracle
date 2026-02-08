use {
    crate::{adrena_ix::ORACLE_PROVIDER_AUTONOM, handlers},
    adrena_abi::oracle::{ChaosLabsBatchPrices, PriceData},
    anchor_client::Program,
    anyhow::Context,
    serde::Deserialize,
    solana_sdk::signature::Keypair,
    std::{
        collections::{HashMap, HashSet},
        fs,
        sync::Arc,
        time::Duration,
    },
    tokio::sync::Mutex,
};

const AUTONOM_MIN_FEED_ID: u8 = 30;
const AUTONOM_MAX_FEED_ID: u8 = 141;

const ADRENA_ORACLE_DISCRIMINATOR_LEN: usize = 8;
const ADRENA_ORACLE_HEADER_LEN: usize = 16;
const ADRENA_ORACLE_PRICE_SLOT_LEN: usize = 64;
const ADRENA_ORACLE_PRICE_FEED_ID_OFFSET: usize = 28;
const ADRENA_ORACLE_PRICE_NAME_LEN_OFFSET: usize = 63;

const YEAR_2100_SECONDS: i64 = 4_102_444_800;

#[derive(Debug, Clone)]
pub struct AutonomRuntimeConfig {
    pub api_url: String,
    pub api_key: String,
    pub feed_map_path: String,
    pub fresh: bool,
    pub poll_interval: Duration,
    pub update_oracle_cu_limit: u32,
}

#[derive(Debug, Clone)]
struct AutonomAliasMapping {
    canonical_by_alias: HashMap<u8, u64>,
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

#[derive(Debug, Deserialize)]
struct AutonomBatchResponse {
    prices: Vec<AutonomPrice>,
    #[serde(default)]
    errors: Vec<AutonomError>,
    signature: String,
    recovery_id: u8,
}

#[derive(Debug, Deserialize)]
struct AutonomPrice {
    feed_id: u64,
    price: u64,
    expo: i32,
    timestamp: i64,
}

#[derive(Debug, Deserialize)]
struct AutonomError {
    feed_id: u64,
    code: String,
    message: String,
}

pub async fn run_autonom_keeper(
    program: Program<Arc<Keypair>>,
    median_priority_fee: Arc<Mutex<u64>>,
    config: AutonomRuntimeConfig,
) -> Result<(), anyhow::Error> {
    let alias_mapping = load_feed_map(&config.feed_map_path)?;

    loop {
        if let Err(err) =
            run_autonom_cycle(&program, &median_priority_fee, &config, &alias_mapping).await
        {
            log::error!("Autonom cycle failed: {:?}", err);
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}

async fn run_autonom_cycle(
    program: &Program<Arc<Keypair>>,
    median_priority_fee: &Arc<Mutex<u64>>,
    config: &AutonomRuntimeConfig,
    alias_mapping: &AutonomAliasMapping,
) -> Result<(), anyhow::Error> {
    let required_alias_feed_ids = fetch_required_autonom_alias_feed_ids(program).await?;
    if required_alias_feed_ids.is_empty() {
        log::warn!("No registered Autonom feed ids found in oracle account; skipping cycle");
        return Ok(());
    }

    let canonical_feed_ids =
        resolve_required_canonical_feed_ids(&required_alias_feed_ids, alias_mapping)?;

    let response = fetch_autonom_batch_response(config, &canonical_feed_ids).await?;
    let batch = build_autonom_batch_prices(response, &required_alias_feed_ids, alias_mapping)?;

    let median_priority_fee = *median_priority_fee.lock().await;

    handlers::update_oracle_with_multi_batch(
        program,
        median_priority_fee,
        config.update_oracle_cu_limit,
        ORACLE_PROVIDER_AUTONOM,
        batch,
    )
    .await?;

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

fn resolve_required_canonical_feed_ids(
    required_alias_feed_ids: &[u8],
    alias_mapping: &AutonomAliasMapping,
) -> Result<Vec<u64>, anyhow::Error> {
    let mut canonical_ids = Vec::with_capacity(required_alias_feed_ids.len());

    for alias_feed_id in required_alias_feed_ids {
        let canonical = alias_mapping
            .canonical_by_alias
            .get(alias_feed_id)
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing alias mapping for required autonom feed_id {}",
                    alias_feed_id
                )
            })?;

        canonical_ids.push(canonical);
    }

    canonical_ids.sort_unstable();
    Ok(canonical_ids)
}

async fn fetch_autonom_batch_response(
    config: &AutonomRuntimeConfig,
    canonical_feed_ids: &[u64],
) -> Result<AutonomBatchResponse, anyhow::Error> {
    let endpoint = format!("{}/prices/batch", config.api_url.trim_end_matches('/'));
    let feed_ids_csv = canonical_feed_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let fresh_param = if config.fresh { "true" } else { "false" }.to_string();
    let api_key = config.api_key.clone();

    tokio::task::spawn_blocking(move || -> Result<AutonomBatchResponse, anyhow::Error> {
        let response = match ureq::get(&endpoint)
            .query("feed_ids", &feed_ids_csv)
            .query("fresh", &fresh_param)
            .set("X-API-Key", &api_key)
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "autonom batch request returned status {} with body: {}",
                    status,
                    body
                ));
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "failed to send autonom batch request: {}",
                    err
                ));
            }
        };

        let body = response
            .into_string()
            .context("failed to read autonom batch response body")?;
        serde_json::from_str::<AutonomBatchResponse>(&body)
            .context("failed to parse autonom batch response json")
    })
    .await
    .context("autonom batch request task failed")?
}

fn build_autonom_batch_prices(
    response: AutonomBatchResponse,
    required_alias_feed_ids: &[u8],
    alias_mapping: &AutonomAliasMapping,
) -> Result<ChaosLabsBatchPrices, anyhow::Error> {
    let mut entries_by_alias = HashMap::<u8, PriceData>::new();

    for price in response.prices {
        if price.expo != -10 {
            return Err(anyhow::anyhow!(
                "unsupported autonom expo {} for canonical feed_id {}",
                price.expo,
                price.feed_id
            ));
        }

        let Some(alias_feed_id) = alias_mapping
            .alias_by_canonical
            .get(&price.feed_id)
            .copied()
        else {
            log::warn!(
                "Skipping autonom price for canonical feed_id {} (no alias mapping)",
                price.feed_id
            );
            continue;
        };

        let normalized_timestamp = normalize_timestamp_to_seconds(price.timestamp);

        let new_price = PriceData {
            feed_id: alias_feed_id,
            price: price.price,
            timestamp: normalized_timestamp,
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
        let error_summary = response
            .errors
            .iter()
            .map(|err| format!("{}:{}:{}", err.feed_id, err.code, err.message))
            .collect::<Vec<_>>()
            .join(" | ");

        return Err(anyhow::anyhow!(
            "autonom batch missing required alias feed_ids {:?}; provider errors: [{}]",
            missing,
            error_summary
        ));
    }

    let signature_hex = response.signature.trim_start_matches("0x");
    let signature_bytes =
        hex::decode(signature_hex).context("failed to decode autonom signature hex")?;
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("autonom signature must be 64 bytes"))?;

    let mut prices: Vec<PriceData> = entries_by_alias.into_values().collect();
    prices.sort_by_key(|price| price.feed_id);

    Ok(ChaosLabsBatchPrices {
        prices,
        signature,
        recovery_id: response.recovery_id,
    })
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

    let mut canonical_by_alias = HashMap::new();
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

        canonical_by_alias.insert(adrena_feed_id, autonom_feed_id);
        alias_by_canonical.insert(autonom_feed_id, adrena_feed_id);
    }

    Ok(AutonomAliasMapping {
        canonical_by_alias,
        alias_by_canonical,
    })
}

fn normalize_timestamp_to_seconds(raw_timestamp: i64) -> i64 {
    let mut normalized = raw_timestamp;
    while normalized > YEAR_2100_SECONDS {
        normalized /= 1000;
    }
    normalized
}
