//! Integration tests: clawback behaviour.

use drip_stream::{DripStream, DripStreamClient, Error};
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token, Address, Env,
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

fn deploy_stream_with_clawback<'a>(
    env: &'a Env,
    sender: &Address,
    recipient: &Address,
    rate: i128,
    duration: u64,
    clawback: bool,
) -> (DripStreamClient<'a>, Address) {
    let token_admin = Address::generate(env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let deposit = rate * duration as i128;

    token::StellarAssetClient::new(env, &token_addr).mint(sender, &deposit);

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(env, &stream_id);
    token::Client::new(env, &token_addr).transfer(sender, &stream_id, &deposit);

    let now = env.ledger().timestamp();
    client.initialize(
        sender,
        recipient,
        &token_addr,
        &rate,
        &now,
        &(now + duration),
        &clawback,
    );

    (client, token_addr)
}

fn advance(env: &Env, secs: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: env.ledger().timestamp() + secs,
        ..env.ledger().get()
    });
}

// ── Clawback enabled ─────────────────────────────────────────────────────────

#[test]
fn clawback_reclaims_unstreamed_portion() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let rate = 1_000_i128;
    let duration = 3_600_u64; // deposit = rate * duration = 3_600_000

    let (client, token_addr) =
        deploy_stream_with_clawback(&env, &sender, &recipient, rate, duration, true);
    let tok = token::Client::new(&env, &token_addr);

    advance(&env, 600); // 600s streamed → 600_000 owed to recipient

    let sender_before = tok.balance(&sender);
    let reclaimed = client.clawback();

    // Unstreamed = 3_600_000 − 600_000 = 3_000_000
    assert_eq!(reclaimed, 3_000_000);
    assert_eq!(tok.balance(&sender) - sender_before, 3_000_000);

    // Recipient's share (600_000) still sits in the contract
    assert_eq!(tok.balance(&client.address), 600_000);
}

#[test]
fn clawback_after_partial_withdrawal_accounts_for_withdrawn() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    let (client, token_addr) =
        deploy_stream_with_clawback(&env, &sender, &recipient, 1_000, 3_600, true);
    let tok = token::Client::new(&env, &token_addr);

    advance(&env, 900); // 900_000 streamed

    // Recipient withdraws 500_000
    client.withdraw(&500_000);
    assert_eq!(tok.balance(&recipient), 500_000);

    // Remaining in contract: 3_600_000 − 500_000 = 3_100_000
    // Owed to recipient (earned but not withdrawn): 900_000 − 500_000 = 400_000
    // Clawback should get: 3_100_000 − 400_000 = 2_700_000
    let sender_before = tok.balance(&sender);
    let reclaimed = client.clawback();

    assert_eq!(reclaimed, 2_700_000);
    assert_eq!(tok.balance(&sender) - sender_before, 2_700_000);
}

#[test]
fn clawback_on_finished_stream_reclaims_nothing() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    let (client, token_addr) =
        deploy_stream_with_clawback(&env, &sender, &recipient, 1_000, 100, true);
    let tok = token::Client::new(&env, &token_addr);

    advance(&env, 200); // past end_time — all 100_000 is owed to recipient

    let sender_before = tok.balance(&sender);
    let reclaimed = client.clawback();

    assert_eq!(reclaimed, 0);
    assert_eq!(tok.balance(&sender) - sender_before, 0);
    // Recipient can still withdraw the full deposit
    assert_eq!(client.withdrawable(), 100_000);
    client.withdraw(&100_000);
    assert_eq!(tok.balance(&recipient), 100_000);
}

// ── Clawback disabled ────────────────────────────────────────────────────────

#[test]
fn clawback_on_non_clawback_stream_is_rejected() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let (client, _) = deploy_stream_with_clawback(&env, &sender, &recip, 100, 3_600, false);
    advance(&env, 100);
    let result = client.try_clawback();
    assert_eq!(result, Err(Ok(Error::ClawbackDisabled)));
}

// ── Cancelled stream ─────────────────────────────────────────────────────────

#[test]
fn clawback_on_cancelled_stream_is_rejected() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let (client, _) = deploy_stream_with_clawback(&env, &sender, &recip, 100, 3_600, true);
    client.cancel();
    let result = client.try_clawback(); // stream cancelled
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

// ── Paused stream clawback ───────────────────────────────────────────────────

#[test]
fn clawback_while_paused_uses_paused_timestamp() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    let (client, token_addr) =
        deploy_stream_with_clawback(&env, &sender, &recipient, 1_000, 3_600, true);
    let tok = token::Client::new(&env, &token_addr);

    advance(&env, 300); // 300_000 streamed at pause point
    client.pause();
    advance(&env, 500); // paused — no additional accrual

    // Owed = 300_000; unstreamed = 3_300_000
    let sender_before = tok.balance(&sender);
    let reclaimed = client.clawback();

    assert_eq!(reclaimed, 3_300_000);
    assert_eq!(tok.balance(&sender) - sender_before, 3_300_000);
}
