use {
    adrena_abi::{self, oracle::BatchPrices},
    anyhow::Context,
    borsh::{BorshDeserialize, BorshSerialize},
    sha2::{Digest, Sha256},
    solana_sdk::{
        instruction::{AccountMeta, Instruction},
        pubkey::Pubkey,
    },
};

pub const ORACLE_PROVIDER_CHAOS_LABS: u8 = 0;
pub const ORACLE_PROVIDER_AUTONOM: u8 = 1;
pub const ORACLE_PROVIDER_SWITCHBOARD: u8 = 2;

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct BatchPricesWithProvider {
    pub provider: u8,
    pub batch: BatchPrices,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct MultiBatchPrices {
    pub batches: Vec<BatchPricesWithProvider>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct UpdatePoolAumParams {
    pub oracle_prices: Option<BatchPrices>,
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
    pub max_age_slots: u64,
    pub feed_map: Vec<SwitchboardFeedMapEntry>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct UpdateOracleParams {
    pub oracle_prices: Option<BatchPrices>,
    pub multi_oracle_prices: Option<MultiBatchPrices>,
    pub switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
}

pub fn build_update_pool_aum_ix(
    payer: &Pubkey,
    pool_pubkey: Pubkey,
    oracle_prices: Option<BatchPrices>,
    multi_oracle_prices: Option<MultiBatchPrices>,
    switchboard_oracle_prices: Option<SwitchboardUpdateParams>,
    quote_account: Option<Pubkey>,
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
        AccountMeta::new_readonly(adrena_abi::pda::get_cortex_pda().0, false),
        AccountMeta::new(pool_pubkey, false),
        AccountMeta::new(oracle_pda, false),
        AccountMeta::new_readonly(lp_token_mint_pubkey, false),
    ];

    // SwitchboardQuote canonical PDA as the first remaining_account
    if let Some(quote_pubkey) = quote_account {
        accounts.push(AccountMeta::new_readonly(quote_pubkey, false));
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
    quote_account: Option<Pubkey>,
) -> Result<Instruction, anyhow::Error> {
    let oracle_pda = adrena_abi::pda::get_oracle_pda().0;

    let params = UpdateOracleParams {
        oracle_prices: None,
        multi_oracle_prices,
        switchboard_oracle_prices: switchboard_oracle_prices.clone(),
    };

    let mut accounts = vec![
        AccountMeta::new_readonly(adrena_abi::pda::get_cortex_pda().0, false),
        AccountMeta::new(oracle_pda, false),
    ];

    // SwitchboardQuote canonical PDA as the first remaining_account
    if let Some(quote_pubkey) = quote_account {
        accounts.push(AccountMeta::new_readonly(quote_pubkey, false));
    }

    Ok(Instruction {
        program_id: adrena_abi::ID,
        accounts,
        data: encode_instruction_data("update_oracle", &params)?,
    })
}

pub fn build_update_oracle_switchboard_ix(
    quote_account: Pubkey,
    max_age_slots: u64,
    feed_map: Vec<SwitchboardFeedMapEntry>,
) -> Result<Instruction, anyhow::Error> {
    build_update_oracle_ix(
        None,
        Some(SwitchboardUpdateParams {
            max_age_slots,
            feed_map,
        }),
        Some(quote_account),
    )
}

pub fn build_update_oracle_multi_ix(
    multi_oracle_prices: MultiBatchPrices,
) -> Result<Instruction, anyhow::Error> {
    build_update_oracle_ix(Some(multi_oracle_prices), None, None)
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
