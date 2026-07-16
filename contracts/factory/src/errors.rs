use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    InvalidDeposit = 2,
    InvalidRate = 3,
    InvalidTimeRange = 4,
    InsufficientDeposit = 5,
    BackdatedStream = 6,
    AlreadyInitialized = 7,
    RateExceedsMax = 8,
    DurationTooShort = 9,
    ArithmeticOverflow = 10,
}
