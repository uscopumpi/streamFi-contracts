#![no_std]

mod auth;
mod config;
mod errors;
mod storage;
mod ttl;

use soroban_sdk::{contract, contractimpl, panic_with_error, Address, Env};

pub use config::GovernorConfig;
pub use errors::Error;
use storage::DataKey;

#[contract]
pub struct DripGovernor;

#[contractimpl]
impl DripGovernor {
    /// Deploy-time initialisation.
    ///
    /// Guards against re-initialization: without this check, anyone could
    /// call `initialize` again to overwrite `Authority` with their own
    /// address, then set `fee_bps` to the maximum or repoint `fee_recipient`.
    pub fn initialize(
        env: Env,
        authority: Address,
        fee_recipient: Address,
        factory_address: Address,
    ) {
        if env.storage().instance().has(&DataKey::Authority) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        ttl::bump(&env);

        let s = env.storage().instance();
        s.set(&DataKey::Authority, &authority);
        s.set(&DataKey::FeeBps, &30_u32);
        s.set(&DataKey::FeeRecipient, &fee_recipient);
        s.set(&DataKey::MinDurationSeconds, &3600_u64);
        s.set(&DataKey::MaxRatePerSecond, &1_000_000_000_000_000_i128);
        s.set(&DataKey::FactoryAddress, &factory_address);
    }

    // ── Reads ────────────────────────────────────────────────────────────

    pub fn config(env: Env) -> GovernorConfig {
        config::load(&env)
    }

    // ── Writes (authority-gated) ─────────────────────────────────────────

    pub fn set_fee_bps(env: Env, fee_bps: u32) -> Result<(), Error> {
        auth::require_authority(&env)?;
        if fee_bps > 10_000 {
            return Err(Error::InvalidParam);
        }
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        Ok(())
    }

    pub fn set_fee_recipient(env: Env, recipient: Address) -> Result<(), Error> {
        auth::require_authority(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &recipient);
        Ok(())
    }

    pub fn set_min_duration(env: Env, seconds: u64) -> Result<(), Error> {
        auth::require_authority(&env)?;
        if seconds == 0 {
            return Err(Error::InvalidParam);
        }
        env.storage()
            .instance()
            .set(&DataKey::MinDurationSeconds, &seconds);
        Ok(())
    }

    pub fn set_max_rate(env: Env, max_rate: i128) -> Result<(), Error> {
        auth::require_authority(&env)?;
        if max_rate <= 0 {
            return Err(Error::InvalidParam);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxRatePerSecond, &max_rate);
        Ok(())
    }

    pub fn transfer_authority(env: Env, new_authority: Address) -> Result<(), Error> {
        auth::require_authority(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Authority, &new_authority);
        Ok(())
    }
}
