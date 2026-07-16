# conduit-contracts

Soroban smart contracts powering the Conduit streaming payments protocol.

Three contracts. One protocol.

---

## Contracts

### `DripStream`

The core contract. One instance is deployed per payment stream. Holds the token balance and enforces the release schedule.

**Storage:**

| Key | Type | Description |
|-----|------|-------------|
| `Sender` | `Address` | Who created and funded the stream |
| `Recipient` | `Address` | Who receives the stream |
| `Token` | `Address` | Stellar asset contract address |
| `RatePerSecond` | `i128` | Tokens released per second (in stroops) |
| `StartTime` | `u64` | Unix timestamp — stream begins |
| `EndTime` | `u64` | Unix timestamp — stream ends (`0` = open-ended) |
| `Withdrawn` | `i128` | Total withdrawn by recipient so far |
| `Paused` | `bool` | Whether the stream is currently paused |
| `PausedAt` | `u64` | Timestamp when stream was last paused |
| `ClawbackEnabled` | `bool` | Whether sender can reclaim unstreamed tokens |
| `Cancelled` | `bool` | Whether the stream has been cancelled |

**Public functions:**

```rust
fn withdraw(env: Env, amount: i128) -> Result<i128, Error>
fn cancel(env: Env) -> Result<(), Error>
fn pause(env: Env) -> Result<(), Error>
fn resume(env: Env) -> Result<(), Error>
fn top_up(env: Env, amount: i128) -> Result<(), Error>
fn clawback(env: Env) -> Result<i128, Error>
fn withdrawable(env: Env) -> i128
fn info(env: Env) -> StreamInfo

// Recipient-initiated escape hatch — see docs/architecture.md
fn force_cancel(env: Env) -> Result<(), Error>

// Recipient reassigns their claim to a new address; withdrawable balance carries over
fn transfer_recipient(env: Env, new_recipient: Address) -> Result<(), Error>

// Read-only: total streamed so far, regardless of what's been withdrawn
fn streamed_total(env: Env) -> i128
```

> **Not yet in the SDK.** `force_cancel`, `transfer_recipient`, and `streamed_total` exist in the
> contract but aren't wrapped by `conduit-sdk` yet — callers need to invoke them directly until
> the SDK catches up.

**Events emitted:**

| Event | Topics | Data |
|-------|--------|------|
| `stream_withdrawn` | `[recipient]` | `{ amount, total_withdrawn, remaining }` |
| `stream_cancelled` | `[sender]` | `{ refund_amount, withdrawn_so_far }` |
| `stream_paused` | `[sender]` | `{ paused_at, withdrawable }` |
| `stream_resumed` | `[sender]` | `{ resumed_at }` |
| `stream_topped_up` | `[sender]` | `{ amount, new_balance }` |
| `stream_clawback` | `[sender]` | `{ amount }` |
| `xfer_rec` | `[old_recipient]` | `new_recipient` |

`force_cancel` reuses the `stream_cancelled` event — from the chain's perspective it settles the same way `cancel()` does.

---

### `DripFactory`

The singleton protocol entry point. Deploys new `DripStream` contracts, assigns them a monotonically incrementing `stream_id`, and maintains the global stream registry.

**Public functions:**

```rust
fn create_stream(
    env:           Env,
    sender:        Address,   // creator / funder — must require_auth
    recipient:     Address,
    token:         Address,
    deposit:       i128,
    rate_per_sec:  i128,
    start_time:    u64,
    end_time:      u64,
    clawback:      bool,
) -> Result<u64, Error>    // returns stream_id

fn stream_address(env: Env, stream_id: u64) -> Option<Address>
fn streams_by_sender(env: Env, sender: Address, offset: u32, limit: u32) -> Vec<u64>
fn streams_by_recipient(env: Env, recipient: Address, offset: u32, limit: u32) -> Vec<u64>
fn stream_count(env: Env) -> u64
fn protocol_fee_bps(env: Env) -> u32   // basis points, e.g. 30 = 0.3%; reads live from DripGovernor

// Governor-only: point future create_stream calls at a new DripStream WASM version.
// Existing streams are unaffected — each is an independently deployed contract.
fn upgrade_stream_wasm(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error>
```

**Validation on `create_stream`:**

- `deposit > 0`
- `rate_per_sec > 0`
- `deposit >= rate_per_sec` (must fund at least 1 second)
- `end_time == 0 || end_time > start_time`
- `start_time >= env.ledger().timestamp()` (no backdated streams)
- `end_time == 0 || deposit >= rate_per_sec × (end_time - start_time)` (must fund the entire declared duration)
- `rate_per_sec <= DripGovernor::config().max_rate_per_second`
- `end_time == 0 || (end_time - start_time) >= DripGovernor::config().min_duration_seconds`
- Token must be a valid Stellar asset contract

---

### `DripGovernor`

Protocol configuration and upgrade authority. Holds mutable parameters that DripFactory reads at stream creation time. Controlled by a multisig authority address (future: on-chain governance).

**Configurable parameters:**

| Parameter | Default | Description |
|-----------|---------|-------------|
| `fee_bps` | `30` | Protocol fee in basis points (30 = 0.3%) |
| `fee_recipient` | treasury | Address that receives protocol fees |
| `min_duration_seconds` | `3600` | Minimum stream duration (1 hour) |
| `max_rate_per_second` | `10^15` | Maximum rate cap |
| `factory_address` | set at init | The DripFactory this governor controls |

**Public functions:**

```rust
fn config(env: Env) -> GovernorConfig   // read-only: full config struct

// All setters below require authority.require_auth() and return InvalidParam on bad input
fn set_fee_bps(env: Env, fee_bps: u32) -> Result<(), Error>              // 0..=10_000
fn set_fee_recipient(env: Env, recipient: Address) -> Result<(), Error>
fn set_min_duration(env: Env, seconds: u64) -> Result<(), Error>         // > 0
fn set_max_rate(env: Env, max_rate: i128) -> Result<(), Error>           // > 0
fn transfer_authority(env: Env, new_authority: Address) -> Result<(), Error>
```

---

## Error Codes

Each contract defines its **own** `Error` enum — the same numeric code means something different
in each one (e.g. code `1` is `NotAuthorized` in `DripStream` and `DripGovernor`, but
`NotInitialized` in `DripFactory`). Match errors against the enum for the contract you called,
not by number alone.

**`DripStream::Error`**

| Code | Name | Description |
|------|------|-------------|
| `1` | `NotAuthorized` | Caller is not the sender or recipient |
| `2` | `StreamNotFound` | Invalid stream ID |
| `3` | `StreamCancelled` | Stream has been cancelled |
| `4` | `StreamNotStarted` | Stream has not started yet |
| `5` | `StreamEnded` | Stream past its end time |
| `6` | `NothingToWithdraw` | Zero withdrawable balance |
| `7` | `InsufficientDeposit` | Deposit too small for the duration |
| `8` | `InvalidTimeRange` | `end_time` ≤ `start_time` |
| `9` | `AlreadyPaused` | Cannot pause an already-paused stream |
| `10` | `NotPaused` | Cannot resume a stream that isn't paused |
| `11` | `ClawbackDisabled` | Clawback not enabled on this stream |
| `12` | `ArithmeticOverflow` | Integer overflow in calculation |
| `13` | `PauseThresholdNotMet` | `force_cancel` called before the pause threshold (30 days) elapsed |
| `14` | `AlreadyInitialized` | `initialize()` called on a stream that's already been initialized |
| `15` | `InvalidAmount` | `withdraw`/`top_up` called with `amount <= 0` |

**`DripFactory::Error`**

| Code | Name | Description |
|------|------|-------------|
| `1` | `NotInitialized` | Factory hasn't been `initialize()`d |
| `2` | `InvalidDeposit` | `deposit <= 0` |
| `3` | `InvalidRate` | `rate_per_sec <= 0` |
| `4` | `InvalidTimeRange` | `end_time != 0 && end_time <= start_time` |
| `5` | `InsufficientDeposit` | `deposit < rate_per_sec` (can't fund even 1 second), or deposit doesn't cover the full `end_time - start_time` |
| `6` | `BackdatedStream` | `start_time < env.ledger().timestamp()` |
| `7` | `AlreadyInitialized` | `initialize()` called on a factory that's already been initialized |
| `8` | `RateExceedsMax` | `rate_per_sec` exceeds `DripGovernor::config().max_rate_per_second` |
| `9` | `DurationTooShort` | `end_time - start_time` is below `DripGovernor::config().min_duration_seconds` |
| `10` | `ArithmeticOverflow` | Integer overflow validating `rate_per_sec × duration` |

**`DripGovernor::Error`**

| Code | Name | Description |
|------|------|-------------|
| `1` | `NotAuthorized` | Caller is not the current authority |
| `2` | `InvalidParam` | Setter argument failed validation (e.g. `fee_bps > 10_000`, `0` duration/rate) |
| `3` | `AlreadyInitialized` | `initialize()` called on a governor that's already been initialized |

---

## Development

### Requirements

| Tool | Version |
|------|---------|
| Rust | ≥ 1.78 |
| `wasm32-unknown-unknown` target | via rustup |
| Stellar CLI | ≥ 20.0 |

### Setup

```bash
git clone https://github.com/conduit-protocol/conduit-contracts
cd conduit-contracts

# Add WASM target
rustup target add wasm32-unknown-unknown

# Build
cargo build --target wasm32-unknown-unknown --release

# Test
cargo test --all

# Lint
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

### Deploy to local network

```bash
# Start local Stellar node (Docker required)
stellar network start local

# Deploy all contracts
./scripts/deploy.sh local

# Output: contract IDs written to .contract-ids/local.json
```

### Deploy to testnet

```bash
# Set up a funded testnet identity
stellar keys generate dev --network testnet --fund

# Deploy
./scripts/deploy.sh testnet
```

---

## Directory Structure

```
conduit-contracts/
├── Cargo.toml                  # workspace
├── contracts/
│   ├── stream/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # contract entry points (thin — delegates to the modules below)
│   │       ├── state.rs        # load/save StreamInfo, cancelled-state guard
│   │       ├── storage.rs      # storage key definitions + StreamInfo struct
│   │       ├── errors.rs       # Error enum
│   │       ├── math.rs         # withdrawable calculation
│   │       ├── events.rs       # event helpers
│   │       └── ttl.rs          # instance TTL extension
│   ├── factory/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # contract entry points (thin — delegates to the modules below)
│   │       ├── storage.rs      # DataKey enum
│   │       ├── errors.rs       # Error enum
│   │       ├── deploy.rs       # WASM hash + deploy logic
│   │       ├── governance.rs   # cross-contract calls into DripGovernor + bounds checks
│   │       ├── query.rs        # pagination helper for streams_by_sender/recipient
│   │       └── ttl.rs          # instance + persistent-entry TTL extension
│   └── governor/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs          # contract entry points (thin — delegates to the modules below)
│           ├── storage.rs      # DataKey enum
│           ├── errors.rs       # Error enum
│           ├── config.rs       # GovernorConfig struct + load helper
│           ├── auth.rs         # authority-gate shared by every write
│           └── ttl.rs          # instance TTL extension
├── tests/
│   ├── stream_lifecycle.rs     # create → withdraw → cancel
│   ├── stream_clawback.rs
│   ├── stream_pause_resume.rs
│   ├── factory_deploy.rs
│   └── governor_config.rs
├── scripts/
│   ├── deploy.sh               # deploy to local / testnet / mainnet
│   ├── upgrade.sh              # upgrade factory/governor WASM
│   └── query.sh                # read stream state from CLI
└── docs/
    ├── architecture.md
    ├── security.md             # threat model
    └── adr/                    # Architecture Decision Records
```

---

## Security Considerations

- All auth checks use `address.require_auth()` — no manual signature verification.
- Arithmetic uses checked operations throughout; overflow returns `Error::ArithmeticOverflow`.
- The `withdrawable()` calculation is read-only and cannot modify state.
- Paused time does not count toward streamed balance (pause freezes the clock).
- Clawback can only be called by the sender and only if enabled at creation time.
- Re-entrancy is prevented by Soroban's execution model (no external calls mid-state-mutation).
- `initialize()` on all three contracts rejects a second call — a stream/factory/governor can't be re-initialized post-deployment to hijack its stored addresses.
- `withdraw`/`top_up` reject non-positive amounts.
- Every state-mutating call extends storage TTL (instance storage, plus the factory's `BySender`/`ByRecipient`/`StreamAddr` persistent entries) — see [`docs/security.md`](./docs/security.md) Known Limitation #1.

**Audit status:** Not yet audited. Do not use on Mainnet with real funds.

---

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md). For contract-specific guidance, see [`docs/architecture.md`](./docs/architecture.md).

---

## License

MIT — see [`LICENSE`](./LICENSE).
