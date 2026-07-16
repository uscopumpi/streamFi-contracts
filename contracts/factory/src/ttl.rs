use soroban_sdk::Env;

// Mirrors the TTL extension convention used across all three contracts.
// Exposed as `pub` so callers can reuse the same threshold/extend-to values
// for the persistent BySender/ByRecipient/StreamAddr registry entries.
pub const THRESHOLD: u32 = 100_000;
pub const EXTEND_TO: u32 = 200_000;

pub fn bump_instance(env: &Env) {
    env.storage().instance().extend_ttl(THRESHOLD, EXTEND_TO);
}
