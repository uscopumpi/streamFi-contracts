# Contributing to conduit-contracts

Thank you for your interest in contributing to Conduit's smart contract layer. This document covers everything you need to get a working environment, understand the codebase, write good tests, and get your PR merged.

---

## Table of Contents

1. [Code of Conduct](#code-of-conduct)
2. [Getting Started](#getting-started)
3. [Repository Layout](#repository-layout)
4. [Development Workflow](#development-workflow)
5. [Writing Contracts](#writing-contracts)
6. [Writing Tests](#writing-tests)
7. [Commit Convention](#commit-convention)
8. [Pull Request Process](#pull-request-process)
9. [Security Vulnerabilities](#security-vulnerabilities)
10. [Architecture Decision Records](#architecture-decision-records)

---

## Code of Conduct

This project follows the [Contributor Covenant Code of Conduct](./CODE_OF_CONDUCT.md). By participating you agree to uphold it. Report unacceptable behaviour to **conduct@conduit.sh**.

---

## Getting Started

### Prerequisites

| Tool | Version | Install |
|------|---------|---------|
| Rust | ≥ 1.78 | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| `wasm32-unknown-unknown` target | — | `rustup target add wasm32-unknown-unknown` |
| Stellar CLI | ≥ 20.0 | `cargo install --locked stellar-cli` |
| Docker | ≥ 24 | Required for local network |

### Clone and build

```bash
git clone https://github.com/conduit-protocol/conduit-contracts
cd conduit-contracts

# Install the WASM target
rustup target add wasm32-unknown-unknown

# Compile all contracts
cargo build --target wasm32-unknown-unknown --release

# Run all tests (unit + integration)
cargo test --all

# Lint
cargo clippy --all-targets --all-features -- -D warnings

# Format check
cargo fmt --all -- --check
```

### Local network

```bash
# Start a local Stellar node (Docker required)
stellar network start local

# Fund a local key
stellar keys generate dev --network local --fund

# Deploy all contracts
./scripts/deploy.sh local

# Contract IDs are written to .contract-ids/local.json
cat .contract-ids/local.json
```

---

## Repository Layout

```
conduit-contracts/
├── Cargo.toml                  # workspace root
├── contracts/
│   ├── stream/                 # DripStream — per-stream payment contract
│   │   └── src/
│   │       ├── lib.rs          # public contract interface (thin — delegates below)
│   │       ├── state.rs        # load/save StreamInfo, cancelled-state guard
│   │       ├── math.rs         # withdrawable / streamed_amount calculations
│   │       ├── storage.rs      # DataKey enum + StreamInfo struct
│   │       ├── errors.rs       # Error contracterror enum
│   │       ├── events.rs       # event emission helpers
│   │       ├── ttl.rs          # instance TTL extension
│   │       └── tests.rs        # unit tests (run with cargo test -p drip-stream)
│   ├── factory/                # DripFactory — deployment + global registry
│   │   └── src/
│   │       ├── lib.rs          # public contract interface (thin — delegates below)
│   │       ├── deploy.rs       # deterministic WASM deployment logic
│   │       ├── governance.rs   # cross-contract calls into DripGovernor + bounds checks
│   │       ├── query.rs        # pagination helper
│   │       ├── errors.rs       # Error contracterror enum
│   │       ├── ttl.rs          # instance + persistent-entry TTL extension
│   │       └── storage.rs
│   └── governor/               # DripGovernor — protocol parameters
│       └── src/
│           ├── lib.rs          # public contract interface (thin — delegates below)
│           ├── config.rs       # GovernorConfig struct + load helper
│           ├── auth.rs         # authority-gate shared by every write
│           ├── errors.rs       # Error contracterror enum
│           ├── ttl.rs          # instance TTL extension
│           └── storage.rs
├── tests/                      # cross-contract integration tests
│   ├── stream_lifecycle.rs
│   ├── stream_pause_resume.rs
│   ├── stream_clawback.rs
│   ├── factory_deploy.rs
│   └── governor_config.rs
├── scripts/
│   ├── deploy.sh               # deploy to local / testnet / mainnet
│   ├── upgrade.sh              # upgrade WASM hash via governor
│   └── query.sh                # query stream state from CLI
└── docs/
    ├── architecture.md         # contract interaction diagram + design rationale
    ├── security.md             # threat model and known limitations
    └── adr/                    # Architecture Decision Records
```

---

## Development Workflow

We use a **feature branch → PR → squash merge** workflow.

```
main          ←── always deployable, protected
  └── feat/your-feature     ← your work lives here
```

1. **Fork** the repo and clone your fork.
2. **Create a branch** from `main`:
   ```bash
   git checkout -b feat/my-feature
   ```
3. Make your changes, keeping commits small and logical.
4. **Run the full check suite** before pushing:
   ```bash
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo test --all
   ```
5. **Push** and open a PR against `conduit-protocol/conduit-contracts:main`.

---

## Writing Contracts

### Style rules

- `#![no_std]` is required in every contract crate — Soroban runs in a constrained WASM environment.
- Use `soroban_sdk` types everywhere. Never use `std::collections`, `Box`, `String`, etc.
- Keep contract entry points (`#[contractimpl]` functions) thin. Move business logic into private helpers or sub-modules (`math.rs`, etc.).
- All arithmetic on token amounts must use `checked_*` methods and return `Error::ArithmeticOverflow` on failure. Never use `+`, `-`, `*` directly on `i128` values.
- Emit events for every state-changing operation. Use helpers in `events.rs`.
- State mutations must happen **before** cross-contract calls (token transfers) in every function. This preserves the correct state-machine ordering even though Soroban prevents reentrancy.

### Auth pattern

```rust
// Always use require_auth(), never manual signature verification
info.sender.require_auth();
```

### Error handling

All errors must be variants of the `Error` `#[contracterror]` enum in `errors.rs`. Never `panic!` in production paths. `unwrap()` is acceptable only for storage reads that are guaranteed to be initialised by `initialize()`.

### Storage tiers

| Use case | Tier |
|----------|------|
| Contract config (set once, read often) | `instance()` |
| Stream state (mutable per-stream data) | `instance()` (on DripStream) |
| Registry indices (BySender, ByRecipient, StreamAddr) | `persistent()` |
| Nothing | `temporary()` |

Remember: `persistent()` entries expire. Production code must call `extend_ttl()` on every read of a persistent entry.

---

## Writing Tests

### Unit tests (`contracts/stream/src/tests.rs`)

Use `soroban_sdk::testutils` to mock auth, ledger timestamps, and token contracts:

```rust
#[test]
fn my_test() {
    let env = Env::default();
    env.mock_all_auths();

    // Advance time by bumping ledger timestamp
    env.ledger().set(LedgerInfo {
        timestamp: 1_000_000 + 600,  // 600 seconds later
        ..env.ledger().get()
    });
}
```

- Every new public function needs at least one positive test and one negative test (testing each error variant it can return).
- Tests that check token balances must create a mock Stellar asset contract via `env.register_stellar_asset_contract()`.

### Integration tests (`tests/`)

Integration tests deploy all three contracts together and test cross-contract interactions. They follow the same `Env::default()` + `mock_all_auths()` pattern but register multiple contracts.

When adding a new integration test file, add it to `Cargo.toml`'s `[[test]]` section:

```toml
[[test]]
name = "my_integration_test"
path = "tests/my_integration_test.rs"
```

### Running specific tests

```bash
# Unit tests for stream contract only
cargo test -p drip-stream

# Single integration test file
cargo test --test stream_lifecycle

# A specific test function
cargo test --test stream_lifecycle -- cancel_halfway_splits_correctly
```

---

## Commit Convention

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <short description>

[optional body]

[optional footer]
```

**Types:**

| Type | When to use |
|------|-------------|
| `feat` | New contract function or feature |
| `fix` | Bug fix in existing logic |
| `test` | Adding or fixing tests |
| `refactor` | Code restructuring with no behaviour change |
| `docs` | Documentation only |
| `chore` | Tooling, CI, dependency updates |
| `security` | Security fix (coordinate with maintainers first) |

**Scopes:** `stream`, `factory`, `governor`, `math`, `deploy`, `tests`, `ci`, `docs`

**Examples:**

```
feat(stream): add transfer_recipient() — reassign stream rights

fix(math): use saturating_sub to guard against withdrawn > streamed edge case

test(factory): add integration tests for BySender index pagination

docs(adr): ADR 003 — recipient transfer without sender veto

security(governor): validate fee_bps does not exceed 10000
```

---

## Pull Request Process

### Branch naming

```
feat/<issue-number>-short-slug      # new feature
fix/<issue-number>-short-slug       # bug fix
test/<issue-number>-short-slug      # tests only
docs/<issue-number>-short-slug      # docs only
refactor/<issue-number>-short-slug  # refactor
security/<issue-number>-short-slug  # security fix
```

Examples: `fix/5-extend-index-ttl`, `feat/9-initialized-event`

### 5-commit convention

Every PR must be structured as **exactly 5 logical commits**. No squashing allowed before review — reviewers read the commit history. The standard 5-commit shape for a bug fix or feature:

| # | Commit type | What it contains |
|---|---|---|
| 1 | `test(<scope>): add failing test for <issue>` | Tests that reproduce the bug or specify the new behaviour — written **first**, expected to fail on `main` |
| 2 | `fix(<scope>): <minimal fix>` | The smallest code change that makes commit 1's tests pass |
| 3 | `test(<scope>): add edge-case and regression tests` | Additional tests covering error variants, boundary values, and related paths touched by the fix |
| 4 | `docs(<scope>): update inline docs and architecture notes` | Rustdoc comments, any updated `docs/` page, ADR if a design decision was made |
| 5 | `chore(<scope>): fmt + clippy clean-up` | `cargo fmt --all` and any clippy warnings introduced or surfaced by the change |

**Notes:**
- If a change truly requires fewer than 5 commits, split one commit further (e.g. separate positive tests from negative tests, or separate docs per module). Five commits is a floor, not a ceiling — complex features may have more, but every PR must have at least 5.
- Commit bodies (not just subjects) must explain **why**, not just what. Reference the issue number: `Fixes #5`.
- Fixup commits (`fixup!`) are not allowed in PR branches — rebase and amend before requesting review.

### Example commit sequence for fix/5-extend-index-ttl

```
test(factory): add test asserting BySender TTL is extended on stream creation

fix(factory): extend_ttl on BySender and ByRecipient indices after each write

test(factory): add integration test for index survival across 100k ledgers

docs(factory): document persistent storage TTL requirements in storage.rs

chore(factory): fmt and clippy after factory changes
```

### Before opening a PR

**Author checklist:**

- [ ] Branch name follows the naming convention above
- [ ] PR title references the issue: `fix(factory): extend index TTL on create (#5)`
- [ ] PR description links to the issue with `Fixes #<n>` or `Closes #<n>`
- [ ] Exactly 5 commits (minimum), each with a meaningful body explaining *why*
- [ ] `cargo fmt --all` — no diff
- [ ] `cargo clippy --all-targets -- -D warnings` — zero warnings
- [ ] `cargo test --all` — all tests pass
- [ ] If adding a new feature, `docs/architecture.md` updated if the design changes
- [ ] If a significant design decision was made, ADR written in `docs/adr/`
- [ ] PR description includes a test plan: what you tested, what you could not test

### Review requirements

- **Mandatory owner review:** Every PR requires approval from **@jaydbrown** (repository owner) before it can be merged. No exceptions — not for docs PRs, not for chore PRs.
- Any PR touching `math.rs`, `errors.rs`, or authentication logic additionally requires **1 further maintainer approval** (2 approvals total).
- CI must be green (fmt, clippy, all tests).
- A maintainer may request a commit be split or re-ordered before approval — commit structure is part of the review.

### Reviewer checklist

When reviewing, check off each item and leave a comment if any fail:

- [ ] First commit is a failing test that demonstrates the problem
- [ ] Fix commit is minimal — no unrelated changes bundled in
- [ ] All new error variants are tested (positive + negative path)
- [ ] Checked arithmetic on all token amounts (`checked_add`, `checked_sub`, etc.)
- [ ] `require_auth()` called on the correct address before any state mutation
- [ ] State mutated before cross-contract calls (correct state-machine ordering)
- [ ] Events emitted for every external state change (`events.rs` helper used)
- [ ] No `unwrap()` on data that could be absent in a reachable code path
- [ ] `extend_ttl()` called for every `persistent()` write
- [ ] 5-commit structure is clean — no `fixup!` commits, no merge commits

### What reviewers look for

- Checked arithmetic on all token amounts.
- `require_auth()` called before any state mutation.
- State mutated before cross-contract calls.
- Events emitted for every external state change.
- No `unwrap()` on data that could be absent in a reachable code path.
- Tests cover the new behaviour and each new error variant.

---

## Security Vulnerabilities

**Do not open a public GitHub issue for security vulnerabilities.**

Report privately via email to **security@conduit.sh** with:
- A description of the vulnerability
- Steps to reproduce or a proof-of-concept
- Potential impact
- Suggested fix (if you have one)

We aim to acknowledge within 48 hours and provide a status update within 1 week. See [`docs/security.md`](./docs/security.md) for the full threat model.

---

## Architecture Decision Records

When you make a significant design decision (choice of storage tier, new invariant, trade-off in the cancel/settle flow), document it as an ADR:

1. Copy `docs/adr/001-per-contract-per-stream.md` as a template.
2. Number sequentially (`004-your-decision.md`).
3. Fill in **Context**, **Decision**, **Rationale**, and **Consequences**.
4. Reference the ADR in your PR description.

ADRs are immutable once merged. If a decision is reversed, write a new ADR that supersedes the old one — do not edit the original.

---

## License

By contributing you agree that your contributions will be licensed under the [MIT License](./LICENSE).
