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
    /// The factory is under an emergency pause; new stream creation is halted.
    ContractPaused = 11,
    /// `pause` was called while the factory was already paused.
    AlreadyPaused = 12,
    /// `unpause` was called while the factory was not paused.
    NotPaused = 13,
    DurationExceedsMax = 14,
    /// The governor contract did not respond (archived, not initialised,
    /// or a host-level error occurred during the cross-contract call).
    GovernorNotResponding = 15,
    /// `create_batch_streams` was called with an empty `requests` vector.
    EmptyBatch = 16,
    /// `create_batch_streams` requests exceeded `MAX_BATCH_SIZE`.
    BatchTooLarge = 17,
}
