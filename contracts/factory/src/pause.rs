use soroban_sdk::Env;

use crate::storage::DataKey;

/// Reads the emergency-pause flag.
///
/// Defaults to `false` (unpaused) when the key has never been set — e.g. a
/// factory that was initialized before this feature existed. This keeps the
/// flag backward-compatible: an absent entry means "running normally".
pub fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

/// Writes the emergency-pause flag.
pub fn set_paused(env: &Env, paused: bool) {
    env.storage().instance().set(&DataKey::Paused, &paused);
}
