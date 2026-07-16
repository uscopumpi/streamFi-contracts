use soroban_sdk::{Address, Env};

use crate::storage::DataKey;
use crate::ttl;
use crate::Error;

/// Requires that the caller is the current authority, and bumps instance TTL
/// on success. Used by every authority-gated write in this contract.
pub fn require_authority(env: &Env) -> Result<(), Error> {
    let authority: Address = env
        .storage()
        .instance()
        .get(&DataKey::Authority)
        .ok_or(Error::NotAuthorized)?;
    authority.require_auth();
    ttl::bump(env);
    Ok(())
}
