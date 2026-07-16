use soroban_sdk::Env;

// Instance storage (all of this contract's state) has a TTL like any other
// Soroban storage entry. Without extending it, an inactive stream would be
// archived and become inaccessible. Values mirror the TTL extension the
// factory already applies to its StreamAddr registry entries.
const TTL_THRESHOLD: u32 = 100_000;
const TTL_EXTEND_TO: u32 = 200_000;

pub fn bump(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD, TTL_EXTEND_TO);
}
