use soroban_sdk::{contracttype, Address, Env, String};

/// Fixed-point scaling factor for rebate calculations.
/// Uses basis points system: 10_000 = 100%
#[allow(dead_code)]
const REBATE_BPS_SCALE: i128 = 10_000;

/// Seconds in a year for APY calculations (365.25 days * 24 hours * 60 minutes * 60 seconds)
#[allow(dead_code)]
const SECONDS_PER_YEAR: i128 = 31_557_600;

/// Maximum rebate rate in basis points (10% = 1000 bps) to prevent excessive yields
#[allow(dead_code)]
const MAX_REBATE_RATE_BPS: i128 = 1_000;

/// Minimum rebate calculation threshold to prevent dust accumulation
#[allow(dead_code)]
const MIN_REBATE_THRESHOLD: i128 = 100;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub struct YieldConfig {
    pub vault_address: Address,
    pub is_active: bool,
    pub accrued_yield: i128,
    /// Annual Percentage Yield in basis points (e.g., 500 = 5% APY)
    pub apy_bps: i128,
    /// Timestamp of last yield calculation
    pub last_updated: u64,
    /// Principal amount deposited in vault
    pub deposited_principal: i128,
}

/// Rebate calculation error types
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
#[allow(dead_code)]
pub enum RebateError {
    ArithmeticOverflow = 1,
    InvalidRate = 2,
    InsufficientPrincipal = 3,
}

#[allow(dead_code)]
impl From<RebateError> for soroban_sdk::Error {
    fn from(err: RebateError) -> Self {
        soroban_sdk::Error::from_contract_error(err as u32)
    }
}

#[allow(dead_code)]
pub fn deposit_to_vault(env: &Env, amount: i128) {
    // In a real implementation, this would make a cross-contract call to the yield vault
    env.events().publish(("YIELD", "DEPOSIT"), amount);
}

#[allow(dead_code)]
pub fn withdraw_from_vault(env: &Env, amount: i128) {
    // Calls vault to withdraw principal back into the stream contract
    env.events().publish(("YIELD", "WITHDRAW"), amount);
}

/// Calculate rebate using fixed-point arithmetic based on time elapsed and APY.
///
/// This function implements deterministic compound interest calculation using only
/// integer arithmetic to avoid floating-point non-determinism in smart contracts.
///
/// Formula: rebate = principal * (apy_bps / BPS_SCALE) * (time_elapsed / SECONDS_PER_YEAR)
///
/// For continuous compounding approximation with small time periods, we use simple interest
/// to maintain precision and avoid expensive exponential calculations.
///
/// # Parameters
/// - `env`: Soroban environment for accessing ledger time and events
/// - `principal`: Principal amount in vault (in stroops/base units)
/// - `apy_bps`: Annual Percentage Yield in basis points (e.g., 500 = 5%)
/// - `time_elapsed_seconds`: Time elapsed since last calculation
///
/// # Returns
/// - `Result<i128, RebateError>`: Calculated rebate amount or error
///
/// # Errors
/// - `ArithmeticOverflow`: If intermediate calculations would overflow
/// - `InvalidRate`: If APY exceeds maximum allowed rate
/// - `InsufficientPrincipal`: If principal is below minimum threshold
#[allow(dead_code)]
pub fn calculate_rebate_with_params(
    env: &Env,
    principal: i128,
    apy_bps: i128,
    time_elapsed_seconds: u64,
) -> Result<i128, RebateError> {
    // Validate parameters
    if !(0..=MAX_REBATE_RATE_BPS).contains(&apy_bps) {
        return Err(RebateError::InvalidRate);
    }

    if principal < MIN_REBATE_THRESHOLD {
        return Err(RebateError::InsufficientPrincipal);
    }

    // Convert time to i128 for arithmetic
    let time_elapsed = time_elapsed_seconds as i128;

    // Calculate rebate = principal * apy_bps * time_elapsed / (BPS_SCALE * SECONDS_PER_YEAR)
    // Reorder operations to maximize precision and minimize overflow risk

    // First multiply principal by apy_bps
    let principal_times_rate = principal
        .checked_mul(apy_bps)
        .ok_or(RebateError::ArithmeticOverflow)?;

    // Then multiply by time elapsed
    let numerator = principal_times_rate
        .checked_mul(time_elapsed)
        .ok_or(RebateError::ArithmeticOverflow)?;

    // Calculate denominator (BPS_SCALE * SECONDS_PER_YEAR)
    let denominator = REBATE_BPS_SCALE
        .checked_mul(SECONDS_PER_YEAR)
        .ok_or(RebateError::ArithmeticOverflow)?;

    // Final division
    let rebate = numerator / denominator;

    env.events().publish(
        ("REBATE", "CALCULATED"),
        (principal, apy_bps, time_elapsed_seconds, rebate),
    );

    Ok(rebate)
}

/// Calculate rebate using mock yield configuration.
///
/// In a production implementation, this would query the actual vault state
/// and use real principal/yield data. For now, it demonstrates the fixed-point
/// calculation with deterministic parameters.
///
/// This function simulates a 5% APY on a principal of 1,000,000 stroops
/// over a time period, using the fixed-point arithmetic implementation.
#[allow(dead_code)]
pub fn calculate_rebate(env: &Env) -> i128 {
    // Mock yield configuration - in production this would be read from storage
    let mock_principal = 1_000_000_i128; // 1M stroops principal
    let mock_apy_bps = 500_i128; // 5% APY in basis points

    // Calculate time elapsed (mock 1 day = 86400 seconds for demonstration)
    let mock_time_elapsed = 86_400_u64; // 1 day

    // Calculate rebate using fixed-point arithmetic
    match calculate_rebate_with_params(env, mock_principal, mock_apy_bps, mock_time_elapsed) {
        Ok(rebate) => {
            env.events().publish(
                ("YIELD", "CALCULATE"),
                String::from_str(env, "Rebate processed"),
            );
            rebate
        }
        Err(_) => {
            env.events().publish(
                ("YIELD", "ERROR"),
                String::from_str(env, "Rebate calculation failed"),
            );
            0 // Return 0 on error to maintain backwards compatibility
        }
    }
}
