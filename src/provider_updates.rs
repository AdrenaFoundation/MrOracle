//! Variant types that flow from provider pollers to the main pool-dispatch
//! loop. The poller task reads from DB, constructs the appropriate variant,
//! and sends it over an mpsc channel. The main loop fans out to each pool
//! that needs the provider.
//!
//! `ProviderUpdate` is `Clone` so a single DB read can be distributed to
//! multiple pools without re-fetching.

use {
    crate::adrena_ix::{BatchPrices, SwitchboardFeedMapEntry},
    solana_sdk::{instruction::Instruction, pubkey::Pubkey},
};

/// Everything a Switchboard update needs to be reconstituted into an on-chain
/// tx. The ed25519_ix + quote_store_ix pair is decoded from the JSON stored
/// in `oracle_batches.metadata` by adrena-data's processSwitchboardInstructions.
///
/// - `ed25519_ix`: precompiled Ed25519 signature-verify instruction produced
///   by `queue.fetchManagedUpdateIxs` off-chain. Must be placed at tx index
///   `instruction_idx` (conventionally 2, after CU price + CU limit).
/// - `quote_store_ix`: the Switchboard quote program instruction that writes
///   the verified quote into the canonical `SwitchboardQuote` PDA.
/// - `quote_account`: the canonical SwitchboardQuote PDA derived from
///   `(queue, [feed_hashes])`. Appended as `remaining_accounts[0]` to the
///   `update_pool_aum` ix.
/// - `feed_map` / `max_age_slots`: used to construct
///   `SwitchboardUpdateParams` inside `UpdatePoolAumParams`.
#[derive(Debug, Clone)]
pub struct SwitchboardOraclePricesUpdate {
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
    pub ed25519_ix: Instruction,
    pub quote_store_ix: Instruction,
    pub quote_account: Pubkey,
}

/// Message pushed from a provider poller onto the dispatch channel. Main loop
/// recv()s these and fans out to each `PoolRuntime` whose `required_providers`
/// set contains the variant's kind.
#[derive(Debug, Clone)]
pub enum ProviderUpdate {
    ChaosLabs(BatchPrices),
    Autonom(BatchPrices),
    Switchboard(SwitchboardOraclePricesUpdate),
}

/// Discriminant used for pool requirement matching and logging. Cheap Copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    ChaosLabs,
    Autonom,
    Switchboard,
}

impl ProviderUpdate {
    pub fn kind(&self) -> ProviderKind {
        match self {
            ProviderUpdate::ChaosLabs(_) => ProviderKind::ChaosLabs,
            ProviderUpdate::Autonom(_) => ProviderKind::Autonom,
            ProviderUpdate::Switchboard(_) => ProviderKind::Switchboard,
        }
    }
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::ChaosLabs => "chaoslabs",
            ProviderKind::Autonom => "autonom",
            ProviderKind::Switchboard => "switchboard",
        }
    }

    pub fn from_str_lowercase(s: &str) -> Option<Self> {
        match s {
            "chaoslabs" => Some(ProviderKind::ChaosLabs),
            "autonom" => Some(ProviderKind::Autonom),
            "switchboard" => Some(ProviderKind::Switchboard),
            _ => None,
        }
    }
}
