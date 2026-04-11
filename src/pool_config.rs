//! Pool runtime + operator config.
//!
//! The operator supplies a `pools_config.json` file listing each pool that
//! MrOracle should update, which providers that pool requires, and the
//! ordered custody account list (matching `pool.custodies[]` on-chain).
//!
//! At runtime, each pool's `PerPoolPending` state is a tiny state machine
//! that collects incoming provider updates over a 5-second window. The main
//! dispatch loop either fires the tx early (when all required providers have
//! arrived) or on window expiry (with whatever has accumulated).

use {
    crate::{
        adrena_ix::BatchPrices,
        provider_updates::{ProviderKind, ProviderUpdate, SwitchboardOraclePricesUpdate},
    },
    anyhow::{anyhow, Context},
    serde::Deserialize,
    solana_sdk::{instruction::AccountMeta, pubkey::Pubkey},
    std::{
        collections::HashSet,
        fs,
        str::FromStr,
        sync::Arc,
        time::{Duration, Instant},
    },
    tokio::sync::Mutex,
};

/// 5s is the collection budget for batching multi-provider updates into one
/// `update_pool_aum` tx. Adrena on-chain staleness is 7s (oracle.rs:33 STALENESS)
/// so 5s gives us ~2s of headroom for tx propagation + confirm.
pub const COLLECTION_WINDOW: Duration = Duration::from_secs(5);

/// Default compute unit limit for an `update_pool_aum` tx carrying only
/// ChaosLabs/Autonom batches (no Switchboard). Matches main's UPDATE_AUM_CU_LIMIT.
pub const DEFAULT_CU_LIMIT: u32 = 200_000;

/// Default compute unit limit when the tx also carries the ed25519 + quote
/// store instructions for Switchboard. These precompiles are CU-heavy.
pub const DEFAULT_CU_LIMIT_WITH_SWITCHBOARD: u32 = 1_400_000;

// ─────────────────────────────────────────────────────────────────────────────
// Operator config file schema
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PoolsConfigFile {
    pub pools: Vec<PoolConfigEntry>,
}

#[derive(Debug, Deserialize)]
pub struct PoolConfigEntry {
    /// Human-readable name for logs. Doesn't need to match on-chain pool.name.
    pub name: String,
    /// Pool PDA as base58. Operator gets this from the adrena CLI.
    pub address: String,
    /// Which providers' batches are required before firing a tx for this pool.
    /// Entries are lowercase strings matching `ProviderKind::from_str_lowercase`.
    pub providers: Vec<String>,
    /// Custody PDAs in the exact order of `pool.custodies[]` on-chain.
    /// Operator must keep this in sync with any add_custody / remove_custody ops.
    pub custodies: Vec<String>,
    /// Compute unit limit. If absent, defaults to `DEFAULT_CU_LIMIT_WITH_SWITCHBOARD`
    /// when the pool requires Switchboard, else `DEFAULT_CU_LIMIT`.
    #[serde(default)]
    pub cu_limit: Option<u32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// PoolRuntime — runtime state for one pool
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct PerPoolPending {
    pub chaoslabs: Option<BatchPrices>,
    pub autonom: Option<BatchPrices>,
    pub switchboard: Option<SwitchboardOraclePricesUpdate>,
    /// Instant at which the first provider arrived in the current collection
    /// window. Reset to None after `take_snapshot` clears the pending state.
    pub first_arrival_at: Option<Instant>,
}

impl PerPoolPending {
    pub fn ingest(&mut self, update: ProviderUpdate) {
        if self.first_arrival_at.is_none() {
            self.first_arrival_at = Some(Instant::now());
        }
        match update {
            ProviderUpdate::ChaosLabs(batch) => self.chaoslabs = Some(batch),
            ProviderUpdate::Autonom(batch) => self.autonom = Some(batch),
            ProviderUpdate::Switchboard(sb) => self.switchboard = Some(sb),
        }
    }

    /// True iff every provider in `required` has a payload pending.
    pub fn is_complete(&self, required: &HashSet<ProviderKind>) -> bool {
        for kind in required {
            let has = match kind {
                ProviderKind::ChaosLabs => self.chaoslabs.is_some(),
                ProviderKind::Autonom => self.autonom.is_some(),
                ProviderKind::Switchboard => self.switchboard.is_some(),
            };
            if !has {
                return false;
            }
        }
        true
    }

    /// True iff the 5s collection window has elapsed since the first arrival.
    /// False if no provider has arrived yet (no reason to fire an empty tx).
    pub fn is_window_expired(&self, window: Duration) -> bool {
        self.first_arrival_at
            .map(|t| t.elapsed() >= window)
            .unwrap_or(false)
    }

    pub fn has_anything(&self) -> bool {
        self.chaoslabs.is_some() || self.autonom.is_some() || self.switchboard.is_some()
    }

    /// Drain the pending state into a snapshot and reset the window.
    pub fn take_snapshot(&mut self) -> PoolSnapshot {
        let snap = PoolSnapshot {
            chaoslabs: self.chaoslabs.take(),
            autonom: self.autonom.take(),
            switchboard: self.switchboard.take(),
        };
        self.first_arrival_at = None;
        snap
    }
}

/// The "firing" payload — a complete (or partial) set of provider data for
/// one pool, ready to be packed into an `update_pool_aum` tx.
#[derive(Debug)]
pub struct PoolSnapshot {
    pub chaoslabs: Option<BatchPrices>,
    pub autonom: Option<BatchPrices>,
    pub switchboard: Option<SwitchboardOraclePricesUpdate>,
}

impl PoolSnapshot {
    pub fn has_switchboard(&self) -> bool {
        self.switchboard.is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.chaoslabs.is_none() && self.autonom.is_none() && self.switchboard.is_none()
    }
}

/// Resolved runtime view of one pool, held by the main dispatch loop.
#[derive(Debug)]
pub struct PoolRuntime {
    pub name: String,
    pub address: Pubkey,
    pub required_providers: HashSet<ProviderKind>,
    pub custody_accounts: Vec<AccountMeta>,
    pub cu_limit: u32,
    pub pending: Arc<Mutex<PerPoolPending>>,
}

impl PoolRuntime {
    pub fn from_config(entry: PoolConfigEntry) -> anyhow::Result<Self> {
        let address = Pubkey::from_str(&entry.address)
            .with_context(|| format!("pool `{}` address is not a valid pubkey", entry.name))?;

        let mut required_providers: HashSet<ProviderKind> = HashSet::new();
        for raw in &entry.providers {
            let kind = ProviderKind::from_str_lowercase(&raw.to_lowercase()).ok_or_else(|| {
                anyhow!(
                    "pool `{}` has unknown provider `{}` (expected chaoslabs | autonom | switchboard)",
                    entry.name,
                    raw
                )
            })?;
            required_providers.insert(kind);
        }

        if required_providers.is_empty() {
            return Err(anyhow!("pool `{}` has empty providers list", entry.name));
        }

        let custody_accounts = entry
            .custodies
            .iter()
            .map(|s| {
                Pubkey::from_str(s)
                    .with_context(|| format!("pool `{}` custody `{s}` is not a valid pubkey", entry.name))
                    .map(|pk| AccountMeta::new_readonly(pk, false))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let has_switchboard = required_providers.contains(&ProviderKind::Switchboard);
        let default_cu = if has_switchboard {
            DEFAULT_CU_LIMIT_WITH_SWITCHBOARD
        } else {
            DEFAULT_CU_LIMIT
        };
        let cu_limit = entry.cu_limit.unwrap_or(default_cu);

        Ok(Self {
            name: entry.name,
            address,
            required_providers,
            custody_accounts,
            cu_limit,
            pending: Arc::new(Mutex::new(PerPoolPending::default())),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Config loader + provider aggregation
// ─────────────────────────────────────────────────────────────────────────────

pub fn load_pools_config(path: &str) -> anyhow::Result<Vec<PoolRuntime>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read pools_config from `{path}`"))?;
    let parsed: PoolsConfigFile = serde_json::from_str(&raw)
        .with_context(|| format!("parse pools_config at `{path}`"))?;

    if parsed.pools.is_empty() {
        return Err(anyhow!("pools_config at `{path}` has no pools"));
    }

    parsed
        .pools
        .into_iter()
        .map(PoolRuntime::from_config)
        .collect()
}

/// Union of every required provider across all pools. The main loop uses this
/// to decide which provider pollers to spawn. Generic over the container so
/// callers can pass either `&[PoolRuntime]` or `&[Arc<PoolRuntime>]`.
pub fn aggregate_provider_requirements<T: AsRef<PoolRuntime>>(
    pools: &[T],
) -> HashSet<ProviderKind> {
    let mut out = HashSet::new();
    for pool in pools {
        out.extend(pool.as_ref().required_providers.iter().copied());
    }
    out
}

impl AsRef<PoolRuntime> for PoolRuntime {
    fn as_ref(&self) -> &PoolRuntime {
        self
    }
}
