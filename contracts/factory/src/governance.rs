use drip_governor::{DripGovernorClient, GovernorConfig};
use soroban_sdk::{Address, Env};

use crate::Error;

/// Fetches the live protocol config from `governor` via a cross-contract call.
///
/// Returns `Err(GovernorNotResponding)` when the cross-contract call fails
/// (governor archived / not initialised / host error) instead of letting the
/// host trap bubble up as an opaque error.
pub fn config(env: &Env, governor: &Address) -> Result<GovernorConfig, Error> {
    // Cross-contract call — flattens the nested Result from try_config()
    // (outer = host error, inner = governor contract error) so callers see
    // a meaningful `GovernorNotResponding` instead of an opaque host trap.
    let result = DripGovernorClient::new(env, governor)
        .try_config()
        .map_err(|_| Error::GovernorNotResponding)?;
    result.map_err(|_| Error::GovernorNotResponding)
}

/// Enforces the governor-controlled rate/duration bounds on a new stream.
///
/// `rate_per_sec` and, for fixed-duration streams, the declared length must
/// respect the protocol parameters DripGovernor holds.
pub fn enforce_bounds(
    config: &GovernorConfig,
    rate_per_sec: i128,
    start_time: u64,
    end_time: u64,
) -> Result<(), Error> {
    if rate_per_sec > config.max_rate_per_second {
        return Err(Error::RateExceedsMax);
    }
    if end_time > 0 {
        let duration = end_time - start_time;
        if duration < config.min_duration_seconds {
            return Err(Error::DurationTooShort);
        }
        if duration > config.max_duration_seconds {
            return Err(Error::DurationExceedsMax);
        }
    }
    Ok(())
}
