use {
    crate::adrena_ix::SwitchboardFeedMapEntry, adrena_abi::oracle::BatchPrices,
    solana_sdk::{instruction::Instruction, pubkey::Pubkey},
};

#[derive(Debug, Clone)]
pub struct SwitchboardOraclePricesUpdate {
    pub queue_pubkey: Pubkey,
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
    /// Ed25519 signature verification instruction (precedes quote store).
    pub ed25519_ix: Instruction,
    /// Quote program store instruction (updates the canonical SwitchboardQuote PDA).
    pub quote_store_ix: Instruction,
    /// Canonical SwitchboardQuote PDA pubkey (derived from queue + feed hashes).
    pub quote_account: Pubkey,
}

#[derive(Debug, Clone)]
pub enum ProviderUpdate {
    ChaosLabs(BatchPrices),
    Autonom(BatchPrices),
    Switchboard(SwitchboardOraclePricesUpdate),
}
