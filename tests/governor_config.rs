//! Integration tests: DripGovernor parameter management.

use drip_governor::{DripGovernor, DripGovernorClient, Error};
use soroban_sdk::{
    testutils::{storage::Instance as _, Address as _},
    Address, Env,
};

fn deploy_governor(env: &Env) -> (DripGovernorClient<'_>, Address, Address) {
    let authority = Address::generate(env);
    let fee_recipient = Address::generate(env);
    let factory_address = Address::generate(env);

    let id = env.register_contract(None, DripGovernor);
    let client = DripGovernorClient::new(env, &id);

    client.initialize(&authority, &fee_recipient, &factory_address);

    (client, authority, fee_recipient)
}

// ── Defaults ─────────────────────────────────────────────────────────────────

#[test]
fn initialize_sets_correct_defaults() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _authority, fee_recipient) = deploy_governor(&env);
    let config = client.config();

    assert_eq!(config.fee_bps, 30);
    assert_eq!(config.min_duration_seconds, 3_600);
    assert_eq!(config.max_rate_per_second, 1_000_000_000_000_000);
    assert_eq!(config.fee_recipient, fee_recipient);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn re_initializing_governor_panics() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _authority, _fee_recipient) = deploy_governor(&env);
    // An attacker calling initialize() again to grant themselves Admin must be
    // rejected — otherwise they could set fee_bps to the maximum or redirect
    // fee_recipient.
    let attacker = Address::generate(&env);
    client.initialize(&attacker, &attacker, &attacker);
}

// ── TTL management ─────────────────────────────────────────────────────────────

#[test]
fn initialize_extends_instance_ttl() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _authority, _fee_recipient) = deploy_governor(&env);
    let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
    assert_eq!(ttl, 200_000);
}

#[test]
fn set_fee_bps_extends_instance_ttl() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _fee_recipient) = deploy_governor(&env);
    client.set_fee_bps(&authority, &50);
    let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
    assert_eq!(ttl, 200_000);
}

// ── Fee BPS ──────────────────────────────────────────────────────────────────

#[test]
fn authority_can_update_fee_bps() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    client.set_fee_bps(&authority, &50);
    assert_eq!(client.config().fee_bps, 50);
}

#[test]
fn fee_bps_of_zero_is_valid() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    client.set_fee_bps(&authority, &0);
    assert_eq!(client.config().fee_bps, 0);
}

#[test]
fn fee_bps_of_10000_is_valid() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    client.set_fee_bps(&authority, &10_000);
    assert_eq!(client.config().fee_bps, 10_000);
}

#[test]
fn fee_bps_exceeding_10000_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    let result = client.try_set_fee_bps(&authority, &10_001);
    assert_eq!(result, Err(Ok(Error::InvalidParam)));
}

// ── Min duration ─────────────────────────────────────────────────────────────

#[test]
fn authority_can_set_min_duration() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    client.set_min_duration(&authority, &7_200);
    assert_eq!(client.config().min_duration_seconds, 7_200);
}

#[test]
fn zero_min_duration_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    let result = client.try_set_min_duration(&authority, &0);
    assert_eq!(result, Err(Ok(Error::InvalidParam)));
}

// ── Max rate ─────────────────────────────────────────────────────────────────

#[test]
fn authority_can_set_max_rate() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    client.set_max_rate(&authority, &500_000_000);
    assert_eq!(client.config().max_rate_per_second, 500_000_000);
}

#[test]
fn zero_max_rate_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    let result = client.try_set_max_rate(&authority, &0);
    assert_eq!(result, Err(Ok(Error::InvalidParam)));
}

#[test]
fn negative_max_rate_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    let result = client.try_set_max_rate(&authority, &-1);
    assert_eq!(result, Err(Ok(Error::InvalidParam)));
}

// ── Fee recipient ────────────────────────────────────────────────────────────

#[test]
fn authority_can_change_fee_recipient() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority, _) = deploy_governor(&env);
    let new_recipient = Address::generate(&env);
    client.set_fee_recipient(&authority, &new_recipient);
    assert_eq!(client.config().fee_recipient, new_recipient);
}

// ── Transfer authority ───────────────────────────────────────────────────────

#[test]
fn authority_transfers_correctly() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, old_authority, _) = deploy_governor(&env);
    let new_authority = Address::generate(&env);
    client.transfer_authority(&old_authority, &new_authority);

    // Post-transfer, a config read still works (roles are stored, not verified
    // on read).
    let config = client.config();
    assert_eq!(config.fee_bps, 30); // defaults unchanged
}
