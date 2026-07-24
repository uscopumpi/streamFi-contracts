use soroban_sdk::{contracttype, Address};

/// Storage keys for the DripFactory contract.
///
/// The `#[contracttype]` macro serializes each variant as an XDR tagged union:
/// the discriminant (variant index) followed by the encoded inner type. This
/// means `DataKey::StreamAddr(42)` and `DataKey::StreamAddr(43)` are distinct
/// keys in Soroban's storage trie, each serialized as:
///   [discriminant: u32][stream_id: u64]
///
/// Similarly, `DataKey::BySender(address)` serializes as:
///   [discriminant: u32][address: XDR-encoded Address]
///
/// Storage is split across two tiers:
/// - **Instance storage**: Small, contract-scoped data that scales with the
///   number of operations (e.g., counters, config). Bounded by instance size limits.
/// - **Persistent storage**: Per-entity data that grows without bound (e.g.,
///   per-stream addresses, per-user indices). Avoids hitting instance size limits
///   as the protocol scales. Each entry has its own TTL and can be extended independently.

#[contracttype]
pub enum DataKey {
    /// **Instance storage.** Monotonically incrementing stream counter.
    /// Key: `DataKey::StreamCount` (no inner type, discriminant only)
    /// Value: `u64` — the next stream ID to assign
    StreamCount,

    /// **Persistent storage.** Maps stream ID to its deployed contract address.
    /// Key: `DataKey::StreamAddr(u64)` — the stream's unique ID
    /// Value: `Address` — the on-chain address of the deployed DripStream contract
    /// Serialization: XDR tagged union [discriminant: u32][stream_id: u64] → [contract_address: XDR Address]
    /// TTL: Extended to `ttl::EXTEND_TO` (200_000 ledgers) on creation
    StreamAddr(u64),

    /// **Persistent storage.** Index of all streams created by a given sender.
    /// Key: `DataKey::BySender(Address)` — the sender's Stellar address
    /// Value: `Vec<u64>` — list of stream IDs created by this sender, in creation order
    /// Serialization: XDR tagged union [discriminant: u32][sender: XDR Address] → [stream_ids: XDR Vec<u64>]
    /// TTL: Extended to `ttl::EXTEND_TO` (200_000 ledgers) on each new stream
    /// Note: Grows unbounded as the sender creates more streams
    BySender(Address),

    /// **Persistent storage.** Index of all streams received by a given recipient.
    /// Key: `DataKey::ByRecipient(Address)` — the recipient's Stellar address
    /// Value: `Vec<u64>` — list of stream IDs where this address is the recipient, in creation order
    /// Serialization: XDR tagged union [discriminant: u32][recipient: XDR Address] → [stream_ids: XDR Vec<u64>]
    /// TTL: Extended to `ttl::EXTEND_TO` (200_000 ledgers) on each new stream
    /// Note: Grows unbounded as the recipient receives more streams
    ByRecipient(Address),

    /// **Instance storage.** WASM hash of the DripStream contract (for deployment).
    /// Key: `DataKey::StreamWasmHash` (no inner type, discriminant only)
    /// Value: `BytesN<32>` — SHA-256 hash of the stream contract WASM
    StreamWasmHash,

    /// **Instance storage.** Address of the DripGovernor contract.
    /// Key: `DataKey::GovernorAddress` (no inner type, discriminant only)
    /// Value: `Address` — the on-chain address of the DripGovernor contract
    GovernorAddress,

    /// **Instance storage.** Emergency-pause flag.
    /// Key: `DataKey::Paused` (no inner type, discriminant only)
    /// Value: `bool` — `true` while the protocol is under an emergency halt.
    /// A missing entry (e.g. a factory initialized before this feature
    /// existed) is treated as `false`/unpaused.
    Paused,

    /// **Instance storage.** Cursor for the bounded persistent-entry TTL
    /// walker. Each call into `pause`/`unpause`/`upgrade_stream_wasm`
    /// advances this cursor by `ttl::BATCH_LIMIT` IDs (modulo `StreamCount`)
    /// and bumps the persistent `StreamAddr(id)` TTLs in that window. This
    /// keeps the registry alive during idle periods independently of
    /// `create_stream` (which only bumps entries it touches).
    /// Key: `DataKey::LastBumpedId` (no inner type, discriminant only)
    /// Value: `u64` — the last stream ID whose persistent `StreamAddr` TTL
    /// was bumped by the walker. Missing entry is treated as `0`.
    LastBumpedId,
}

/// A single stream configuration within a `create_batch_streams` call.
///
/// Mirrors the per-stream parameters of `create_stream` minus `sender`
/// (the whole batch shares one sender/authorizer) -- see
/// `DripFactory::create_batch_streams`.
#[contracttype]
#[derive(Clone)]
pub struct BatchStreamRequest {
    pub recipient: Address,
    pub token: Address,
    pub deposit: i128,
    pub rate_per_sec: i128,
    pub start_time: u64,
    pub end_time: u64,
}
