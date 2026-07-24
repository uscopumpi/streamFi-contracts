//! Integration tests: DripGovernor role-based access control.
//!
//! Exercises granting and revoking roles, the independence of each role's
//! authority, rejection of unauthorized callers, and the last-admin guard.

use drip_governor::{DripGovernor, DripGovernorClient, Error, Role};
use soroban_sdk::{testutils::Address as _, Address, Env};

/// Deploys a governor and returns the client plus the bootstrap authority
/// (which starts out holding every role).
fn deploy_governor(env: &Env) -> (DripGovernorClient<'_>, Address) {
    let authority = Address::generate(env);
    let fee_recipient = Address::generate(env);
    let factory_address = Address::generate(env);

    let id = env.register_contract(None, DripGovernor);
    let client = DripGovernorClient::new(env, &id);
    client.initialize(&authority, &fee_recipient, &factory_address);

    (client, authority)
}

// ── Bootstrap ──────────────────────────────────────────────────────────────────

#[test]
fn authority_bootstraps_with_all_roles() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    assert!(client.has_role(&Role::Admin, &authority));
    assert!(client.has_role(&Role::FeeManager, &authority));
    assert!(client.has_role(&Role::RateManager, &authority));
}

#[test]
fn unrelated_account_holds_no_roles() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _authority) = deploy_governor(&env);
    let stranger = Address::generate(&env);
    assert!(!client.has_role(&Role::Admin, &stranger));
    assert!(!client.has_role(&Role::FeeManager, &stranger));
    assert!(!client.has_role(&Role::RateManager, &stranger));
}

// ── Grant / revoke ─────────────────────────────────────────────────────────────

#[test]
fn admin_can_grant_a_role() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let fee_manager = Address::generate(&env);

    client.grant_role(&authority, &Role::FeeManager, &fee_manager);
    assert!(client.has_role(&Role::FeeManager, &fee_manager));
    // Granting one role does not confer the others.
    assert!(!client.has_role(&Role::Admin, &fee_manager));
    assert!(!client.has_role(&Role::RateManager, &fee_manager));
}

#[test]
fn admin_can_revoke_a_role() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let fee_manager = Address::generate(&env);

    client.grant_role(&authority, &Role::FeeManager, &fee_manager);
    client.revoke_role(&authority, &Role::FeeManager, &fee_manager);
    assert!(!client.has_role(&Role::FeeManager, &fee_manager));
}

#[test]
fn granting_is_idempotent() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let other_admin = Address::generate(&env);

    // Two grants, then a single revoke, must leave the account without the
    // role — proving the admin count wasn't inflated to 2 by the double grant.
    client.grant_role(&authority, &Role::Admin, &other_admin);
    client.grant_role(&authority, &Role::Admin, &other_admin);
    client.revoke_role(&authority, &Role::Admin, &other_admin);
    assert!(!client.has_role(&Role::Admin, &other_admin));
}

// ── Authorization ──────────────────────────────────────────────────────────────

#[test]
fn non_admin_cannot_grant_roles() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    // fee_manager holds FeeManager but not Admin, so it can't grant roles.
    let fee_manager = Address::generate(&env);
    client.grant_role(&authority, &Role::FeeManager, &fee_manager);

    let target = Address::generate(&env);
    let result = client.try_grant_role(&fee_manager, &Role::RateManager, &target);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    assert!(!client.has_role(&Role::RateManager, &target));
}

#[test]
fn fee_manager_can_set_fees_but_not_rates() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let fee_manager = Address::generate(&env);
    client.grant_role(&authority, &Role::FeeManager, &fee_manager);

    // Allowed: fee parameters.
    client.set_fee_bps(&fee_manager, &75);
    assert_eq!(client.config().fee_bps, 75);

    // Rejected: rate parameters belong to RateManager.
    let result = client.try_set_max_rate(&fee_manager, &1_000);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn rate_manager_can_set_rates_but_not_fees() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let rate_manager = Address::generate(&env);
    client.grant_role(&authority, &Role::RateManager, &rate_manager);

    // Allowed: rate/duration parameters.
    client.set_max_rate(&rate_manager, &1_000);
    assert_eq!(client.config().max_rate_per_second, 1_000);
    client.set_min_duration(&rate_manager, &7_200);
    assert_eq!(client.config().min_duration_seconds, 7_200);

    // Rejected: fee parameters belong to FeeManager.
    let result = client.try_set_fee_bps(&rate_manager, &75);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoked_manager_loses_access() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let fee_manager = Address::generate(&env);
    client.grant_role(&authority, &Role::FeeManager, &fee_manager);
    client.set_fee_bps(&fee_manager, &75);

    client.revoke_role(&authority, &Role::FeeManager, &fee_manager);
    let result = client.try_set_fee_bps(&fee_manager, &80);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    // The last successful write stands.
    assert_eq!(client.config().fee_bps, 75);
}

#[test]
fn two_managers_operate_independently() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let fee_manager = Address::generate(&env);
    let rate_manager = Address::generate(&env);
    client.grant_role(&authority, &Role::FeeManager, &fee_manager);
    client.grant_role(&authority, &Role::RateManager, &rate_manager);

    client.set_fee_bps(&fee_manager, &42);
    client.set_max_rate(&rate_manager, &2_000);

    assert_eq!(client.config().fee_bps, 42);
    assert_eq!(client.config().max_rate_per_second, 2_000);
}

// ── Last-admin guard ───────────────────────────────────────────────────────────

#[test]
fn revoking_the_last_admin_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let result = client.try_revoke_role(&authority, &Role::Admin, &authority);
    assert_eq!(result, Err(Ok(Error::LastAdmin)));
    assert!(client.has_role(&Role::Admin, &authority));
}

#[test]
fn admin_can_be_revoked_once_a_second_admin_exists() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let second_admin = Address::generate(&env);

    client.grant_role(&authority, &Role::Admin, &second_admin);
    // With two admins, revoking one is allowed.
    client.revoke_role(&authority, &Role::Admin, &authority);
    assert!(!client.has_role(&Role::Admin, &authority));
    assert!(client.has_role(&Role::Admin, &second_admin));
}

// ── transfer_authority ─────────────────────────────────────────────────────────

#[test]
fn transfer_authority_moves_admin() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, authority) = deploy_governor(&env);
    let new_authority = Address::generate(&env);

    client.transfer_authority(&authority, &new_authority);
    assert!(client.has_role(&Role::Admin, &new_authority));
    assert!(!client.has_role(&Role::Admin, &authority));

    // The new admin can administer roles; the old one can't.
    let target = Address::generate(&env);
    client.grant_role(&new_authority, &Role::FeeManager, &target);
    assert!(client.has_role(&Role::FeeManager, &target));

    let result = client.try_grant_role(&authority, &Role::RateManager, &target);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

// ── Regression: uninitialised governor does not panic ────────────────────────
//
// Issue #89: the original `config()` used `unwrap()` on required storage keys.
// A cross-contract caller on an uninitialised governor would receive an opaque
// host trap with no indication of what went wrong. After the fix `config()`
// returns `Result<GovernorConfig, Error>` and `load()` returns `NotInitialized`
// when the mandatory keys are missing.

#[test]
fn uninitialized_governor_returns_error_not_panics() {
    let env = Env::default();
    let id = env.register_contract(None, DripGovernor);
    let client = DripGovernorClient::new(&env, &id);

    // Calling config() on an uninitialised governor must return an error
    // rather than panicking inside a cross-contract call.
    let result = client.try_config();
    assert!(result.is_err());
}

#[test]
fn config_after_initialize_returns_correct_values() {
    let env = Env::default();
    env.mock_all_auths();

    let (client, _authority) = deploy_governor(&env);
    let cfg = client.config();
    assert_eq!(cfg.fee_bps, 30);
    assert_eq!(cfg.min_duration_seconds, 3600);
    assert_eq!(cfg.max_rate_per_second, 1_000_000_000_000_000);
}
