//! Integration tests: DripFactory validation and registry queries.
//!
//! Deployment tests (create_stream success path) require the stream WASM to be
//! built first (`cargo build --target wasm32-unknown-unknown --release`).
//! The tests in this file cover validation guards and registry read-paths that
//! do not require an actual stream deployment.

use drip_factory::{DripFactory, DripFactoryClient, Error};
use drip_governor::{DripGovernor, DripGovernorClient};
use soroban_sdk::{
    testutils::{storage::Instance as _, Address as _, Ledger, LedgerInfo},
    token, Address, BytesN, Env,
};

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

/// Deploys a real DripFactory wired to a real, initialized DripGovernor —
/// matching the two-step deploy order in docs/architecture.md (both
/// contracts are registered first, then each is initialized with the
/// other's address). create_stream reads governor config, so a dummy
/// governor address is no longer sufficient here.
fn deploy_factory(env: &Env) -> DripFactoryClient<'_> {
    let factory_id = env.register_contract(None, DripFactory);
    let governor_id = env.register_contract(None, DripGovernor);

    let authority = Address::generate(env);
    let fee_recipient = Address::generate(env);
    let governor_client = DripGovernorClient::new(env, &governor_id);
    governor_client.initialize(&authority, &fee_recipient, &factory_id);

    let client = DripFactoryClient::new(env, &factory_id);
    let dummy_hash = BytesN::from_array(env, &[0u8; 32]);
    client.initialize(&dummy_hash, &governor_id);
    client
}

/// Same as `deploy_factory`, but also hands back the governor client so
/// tests can adjust `max_rate_per_second` / `min_duration_seconds` and
/// observe the factory enforcing them.
fn deploy_factory_with_governor<'a>(
    env: &'a Env,
) -> (DripFactoryClient<'a>, DripGovernorClient<'a>) {
    let factory_id = env.register_contract(None, DripFactory);
    let governor_id = env.register_contract(None, DripGovernor);

    let authority = Address::generate(env);
    let fee_recipient = Address::generate(env);
    let governor_client = DripGovernorClient::new(env, &governor_id);
    governor_client.initialize(&authority, &fee_recipient, &factory_id);

    let factory_client = DripFactoryClient::new(env, &factory_id);
    let dummy_hash = BytesN::from_array(env, &[0u8; 32]);
    factory_client.initialize(&dummy_hash, &governor_id);

    (factory_client, governor_client)
}

// ── Fresh factory state ───────────────────────────────────────────────────────

#[test]
fn stream_count_starts_at_zero() {
    let env = base_env();
    let client = deploy_factory(&env);
    assert_eq!(client.stream_count(), 0);
}

#[test]
fn stream_address_returns_none_for_nonexistent_id() {
    let env = base_env();
    let client = deploy_factory(&env);
    assert!(client.stream_address(&0).is_none());
    assert!(client.stream_address(&999).is_none());
}

#[test]
fn streams_by_sender_returns_empty_for_unknown_address() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let result = client.streams_by_sender(&sender, &0, &10);
    assert_eq!(result.len(), 0);
}

#[test]
fn pagination_does_not_panic_when_offset_plus_limit_overflows_u32() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    // offset + limit would overflow u32 with raw addition; must not panic
    // and must simply return no results since the index is empty.
    let result = client.streams_by_sender(&sender, &u32::MAX, &u32::MAX);
    assert_eq!(result.len(), 0);
}

#[test]
fn streams_by_recipient_returns_empty_for_unknown_address() {
    let env = base_env();
    let client = deploy_factory(&env);
    let recip = Address::generate(&env);
    let result = client.streams_by_recipient(&recip, &0, &10);
    assert_eq!(result.len(), 0);
}

#[test]
fn protocol_fee_bps_returns_default_30() {
    let env = base_env();
    let client = deploy_factory(&env);
    assert_eq!(client.protocol_fee_bps(), 30);
}

#[test]
fn upgrade_stream_wasm_on_uninitialized_factory_is_rejected() {
    let env = base_env();
    let id = env.register_contract(None, DripFactory);
    let client = DripFactoryClient::new(&env, &id);
    let new_hash = BytesN::from_array(&env, &[2u8; 32]);
    let result = client.try_upgrade_stream_wasm(&new_hash);
    assert_eq!(result, Err(Ok(Error::NotInitialized)));
}

// ── TTL management ─────────────────────────────────────────────────────────────

#[test]
fn initialize_extends_factory_instance_ttl() {
    let env = base_env();
    let client = deploy_factory(&env);
    let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
    assert_eq!(ttl, 200_000);
}

#[test]
fn upgrade_stream_wasm_extends_instance_ttl() {
    let env = base_env();
    let client = deploy_factory(&env);
    let new_hash = BytesN::from_array(&env, &[3u8; 32]);
    client.upgrade_stream_wasm(&new_hash);
    let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
    assert_eq!(ttl, 200_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn re_initializing_factory_panics() {
    let env = base_env();
    let client = deploy_factory(&env);
    // An attacker calling initialize() again to swap in their own
    // stream_wasm_hash/governor must be rejected.
    let attacker_governor = Address::generate(&env);
    let attacker_hash = BytesN::from_array(&env, &[1u8; 32]);
    client.initialize(&attacker_hash, &attacker_governor);
}

// ── Validation errors (all fail before deployment) ────────────────────────────

fn make_token(env: &Env, sender: &Address, amount: i128) -> Address {
    let admin = Address::generate(env);
    let addr = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    token::StellarAssetClient::new(env, &addr).mint(sender, &amount);
    addr
}

#[test]
fn create_stream_rejects_zero_deposit() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 0);
    let now = env.ledger().timestamp();
    // deposit = 0 → InvalidDeposit before any deployment
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &0,
        &100,
        &(now + 100),
        &(now + 3_700),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::InvalidDeposit)));
}

#[test]
fn create_stream_rejects_zero_rate() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 10_000);
    let now = env.ledger().timestamp();
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &10_000,
        &0,
        &(now + 100),
        &(now + 3_700),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::InvalidRate)));
}

#[test]
fn create_stream_rejects_deposit_less_than_one_second() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 10_000);
    let now = env.ledger().timestamp();
    // deposit (50) < rate_per_sec (100) → InsufficientDeposit
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &50,
        &100,
        &(now + 100),
        &(now + 3_700),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::InsufficientDeposit)));
}

#[test]
fn create_stream_rejects_deposit_short_of_full_duration() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let now = env.ledger().timestamp();
    // rate 100/s over a 3_600s window needs a 360_000 deposit; passing
    // enough for only 1 second (which alone would pass the old check)
    // must now be rejected because it can't fund the declared end_time.
    let token = make_token(&env, &sender, 100);
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &100,
        &100,
        &now,
        &(now + 3_600),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::InsufficientDeposit)));
}

#[test]
fn create_stream_rejects_start_time_in_past() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 100_000);
    let now = env.ledger().timestamp();
    // start_time = now - 1 → BackdatedStream
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &100_000,
        &100,
        &(now - 1),
        &(now + 3_600),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::BackdatedStream)));
}

#[test]
fn create_stream_rejects_end_before_start() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 100_000);
    let now = env.ledger().timestamp();
    // end_time <= start_time → InvalidTimeRange
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &100_000,
        &100,
        &(now + 100),
        &(now + 50),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::InvalidTimeRange)));
}

#[test]
fn create_stream_rejects_end_equal_to_start() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 100_000);
    let now = env.ledger().timestamp();
    let start = now + 100;
    let result = client.try_create_stream(
        &sender, &recip, &token, &100_000, &100, &start, &start, &false,
    );
    assert_eq!(result, Err(Ok(Error::InvalidTimeRange)));
}

// ── Governor-controlled bounds ────────────────────────────────────────────────

#[test]
fn create_stream_rejects_rate_above_governor_max() {
    let env = base_env();
    let (factory, governor) = deploy_factory_with_governor(&env);
    governor.set_max_rate(&1_000);

    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 1_000_000);
    let now = env.ledger().timestamp();
    // rate 1_001 > governor's max_rate_per_second of 1_000
    let result = factory.try_create_stream(
        &sender, &recip, &token, &1_000_000, &1_001, &now, &0, &false,
    );
    assert_eq!(result, Err(Ok(Error::RateExceedsMax)));
}

#[test]
fn create_stream_rejects_duration_below_governor_minimum() {
    let env = base_env();
    let (factory, governor) = deploy_factory_with_governor(&env);
    governor.set_min_duration(&7_200);

    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 1_000_000);
    let now = env.ledger().timestamp();
    // duration of 3_600s < governor's min_duration_seconds of 7_200
    let result = factory.try_create_stream(
        &sender,
        &recip,
        &token,
        &1_000_000,
        &100,
        &now,
        &(now + 3_600),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::DurationTooShort)));
}

#[test]
fn protocol_fee_bps_reflects_live_governor_value() {
    let env = base_env();
    let (factory, governor) = deploy_factory_with_governor(&env);
    assert_eq!(factory.protocol_fee_bps(), 30);

    governor.set_fee_bps(&75);
    assert_eq!(factory.protocol_fee_bps(), 75);
}

#[test]
fn protocol_fee_bps_defaults_to_30_when_uninitialized() {
    let env = base_env();
    let id = env.register_contract(None, DripFactory);
    let client = DripFactoryClient::new(&env, &id);
    assert_eq!(client.protocol_fee_bps(), 30);
}
