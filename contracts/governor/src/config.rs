use soroban_sdk::{contracttype, Address, Env};

use crate::storage::DataKey;
use crate::Error;

#[contracttype]
#[derive(Clone)]
pub struct GovernorConfig {
    pub fee_bps: u32,
    pub fee_recipient: Address,
    pub min_duration_seconds: u64,
    pub max_duration_seconds: u64,
    pub max_rate_per_second: i128,
    pub factory_address: Address,
}

/// Load the governor configuration from instance storage.
///
/// Returns `Err(NotInitialized)` when the governor has not been
/// initialised — rather than panicking on missing keys — so callers
/// (including cross-contract callers in `DripFactory`) get a
/// meaningful error instead of a generic host trap.
pub fn load(env: &Env) -> Result<GovernorConfig, Error> {
    let s = env.storage().instance();

    let fee_recipient: Address = s
        .get(&DataKey::FeeRecipient)
        .ok_or(Error::NotInitialized)?;
    let factory_address: Address = s
        .get(&DataKey::FactoryAddress)
        .ok_or(Error::NotInitialized)?;

    Ok(GovernorConfig {
        fee_bps: s.get(&DataKey::FeeBps).unwrap_or(30),
        fee_recipient,
        min_duration_seconds: s.get(&DataKey::MinDurationSeconds).unwrap_or(3600),
        max_duration_seconds: s
            .get(&DataKey::MaxDurationSeconds)
            .unwrap_or(315_360_000), // 10 years
        max_rate_per_second: s
            .get(&DataKey::MaxRatePerSecond)
            .unwrap_or(1_000_000_000_000_000),
        factory_address,
    })
}
