use soroban_sdk::{contracttype, Address, Env};

use crate::storage::{DataKey, RoleKey};
use crate::ttl;
use crate::Error;

/// Protocol administration roles.
///
/// Each role gates a distinct slice of governor state, so independent wallets
/// can own fee policy and rate/duration bounds without sharing a single
/// all-powerful key:
///
/// - `Admin`       — grant and revoke roles (including `Admin` itself).
/// - `FeeManager`  — `set_fee_bps`, `set_fee_recipient`.
/// - `RateManager` — `set_max_rate`, `set_min_duration`.
///
/// A role may be held by any number of accounts, and one account may hold any
/// combination of roles.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Admin,
    FeeManager,
    RateManager,
}

fn key(role: Role, account: &Address) -> DataKey {
    DataKey::Role(RoleKey {
        role,
        account: account.clone(),
    })
}

/// Whether `account` currently holds `role`.
pub fn has_role(env: &Env, role: Role, account: &Address) -> bool {
    env.storage().instance().has(&key(role, account))
}

/// Number of accounts currently holding `Role::Admin` (zero pre-initialization).
pub fn admin_count(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::AdminCount)
        .unwrap_or(0)
}

/// Grants `role` to `account`.
///
/// Idempotent: re-granting a role the account already holds is a no-op, so the
/// admin count can never be inflated by repeated grants.
pub fn grant(env: &Env, role: Role, account: &Address) {
    if has_role(env, role, account) {
        return;
    }
    env.storage().instance().set(&key(role, account), &true);
    if role == Role::Admin {
        let next = admin_count(env) + 1;
        env.storage().instance().set(&DataKey::AdminCount, &next);
    }
}

/// Revokes `role` from `account`.
///
/// Idempotent when the account doesn't hold the role. Refuses to remove the
/// final `Admin` (`LastAdmin`): a governor with zero admins could never grant
/// a new one, permanently freezing every protocol parameter.
pub fn revoke(env: &Env, role: Role, account: &Address) -> Result<(), Error> {
    if !has_role(env, role, account) {
        return Ok(());
    }
    if role == Role::Admin {
        let count = admin_count(env);
        if count <= 1 {
            return Err(Error::LastAdmin);
        }
        env.storage()
            .instance()
            .set(&DataKey::AdminCount, &(count - 1));
    }
    env.storage().instance().remove(&key(role, account));
    Ok(())
}

/// Requires that `caller` both authorized the transaction and holds `role`,
/// then bumps instance TTL. Every role-gated write funnels through here.
pub fn require_role(env: &Env, caller: &Address, role: Role) -> Result<(), Error> {
    caller.require_auth();
    if !has_role(env, role, caller) {
        return Err(Error::NotAuthorized);
    }
    ttl::bump(env);
    Ok(())
}
