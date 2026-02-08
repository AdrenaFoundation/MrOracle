use {
    anchor_client::{solana_sdk::signer::keypair::read_keypair_file, Client, Cluster},
    anyhow::Context,
    clap::Parser,
    openssl::ssl::{SslConnector, SslMethod},
    postgres_openssl::MakeTlsConnector,
    priority_fees::fetch_mean_priority_fee,
    solana_sdk::{instruction::AccountMeta, pubkey::Pubkey},
    std::{env, str::FromStr, sync::Arc, thread::sleep, time::Duration},
    tokio::{sync::Mutex, task::JoinHandle, time::interval, time::Instant},
};

pub mod adrena_ix;
pub mod autonom;
pub mod db;
pub mod handlers;
pub mod priority_fees;
pub mod switchboard;
pub mod utils;

use utils::format_chaos_labs_oracle_entry_to_params::{
    format_chaos_labs_oracle_entry_to_params, load_chaos_labs_feed_map,
};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:10000";
const MEAN_PRIORITY_FEE_PERCENTILE: u64 = 2500; // 25th
const PRIORITY_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(5); // seconds
const UPDATE_AUM_CU_LIMIT: u32 = 120_000;
const UPDATE_ORACLE_CU_LIMIT: u32 = 1_400_000;

const DEFAULT_SWITCHBOARD_CROSSBAR_URL: &str = "https://crossbar.switchboard.xyz";
const DEFAULT_SWITCHBOARD_NETWORK: &str = "mainnet";
const DEFAULT_SWITCHBOARD_MAINNET_QUEUE_PUBKEY: &str =
    "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";
const DEFAULT_SWITCHBOARD_DEVNET_QUEUE_PUBKEY: &str =
    "EYiAmGSdsQTuCw413V5BzaruWuCCSDgTPtBGvLkXHbe7";
const DEFAULT_SWITCHBOARD_CYCLE_MS: u64 = 5_000;
const DEFAULT_SWITCHBOARD_MAX_AGE_SLOTS: u64 = 32;
const DEFAULT_AUTONOM_API_URL: &str = "http://178.128.21.71:3000";
const DEFAULT_AUTONOM_POLL_MS: u64 = 3_000;

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum ArgsCommitment {
    #[default]
    Processed,
    Confirmed,
    Finalized,
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

    /// Switchboard API key (if omitted, reads SWITCHBOARD_API_KEY env var)
    #[clap(long)]
    switchboard_api_key: Option<String>,

    /// Crossbar base URL
    #[clap(long, default_value_t = String::from(DEFAULT_SWITCHBOARD_CROSSBAR_URL))]
    switchboard_crossbar_url: String,

    /// Dedicated gateway URL (optional). If omitted, fetched from crossbar.
    #[clap(long)]
    switchboard_gateway_url: Option<String>,

    /// Switchboard network passed to crossbar gateways route (mainnet/devnet)
    #[clap(long, default_value_t = String::from(DEFAULT_SWITCHBOARD_NETWORK))]
    switchboard_network: String,

    /// Switchboard queue pubkey
    #[clap(long, default_value_t = String::from(DEFAULT_SWITCHBOARD_MAINNET_QUEUE_PUBKEY))]
    switchboard_queue_pubkey: String,

    /// Path to switchboard feed mapping json
    #[clap(long)]
    switchboard_feed_map_path: Option<String>,

    /// Switchboard quote max age in slots
    #[clap(long, default_value_t = DEFAULT_SWITCHBOARD_MAX_AGE_SLOTS)]
    switchboard_max_age_slots: u64,

    /// Switchboard accumulation cycle length in milliseconds
    #[clap(long, default_value_t = DEFAULT_SWITCHBOARD_CYCLE_MS)]
    switchboard_cycle_ms: u64,

    /// Compute unit limit for update_oracle switchboard tx
    #[clap(long, default_value_t = UPDATE_ORACLE_CU_LIMIT)]
    update_oracle_cu_limit: u32,

    /// Autonom API base URL
    #[clap(long, default_value_t = String::from(DEFAULT_AUTONOM_API_URL))]
    autonom_api_url: String,

    /// Autonom API key (if omitted, reads AUTONOM_API_KEY env var, then defaults to readkey1)
    #[clap(long)]
    autonom_api_key: Option<String>,

    /// Path to autonom feed mapping json
    #[clap(long)]
    autonom_feed_map_path: Option<String>,

    /// Request fresh prices from autonom (if false, allows last-known values)
    #[clap(long, default_value_t = false)]
    autonom_fresh: bool,

    /// Autonom polling interval in milliseconds
    #[clap(long, default_value_t = DEFAULT_AUTONOM_POLL_MS)]
    autonom_poll_ms: u64,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();
    let individual_provider_flags = [
        args.only_chaoslabs,
        args.only_autonom,
        args.only_switchboard,
    ]
    .iter()
    .filter(|flag| **flag)
    .count();

    let run_chaoslabs = if individual_provider_flags == 0 {
        true
    } else {
        args.only_chaoslabs
    };
    let run_autonom = if individual_provider_flags == 0 {
        true
    } else {
        args.only_autonom
    };
    let run_switchboard = if individual_provider_flags == 0 {
        true
    } else {
        args.only_switchboard
    };

    log::info!(
        "Provider mode => chaoslabs={}, autonom={}, switchboard={}",
        run_chaoslabs,
        run_autonom,
        run_switchboard
    );

    let args = args.clone();
    let mut periodical_priority_fees_fetching_task: Option<JoinHandle<Result<(), anyhow::Error>>> =
        None;

    // In case it errored out, abort the fee task (will be recreated)
    if let Some(t) = periodical_priority_fees_fetching_task.take() {
        t.abort();
    }

    let payer = read_keypair_file(args.payer_keypair.clone()).unwrap();
    let payer = Arc::new(payer);
    let client = Client::new(
        Cluster::Custom(args.endpoint.clone(), args.endpoint.clone()),
        Arc::clone(&payer),
    );

    let program = client
        .program(adrena_abi::ID)
        .map_err(|e| anyhow::anyhow!("Failed to get program: {:?}", e))?;

    let db_pool = if run_chaoslabs {
        let combined_cert = args
            .combined_cert
            .clone()
            .ok_or_else(|| anyhow::anyhow!("chaoslabs mode requires --combined-cert"))?;
        let db_string = args
            .db_string
            .clone()
            .ok_or_else(|| anyhow::anyhow!("chaoslabs mode requires --db-string"))?;

        // ////////////////////////////////////////////////////////////////
        // DB CONNECTION POOL
        // ////////////////////////////////////////////////////////////////
        let mut ssl_builder = SslConnector::builder(SslMethod::tls()).unwrap();
        ssl_builder.set_ca_file(&combined_cert).map_err(|e| {
            log::error!("Failed to set CA file: {}", e);
            anyhow::anyhow!("Failed to set CA file: {}", e)
        })?;
        let tls_connector = MakeTlsConnector::new(ssl_builder.build());

        let mut db_config = db_string.parse::<tokio_postgres::Config>()?;

        // Add connection timeout and keepalive settings
        db_config.connect_timeout(Duration::from_secs(2)); // Fast timeout for 3s cycle, connection should be established in less than 2s
        db_config.keepalives(true);
        db_config.keepalives_idle(Duration::from_secs(6)); // Check connection health after 6s of inactivity, should not happen considering 3s cycles but it is better to be safe

        let mgr_config = deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Fast,
        };
        let mgr = deadpool_postgres::Manager::from_config(db_config, tls_connector, mgr_config);
        let db_pool = deadpool_postgres::Pool::builder(mgr)
            .max_size(1) // Single connection - only one query at a time
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to create DB pool: {:?}", e))?;
        Some(db_pool)
    } else {
        None
    };

    let chaoslabs_feed_bindings = if run_chaoslabs {
        let chaoslabs_feed_map_path = args
            .chaoslabs_feed_map_path
            .clone()
            .or_else(|| env::var("CHAOSLABS_FEED_MAP_PATH").ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "chaoslabs mode requires --chaoslabs-feed-map-path or CHAOSLABS_FEED_MAP_PATH"
                )
            })?;

        load_chaos_labs_feed_map(&chaoslabs_feed_map_path)?
    } else {
        vec![]
    };

    // ////////////////////////////////////////////////////////////////
    // Side thread to fetch the median priority fee every 5 seconds
    // ////////////////////////////////////////////////////////////////
    let median_priority_fee = Arc::new(Mutex::new(0u64));
    // Spawn a task to poll priority fees every 5 seconds
    log::info!("3 - Spawn a task to poll priority fees every 5 seconds...");
    #[allow(unused_assignments)]
    {
        periodical_priority_fees_fetching_task = Some({
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
                                "  <> Updated median priority fee 30th percentile to : {} µLamports / cu",
                                fee
                            );
                    }
                }
            })
        });
    }

    let custody_accounts = if run_chaoslabs {
        let pool = program
            .account::<adrena_abi::Pool>(adrena_abi::MAIN_POOL_ID)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get pool from DB: {:?}", e))?;

        let mut custodies_accounts: Vec<AccountMeta> = vec![];
        for key in pool.custodies.iter() {
            if key != &Pubkey::default() {
                custodies_accounts.push(AccountMeta {
                    pubkey: *key,
                    is_signer: false,
                    is_writable: false,
                });
            }
        }
        [custodies_accounts].concat()
    } else {
        vec![]
    };

    if run_switchboard {
        let switchboard_api_key = args
            .switchboard_api_key
            .clone()
            .or_else(|| env::var("SWITCHBOARD_API_KEY").ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "switchboard is enabled but no api key provided (use --switchboard-api-key or SWITCHBOARD_API_KEY env var)"
                )
            })?;

        let switchboard_feed_map_path = args
            .switchboard_feed_map_path
            .clone()
            .or_else(|| env::var("SWITCHBOARD_FEED_MAP_PATH").ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "switchboard is enabled but no feed map path provided (--switchboard-feed-map-path or SWITCHBOARD_FEED_MAP_PATH)"
                )
            })?;

        let switchboard_feed_bindings =
            switchboard::load_switchboard_feed_bindings(&switchboard_feed_map_path)?;
        let switchboard_queue_pubkey = Pubkey::from_str(&args.switchboard_queue_pubkey)
            .with_context(|| {
                format!(
                    "invalid switchboard queue pubkey `{}`",
                    args.switchboard_queue_pubkey
                )
            })?;

        // Adrena's on-chain verifier currently accepts only the default Switchboard queue.
        let expected_queue_for_network = if args
            .switchboard_network
            .to_ascii_lowercase()
            .contains("devnet")
        {
            Pubkey::from_str(DEFAULT_SWITCHBOARD_DEVNET_QUEUE_PUBKEY).unwrap()
        } else {
            Pubkey::from_str(DEFAULT_SWITCHBOARD_MAINNET_QUEUE_PUBKEY).unwrap()
        };
        if switchboard_queue_pubkey != expected_queue_for_network {
            return Err(anyhow::anyhow!(
                "switchboard queue pubkey {} does not match default queue {} for network `{}`; adrena on-chain verification currently requires the default queue",
                switchboard_queue_pubkey,
                expected_queue_for_network,
                args.switchboard_network
            ));
        }

        let switchboard_cfg = switchboard::SwitchboardRuntimeConfig {
            api_key: switchboard_api_key,
            crossbar_url: args.switchboard_crossbar_url.clone(),
            gateway_url: args.switchboard_gateway_url.clone(),
            network: args.switchboard_network.clone(),
            queue_pubkey: switchboard_queue_pubkey,
            max_age_slots: args.switchboard_max_age_slots,
            cycle_timeout: Duration::from_millis(args.switchboard_cycle_ms),
            update_oracle_cu_limit: args.update_oracle_cu_limit,
            feed_bindings: switchboard_feed_bindings,
        };

        let switchboard_client = Client::new(
            Cluster::Custom(args.endpoint.clone(), args.endpoint.clone()),
            Arc::clone(&payer),
        );
        let switchboard_program = switchboard_client
            .program(adrena_abi::ID)
            .map_err(|e| anyhow::anyhow!("Failed to create switchboard program client: {:?}", e))?;

        switchboard::validate_switchboard_feed_bindings_against_oracle(
            &switchboard_program,
            &switchboard_cfg.feed_bindings,
        )
        .await?;

        let switchboard_priority_fee = Arc::clone(&median_priority_fee);
        tokio::spawn(async move {
            if let Err(err) = switchboard::run_switchboard_keeper(
                switchboard_program,
                switchboard_priority_fee,
                switchboard_cfg,
            )
            .await
            {
                log::error!("Switchboard keeper exited with error: {:?}", err);
            }
        });
    }

    if run_autonom {
        let autonom_startup_result: Result<(), anyhow::Error> = (|| {
            let autonom_api_key = args
            .autonom_api_key
            .clone()
            .or_else(|| env::var("AUTONOM_API_KEY").ok())
            .unwrap_or_else(|| {
                log::warn!(
                    "autonom is enabled without --autonom-api-key/AUTONOM_API_KEY; defaulting to readkey1"
                );
                "readkey1".to_string()
            });

            let autonom_feed_map_path = args
                .autonom_feed_map_path
                .clone()
                .or_else(|| env::var("AUTONOM_FEED_MAP_PATH").ok())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "autonom is enabled but no feed map path provided (--autonom-feed-map-path or AUTONOM_FEED_MAP_PATH)"
                    )
                })?;

            let autonom_cfg = autonom::AutonomRuntimeConfig {
                api_url: args.autonom_api_url.clone(),
                api_key: autonom_api_key,
                feed_map_path: autonom_feed_map_path,
                fresh: args.autonom_fresh,
                poll_interval: Duration::from_millis(args.autonom_poll_ms),
                update_oracle_cu_limit: args.update_oracle_cu_limit,
            };

            let autonom_client = Client::new(
                Cluster::Custom(args.endpoint.clone(), args.endpoint.clone()),
                Arc::clone(&payer),
            );
            let autonom_program = autonom_client
                .program(adrena_abi::ID)
                .map_err(|e| anyhow::anyhow!("Failed to create autonom program client: {:?}", e))?;

            let autonom_priority_fee = Arc::clone(&median_priority_fee);
            tokio::spawn(async move {
                if let Err(err) =
                    autonom::run_autonom_keeper(autonom_program, autonom_priority_fee, autonom_cfg)
                        .await
                {
                    log::error!("Autonom keeper exited with error: {:?}", err);
                }
            });

            Ok(())
        })();

        if let Err(err) = autonom_startup_result {
            return Err(err);
        }
    }

    // ////////////////////////////////////////////////////////////////
    // CORE LOOP
    // - call updateAum IX to update Oracle and ALP prices onchain
    // ////////////////////////////////////////////////////////////////
    if run_chaoslabs {
        let db_pool = db_pool.expect("db_pool must be initialized when chaoslabs is enabled");

        loop {
            let start = Instant::now();
            let assets_prices = db::get_assets_prices::get_assets_prices(&db_pool).await;
            match assets_prices {
                Ok(Some(assets_prices)) => {
                    let last_trading_prices = format_chaos_labs_oracle_entry_to_params(
                        &assets_prices,
                        &chaoslabs_feed_bindings,
                    );

                    match last_trading_prices {
                        Ok(last_trading_prices) => {
                            // ignore errors on call since we want to keep executing IX
                            let _ = handlers::update_pool_aum(
                                &program,
                                *median_priority_fee.lock().await,
                                last_trading_prices,
                                custody_accounts.clone(),
                            )
                            .await;
                        }
                        Err(e) => {
                            log::error!("Error formatting assets prices: {:?}", e);
                        }
                    }
                }
                Ok(None) => {
                    log::warn!("No assets prices found in DB - Sleeping 500ms");
                }
                Err(e) => {
                    log::error!("Database query error: {:?} - Sleeping 500ms", e);
                }
            }

            let elapsed = start.elapsed();
            let sleep_duration = Duration::from_secs(5).saturating_sub(elapsed);
            if sleep_duration > Duration::from_millis(0) {
                sleep(sleep_duration);
            }
        }
    } else {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
}
