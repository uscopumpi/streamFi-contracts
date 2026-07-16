use soroban_sdk::Env;

use crate::errors::Error;
use crate::storage::StreamInfo;

/// Returns the total tokens that have streamed up to `now`,
/// excluding any paused time. Does not account for withdrawals.
pub fn streamed_amount(env: &Env, info: &StreamInfo) -> Result<i128, Error> {
    let now = env.ledger().timestamp();

    // Stream has not started yet
    if now < info.start_time {
        return Ok(0);
    }

    // Clamp to end_time if set
    let effective_now = if info.end_time > 0 && now > info.end_time {
        info.end_time
    } else if info.paused {
        info.paused_at
    } else {
        now
    };

    let elapsed = effective_now
        .checked_sub(info.start_time)
        .ok_or(Error::ArithmeticOverflow)? as i128;

    info.rate_per_second
        .checked_mul(elapsed)
        .ok_or(Error::ArithmeticOverflow)
}

/// Returns tokens available for the recipient to withdraw right now.
///
/// Guards against the case where `info.withdrawn` could theoretically
/// exceed `streamed` (e.g. if ledger time skews or a top-up changes
/// effective balance) by clamping to zero rather than returning an error.
pub fn withdrawable(env: &Env, info: &StreamInfo) -> Result<i128, Error> {
    let streamed = streamed_amount(env, info)?;
    let available = streamed.saturating_sub(info.withdrawn);
    Ok(available.max(0))
}
