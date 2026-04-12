//! MrOracle release/39 — multi-provider + multi-pool dispatcher.
//!
//! High-level flow:
//!
//!   ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
//!   │ chaoslabs    │   │ autonom      │   │ switchboard  │
//!   │ poller       │   │ poller       │   │ poller       │
//!   │ (DB read)    │   │ (DB read)    │   │ (DB read)    │
//!   └──────┬───────┘   └──────┬───────┘   └──────┬───────┘
//!          │                  │                  │
//!          └──────────────────┼──────────────────┘
//!                             │  ProviderUpdate (mpsc)
//!                             ▼
//!               ┌─────────────────────────────┐
//!               │ main dispatch loop          │
//!               │  - recv(ProviderUpdate)     │
//!               │  - fan out to each pool     │
//!               │  - start 5s window on first │
//!               │    arrival per pool         │
//!               │  - fire tx when either      │
//!               │    (a) all required         │
//!               │        providers present OR │
//!               │    (b) window expired       │
//!               └──────────┬──────────────────┘
//!                          │
//!                          ▼
//!                  RpcFallback (from main)
//!                          │
//!                          ▼
//!                       Solana
//!
//! Priority fee refresher runs as a separate task, unchanged from main.

use {
    crate::{
        oracle_poller::run_provider_poller,
        pool_config::{aggregate_provider_requirements, load_pools_config, PoolRuntime, COLLECTION_WINDOW},
        priority_fees::fetch_mean_priority_fee,
        provider_updates::{ProviderKind, ProviderUpdate},
        providers::{make_autonom_cycle, make_chaoslabs_cycle, make_switchboard_cycle},
        rpc_fallback::RpcFallback,
    },
    clap::Parser,
    openssl::ssl::{SslConnector, SslMethod},
    postgres_openssl::MakeTlsConnector,
    solana_sdk::{pubkey::Pubkey, signer::keypair::read_keypair_file},
    std::{env, str::FromStr, sync::Arc, time::Duration},
    tokio::{
        sync::{mpsc, Mutex},
        time::{interval, sleep},
    },
};

pub mod adrena_ix;
pub mod db;
pub mod handlers;
pub mod oracle_poller;
pub mod pool_config;
pub mod priority_fees;
pub mod provider_updates;
pub mod providers;
pub mod rpc_fallback;
pub mod utils;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:10000";
const MEAN_PRIORITY_FEE_PERCENTILE: u64 = 2500; // 25th
const PRIORITY_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// How often the window-expiry check wakes up. 500ms is fine-grained enough
/// to keep our effective window within (COLLECTION_WINDOW + 500ms) in the
/// worst case.
const WINDOW_POLL_TICK: Duration = Duration::from_millis(500);

/// Per-provider poll interval. Default is a balance between DB load and
/// freshness — matches what MrOracle switchboard branch used.
const DEFAULT_CHAOSLABS_POLL_MS: u64 = 400;
const DEFAULT_AUTONOM_POLL_MS: u64 = 2_000;
const DEFAULT_SWITCHBOARD_POLL_MS: u64 = 2_000;

/// Default Switchboard MAINNET queue. Matches Queue.DEFAULT_MAINNET_KEY in
/// the on-demand SDK. Overridable via --switchboard-queue-pubkey.
const DEFAULT_SWITCHBOARD_MAINNET_QUEUE: &str = "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";
const DEFAULT_SWITCHBOARD_MAX_AGE_SLOTS: u64 = 150;

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum ArgsCommitment {
    #[default]
    Processed,
    Confirmed,
    Finalized,
}

impl From<ArgsCommitment> for solana_sdk::commitment_config::CommitmentConfig {
    fn from(c: ArgsCommitment) -> Self {
        use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
        CommitmentConfig {
            commitment: match c {
                ArgsCommitment::Processed => CommitmentLevel::Processed,
                ArgsCommitment::Confirmed => CommitmentLevel::Confirmed,
                ArgsCommitment::Finalized => CommitmentLevel::Finalized,
            },
        }
    }
}

#[derive(Debug, Clone, Parser)]
#[clap(author, version, about)]
struct Args {
    /// Primary Solana JSON-RPC endpoint
    #[clap(long, default_value_t = String::from(DEFAULT_ENDPOINT))]
    rpc: String,

    /// Backup RPC endpoint URL (optional, used as fallback)
    #[clap(long)]
    rpc_backup: Option<String>,

    /// Public RPC endpoint URL (last-resort fallback)
    #[clap(long, default_value_t = String::from(rpc_fallback::DEFAULT_PUBLIC_RPC))]
    rpc_public: String,

    /// Commitment level: processed, confirmed or finalized
    #[clap(long)]
    commitment: Option<ArgsCommitment>,

    /// Path to the payer keypair
    #[clap(long)]
    payer_keypair: String,

    /// Postgres connection string
    #[clap(long)]
    db_string: String,

    /// Combined TLS certificate path
    #[clap(long)]
    combined_cert: String,

    /// Pools config JSON — lists each pool + its providers + custodies.
    /// See pools_config.example.json at repo root.
    #[clap(long)]
    pools_config: String,

    /// ChaosLabs polling interval in milliseconds
    #[clap(long, default_value_t = DEFAULT_CHAOSLABS_POLL_MS)]
    chaoslabs_poll_ms: u64,

    /// Autonom polling interval in milliseconds
    #[clap(long, default_value_t = DEFAULT_AUTONOM_POLL_MS)]
    autonom_poll_ms: u64,

    /// Switchboard polling interval in milliseconds
    #[clap(long, default_value_t = DEFAULT_SWITCHBOARD_POLL_MS)]
    switchboard_poll_ms: u64,

    /// Switchboard feed map JSON path (required if any pool uses switchboard)
    #[clap(long)]
    switchboard_feed_map_path: Option<String>,

    /// Switchboard queue pubkey (mainnet default if omitted)
    #[clap(long, default_value_t = String::from(DEFAULT_SWITCHBOARD_MAINNET_QUEUE))]
    switchboard_queue_pubkey: String,

    /// Switchboard max age in slots for the on-chain staleness gate
    #[clap(long, default_value_t = DEFAULT_SWITCHBOARD_MAX_AGE_SLOTS)]
    switchboard_max_age_slots: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();

    log::info!("MrOracle release/39 — multi-provider dispatcher starting");

    let commitment: solana_sdk::commitment_config::CommitmentConfig =
        args.commitment.unwrap_or_default().into();

    let payer = read_keypair_file(args.payer_keypair.clone())
        .map_err(|e| anyhow::anyhow!("read payer keypair: {e:?}"))?;
    let payer = Arc::new(payer);

    let rpc_fallback = Arc::new(RpcFallback::new(
        &args.rpc,
        args.rpc_backup.as_deref(),
        &args.rpc_public,
        commitment,
        Arc::clone(&payer),
    ));

    log::info!("payer={}", payer.pubkey_string());

    // ────────────────────────────────────────────────────────────────
    // Load pools config
    // ────────────────────────────────────────────────────────────────
    let mut pools: Vec<Arc<PoolRuntime>> = load_pools_config(&args.pools_config)?
        .into_iter()
        .map(Arc::new)
        .collect();

    for pool in &pools {
        log::info!(
            "pool `{}` address={} providers={:?} custodies={} cu_limit={}",
            pool.name,
            pool.address,
            pool.required_providers,
            pool.all_custody_accounts.len(),
            pool.cu_limit,
        );
    }

    let required_providers = aggregate_provider_requirements(&pools);
    log::info!("required providers across all pools: {:?}", required_providers);

    // Validate custody order against on-chain state — quarantine invalid pools
    let quarantined = pool_config::validate_custodies_against_chain(
        &pools.iter().map(|p| p.as_ref()).collect::<Vec<_>>(),
        rpc_fallback.primary_client(),
    ).await;
    if !quarantined.is_empty() {
        log::warn!("⚠ {} pool(s) quarantined due to validation failure: {:?}", quarantined.len(), quarantined);
        // Remove quarantined pools from the active set
        pools.retain(|p| !quarantined.contains(&p.name));
        if pools.is_empty() {
            return Err(anyhow::anyhow!("All pools failed validation — cannot start"));
        }
    }

    // ────────────────────────────────────────────────────────────────
    // DB pool
    // ────────────────────────────────────────────────────────────────
    let mut ssl_builder = SslConnector::builder(SslMethod::tls()).unwrap();
    ssl_builder
        .set_ca_file(&args.combined_cert)
        .map_err(|e| anyhow::anyhow!("set_ca_file: {e}"))?;
    let tls_connector = MakeTlsConnector::new(ssl_builder.build());

    let mut db_config = args.db_string.parse::<tokio_postgres::Config>()?;
    db_config.connect_timeout(Duration::from_secs(2));
    db_config.keepalives(true);
    db_config.keepalives_idle(Duration::from_secs(6));

    let mgr_config = deadpool_postgres::ManagerConfig {
        recycling_method: deadpool_postgres::RecyclingMethod::Fast,
    };
    let mgr = deadpool_postgres::Manager::from_config(db_config, tls_connector, mgr_config);
    let db_pool = deadpool_postgres::Pool::builder(mgr)
        .max_size(4) // 3 pollers + 1 misc
        .build()
        .map_err(|e| anyhow::anyhow!("build deadpool: {e:?}"))?;

    // ────────────────────────────────────────────────────────────────
    // Priority fee refresher (unchanged from main)
    // ────────────────────────────────────────────────────────────────
    let median_priority_fee = Arc::new(Mutex::new(0u64));
    {
        let median_priority_fee = Arc::clone(&median_priority_fee);
        let rpc_fallback_clone = Arc::clone(&rpc_fallback);
        tokio::spawn(async move {
            let mut fee_refresh_interval = interval(PRIORITY_FEE_REFRESH_INTERVAL);
            loop {
                fee_refresh_interval.tick().await;
                if let Ok(fee) =
                    fetch_mean_priority_fee(&rpc_fallback_clone, MEAN_PRIORITY_FEE_PERCENTILE).await
                {
                    let mut fee_lock = median_priority_fee.lock().await;
                    *fee_lock = fee;
                    log::debug!("  <> Updated mean priority fee to: {fee} µLamports / cu");
                }
            }
        });
    }

    // ────────────────────────────────────────────────────────────────
    // Spawn provider pollers
    // ────────────────────────────────────────────────────────────────
    let (update_tx, mut update_rx) = mpsc::channel::<ProviderUpdate>(64);

    if required_providers.contains(&ProviderKind::ChaosLabs) {
        let sender = update_tx.clone();
        let cycle = make_chaoslabs_cycle(db_pool.clone());
        let poll = Duration::from_millis(args.chaoslabs_poll_ms);
        tokio::spawn(async move {
            run_provider_poller("chaoslabs", poll, cycle, sender).await;
        });
    }

    if required_providers.contains(&ProviderKind::Autonom) {
        let sender = update_tx.clone();
        let cycle = make_autonom_cycle(db_pool.clone());
        let poll = Duration::from_millis(args.autonom_poll_ms);
        tokio::spawn(async move {
            run_provider_poller("autonom", poll, cycle, sender).await;
        });
    }

    if required_providers.contains(&ProviderKind::Switchboard) {
        let feed_map_path = args
            .switchboard_feed_map_path
            .as_deref()
            .ok_or_else(|| {
                anyhow::anyhow!("--switchboard-feed-map-path is required when any pool uses switchboard")
            })?;
        let queue_pubkey = Pubkey::from_str(&args.switchboard_queue_pubkey).map_err(|e| {
            anyhow::anyhow!("invalid --switchboard-queue-pubkey: {e}")
        })?;
        let sb_config = providers::switchboard::load_switchboard_feed_map(
            feed_map_path,
            queue_pubkey,
            args.switchboard_max_age_slots,
        )?;
        log::info!(
            "switchboard: queue={} feeds={} max_age_slots={}",
            sb_config.queue_pubkey,
            sb_config.feed_map.len(),
            sb_config.max_age_slots,
        );
        let sender = update_tx.clone();
        let cycle = make_switchboard_cycle(db_pool.clone(), sb_config);
        let poll = Duration::from_millis(args.switchboard_poll_ms);
        tokio::spawn(async move {
            run_provider_poller("switchboard", poll, cycle, sender).await;
        });
    }

    // Drop the original sender so the receiver sees EOF if every poller dies.
    drop(update_tx);

    // ────────────────────────────────────────────────────────────────
    // Main dispatch loop
    //
    // On each tick:
    //   1. Drain up to 64 queued ProviderUpdates (non-blocking). Distribute
    //      each to every pool whose required_providers contains its kind.
    //   2. Scan all pools for fire conditions (complete OR window expired).
    //   3. Fire fire_update_pool_aum for each pool that's ready.
    //   4. Sleep WINDOW_POLL_TICK (500ms) and loop.
    // ────────────────────────────────────────────────────────────────
    loop {
        // Drain the channel without blocking: if there's nothing right now,
        // recv() returns None via try_recv so we move on to the window check.
        loop {
            match update_rx.try_recv() {
                Ok(update) => {
                    let kind = update.kind();
                    log::debug!("dispatch: received {:?} update", kind);
                    for pool in &pools {
                        if !pool.required_providers.contains(&kind) {
                            continue;
                        }
                        let mut pending = pool.pending.lock().await;
                        pending.ingest(update.clone());
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    log::error!("all provider pollers stopped, exiting main loop");
                    return Err(anyhow::anyhow!("provider channel disconnected"));
                }
            }
        }

        // Fire any ready pools.
        for pool in &pools {
            let should_fire;
            let snapshot_opt;
            {
                let mut pending = pool.pending.lock().await;
                let complete = pending.is_complete(&pool.required_providers);
                let expired = pending.is_window_expired(COLLECTION_WINDOW);
                if complete || (expired && pending.has_anything()) {
                    snapshot_opt = Some(pending.take_snapshot());
                    should_fire = true;
                } else {
                    snapshot_opt = None;
                    should_fire = false;
                }
            }

            if should_fire {
                let snapshot = snapshot_opt.unwrap();
                let fee = *median_priority_fee.lock().await;
                let rpc_fallback = Arc::clone(&rpc_fallback);
                let pool = Arc::clone(pool);
                // Fire-and-forget: run the send on a fresh task so a slow
                // send on one pool doesn't hold up the dispatch loop for
                // other pools.
                tokio::spawn(async move {
                    if let Err(err) =
                        handlers::fire_update_pool_aum(&rpc_fallback, &pool, snapshot, fee).await
                    {
                        log::error!("[{}] fire_update_pool_aum failed: {err:#}", pool.name);
                    }
                });
            }
        }

        sleep(WINDOW_POLL_TICK).await;
    }
}

// ── Small helper: extend Keypair with pubkey_string for log formatting.
//    Avoids a chain of `.pubkey().to_string()` calls at the log sites.
trait KeypairPubkeyString {
    fn pubkey_string(&self) -> String;
}

impl KeypairPubkeyString for solana_sdk::signature::Keypair {
    fn pubkey_string(&self) -> String {
        use solana_sdk::signer::Signer;
        self.pubkey().to_string()
    }
}
