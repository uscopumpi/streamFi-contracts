//! Integration tests: DripFactory emergency pause.
//!
//! Covers the governor-gated `pause`/`unpause` controls and the guard they
//! place on `create_stream`. These tests exercise the validation/guard paths
//! that do not require an actual stream deployment (which needs the stream
//! WASM to be built first).

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

fn make_token(env: &Env, sender: &Address, amount: i128) -> Address {
    let admin = Address::generate(env);
    let addr = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    token::StellarAssetClient::new(env, &addr).mint(sender, &amount);
    addr
}

// ── Default state ──────────────────────────────────────────────────────────────

#[test]
fn factory_starts_unpaused() {
    let env = base_env();
    let client = deploy_factory(&env);
    assert!(!client.is_paused());
}

// ── Pause / unpause lifecycle ──────────────────────────────────────────────────

#[test]
fn governor_can_pause_and_unpause() {
    let env = base_env();
    let client = deploy_factory(&env);

    client.pause();
    assert!(client.is_paused());

    client.unpause();
    assert!(!client.is_paused());
}

#[test]
fn pausing_when_already_paused_is_rejected() {
    let env = base_env();
    let client = deploy_factory(&env);

    client.pause();
    let result = client.try_pause();
    assert_eq!(result, Err(Ok(Error::AlreadyPaused)));
}

#[test]
fn unpausing_when_not_paused_is_rejected() {
    let env = base_env();
    let client = deploy_factory(&env);

    let result = client.try_unpause();
    assert_eq!(result, Err(Ok(Error::NotPaused)));
}

#[test]
fn pause_on_uninitialized_factory_is_rejected() {
    let env = base_env();
    let id = env.register_contract(None, DripFactory);
    let client = DripFactoryClient::new(&env, &id);
    let result = client.try_pause();
    assert_eq!(result, Err(Ok(Error::NotInitialized)));
}

#[test]
fn unpause_on_uninitialized_factory_is_rejected() {
    let env = base_env();
    let id = env.register_contract(None, DripFactory);
    let client = DripFactoryClient::new(&env, &id);
    let result = client.try_unpause();
    assert_eq!(result, Err(Ok(Error::NotInitialized)));
}

// ── create_stream guard ────────────────────────────────────────────────────────

#[test]
fn create_stream_is_rejected_while_paused() {
    let env = base_env();
    let client = deploy_factory(&env);
    client.pause();

    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    // Fully valid parameters — the only reason this must fail is the pause,
    // which short-circuits before any validation or deposit transfer.
    let token = make_token(&env, &sender, 1_000_000);
    let now = env.ledger().timestamp();
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &1_000_000,
        &100,
        &now,
        &(now + 7_200),
        &false,
    );
    assert_eq!(result, Err(Ok(Error::ContractPaused)));
}

#[test]
fn create_stream_works_again_after_unpause() {
    let env = base_env();
    let client = deploy_factory(&env);
    client.pause();
    client.unpause();

    // With a dummy (all-zero) stream WASM hash the deploy step cannot succeed,
    // but the pause guard runs first: once unpaused, create_stream progresses
    // past the guard and fails later for an unrelated reason — never with
    // ContractPaused.
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let token = make_token(&env, &sender, 1_000_000);
    let now = env.ledger().timestamp();
    let result = client.try_create_stream(
        &sender,
        &recip,
        &token,
        &1_000_000,
        &100,
        &now,
        &(now + 7_200),
        &false,
    );
    assert_ne!(result, Err(Ok(Error::ContractPaused)));
}

// ── TTL management ─────────────────────────────────────────────────────────────

#[test]
fn pause_extends_instance_ttl() {
    let env = base_env();
    let client = deploy_factory(&env);
    client.pause();
    let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
    assert_eq!(ttl, 200_000);
}
