use {
    crate::{
        adrena_ix::SwitchboardFeedMapEntry,
        db,
        oracle_poller::PollerPayload,
        provider_updates::{ProviderUpdate, SwitchboardOraclePricesUpdate},
    },
    anyhow::Context,
    base64::{engine::general_purpose::STANDARD as BASE64, Engine},
    deadpool_postgres::Pool as DbPool,
    serde::{Deserialize, Serialize},
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    std::{collections::HashSet, fs, future::Future, pin::Pin, str::FromStr, time::Duration},
};

const SWITCHBOARD_MIN_FEED_ID: u8 = 142;
const SWITCHBOARD_MAX_FEED_ID: u8 = 255;

#[derive(Debug, Clone)]
pub struct SwitchboardFeedBinding {
    pub adrena_feed_id: u8,
    pub switchboard_feed_hash_hex: String,
    pub switchboard_feed_hash: [u8; 32],
}

impl SwitchboardFeedBinding {
    pub fn to_feed_map_entry(&self) -> SwitchboardFeedMapEntry {
        SwitchboardFeedMapEntry {
            adrena_feed_id: self.adrena_feed_id,
            switchboard_feed_hash: self.switchboard_feed_hash,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SwitchboardRuntimeConfig {
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub poll_interval: Duration,
    pub feed_bindings: Vec<SwitchboardFeedBinding>,
    pub instruction_idx: u32,
}

// ---------------------------------------------------------------------------
// JSON deserialization for DB JSONB columns (matches adrena-data SerializedInstruction)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SidecarAccountMeta {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SidecarInstruction {
    program_id: String,
    accounts: Vec<SidecarAccountMeta>,
    data: String, // base64-encoded
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SidecarOutput {
    ed25519_ix: SidecarInstruction,
    quote_store_ix: SidecarInstruction,
    quote_account: String, // base58-encoded pubkey
}

pub(crate) fn deserialize_instruction(raw: SidecarInstruction) -> Result<Instruction, anyhow::Error> {
    let program_id = Pubkey::from_str(&raw.program_id)
        .with_context(|| format!("invalid instruction program_id: {}", raw.program_id))?;

    let accounts = raw
        .accounts
        .into_iter()
        .map(|a| {
            let pubkey = Pubkey::from_str(&a.pubkey)
                .with_context(|| format!("invalid account pubkey: {}", a.pubkey))?;
            Ok(AccountMeta {
                pubkey,
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
        })
        .collect::<Result<Vec<_>, anyhow::Error>>()?;

    let data = BASE64
        .decode(&raw.data)
        .context("invalid base64 instruction data")?;

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

// ---------------------------------------------------------------------------
// Feed map config loading (unchanged — still reads switchboard_feed_map.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SwitchboardFeedBindingRaw {
    adrena_feed_id: u8,
    switchboard_feed_hash: String,
    // Accepted but ignored for backwards compatibility
    #[serde(default)]
    #[allow(dead_code)]
    pull_feed_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SwitchboardFeedMapFile {
    Bare(Vec<SwitchboardFeedBindingRaw>),
    Versioned {
        _schema_version: Option<String>,
        switchboard_feed_map: Vec<SwitchboardFeedBindingRaw>,
    },
}

pub fn load_switchboard_feed_bindings(
    switchboard_feed_map_path: &str,
) -> Result<Vec<SwitchboardFeedBinding>, anyhow::Error> {
    let file_contents = fs::read_to_string(switchboard_feed_map_path).with_context(|| {
        format!("failed to read switchboard feed map file `{switchboard_feed_map_path}`")
    })?;

    let parsed: SwitchboardFeedMapFile =
        serde_json::from_str(&file_contents).with_context(|| {
            format!(
                "failed to parse switchboard feed map json from `{switchboard_feed_map_path}`"
            )
        })?;

    let raw_entries = match parsed {
        SwitchboardFeedMapFile::Bare(entries) => entries,
        SwitchboardFeedMapFile::Versioned {
            _schema_version: _,
            switchboard_feed_map,
        } => switchboard_feed_map,
    };

    if raw_entries.is_empty() {
        return Err(anyhow::anyhow!(
            "switchboard feed map is empty: `{switchboard_feed_map_path}`"
        ));
    }

    let mut seen_feed_ids = HashSet::new();
    let mut seen_hashes = HashSet::new();
    let mut bindings = Vec::with_capacity(raw_entries.len());

    for entry in raw_entries {
        if !(SWITCHBOARD_MIN_FEED_ID..=SWITCHBOARD_MAX_FEED_ID).contains(&entry.adrena_feed_id) {
            return Err(anyhow::anyhow!(
                "invalid switchboard adrena_feed_id {} (expected {}..={})",
                entry.adrena_feed_id,
                SWITCHBOARD_MIN_FEED_ID,
                SWITCHBOARD_MAX_FEED_ID
            ));
        }

        let normalized_hash = normalize_feed_hash(&entry.switchboard_feed_hash);
        let decoded_hash = hex::decode(&normalized_hash).with_context(|| {
            format!(
                "failed to decode switchboard feed hash `{}` for adrena_feed_id {}",
                entry.switchboard_feed_hash, entry.adrena_feed_id
            )
        })?;

        let switchboard_feed_hash: [u8; 32] = decoded_hash.try_into().map_err(|_| {
            anyhow::anyhow!(
                "invalid switchboard feed hash length for adrena_feed_id {} (expected 32 bytes)",
                entry.adrena_feed_id
            )
        })?;

        if !seen_feed_ids.insert(entry.adrena_feed_id) {
            return Err(anyhow::anyhow!(
                "duplicate switchboard adrena_feed_id {} in mapping",
                entry.adrena_feed_id
            ));
        }

        if !seen_hashes.insert(normalized_hash.clone()) {
            return Err(anyhow::anyhow!(
                "duplicate switchboard feed hash `{}` in mapping",
                normalized_hash
            ));
        }

        bindings.push(SwitchboardFeedBinding {
            adrena_feed_id: entry.adrena_feed_id,
            switchboard_feed_hash_hex: normalized_hash,
            switchboard_feed_hash,
        });
    }

    bindings.sort_by_key(|entry| entry.adrena_feed_id);
    Ok(bindings)
}

// ---------------------------------------------------------------------------
// Switchboard cycle function for generic poller
// ---------------------------------------------------------------------------

pub fn make_switchboard_cycle(
    db_pool: DbPool,
    config: SwitchboardRuntimeConfig,
) -> impl Fn() -> Pin<Box<dyn Future<Output = Result<Option<PollerPayload>, anyhow::Error>> + Send>>
       + Send
       + Sync {
    let feed_map: Vec<SwitchboardFeedMapEntry> = config
        .feed_bindings
        .iter()
        .map(SwitchboardFeedBinding::to_feed_map_entry)
        .collect();

    move || {
        let db_pool = db_pool.clone();
        let config = config.clone();
        let feed_map = feed_map.clone();
        Box::pin(async move {
            let batch =
                match db::get_latest_oracle_batch_by_provider(&db_pool, "switchboard").await? {
                    Some(b) => b,
                    None => return Ok(None),
                };

            let batch_id = batch.oracle_batch_id;
            let metadata = batch.metadata.ok_or_else(|| {
                anyhow::anyhow!(
                    "switchboard oracle_batch {} has no metadata",
                    batch_id
                )
            })?;

            let output: SidecarOutput = serde_json::from_value(metadata)
                .context("failed to deserialize switchboard metadata as SidecarOutput")?;

            let update = build_update_from_sidecar(output, &config, &feed_map)?;

            Ok(Some(PollerPayload {
                dedup_key: batch_id.to_string(),
                update: ProviderUpdate::Switchboard(update),
            }))
        })
    }
}

pub(crate) fn build_update_from_sidecar(
    output: SidecarOutput,
    config: &SwitchboardRuntimeConfig,
    switchboard_feed_map: &[SwitchboardFeedMapEntry],
) -> Result<SwitchboardOraclePricesUpdate, anyhow::Error> {
    let ed25519_ix = deserialize_instruction(output.ed25519_ix)?;
    let quote_store_ix = deserialize_instruction(output.quote_store_ix)?;
    let quote_account =
        Pubkey::from_str(&output.quote_account).context("invalid quote_account pubkey")?;

    Ok(SwitchboardOraclePricesUpdate {
        queue_pubkey: config.queue_pubkey,
        max_age_slots: config.max_age_slots,
        feed_map: switchboard_feed_map.to_vec(),
        ed25519_ix,
        quote_store_ix,
        quote_account,
    })
}

fn normalize_feed_hash(hash: &str) -> String {
    hash.trim().trim_start_matches("0x").to_ascii_lowercase()
}
