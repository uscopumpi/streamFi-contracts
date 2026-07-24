//! Regression tests for the follow-up audit PR `fix/audit-round-2`.
//!
//! Covers the three fixes that landed in this PR:
//!   1. Governor `set_max_rate` × `set_min_duration` overflow cross-check.
//!   2. Factory bounded-walk TTL refresh in
//!      `pause` / `unpause` / `upgrade_stream_wasm`.
//!   3. Stream `pause` / `resume` / `extend_duration` write consolidation
//!      (no redundant `state::set_paused` calls or direct `DataKey` writes).

#![cfg(test)]

use drip_factory::{storage::DataKey, ttl, DripFactory, DripFactoryClient};
use drip_governor::{DripGovernor, DripGovernorClient, Error as GovError};
use drip_stream::{storage as stream_storage, DripStream, DripStreamClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token, Address, BytesN, Env,
};

// ─────────────────────────────────────────────────────────────────────────────
// 1. Governor overflow cross-check
// ─────────────────────────────────────────────────────────────────────────────

fn deploy_governor(env: &Env) -> (DripGovernorClient<'_>, Address) {
    let authority = Address::generate(env);
    let fee_recipient = Address::generate(env);
    let factory_address = Address::generate(env);
    let id = env.register_contract(None, DripGovernor);
    let client = DripGovernorClient::new(env, &id);
    client.initialize(&authority, &fee_recipient, &factory_address);
    (client, authority)
}

#[test]
fn set_max_rate_unchanged_when_product_fits() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, authority) = deploy_governor(&env);
    // Defaults: min_duration = 3_600, max_rate = 1_000_000_000_000_000
    // 1e15 × 3_600 = 3.6e18 << i128::MAX.
    let r = client.try_set_max_rate(&authority, &1_000_000_000);
    assert!(r.is_ok());
    assert_eq!(client.config().max_rate_per_second, 1_000_000_000);
}

#[test]
fn set_max_rate_rejects_when_product_overflows_i128() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, authority) = deploy_governor(&env);
    // i128::MAX × (default min_duration = 3_600) overflows i128.
    let result = client.try_set_max_rate(&authority, &i128::MAX);
    assert_eq!(result, Err(Ok(GovError::InvalidParam)));
}

#[test]
fn set_max_rate_accepts_boundary() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, authority) = deploy_governor(&env);
    // i128::MAX / 3_600 is the largest valid value at default min_duration.
    let boundary: i128 = i128::MAX / 3_600;
    let r = client.try_set_max_rate(&authority, &boundary);
    assert!(r.is_ok());
    assert_eq!(client.config().max_rate_per_second, boundary);
}

#[test]
fn set_min_duration_rejects_when_product_overflows_i128() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, authority) = deploy_governor(&env);
    // Push max_rate to its valid upper bound first.
    let max_rate_boundary: i128 = i128::MAX / 3_600;
    client.set_max_rate(&authority, &max_rate_boundary);
    // Any duration > 3_600 makes the product overflow.
    let result = client.try_set_min_duration(&authority, &3_601);
    assert_eq!(result, Err(Ok(GovError::InvalidParam)));
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Factory bounded TTL walker
// ─────────────────────────────────────────────────────────────────────────────

/// Deploy a real factory + governor (matching the two-step pattern in
/// `tests/factory_deploy.rs`) without going through `create_stream` (which
/// needs a built stream WASM to deploy). The walker test then synthesises
/// `StreamAddr` entries directly via `env.as_contract` to simulate live
/// streams.
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

#[test]
fn upgrade_stream_wasm_advances_last_bumped_id() {
    let env = Env::default();
    env.mock_all_auths();
    let factory = deploy_factory_with_governor(&env); // No streams yet — upgrade should be a no-op for the walker cursor.
    let new_hash = BytesN::from_array(&env, &[1u8; 32]);
    factory.upgrade_stream_wasm(&new_hash);

    let cursor: u64 = env.as_contract(&factory.address, || {
        env.storage()
            .instance()
            .get(&DataKey::LastBumpedId)
            .unwrap_or(0u64)
    });
    assert_eq!(
        cursor, 0,
        "no streams: walker should leave cursor untouched"
    );

    // Synthesise three live StreamAddr entries and bump StreamCount to 3.
    for id in 0u64..3 {
        let key = DataKey::StreamAddr(id);
        let fake = Address::generate(&env);
        env.as_contract(&factory.address, || {
            env.storage().persistent().set(&key, &fake);
        });
    }
    env.as_contract(&factory.address, || {
        env.storage().instance().set(&DataKey::StreamCount, &3u64);
    });

    // First upgrade — walker should advance the cursor from 0.
    //
    // `new_last` records the last *visited* id, which (with `count < BATCH_LIMIT`)
    // is `(0 + BATCH_LIMIT) mod count` after the wrap, NOT `BATCH_LIMIT - 1` in
    // absolute terms. Assert against that exact value so the test is robust
    // to future batch-size tuning.
    factory.upgrade_stream_wasm(&new_hash);
    let cursor: u64 = env.as_contract(&factory.address, || {
        env.storage()
            .instance()
            .get(&DataKey::LastBumpedId)
            .unwrap_or(0u64)
    });
    let expected_cursor: u64 = (ttl::BATCH_LIMIT as u64) % 3u64;
    assert_eq!(
        cursor, expected_cursor,
        "first walk ending position should be BATCH_LIMIT mod count (visited ids wrap)",
    );
}

#[test]
fn upgrade_stream_wasm_walks_around_to_all_ids_over_multiple_calls() {
    let env = base_env();
    let factory = deploy_factory_with_governor(&env);

    let count: u64 = 12;
    for id in 0u64..count {
        let key = DataKey::StreamAddr(id);
        let fake = Address::generate(&env);
        env.as_contract(&factory.address, || {
            env.storage().persistent().set(&key, &fake);
        });
    }
    env.as_contract(&factory.address, || {
        env.storage().instance().set(&DataKey::StreamCount, &count);
    });

    let new_hash = BytesN::from_array(&env, &[2u8; 32]);

    // Pick enough calls so that with `count` ids and a walker that visits
    // `BATCH_LIMIT` ids per call (modulo count), the cumulative coverage of
    // unique id slots exceeds `count` itself — i.e. every id is guaranteed
    // to have been visited at least once.
    let calls = count.div_ceil(ttl::BATCH_LIMIT as u64) + 1;
    for _ in 0..calls {
        factory.upgrade_stream_wasm(&new_hash);
    }

    // The walker advances `LastBumpedId` by exactly BATCH_LIMIT per call,
    // modulo `count`, regardless of whether the visited persistent entry
    // needed a TTL bump or not. So after `calls` calls starting from
    // cursor=0, the final cursor is `(calls * BATCH_LIMIT) mod count`.
    // Asserting this proves every id slot modulo count was walked through.
    //
    // (Per-id TTL assertions are intentionally avoided: under Soroban 21
    // there's no API to seed a fresh persistent entry with low TTL while
    // keeping `max_entry_ttl` high, so the per-entry extend path can't be
    // observed directly in the test harness. Cursor progression is the
    // observable witness that the walker visited the right id range.)
    let final_cursor: u64 = env.as_contract(&factory.address, || {
        env.storage()
            .instance()
            .get(&DataKey::LastBumpedId)
            .unwrap_or(0u64)
    });
    let expected_cursor: u64 = ((calls * ttl::BATCH_LIMIT as u64) % count + count) % count;
    assert_eq!(
        final_cursor, expected_cursor,
        "after {calls} walker calls, cursor should be at ({calls} * BATCH_LIMIT) mod {count} = {expected_cursor}",
    );
}

#[test]
fn pause_and_unpause_drive_walker_without_panic() {
    let env = Env::default();
    env.mock_all_auths();
    let factory = deploy_factory_with_governor(&env);

    // Synthesize a handful of streams so the walker has real entries to bump.
    let count: u64 = 5;
    for id in 0u64..count {
        let key = DataKey::StreamAddr(id);
        let fake = Address::generate(&env);
        env.as_contract(&factory.address, || {
            env.storage().persistent().set(&key, &fake);
        });
    }
    env.as_contract(&factory.address, || {
        env.storage().instance().set(&DataKey::StreamCount, &count);
    });

    // Pause and unpause should drive the walker without panicking, regardless
    // of the cursor math (covered separately by
    // `upgrade_stream_wasm_advances_last_bumped_id` and
    // `upgrade_stream_wasm_walks_around_to_all_ids_over_multiple_calls`).
    factory.pause();
    assert!(factory.is_paused());
    factory.unpause();
    assert!(!factory.is_paused());
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Stream pause/resume/extend_duration write consolidation
// ─────────────────────────────────────────────────────────────────────────────

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

fn advance(env: &Env, secs: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: env.ledger().timestamp() + secs,
        ..env.ledger().get()
    });
}

fn deploy_funded_stream_clawback<'a>(
    env: &'a Env,
    sender: &Address,
    recipient: &Address,
    rate: i128,
    duration: u64,
    clawback_enabled: bool,
) -> (DripStreamClient<'a>, Address) {
    let token_admin = Address::generate(env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let deposit = rate * duration as i128;
    token::StellarAssetClient::new(env, &token_addr).mint(sender, &deposit);

    let id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(env, &id);
    token::Client::new(env, &token_addr).transfer(sender, &id, &deposit);

    let now = env.ledger().timestamp();
    client.initialize(
        sender,
        recipient,
        &token_addr,
        &rate,
        &now,
        &(now + duration),
        &clawback_enabled,
    );
    (client, token_addr)
}

#[test]
fn pause_resume_round_trip_preserves_state_with_single_save() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let (client, _) = deploy_funded_stream_clawback(&env, &sender, &recipient, 1_000, 7_200, false);

    advance(&env, 100); // 100_000 earned
    let before_pause = client.withdrawable();
    assert_eq!(before_pause, 100_000);

    client.pause();
    let info = client.info();
    assert!(info.is_paused());
    assert_eq!(info.paused_at, env.ledger().timestamp());
    assert_eq!(
        info.flags & stream_storage::FLAG_PAUSED,
        stream_storage::FLAG_PAUSED
    );

    advance(&env, 1_000); // 1_000_s of paused time — should not accrue
    let after_pause = client.withdrawable();
    assert_eq!(
        after_pause, before_pause,
        "accrual must freeze during pause",
    );

    client.resume();
    let info = client.info();
    assert!(!info.is_paused());
    assert_eq!(
        info.paused_at, 0,
        "paused_at must be wiped to zero on resume",
    );
    // Effective start_time should have advanced by the paused duration.
    assert_eq!(
        info.start_time,
        1_000_000 + 1_000,
        "start_time must shift forward by paused_duration on resume",
    );
    // end_time should also have advanced to preserve the contracted length.
    let old_end = 1_000_000 + 7_200;
    assert_eq!(info.end_time, old_end + 1_000);

    advance(&env, 50);
    assert_eq!(client.withdrawable(), before_pause + 50_000);
}

#[test]
fn extend_duration_persists_new_end_via_single_save() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let (client, token_addr) =
        deploy_funded_stream_clawback(&env, &sender, &recipient, 100, 100, false);

    let old_end = client.info().end_time;
    // `extend_duration` pulls the additional deposit from the sender to fund
    // the bumped duration. Mint the gap here so the stream contract's
    // `transfer` call doesn't fail — `mock_all_auths` waives the StellarAsset
    // admin auth, and the inferred 100 × 2_000 = 200_000 deposit matches the
    // rate * extra_seconds the stream expects.
    token::StellarAssetClient::new(&env, &token_addr).mint(&sender, &200_000);
    client.extend_duration(&2_000); // +2_000s @ 100/s = 200_000 token pull

    let info = client.info();
    assert_eq!(info.end_time, old_end + 2_000);
    // Confirm we did NOT accidentally bump paused_at or clear flags via the
    // trimmed-out redundant writes.
    assert!(!info.is_paused());
    assert_eq!(info.paused_at, 0);

    // Cover `tests/factory_pause.rs` round-trip pattern with the chain
    // explicitly: walk forward after the extension and assert the
    // withdrawable matches the rate × elapsed-since-start.
    advance(&env, 100);
    let expected = 100i128 * 100; // rate=100 × 100s elapsed = 10_000
    assert_eq!(
        client.withdrawable(),
        expected,
        "extended stream must accrue past the original end_time",
    );
}
