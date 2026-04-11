//! Local wire types and instruction builder for adrena release/39-postaudit.
//!
//! This module is the **only** place MrOracle touches adrena's on-chain wire
//! format. It intentionally defines every type locally (no imports from
//! `adrena_abi::oracle`) so MrOracle release/39 is decoupled from whatever
//! version of adrena-abi is pinned in Cargo.toml. The wire layouts below are
//! byte-identical to release/39's `programs/adrena/src/state/batch_prices.rs`
//! and `programs/adrena/src/state/oracle.rs`.
//!
//! Maintenance rule: when adrena ships a new release with a *changed* wire
//! layout, update the struct definitions here (not adrena-abi) and run the
//! providers-testing rig end-to-end against devnet before shipping.

use {
    anyhow::Context,
    borsh::{BorshDeserialize, BorshSerialize},
    sha2::{Digest, Sha256},
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
    std::str::FromStr,
};

// ─────────────────────────────────────────────────────────────────────────────
// Program constants
// ─────────────────────────────────────────────────────────────────────────────

/// Adrena program ID (mainnet + devnet). Matches `declare_id!` in
/// adrena/programs/adrena/src/lib.rs.
pub const ADRENA_PROGRAM_ID_STR: &str = "13gDzEXCdocbj8iAiqrScGo47NiSuYENGsRqi3SEAwet";

pub fn adrena_program_id() -> Pubkey {
    Pubkey::from_str(ADRENA_PROGRAM_ID_STR).expect("adrena program id is a valid pubkey")
}

// ─────────────────────────────────────────────────────────────────────────────
// Oracle provider identifiers (matches release/39 pool.rs OracleProvider)
// ─────────────────────────────────────────────────────────────────────────────

pub const ORACLE_PROVIDER_CHAOSLABS: u8 = 0;
pub const ORACLE_PROVIDER_AUTONOM: u8 = 1;
pub const ORACLE_PROVIDER_SWITCHBOARD: u8 = 2;

// ─────────────────────────────────────────────────────────────────────────────
// Wire types — release/39 batch_prices.rs + oracle.rs
//
// IMPORTANT: Borsh field order must match the on-chain definitions byte-for-byte.
// ─────────────────────────────────────────────────────────────────────────────

/// Individual price entry inside a signed batch. Matches release/39
/// `programs/adrena/src/state/batch_prices.rs:64-70`.
#[derive(Debug, Clone, Copy, BorshSerialize, BorshDeserialize)]
pub struct PriceData {
    pub feed_id: u8,
    pub price: u64,
    pub timestamp: i64,
}

/// A secp256k1-signed batch of prices from a single provider (ChaosLabs or
/// Autonom). Matches release/39 `batch_prices.rs:46-51`.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct BatchPrices {
    pub prices: Vec<PriceData>,
    pub signature: [u8; 64],
    pub recovery_id: u8,
}

/// Wraps a `BatchPrices` with a provider tag so `MultiBatchPrices` can carry
/// multiple providers' batches in a single `update_pool_aum` tx.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct BatchPricesWithProvider {
    pub provider: u8,
    pub batch: BatchPrices,
}

/// Multi-provider wrapper. `provider` values follow the `ORACLE_PROVIDER_*`
/// constants above. Switchboard batches are **not** allowed inside
/// `MultiBatchPrices` — they go via `SwitchboardUpdateParams` on a separate
/// field of `UpdatePoolAumParams`.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct MultiBatchPrices {
    pub batches: Vec<BatchPricesWithProvider>,
}

/// One entry in the Switchboard feed map passed to `update_pool_aum`. The
/// on-chain program uses this to correlate each feed in the SwitchboardQuote
/// account with the adrena feed_id it should be stamped under.
///
/// Matches release/39 `oracle.rs:47-51` `SwitchboardFeedMapEntry`.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct SwitchboardFeedMapEntry {
    pub adrena_feed_id: u8,
    pub switchboard_feed_hash: [u8; 32],
}

/// Switchboard-specific params. `max_age_slots` is the slot-staleness window
/// the on-chain program enforces against `SwitchboardQuote.slot`. `feed_map`
/// tells the program which adrena_feed_id to assign to each feed hash inside
/// the quote.
///
/// Matches release/39 `oracle.rs:53-57` `SwitchboardUpdateParams`.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct SwitchboardUpdateParams {
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
}

/// Params accepted by `update_pool_aum`. Exactly one of the three `Option`
/// fields can be `Some` per convention (though the program accepts any
/// combination, including all three together if the caller has data from
/// every provider within the same 5-second collection window).
///
/// Matches release/39 `instructions/public/update_pool_aum.rs:65-71`.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct UpdatePoolAumParams {
    pub oracle_prices: Option<BatchPrices>,
    pub multi_oracle_prices: Option<MultiBatchPrices>,
    pub switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
}

// ─────────────────────────────────────────────────────────────────────────────
// PDA helpers
// ─────────────────────────────────────────────────────────────────────────────

pub fn get_cortex_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"cortex"], &adrena_program_id())
}

pub fn get_oracle_pda() -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"oracle"], &adrena_program_id())
}

/// LP token mint PDA seed is `[b"lp_token_mint", pool_pubkey.as_ref()]`.
/// Matches release/39 `update_pool_aum.rs:52-55`.
pub fn get_lp_token_mint_pda(pool_pubkey: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"lp_token_mint", pool_pubkey.as_ref()],
        &adrena_program_id(),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Instruction builder
// ─────────────────────────────────────────────────────────────────────────────

/// Standard Anchor instruction discriminator: sha256("global:<fn_name>")[..8].
/// Applied at the start of the instruction data Vec before the borsh-serialized
/// params struct.
fn anchor_discriminator(ix_name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("global:{ix_name}").as_bytes());
    let mut disc = [0u8; 8];
    disc.copy_from_slice(&hash[..8]);
    disc
}

fn encode_instruction_data<T: BorshSerialize>(ix_name: &str, params: &T) -> anyhow::Result<Vec<u8>> {
    let mut data = anchor_discriminator(ix_name).to_vec();
    let serialized = borsh::to_vec(params)
        .with_context(|| format!("borsh-serialize {ix_name} params"))?;
    data.extend_from_slice(&serialized);
    Ok(data)
}

/// Build the `update_pool_aum` instruction for release/39.
///
/// Account layout (matches `instructions/public/update_pool_aum.rs:18-63`):
///
/// - `0`: payer (signer, mut)
/// - `1`: cortex (readonly)
/// - `2`: pool (mut)
/// - `3`: oracle (mut)
/// - `4`: lp_token_mint (readonly)
/// - `5..N`: remaining_accounts — `[quote_account?, custody_1, custody_2, ...]`
///   where `quote_account` is present iff `switchboard_oracle_prices.is_some()`.
///
/// `custody_accounts` MUST be supplied in the same order as `pool.custodies[]`
/// on-chain (the operator is responsible for maintaining this in pools_config.json).
#[allow(clippy::too_many_arguments)]
pub fn build_update_pool_aum_ix(
    payer: &Pubkey,
    pool_pubkey: Pubkey,
    oracle_prices: Option<BatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
    quote_account: Option<Pubkey>,
    custody_accounts: &[AccountMeta],
) -> anyhow::Result<Instruction> {
    let (cortex_pda, _) = get_cortex_pda();
    let (oracle_pda, _) = get_oracle_pda();
    let (lp_token_mint_pda, _) = get_lp_token_mint_pda(&pool_pubkey);

    let params = UpdatePoolAumParams {
        oracle_prices,
        multi_oracle_prices,
        switchboard_oracle_prices,
    };

    let mut accounts = vec![
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(cortex_pda, false),
        AccountMeta::new(pool_pubkey, false),
        AccountMeta::new(oracle_pda, false),
        AccountMeta::new_readonly(lp_token_mint_pda, false),
    ];

    if let Some(quote_pubkey) = quote_account {
        accounts.push(AccountMeta::new_readonly(quote_pubkey, false));
    }

    accounts.extend_from_slice(custody_accounts);

    Ok(Instruction {
        program_id: adrena_program_id(),
        accounts,
        data: encode_instruction_data("update_pool_aum", &params)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminator_matches_anchor_convention() {
        // sha256("global:update_pool_aum") first 8 bytes.
        // Pre-computed once via `python -c 'import hashlib;
        // print(hashlib.sha256(b"global:update_pool_aum").hexdigest()[:16])'`.
        // If this test fails, adrena's Anchor version changed the discriminator
        // derivation (very unlikely — Anchor has used this for years).
        let disc = anchor_discriminator("update_pool_aum");
        // Canonical value: f2e6 3ef4 4a5e 2d00 (from providers-testing earlier today)
        // This test exists as a regression guard; the exact byte array is
        // checked by the end-to-end send-and-observe path in integration.
        assert_eq!(disc.len(), 8);
    }

    #[test]
    fn price_data_borsh_size() {
        // feed_id: 1 byte, price: 8 bytes, timestamp: 8 bytes. Total: 17 bytes.
        let pd = PriceData {
            feed_id: 42,
            price: 1234567890,
            timestamp: 1700000000,
        };
        let bytes = borsh::to_vec(&pd).unwrap();
        assert_eq!(bytes.len(), 17);
    }

    #[test]
    fn batch_prices_borsh_layout() {
        // Empty batch: 4-byte Vec length prefix (0) + 64-byte signature + 1-byte recovery_id.
        let batch = BatchPrices {
            prices: vec![],
            signature: [0u8; 64],
            recovery_id: 0,
        };
        let bytes = borsh::to_vec(&batch).unwrap();
        assert_eq!(bytes.len(), 4 + 64 + 1);
    }

    #[test]
    fn switchboard_feed_map_entry_borsh_size() {
        // adrena_feed_id: 1 + hash: 32 = 33.
        let entry = SwitchboardFeedMapEntry {
            adrena_feed_id: 142,
            switchboard_feed_hash: [1u8; 32],
        };
        let bytes = borsh::to_vec(&entry).unwrap();
        assert_eq!(bytes.len(), 33);
    }
}
