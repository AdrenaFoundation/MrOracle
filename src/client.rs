use {
    adrena_abi::{self, Pool},
    anchor_client::{solana_sdk::signer::keypair::read_keypair_file, Client, Cluster},
    clap::Parser,
    openssl::ssl::{SslConnector, SslMethod},
    postgres_openssl::MakeTlsConnector,
    priority_fees::fetch_mean_priority_fee,
    solana_sdk::{instruction::AccountMeta, pubkey::Pubkey},
    std::{env, sync::Arc, thread::sleep, time::Duration},
    tokio::{sync::Mutex, task::JoinHandle, time::interval, time::Instant},
};

pub mod db;
pub mod handlers;
pub mod priority_fees;
pub mod utils;

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
    db_string: String,

    /// Combined certificate
    #[clap(long)]
    combined_cert: String,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();

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

    // ////////////////////////////////////////////////////////////////
    // DB CONNECTION
    // ////////////////////////////////////////////////////////////////
    // Connect to the DB that contains the table matching the UserStaking accounts to their owners
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();

    // Use the combined certificate
    let _ = builder
        .set_ca_file(&args.combined_cert)
        .map_err(|e| anyhow::anyhow!("Failed to set CA file: {}", e));

    let connector = MakeTlsConnector::new(builder.build());

    let (db, db_connection) = tokio_postgres::connect(&args.db_string, connector)
        .await
        .map_err(|e| anyhow::anyhow!("PostgreSQL connection error: {:?}", e))?;

    // Continous polling to keep connection alive in a separate thread
    tokio::spawn(async move {
        if let Err(e) = db_connection.await {
            log::error!("PostgreSQL connection error: {}", e);
        }
    });

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

    let pool = program
        .account::<Pool>(adrena_abi::MAIN_POOL_ID)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get pool from DB: {:?}", e))?;

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
        let assets_prices = db::get_assets_prices::get_assets_prices(&db).await;
        match assets_prices {
            Ok(Some(assets_prices)) => {
                let last_trading_prices = format_chaos_labs_oracle_entry_to_params(&assets_prices);

                match last_trading_prices {
                    Ok(last_trading_prices) => {
                        // ignore errors on call since we want to keep executing IX
                        let _ = handlers::update_pool_aum(
                            &program,
                            *median_priority_fee.lock().await,
                            last_trading_prices,
                            remaining_accounts.clone(),
                        )
                        .await;
                    }
                    Err(e) => {
                        log::error!("Error formatting assets prices: {:?}", e);
                    }
                }

                let elapsed = start.elapsed();

                sleep(Duration::from_secs(3 - elapsed.as_secs()));
            }
            Ok(None) => {
                log::info!("No assets prices found in DB - Sleeping 5 seconds");
                sleep(Duration::from_millis(500));
            }
            Err(e) => {
                log::error!("Error getting assets prices: {:?}", e);
                sleep(Duration::from_millis(500));
            }
        }
    }
}
