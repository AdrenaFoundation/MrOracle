use {
    crate::{
        adrena_ix::SwitchboardFeedMapEntry,
        provider_updates::{ProviderUpdate, SwitchboardOraclePricesUpdate},
    },
    anchor_client::Program,
    anyhow::Context,
    serde::Deserialize,
    solana_sdk::{pubkey::Pubkey, signature::Keypair},
    std::{
        collections::HashSet,
        fs,
        str::FromStr,
        sync::Arc,
        time::Duration,
    },
    switchboard_on_demand::client::{
        crossbar::CrossbarClient,
        gateway::Gateway,
        pull_feed::{FetchUpdateManyParams, PullFeed, SbContext},
    },
    tokio::sync::mpsc,
};

const SWITCHBOARD_MIN_FEED_ID: u8 = 142;
const SWITCHBOARD_MAX_FEED_ID: u8 = 255;
const ADRENA_ORACLE_DISCRIMINATOR_LEN: usize = 8;
const ADRENA_ORACLE_HEADER_LEN: usize = 16;
const ADRENA_ORACLE_PRICE_SLOT_LEN: usize = 64;
const ADRENA_ORACLE_PRICE_FEED_ID_OFFSET: usize = 28;
const ADRENA_ORACLE_PRICE_NAME_LEN_OFFSET: usize = 63;

#[derive(Debug, Clone)]
pub struct SwitchboardFeedBinding {
    pub adrena_feed_id: u8,
    pub switchboard_feed_hash_hex: String,
    pub switchboard_feed_hash: [u8; 32],
    pub pull_feed_pubkey: Pubkey,
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
    pub crossbar_url: String,
    pub gateway_url: Option<String>,
    pub network: String,
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub poll_interval: Duration,
    pub feed_bindings: Vec<SwitchboardFeedBinding>,
}

#[derive(Debug, Deserialize)]
struct SwitchboardFeedBindingRaw {
    adrena_feed_id: u8,
    switchboard_feed_hash: String,
    pull_feed_pubkey: String,
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
            format!("failed to parse switchboard feed map json from `{switchboard_feed_map_path}`")
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
    let mut seen_pubkeys = HashSet::new();
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

        let pull_feed_pubkey = Pubkey::from_str(entry.pull_feed_pubkey.trim()).with_context(|| {
            format!(
                "invalid pull_feed_pubkey `{}` for adrena_feed_id {}",
                entry.pull_feed_pubkey, entry.adrena_feed_id
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

        if !seen_pubkeys.insert(pull_feed_pubkey) {
            return Err(anyhow::anyhow!(
                "duplicate pull_feed_pubkey `{}` in mapping",
                pull_feed_pubkey
            ));
        }

        bindings.push(SwitchboardFeedBinding {
            adrena_feed_id: entry.adrena_feed_id,
            switchboard_feed_hash_hex: normalized_hash,
            switchboard_feed_hash,
            pull_feed_pubkey,
        });
    }

    bindings.sort_by_key(|entry| entry.adrena_feed_id);
    Ok(bindings)
}

pub async fn validate_switchboard_feed_bindings_against_oracle(
    program: &Program<Arc<Keypair>>,
    feed_bindings: &[SwitchboardFeedBinding],
) -> Result<Vec<u8>, anyhow::Error> {
    let required_feed_ids = fetch_required_switchboard_feed_ids(program).await?;
    if required_feed_ids.is_empty() {
        return Err(anyhow::anyhow!(
            "on-chain oracle has no registered switchboard feed_ids ({}..={})",
            SWITCHBOARD_MIN_FEED_ID,
            SWITCHBOARD_MAX_FEED_ID
        ));
    }

    let required: HashSet<u8> = required_feed_ids.iter().copied().collect();
    let configured: HashSet<u8> = feed_bindings
        .iter()
        .map(|entry| entry.adrena_feed_id)
        .collect();

    let mut missing_feed_ids: Vec<u8> = required.difference(&configured).copied().collect();
    missing_feed_ids.sort_unstable();

    let mut extra_feed_ids: Vec<u8> = configured.difference(&required).copied().collect();
    extra_feed_ids.sort_unstable();

    if !missing_feed_ids.is_empty() || !extra_feed_ids.is_empty() {
        return Err(anyhow::anyhow!(
            "switchboard feed map mismatch against on-chain oracle feed_ids; missing={:?}, extra={:?}",
            missing_feed_ids,
            extra_feed_ids
        ));
    }

    log::info!(
        "Switchboard feed-map preflight passed ({} feed_ids): {:?}",
        required_feed_ids.len(),
        required_feed_ids
    );

    Ok(required_feed_ids)
}

pub async fn run_switchboard_keeper(
    program: Program<Arc<Keypair>>,
    config: SwitchboardRuntimeConfig,
    update_sender: mpsc::Sender<ProviderUpdate>,
) -> Result<(), anyhow::Error> {
    let sb_context = SbContext::new();
    let rpc_client = program.rpc();

    let gateway = resolve_gateway(&config).await?;
    let crossbar = CrossbarClient::new(&config.crossbar_url, false);

    let feed_pubkeys: Vec<Pubkey> = config
        .feed_bindings
        .iter()
        .map(|b| b.pull_feed_pubkey)
        .collect();

    let payer_pubkey = program.payer();

    log::info!(
        "Switchboard OnDemand keeper started: feeds={}, poll={}ms, max_age_slots={}",
        feed_pubkeys.len(),
        config.poll_interval.as_millis(),
        config.max_age_slots
    );

    loop {
        match poll_switchboard_once(
            &sb_context,
            &rpc_client,
            &config,
            &gateway,
            &crossbar,
            &feed_pubkeys,
            payer_pubkey,
        )
        .await
        {
            Ok(payload) => {
                update_sender
                    .send(ProviderUpdate::Switchboard(payload))
                    .await
                    .map_err(|_| anyhow::anyhow!("switchboard provider update channel closed"))?;
            }
            Err(err) => {
                log::error!("Switchboard OnDemand poll failed: {:?}", err);
            }
        }

        tokio::time::sleep(config.poll_interval).await;
    }
}

async fn poll_switchboard_once(
    context: &Arc<SbContext>,
    rpc_client: &solana_client::nonblocking::rpc_client::RpcClient,
    config: &SwitchboardRuntimeConfig,
    gateway: &Gateway,
    crossbar: &CrossbarClient,
    feed_pubkeys: &[Pubkey],
    payer: Pubkey,
) -> Result<SwitchboardOraclePricesUpdate, anyhow::Error> {
    let (ixs, _luts) = PullFeed::fetch_update_consensus_ix(
        context.clone(),
        rpc_client,
        FetchUpdateManyParams {
            feeds: feed_pubkeys.to_vec(),
            payer,
            gateway: gateway.clone(),
            crossbar: Some(crossbar.clone()),
            num_signatures: None,
            debug: None,
        },
    )
    .await
    .context("PullFeed::fetch_update_consensus_ix failed")?;

    if ixs.len() != 2 {
        return Err(anyhow::anyhow!(
            "expected 2 instructions from fetch_update_consensus_ix (secp256k1 + submit), got {}",
            ixs.len()
        ));
    }

    let switchboard_feed_map: Vec<SwitchboardFeedMapEntry> = config
        .feed_bindings
        .iter()
        .map(SwitchboardFeedBinding::to_feed_map_entry)
        .collect();

    let pull_feed_pubkeys: Vec<Pubkey> = config
        .feed_bindings
        .iter()
        .map(|b| b.pull_feed_pubkey)
        .collect();

    Ok(SwitchboardOraclePricesUpdate {
        queue_pubkey: config.queue_pubkey,
        max_age_slots: config.max_age_slots,
        feed_map: switchboard_feed_map,
        secp_ix: ixs[0].clone(),
        submit_ix: ixs[1].clone(),
        pull_feed_pubkeys,
    })
}

async fn resolve_gateway(config: &SwitchboardRuntimeConfig) -> Result<Gateway, anyhow::Error> {
    if let Some(gateway_url) = config.gateway_url.clone() {
        return Ok(Gateway::new(gateway_url));
    }

    let crossbar = CrossbarClient::new(&config.crossbar_url, false);
    let gateways = crossbar
        .fetch_gateways(&config.network)
        .await
        .with_context(|| {
            format!(
                "failed to fetch switchboard gateways from crossbar {} for network {}",
                config.crossbar_url, config.network
            )
        })?;

    let gateway_url = gateways.first().cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "no switchboard gateways returned for network {} from {}",
            config.network,
            config.crossbar_url
        )
    })?;

    Ok(Gateway::new(gateway_url))
}

fn normalize_feed_hash(hash: &str) -> String {
    hash.trim().trim_start_matches("0x").to_ascii_lowercase()
}

async fn fetch_required_switchboard_feed_ids(
    program: &Program<Arc<Keypair>>,
) -> Result<Vec<u8>, anyhow::Error> {
    let oracle_pda = adrena_abi::pda::get_oracle_pda().0;
    let account_data = program
        .rpc()
        .get_account_data(&oracle_pda)
        .await
        .with_context(|| format!("failed to fetch oracle account data at {oracle_pda}"))?;

    Ok(parse_registered_switchboard_feed_ids(&account_data))
}

fn parse_registered_switchboard_feed_ids(account_data: &[u8]) -> Vec<u8> {
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
            && (SWITCHBOARD_MIN_FEED_ID..=SWITCHBOARD_MAX_FEED_ID).contains(&feed_id)
            && seen.insert(feed_id)
        {
            out.push(feed_id);
        }

        offset += ADRENA_ORACLE_PRICE_SLOT_LEN;
    }

    out.sort_unstable();
    out
}
