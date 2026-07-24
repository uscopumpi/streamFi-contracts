#![no_std]

mod deploy;
mod errors;
mod governance;
mod pause;
mod query;
pub mod storage;
pub mod ttl;

// Import `token` as `tok` to avoid shadowing by any `token: Address` parameter.
use soroban_sdk::{
    contract, contractimpl, panic_with_error, token as tok, Address, BytesN, Env, IntoVal, Vec,
};

pub use errors::Error;
pub use storage::BatchStreamRequest;
use storage::DataKey;

/// Maximum number of streams accepted by a single `create_batch_streams`
/// call. Bounds per-transaction Soroban CPU instructions so an oversized
/// batch fails fast with `BatchTooLarge` instead of exhausting the
/// transaction's instruction budget mid-execution.
pub const MAX_BATCH_SIZE: u32 = 100;

#[contract]
pub struct DripFactory;

#[contractimpl]
impl DripFactory {
    /// One-time setup — called by the deploy script.
    ///
    /// Guards against re-initialization: without this check, anyone could
    /// call `initialize` again to point the factory at an attacker-controlled
    /// `stream_wasm_hash` or `governor`, hijacking every subsequent
    /// `create_stream` call.
    pub fn initialize(env: Env, stream_wasm_hash: BytesN<32>, governor: Address) {
        if env.storage().instance().has(&DataKey::StreamCount) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        ttl::bump_instance(&env);

        env.storage()
            .instance()
            .set(&DataKey::StreamWasmHash, &stream_wasm_hash);
        env.storage()
            .instance()
            .set(&DataKey::GovernorAddress, &governor);
        env.storage().instance().set(&DataKey::StreamCount, &0_u64);
    }

    /// Deploy a new DripStream and register it.
    ///
    /// The caller (`sender`) must pass their address explicitly — Soroban has no
    /// implicit `msg.sender`. `sender.require_auth()` enforces that the transaction
    /// is signed by the address it claims to be.
    #[allow(clippy::too_many_arguments)]
    pub fn create_stream(
        env: Env,
        sender: Address, // the stream creator / funder
        recipient: Address,
        token: Address, // Stellar asset contract address
        deposit: i128,
        rate_per_sec: i128,
        start_time: u64,
        end_time: u64,
        clawback: bool,
    ) -> Result<u64, Error> {
        // ── Auth ─────────────────────────────────────────────────────────
        sender.require_auth();

        // ── Emergency pause ──────────────────────────────────────────────
        // Checked before any validation or state access so a halted protocol
        // rejects new streams immediately, without pulling a deposit or paying
        // a TTL extension. Already-deployed streams are independent contracts
        // and are unaffected by this flag.
        if pause::is_paused(&env) {
            return Err(Error::ContractPaused);
        }

        // ── Validation ───────────────────────────────────────────────────
        // Fail early: all input checks run before any state is touched, so
        // invalid calls (e.g. an empty stream with a non-positive amount)
        // neither mutate storage nor pay a TTL extension.
        if deposit <= 0 {
            return Err(Error::InvalidDeposit);
        }
        if rate_per_sec <= 0 {
            return Err(Error::InvalidRate);
        }
        if deposit < rate_per_sec {
            return Err(Error::InsufficientDeposit);
        }
        if end_time > 0 && end_time <= start_time {
            return Err(Error::InvalidTimeRange);
        }
        if start_time < env.ledger().timestamp() {
            return Err(Error::BackdatedStream);
        }
        // A fixed-duration stream must be funded for its entire declared
        // length — otherwise it silently drains before end_time. `deposit
        // >= rate_per_sec` above only guarantees 1 second of streaming.
        if end_time > 0 {
            let duration = (end_time - start_time) as i128;
            let required = rate_per_sec
                .checked_mul(duration)
                .ok_or(Error::ArithmeticOverflow)?;
            if deposit < required {
                return Err(Error::InsufficientDeposit);
            }
        }

        // ── Governor-controlled bounds ──────────────────────────────────────
        let governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::GovernorAddress)
            .ok_or(Error::NotInitialized)?;
        let config = governance::config(&env, &governor)?;
        governance::enforce_bounds(&config, rate_per_sec, start_time, end_time)?;

        // ── All validation passed — safe to touch state now ──────────────
        ttl::bump_instance(&env);

        // ── Pull deposit from sender ──────────────────────────────────────
        // Using the aliased `tok` to avoid any future shadowing issues.
        let tk = tok::Client::new(&env, &token);
        tk.transfer(&sender, &env.current_contract_address(), &deposit);

        // ── Assign stream ID ─────────────────────────────────────────────
        let stream_count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::StreamCount)
            .unwrap_or(0);
        let stream_id = stream_count;

        let wasm_hash: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::StreamWasmHash)
            .ok_or(Error::NotInitialized)?;

        // ── Deploy DripStream ────────────────────────────────────────────
        let init_args = soroban_sdk::vec![
            &env,
            sender.to_val(),
            recipient.to_val(),
            token.to_val(),
            rate_per_sec.into_val(&env),
            start_time.into_val(&env),
            end_time.into_val(&env),
            clawback.into_val(&env),
        ];

        let stream_addr = deploy::deploy_stream(&env, &wasm_hash, stream_id, init_args);

        // Forward the deposit into the newly deployed stream contract.
        tk.transfer(&env.current_contract_address(), &stream_addr, &deposit);

        // ── Index ─────────────────────────────────────────────────────────
        // StreamAddr and the sender/recipient indices grow without bound, so
        // they use persistent storage (not instance storage) to avoid hitting
        // instance storage size limits as the protocol scales.
        //
        // Persistent storage entry 1 — StreamAddr:
        //   Key:   DataKey::StreamAddr(stream_id)
        //          XDR serialization: [discriminant: u32][stream_id: u64]
        //   Value: Address (the deployed DripStream contract address)
        //          XDR serialization: XDR-encoded contract Address
        env.storage()
            .persistent()
            .set(&DataKey::StreamAddr(stream_id), &stream_addr);
        // Extend TTL on the stream address entry so it outlives ledger pruning.
        env.storage().persistent().extend_ttl(
            &DataKey::StreamAddr(stream_id),
            ttl::THRESHOLD,
            ttl::EXTEND_TO,
        );
        env.storage()
            .instance()
            .set(&DataKey::StreamCount, &(stream_count + 1));

        // Persistent storage entry 2 — BySender:
        //   Key:   DataKey::BySender(sender)
        //          XDR serialization: [discriminant: u32][sender: XDR Address]
        //   Value: Vec<u64> (ordered list of stream IDs this sender has created)
        //          XDR serialization: XDR-encoded Vec of u64 elements
        let mut by_sender: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::BySender(sender.clone()))
            .unwrap_or(Vec::new(&env));
        by_sender.push_back(stream_id);
        env.storage()
            .persistent()
            .set(&DataKey::BySender(sender.clone()), &by_sender);
        env.storage().persistent().extend_ttl(
            &DataKey::BySender(sender),
            ttl::THRESHOLD,
            ttl::EXTEND_TO,
        );

        // Persistent storage entry 3 — ByRecipient:
        //   Key:   DataKey::ByRecipient(recipient)
        //          XDR serialization: [discriminant: u32][recipient: XDR Address]
        //   Value: Vec<u64> (ordered list of stream IDs where this address is recipient)
        //          XDR serialization: XDR-encoded Vec of u64 elements
        let mut by_recipient: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::ByRecipient(recipient.clone()))
            .unwrap_or(Vec::new(&env));
        by_recipient.push_back(stream_id);
        env.storage()
            .persistent()
            .set(&DataKey::ByRecipient(recipient.clone()), &by_recipient);
        env.storage().persistent().extend_ttl(
            &DataKey::ByRecipient(recipient),
            ttl::THRESHOLD,
            ttl::EXTEND_TO,
        );

        Ok(stream_id)
    }

    /// Create multiple streams in one transaction, all funded and
    /// authorized by the same `sender`.
    ///
    /// Reuses `create_stream` verbatim for each request, so validation,
    /// governor-bound enforcement, the deposit transfer, deployment, and
    /// registry indexing are all identical to the single-stream path --
    /// including whatever per-stream signals `create_stream` /
    /// `DripStream::initialize` already produce.
    ///
    /// Atomicity: Soroban transactions are all-or-nothing at the host
    /// level. If any request fails validation, the `?` below propagates
    /// that error immediately, and every storage write and token
    /// transfer already made earlier in this same call is rolled back
    /// by the host -- no partial-batch state is ever left behind.
    pub fn create_batch_streams(
        env: Env,
        sender: Address,
        requests: Vec<BatchStreamRequest>,
        clawback: bool,
    ) -> Result<Vec<u64>, Error> {
        if requests.is_empty() {
            return Err(Error::EmptyBatch);
        }
        if requests.len() > MAX_BATCH_SIZE {
            return Err(Error::BatchTooLarge);
        }

        let mut stream_ids = Vec::new(&env);
        for request in requests.iter() {
            let stream_id = Self::create_stream(
                env.clone(),
                sender.clone(),
                request.recipient,
                request.token,
                request.deposit,
                request.rate_per_sec,
                request.start_time,
                request.end_time,
                clawback,
            )?;
            stream_ids.push_back(stream_id);
        }
        Ok(stream_ids)
    }

    pub fn stream_address(env: Env, stream_id: u64) -> Option<Address> {
        env.storage()
            .persistent()
            .get(&DataKey::StreamAddr(stream_id))
    }

    pub fn streams_by_sender(env: Env, sender: Address, offset: u32, limit: u32) -> Vec<u64> {
        let all: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::BySender(sender))
            .unwrap_or(Vec::new(&env));
        query::paginate(&env, all, offset, limit)
    }

    pub fn streams_by_recipient(env: Env, recipient: Address, offset: u32, limit: u32) -> Vec<u64> {
        let all: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::ByRecipient(recipient))
            .unwrap_or(Vec::new(&env));
        query::paginate(&env, all, offset, limit)
    }

    pub fn stream_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::StreamCount)
            .unwrap_or(0)
    }

    /// Read-only: current protocol fee in basis points.
    ///
    /// Reads live from DripGovernor. Falls back to the protocol default (30
    /// bps) if the factory hasn't been initialized yet — there is no
    /// governor address to call in that state.
    pub fn protocol_fee_bps(env: Env) -> u32 {
        let governor: Option<Address> = env.storage().instance().get(&DataKey::GovernorAddress);
        match governor {
            Some(governor) => governance::config(&env, &governor).map(|c| c.fee_bps).unwrap_or(30),
            None => 30,
        }
    }

    /// Update the stored stream WASM hash.
    ///
    /// Called after a new stream contract version is uploaded so subsequent
    /// `create_stream` calls deploy the new implementation. Existing streams
    /// are unaffected — each is an independent deployed contract.
    ///
    /// As a maintenance operation, this also drives the bounded TTL walker
    /// so the persistent-by-ID registry stays alive during protocol-upgrade
    /// activity even if no `create_stream` calls happen between upgrades.
    pub fn upgrade_stream_wasm(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        // Only governor may update the wasm hash.
        let governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::GovernorAddress)
            .ok_or(Error::NotInitialized)?;
        governor.require_auth();
        ttl::bump_instance(&env);
        ttl::bump_persistent_bucket(&env);
        env.storage()
            .instance()
            .set(&DataKey::StreamWasmHash, &new_wasm_hash);
        Ok(())
    }

    /// Emergency halt: stop all new stream creation.
    ///
    /// Intended for an extreme protocol emergency. While paused, every
    /// `create_stream` call reverts with `ContractPaused` before any deposit
    /// is pulled. Existing streams are independent deployed contracts and keep
    /// running; front-ends and the stream contract can gate withdrawals by
    /// reading `is_paused`.
    ///
    /// Gated on the governor, matching `upgrade_stream_wasm` — the same
    /// authority that controls protocol parameters controls the halt.
    pub fn pause(env: Env) -> Result<(), Error> {
        let governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::GovernorAddress)
            .ok_or(Error::NotInitialized)?;
        governor.require_auth();
        if pause::is_paused(&env) {
            return Err(Error::AlreadyPaused);
        }
        ttl::bump_instance(&env);
        // Maintenance op — drive the bounded TTL walker so the persistent
        // registry doesn't silently archive during an emergency-pause idle
        // period.
        ttl::bump_persistent_bucket(&env);
        pause::set_paused(&env, true);
        Ok(())
    }

    /// Lift the emergency pause, allowing `create_stream` again.
    ///
    /// Gated on the governor, matching `pause`.
    pub fn unpause(env: Env) -> Result<(), Error> {
        let governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::GovernorAddress)
            .ok_or(Error::NotInitialized)?;
        governor.require_auth();
        if !pause::is_paused(&env) {
            return Err(Error::NotPaused);
        }
        ttl::bump_instance(&env);
        // Maintenance op — drive the bounded TTL walker so the persistent
        // registry doesn't silently archive while the protocol is coming
        // back online and `create_stream` hasn't yet resumed.
        ttl::bump_persistent_bucket(&env);
        pause::set_paused(&env, false);
        Ok(())
    }

    /// Read-only: whether the factory is currently under an emergency pause.
    ///
    /// Returns `false` for a factory that predates this feature (the flag was
    /// never written). Exposed so the stream contract and off-chain infra can
    /// enforce the halt on withdrawals as well as creation.
    pub fn is_paused(env: Env) -> bool {
        pause::is_paused(&env)
    }
}
