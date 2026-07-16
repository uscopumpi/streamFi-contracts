use soroban_sdk::{contracttype, Address, Env};

use crate::storage::DataKey;

#[contracttype]
#[derive(Clone)]
pub struct GovernorConfig {
    pub fee_bps: u32,
    pub fee_recipient: Address,
    pub min_duration_seconds: u64,
    pub max_rate_per_second: i128,
    pub factory_address: Address,
}

pub fn load(env: &Env) -> GovernorConfig {
    let s = env.storage().instance();
    GovernorConfig {
        fee_bps: s.get(&DataKey::FeeBps).unwrap_or(30),
        fee_recipient: s.get(&DataKey::FeeRecipient).unwrap(),
        min_duration_seconds: s.get(&DataKey::MinDurationSeconds).unwrap_or(3600),
        max_rate_per_second: s
            .get(&DataKey::MaxRatePerSecond)
            .unwrap_or(1_000_000_000_000_000),
        factory_address: s.get(&DataKey::FactoryAddress).unwrap(),
    }
}
