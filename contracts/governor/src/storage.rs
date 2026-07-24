use soroban_sdk::{contracttype, Address};

use crate::role::Role;

/// Composite key identifying a single (role, account) grant.
///
/// Soroban `contracttype` enum variants carry at most one payload, so the
/// (role, account) pair is wrapped in this struct rather than expressed as a
/// two-field `DataKey` variant.
#[contracttype]
#[derive(Clone)]
pub struct RoleKey {
    pub role: Role,
    pub account: Address,
}

#[contracttype]
pub enum DataKey {
    /// Fee in basis points (e.g. 30 = 0.3%)
    FeeBps,
    /// Address that receives protocol fees
    FeeRecipient,
    /// Minimum allowed stream duration in seconds
    MinDurationSeconds,
    /// Maximum rate per second in stroops
    MaxRatePerSecond,
    /// Maximum allowed stream duration in seconds
    MaxDurationSeconds,
    /// The DripFactory contract this governor controls
    FactoryAddress,
    /// Presence marks that `account` holds `role`. The stored value is an
    /// unused `bool`; membership is expressed entirely by the key existing.
    Role(RoleKey),
    /// Number of accounts currently holding `Role::Admin`. Tracked so the last
    /// admin can never be revoked, which would freeze governance permanently.
    AdminCount,
}
