use soroban_sdk::{symbol_short, Address, Env};

pub fn withdrawn(
    env: &Env,
    recipient: &Address,
    amount: i128,
    total_withdrawn: i128,
    remaining: i128,
) {
    env.events().publish(
        (symbol_short!("withdrawn"), recipient.clone()),
        (amount, total_withdrawn, remaining),
    );
}

pub fn cancelled(env: &Env, sender: &Address, refund_amount: i128, withdrawn_so_far: i128) {
    env.events().publish(
        (symbol_short!("cancelled"), sender.clone()),
        (refund_amount, withdrawn_so_far),
    );
}

pub fn paused(env: &Env, sender: &Address, paused_at: u64, withdrawable: i128) {
    env.events().publish(
        (symbol_short!("paused"), sender.clone()),
        (paused_at, withdrawable),
    );
}

pub fn resumed(env: &Env, sender: &Address, resumed_at: u64) {
    env.events()
        .publish((symbol_short!("resumed"), sender.clone()), resumed_at);
}

pub fn topped_up(env: &Env, sender: &Address, amount: i128, new_balance: i128) {
    env.events().publish(
        (symbol_short!("topped_up"), sender.clone()),
        (amount, new_balance),
    );
}

pub fn clawback(env: &Env, sender: &Address, amount: i128) {
    env.events()
        .publish((symbol_short!("clawback"), sender.clone()), amount);
}

pub fn recipient_transferred(env: &Env, old_recipient: &Address, new_recipient: &Address) {
    env.events().publish(
        (symbol_short!("xfer_rec"), old_recipient.clone()),
        new_recipient.clone(),
    );
}
