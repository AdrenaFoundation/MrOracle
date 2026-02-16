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
        collections::{HashMap, HashSet},
        fs,
        sync::Arc,
        time::Duration,
    },
    switchboard_on_demand::client::{
        crossbar::CrossbarClient,
        surge::{ConnectionState, FeedSubscription, Surge, SurgeEvent, SurgeUpdate},
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
    pub api_key: String,
    pub crossbar_url: String,
    pub gateway_url: Option<String>,
    pub network: String,
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub cycle_timeout: Duration,
    pub feed_bindings: Vec<SwitchboardFeedBinding>,
}

#[derive(Debug, Deserialize)]
struct SwitchboardFeedBindingRaw {
    adrena_feed_id: u8,
    switchboard_feed_hash: String,
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

#[derive(Debug, Clone)]
struct PendingQuoteUpdate {
    update: SurgeUpdate,
    covered_hashes: HashSet<String>,
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
    loop {
        if let Err(err) = run_switchboard_session(&program, &config, &update_sender).await {
            log::error!("Switchboard session failed: {:?}", err);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn run_switchboard_session(
    program: &Program<Arc<Keypair>>,
    config: &SwitchboardRuntimeConfig,
    update_sender: &mpsc::Sender<ProviderUpdate>,
) -> Result<(), anyhow::Error> {
    let gateway_url = resolve_gateway_url(config).await?;

    let surge = Surge::init(config.api_key.clone(), gateway_url.clone(), false);
    let mut event_rx = surge.subscribe_events();

    surge
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect surge websocket: {e}"))?;
    wait_for_connected_state(&surge).await?;

    let subscriptions: Vec<FeedSubscription> = config
        .feed_bindings
        .iter()
        .map(|binding| FeedSubscription::FeedHash {
            feed_hash: binding.switchboard_feed_hash_hex.clone(),
        })
        .collect();

    surge
        .subscribe(subscriptions)
        .await
        .map_err(|e| anyhow::anyhow!("failed to subscribe to switchboard feeds: {e}"))?;

    log::info!(
        "Switchboard session ready: gateway={}, tracked_feeds={}, cycle={}ms, max_age_slots={}",
        gateway_url,
        config.feed_bindings.len(),
        config.cycle_timeout.as_millis(),
        config.max_age_slots
    );

    let required_hashes: HashSet<String> = config
        .feed_bindings
        .iter()
        .map(|entry| entry.switchboard_feed_hash_hex.clone())
        .collect();

    let mut pending_updates: HashMap<Pubkey, PendingQuoteUpdate> = HashMap::new();
    let mut cycle_interval = tokio::time::interval(config.cycle_timeout);

    loop {
        tokio::select! {
            _ = cycle_interval.tick() => {
                if !pending_updates.is_empty() {
                    let covered = get_covered_hashes(&pending_updates);
                    if covered.len() < required_hashes.len() {
                        log::warn!(
                            "Switchboard cycle incomplete (covered {} / {} feeds), dropping partial quote set",
                            covered.len(),
                            required_hashes.len(),
                        );
                        pending_updates.clear();
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(SurgeEvent::Connected) => {
                        log::info!("Switchboard websocket connected");
                    }
                    Ok(SurgeEvent::Subscribed(_)) => {
                        log::info!("Switchboard subscriptions acknowledged");
                    }
                    Ok(SurgeEvent::UnsignedUpdate(_)) => {}
                    Ok(SurgeEvent::Update(update)) => {
                        let covered_hashes = extract_covered_hashes(&update, &required_hashes);
                        if covered_hashes.is_empty() {
                            continue;
                        }

                        let quote_account = derive_quote_account(program.payer(), config.queue_pubkey, &update)?;
                        pending_updates.insert(
                            quote_account,
                            PendingQuoteUpdate {
                                update,
                                covered_hashes,
                            },
                        );

                        let covered = get_covered_hashes(&pending_updates);
                        if covered.len() == required_hashes.len() {
                            let mut ordered: Vec<(String, SurgeUpdate)> = pending_updates
                                .iter()
                                .map(|(quote_account, pending)| {
                                    (quote_account.to_string(), pending.update.clone())
                                })
                                .collect();
                            ordered.sort_by(|left, right| left.0.cmp(&right.0));

                            let updates: Vec<SurgeUpdate> =
                                ordered.into_iter().map(|(_, update)| update).collect();
                            let switchboard_feed_map = config
                                .feed_bindings
                                .iter()
                                .map(SwitchboardFeedBinding::to_feed_map_entry)
                                .collect();

                            let payload = SwitchboardOraclePricesUpdate {
                                queue_pubkey: config.queue_pubkey,
                                max_age_slots: config.max_age_slots,
                                feed_map: switchboard_feed_map,
                                updates,
                            };
                            update_sender
                                .send(ProviderUpdate::Switchboard(payload))
                                .await
                                .map_err(|_| anyhow::anyhow!("switchboard provider update channel closed"))?;

                            pending_updates.clear();
                        }
                    }
                    Ok(SurgeEvent::Error(err)) => {
                        return Err(anyhow::anyhow!("switchboard websocket error: {}", err));
                    }
                    Ok(SurgeEvent::Disconnected) => {
                        return Err(anyhow::anyhow!("switchboard websocket disconnected"));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        log::warn!(
                            "Switchboard event stream lagged; skipped {} event(s)",
                            skipped
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("switchboard event stream closed"));
                    }
                }
            }
        }
    }
}

fn extract_covered_hashes(
    update: &SurgeUpdate,
    required_hashes: &HashSet<String>,
) -> HashSet<String> {
    update
        .get_signed_feeds()
        .into_iter()
        .map(|hash| normalize_feed_hash(&hash))
        .filter(|hash| required_hashes.contains(hash))
        .collect()
}

fn get_covered_hashes(pending_updates: &HashMap<Pubkey, PendingQuoteUpdate>) -> HashSet<String> {
    pending_updates
        .values()
        .flat_map(|pending| pending.covered_hashes.iter().cloned())
        .collect()
}

fn derive_quote_account(
    payer: Pubkey,
    queue_pubkey: Pubkey,
    update: &SurgeUpdate,
) -> Result<Pubkey, anyhow::Error> {
    let generated_ixs = update
        .to_quote_ix(queue_pubkey, payer, 0)
        .map_err(|e| anyhow::anyhow!("failed to derive quote account from surge update: {e}"))?;

    let quote_program_ix = generated_ixs
        .get(1)
        .context("missing quote-program instruction while deriving quote account")?;

    quote_program_ix
        .accounts
        .get(1)
        .map(|account| account.pubkey)
        .context("missing quote account meta while deriving quote account")
}

async fn resolve_gateway_url(config: &SwitchboardRuntimeConfig) -> Result<String, anyhow::Error> {
    if let Some(gateway_url) = config.gateway_url.clone() {
        return Ok(gateway_url);
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

    gateways.first().cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "no switchboard gateways returned for network {} from {}",
            config.network,
            config.crossbar_url
        )
    })
}

async fn wait_for_connected_state(surge: &Surge) -> Result<(), anyhow::Error> {
    for _ in 0..20 {
        if surge.get_state().await == ConnectionState::Connected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(anyhow::anyhow!(
        "switchboard websocket did not reach connected state"
    ))
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
