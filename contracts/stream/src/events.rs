use soroban_sdk::{panic_with_error, symbol_short, Address, Env};

use crate::{storage::DataKey, Error};

/// Allocate the next event sequence before publishing its payload.
///
/// Event publication and storage writes are part of the same Soroban
/// transaction, so either both commit or both are rolled back. Existing
/// streams that predate this key start at sequence zero.
fn next_sequence(env: &Env) -> u64 {
    let storage = env.storage().instance();
    let current = storage.get::<_, u64>(&DataKey::EventSequence).unwrap_or(0);
    let Some(next) = current.checked_add(1) else {
        panic_with_error!(env, Error::ArithmeticOverflow);
    };
    storage.set(&DataKey::EventSequence, &next);
    next
}

pub fn withdrawn(
    env: &Env,
    recipient: &Address,
    amount: i128,
    total_withdrawn: i128,
    remaining: i128,
) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("withdrawn"), recipient.clone(), sequence),
        (amount, total_withdrawn, remaining),
    );
}

pub fn cancelled(env: &Env, sender: &Address, refund_amount: i128, withdrawn_so_far: i128) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("cancelled"), sender.clone(), sequence),
        (refund_amount, withdrawn_so_far),
    );
}

pub fn paused(env: &Env, sender: &Address, paused_at: u64, withdrawable: i128) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("paused"), sender.clone(), sequence),
        (paused_at, withdrawable),
    );
}

pub fn resumed(env: &Env, sender: &Address, resumed_at: u64) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("resumed"), sender.clone(), sequence),
        resumed_at,
    );
}

pub fn topped_up(env: &Env, sender: &Address, amount: i128, new_balance: i128) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("topped_up"), sender.clone(), sequence),
        (amount, new_balance),
    );
}

pub fn clawback(env: &Env, sender: &Address, amount: i128) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("clawback"), sender.clone(), sequence),
        amount,
    );
}

pub fn recipient_transferred(env: &Env, old_recipient: &Address, new_recipient: &Address) {
    let sequence = next_sequence(env);
    env.events().publish(
        (symbol_short!("xfer_rec"), old_recipient.clone(), sequence),
        new_recipient.clone(),
    );
}
