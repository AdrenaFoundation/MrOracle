use {
    crate::{
        adrena_ix::{
            BatchPricesWithProvider, MultiBatchPrices, ORACLE_PROVIDER_AUTONOM,
            ORACLE_PROVIDER_CHAOS_LABS,
        },
        provider_updates::{ProviderUpdate, SwitchboardOraclePricesUpdate},
    },
    adrena_abi::oracle::BatchPrices,
    serde::Deserialize,
    solana_sdk::{instruction::AccountMeta, pubkey::Pubkey},
    std::{collections::HashSet, str::FromStr},
};

const DEFAULT_CU_LIMIT: u32 = 120_000;
const DEFAULT_CU_LIMIT_WITH_SWITCHBOARD: u32 = 1_400_000;

// ── Config file types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct PoolEntry {
    pub name: String,
    pub address: String,
    pub providers: Vec<String>,
    pub cu_limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolsConfig {
    pub pools: Vec<PoolEntry>,
}

// ── Runtime types ────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct PerPoolPending {
    chaoslabs: Option<BatchPrices>,
    autonom: Option<BatchPrices>,
    switchboard: Option<SwitchboardOraclePricesUpdate>,
}

impl PerPoolPending {
    pub fn ingest(&mut self, update: &ProviderUpdate) {
        match update {
            ProviderUpdate::ChaosLabs(batch) => self.chaoslabs = Some(batch.clone()),
            ProviderUpdate::Autonom(batch) => self.autonom = Some(batch.clone()),
            ProviderUpdate::Switchboard(sb) => self.switchboard = Some(sb.clone()),
        }
    }

    pub fn is_ready(
        &self,
        needs_chaoslabs: bool,
        needs_autonom: bool,
        needs_switchboard: bool,
    ) -> bool {
        (!needs_chaoslabs || self.chaoslabs.is_some())
            && (!needs_autonom || self.autonom.is_some())
            && (!needs_switchboard || self.switchboard.is_some())
    }

    pub fn take_payload(
        &mut self,
        needs_chaoslabs: bool,
        needs_autonom: bool,
        needs_switchboard: bool,
    ) -> Result<
        (
            Option<BatchPrices>,
            Option<MultiBatchPrices>,
            Option<SwitchboardOraclePricesUpdate>,
        ),
        anyhow::Error,
    > {
        let switchboard_oracle_prices = if needs_switchboard {
            Some(
                self.switchboard
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("missing switchboard payload"))?,
            )
        } else {
            None
        };

        match (needs_chaoslabs, needs_autonom) {
            (true, false) => {
                let chaoslabs = self
                    .chaoslabs
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("missing chaoslabs payload"))?;
                let multi = MultiBatchPrices {
                    batches: vec![BatchPricesWithProvider {
                        provider: ORACLE_PROVIDER_CHAOS_LABS,
                        batch: chaoslabs,
                    }],
                };
                Ok((None, Some(multi), switchboard_oracle_prices))
            }
            (false, true) => {
                let autonom = self
                    .autonom
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("missing autonom payload"))?;
                let multi = MultiBatchPrices {
                    batches: vec![BatchPricesWithProvider {
                        provider: ORACLE_PROVIDER_AUTONOM,
                        batch: autonom,
                    }],
                };
                Ok((None, Some(multi), switchboard_oracle_prices))
            }
            (true, true) => {
                let chaoslabs = self
                    .chaoslabs
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("missing chaoslabs payload"))?;
                let autonom = self
                    .autonom
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("missing autonom payload"))?;
                let multi = MultiBatchPrices {
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
                Ok((None, Some(multi), switchboard_oracle_prices))
            }
            (false, false) => Ok((None, None, switchboard_oracle_prices)),
        }
    }
}

pub struct PoolRuntime {
    pub name: String,
    pub address: Pubkey,
    pub needs_chaoslabs: bool,
    pub needs_autonom: bool,
    pub needs_switchboard: bool,
    pub cu_limit: u32,
    pub custody_accounts: Vec<AccountMeta>,
    pub pending: PerPoolPending,
}

// ── Config loading ───────────────────────────────────────────────────────────

pub fn load_pools_config(path: &str) -> Result<PoolsConfig, anyhow::Error> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read pools config at `{}`: {}", path, e))?;
    let config: PoolsConfig = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse pools config at `{}`: {}", path, e))?;

    if config.pools.is_empty() {
        return Err(anyhow::anyhow!(
            "pools config at `{}` contains no pools",
            path
        ));
    }

    Ok(config)
}

pub fn resolve_pool_runtimes(config: &PoolsConfig) -> Result<Vec<PoolRuntime>, anyhow::Error> {
    let mut runtimes = Vec::with_capacity(config.pools.len());

    for entry in &config.pools {
        let address = Pubkey::from_str(&entry.address).map_err(|e| {
            anyhow::anyhow!(
                "invalid pool address `{}` for pool `{}`: {}",
                entry.address,
                entry.name,
                e
            )
        })?;

        let providers: HashSet<String> =
            entry.providers.iter().map(|p| p.to_lowercase()).collect();
        let needs_chaoslabs = providers.contains("chaoslabs");
        let needs_autonom = providers.contains("autonom");
        let needs_switchboard = providers.contains("switchboard");

        if !needs_chaoslabs && !needs_autonom && !needs_switchboard {
            return Err(anyhow::anyhow!(
                "pool `{}` has no valid providers configured",
                entry.name
            ));
        }

        let cu_limit = entry.cu_limit.unwrap_or(if needs_switchboard {
            DEFAULT_CU_LIMIT_WITH_SWITCHBOARD
        } else {
            DEFAULT_CU_LIMIT
        });

        runtimes.push(PoolRuntime {
            name: entry.name.clone(),
            address,
            needs_chaoslabs,
            needs_autonom,
            needs_switchboard,
            cu_limit,
            custody_accounts: vec![],
            pending: PerPoolPending::default(),
        });
    }

    Ok(runtimes)
}

/// Union of all pools' provider requirements — determines which global pollers to start.
pub fn aggregate_provider_requirements(runtimes: &[PoolRuntime]) -> (bool, bool, bool) {
    let mut chaoslabs = false;
    let mut autonom = false;
    let mut switchboard = false;

    for rt in runtimes {
        chaoslabs |= rt.needs_chaoslabs;
        autonom |= rt.needs_autonom;
        switchboard |= rt.needs_switchboard;
    }

    (chaoslabs, autonom, switchboard)
}
