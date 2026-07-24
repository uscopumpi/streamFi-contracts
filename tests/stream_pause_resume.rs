//! Integration tests: pause / resume behaviour.

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

fn deploy_stream<'a>(
    env: &'a Env,
    sender: &Address,
    recipient: &Address,
    rate: i128,
    duration: u64,
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
        &false,
    );

    (client, token_addr)
}

fn advance(env: &Env, secs: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: env.ledger().timestamp() + secs,
        ..env.ledger().get()
    });
}

// ── Pause freezes accrual ────────────────────────────────────────────────────

#[test]
fn pause_freezes_withdrawable_amount() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let (client, _) = deploy_stream(&env, &sender, &recipient, 1_000, 3_600);

    advance(&env, 200);
    let withdrawable_at_pause = client.withdrawable(); // 200_000
    assert_eq!(withdrawable_at_pause, 200_000);

    client.pause();

    advance(&env, 500); // 500 more seconds pass — should not count
    assert_eq!(client.withdrawable(), withdrawable_at_pause); // still 200_000
}

#[test]
fn paused_time_excluded_from_streamed_total() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let (client, _) = deploy_stream(&env, &sender, &recipient, 1_000, 7_200);

    advance(&env, 100); // 100s → 100_000 streamed
    client.pause();
    advance(&env, 1_000); // 1000s paused (should not count)
    client.resume();
    advance(&env, 100); // 100s more → +100_000

    // Total should be 200s × 1_000 = 200_000
    assert_eq!(client.withdrawable(), 200_000);
}

#[test]
fn resume_after_pause_continues_stream_correctly() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let (client, token_addr) = deploy_stream(&env, &sender, &recipient, 500, 7_200);
    let tok = token::Client::new(&env, &token_addr);

    advance(&env, 400); // 400 × 500 = 200_000 streamed
    client.pause();
    advance(&env, 2_000); // 2000s paused
    client.resume();
    advance(&env, 200); // 200 × 500 = 100_000 more streamed

    assert_eq!(client.withdrawable(), 300_000);

    client.withdraw(&300_000);
    assert_eq!(tok.balance(&recipient), 300_000);
}

// ── Error cases ──────────────────────────────────────────────────────────────

#[test]
fn double_pause_is_rejected() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let (client, _) = deploy_stream(&env, &sender, &recip, 100, 3_600);
    client.pause();
    let result = client.try_pause(); // second pause should error
    assert_eq!(result, Err(Ok(Error::AlreadyPaused)));
}

#[test]
fn resume_on_running_stream_is_rejected() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recip = Address::generate(&env);
    let (client, _) = deploy_stream(&env, &sender, &recip, 100, 3_600);
    let result = client.try_resume(); // stream is not paused
    assert_eq!(result, Err(Ok(Error::NotPaused)));
}

// ── Recipient can withdraw while paused ──────────────────────────────────────

#[test]
fn recipient_can_withdraw_accumulated_balance_while_paused() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let (client, token_addr) = deploy_stream(&env, &sender, &recipient, 1_000, 3_600);
    let tok = token::Client::new(&env, &token_addr);

    advance(&env, 300); // 300_000 streamed
    client.pause();
    advance(&env, 1_000); // time passes but stream frozen

    // Recipient should still be able to withdraw the 300_000 earned before pause
    let withdrawn = client.withdraw(&300_000);
    assert_eq!(withdrawn, 300_000);
    assert_eq!(tok.balance(&recipient), 300_000);

    // Nothing more accrues while paused
    assert_eq!(client.withdrawable(), 0);
}

// ── Multiple pause/resume cycles ─────────────────────────────────────────────

#[test]
fn multiple_pause_resume_cycles_accumulate_correctly() {
    let env = base_env();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let (client, _) = deploy_stream(&env, &sender, &recipient, 1_000, 36_000);

    // Cycle 1: stream 100s, pause 500s, resume
    advance(&env, 100);
    client.pause();
    advance(&env, 500);
    client.resume();

    // Cycle 2: stream 200s, pause 1000s, resume
    advance(&env, 200);
    client.pause();
    advance(&env, 1_000);
    client.resume();

    // Cycle 3: stream 50s
    advance(&env, 50);

    // Only 100 + 200 + 50 = 350 seconds of actual streaming
    assert_eq!(client.withdrawable(), 350_000);
}
