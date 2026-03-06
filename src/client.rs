use {
    anchor_client::{
        solana_sdk::{
            signature::Keypair,
            signer::keypair::read_keypair_file,
        },
        Client, Cluster, Program,
    },
    clap::Parser,
    openssl::ssl::{SslConnector, SslMethod, SslVerifyMode},
    postgres_openssl::MakeTlsConnector,
    priority_fees::fetch_mean_priority_fee,
    solana_sdk::{instruction::AccountMeta, pubkey::Pubkey},
    std::{env, future::Future, pin::Pin, str::FromStr, sync::Arc, time::Duration},
    tokio::{
        sync::{mpsc, Mutex},
        time::interval,
    },
};

pub mod adrena_ix;
pub mod autonom;
pub mod db;
pub mod handlers;
pub mod oracle_poller;
pub mod priority_fees;
pub mod provider_updates;
pub mod switchboard;
pub mod utils;

use {
    adrena_abi::oracle::BatchPrices,
    adrena_ix::{
        BatchPricesWithProvider, MultiBatchPrices, ORACLE_PROVIDER_AUTONOM,
        ORACLE_PROVIDER_CHAOS_LABS,
    },
    provider_updates::{ProviderUpdate, SwitchboardOraclePricesUpdate},
    utils::format_chaos_labs_oracle_entry_to_params::{
        format_chaos_labs_oracle_batch_to_params, format_chaos_labs_oracle_entry_to_params,
        load_chaos_labs_feed_map, ChaosLabsFeedBinding,
    },
};

type DbPool = deadpool_postgres::Pool;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:10000";
const MEAN_PRIORITY_FEE_PERCENTILE: u64 = 2500; // 25th
const PRIORITY_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(5); // seconds
const UPDATE_AUM_CU_LIMIT: u32 = 120_000;
const UPDATE_POOL_AUM_CU_LIMIT_WITH_SWITCHBOARD: u32 = 1_400_000;

const DEFAULT_SWITCHBOARD_MAINNET_QUEUE_PUBKEY: &str =
    "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";
const DEFAULT_SWITCHBOARD_POLL_MS: u64 = 2_000;
const DEFAULT_SWITCHBOARD_MAX_AGE_SLOTS: u64 = 32;
const DEFAULT_SWITCHBOARD_INSTRUCTION_IDX: u32 = 2; // Ed25519 ix at index 2 (after 2 compute budget ixs)
const DEFAULT_AUTONOM_POLL_MS: u64 = 2_000;
const CHAOSLABS_CYCLE: Duration = Duration::from_secs(2);

fn is_dry_run() -> bool {
    env::var("DRY_RUN")
        .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum ArgsCommitment {
    #[default]
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, Eq, PartialEq, Hash)]
enum ProviderArg {
    Chaoslabs,
    Autonom,
    Switchboard,
}

#[derive(Debug, Clone, Parser)]
#[clap(author, version, about)]
struct Args {
    #[clap(short, long, default_value_t = String::from(DEFAULT_ENDPOINT))]
    /// Service endpoint
    endpoint: String,

    #[clap(long)]
    x_token: Option<String>,

    /// Commitment level: processed, confirmed or finalized
    #[clap(long)]
    commitment: Option<ArgsCommitment>,

    /// Path to the payer keypair
    #[clap(long)]
    payer_keypair: String,

    /// DB Url
    #[clap(long)]
    db_string: Option<String>,

    /// Combined certificate
    #[clap(long)]
    combined_cert: Option<String>,

    /// Run only ChaosLabs path (disables Autonom + Switchboard)
    #[clap(
        long,
        default_value_t = false,
        conflicts_with_all = ["only_autonom", "only_switchboard"]
    )]
    only_chaoslabs: bool,

    /// Run only Autonom path (disables ChaosLabs + Switchboard)
    #[clap(
        long,
        default_value_t = false,
        conflicts_with_all = ["only_chaoslabs", "only_switchboard"]
    )]
    only_autonom: bool,

    /// Run only Switchboard path (disables ChaosLabs + Autonom)
    #[clap(
        long,
        default_value_t = false,
        conflicts_with_all = ["only_chaoslabs", "only_autonom"]
    )]
    only_switchboard: bool,

    /// Path to chaoslabs feed mapping json
    #[clap(long)]
    chaoslabs_feed_map_path: Option<String>,

    /// Switchboard queue pubkey
    #[clap(long, default_value_t = String::from(DEFAULT_SWITCHBOARD_MAINNET_QUEUE_PUBKEY))]
    switchboard_queue_pubkey: String,

    /// Path to switchboard feed mapping json
    #[clap(long)]
    switchboard_feed_map_path: Option<String>,

    /// Switchboard PullFeed max age in slots
    #[clap(long, default_value_t = DEFAULT_SWITCHBOARD_MAX_AGE_SLOTS)]
    switchboard_max_age_slots: u64,

    /// Switchboard OnDemand polling interval in milliseconds
    #[clap(long, default_value_t = DEFAULT_SWITCHBOARD_POLL_MS)]
    switchboard_poll_ms: u64,

    /// Index of the Ed25519 instruction in the final transaction (after compute budget ixs)
    #[clap(long, default_value_t = DEFAULT_SWITCHBOARD_INSTRUCTION_IDX)]
    switchboard_instruction_idx: u32,

    /// Compute unit limit for coordinated update_pool_aum tx
    #[clap(long, default_value_t = UPDATE_POOL_AUM_CU_LIMIT_WITH_SWITCHBOARD)]
    update_oracle_cu_limit: u32,

    /// Path to autonom feed mapping json
    #[clap(long)]
    autonom_feed_map_path: Option<String>,

    /// Autonom polling interval in milliseconds
    #[clap(long, default_value_t = DEFAULT_AUTONOM_POLL_MS)]
    autonom_poll_ms: u64,

    /// Explicit provider set, comma-separated (e.g. --providers chaoslabs,autonom)
    #[clap(
        long,
        value_delimiter = ',',
        num_args = 1..=3,
        conflicts_with_all = ["only_chaoslabs", "only_autonom", "only_switchboard"]
    )]
    providers: Option<Vec<ProviderArg>>,
}

#[derive(Default)]
struct PendingProviderUpdates {
    chaoslabs: Option<BatchPrices>,
    autonom: Option<BatchPrices>,
    switchboard: Option<SwitchboardOraclePricesUpdate>,
}

impl PendingProviderUpdates {
    fn ingest(&mut self, update: ProviderUpdate) {
        match update {
            ProviderUpdate::ChaosLabs(batch) => self.chaoslabs = Some(batch),
            ProviderUpdate::Autonom(batch) => self.autonom = Some(batch),
            ProviderUpdate::Switchboard(update) => self.switchboard = Some(update),
        }
    }

    fn is_ready(&self, run_chaoslabs: bool, run_autonom: bool, run_switchboard: bool) -> bool {
        (!run_chaoslabs || self.chaoslabs.is_some())
            && (!run_autonom || self.autonom.is_some())
            && (!run_switchboard || self.switchboard.is_some())
    }

    fn take_payload(
        &mut self,
        run_chaoslabs: bool,
        run_autonom: bool,
        run_switchboard: bool,
    ) -> Result<
        (
            Option<BatchPrices>,
            Option<MultiBatchPrices>,
            Option<SwitchboardOraclePricesUpdate>,
        ),
        anyhow::Error,
    > {
        let switchboard_oracle_prices = if run_switchboard {
            Some(self.switchboard.take().ok_or_else(|| {
                anyhow::anyhow!("missing switchboard payload for coordinated cycle")
            })?)
        } else {
            None
        };

        match (run_chaoslabs, run_autonom) {
            (true, false) => {
                let chaoslabs = self.chaoslabs.take().ok_or_else(|| {
                    anyhow::anyhow!("missing chaoslabs payload for coordinated cycle")
                })?;
                Ok((Some(chaoslabs), None, switchboard_oracle_prices))
            }
            (false, true) => {
                let autonom = self.autonom.take().ok_or_else(|| {
                    anyhow::anyhow!("missing autonom payload for coordinated cycle")
                })?;
                Ok((Some(autonom), None, switchboard_oracle_prices))
            }
            (true, true) => {
                let chaoslabs = self.chaoslabs.take().ok_or_else(|| {
                    anyhow::anyhow!("missing chaoslabs payload for coordinated cycle")
                })?;
                let autonom = self.autonom.take().ok_or_else(|| {
                    anyhow::anyhow!("missing autonom payload for coordinated cycle")
                })?;

                let multi_oracle_prices = MultiBatchPrices {
                    batches: vec![
                        BatchPricesWithProvider {
                            provider: ORACLE_PROVIDER_CHAOS_LABS,
                            batch: chaoslabs,
                        },
                        BatchPricesWithProvider {
                            provider: ORACLE_PROVIDER_AUTONOM,
                            batch: autonom,
                        },
                    ],
                };

                Ok((None, Some(multi_oracle_prices), switchboard_oracle_prices))
            }
            (false, false) => Ok((None, None, switchboard_oracle_prices)),
        }
    }
}

fn resolve_provider_mode(args: &Args) -> (bool, bool, bool) {
    if let Some(providers) = args.providers.as_ref() {
        let has_chaoslabs = providers.contains(&ProviderArg::Chaoslabs);
        let has_autonom = providers.contains(&ProviderArg::Autonom);
        let has_switchboard = providers.contains(&ProviderArg::Switchboard);
        return (has_chaoslabs, has_autonom, has_switchboard);
    }

    let selected = [
        args.only_chaoslabs,
        args.only_autonom,
        args.only_switchboard,
    ]
    .iter()
    .filter(|flag| **flag)
    .count();

    if selected == 0 {
        (true, true, true)
    } else {
        (
            args.only_chaoslabs,
            args.only_autonom,
            args.only_switchboard,
        )
    }
}

fn has_explicit_provider_selection(args: &Args) -> bool {
    args.providers.is_some() || args.only_chaoslabs || args.only_autonom || args.only_switchboard
}

fn resolve_target_pool_pubkey() -> Result<Pubkey, anyhow::Error> {
    match env::var("MAIN_POOL_ID") {
        Ok(pool_pubkey_raw) => Pubkey::from_str(pool_pubkey_raw.trim()).map_err(|e| {
            anyhow::anyhow!(
                "invalid MAIN_POOL_ID env var `{}`: {:?}",
                pool_pubkey_raw,
                e
            )
        }),
        Err(env::VarError::NotPresent) => Ok(adrena_abi::MAIN_POOL_ID),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow::anyhow!(
            "MAIN_POOL_ID env var contains non-utf8 data"
        )),
    }
}

async fn create_db_pool(args: &Args) -> Result<DbPool, anyhow::Error> {
    let combined_cert = args
        .combined_cert
        .clone()
        .ok_or_else(|| anyhow::anyhow!("DB-backed providers require --combined-cert"))?;
    let db_string = args
        .db_string
        .clone()
        .ok_or_else(|| anyhow::anyhow!("DB-backed providers require --db-string"))?;

    let mut ssl_builder = SslConnector::builder(SslMethod::tls()).unwrap();
    ssl_builder
        .set_ca_file(&combined_cert)
        .map_err(|e| anyhow::anyhow!("failed to set CA file: {e}"))?;
    // Allow hostname mismatch for local dev (snakeoil cert CN != "localhost")
    ssl_builder.set_verify(SslVerifyMode::NONE);
    let tls_connector = MakeTlsConnector::new(ssl_builder.build());

    let mut db_config = db_string.parse::<tokio_postgres::Config>()?;
    db_config.connect_timeout(Duration::from_secs(2));
    db_config.keepalives(true);
    db_config.keepalives_idle(Duration::from_secs(6));

    let mgr_config = deadpool_postgres::ManagerConfig {
        recycling_method: deadpool_postgres::RecyclingMethod::Fast,
    };
    let mgr = deadpool_postgres::Manager::from_config(db_config, tls_connector, mgr_config);

    deadpool_postgres::Pool::builder(mgr)
        .max_size(1)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to create DB pool: {e:?}"))
}

async fn load_custody_accounts(
    program: &Program<Arc<Keypair>>,
    pool_pubkey: Pubkey,
) -> Result<Vec<AccountMeta>, anyhow::Error> {
    let pool = program
        .account::<adrena_abi::Pool>(pool_pubkey)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load pool account {}: {e:?}", pool_pubkey))?;

    let mut out = Vec::new();
    for key in &pool.custodies {
        if key != &Pubkey::default() {
            out.push(AccountMeta {
                pubkey: *key,
                is_signer: false,
                is_writable: false,
            });
        }
    }

    Ok(out)
}

fn make_chaoslabs_cycle(
    db_pool: DbPool,
    feed_bindings: Vec<ChaosLabsFeedBinding>,
) -> impl Fn() -> Pin<Box<dyn Future<Output = Result<Option<oracle_poller::PollerPayload>, anyhow::Error>> + Send>>
       + Send
       + Sync {
    move || {
        let db_pool = db_pool.clone();
        let feed_bindings = feed_bindings.clone();
        Box::pin(async move {
            match db::get_latest_oracle_batch_by_provider(&db_pool, "chaoslabs").await {
                Ok(Some(batch)) => {
                    let id = batch.oracle_batch_id;
                    let result =
                        format_chaos_labs_oracle_batch_to_params(&batch, &feed_bindings)?;
                    Ok(Some(oracle_poller::PollerPayload {
                        dedup_key: id.to_string(),
                        update: ProviderUpdate::ChaosLabs(result),
                    }))
                }
                Ok(None) => {
                    log::warn!(
                        "No chaoslabs batch in oracle_batches, trying legacy assets_price"
                    );
                    match db::get_assets_prices::get_assets_prices(&db_pool).await {
                        Ok(Some(assets)) => {
                            let key = format!(
                                "legacy:{}:{}",
                                assets.signature,
                                assets.latest_timestamp.timestamp_millis()
                            );
                            let batch = format_chaos_labs_oracle_entry_to_params(
                                &assets,
                                &feed_bindings,
                            )?;
                            Ok(Some(oracle_poller::PollerPayload {
                                dedup_key: key,
                                update: ProviderUpdate::ChaosLabs(batch),
                            }))
                        }
                        Ok(None) => Ok(None),
                        Err(e) => Err(anyhow::anyhow!(
                            "legacy assets_price fallback failed: {e:?}"
                        )),
                    }
                }
                Err(e) => {
                    log::warn!(
                        "oracle_batches query failed ({e:?}); trying legacy fallback"
                    );
                    match db::get_assets_prices::get_assets_prices(&db_pool).await {
                        Ok(Some(assets)) => {
                            let key = format!(
                                "legacy:{}:{}",
                                assets.signature,
                                assets.latest_timestamp.timestamp_millis()
                            );
                            let batch = format_chaos_labs_oracle_entry_to_params(
                                &assets,
                                &feed_bindings,
                            )?;
                            Ok(Some(oracle_poller::PollerPayload {
                                dedup_key: key,
                                update: ProviderUpdate::ChaosLabs(batch),
                            }))
                        }
                        Ok(None) => Err(anyhow::anyhow!(
                            "No chaoslabs data in either source"
                        )),
                        Err(le) => Err(anyhow::anyhow!(
                            "Both sources failed: oracle_batches={e:?}, legacy={le:?}"
                        )),
                    }
                }
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenvy::dotenv().ok();

    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();
    let strict_provider_init = has_explicit_provider_selection(&args);
    let pool_pubkey = resolve_target_pool_pubkey()?;

    let (requested_chaoslabs, requested_autonom, requested_switchboard) =
        resolve_provider_mode(&args);
    let mut active_chaoslabs = requested_chaoslabs;
    let mut active_autonom = requested_autonom;
    let mut active_switchboard = requested_switchboard;

    log::info!("Target pool => {}", pool_pubkey);

    log::info!(
        "Requested provider mode => chaoslabs={}, autonom={}, switchboard={}",
        requested_chaoslabs,
        requested_autonom,
        requested_switchboard
    );

    let payer = read_keypair_file(args.payer_keypair.clone()).map_err(|e| {
        anyhow::anyhow!(
            "failed to read payer keypair `{}`: {}",
            args.payer_keypair,
            e
        )
    })?;
    let payer = Arc::new(payer);

    let client = Client::new(
        Cluster::Custom(args.endpoint.clone(), args.endpoint.clone()),
        Arc::clone(&payer),
    );

    let program = client
        .program(adrena_abi::ID)
        .map_err(|e| anyhow::anyhow!("failed to get program: {e:?}"))?;

    let mut db_pool: Option<DbPool> = None;
    if active_chaoslabs || active_autonom || active_switchboard {
        match create_db_pool(&args).await {
            Ok(pool) => {
                db_pool = Some(pool);
            }
            Err(err) => {
                if strict_provider_init {
                    return Err(anyhow::anyhow!(
                        "DB pool init failed for selected DB-backed providers (chaoslabs/autonom/switchboard): {:?}",
                        err
                    ));
                }
                log::error!(
                    "DB pool init failed; disabling DB-backed providers (chaoslabs/autonom/switchboard): {:?}",
                    err
                );
                active_chaoslabs = false;
                active_autonom = false;
                active_switchboard = false;
            }
        }
    }

    let mut chaoslabs_feed_bindings = vec![];
    if active_chaoslabs {
        let chaoslabs_feed_map_path = args
            .chaoslabs_feed_map_path
            .clone()
            .or_else(|| env::var("CHAOSLABS_FEED_MAP_PATH").ok());

        match chaoslabs_feed_map_path {
            Some(path) => match load_chaos_labs_feed_map(&path) {
                Ok(bindings) => {
                    chaoslabs_feed_bindings = bindings;
                }
                Err(err) => {
                    if strict_provider_init {
                        return Err(anyhow::anyhow!(
                            "Invalid chaoslabs feed map at `{}` for selected provider: {:?}",
                            path,
                            err
                        ));
                    }
                    log::error!(
                        "Invalid chaoslabs feed map at `{}`; disabling chaoslabs provider: {:?}",
                        path,
                        err
                    );
                    active_chaoslabs = false;
                }
            },
            None => {
                if strict_provider_init {
                    return Err(anyhow::anyhow!(
                        "chaoslabs selected via CLI but no feed map path provided"
                    ));
                }
                log::error!(
                    "chaoslabs selected but no feed map path provided; disabling chaoslabs provider"
                );
                active_chaoslabs = false;
            }
        }
    }

    let dry_run = is_dry_run();
    if dry_run {
        log::info!("DRY_RUN=true — will build payloads but skip on-chain sends");
    }

    let median_priority_fee = Arc::new(Mutex::new(0u64));
    if !dry_run {
        let median_priority_fee = Arc::clone(&median_priority_fee);
        tokio::spawn(async move {
            let mut fee_refresh_interval = interval(PRIORITY_FEE_REFRESH_INTERVAL);
            loop {
                fee_refresh_interval.tick().await;
                if let Ok(fee) =
                    fetch_mean_priority_fee(&client, MEAN_PRIORITY_FEE_PERCENTILE).await
                {
                    let mut fee_lock = median_priority_fee.lock().await;
                    *fee_lock = fee;
                    log::debug!(
                        "  <> Updated median priority fee percentile to: {} µLamports / cu",
                        fee
                    );
                }
            }
        });
    }

    let custody_accounts = if dry_run {
        log::info!("DRY_RUN: skipping custody account loading (no RPC needed)");
        vec![]
    } else if active_chaoslabs || active_autonom || active_switchboard {
        loop {
            match load_custody_accounts(&program, pool_pubkey).await {
                Ok(accounts) => break accounts,
                Err(err) => {
                    log::error!(
                        "Failed to load custody accounts (will retry in 5s): {:?}",
                        err
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    } else {
        vec![]
    };

    let (provider_updates_tx, mut provider_updates_rx) = mpsc::channel::<ProviderUpdate>(128);

    if active_chaoslabs {
        match db_pool.clone() {
            Some(chaoslabs_db_pool) => {
                let cycle =
                    make_chaoslabs_cycle(chaoslabs_db_pool, chaoslabs_feed_bindings.clone());
                let tx = provider_updates_tx.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        oracle_poller::run_provider_poller("chaoslabs", CHAOSLABS_CYCLE, cycle, tx)
                            .await
                    {
                        log::error!("ChaosLabs keeper exited with error: {err:?}");
                    }
                });
            }
            None => {
                if strict_provider_init {
                    return Err(anyhow::anyhow!(
                        "chaoslabs selected via CLI but DB pool is unavailable"
                    ));
                }
                log::error!("chaoslabs selected but DB pool unavailable; disabling chaoslabs");
                active_chaoslabs = false;
            }
        }
    }

    if active_switchboard {
        let switchboard_feed_map_path = args
            .switchboard_feed_map_path
            .clone()
            .or_else(|| env::var("SWITCHBOARD_FEED_MAP_PATH").ok());

        match switchboard_feed_map_path {
            Some(feed_map_path) => {
                let switchboard_feed_bindings = match switchboard::load_switchboard_feed_bindings(
                    &feed_map_path,
                ) {
                    Ok(bindings) => bindings,
                    Err(err) => {
                        if strict_provider_init {
                            return Err(anyhow::anyhow!(
                                "Invalid switchboard feed map at `{}` for selected provider: {:?}",
                                feed_map_path,
                                err
                            ));
                        }
                        log::error!(
                            "Invalid switchboard feed map at `{}`; disabling switchboard: {:?}",
                            feed_map_path,
                            err
                        );
                        active_switchboard = false;
                        vec![]
                    }
                };

                if active_switchboard {
                    let switchboard_queue_pubkey = match Pubkey::from_str(
                        &args.switchboard_queue_pubkey,
                    ) {
                        Ok(pubkey) => pubkey,
                        Err(err) => {
                            if strict_provider_init {
                                return Err(anyhow::anyhow!(
                                    "Invalid switchboard queue pubkey `{}` for selected provider: {:?}",
                                    args.switchboard_queue_pubkey,
                                    err
                                ));
                            }
                            log::error!(
                                "Invalid switchboard queue pubkey `{}`; disabling switchboard: {:?}",
                                args.switchboard_queue_pubkey,
                                err
                            );
                            active_switchboard = false;
                            Pubkey::default()
                        }
                    };

                    if active_switchboard {
                        match db_pool.clone() {
                            Some(switchboard_db_pool) => {
                                let poll_interval =
                                    Duration::from_millis(args.switchboard_poll_ms);
                                let switchboard_cfg = switchboard::SwitchboardRuntimeConfig {
                                    queue_pubkey: switchboard_queue_pubkey,
                                    max_age_slots: args.switchboard_max_age_slots,
                                    poll_interval,
                                    feed_bindings: switchboard_feed_bindings,
                                    instruction_idx: args.switchboard_instruction_idx,
                                };

                                let cycle = switchboard::make_switchboard_cycle(
                                    switchboard_db_pool,
                                    switchboard_cfg,
                                );
                                let tx = provider_updates_tx.clone();
                                tokio::spawn(async move {
                                    if let Err(err) = oracle_poller::run_provider_poller(
                                        "switchboard",
                                        poll_interval,
                                        cycle,
                                        tx,
                                    )
                                    .await
                                    {
                                        log::error!(
                                            "Switchboard keeper exited with error: {err:?}"
                                        );
                                    }
                                });
                            }
                            None => {
                                if strict_provider_init {
                                    return Err(anyhow::anyhow!(
                                        "switchboard selected but DB pool unavailable"
                                    ));
                                }
                                log::error!(
                                    "switchboard selected but DB pool unavailable; disabling"
                                );
                                active_switchboard = false;
                            }
                        }
                    }
                }
            }
            None => {
                if strict_provider_init {
                    return Err(anyhow::anyhow!(
                        "switchboard selected via CLI but required config is missing (feed map path)"
                    ));
                }
                log::error!(
                    "switchboard selected but required config is missing (feed map path); disabling switchboard"
                );
                active_switchboard = false;
            }
        }
    }

    if active_autonom {
        let autonom_feed_map_path = args
            .autonom_feed_map_path
            .clone()
            .or_else(|| env::var("AUTONOM_FEED_MAP_PATH").ok());

        match (db_pool.clone(), autonom_feed_map_path) {
            (Some(autonom_db_pool), Some(feed_map_path)) => {
                match autonom::make_autonom_cycle(autonom_db_pool, &feed_map_path) {
                    Ok(cycle) => {
                        let tx = provider_updates_tx.clone();
                        let poll_interval = Duration::from_millis(args.autonom_poll_ms);
                        tokio::spawn(async move {
                            if let Err(err) = oracle_poller::run_provider_poller(
                                "autonom",
                                poll_interval,
                                cycle,
                                tx,
                            )
                            .await
                            {
                                log::error!("Autonom keeper exited with error: {err:?}");
                            }
                        });
                    }
                    Err(err) => {
                        if strict_provider_init {
                            return Err(anyhow::anyhow!(
                                "Failed to initialize autonom cycle for selected provider: {:?}",
                                err
                            ));
                        }
                        log::error!(
                            "Failed to initialize autonom cycle; disabling autonom: {:?}",
                            err
                        );
                        active_autonom = false;
                    }
                }
            }
            (None, _) => {
                if strict_provider_init {
                    return Err(anyhow::anyhow!(
                        "autonom selected via CLI but DB pool is unavailable"
                    ));
                }
                log::error!("autonom selected but DB pool unavailable; disabling autonom");
                active_autonom = false;
            }
            (_, None) => {
                if strict_provider_init {
                    return Err(anyhow::anyhow!(
                        "autonom selected via CLI but no feed map path provided"
                    ));
                }
                log::error!("autonom selected but no feed map path provided; disabling autonom");
                active_autonom = false;
            }
        }
    }

    log::info!(
        "Active provider mode => chaoslabs={}, autonom={}, switchboard={}",
        active_chaoslabs,
        active_autonom,
        active_switchboard
    );

    if !active_chaoslabs && !active_autonom && !active_switchboard {
        log::error!("No active providers after startup checks; entering passive mode");
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    let mut pending_updates = PendingProviderUpdates::default();

    while let Some(update) = provider_updates_rx.recv().await {
        pending_updates.ingest(update);

        if !pending_updates.is_ready(active_chaoslabs, active_autonom, active_switchboard) {
            continue;
        }

        let (oracle_prices, multi_oracle_prices, switchboard_oracle_prices) = match pending_updates
            .take_payload(active_chaoslabs, active_autonom, active_switchboard)
        {
            Ok(payload) => payload,
            Err(err) => {
                log::error!("Failed to compose coordinated provider payload: {:?}", err);
                continue;
            }
        };

        if dry_run {
            let has_chaoslabs = oracle_prices.is_some();
            let has_multi = multi_oracle_prices.as_ref().map(|m| m.batches.len()).unwrap_or(0);
            let has_switchboard = switchboard_oracle_prices.is_some();
            log::info!(
                "   <> DRY_RUN payload ready: chaoslabs={}, multi_batches={}, switchboard={}",
                has_chaoslabs,
                has_multi,
                has_switchboard
            );
        } else {
            let priority_fee = *median_priority_fee.lock().await;
            let cu_limit = if active_switchboard {
                args.update_oracle_cu_limit
            } else {
                UPDATE_AUM_CU_LIMIT
            };

            if let Err(err) = handlers::update_pool_aum_combined(
                &program,
                pool_pubkey,
                priority_fee,
                cu_limit,
                oracle_prices,
                multi_oracle_prices,
                switchboard_oracle_prices,
                custody_accounts.clone(),
            )
            .await
            {
                log::error!("Coordinated update_pool_aum send failed (soft fail): {err:?}");
            }
        }
    }

    log::error!("All provider update channels are closed; entering passive mode");
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
