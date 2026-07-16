use soroban_sdk::{contracttype, Address};

#[contracttype]
pub enum DataKey {
    /// Monotonically incrementing stream counter
    StreamCount,
    /// stream_id → contract Address
    StreamAddr(u64),
    /// sender Address → Vec<stream_id>
    BySender(Address),
    /// recipient Address → Vec<stream_id>
    ByRecipient(Address),
    /// Stored WASM hash of DripStream (for deployment)
    StreamWasmHash,
    /// Address of DripGovernor
    GovernorAddress,
}
