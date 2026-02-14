use {
    crate::adrena_ix::SwitchboardFeedMapEntry, adrena_abi::oracle::ChaosLabsBatchPrices,
    solana_sdk::pubkey::Pubkey, switchboard_on_demand::client::surge::SurgeUpdate,
};

#[derive(Debug, Clone)]
pub struct SwitchboardOraclePricesUpdate {
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
    pub updates: Vec<SurgeUpdate>,
}

#[derive(Debug, Clone)]
pub enum ProviderUpdate {
    ChaosLabs(ChaosLabsBatchPrices),
    Autonom(ChaosLabsBatchPrices),
    Switchboard(SwitchboardOraclePricesUpdate),
}
