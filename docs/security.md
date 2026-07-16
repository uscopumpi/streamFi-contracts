# Security

Threat model, known limitations, and safe usage guidance for Conduit contracts.

> **Audit status:** Not yet audited. Do not deploy on Stellar Mainnet with real funds.

---

## Threat Model

### Assets at risk

- Token balances held inside `DripStream` contracts.
- In-flight deposits held by `DripFactory` for the duration of `create_stream` (one transaction, sub-second).

### Trusted parties

| Party | Trust level | What they can do |
|-------|-------------|-----------------|
| Stream sender | High | Created and funded the stream. Can pause, resume, cancel, top-up. Can clawback if enabled. |
| Stream recipient | Partial | Can only `withdraw()` — cannot pause, cancel, or access more than their earned balance. |
| DripGovernor authority | High | Can change fee rates and protocol parameters. Cannot touch stream balances. |
| DripFactory | Internal | Deploys streams and forwards deposits. Holds funds for ≤ 1 transaction. |

---

## Authentication

All sensitive operations are guarded by `address.require_auth()`. This delegates to Soroban's built-in auth framework, which verifies that the signed payload matches the contract invocation being authorised. There is no manual signature verification in the contracts.

Specifically:
- `cancel`, `pause`, `resume`, `top_up`, `clawback` → `sender.require_auth()`
- `withdraw` → `recipient.require_auth()`
- `create_stream` → `sender.require_auth()`
- All governor write functions → `authority.require_auth()`

---

## Reentrancy

Soroban's execution model is synchronous and single-threaded within a transaction. Cross-contract calls (e.g., `token.transfer`) cannot call back into the calling contract mid-execution. Reentrancy is structurally impossible.

For defence-in-depth, state mutations (setting `Cancelled = true`) are performed before token transfers in `cancel()`.

---

## Arithmetic

All arithmetic uses Rust's `checked_*` methods. Overflow returns `Error::ArithmeticOverflow` rather than wrapping silently. The `i128` type is used for all token amounts (matching Stellar's native representation).

---

## Known Limitations

### 1. No TTL management in scaffold — ✅ Resolved

Soroban `persistent()` storage entries expire after a configurable number of ledgers if their TTL is not extended. The scaffold did not include TTL extension calls beyond the factory's `StreamAddr` registry entry.

**Resolved:** every state-mutating call on all three contracts now extends instance TTL (`DripStream`, `DripFactory`, `DripGovernor`), and `DripFactory::create_stream` extends the `BySender`/`ByRecipient` persistent entries the same way `StreamAddr` already was. A `keep_alive(stream_id)`-style function anyone can call to refresh an inactive stream's TTL without touching its state is still a possible future addition, but is not required for the archival risk itself to be closed.

### 2. BySender / ByRecipient indices are unbounded vecs

Storing a `Vec<u64>` of stream IDs per address means a single address creating thousands of streams will hold a very large persistent storage entry. Large entries cost more in ledger fees.

**Mitigation (future):** Switch to a cursor-based linked list or off-chain indexing via Horizon events.

### 3. Governor authority is a single address

The current `DripGovernor` uses a single `authority` address for all configuration changes. If this key is compromised, an attacker could set `fee_bps = 10000` (100%) on future streams or drain the fee recipient.

**Mitigation:** Use a multisig or time-lock contract as the `authority`.

### 4. No minimum deposit validation against end_time — ✅ Resolved

The factory validated `deposit >= rate_per_sec` (at least 1 second of streaming), but did not validate `deposit >= rate_per_sec × (end_time - start_time)`. A stream could be created where the deposit covered less time than the declared duration, and would simply drain early.

**Resolved:** `create_stream` now requires `deposit >= rate_per_sec × (end_time - start_time)` for any stream with `end_time > 0` (open-ended streams are unaffected, since there's no fixed duration to validate against).

### 5. Pause-state attack by sender — ✅ Mitigated

A malicious sender pausing a stream indefinitely, blocking further accrual for the recipient while retaining full control of unstreamed tokens, is mitigated by `force_cancel()`: the recipient can unilaterally settle the stream once it's been paused for more than `PAUSE_THRESHOLD_SECS` (30 days). See `docs/architecture.md` for the settlement details.

### 6. Clawback can be used adversarially

A sender with `clawback_enabled = true` can call `clawback()` to reclaim all unstreamed tokens at any time, effectively starving the recipient of future payments. Recipients should verify `clawback_enabled` before accepting a stream.

**Mitigation:** The app should display a prominent warning when `clawback_enabled = true`. Consider requiring recipient acknowledgement before stream activation.

---

## Disclosure

To report a security vulnerability privately:

- **Email:** security@conduit.sh
- **PGP:** key in `SECURITY.md` at the org root

Do not open a public GitHub issue for security vulnerabilities.

---

## Audit Checklist (pre-mainnet)

- [x] All arithmetic overflow paths tested under adversarial inputs
- [x] TTL management added to all persistent storage reads (and instance storage — see Known Limitation #1)
- [x] `initialize()` guarded against re-invocation on all three contracts
- [x] `withdraw`/`top_up` reject non-positive amounts
- [x] `create_stream` deposit validated against full stream duration (see Known Limitation #4)
- [x] `DripGovernor` config (`min_duration_seconds`, `max_rate_per_second`, `fee_bps`) actually enforced by `DripFactory`
- [x] `Cargo.lock` committed for reproducible builds
- [ ] Governor authority switched to multisig (see Known Limitation #3)
- [ ] BySender/ByRecipient indices redesigned to bound growth (see Known Limitation #2)
- [ ] Full integration test suite passing on testnet
- [ ] External audit completed by a Soroban-specialised firm
- [ ] Bug bounty program active before mainnet deployment
