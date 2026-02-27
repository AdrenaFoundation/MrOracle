use {
    adrena_abi::{self, oracle::ChaosLabsBatchPrices, CORTEX_ID},
    anyhow::Context,
    borsh::{BorshDeserialize, BorshSerialize},
    sha2::{Digest, Sha256},
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
        sysvar::instructions as instructions_sysvar,
    },
};

pub const ORACLE_PROVIDER_CHAOS_LABS: u8 = 0;
pub const ORACLE_PROVIDER_AUTONOM: u8 = 1;
pub const ORACLE_PROVIDER_SWITCHBOARD: u8 = 2;

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct BatchPricesWithProvider {
    pub provider: u8,
    pub batch: ChaosLabsBatchPrices,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct MultiBatchPrices {
    pub batches: Vec<BatchPricesWithProvider>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct UpdatePoolAumParams {
    pub oracle_prices: Option<ChaosLabsBatchPrices>,
    pub multi_oracle_prices: Option<MultiBatchPrices>,
    pub switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct SwitchboardFeedMapEntry {
    pub adrena_feed_id: u8,
    pub switchboard_feed_hash: [u8; 32],
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct SwitchboardUpdateParams {
    pub submit_instruction_index: u8,
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct UpdateOracleParams {
    pub oracle_prices: Option<ChaosLabsBatchPrices>,
    pub multi_oracle_prices: Option<MultiBatchPrices>,
    pub switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
}

pub fn build_update_pool_aum_ix(
    payer: &Pubkey,
    pool_pubkey: Pubkey,
    oracle_prices: Option<ChaosLabsBatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
    pull_feed_pubkeys: &[Pubkey],
    custody_accounts: &[AccountMeta],
) -> Result<Instruction, anyhow::Error> {
    let oracle_pda = adrena_abi::pda::get_oracle_pda().0;
    let lp_token_mint_pubkey = adrena_abi::pda::get_lp_token_mint_pda(&pool_pubkey).0;

    let params = UpdatePoolAumParams {
        oracle_prices,
        multi_oracle_prices,
        switchboard_oracle_prices: switchboard_oracle_prices.clone(),
    };

    let mut accounts = vec![
        AccountMeta::new(*payer, true),
        AccountMeta::new_readonly(CORTEX_ID, false),
        AccountMeta::new(pool_pubkey, false),
        AccountMeta::new(oracle_pda, false),
        AccountMeta::new_readonly(lp_token_mint_pubkey, false),
    ];

    if switchboard_oracle_prices.is_some() {
        accounts.push(AccountMeta::new_readonly(instructions_sysvar::ID, false));
        for pull_feed_pubkey in pull_feed_pubkeys {
            accounts.push(AccountMeta::new_readonly(*pull_feed_pubkey, false));
        }
    }

    accounts.extend_from_slice(custody_accounts);

    Ok(Instruction {
        program_id: adrena_abi::ID,
        accounts,
        data: encode_instruction_data("update_pool_aum", &params)?,
    })
}

pub fn build_update_oracle_ix(
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
    pull_feed_pubkeys: &[Pubkey],
) -> Result<Instruction, anyhow::Error> {
    let oracle_pda = adrena_abi::pda::get_oracle_pda().0;

    let params = UpdateOracleParams {
        oracle_prices: None,
        multi_oracle_prices,
        switchboard_oracle_prices: switchboard_oracle_prices.clone(),
    };

    let mut accounts = vec![
        AccountMeta::new_readonly(CORTEX_ID, false),
        AccountMeta::new(oracle_pda, false),
    ];

    if switchboard_oracle_prices.is_some() {
        accounts.push(AccountMeta::new_readonly(instructions_sysvar::ID, false));
        for pull_feed_pubkey in pull_feed_pubkeys {
            accounts.push(AccountMeta::new_readonly(*pull_feed_pubkey, false));
        }
    }

    Ok(Instruction {
        program_id: adrena_abi::ID,
        accounts,
        data: encode_instruction_data("update_oracle", &params)?,
    })
}

pub fn build_update_oracle_switchboard_ix(
    pull_feed_pubkeys: &[Pubkey],
    submit_instruction_index: u8,
    max_age_slots: u64,
    feed_map: Vec<SwitchboardFeedMapEntry>,
) -> Result<Instruction, anyhow::Error> {
    build_update_oracle_ix(
        None,
        Some(SwitchboardUpdateParams {
            submit_instruction_index,
            max_age_slots,
            feed_map,
        }),
        pull_feed_pubkeys,
    )
}

pub fn build_update_oracle_multi_ix(
    multi_oracle_prices: MultiBatchPrices,
) -> Result<Instruction, anyhow::Error> {
    build_update_oracle_ix(Some(multi_oracle_prices), None, &[])
}

fn encode_instruction_data<T: BorshSerialize>(
    instruction_name: &str,
    params: &T,
) -> Result<Vec<u8>, anyhow::Error> {
    let mut data = instruction_discriminator(instruction_name).to_vec();
    let mut serialized = borsh::to_vec(params).with_context(|| {
        format!("failed to serialize params for instruction `{instruction_name}`")
    })?;
    data.append(&mut serialized);
    Ok(data)
}

fn instruction_discriminator(instruction_name: &str) -> [u8; 8] {
    let hash = Sha256::digest(format!("global:{instruction_name}").as_bytes());
    let mut discriminator = [0u8; 8];
    discriminator.copy_from_slice(&hash[..8]);
    discriminator
}
