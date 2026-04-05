use {
    adrena_abi::{self, Pool},
    clap::Parser,
    openssl::ssl::{SslConnector, SslMethod},
    postgres_openssl::MakeTlsConnector,
    priority_fees::fetch_mean_priority_fee,
    solana_sdk::{
        instruction::AccountMeta,
        pubkey::Pubkey,
        signer::keypair::read_keypair_file,
    },
    std::{env, sync::Arc, thread::sleep, time::Duration},
    tokio::{sync::Mutex, time::interval, time::Instant},
};

pub mod db;
pub mod handlers;
pub mod priority_fees;
pub mod rpc_fallback;
pub mod utils;

use rpc_fallback::RpcFallback;
use utils::format_chaos_labs_oracle_entry_to_params::format_chaos_labs_oracle_entry_to_params;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:10000";
const MEAN_PRIORITY_FEE_PERCENTILE: u64 = 2500; // 25th
const PRIORITY_FEE_REFRESH_INTERVAL: Duration = Duration::from_secs(5); // seconds
const UPDATE_AUM_CU_LIMIT: u32 = 120_000;

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
    #[clap(long, default_value_t = String::from(DEFAULT_ENDPOINT))]
    /// Primary Solana JSON-RPC endpoint
    rpc: String,

    /// Commitment level: processed, confirmed or finalized
    #[clap(long)]
    commitment: Option<ArgsCommitment>,

    /// Path to the payer keypair
    #[clap(long)]
    payer_keypair: String,

    /// DB Url
    #[clap(long)]
    db_string: String,

    /// Combined certificate
    #[clap(long)]
    combined_cert: String,

    /// Backup RPC endpoint URL (optional, used as fallback)
    #[clap(long)]
    rpc_backup: Option<String>,

    /// Public RPC endpoint URL (last-resort fallback)
    #[clap(long, default_value_t = String::from(rpc_fallback::DEFAULT_PUBLIC_RPC))]
    rpc_public: String,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();

    let commitment: solana_sdk::commitment_config::CommitmentConfig =
        args.commitment.unwrap_or_default().into();

    let payer = read_keypair_file(args.payer_keypair.clone()).unwrap();
    let payer = Arc::new(payer);

    let rpc_fallback = Arc::new(RpcFallback::new(
        &args.rpc,
        args.rpc_backup.as_deref(),
        &args.rpc_public,
        commitment,
        Arc::clone(&payer),
    ));

    // ////////////////////////////////////////////////////////////////
    // DB CONNECTION POOL
    // ////////////////////////////////////////////////////////////////
    let mut ssl_builder = SslConnector::builder(SslMethod::tls()).unwrap();
    ssl_builder.set_ca_file(&args.combined_cert).map_err(|e| {
        log::error!("Failed to set CA file: {}", e);
        anyhow::anyhow!("Failed to set CA file: {}", e)
    })?;
    let tls_connector = MakeTlsConnector::new(ssl_builder.build());

    let mut db_config = args.db_string.parse::<tokio_postgres::Config>()?;

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

    // ////////////////////////////////////////////////////////////////
    // Side thread to fetch the median priority fee every 5 seconds
    // ////////////////////////////////////////////////////////////////
    let median_priority_fee = Arc::new(Mutex::new(0u64));
    // Spawn a task to poll priority fees every 5 seconds
    log::info!("3 - Spawn a task to poll priority fees every 5 seconds...");
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
                    log::debug!(
                        "  <> Updated mean priority fee to: {} µLamports / cu",
                        fee
                    );
                }
            }
        });
    }

    let pool = rpc_fallback
        .get_account::<Pool>(&adrena_abi::MAIN_POOL_ID, "Pool Fetch")
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get pool: {:?}", e))?;

    let mut custodies_accounts: Vec<AccountMeta> = vec![];
    for key in pool.custodies.iter() {
        if key != &Pubkey::default() {
            custodies_accounts.push(AccountMeta {
                pubkey: key.clone(),
                is_signer: false,
                is_writable: false,
            });
        }
    }

    let remaining_accounts = [custodies_accounts].concat();

    // ////////////////////////////////////////////////////////////////
    // CORE LOOP
    // - call updateAum IX to update Oracle and ALP prices onchain
    // ////////////////////////////////////////////////////////////////
    loop {
        let start = Instant::now();
        let assets_prices = db::get_assets_prices::get_assets_prices(&db_pool).await;
        match assets_prices {
            Ok(Some(assets_prices)) => {
                let last_trading_prices = format_chaos_labs_oracle_entry_to_params(&assets_prices);

                match last_trading_prices {
                    Ok(last_trading_prices) => {
                        // Copy the fee out of the lock BEFORE awaiting the TX send,
                        // otherwise the MutexGuard is held across the await and blocks
                        // the priority-fee refresher task for the duration of the send.
                        let fee = *median_priority_fee.lock().await;
                        // ignore errors on call since we want to keep executing IX
                        let _ = handlers::update_pool_aum(
                            &rpc_fallback,
                            fee,
                            last_trading_prices,
                            remaining_accounts.clone(),
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
}
