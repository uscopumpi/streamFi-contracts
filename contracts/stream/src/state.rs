use soroban_sdk::Env;

use crate::storage::{DataKey, StreamInfo};
use crate::Error;

pub fn load(env: &Env) -> StreamInfo {
    StreamInfo {
        sender: env.storage().instance().get(&DataKey::Sender).unwrap(),
        recipient: env.storage().instance().get(&DataKey::Recipient).unwrap(),
        token: env.storage().instance().get(&DataKey::Token).unwrap(),
        rate_per_second: env
            .storage()
            .instance()
            .get(&DataKey::RatePerSecond)
            .unwrap(),
        start_time: env.storage().instance().get(&DataKey::StartTime).unwrap(),
        end_time: env.storage().instance().get(&DataKey::EndTime).unwrap(),
        withdrawn: env
            .storage()
            .instance()
            .get(&DataKey::Withdrawn)
            .unwrap_or(0),
        paused: env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false),
        paused_at: env
            .storage()
            .instance()
            .get(&DataKey::PausedAt)
            .unwrap_or(0),
        clawback_enabled: env
            .storage()
            .instance()
            .get(&DataKey::ClawbackEnabled)
            .unwrap_or(false),
        cancelled: env
            .storage()
            .instance()
            .get(&DataKey::Cancelled)
            .unwrap_or(false),
    }
}

pub fn save_withdrawn(env: &Env, amount: i128) {
    env.storage().instance().set(&DataKey::Withdrawn, &amount);
}

pub fn assert_not_cancelled(info: &StreamInfo) -> Result<(), Error> {
    if info.cancelled {
        Err(Error::StreamCancelled)
    } else {
        Ok(())
    }
}
