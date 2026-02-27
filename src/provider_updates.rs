use {
    crate::adrena_ix::SwitchboardFeedMapEntry, adrena_abi::oracle::ChaosLabsBatchPrices,
    solana_sdk::{instruction::Instruction, pubkey::Pubkey},
};

#[derive(Debug, Clone)]
pub struct SwitchboardOraclePricesUpdate {
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
    /// The secp256k1 signature verification instruction (precedes submit).
    pub secp_ix: Instruction,
    /// The PullFeedSubmitResponseConsensus instruction.
    pub submit_ix: Instruction,
    /// PullFeed account pubkeys, in the same order as feed_map.
    pub pull_feed_pubkeys: Vec<Pubkey>,
}

#[derive(Debug, Clone)]
pub enum ProviderUpdate {
    ChaosLabs(ChaosLabsBatchPrices),
    Autonom(ChaosLabsBatchPrices),
    Switchboard(SwitchboardOraclePricesUpdate),
}
