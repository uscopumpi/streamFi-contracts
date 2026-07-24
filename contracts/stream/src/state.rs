use soroban_sdk::Env;

use crate::storage::{DataKey, StreamInfo, FLAG_CANCELLED, FLAG_CLAWBACK_ENABLED, FLAG_PAUSED};
use crate::Error;

/// Load the full stream state in a single storage read.
///
/// Tries the consolidated `Config` key first (written by all new
/// `initialize()` calls). Falls back to reading each field individually
/// for streams that were initialized before this optimisation landed —
/// this keeps older on-chain instances readable without a migration.
pub fn load(env: &Env) -> StreamInfo {
    let s = env.storage().instance();

    // Fast path: stream was initialized with the consolidated key.
    if s.has(&DataKey::Config) {
        if let Some(info) = s.get::<_, StreamInfo>(&DataKey::Config) {
            return info;
        }
    }

    // Legacy path: read each field individually (pre-optimisation streams).
    // Reconstructs the packed `flags` bitfield from the old dedicated
    // ClawbackEnabled/Cancelled keys plus the existing Flags key (which
    // already held the Paused bit before the single-key consolidation).
    let mut flags: u32 = s.get(&DataKey::Flags).unwrap_or(0);
    if s.get(&DataKey::ClawbackEnabled).unwrap_or(false) {
        flags |= FLAG_CLAWBACK_ENABLED;
    }
    if s.get(&DataKey::Cancelled).unwrap_or(false) {
        flags |= FLAG_CANCELLED;
    }

    StreamInfo {
        sender: s.get(&DataKey::Sender).unwrap(),
        recipient: s.get(&DataKey::Recipient).unwrap(),
        token: s.get(&DataKey::Token).unwrap(),
        rate_per_second: s.get(&DataKey::RatePerSecond).unwrap(),
        start_time: s.get(&DataKey::StartTime).unwrap(),
        end_time: s.get(&DataKey::EndTime).unwrap(),
        withdrawn: s.get(&DataKey::Withdrawn).unwrap_or(0),
        paused_at: s.get(&DataKey::PausedAt).unwrap_or(0),
        flags,
    }
}

/// Persist the entire stream state and keep the legacy individual keys in sync.
pub fn save(env: &Env, info: &StreamInfo) {
    let s = env.storage().instance();
    s.set(&DataKey::Config, info);
    s.set(&DataKey::Sender, &info.sender);
    s.set(&DataKey::Recipient, &info.recipient);
    s.set(&DataKey::Token, &info.token);
    s.set(&DataKey::RatePerSecond, &info.rate_per_second);
    s.set(&DataKey::StartTime, &info.start_time);
    s.set(&DataKey::EndTime, &info.end_time);
    s.set(&DataKey::Withdrawn, &info.withdrawn);
    s.set(&DataKey::PausedAt, &info.paused_at);
    s.set(&DataKey::Flags, &info.flags);
}

/// Update only the `withdrawn` counter without touching the other fields.
///
/// Uses load-modify-save so the single-struct layout is maintained.
pub fn save_withdrawn(env: &Env, amount: i128) {
    let mut info = load(env);
    info.withdrawn = amount;
    save(env, &info);
}

/// Mark the stream as paused or resumed.
///
/// No longer called by `DripStream::pause`/`resume` directly — those
/// methods now build an updated `StreamInfo` and call `state::save` once.
/// Retained as a re-export-able helper because the legacy dual-write flow
/// (the one this commit removed) referenced it; downstream call sites should
/// prefer building the struct in-line and calling `save` directly.
#[allow(dead_code)]
pub fn set_paused(env: &Env, paused: bool) {
    let mut info = load(env);
    if paused {
        info.flags |= FLAG_PAUSED;
    } else {
        info.flags &= !FLAG_PAUSED;
    }
    save(env, &info);
}

pub fn set_cancelled(env: &Env) {
    let mut info = load(env);
    info.flags |= FLAG_CANCELLED;
    save(env, &info);
}

pub fn assert_not_cancelled(info: &StreamInfo) -> Result<(), Error> {
    if info.is_cancelled() {
        Err(Error::StreamCancelled)
    } else {
        Ok(())
    }
}
