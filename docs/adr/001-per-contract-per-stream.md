# ADR-001: One contract instance per stream

**Status:** Accepted  
**Date:** 2026-01

---

## Context

Streaming payment protocols on EVM (Sablier, Superfluid) use a single registry contract that tracks all streams in a mapping. We had to decide whether to follow the same pattern on Soroban or take advantage of Soroban's cheap contract deployment.

---

## Decision

Deploy a new `DripStream` contract instance for every stream. Each stream is fully self-contained — its balance, state, and logic live in its own contract.

---

## Rationale

**Isolation.** A bug in one stream's state (e.g., a corrupted `Withdrawn` counter) cannot affect any other stream. In a shared-state registry, a logic error can corrupt all streams simultaneously.

**Parallel execution.** Soroban can execute transactions touching different contracts in parallel within the same ledger. A shared registry is a sequential bottleneck. Per-stream contracts allow all streams to be updated concurrently.

**Auditability.** Each stream contract holds only its own funds. External observers can trivially verify a stream's balance by calling `token.balance(stream_address)` and comparing to what `withdrawable()` says. No global accounting state to reason about.

**Soroban deployment cost.** On Soroban, deploying a WASM that has already been uploaded costs only the state entry overhead (not the full WASM size again). The `DripFactory` stores the WASM hash once; each `create_stream` pays a deterministic salt deployment fee that is orders of magnitude cheaper than EVM factory patterns.

---

## Consequences

- **Higher ledger footprint per stream.** Each stream is a full contract instance with its own instance storage entry.
- **Factory required.** Without a factory, users would need to manage WASM hashes manually. The factory abstracts this.
- **No cross-stream atomics.** You cannot atomically split a stream into two or merge two streams in a single transaction. Accepted for v1.
- **Index lives in factory.** Because each stream is independent, the factory must maintain `BySender` / `ByRecipient` indices. These are the only piece of shared mutable state in the protocol.
