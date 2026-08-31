#![cfg(test)]

// The crate is `#![no_std]`, but this module only compiles under `cargo test`,
// where `std` is available as a linked dependency of the test harness anyway.
extern crate std;
use std::boxed::Box;

use soroban_sdk::{
    symbol_short,
    testutils::{storage::Instance as _, Address as _, Events as _, Ledger, LedgerInfo},
    token, Address, Env, IntoVal, TryIntoVal,
};

use crate::{storage::DataKey, DripStream, DripStreamClient, Error};

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
fn storage_version_set_on_initialize() {
    let s = Setup::new(100, 3600, false);
    assert_eq!(s.client.storage_version(), 1);
}

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
    s.client.pause(&s.sender);
    s.advance_secs(500); // time passes but stream is paused
    assert_eq!(s.client.withdrawable(), before_pause); // unchanged
}

#[test]
fn resume_continues_streaming() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100); // 100s elapsed → 10_000 owed
    s.client.pause(&s.sender);
    s.advance_secs(200); // 200s paused (should not count)
    s.client.resume(&s.sender);
    s.advance_secs(50); // 50s more elapsed → +5_000
                        // Total should be 150s of streaming = 15_000
    assert_eq!(s.client.withdrawable(), 15_000);
}

#[test]
fn double_pause_panics() {
    let s = Setup::new(100, 3600, false);
    s.client.pause(&s.sender);
    let result = s.client.try_pause(&s.sender);
    assert_eq!(result, Err(Ok(Error::AlreadyPaused)));
}

#[test]
fn resume_unpaused_panics() {
    let s = Setup::new(100, 3600, false);
    let result = s.client.try_resume(&s.sender); // not paused
    assert_eq!(result, Err(Ok(Error::NotPaused)));
}

#[test]
fn pause_before_start_rejected() {
    let s = Setup::new(100, 3600, false);
    let mut ledger = s.env.ledger().get();
    ledger.timestamp -= 1;
    s.env.ledger().set(ledger);

    let result = s.client.try_pause(&s.sender);
    assert_eq!(result, Err(Ok(Error::StreamNotStarted)));
}

// ── Cancel ────────────────────────────────────────────────────────────────────

#[test]
fn cancel_before_start_refunds_full_deposit() {
    let s = Setup::new(100, 3600, false);
    let deposit = 100 * 3600;
    let sender_before = s.token.balance(&s.sender);
    s.client.cancel(&s.sender);
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
    s.client.cancel(&s.sender);
    // Recipient gets 1800 × 100 = 180_000 (earned but not withdrawn)
    // Sender gets 180_000 refund
    assert_eq!(s.token.balance(&s.recipient) - recipient_before, 180_000);
    assert_eq!(s.token.balance(&s.sender) - sender_before, 180_000);
}

#[test]
fn cancel_advances_withdrawn_by_the_payout() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(1800); // halfway → 180_000 earned, none pulled via withdraw()
    s.client.cancel(&s.sender);

    // `cancel` paid the recipient 180_000 directly. Post-cancel `withdrawn`
    // must reflect what the recipient received, not just withdraw() pulls.
    assert_eq!(s.client.info().withdrawn, 180_000);
}

#[test]
fn cancel_advances_withdrawn_on_top_of_prior_withdrawals() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(1800); // 180_000 earned
    let pulled = s.client.withdraw(&50_000);
    assert_eq!(pulled, 50_000);
    s.client.cancel(&s.sender);

    // 50_000 via withdraw() + 130_000 paid out by cancel = 180_000 total.
    assert_eq!(s.client.info().withdrawn, 180_000);
}

#[test]
fn cancel_then_cancel_panics() {
    let s = Setup::new(100, 3600, false);
    s.client.cancel(&s.sender);
    let result = s.client.try_cancel(&s.sender);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn withdraw_after_cancel_panics() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    s.client.cancel(&s.sender);
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
    let reclaimed = s.client.clawback(&s.sender);
    // reclaimed = total_balance − owed = (100×3600) − 60_000 = 300_000
    assert_eq!(reclaimed, 300_000);
    assert_eq!(s.token.balance(&s.sender) - sender_before, 300_000);
}

#[test]
fn clawback_disabled_panics() {
    let s = Setup::new(100, 3600, false);
    let result = s.client.try_clawback(&s.sender);
    assert_eq!(result, Err(Ok(Error::ClawbackDisabled)));
}

// ── Top-up ────────────────────────────────────────────────────────────────────

#[test]
fn top_up_increases_contract_balance() {
    let s = Setup::new(100, 3600, false);
    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &50_000);

    let stream_before = s.token.balance(&s.client.address);
    s.client.top_up(&s.sender, &50_000);
    assert_eq!(s.token.balance(&s.client.address), stream_before + 50_000);
}

#[test]
fn top_up_on_cancelled_stream_is_rejected() {
    let s = Setup::new(100, 3600, false);
    s.client.cancel(&s.sender);

    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &10_000);

    let result = s.client.try_top_up(&s.sender, &10_000);
    assert!(result.is_err());
}

#[test]
fn top_up_rejects_zero_and_negative_amount() {
    let s = Setup::new(100, 3600, false);
    assert_eq!(
        s.client.try_top_up(&s.sender, &0),
        Err(Ok(Error::InvalidAmount))
    );
    assert_eq!(
        s.client.try_top_up(&s.sender, &-1),
        Err(Ok(Error::InvalidAmount))
    );
}

#[test]
fn withdraw_rejects_zero_and_negative_amount() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    assert_eq!(s.client.try_withdraw(&0), Err(Ok(Error::InvalidAmount)));
    assert_eq!(s.client.try_withdraw(&-1), Err(Ok(Error::InvalidAmount)));
}

// ── Empty-stream guard ───────────────────────────────────────────────────────

/// Deploy a bare DripStream (bypassing the factory — allowed per ADR-001,
/// one contract per stream) and attempt to initialize it with a zero rate.
/// Such a stream would escrow tokens but never release any ("empty
/// stream") and must be rejected at initialization time with
/// `InvalidAmount` (error #15).
#[test]
#[should_panic(expected = "Error(Contract, #15)")]
fn initialize_rejects_zero_rate() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    let now: u64 = 1_000_000;
    client.initialize(
        &sender,
        &recipient,
        &token_addr,
        &0, // rate_per_second = 0 → empty stream
        &now,
        &(now + 3_600),
        &false,
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #15)")]
fn initialize_rejects_negative_rate() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    let now: u64 = 1_000_000;
    client.initialize(
        &sender,
        &recipient,
        &token_addr,
        &-1, // negative rate → empty stream
        &now,
        &(now + 3_600),
        &false,
    );
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

// ── Time-range boundary guard (issue #81) ────────────────────────────────────

/// A bounded stream whose `end_time` is *before* `start_time` is malformed.
/// `initialize()` must reject it at the boundary with `InvalidTimeRange`
/// (error #8) before any state is persisted — otherwise the escrowed balance
/// gets permanently locked (see `malformed_time_range_would_lock_funds`).
#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn initialize_rejects_end_time_before_start() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    let now: u64 = 1_000_000;
    client.initialize(
        &sender,
        &recipient,
        &token_addr,
        &100,
        &now,           // start_time
        &(now - 3_600), // end_time BEFORE start_time → malformed
        &false,
    );
}

/// A zero-duration bounded stream (`end_time == start_time`) releases nothing
/// yet still escrows tokens — the same "empty stream" class the zero-rate
/// guard already rejects. It must fail with `InvalidTimeRange` (error #8).
#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn initialize_rejects_end_time_equal_start() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    let now: u64 = 1_000_000;
    client.initialize(
        &sender,
        &recipient,
        &token_addr,
        &100,
        &now, // start_time
        &now, // end_time == start_time → zero-duration, malformed
        &false,
    );
}

/// The guard must NOT reject legitimate open-ended streams (`end_time == 0`).
/// This is the regression fence around the boundary check: `0` is a sentinel
/// for "no end", not a time that precedes `start_time`.
#[test]
fn initialize_accepts_open_ended_stream() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    let now: u64 = 1_000_000;
    client.initialize(
        &sender,
        &recipient,
        &token_addr,
        &100,
        &now,
        &0, // open-ended → valid
        &false,
    );

    let inf = client.info();
    assert_eq!(inf.end_time, 0);
    assert_eq!(inf.start_time, now);
}

/// Documents the exact failure mode the boundary check prevents.
///
/// We inject malformed state (`end_time < start_time`) directly through the
/// legacy per-field storage keys, bypassing `initialize()`'s guard, then show
/// that once ledger time passes `start_time` the release math underflows and
/// surfaces `ArithmeticOverflow`. In a real deployment that same error fires
/// inside `withdraw`, `cancel`, and `clawback`, so the escrow could be neither
/// paid out nor refunded — the funds would be locked forever. The guard in
/// `initialize()` makes this state unreachable in the first place.
#[test]
fn malformed_time_range_would_lock_funds() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let start_time: u64 = 1_000_000;
    let end_time: u64 = start_time - 3_600; // end BEFORE start — malformed

    let stream_id = env.register_contract(None, DripStream);

    // Inject the malformed state directly via the legacy per-field keys.
    // `state::load` reconstructs from these when no `Config` key is present,
    // so this reproduces exactly what a pre-fix `initialize()` would persist.
    env.as_contract(&stream_id, || {
        let storage = env.storage().instance();
        storage.set(&DataKey::Sender, &sender);
        storage.set(&DataKey::Recipient, &recipient);
        storage.set(&DataKey::Token, &token_addr);
        storage.set(&DataKey::RatePerSecond, &100_i128);
        storage.set(&DataKey::StartTime, &start_time);
        storage.set(&DataKey::EndTime, &end_time);
        storage.set(&DataKey::Withdrawn, &0_i128);
        storage.set(&DataKey::PausedAt, &0_u64);
        storage.set(&DataKey::Flags, &0_u32);
    });

    // Advance ledger past start_time so the release math actually runs.
    env.ledger().set(LedgerInfo {
        timestamp: start_time + 10,
        protocol_version: 21,
        sequence_number: 1,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 16,
        min_persistent_entry_ttl: 4096,
        max_entry_ttl: 6_312_000,
    });

    let result = env.as_contract(&stream_id, || {
        let info = crate::state::load(&env);
        crate::math::streamed_amount(&env, &info)
    });

    // The trap: the settlement math the whole contract depends on is bricked.
    assert_eq!(result, Err(Error::ArithmeticOverflow));
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

    s.client.cancel(&s.sender);
    assert_eq!(s.client.withdrawable(), 0);
}

#[test]
fn pause_then_cancel_refunds_correctly() {
    let s = Setup::new(100, 3600, false);
    let deposit = 100 * 3600; // 360_000

    s.advance_secs(600); // 60_000 streamed
    s.client.pause(&s.sender);
    s.advance_secs(1_000); // time passes; not counted

    let sender_before = s.token.balance(&s.sender);
    let recipient_before = s.token.balance(&s.recipient);
    s.client.cancel(&s.sender);

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
    assert!(!inf.is_paused());
    assert!(!inf.is_cancelled());
    assert!(inf.is_clawback_enabled());
    assert_eq!(inf.withdrawn, 0);
}

#[test]
fn info_reflects_pause_state() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    s.client.pause(&s.sender);

    let inf = s.client.info();
    assert!(inf.is_paused());
    assert!(inf.paused_at > 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #18)")]
fn uninitialized_stream_info_returns_not_initialized() {
    let env = Env::default();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    client.info();
}

#[test]
#[should_panic(expected = "Error(Contract, #18)")]
fn uninitialized_stream_withdrawable_returns_not_initialized() {
    let env = Env::default();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    client.withdrawable();
}

#[test]
#[should_panic(expected = "Error(Contract, #18)")]
fn uninitialized_stream_streamed_total_returns_not_initialized() {
    let env = Env::default();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    client.streamed_total();
}

#[test]
fn uninitialized_stream_mutations_return_not_initialized() {
    let env = Env::default();
    env.mock_all_auths();

    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);
    let caller = Address::generate(&env);

    assert_eq!(client.try_withdraw(&1), Err(Ok(Error::NotInitialized)));
    assert_eq!(client.try_cancel(&caller), Err(Ok(Error::NotInitialized)));
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

// ── Event delivery recovery ─────────────────────────────────────────────────

#[test]
fn delayed_consumer_retains_payloads_and_can_detect_sequence_gaps() {
    let s = Setup::new(100, 3_600, false);

    // Simulate rapid state changes while a consumer is disconnected. The
    // consumer reads the committed events only after all mutations complete.
    s.advance_secs(10);
    let paused_at = s.env.ledger().timestamp();
    s.client.pause(&s.sender);

    s.advance_secs(5);
    let resumed_at = s.env.ledger().timestamp();
    s.client.resume(&s.sender);

    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &500);
    s.client.top_up(&s.sender, &500);
    let balance_after_top_up = s.token.balance(&s.client.address);

    assert_eq!(s.client.event_sequence(), 4);

    let all_events = s.env.events().all();
    let stream_events: std::vec::Vec<_> = all_events
        .iter()
        .filter(|(contract, _, _)| contract == &s.client.address)
        .collect();

    assert_eq!(stream_events.len(), 4);

    // Event topics come back as `Vec<Val>` (which implements `PartialEq`) so
    // they can be compared directly. Event *data*, however, is a raw `Val`,
    // which deliberately has no `PartialEq` — comparing two `Val`s directly is
    // a compile error. Decode each data payload back into concrete Rust types
    // and compare those instead.
    //
    // `stream_events[0]` is the `created` event emitted by `initialize()` in
    // `Setup::new`, occupying sequence 1.
    assert_eq!(
        stream_events[1].1,
        (symbol_short!("paused"), s.sender.clone(), 2_u64).into_val(&s.env)
    );
    let paused_data: (u64, i128) = stream_events[1].2.try_into_val(&s.env).unwrap();
    assert_eq!(paused_data, (paused_at, 1_000_i128));

    assert_eq!(
        stream_events[2].1,
        (symbol_short!("resumed"), s.sender.clone(), 3_u64).into_val(&s.env)
    );
    let resumed_data: u64 = stream_events[2].2.try_into_val(&s.env).unwrap();
    assert_eq!(resumed_data, resumed_at);

    assert_eq!(
        stream_events[3].1,
        (symbol_short!("topped_up"), s.sender.clone(), 4_u64).into_val(&s.env)
    );
    let topped_up_data: (i128, i128) = stream_events[3].2.try_into_val(&s.env).unwrap();
    assert_eq!(topped_up_data, (500_i128, balance_after_top_up));
}

// ── Extend duration ─────────────────────────────────────────────────────────

#[test]
fn extend_duration_success() {
    let s = Setup::new(100, 3_600, false);
    let before_end = s.client.info().end_time;

    // Mint exact deposit needed to extend by 100s (100 × 100 = 10_000)
    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &10_000);

    let contract_before = s.token.balance(&s.client.address);
    s.client.extend_duration(&s.sender, &100);

    assert_eq!(s.client.info().end_time, before_end + 100);
    assert_eq!(s.token.balance(&s.client.address), contract_before + 10_000);
}

#[test]
fn extend_duration_rejected_for_open_ended() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let now: u64 = 1_000_000;
    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    client.initialize(&sender, &recipient, &token_addr, &100, &now, &0, &false);

    let result = client.try_extend_duration(&sender, &100);
    assert_eq!(result, Err(Ok(Error::InvalidTimeRange)));
}

#[test]
fn extend_duration_rejects_on_arithmetic_overflow() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let now: u64 = 1_000_000;
    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    // Use an extremely large rate so (rate × 2) overflows i128
    let huge_rate: i128 = i128::MAX;
    client.initialize(
        &sender,
        &recipient,
        &token_addr,
        &huge_rate,
        &now,
        &(now + 10),
        &false,
    );

    let result = client.try_extend_duration(&sender, &2);
    assert_eq!(result, Err(Ok(Error::ArithmeticOverflow)));
}

#[test]
fn legacy_storage_layout_still_loads_and_tracks_state() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let now: u64 = 1_000_000;
    let stream_id = env.register_contract(None, DripStream);

    env.as_contract(&stream_id, || {
        let storage = env.storage().instance();
        storage.set(&DataKey::Sender, &sender);
        storage.set(&DataKey::Recipient, &recipient);
        storage.set(&DataKey::Token, &token_addr);
        storage.set(&DataKey::RatePerSecond, &100_i128);
        storage.set(&DataKey::StartTime, &now);
        storage.set(&DataKey::EndTime, &(now + 3_600));
        storage.set(&DataKey::Withdrawn, &0_i128);
        storage.set(&DataKey::PausedAt, &0_u64);
        storage.set(&DataKey::Flags, &0_u32);
    });

    let info = env.as_contract(&stream_id, || crate::state::load(&env));
    assert_eq!(info.rate_per_second, 100);
    assert!(!info.is_paused());
    assert!(!info.is_cancelled());
    assert!(!info.is_clawback_enabled());
}

// ── Cancellation CEI / settlement invariants (issue #78) ──────────────────────
//
// Issue #78 alleged a reentrancy drain in `cancel_batch_streams`. No such
// function exists in this workspace — the factory only exposes
// `create_batch_streams`, and cancellation lives entirely in this contract as
// `cancel()` / `force_cancel()`. Both already commit the cancelled flag before
// any `token::transfer`, which is the ordering the issue asked for.
//
// The specific attack described (a token contract re-entering the stream from
// a transfer callback) is not expressible on Soroban: the host refuses to
// re-enter a contract that is already on the call stack, and SEP-41 tokens
// have no receiver hooks to fire in the first place. So there is no
// `test_reentrancy_on_batch_cancel` to write — a test cannot construct the
// precondition.
//
// What IS worth locking in is the property that made the attack impossible:
// after cancellation, the contract holds no balance AND is durably marked
// cancelled, so there is no state in which a second settlement could pay out
// again. These tests pin that down so a future refactor cannot silently move a
// transfer ahead of the state write, or leave a re-drainable remainder behind.

/// `cancel()` must leave zero balance in the contract and a durably-set
/// cancelled flag. If a transfer were ever moved ahead of `state::save`, an
/// interleaved settlement would observe `is_cancelled() == false` while the
/// balance was still non-zero — this asserts that window is closed.
#[test]
fn cancel_commits_state_and_drains_balance() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(1800);

    assert!(!s.client.info().is_cancelled());
    assert!(s.token.balance(&s.client.address) > 0);

    s.client.cancel(&s.sender);

    assert!(s.client.info().is_cancelled());
    assert_eq!(s.token.balance(&s.client.address), 0);
}

/// Value conservation across `cancel()`: every stroop the contract held is
/// accounted for by exactly one payout, and nothing is left to drain twice.
#[test]
fn cancel_conserves_value_and_leaves_nothing_to_redrain() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(900);

    let escrowed = s.token.balance(&s.client.address);
    let sender_before = s.token.balance(&s.sender);
    let recipient_before = s.token.balance(&s.recipient);

    s.client.cancel(&s.sender);

    let paid_to_sender = s.token.balance(&s.sender) - sender_before;
    let paid_to_recipient = s.token.balance(&s.recipient) - recipient_before;

    assert_eq!(paid_to_sender + paid_to_recipient, escrowed);
    assert_eq!(s.token.balance(&s.client.address), 0);
}

/// Same invariant for the recipient-initiated escape hatch.
#[test]
fn force_cancel_commits_state_and_drains_balance() {
    // 60-day stream so the 30-day pause threshold is reached well before
    // end_time — otherwise `streamed_amount` clamps to end_time and the
    // pause branch never applies.
    let s = Setup::new(100, 5_184_000, false);
    s.advance_secs(1_000);
    s.client.pause(&s.sender);
    s.advance_secs(2_592_001); // 30 days + 1s

    let escrowed = s.token.balance(&s.client.address);
    let sender_before = s.token.balance(&s.sender);
    let recipient_before = s.token.balance(&s.recipient);

    s.client.force_cancel();

    assert!(s.client.info().is_cancelled());
    assert_eq!(s.token.balance(&s.client.address), 0);

    let paid_to_sender = s.token.balance(&s.sender) - sender_before;
    let paid_to_recipient = s.token.balance(&s.recipient) - recipient_before;
    // Only the 1_000s streamed before the pause is owed to the recipient.
    assert_eq!(paid_to_recipient, 100_000);
    assert_eq!(paid_to_sender + paid_to_recipient, escrowed);
    // `withdrawn` reflects the tokens the recipient received in force_cancel.
    assert_eq!(s.client.info().withdrawn, 100_000);
}

/// Every value-moving entry point must reject an already-cancelled stream.
/// This is the guard that makes a second settlement impossible regardless of
/// how the caller reaches it.
#[test]
fn all_settlement_paths_rejected_after_cancel() {
    let s = Setup::new(100, 3600, true); // clawback enabled
    s.advance_secs(900);
    s.client.cancel(&s.sender);

    assert_eq!(
        s.client.try_cancel(&s.sender),
        Err(Ok(Error::StreamCancelled))
    );
    assert_eq!(s.client.try_force_cancel(), Err(Ok(Error::StreamCancelled)));
    assert_eq!(
        s.client.try_clawback(&s.sender),
        Err(Ok(Error::StreamCancelled))
    );
    assert_eq!(s.client.try_withdraw(&1), Err(Ok(Error::StreamCancelled)));
    assert_eq!(s.token.balance(&s.client.address), 0);
}

/// Mirror of the above, entered through `force_cancel()` instead.
#[test]
fn all_settlement_paths_rejected_after_force_cancel() {
    let s = Setup::new(100, 5_184_000, true); // clawback enabled
    s.advance_secs(1_000);
    s.client.pause(&s.sender);
    s.advance_secs(2_592_001);
    s.client.force_cancel();

    assert_eq!(
        s.client.try_cancel(&s.sender),
        Err(Ok(Error::StreamCancelled))
    );
    assert_eq!(s.client.try_force_cancel(), Err(Ok(Error::StreamCancelled)));
    assert_eq!(
        s.client.try_clawback(&s.sender),
        Err(Ok(Error::StreamCancelled))
    );
    assert_eq!(s.client.try_withdraw(&1), Err(Ok(Error::StreamCancelled)));
    assert_eq!(s.token.balance(&s.client.address), 0);
}

/// Partial withdrawals before cancellation must not let the recipient be paid
/// twice for the same streamed seconds.
#[test]
fn cancel_after_partial_withdrawal_does_not_double_pay() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(1800); // 180_000 streamed

    s.client.withdraw(&100_000);

    let escrowed = s.token.balance(&s.client.address);
    let sender_before = s.token.balance(&s.sender);
    let recipient_before = s.token.balance(&s.recipient);

    s.client.cancel(&s.sender);

    let paid_to_recipient = s.token.balance(&s.recipient) - recipient_before;
    let paid_to_sender = s.token.balance(&s.sender) - sender_before;

    // Recipient is owed only the 80_000 not already withdrawn.
    assert_eq!(paid_to_recipient, 80_000);
    assert_eq!(paid_to_sender, 180_000);
    assert_eq!(paid_to_sender + paid_to_recipient, escrowed);
    assert_eq!(s.token.balance(&s.client.address), 0);
}

/// The cancelled flag must survive as committed state, not just as a value in
/// the cancelling invocation's memory — a later, independent invocation has to
/// observe it.
#[test]
fn cancelled_flag_is_durable_across_invocations() {
    let s = Setup::new(100, 3600, false);
    s.client.cancel(&s.sender);

    let persisted = s
        .env
        .as_contract(&s.client.address, || crate::state::load(&s.env));
    assert!(persisted.is_cancelled());

    assert_eq!(s.client.withdrawable(), 0);
    assert_eq!(s.client.streamed_total(), 0);
}

// ── Issue #205: top_up_and_extend convenience ────────────────────────────────

#[test]
fn top_up_and_extend_updates_balance_and_end_time() {
    let s = Setup::new(100, 3_600, false);
    let before_end = s.client.info().end_time;

    // Mint exact deposit needed: 100 rate × 200s = 20_000
    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &20_000);

    let contract_before = s.token.balance(&s.client.address);
    s.client.top_up_and_extend(&s.sender, &20_000, &200);

    assert_eq!(s.client.info().end_time, before_end + 200);
    assert_eq!(s.token.balance(&s.client.address), contract_before + 20_000);
}

#[test]
fn top_up_and_extend_rejects_zero_amount() {
    let s = Setup::new(100, 3_600, false);
    let result = s.client.try_top_up_and_extend(&s.sender, &0, &100);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn top_up_and_extend_rejects_zero_extra_time() {
    let s = Setup::new(100, 3_600, false);
    let result = s.client.try_top_up_and_extend(&s.sender, &10_000, &0);
    assert_eq!(result, Err(Ok(Error::InvalidTimeRange)));
}

#[test]
fn top_up_and_extend_rejected_on_cancelled_stream() {
    let s = Setup::new(100, 3_600, false);
    s.client.cancel(&s.sender);

    let token_admin = token::StellarAssetClient::new(&s.env, &s.token.address);
    token_admin.mint(&s.sender, &10_000);

    let result = s.client.try_top_up_and_extend(&s.sender, &10_000, &100);
    assert!(result.is_err());
}

#[test]
fn top_up_and_extend_rejected_for_open_ended_stream() {
    let env = Env::default();
    env.mock_all_auths();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();

    let now: u64 = 1_000_000;
    let stream_id = env.register_contract(None, DripStream);
    let client = DripStreamClient::new(&env, &stream_id);

    client.initialize(&sender, &recipient, &token_addr, &100, &now, &0, &false);

    let result = client.try_top_up_and_extend(&sender, &10_000, &100);
    assert_eq!(result, Err(Ok(Error::InvalidTimeRange)));
}

// ── Operator access control ─────────────────────────────────────────────────

#[test]
fn operator_returns_none_initially() {
    let s = Setup::new(100, 3600, false);
    assert_eq!(s.client.operator(), None);
}

#[test]
fn set_operator_sets_address() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    assert_eq!(s.client.operator(), Some(operator));
}

#[test]
fn set_operator_rejects_non_sender() {
    let s = Setup::new(100, 3600, false);
    let non_sender = Address::generate(&s.env);
    let operator = Address::generate(&s.env);
    let result = s.client.try_set_operator(&non_sender, &operator);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn set_operator_rejects_on_cancelled_stream() {
    let s = Setup::new(100, 3600, false);
    s.client.cancel(&s.sender);
    let operator = Address::generate(&s.env);
    let result = s.client.try_set_operator(&s.sender, &operator);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn set_operator_requires_revoke_before_replacement() {
    let s = Setup::new(100, 3600, false);
    let op1 = Address::generate(&s.env);
    let op2 = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &op1);
    assert_eq!(s.client.operator(), Some(op1.clone()));
    let result = s.client.try_set_operator(&s.sender, &op2);
    assert_eq!(result, Err(Ok(Error::OperatorAlreadySet)));
    assert_eq!(s.client.operator(), Some(op1));
}

#[test]
fn revoke_operator_removes_address() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    assert_eq!(s.client.operator(), Some(operator));
    s.client.revoke_operator(&s.sender);
    assert_eq!(s.client.operator(), None);
}

#[test]
fn revoke_operator_rejects_non_sender() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    let non_sender = Address::generate(&s.env);
    let result = s.client.try_revoke_operator(&non_sender);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoke_operator_rejects_on_cancelled_stream() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.cancel(&s.sender);
    let result = s.client.try_revoke_operator(&s.sender);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn operator_events_emit_correct_topic_shape_and_sequence() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);

    s.client.set_operator(&s.sender, &operator);
    s.client.revoke_operator(&s.sender);

    assert_eq!(s.client.event_sequence(), 3);

    let all_events = s.env.events().all();
    let stream_events: std::vec::Vec<_> = all_events
        .iter()
        .filter(|(contract, _, _)| contract == &s.client.address)
        .collect();

    assert_eq!(stream_events.len(), 3);

    // Event 1: set_op
    assert_eq!(
        stream_events[1].1,
        (symbol_short!("set_op"), s.sender.clone(), 2_u64).into_val(&s.env)
    );
    let set_op_data: Address = stream_events[1].2.try_into_val(&s.env).unwrap();
    assert_eq!(set_op_data, operator);

    // Event 2: rm_op with ((rm_op, sender, 3), ())
    assert_eq!(
        stream_events[2].1,
        (symbol_short!("rm_op"), s.sender.clone(), 3_u64).into_val(&s.env)
    );
    let rm_op_data: () = stream_events[2].2.try_into_val(&s.env).unwrap();
    assert_eq!(rm_op_data, ());
}

// ── Operator exercises sender-gated functions ──────────────────────────────

#[test]
fn operator_can_cancel() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);

    s.advance_secs(1800);
    let sender_before = s.token.balance(&s.sender);
    let recipient_before = s.token.balance(&s.recipient);
    s.client.cancel(&operator);

    assert!(s.client.info().is_cancelled());
    assert_eq!(s.token.balance(&s.recipient) - recipient_before, 180_000);
    assert_eq!(s.token.balance(&s.sender) - sender_before, 180_000);
}

#[test]
fn operator_can_pause() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);

    s.advance_secs(100);
    let before_pause = s.client.withdrawable();
    s.client.pause(&operator);

    assert!(s.client.info().is_paused());
    s.advance_secs(500);
    assert_eq!(s.client.withdrawable(), before_pause);
}

#[test]
fn operator_can_resume() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);

    s.advance_secs(100);
    s.client.pause(&operator);
    s.advance_secs(200);
    s.client.resume(&operator);
    s.advance_secs(50);

    // 150s of streaming = 15_000
    assert_eq!(s.client.withdrawable(), 15_000);
}

#[test]
fn operator_passes_auth_gate_for_top_up() {
    // top_up transfers tokens FROM the sender via tk.transfer(&sender, …),
    // which requires the sender's auth in a nested cross-contract call.
    // mock_all_auths() only covers root-level auth, so the full happy-path
    // cannot be exercised without explicit non-root auth entries. Instead,
    // we verify the operator clears the require_sender_or_operator gate:
    // a cancelled stream fails with StreamCancelled (not NotAuthorized),
    // proving the operator was accepted as authorized.
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.cancel(&s.sender);

    let result = s.client.try_top_up(&operator, &1_000);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn operator_passes_auth_gate_for_extend_duration() {
    // extend_duration transfers tokens FROM the sender via tk.transfer.
    // Same auth limitation as top_up — verify the operator clears the gate
    // by checking StreamCancelled (not NotAuthorized) on a cancelled stream.
    let s = Setup::new(100, 3_600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.cancel(&s.sender);

    let result = s.client.try_extend_duration(&operator, &100);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn operator_passes_auth_gate_for_top_up_and_extend() {
    // top_up_and_extend transfers tokens FROM the sender via tk.transfer.
    // Same auth limitation as top_up — verify the operator clears the gate
    // by checking StreamCancelled (not NotAuthorized) on a cancelled stream.
    let s = Setup::new(100, 3_600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.cancel(&s.sender);

    let result = s.client.try_top_up_and_extend(&operator, &1_000, &100);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn operator_can_clawback() {
    let s = Setup::new(100, 3600, true);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);

    s.advance_secs(600); // 60_000 owed to recipient
    let sender_before = s.token.balance(&s.sender);
    let reclaimed = s.client.clawback(&operator);

    assert_eq!(reclaimed, 300_000);
    assert_eq!(s.token.balance(&s.sender) - sender_before, 300_000);
}

// ── Non-sender/non-operator rejected by sender-gated functions ─────────────

#[test]
fn non_sender_non_operator_rejected_by_cancel() {
    let s = Setup::new(100, 3600, false);
    let rando = Address::generate(&s.env);
    let result = s.client.try_cancel(&rando);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn non_sender_non_operator_rejected_by_pause() {
    let s = Setup::new(100, 3600, false);
    let rando = Address::generate(&s.env);
    let result = s.client.try_pause(&rando);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn non_sender_non_operator_rejected_by_resume() {
    let s = Setup::new(100, 3600, false);
    s.client.pause(&s.sender);
    let rando = Address::generate(&s.env);
    let result = s.client.try_resume(&rando);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn non_sender_non_operator_rejected_by_top_up() {
    let s = Setup::new(100, 3600, false);
    let rando = Address::generate(&s.env);
    let result = s.client.try_top_up(&rando, &1_000);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn non_sender_non_operator_rejected_by_extend_duration() {
    let s = Setup::new(100, 3_600, false);
    let rando = Address::generate(&s.env);
    let result = s.client.try_extend_duration(&rando, &100);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn non_sender_non_operator_rejected_by_top_up_and_extend() {
    let s = Setup::new(100, 3_600, false);
    let rando = Address::generate(&s.env);
    let result = s.client.try_top_up_and_extend(&rando, &1_000, &100);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn non_sender_non_operator_rejected_by_clawback() {
    let s = Setup::new(100, 3600, true);
    let rando = Address::generate(&s.env);
    let result = s.client.try_clawback(&rando);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

// ── Revoked operator loses all delegated rights ────────────────────────────

#[test]
fn revoked_operator_rejected_by_cancel() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.revoke_operator(&s.sender);

    let result = s.client.try_cancel(&operator);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoked_operator_rejected_by_pause() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.revoke_operator(&s.sender);

    let result = s.client.try_pause(&operator);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoked_operator_rejected_by_top_up() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.revoke_operator(&s.sender);

    let result = s.client.try_top_up(&operator, &1_000);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoked_operator_rejected_by_clawback() {
    let s = Setup::new(100, 3600, true);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.revoke_operator(&s.sender);

    let result = s.client.try_clawback(&operator);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoked_operator_rejected_by_extend_duration() {
    let s = Setup::new(100, 3_600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.revoke_operator(&s.sender);

    let result = s.client.try_extend_duration(&operator, &100);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

#[test]
fn revoked_operator_rejected_by_top_up_and_extend() {
    let s = Setup::new(100, 3_600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);
    s.client.revoke_operator(&s.sender);

    let result = s.client.try_top_up_and_extend(&operator, &1_000, &100);
    assert_eq!(result, Err(Ok(Error::NotAuthorized)));
}

// ── Sender retains access after setting operator ───────────────────────────

#[test]
fn sender_still_can_cancel_after_setting_operator() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);

    s.advance_secs(900);
    s.client.cancel(&s.sender);
    assert!(s.client.info().is_cancelled());
}

#[test]
fn sender_still_can_pause_after_setting_operator() {
    let s = Setup::new(100, 3600, false);
    let operator = Address::generate(&s.env);
    s.client.set_operator(&s.sender, &operator);

    s.client.pause(&s.sender);
    assert!(s.client.info().is_paused());
}

// ── Operator cannot access recipient-only functions ────────────────────────

// withdraw() and transfer_recipient() are gated by info.recipient.require_auth(),
// not by require_sender_or_operator. They don't take a caller parameter — the
// auth is checked against the stored recipient address. An operator cannot
// satisfy this auth because they are not the recipient. This is already covered
// by the existing withdraw tests (which always pass recipient auth) and would
// require disabling mock_all_auths() to test negative cases properly.
// ── State storage: save() writes only Config; legacy keys are cleaned ─────────
//
// `save()` used to mirror the 9 individual per-field keys alongside the
// consolidated `Config` on every mutation, but `load()` reads `Config` on its
// fast path (always true for post-consolidation streams). Those mirrors were
// never read — pure write/rent overhead on every state mutation. `save()` now
// writes only `Config` and, on the first `save()` of a pre-consolidation
// stream, removes the legacy keys one-time.

#[test]
fn initialize_writes_only_config_and_not_legacy_keys() {
    let s = Setup::new(100, 3600, false);

    let (has_config, has_sender, has_withdrawn, has_flags) =
        s.env.as_contract(&s.client.address, || {
            let storage = s.env.storage().instance();
            (
                storage.has(&DataKey::Config),
                storage.has(&DataKey::Sender),
                storage.has(&DataKey::Withdrawn),
                storage.has(&DataKey::Flags),
            )
        });

    assert!(
        has_config,
        "consolidated Config must be written on initialize"
    );
    assert!(!has_sender, "legacy Sender key must not be written");
    assert!(!has_withdrawn, "legacy Withdrawn key must not be written");
    assert!(!has_flags, "legacy Flags key must not be written");

    // initialize() must still expose the full state via load()/info().
    let info = s
        .env
        .as_contract(&s.client.address, || crate::state::load(&s.env));
    assert_eq!(info.rate_per_second, 100);
    assert!(!info.is_clawback_enabled());
}

#[test]
fn state_mutation_writes_only_config_not_legacy_keys() {
    let s = Setup::new(100, 3600, false);
    s.advance_secs(100);
    s.client.withdraw(&10_000); // drives state::save

    let (has_config, has_sender, has_withdrawn) = s.env.as_contract(&s.client.address, || {
        let storage = s.env.storage().instance();
        (
            storage.has(&DataKey::Config),
            storage.has(&DataKey::Sender),
            storage.has(&DataKey::Withdrawn),
        )
    });
    assert!(has_config, "Config must be present after a mutation");
    assert!(
        !has_sender,
        "mutation must not resurrect the legacy Sender key"
    );
    assert!(
        !has_withdrawn,
        "mutation must not resurrect the legacy Withdrawn key"
    );

    // The mutation's effect must still be durable through Config.
    let info = s
        .env
        .as_contract(&s.client.address, || crate::state::load(&s.env));
    assert_eq!(info.withdrawn, 10_000);
}

#[test]
fn save_migrates_legacy_keys_to_config_once() {
    let env = Env::default();
    env.mock_all_auths();
    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let stream_id = env.register_contract(None, DripStream);

    // Simulate a pre-consolidation stream: only the per-field keys exist.
    env.as_contract(&stream_id, || {
        let storage = env.storage().instance();
        storage.set(&DataKey::Sender, &sender);
        storage.set(&DataKey::Recipient, &recipient);
        storage.set(&DataKey::Token, &token_addr);
        storage.set(&DataKey::RatePerSecond, &100_i128);
        storage.set(&DataKey::StartTime, &1_000_000_u64);
        storage.set(&DataKey::EndTime, &1_003_600_u64);
        storage.set(&DataKey::Withdrawn, &0_i128);
        storage.set(&DataKey::PausedAt, &0_u64);
        storage.set(&DataKey::Flags, &0_u32);
        assert!(!storage.has(&DataKey::Config));
    });

    // First save() supersedes the legacy keys and writes Config.
    env.as_contract(&stream_id, || {
        crate::state::save(
            &env,
            &crate::storage::StreamInfo {
                sender: sender.clone(),
                recipient: recipient.clone(),
                token: token_addr.clone(),
                rate_per_second: 100,
                start_time: 1_000_000,
                end_time: 1_003_600,
                withdrawn: 0,
                paused_at: 0,
                flags: 0,
            },
        );
    });

    // Legacy keys removed, Config present, load() returns the right state.
    env.as_contract(&stream_id, || {
        let storage = env.storage().instance();
        assert!(storage.has(&DataKey::Config));
        assert!(!storage.has(&DataKey::Sender));
        assert!(!storage.has(&DataKey::Recipient));
        assert!(!storage.has(&DataKey::Withdrawn));
        assert!(!storage.has(&DataKey::Flags));

        let info = crate::state::load(&env);
        assert_eq!(info.rate_per_second, 100);
        assert_eq!(info.sender, sender);
        assert_eq!(info.recipient, recipient);
    });
}
