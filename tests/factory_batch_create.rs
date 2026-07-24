//! Integration tests: DripFactory::create_batch_streams validation and
//! atomicity.
//!
//! Like tests/factory_deploy.rs, tests here that would require a
//! successfully *deployed* stream (and thus a built stream WASM via
//! `cargo build --target wasm32-unknown-unknown --release`) are gated
//! behind #[ignore]. The tests below cover everything reachable without
//! a real deployment: the empty-batch guard, the MAX_BATCH_SIZE cap, and
//! atomic revert-before-any-deployment when one request in the batch
//! fails validation.

use drip_factory::{BatchStreamRequest, DripFactory, DripFactoryClient, Error, MAX_BATCH_SIZE};
use drip_governor::{DripGovernor, DripGovernorClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token, Address, BytesN, Env, Vec,
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

/// Mirrors tests/factory_deploy.rs::deploy_factory.
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

fn valid_request(recipient: &Address, token: &Address, now: u64) -> BatchStreamRequest {
    BatchStreamRequest {
        recipient: recipient.clone(),
        token: token.clone(),
        deposit: 360_000,
        rate_per_sec: 100,
        start_time: now,
        end_time: now + 3_600,
    }
}

#[test]
fn create_batch_streams_rejects_empty_batch() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let requests: Vec<BatchStreamRequest> = Vec::new(&env);
    let result = client.try_create_batch_streams(&sender, &requests, &false);
    assert_eq!(result, Err(Ok(Error::EmptyBatch)));
}

#[test]
fn create_batch_streams_rejects_batch_over_max_size() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let now = env.ledger().timestamp();
    let token = make_token(&env, &sender, 1);

    // MAX_BATCH_SIZE + 1 identical (otherwise-valid-shaped) requests --
    // the size cap must reject before any of them are even inspected.
    let mut requests: Vec<BatchStreamRequest> = Vec::new(&env);
    for _ in 0..(MAX_BATCH_SIZE + 1) {
        requests.push_back(valid_request(&recipient, &token, now));
    }

    let result = client.try_create_batch_streams(&sender, &requests, &false);
    assert_eq!(result, Err(Ok(Error::BatchTooLarge)));
}

#[test]
fn create_batch_streams_reverts_whole_batch_on_first_invalid_request() {
    // The first request has deposit = 0 (InvalidDeposit), which
    // create_stream rejects before touching any state or deploying
    // anything. Confirms the batch aborts immediately via `?` and
    // stream_count stays at 0 -- no partial creation.
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let now = env.ledger().timestamp();
    let token = make_token(&env, &sender, 100_000);

    let mut bad = valid_request(&recipient, &token, now);
    bad.deposit = 0;

    let mut requests: Vec<BatchStreamRequest> = Vec::new(&env);
    requests.push_back(bad);

    let result = client.try_create_batch_streams(&sender, &requests, &false);
    assert_eq!(result, Err(Ok(Error::InvalidDeposit)));
    assert_eq!(client.stream_count(), 0);
}

// -- Gas benchmark (requires a built stream WASM) ----------------------------
//
// A full successful create_batch_streams call deploys a real DripStream
// per request, which -- per tests/factory_deploy.rs's own docstring --
// requires `cargo build --target wasm32-unknown-unknown --release` first.
// This repo's existing test suite deliberately avoids that build step
// (see tests/factory_ttl.rs), so this benchmark is #[ignore]d rather than
// run by default `cargo test`.
//
// Run after building the stream WASM:
//   cargo build -p drip-stream --target wasm32-unknown-unknown --release
//   cargo test --test factory_batch_create -- --ignored --nocapture
//
// Uses env.budget().print() to report cost after each successful batch.
#[test]
#[ignore = "requires stream WASM built via cargo build -p drip-stream --target wasm32-unknown-unknown --release"]
fn gas_usage_for_batches_of_10_50_100() {
    let env = base_env();
    let client = deploy_factory(&env);
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let now = env.ledger().timestamp();

    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/target/wasm32-unknown-unknown/release/drip_stream.wasm"
    );
    let wasm = std::fs::read(wasm_path).expect("build the stream WASM first: cargo build -p drip-stream --target wasm32-unknown-unknown --release");
    let wasm_hash = env.deployer().upload_contract_wasm(wasm.as_slice());
    env.as_contract(&client.address, || {
        env.storage()
            .instance()
            .set(&drip_factory::storage::DataKey::StreamWasmHash, &wasm_hash);
    });

    let token = make_token(&env, &sender, 1_000_000_000);

    for size in [10u32, 50, 100] {
        let mut requests: Vec<BatchStreamRequest> = Vec::new(&env);
        for _ in 0..size {
            requests.push_back(valid_request(&recipient, &token, now));
        }

        let result = client.try_create_batch_streams(&sender, &requests, &false);
        assert!(
            result.is_ok(),
            "batch of {size} should succeed with a real stream WASM"
        );

        std::println!("=== batch size {size} ===");
        env.budget().print();
    }
}
