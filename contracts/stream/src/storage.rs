use soroban_sdk::{contracttype, Address};

pub const FLAG_PAUSED: u32 = 1;
pub const FLAG_CLAWBACK_ENABLED: u32 = 1 << 1;
pub const FLAG_CANCELLED: u32 = 1 << 2;

#[contracttype]
pub enum DataKey {
    Sender,
    Recipient,
    Token,
    RatePerSecond,
    StartTime,
    EndTime,
    Withdrawn,
    PausedAt,
    Flags,
    ClawbackEnabled,
    Cancelled,
    /// Single-key representation of all stream fields.
    /// Replaces the 11 individual keys above for new writes — loaded in one
    /// storage read instead of eleven.
    Config,
    /// Monotonic identifier attached to every contract event.
    ///
    /// Consumers compare this value with the last sequence they processed
    /// after reconnecting so missing ledger events cannot go unnoticed.
    EventSequence,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamInfo {
    pub sender: Address,
    pub recipient: Address,
    pub token: Address,
    pub rate_per_second: i128,
    pub start_time: u64,
    pub end_time: u64,
    pub withdrawn: i128,
    pub paused_at: u64,
    pub flags: u32,
}

impl StreamInfo {
    pub fn is_paused(&self) -> bool {
        (self.flags & FLAG_PAUSED) != 0
    }

    pub fn is_cancelled(&self) -> bool {
        (self.flags & FLAG_CANCELLED) != 0
    }

    pub fn is_clawback_enabled(&self) -> bool {
        (self.flags & FLAG_CLAWBACK_ENABLED) != 0
    }
}
