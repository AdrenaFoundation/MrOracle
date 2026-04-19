//! Switchboard provider cycle.
//!
//! Reads the latest `oracle_batches` row where provider='switchboard', then
//! deserializes the `metadata` JSON blob (written by adrena-data's
//! processSwitchboardInstructions.ts) to reconstitute:
//!   - ed25519_ix: the Ed25519 signature-verify instruction produced by
//!     `queue.fetchManagedUpdateIxs` off-chain
//!   - quote_store_ix: the Switchboard quote program's store instruction
//!   - quote_account: the canonical SwitchboardQuote PDA
//!
//! These three pieces are wrapped in `SwitchboardOraclePricesUpdate` and sent
//! over the dispatch channel. The update_pool_aum handler later splices them
//! into the tx at positions [ed25519, quote_store, update_pool_aum].
//!
//! The feed_map used by the on-chain verify path is loaded separately from a
//! local JSON config file. The metadata's `quote_slot` is used as the poller
//! dedup key so cycles that re-read the same (already-published) quote skip
//! emitting redundant updates.

use {
    crate::{
        adrena_ix::SwitchboardFeedMapEntry,
        db,
        oracle_poller::{CycleFn, PollerPayload},
        provider_updates::{ProviderUpdate, SwitchboardOraclePricesUpdate},
    },
    adrena_abi::{
        feed_ids::SWITCHBOARD_RANGE,
        feed_maps::{SWITCHBOARD_DEVNET_JSON, SWITCHBOARD_MAINNET_JSON},
    },
    anyhow::{anyhow, Context},
    base64::{engine::general_purpose::STANDARD as BASE64, Engine},
    deadpool_postgres::Pool as DbPool,
    serde::Deserialize,
    sha2::{Digest, Sha256},
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    std::{fs, str::FromStr},
};

const SWITCHBOARD_PROVIDER: &str = "switchboard";
// Source of truth lives in adrena_abi::feed_ids::SWITCHBOARD_RANGE — see also
// `OracleProvider::Switchboard.feed_id_range()` in adrena-abi/src/oracle.rs.
const SWITCHBOARD_MIN_FEED_ID: u8 = SWITCHBOARD_RANGE.0;
const SWITCHBOARD_MAX_FEED_ID: u8 = SWITCHBOARD_RANGE.1;

/// Cluster selector for the embedded Switchboard feed map. Defaults to mainnet
/// when no explicit override is provided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchboardCluster {
    Mainnet,
    Devnet,
}

impl SwitchboardCluster {
    pub fn embedded_json(&self) -> &'static str {
        match self {
            SwitchboardCluster::Mainnet => SWITCHBOARD_MAINNET_JSON,
            SwitchboardCluster::Devnet => SWITCHBOARD_DEVNET_JSON,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SwitchboardCluster::Mainnet => "mainnet",
            SwitchboardCluster::Devnet => "devnet",
        }
    }
}

impl FromStr for SwitchboardCluster {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "mainnet" | "main" => Ok(SwitchboardCluster::Mainnet),
            "devnet" | "dev" => Ok(SwitchboardCluster::Devnet),
            other => Err(anyhow!("invalid switchboard cluster `{other}` (expected mainnet|devnet)")),
        }
    }
}

fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

// ─────────────────────────────────────────────────────────────────────────────
// Switchboard feed map runtime config + JSON parser. Default source is the
// embedded adrena-abi central map (see `load_switchboard_feed_map_embedded`
// below); operator may supply an override file via --switchboard-feed-map-path
// (handled by `load_switchboard_feed_map`). Both routes parse the same JSON
// shape via `parse_feed_map`.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SwitchboardRuntimeConfig {
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
}

#[derive(Debug, Deserialize)]
struct SwitchboardFeedMapFile {
    #[serde(default)]
    _schema_version: Option<String>,
    switchboard_feed_map: Vec<SwitchboardFeedMapRaw>,
}

#[derive(Debug, Deserialize)]
struct SwitchboardFeedMapRaw {
    adrena_feed_id: u8,
    switchboard_feed_hash: String,
    /// Set to true in the central adrena-abi map for placeholder feed hashes
    /// (currently slots 143/144 = jitoSOL/BTC). Active feeds with `_DUMMY: true`
    /// are rejected at startup so production cannot accidentally publish a
    /// fake price.
    #[serde(default, rename = "_DUMMY")]
    dummy: bool,
}

/// Load the central adrena-abi switchboard feed map for the given cluster. Use
/// this when no `--switchboard-feed-map-path` override is supplied.
///
/// Dummy slots (`_DUMMY: true`) in the central map are **automatically skipped
/// with a warning log per slot**. To activate a slot that's currently marked
/// dummy, the operator must either (a) push a non-dummy version of the central
/// map to adrena-abi (which also requires updating
/// `tests/no_dummy_in_production.rs` ALLOWED_DUMMY_FEED_IDS), or (b) supply a
/// real-hash override file via `--switchboard-feed-map-path`.
pub fn load_switchboard_feed_map_embedded(
    cluster: SwitchboardCluster,
    queue_pubkey: Pubkey,
    max_age_slots: u64,
) -> anyhow::Result<SwitchboardRuntimeConfig> {
    let json = cluster.embedded_json();
    log::info!(
        "switchboard: loading embedded {} feed map from adrena-abi (sha256={})",
        cluster.label(),
        sha256_hex(json),
    );
    parse_feed_map(json, queue_pubkey, max_age_slots, /*skip_dummy=*/ true)
        .with_context(|| format!("parse embedded switchboard {} feed map", cluster.label()))
}

/// Load a switchboard feed map from a local file path. Logs the path and the
/// sha256 of the file contents so override drift is visible in service logs.
///
/// **Override files MUST NOT contain `_DUMMY: true` entries** — operator-
/// supplied files are expected to be production-grade. Any dummy slot in an
/// override file is treated as a hard error at startup (you don't choose a
/// custom file just to ship placeholder hashes).
pub fn load_switchboard_feed_map(
    feed_map_path: &str,
    queue_pubkey: Pubkey,
    max_age_slots: u64,
) -> anyhow::Result<SwitchboardRuntimeConfig> {
    let raw = fs::read_to_string(feed_map_path)
        .with_context(|| format!("read switchboard feed map at `{feed_map_path}`"))?;
    log::warn!(
        "switchboard: using OVERRIDE feed map path `{feed_map_path}` (sha256={}) — not the pinned adrena-abi artifact",
        sha256_hex(&raw),
    );
    parse_feed_map(&raw, queue_pubkey, max_age_slots, /*skip_dummy=*/ false)
        .with_context(|| format!("parse switchboard feed map at `{feed_map_path}`"))
}

/// Internal: parse a feed-map JSON string and validate every entry.
///
/// If `skip_dummy` is true (embedded mode), entries with `_DUMMY: true` are
/// dropped with a warning log per slot. If false (override mode), dummies
/// are a hard error.
///
/// Other hard errors (any of these abort startup):
///   * empty feed map (after dummy filtering, if applicable)
///   * adrena_feed_id outside the Switchboard 142..=255 range
///   * malformed switchboard_feed_hash (non-hex, wrong length)
///   * duplicate adrena_feed_id
fn parse_feed_map(
    raw: &str,
    queue_pubkey: Pubkey,
    max_age_slots: u64,
    skip_dummy: bool,
) -> anyhow::Result<SwitchboardRuntimeConfig> {
    // Support both the bare array form (legacy) and the wrapped form with
    // `_schema_version` + `switchboard_feed_map`.
    let entries: Vec<SwitchboardFeedMapRaw> = match serde_json::from_str::<SwitchboardFeedMapFile>(raw) {
        Ok(parsed) => parsed.switchboard_feed_map,
        Err(_) => serde_json::from_str(raw).context("parse switchboard feed map JSON")?,
    };

    if entries.is_empty() {
        return Err(anyhow!("switchboard feed map is empty"));
    }

    let mut feed_map = Vec::with_capacity(entries.len());
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if !(SWITCHBOARD_MIN_FEED_ID..=SWITCHBOARD_MAX_FEED_ID).contains(&entry.adrena_feed_id) {
            return Err(anyhow!(
                "switchboard adrena_feed_id {} outside range {SWITCHBOARD_MIN_FEED_ID}..={SWITCHBOARD_MAX_FEED_ID}",
                entry.adrena_feed_id
            ));
        }

        if entry.dummy {
            if skip_dummy {
                log::warn!(
                    "switchboard adrena_feed_id {} is marked _DUMMY: true in the central map — skipping. \
                     This slot is NOT being submitted to chain. To activate, supply a real-hash override \
                     via --switchboard-feed-map-path or unmark in adrena-abi.",
                    entry.adrena_feed_id
                );
                continue;
            } else {
                return Err(anyhow!(
                    "switchboard adrena_feed_id {} is marked _DUMMY: true in the override file. \
                     Override files must contain real production hashes — remove the _DUMMY flag or use \
                     a different file.",
                    entry.adrena_feed_id
                ));
            }
        }

        if !seen.insert(entry.adrena_feed_id) {
            return Err(anyhow!(
                "duplicate switchboard adrena_feed_id {}",
                entry.adrena_feed_id
            ));
        }

        let hash_hex = entry.switchboard_feed_hash.trim_start_matches("0x").to_ascii_lowercase();
        let hash_bytes = hex::decode(&hash_hex)
            .with_context(|| format!("decode switchboard_feed_hash {hash_hex}"))?;
        if hash_bytes.len() != 32 {
            return Err(anyhow!(
                "switchboard_feed_hash length is {} (expected 32)",
                hash_bytes.len()
            ));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&hash_bytes);

        feed_map.push(SwitchboardFeedMapEntry {
            adrena_feed_id: entry.adrena_feed_id,
            switchboard_feed_hash: hash,
        });
    }

    if feed_map.is_empty() {
        return Err(anyhow!(
            "switchboard feed map is empty after dummy filtering — every entry was marked _DUMMY: true",
        ));
    }

    Ok(SwitchboardRuntimeConfig {
        queue_pubkey,
        max_age_slots,
        feed_map,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Sidecar JSON (mirrors adrena-data/cron/process/processSwitchboardInstructions.ts)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SidecarAccountMeta {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

#[derive(Debug, Deserialize)]
struct SidecarInstruction {
    program_id: String,
    accounts: Vec<SidecarAccountMeta>,
    data: String, // base64
}

#[derive(Debug, Deserialize)]
struct SidecarOutput {
    ed25519_ix: SidecarInstruction,
    quote_store_ix: SidecarInstruction,
    quote_account: String, // base58 Pubkey
    quote_slot: u64,
    // Expected to be 2 (ed25519_ix position after the two compute-budget ixs).
    // We assert this at parse time rather than silently ignoring it.
    #[serde(default, rename = "instruction_idx")]
    pub instruction_idx: Option<u32>,
}

fn deserialize_instruction(raw: SidecarInstruction) -> anyhow::Result<Instruction> {
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
        .collect::<anyhow::Result<Vec<_>>>()?;

    let data = BASE64
        .decode(&raw.data)
        .context("decode base64 instruction data")?;

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Cycle builder
// ─────────────────────────────────────────────────────────────────────────────

pub fn make_switchboard_cycle(db_pool: DbPool, config: SwitchboardRuntimeConfig) -> CycleFn {
    Box::new(move || {
        let db_pool = db_pool.clone();
        let config = config.clone();
        Box::pin(async move {
            let batch = match db::get_latest_oracle_batch_by_provider(&db_pool, SWITCHBOARD_PROVIDER)
                .await
                .context("switchboard: read latest oracle batch")?
            {
                Some(b) => b,
                None => return Ok(None),
            };

            let metadata = batch.metadata.ok_or_else(|| {
                anyhow!(
                    "switchboard oracle_batch {} has no metadata (sidecar output missing)",
                    batch.oracle_batch_id
                )
            })?;

            let sidecar: SidecarOutput = serde_json::from_value(metadata).with_context(|| {
                format!(
                    "deserialize switchboard sidecar metadata for batch {}",
                    batch.oracle_batch_id
                )
            })?;

            // Assert instruction_idx matches our hardcoded tx layout (ed25519 at position 2,
            // after compute-budget price + limit). If the sidecar ever changes this, we must
            // update the tx assembly in handlers/update_pool_aum.rs.
            match sidecar.instruction_idx {
                Some(2) => {}
                Some(idx) => {
                    anyhow::bail!(
                        "switchboard sidecar instruction_idx={} but expected 2 — \
                         tx layout assumption violated, update handlers/update_pool_aum.rs",
                        idx
                    );
                }
                None => {
                    // Not fatal today (field is optional for back-compat), but if it ever
                    // disappears from the producer side we lose the only cross-boundary
                    // check on ed25519 placement. Surface loudly so future layout drift
                    // doesn't hide behind silent acceptance.
                    log::warn!(
                        "switchboard sidecar for batch {} omits instruction_idx — cannot \
                         validate ed25519 tx position. Check adrena-data sidecar writer; \
                         if layout changed, update handlers/update_pool_aum.rs.",
                        batch.oracle_batch_id,
                    );
                }
            }

            let ed25519_ix = deserialize_instruction(sidecar.ed25519_ix)
                .context("decode switchboard ed25519_ix")?;
            let quote_store_ix = deserialize_instruction(sidecar.quote_store_ix)
                .context("decode switchboard quote_store_ix")?;
            let quote_account = Pubkey::from_str(&sidecar.quote_account)
                .with_context(|| format!("invalid quote_account pubkey: {}", sidecar.quote_account))?;

            let update = SwitchboardOraclePricesUpdate {
                queue_pubkey: config.queue_pubkey,
                max_age_slots: config.max_age_slots,
                feed_map: config.feed_map.clone(),
                ed25519_ix,
                quote_store_ix,
                quote_account,
            };

            // Dedup by quote_slot + batch_id so a repeated DB read doesn't
            // re-emit an already-processed quote. oracle_batch_id changes on
            // each insert so it alone is enough, but we include quote_slot
            // for log readability.
            let dedup_key = format!("{}:{}", batch.oracle_batch_id, sidecar.quote_slot);

            Ok(Some(PollerPayload {
                dedup_key,
                update: ProviderUpdate::Switchboard(update),
            }))
        })
    })
}
