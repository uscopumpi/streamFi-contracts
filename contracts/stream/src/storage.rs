use soroban_sdk::{contracttype, Address};

#[contracttype]
pub enum DataKey {
    Sender,
    Recipient,
    Token,
    RatePerSecond,
    StartTime,
    EndTime,
    Withdrawn,
    Paused,
    PausedAt,
    ClawbackEnabled,
    Cancelled,
}

#[contracttype]
#[derive(Clone)]
pub struct StreamInfo {
    pub sender: Address,
    pub recipient: Address,
    pub token: Address,
    pub rate_per_second: i128,
    pub start_time: u64,
    pub end_time: u64,
    pub withdrawn: i128,
    pub paused: bool,
    pub paused_at: u64,
    pub clawback_enabled: bool,
    pub cancelled: bool,
}
