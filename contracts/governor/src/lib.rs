#![no_std]

mod config;
mod errors;
mod role;
mod storage;
mod ttl;

use soroban_sdk::{contract, contractimpl, panic_with_error, Address, Env};

pub use config::GovernorConfig;
pub use errors::Error;
pub use role::Role;
use storage::DataKey;

#[contract]
pub struct DripGovernor;

#[contractimpl]
impl DripGovernor {
    /// Deploy-time initialisation.
    ///
    /// Guards against re-initialization: without this check, anyone could call
    /// `initialize` again to grant themselves `Admin`, then set `fee_bps` to
    /// the maximum or repoint `fee_recipient`.
    ///
    /// The deploy `authority` is granted every role, so a single wallet can
    /// bootstrap the protocol and later delegate fee and rate/duration
    /// management to separate wallets via [`DripGovernor::grant_role`].
    pub fn initialize(
        env: Env,
        authority: Address,
        fee_recipient: Address,
        factory_address: Address,
    ) {
        if env.storage().instance().has(&DataKey::FactoryAddress) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        ttl::bump(&env);

        let s = env.storage().instance();
        s.set(&DataKey::FeeBps, &30_u32);
        s.set(&DataKey::FeeRecipient, &fee_recipient);
        s.set(&DataKey::MinDurationSeconds, &3600_u64);
        s.set(&DataKey::MaxDurationSeconds, &315_360_000_u64); // 10 years
        s.set(&DataKey::MaxRatePerSecond, &1_000_000_000_000_000_i128);
        s.set(&DataKey::FactoryAddress, &factory_address);

        role::grant(&env, Role::Admin, &authority);
        role::grant(&env, Role::FeeManager, &authority);
        role::grant(&env, Role::RateManager, &authority);
    }

    // ── Reads ────────────────────────────────────────────────────────────

    pub fn config(env: Env) -> Result<GovernorConfig, Error> {
        config::load(&env)
    }

    /// Whether `account` currently holds `role`.
    pub fn has_role(env: Env, role: Role, account: Address) -> bool {
        role::has_role(&env, role, &account)
    }

    // ── Role administration (Admin-gated) ────────────────────────────────

    /// Grants `role` to `account`. Only an `Admin` may call this.
    pub fn grant_role(
        env: Env,
        caller: Address,
        role: Role,
        account: Address,
    ) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::Admin)?;
        role::grant(&env, role, &account);
        Ok(())
    }

    /// Revokes `role` from `account`. Only an `Admin` may call this.
    ///
    /// Rejected with `LastAdmin` if it would remove the final `Admin`.
    pub fn revoke_role(
        env: Env,
        caller: Address,
        role: Role,
        account: Address,
    ) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::Admin)?;
        role::revoke(&env, role, &account)
    }

    /// Hands the full `Admin` role from `caller` to `new_authority`.
    ///
    /// Grants first so the subsequent revoke can never trip the `LastAdmin`
    /// guard, then revokes `caller`. Kept for API familiarity — equivalent to
    /// a `grant_role(Admin, new)` followed by `revoke_role(Admin, caller)`.
    pub fn transfer_authority(
        env: Env,
        caller: Address,
        new_authority: Address,
    ) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::Admin)?;
        role::grant(&env, Role::Admin, &new_authority);
        role::revoke(&env, Role::Admin, &caller)
    }

    // ── Parameter writes (role-gated) ────────────────────────────────────

    pub fn set_fee_bps(env: Env, caller: Address, fee_bps: u32) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::FeeManager)?;
        if fee_bps > 10_000 {
            return Err(Error::InvalidParam);
        }
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        Ok(())
    }

    pub fn set_fee_recipient(env: Env, caller: Address, recipient: Address) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::FeeManager)?;
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &recipient);
        Ok(())
    }

    pub fn set_min_duration(env: Env, caller: Address, seconds: u64) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::RateManager)?;
        if seconds == 0 {
            return Err(Error::InvalidParam);
        }
        // Cross-check against current `MaxRatePerSecond`: the product
        // `max_rate * min_duration` is the upper bound on stream principal a
        // caller may commit, and capacity math (in `DripFactory::create_stream`)
        // relies on it fitting in a single `i128` (which has
        // ~10^38 capacity). Reject up-front instead of letting valid-looking
        // parameters fail at create_stream with `ArithmeticOverflow`.
        let max_rate: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxRatePerSecond)
            .unwrap_or(1_000_000_000_000_000);
        let secs_i: i128 = seconds as i128;
        max_rate.checked_mul(secs_i).ok_or(Error::InvalidParam)?;
        env.storage()
            .instance()
            .set(&DataKey::MinDurationSeconds, &seconds);
        Ok(())
    }

    pub fn set_max_rate(env: Env, caller: Address, max_rate: i128) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::RateManager)?;
        if max_rate <= 0 {
            return Err(Error::InvalidParam);
        }
        // Mirror cross-check on the `min_duration_seconds` side (see
        // `set_min_duration`). Both setters read the counterpart from storage,
        // so whichever order the two settings arrive in is safe.
        let min_duration: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MinDurationSeconds)
            .unwrap_or(3_600);
        let secs_i: i128 = min_duration as i128;
        max_rate.checked_mul(secs_i).ok_or(Error::InvalidParam)?;
        env.storage()
            .instance()
            .set(&DataKey::MaxRatePerSecond, &max_rate);
        Ok(())
    }
    pub fn set_max_duration(env: Env, caller: Address, seconds: u64) -> Result<(), Error> {
        role::require_role(&env, &caller, Role::RateManager)?;
        if seconds == 0 {
            return Err(Error::InvalidParam);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxDurationSeconds, &seconds);
        Ok(())
    }
}
