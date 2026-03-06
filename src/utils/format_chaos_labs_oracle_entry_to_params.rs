use {
    crate::db::{get_assets_prices::AssetsPrices, OracleBatch},
    adrena_abi::oracle::{BatchPrices, PriceData},
    anyhow::Context,
    rust_decimal::prelude::ToPrimitive,
    serde::Deserialize,
    std::{
        collections::HashSet,
        fs,
    },
};

const CHAOSLABS_MIN_FEED_ID: u8 = 0;
const CHAOSLABS_MAX_FEED_ID: u8 = 29;

#[derive(Debug, Clone)]
pub struct ChaosLabsFeedBinding {
    pub adrena_feed_id: u8,
    price_field: ChaosLabsPriceField,
    timestamp_field: ChaosLabsTimestampField,
}

#[derive(Debug, Deserialize)]
struct ChaosLabsFeedMapEntryRaw {
    adrena_feed_id: u8,
    source_price_field: String,
    source_timestamp_field: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ChaosLabsFeedMapFile {
    Bare(Vec<ChaosLabsFeedMapEntryRaw>),
    Versioned {
        _schema_version: Option<String>,
        chaoslabs_feed_map: Vec<ChaosLabsFeedMapEntryRaw>,
    },
}

#[derive(Debug, Clone, Copy)]
enum ChaosLabsPriceField {
    SolUsdPrice,
    JitoSolUsdPrice,
    BtcUsdPrice,
    WbtcUsdPrice,
    BonkUsdPrice,
    UsdcUsdPrice,
}

#[derive(Debug, Clone, Copy)]
enum ChaosLabsTimestampField {
    SolUsdPriceTs,
    JitoSolUsdPriceTs,
    BtcUsdPriceTs,
    WbtcUsdPriceTs,
    BonkUsdPriceTs,
    UsdcUsdPriceTs,
}

impl ChaosLabsPriceField {
    fn parse(field: &str) -> Result<Self, anyhow::Error> {
        match field {
            "solusd_price" => Ok(Self::SolUsdPrice),
            "jitosolusd_price" => Ok(Self::JitoSolUsdPrice),
            "btcusd_price" => Ok(Self::BtcUsdPrice),
            "wbtcusd_price" => Ok(Self::WbtcUsdPrice),
            "bonkusd_price" => Ok(Self::BonkUsdPrice),
            "usdcusd_price" => Ok(Self::UsdcUsdPrice),
            _ => Err(anyhow::anyhow!(
                "unsupported chaoslabs source_price_field `{}`",
                field
            )),
        }
    }

    fn read(self, entry: &AssetsPrices) -> Result<u64, anyhow::Error> {
        let value = match self {
            Self::SolUsdPrice => entry.solusd_price.to_u64(),
            Self::JitoSolUsdPrice => entry.jitosolusd_price.to_u64(),
            Self::BtcUsdPrice => entry.btcusd_price.to_u64(),
            Self::WbtcUsdPrice => entry.wbtcusd_price.to_u64(),
            Self::BonkUsdPrice => entry.bonkusd_price.to_u64(),
            Self::UsdcUsdPrice => entry.usdcusd_price.to_u64(),
        };

        value.ok_or_else(|| anyhow::anyhow!("failed to convert chaoslabs price to u64"))
    }
}

impl ChaosLabsTimestampField {
    fn parse(field: &str) -> Result<Self, anyhow::Error> {
        match field {
            "solusd_price_ts" => Ok(Self::SolUsdPriceTs),
            "jitosolusd_price_ts" => Ok(Self::JitoSolUsdPriceTs),
            "btcusd_price_ts" => Ok(Self::BtcUsdPriceTs),
            "wbtcusd_price_ts" => Ok(Self::WbtcUsdPriceTs),
            "bonkusd_price_ts" => Ok(Self::BonkUsdPriceTs),
            "usdcusd_price_ts" => Ok(Self::UsdcUsdPriceTs),
            _ => Err(anyhow::anyhow!(
                "unsupported chaoslabs source_timestamp_field `{}`",
                field
            )),
        }
    }

    fn read(self, entry: &AssetsPrices) -> i64 {
        match self {
            Self::SolUsdPriceTs => entry.solusd_price_ts.timestamp(),
            Self::JitoSolUsdPriceTs => entry.jitosolusd_price_ts.timestamp(),
            Self::BtcUsdPriceTs => entry.btcusd_price_ts.timestamp(),
            Self::WbtcUsdPriceTs => entry.wbtcusd_price_ts.timestamp(),
            Self::BonkUsdPriceTs => entry.bonkusd_price_ts.timestamp(),
            Self::UsdcUsdPriceTs => entry.usdcusd_price_ts.timestamp(),
        }
    }
}

pub fn load_chaos_labs_feed_map(
    chaoslabs_feed_map_path: &str,
) -> Result<Vec<ChaosLabsFeedBinding>, anyhow::Error> {
    let file_contents = fs::read_to_string(chaoslabs_feed_map_path).with_context(|| {
        format!("failed to read chaoslabs feed map file `{chaoslabs_feed_map_path}`")
    })?;

    let parsed: ChaosLabsFeedMapFile = serde_json::from_str(&file_contents).with_context(|| {
        format!("failed to parse chaoslabs feed map json from `{chaoslabs_feed_map_path}`")
    })?;

    let raw_entries = match parsed {
        ChaosLabsFeedMapFile::Bare(entries) => entries,
        ChaosLabsFeedMapFile::Versioned {
            _schema_version: _,
            chaoslabs_feed_map,
        } => chaoslabs_feed_map,
    };

    if raw_entries.is_empty() {
        return Err(anyhow::anyhow!(
            "chaoslabs feed map is empty: `{chaoslabs_feed_map_path}`"
        ));
    }

    let mut seen_feed_ids = HashSet::new();
    let mut bindings = Vec::with_capacity(raw_entries.len());

    for entry in raw_entries {
        if !(CHAOSLABS_MIN_FEED_ID..=CHAOSLABS_MAX_FEED_ID).contains(&entry.adrena_feed_id) {
            return Err(anyhow::anyhow!(
                "invalid chaoslabs adrena_feed_id {} (expected {}..={})",
                entry.adrena_feed_id,
                CHAOSLABS_MIN_FEED_ID,
                CHAOSLABS_MAX_FEED_ID
            ));
        }

        if !seen_feed_ids.insert(entry.adrena_feed_id) {
            return Err(anyhow::anyhow!(
                "duplicate chaoslabs adrena_feed_id {} in mapping",
                entry.adrena_feed_id
            ));
        }

        let price_field = ChaosLabsPriceField::parse(&entry.source_price_field)?;
        let timestamp_field = ChaosLabsTimestampField::parse(&entry.source_timestamp_field)?;

        bindings.push(ChaosLabsFeedBinding {
            adrena_feed_id: entry.adrena_feed_id,
            price_field,
            timestamp_field,
        });
    }

    Ok(bindings)
}

pub fn format_chaos_labs_oracle_entry_to_params(
    chaos_labs_oracle_entry: &AssetsPrices,
    feed_bindings: &[ChaosLabsFeedBinding],
) -> Result<BatchPrices, anyhow::Error> {
    if feed_bindings.is_empty() {
        return Err(anyhow::anyhow!("chaoslabs feed mapping is empty"));
    }

    let signature_hex = chaos_labs_oracle_entry.signature.trim_start_matches("0x");
    let signature_bytes =
        hex::decode(signature_hex).context("failed to decode chaoslabs signature hex")?;
    let signature_vec: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("chaoslabs signature must be 64 bytes"))?;

    let recovery_id = chaos_labs_oracle_entry.recovery_id as u8;

    let mut prices = Vec::with_capacity(feed_bindings.len());
    for binding in feed_bindings {
        prices.push(PriceData {
            feed_id: binding.adrena_feed_id,
            price: binding.price_field.read(chaos_labs_oracle_entry)?,
            timestamp: binding.timestamp_field.read(chaos_labs_oracle_entry),
        });
    }

    Ok(BatchPrices {
        prices,
        signature: signature_vec,
        recovery_id,
    })
}

pub fn format_chaos_labs_oracle_batch_to_params(
    oracle_batch: &OracleBatch,
    feed_bindings: &[ChaosLabsFeedBinding],
) -> Result<BatchPrices, anyhow::Error> {
    if feed_bindings.is_empty() {
        return Err(anyhow::anyhow!("chaoslabs feed mapping is empty"));
    }

    if oracle_batch.provider != "chaoslabs" {
        return Err(anyhow::anyhow!(
            "expected chaoslabs provider batch, got `{}`",
            oracle_batch.provider
        ));
    }

    let (signature_vec, recovery_id) = oracle_batch.decode_signature()?;

    // Build a set of expected feed IDs from the feed map for validation.
    let expected_feed_ids: HashSet<u8> = feed_bindings.iter().map(|b| b.adrena_feed_id).collect();

    // CRITICAL: Iterate over DB prices in insertion order (oracle_batch_price_id ASC).
    // The ChaosLabs API signs the batch in its response order. The on-chain
    // build_message_hash() reconstructs the hash in the order prices appear.
    // Reordering would break signature verification (InvalidOracleSignature).
    let mut prices = Vec::with_capacity(feed_bindings.len());
    let mut seen_feed_ids = HashSet::new();

    for price_row in &oracle_batch.prices {
        let feed_id = u8::try_from(price_row.feed_id).map_err(|_| {
            anyhow::anyhow!(
                "invalid chaoslabs batch feed_id {}; expected u8",
                price_row.feed_id
            )
        })?;

        // Skip feeds not in our feed map
        if !expected_feed_ids.contains(&feed_id) {
            continue;
        }

        if !seen_feed_ids.insert(feed_id) {
            return Err(anyhow::anyhow!(
                "duplicate chaoslabs batch feed_id {} in batch {}",
                feed_id,
                oracle_batch.oracle_batch_id
            ));
        }

        if price_row.exponent != -10 {
            return Err(anyhow::anyhow!(
                "unsupported chaoslabs exponent {} for feed_id {}",
                price_row.exponent,
                feed_id
            ));
        }

        let normalized_price = price_row.price.to_u64().ok_or_else(|| {
            anyhow::anyhow!(
                "failed to convert chaoslabs price {} to u64 for feed_id {}",
                price_row.price,
                feed_id
            )
        })?;

        prices.push(PriceData {
            feed_id,
            price: normalized_price,
            timestamp: price_row.price_timestamp.timestamp(),
        });
    }

    // Verify all expected feeds were found
    let missing: Vec<u8> = expected_feed_ids
        .difference(&seen_feed_ids)
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(anyhow::anyhow!(
            "missing chaoslabs price(s) for feed_id(s) {:?} in batch {}",
            missing,
            oracle_batch.oracle_batch_id
        ));
    }

    Ok(BatchPrices {
        prices,
        signature: signature_vec,
        recovery_id,
    })
}
