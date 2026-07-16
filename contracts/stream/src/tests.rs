#![cfg(test)]

// The crate is `#![no_std]`, but this module only compiles under `cargo test`,
// where `std` is available as a linked dependency of the test harness anyway.
extern crate std;
use std::boxed::Box;

use soroban_sdk::{
    testutils::{storage::Instance as _, Address as _, Ledger, LedgerInfo},
    token, Address, Env,
};

use crate::{DripStream, DripStreamClient, Error};

/// Deploy a mock token and a DripStream, returning both clients and
/// the sender/recipient addresses.
struct Setup {
    env: Env,
    client: DripStreamClient<'static>,
    token: token::Client<'static>,
    sender: Address,
    recipient: Address,
}

impl Setup {
    fn new(rate_per_second: i128, duration_secs: u64, clawback: bool) -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        // Deploy a mock Stellar asset contract
        let token_admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let tok = token::Client::new(&env, &token_addr);
        let tok_admin = token::StellarAssetClient::new(&env, &token_addr);

        let deposit = rate_per_second * duration_secs as i128;

        // Mint the deposit to the sender
        tok_admin.mint(&sender, &deposit);

        // Set ledger timestamp to a baseline
        let now: u64 = 1_000_000;
        env.ledger().set(LedgerInfo {
            timestamp: now,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        // Deploy stream
        let stream_id = env.register_contract(None, DripStream);
        let client = DripStreamClient::new(&env, &stream_id);

        // Transfer deposit into stream
        tok.transfer(&sender, &stream_id, &deposit);

        client.initialize(
            &sender,
            &recipient,
            &token_addr,
            &rate_per_second,
            &now,                   // start_time = now
            &(now + duration_secs), // end_time
            &clawback,
        );

        // Leak the env so we can return 'static references — acceptable in tests.
        let env: &'static Env = Box::leak(Box::new(env));
        let client = DripStreamClient::new(env, &stream_id);
        let token = token::Client::new(env, &token_addr);

        Self {
            env: unsafe { std::ptr::read(env) },
            client,
            token,
            sender,
            recipient,
        }
    }

    fn advance_secs(&self, secs: u64) {
        let ts = self.env.ledger().timestamp() + secs;
        self.env.ledger().set(LedgerInfo {
            timestamp: ts,
            ..self.env.ledger().get()
        });
    }
}

// ── Withdraw ─────────────────────────────────────────────────────────────────

#[test]
fn withdraw_zero_at_start() {
    let s = Setup::new(100, 3600, false);
    // At exactly start_time, elapsed = 0
    assert_eq!(s.client.withdrawable(), 0);
}

#[test]
fn withdraw_correct_after_elapsed() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    // 100 seconds × 100 stroops/s = 10_000 stroops
    assert_eq!(s.client.withdrawable(), 10_000);
    let withdrawn = s.client.withdraw(&10_000);
    assert_eq!(withdrawn, 10_000);
    assert_eq!(s.token.balance(&s.recipient), 10_000);
}

#[test]
fn withdraw_capped_at_available() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(50);
    // Available = 5_000; requesting 99_999 should give back only 5_000
    let withdrawn = s.client.withdraw(&99_999);
    assert_eq!(withdrawn, 5_000);
}

#[test]
fn withdraw_before_any_elapsed_panics() {
    let s = Setup::new(100, 3600, false);
    let result = s.client.try_withdraw(&1);
    assert_eq!(result, Err(Ok(Error::NothingToWithdraw)));
}

#[test]
fn withdrawable_stops_at_end_time() {
    let s = Setup::new(100, 100, false); // 100s stream
    s.advance_secs(200); // advance past end_time
                         // Should be capped at 100s worth = 10_000
    assert_eq!(s.client.withdrawable(), 10_000);
}

// ── Pause / Resume ────────────────────────────────────────────────────────────

#[test]
fn pause_freezes_withdrawable() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    let before_pause = s.client.withdrawable();
    s.client.pause();
    s.advance_secs(500); // time passes but stream is paused
    assert_eq!(s.client.withdrawable(), before_pause); // unchanged
}

#[test]
fn resume_continues_streaming() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100); // 100s elapsed → 10_000 owed
    s.client.pause();
    s.advance_secs(200); // 200s paused (should not count)
    s.client.resume();
    s.advance_secs(50); // 50s more elapsed → +5_000
                        // Total should be 150s of streaming = 15_000
    assert_eq!(s.client.withdrawable(), 15_000);
}

#[test]
fn double_pause_panics() {
    let s = Setup::new(100, 3600, false);
    s.client.pause();
    let result = s.client.try_pause();
    assert_eq!(result, Err(Ok(Error::AlreadyPaused)));
}

#[test]
fn resume_unpaused_panics() {
    let s = Setup::new(100, 3600, false);
    let result = s.client.try_resume(); // not paused
    assert_eq!(result, Err(Ok(Error::NotPaused)));
}

// ── Cancel ────────────────────────────────────────────────────────────────────

#[test]
fn cancel_before_start_refunds_full_deposit() {
    let s = Setup::new(100, 3600, false);
    let deposit = 100 * 3600;
    let sender_before = s.token.balance(&s.sender);
    s.client.cancel();
    let sender_after = s.token.balance(&s.sender);
    assert_eq!(sender_after - sender_before, deposit);
    assert_eq!(s.token.balance(&s.recipient), 0);
}

#[test]
fn cancel_halfway_splits_correctly() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(1800); // halfway
    let sender_before = s.token.balance(&s.sender);
    let recipient_before = s.token.balance(&s.recipient);
    s.client.cancel();
    // Recipient gets 1800 × 100 = 180_000 (earned but not withdrawn)
    // Sender gets 180_000 refund
    assert_eq!(s.token.balance(&s.recipient) - recipient_before, 180_000);
    assert_eq!(s.token.balance(&s.sender) - sender_before, 180_000);
}

#[test]
fn cancel_then_cancel_panics() {
    let s = Setup::new(100, 3600, false);
    s.client.cancel();
    let result = s.client.try_cancel();
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn withdraw_after_cancel_panics() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    s.client.cancel();
    // stream is fully settled; withdraw blocked
    let result = s.client.try_withdraw(&1);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

// ── Clawback ─────────────────────────────────────────────────────────────────

#[test]
fn clawback_reclaims_unstreamed() {
    let s = Setup::new(100, 3600, true); // clawback enabled
    s.advance_secs(600); // 600s streamed → 60_000 owed to recipient
    let sender_before = s.token.balance(&s.sender);
    let reclaimed = s.client.clawback();
    // reclaimed = total_balance − owed = (100×3600) − 60_000 = 300_000
    assert_eq!(reclaimed, 300_000);
    assert_eq!(s.token.balance(&s.sender) - sender_before, 300_000);
}

#[test]
fn clawback_disabled_panics() {
    let s = Setup::new(100, 3600, false);
    let result = s.client.try_clawback();
    assert_eq!(result, Err(Ok(Error::ClawbackDisabled)));
}

// ── Top-up ────────────────────────────────────────────────────────────────────

#[test]
fn top_up_increases_contract_balance() {
    let s = Setup::new(100, 3600, false);
    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &50_000);

    let stream_before = s.token.balance(&s.client.address);
    s.client.top_up(&50_000);
    assert_eq!(s.token.balance(&s.client.address), stream_before + 50_000);
}

#[test]
fn top_up_on_cancelled_stream_is_rejected() {
    let s = Setup::new(100, 3600, false);
    s.client.cancel();

    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &10_000);

    let result = s.client.try_top_up(&10_000);
    assert!(result.is_err());
}

#[test]
fn top_up_rejects_zero_and_negative_amount() {
    let s = Setup::new(100, 3600, false);
    assert_eq!(s.client.try_top_up(&0), Err(Ok(Error::InvalidAmount)));
    assert_eq!(s.client.try_top_up(&-1), Err(Ok(Error::InvalidAmount)));
}

#[test]
fn withdraw_rejects_zero_and_negative_amount() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    assert_eq!(s.client.try_withdraw(&0), Err(Ok(Error::InvalidAmount)));
    assert_eq!(s.client.try_withdraw(&-1), Err(Ok(Error::InvalidAmount)));
}

// ── Initialization guard ──────────────────────────────────────────────────────

#[test]
#[should_panic(expected = "Error(Contract, #14)")]
fn re_initializing_an_active_stream_panics() {
    let s = Setup::new(100, 3600, false);
    // An attacker calling initialize() again to hijack sender/recipient
    // must be rejected — otherwise they could redirect the escrowed balance
    // to themselves via cancel()/clawback().
    let attacker = Address::generate(&s.env);
    s.client
        .initialize(&attacker, &attacker, &s.token.address, &1, &0, &0, &false);
}

// ── TTL management ─────────────────────────────────────────────────────────────

#[test]
fn initialize_extends_instance_ttl() {
    let s = Setup::new(100, 3600, false);
    // Without an explicit extend_ttl call, instance storage TTL is left at
    // whatever the host assigns on creation, which is well under the
    // production-safe window. initialize() must bump it immediately.
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert_eq!(ttl, 200_000);
}

#[test]
fn withdraw_extends_instance_ttl() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    s.client.withdraw(&1);
    let ttl = s
        .env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl());
    assert_eq!(ttl, 200_000);
}

// ── Cancelled stream state ────────────────────────────────────────────────────

#[test]
fn withdrawable_returns_zero_after_cancel() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(500);
    assert!(s.client.withdrawable() > 0);

    s.client.cancel();
    assert_eq!(s.client.withdrawable(), 0);
}

#[test]
fn pause_then_cancel_refunds_correctly() {
    let s = Setup::new(100, 3600, false);
    let deposit = 100 * 3600; // 360_000

    s.advance_secs(600); // 60_000 streamed
    s.client.pause();
    s.advance_secs(1_000); // time passes; not counted

    let sender_before = s.token.balance(&s.sender);
    let recipient_before = s.token.balance(&s.recipient);
    s.client.cancel();

    // Recipient should get 60_000 (earned before pause)
    // Sender should get 360_000 − 60_000 = 300_000
    assert_eq!(s.token.balance(&s.recipient) - recipient_before, 60_000);
    assert_eq!(s.token.balance(&s.sender) - sender_before, 300_000);
    let _ = deposit; // suppress unused warning
}

// ── Stream info ───────────────────────────────────────────────────────────────

#[test]
fn info_returns_correct_initial_state() {
    let s = Setup::new(250, 7_200, true);
    let inf = s.client.info();

    assert_eq!(inf.rate_per_second, 250);
    assert!(!inf.paused);
    assert!(!inf.cancelled);
    assert!(inf.clawback_enabled);
    assert_eq!(inf.withdrawn, 0);
}

#[test]
fn info_reflects_pause_state() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    s.client.pause();

    let inf = s.client.info();
    assert!(inf.paused);
    assert!(inf.paused_at > 0);
}

// ── Edge cases ────────────────────────────────────────────────────────────────

#[test]
fn withdraw_exactly_full_balance_succeeds() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(3600); // end_time reached — full deposit earned
    let total = 100 * 3600; // 360_000

    let withdrawn = s.client.withdraw(&(total as i128));
    assert_eq!(withdrawn, total as i128);
    assert_eq!(s.token.balance(&s.recipient), total as i128);
}

#[test]
fn multiple_sequential_withdrawals_sum_correctly() {
    let s = Setup::new(1_000, 3_600, false);
    s.advance_secs(900); // 900_000 streamed

    let w1 = s.client.withdraw(&300_000);
    let w2 = s.client.withdraw(&300_000);
    let w3 = s.client.withdraw(&300_000);

    assert_eq!(w1 + w2 + w3, 900_000);
    assert_eq!(s.token.balance(&s.recipient), 900_000);
}
