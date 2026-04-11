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
    anyhow::{anyhow, Context},
    base64::{engine::general_purpose::STANDARD as BASE64, Engine},
    deadpool_postgres::Pool as DbPool,
    serde::Deserialize,
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    std::{fs, str::FromStr},
};

const SWITCHBOARD_PROVIDER: &str = "switchboard";
const SWITCHBOARD_MIN_FEED_ID: u8 = 142;
const SWITCHBOARD_MAX_FEED_ID: u8 = 255;

// ─────────────────────────────────────────────────────────────────────────────
// Local feed-map JSON (mirrors adrena-data's switchboard_feed_map.json)
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
}

pub fn load_switchboard_feed_map(
    feed_map_path: &str,
    queue_pubkey: Pubkey,
    max_age_slots: u64,
) -> anyhow::Result<SwitchboardRuntimeConfig> {
    let raw = fs::read_to_string(feed_map_path)
        .with_context(|| format!("read switchboard feed map at `{feed_map_path}`"))?;

    // Support both the bare array form (legacy) and the wrapped form with
    // `_schema_version` + `switchboard_feed_map`.
    let entries: Vec<SwitchboardFeedMapRaw> = match serde_json::from_str::<SwitchboardFeedMapFile>(&raw) {
        Ok(parsed) => parsed.switchboard_feed_map,
        Err(_) => serde_json::from_str(&raw)
            .with_context(|| format!("parse switchboard feed map at `{feed_map_path}`"))?,
    };

    if entries.is_empty() {
        return Err(anyhow!("switchboard feed map at `{feed_map_path}` is empty"));
    }

    let mut feed_map = Vec::with_capacity(entries.len());
    for entry in entries {
        if !(SWITCHBOARD_MIN_FEED_ID..=SWITCHBOARD_MAX_FEED_ID).contains(&entry.adrena_feed_id) {
            return Err(anyhow!(
                "switchboard adrena_feed_id {} outside range {SWITCHBOARD_MIN_FEED_ID}..={SWITCHBOARD_MAX_FEED_ID}",
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
    // Ignored — we always send instruction_idx=2 in the on-chain tx (matches
    // the adrena-data sidecar default and the position of ed25519_ix after
    // the two compute-budget ixs). Kept for forward-compat if ops ever needs
    // to override the tx layout.
    #[serde(default, rename = "instruction_idx")]
    #[allow(dead_code)]
    _instruction_idx: Option<u32>,
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
