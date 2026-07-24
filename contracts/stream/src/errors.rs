use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotAuthorized = 1,
    StreamNotFound = 2,
    StreamCancelled = 3,
    StreamNotStarted = 4,
    StreamEnded = 5,
    NothingToWithdraw = 6,
    InsufficientDeposit = 7,
    InvalidTimeRange = 8,
    AlreadyPaused = 9,
    NotPaused = 10,
    ClawbackDisabled = 11,
    ArithmeticOverflow = 12,
    PauseThresholdNotMet = 13,
    AlreadyInitialized = 14,
    InvalidAmount = 15,
}
