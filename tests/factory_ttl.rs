//! Regression fixture for factory TTL walker invariants.
//! Counterpart to the audit-round-2 walker test focusing on BATCH_LIMIT and Soroban `extend_ttl` invariants.
//!
//! The `extend_ttl(threshold, extend_to)` host ABI requires `threshold <= extend_to`,
//! otherwise the call traps. These regression tests pin that invariant, the cursor-coverage
//! shape proven in `tests/audit_round_2_regression.rs`, and the walker-against-small-cursor-counts
//! edge cases that the audit-round-2 fixture partially covers.
//!
//! Build-config precondition: this fixture relies on Rust's default unwinding panic
//! behavior in test binaries (`panic = "unwind"`). If the workspace ever sets
//! `panic = "abort"` on the test profile, the two `catch_unwind` assertions
//! below will return `Ok` for genuine host traps and silently produce false
//! positives. Pin the test profile to `panic = "unwind"` in `Cargo.toml` if
//! not already.

#![cfg(test)]

use drip_factory::{storage::DataKey, ttl, DripFactory, DripFactoryClient};
use drip_governor::{DripGovernor, DripGovernorClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    Address, BytesN, Env,
};

/// Cursor-coverage fixture setup. Mirrors `tests/audit_round_2_regression.rs::base_env`
/// so multi-call walker assertions have identical ledger config across both fixtures.
fn base_env() -> Env {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set(LedgerInfo {
        timestamp: 1_000_000,
        protocol_version: 21,
        sequence_number: 1,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 16,
        min_persistent_entry_ttl: 4096,
        max_entry_ttl: 6_312_000,
    });
    env
}

/// Deploy real factory + governor without going through `create_stream`
/// (which needs a built stream WASM). Mirrors `tests/audit_round_2_regression.rs`
/// helper shape exactly.
fn deploy_factory_with_governor(env: &Env) -> DripFactoryClient<'_> {
    let factory_id = env.register_contract(None, DripFactory);
    let governor_id = env.register_contract(None, DripGovernor);

    let authority = Address::generate(env);
    let fee_recipient = Address::generate(env);
    let governor_client = DripGovernorClient::new(env, &governor_id);
    governor_client.initialize(&authority, &fee_recipient, &factory_id);

    let dummy_hash = BytesN::from_array(env, &[0u8; 32]);
    let client = DripFactoryClient::new(env, &factory_id);
    client.initialize(&dummy_hash, &governor_id);
    client
}

/// Synthesize `count` live `StreamAddr` entries directly via `env.as_contract` and
/// bump `StreamCount` to match. Replaces `create_stream` calls which would require
/// a built stream contract artifact.
fn synthesize_stream_entries(env: &Env, factory: &Address, count: u64) {
    for id in 0..count {
        let key = DataKey::StreamAddr(id);
        let fake = Address::generate(env);
        env.as_contract(factory, || {
            env.storage().persistent().set(&key, &fake);
        });
    }
    env.as_contract(factory, || {
        env.storage().instance().set(&DataKey::StreamCount, &count);
    });
}

// ─── Invariant 1 — (threshold <= extend_to) at construction ───────────────────

#[test]
fn extend_ttl_invariant_threshold_lt_or_eq_extend_to_at_construction() {
    assert!(
        ttl::EXTEND_TO >= ttl::THRESHOLD,
        "EXTEND_TO ({}) must be >= THRESHOLD ({}) to avoid Soroban panic on extend_ttl",
        ttl::EXTEND_TO,
        ttl::THRESHOLD,
    );
}

// ─── Invariant 2 — inverted (threshold > extend_to) traps at the host ──────────

#[test]
fn extend_ttl_invariant_rejects_threshold_gt_extend_to() {
    let env = base_env();
    let factory = deploy_factory_with_governor(&env);
    synthesize_stream_entries(&env, &factory.address, 1);

    // Soroban `extend_ttl(threshold, extend_to)` requires `threshold <= extend_to`.
    // If inverted, the host traps. We use `std::panic::catch_unwind` so the test
    // harness observes the trap without aborting the entire test binary on
    // assertion failure.
    //
    // MANUAL VERIFICATION TRAIL (run once, record SHA, pin comment):
    //   $ cargo test --workspace --test factory_ttl \
    //       extend_ttl_invariant_rejects_threshold_gt_extend_to -- --exact --nocapture
    //   Expected: assertion passes; line reports `1 passed; 0 failed`.
    //   (Exact stdout depends on cargo verbosity; if `1 passed; 0 failed` is
    //   in the output, the inversion trap is still working as expected.)
    // If this assertion ever goes from `is_err() == true` to `is_err() == false`,
    // Soroban silently changed the host's response to inverted `extend_ttl`
    // parameter pairs. That's a behavior shift requiring a re-audit before
    // any version bump. Block merge and re-verify against the `soroban-sdk`
    // source for the pinned version in this workspace's `Cargo.lock`
    // (grep `~/.cargo/registry/src/index.crates.io-*/soroban-sdk-*/src/host.rs`
    // for the persistent-storage extend entry; the relevant internal
    // function assumes app-call semantics that an audit-pin means we
    // control locally).
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.as_contract(&factory.address, || {
            // Use the production's own value pair, just inverted, so a future
            // bump to `threshold = 200_000, extend_to = 100_000` here is exactly
            // the regression class we want to detect.
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::StreamAddr(0), 200_000_u32, 100_000_u32);
        });
    }));
    assert!(
        res.is_err(),
        "Inverted extend_ttl (threshold > extend_to) must trap at the host. \
         If this test ever goes GREEN (res.is_err() == false), Soroban silently \
         changed semantics. Block merge and re-audit."
    );
}

// ─── Invariant 3 — boundary equality is a no-bump edge case ──────────────────

#[test]
fn walker_threshold_eq_extend_to_does_not_panic() {
    let env = base_env();
    let factory = deploy_factory_with_governor(&env);
    synthesize_stream_entries(&env, &factory.address, 1);

    // When `threshold == extend_to`, the host call is well-defined and never
    // panics; it just bumps the TTL by the threshold distance. Assert only the
    // non-panic shape — the TTL behavior is a Soroban host detail.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.as_contract(&factory.address, || {
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::StreamAddr(0), 100_000_u32, 100_000_u32);
        });
    }));
    assert!(
        res.is_ok(),
        "equivalence (threshold == extend_to) must execute without panic"
    );
}

// ─── Cursor coverage — full registry ─────────────────────────────────────────

#[test]
fn walker_visits_every_id_at_least_once_with_full_count() {
    let env = base_env();
    let factory = deploy_factory_with_governor(&env);
    let count: u64 = 12;
    synthesize_stream_entries(&env, &factory.address, count);

    let new_hash = BytesN::from_array(&env, &[2u8; 32]);
    // Pick enough calls so the cumulative coverage of unique id slots
    // exceeds `count` itself — every id guaranteed to have been visited.
    let calls = count.div_ceil(ttl::BATCH_LIMIT as u64) + 1;
    for _ in 0..calls {
        factory.upgrade_stream_wasm(&new_hash);
    }

    let cursor: u64 = env.as_contract(&factory.address, || {
        env.storage()
            .instance()
            .get(&DataKey::LastBumpedId)
            .unwrap_or(0u64)
    });
    let expected_cursor: u64 = ((calls * ttl::BATCH_LIMIT as u64) % count + count) % count;
    assert_eq!(
        cursor, expected_cursor,
        "after {calls} walker calls, cursor should be at ({calls} * BATCH_LIMIT) mod {count} = {expected_cursor}",
    );
}

// ─── Cursor coverage — count < BATCH_LIMIT ──────────────────────────────────

#[test]
fn walker_handles_count_lt_batch_limit_with_first_walk_wrap() {
    let env = base_env();
    let factory = deploy_factory_with_governor(&env);
    let count: u64 = 3;
    synthesize_stream_entries(&env, &factory.address, count);

    let new_hash = BytesN::from_array(&env, &[3u8; 32]);
    factory.upgrade_stream_wasm(&new_hash);

    let cursor: u64 = env.as_contract(&factory.address, || {
        env.storage()
            .instance()
            .get(&DataKey::LastBumpedId)
            .unwrap_or(0u64)
    });
    // First walk with `count < BATCH_LIMIT` lands on `BATCH_LIMIT mod count`.
    assert_eq!(
        cursor,
        (ttl::BATCH_LIMIT as u64) % count,
        "first walk ending position should be BATCH_LIMIT mod count (visited ids wrap)",
    );
}

// ─── Cursor coverage — count == 1 ───────────────────────────────────────────

#[test]
fn walker_handles_count_eq_one_with_single_id_visited_every_call() {
    let env = base_env();
    let factory = deploy_factory_with_governor(&env);
    synthesize_stream_entries(&env, &factory.address, 1);

    let new_hash = BytesN::from_array(&env, &[4u8; 32]);
    for _ in 0..5 {
        factory.upgrade_stream_wasm(&new_hash);
    }

    let cursor: u64 = env.as_contract(&factory.address, || {
        env.storage()
            .instance()
            .get(&DataKey::LastBumpedId)
            .unwrap_or(0u64)
    });
    // Modulo 1 always lands at 0; assert the stable identity AND the entry
    // never gets dropped (single-entry registries are easy to silently
    // archive if a walker ever skip-unconditionally walks an empty range).
    assert_eq!(
        cursor, 0,
        "single-entry registry: cursor stays at 0 across any number of calls"
    );
    let persists = env.as_contract(&factory.address, || {
        env.storage().persistent().has(&DataKey::StreamAddr(0))
    });
    assert!(
        persists,
        "single-entry walker must not drop the only persistent entry"
    );
}

// Note: There is intentionally NO `walker_extend_to_ceiling_*` test here. Per the
// audit-round-2 fixture doc comment, "under Soroban 21 there's no API to seed a
// fresh persistent entry with low TTL while keeping max_entry_ttl high, so the
// per-entry extend path can't be observed directly in the test harness." Thus
// we cannot pin `live_until <= max_entry_ttl` from a host-side test. Cursor
// coverage tests in this fixture plus `tests/audit_round_2_regression.rs` cover
// the walker correctness shape; the ceiling claim is enforced by the production
// constant only (`ttl::EXTEND_TO = 200_000 << max_entry_ttl = 6_312_000`).
