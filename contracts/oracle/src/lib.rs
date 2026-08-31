#![no_std]

use drip_common::rbac;
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Vec};

/// TTL extension constants matching the convention used across sibling
/// contracts (factory, governor, stream). `bump_instance` is called from
/// every state-mutating entry point so instance storage entries
/// (`DataKey::Admin`, `DataKey::Config`, `DataKey::Price`, etc.) never
/// silently archive during idle periods.
const THRESHOLD: u32 = 100_000;
const EXTEND_TO: u32 = 200_000;

/// Maximum number of distinct price feeders the oracle will track. Caps the
/// `DataKey::Submitters` list and the per-feeder `Submission` loop in
/// `get_twap_price` (one storage `get` per entry). Without a cap the list
/// grows once per unique feeder for the life of the contract and
/// `get_twap_price` pays linear CPU. `persistent()` is used for these
/// entries (see `storage.rs` rule: unbounded per-entity data belongs in
/// `persistent()`, not `instance()`), but even persistent growth must be
/// bounded to keep aggregation within the transaction budget. 32 feeders is
/// ample for a TWAP median and keeps `get_twap_price` well under budget
/// (32 gets + insertion sort).
const MAX_SUBMITTERS: u32 = 32;

fn bump_instance(env: &Env) {
    env.storage().instance().extend_ttl(THRESHOLD, EXTEND_TO);
}

/// Protocol administration roles for the oracle.
///
/// Separates concerns so independent wallets can own price submission,
/// oracle configuration, and emergency pause authority:
///
/// - `Admin`       — configure oracle, grant/revoke roles, emergency pause.
/// - `PriceFeeder` — submit prices. `submit_price` requires this role
///                   specifically; being `Admin` alone is not sufficient.
/// - `Pauser`      — call `pause`/`unpause` without needing full `Admin`.
///                   Mirrors `DripGovernor::Role::Pauser`, closing the
///                   delegation gap noted in issue #203.
#[contracttype]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Admin,
    PriceFeeder,
    /// Emergency-pause authority. A holder can call `pause`/`unpause`
    /// without being granted full `Admin` — enabling an operational hot
    /// wallet to halt price submission during an incident while the admin
    /// key stays in cold storage.
    Pauser,
}

/// Composite key identifying a single (role, account) grant.
#[contracttype]
#[derive(Clone)]
pub struct RoleKey {
    pub role: Role,
    pub account: Address,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Config,
    Price,
    Role(RoleKey),
    AdminCount,
    Paused,
    /// **Persistent storage.** Most recent submission from a single feeder,
    /// keyed by feeder address. Stored in `persistent()` (not `instance()`)
    /// per the `storage.rs` rule: unbounded per-entity data belongs in
    /// `persistent()` to avoid instance bloat. Aggregated (median) across
    /// every address in `Submitters` by `get_twap_price`, so no single
    /// feeder's price is trusted alone. TTL extended on each `submit_price`.
    Submission(Address),
    /// **Persistent storage.** Every address that has called `submit_price`,
    /// iterated by `get_twap_price` to build the aggregation set. Stored in
    /// `persistent()` with TTL extension; capped at [`MAX_SUBMITTERS`] so
    /// aggregation stays bounded (one `get` per entry).
    Submitters,
    /// Index of all accounts currently holding a given role.
    ///
    /// Maintained alongside every `grant_role_inner`/`revoke_role_inner`
    /// call so role membership can be enumerated on-chain without replaying
    /// every `grant`/`revoke` event from genesis — mirrors
    /// `DripGovernor::DataKey::RoleMembers`, closing the gap noted in the
    /// off-chain tooling audit.
    RoleMembers(Role),
}

/// Configuration parameters for the TWAP oracle.
///
/// Defines the fixed-point decimal scaling, asset peg identifier, and
/// maximum allowed price staleness.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleConfig {
    /// Number of decimal places used in fixed-point price submissions (maximum 19
    /// when `price` is `u64`; see `TwapOracle::submit_price`).
    ///
    /// Fixed-point prices submitted via [`TwapOracle::submit_price`] are scaled
    /// by `10^decimals`. With `price: u64`, `10^20 > u64::MAX (≈1.84e19)` so
    /// any `decimals >= 20` makes real-world prices unrepresentable. Capped at
    /// 19 to keep `10^decimals <= u64::MAX`; exceeding 19 causes
    /// [`TwapOracle::configure_oracle`] to return `Err(Error::InvalidDecimals)`.
    /// To support `decimals > 19`, widen `PriceData::price` / `submit_price`
    /// to `u128`/`i128` and update the aggregation.
    pub decimals: u32,
    /// Target asset peg identifier (e.g., currency/asset pairing representation).
    pub asset_peg: u32,
    /// Maximum allowable age (in seconds) of a price submission before it is
    /// treated as stale by [`TwapOracle::get_twap_price`] or [`TwapOracle::is_price_stale`].
    pub max_staleness: u64,
    /// Absolute upper-bound ceiling for submitted prices. When non-zero,
    /// [`TwapOracle::submit_price`] rejects any `price > max_price` with
    /// [`Error::PriceExceedsMaxPrice`] before the value enters the TWAP
    /// window. Set to `0` to disable the ceiling (unlimited).
    ///
    /// This catches fat-fingered or malicious admin key submissions that
    /// would otherwise corrupt the TWAP median until enough correct
    /// observations roll it out — see issue #226.
    pub max_price: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub price: u64,
    pub updated_at: u64,
}

/// Aggregated price health, computed from the same per-feeder submission set
/// that [`get_twap_price`](TwapOracle::get_twap_price) aggregates, so these
/// metrics can never disagree with the median it returns.
///
/// The old `price_age` / `is_price_stale` views keyed off the legacy
/// single-value `DataKey::Price` scalar, which `submit_price` overwrites with
/// whichever feeder submitted last. That let one fresh feeder make the whole
/// price look fresh while the TWAP median was dominated by other feeders'
/// older-but-still-fresh values — a number materially different from the
/// "latest price" a UI showed next to a green indicator. Callers also had no
/// way to see the quorum (how many distinct feeders contributed) or the spread
/// of submission ages behind the value they were about to read.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceStatus {
    /// Number of distinct feeders whose most-recent submission is still within
    /// `max_staleness` and non-zero — exactly the set `get_twap_price`'s
    /// median is computed over. A caller can use this as a quorum indicator:
    /// `1` means no diversification (any single feeder drives the price),
    /// while several mean the median is genuinely cross-feeder.
    pub fresh_submitter_count: u32,
    /// Age in seconds of the newest fresh submission (the youngest individual
    /// observation contributing to the median). `0` when `stale` is `true`.
    pub newest_age: u64,
    /// Age in seconds of the oldest fresh submission (how old the stalest
    /// observation contributing to the median is). `0` when `stale` is `true`.
    /// Together with `newest_age`, this exposes the spread in the aggregation
    /// window so a caller can judge how tightly clustered the fresh values are.
    pub oldest_fresh_age: u64,
    /// `true` when there are zero fresh submissions — exactly when
    /// `get_twap_price` returns `Error::OracleStalePrice`.
    pub stale: bool,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    OracleStalePrice = 1001,
    OracleNotConfigured = 1002,
    InvalidPrice = 1003,
    OracleLocked = 1004,
    CalculationOverflow = 1005,
    NotAuthorized = 1006,
    AlreadyInitialized = 1007,
    NoPriceAvailable = 1008,
    ArithmeticOverflow = 1009,
    InvalidDecimals = 1010,
    /// The oracle is under an emergency pause; price submission is halted.
    ContractPaused = 1011,
    /// `pause` was called while the oracle was already paused.
    AlreadyPaused = 1012,
    /// `unpause` was called while the oracle was not paused.
    NotPaused = 1013,
    /// Refused to revoke the last `Admin`, which would freeze oracle governance.
    LastAdmin = 1014,
    /// `max_staleness` was set to 0 (degenerate: causes all price submissions to be immediately stale).
    InvalidMaxStaleness = 1015,
    /// `Submitters` already holds `MAX_SUBMITTERS` distinct feeders.
    TooManySubmitters = 1016,
    /// Submitted price exceeds the configured `max_price` ceiling.
    PriceExceedsMaxPrice = 1017,
}

#[contract]
pub struct TwapOracle;

#[contractimpl]
impl TwapOracle {
    /// One-time setup — called by the deploy script.
    ///
    /// Guards against re-initialization: without this check, anyone could call
    /// `initialize` again to set themselves as `Admin`, takeover oracle
    /// governance, and manipulate price updates.
    ///
    /// Grants `Admin` role to `admin` so a single wallet can bootstrap the
    /// oracle and later delegate price submission to a separate
    /// `PriceFeeder` wallet via [`TwapOracle::grant_role`].
    pub fn initialize(env: Env, admin: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        bump_instance(&env);
        env.storage().instance().set(&DataKey::Admin, &admin);
        grant_role_inner(&env, Role::Admin, &admin);
        Ok(())
    }

    // ── Role administration (Admin-gated) ────────────────────────────────

    /// Whether `account` currently holds `role`.
    pub fn has_role(env: Env, role: Role, account: Address) -> bool {
        has_role(&env, role, &account)
    }

    /// Returns every account currently holding `role`.
    ///
    /// Reads from the persistent `RoleMembers` index maintained by
    /// `grant_role`/`revoke_role`. Returns an empty vector if no accounts
    /// hold the role — no event-log replay needed, unlike the old design.
    pub fn role_members(env: Env, role: Role) -> Vec<Address> {
        role_members(&env, role)
    }

    /// Grants `role` to `account`. Only an `Admin` may call this.
    pub fn grant_role(
        env: Env,
        caller: Address,
        role: Role,
        account: Address,
    ) -> Result<(), Error> {
        require_role_or_admin(&env, &caller, Role::Admin)?;
        bump_instance(&env);
        if grant_role_inner(&env, role, &account) {
            events::grant_role(&env, &caller, role, &account);
        }
        Ok(())
    }

    /// Revokes `role` from `account`. Only an `Admin` may call this.
    ///
    /// Rejected with `LastAdmin` if it would remove the final `Admin`.
    /// When revoking `PriceFeeder`, any active submission for `account` is deleted
    /// and `account` is dropped from `Submitters` immediately so that revoked feeders
    /// do not continue to influence TWAP median aggregation.
    pub fn revoke_role(
        env: Env,
        caller: Address,
        role: Role,
        account: Address,
    ) -> Result<(), Error> {
        require_role_or_admin(&env, &caller, Role::Admin)?;
        bump_instance(&env);
        if revoke_role_inner(&env, role, &account)? {
            events::revoke_role(&env, &caller, role, &account);
        }
        Ok(())
    }

    /// Explicitly purges a feeder's submission data and removes them from the
    /// submitters set. Only an `Admin` may call this.
    pub fn purge_submitter(env: Env, caller: Address, feeder: Address) -> Result<(), Error> {
        require_role_or_admin(&env, &caller, Role::Admin)?;
        bump_instance(&env);
        remove_submitter(&env, &feeder);
        Ok(())
    }

    /// Moves the `Admin` role from `caller` to `new_admin` atomically, so
    /// oracle governance is never permanently stuck on a single key — see
    /// issue #192. Equivalent to `grant_role(Admin, new_admin)` followed by
    /// `revoke_role(Admin, caller)`, but as one call so the contract never
    /// passes through a state where `caller` has been revoked without
    /// `new_admin` already holding the role.
    pub fn transfer_admin(env: Env, caller: Address, new_admin: Address) -> Result<(), Error> {
        require_role_or_admin(&env, &caller, Role::Admin)?;
        bump_instance(&env);

        grant_role_inner(&env, Role::Admin, &new_admin);
        revoke_role_inner(&env, Role::Admin, &caller)?;

        events::admin_transferred(&env, &caller, &new_admin);
        Ok(())
    }

    // ── Reads ────────────────────────────────────────────────────────────

    /// Reconfigures oracle parameters and pricing settings. Admin-gated.
    ///
    /// # Authorization
    ///
    /// Only an account holding the `Admin` role (`Role::Admin`) may call this.
    /// The `caller` must authenticate the transaction via `caller.require_auth()`.
    /// Reverts with [`Error::NotAuthorized`] if `caller` does not hold the `Admin` role.
    ///
    /// # Parameters
    ///
    /// - `env`: The Soroban environment.
    /// - `caller`: Address of the admin invoking the configuration update (must authenticate).
    /// - `config`: An [`OracleConfig`] struct carrying:
    ///   - `decimals`: Fixed-point decimal precision for submitted prices (max 19
    ///     for `price: u64`; `10^20 > u64::MAX`). Reverts with
    ///     [`Error::InvalidDecimals`] if `config.decimals > 19`. To use
    ///     `decimals > 19`, widen `PriceData::price` to `u128`.
    ///   - `asset_peg`: Target asset peg identifier/format.
    ///   - `max_staleness`: Maximum allowable age in seconds for price observations before
    ///     they are deemed stale.
    ///   - `max_price`: Absolute upper-bound ceiling for submitted prices.
    ///     When non-zero, [`submit_price`](Self::submit_price) rejects any
    ///     `price > max_price` with [`Error::PriceExceedsMaxPrice`]. Set to
    ///     `0` to disable (no ceiling). Catches fat-fingered or malicious
    ///     submissions before they corrupt the TWAP window (#226).
    ///
    /// # Price Cache Invalidation
    ///
    /// When `decimals` or `asset_peg` changes relative to the currently stored
    /// config, all existing price data (`DataKey::Price`, per-feeder
    /// `DataKey::Submission` entries, and the `DataKey::Submitters` list) is
    /// cleared. This prevents stale prices submitted under the old config from
    /// being silently misinterpreted under the new parameters — the next
    /// `get_twap_price` call will return `NoPriceAvailable` until a fresh
    /// `submit_price` is made. Changes to `max_staleness` alone do not clear
    /// price data, as those do not affect price magnitude interpretation.
    ///
    /// # Errors
    ///
    /// - [`Error::NotAuthorized`]: `caller` is not an `Admin` or auth verification fails.
    /// - [`Error::InvalidDecimals`]: `config.decimals` exceeds 19 (u64 price limit).
    pub fn configure_oracle(env: Env, caller: Address, config: OracleConfig) -> Result<(), Error> {
        require_role_or_admin(&env, &caller, Role::Admin)?;

        if config.decimals > 19 {
            return Err(Error::InvalidDecimals);
        }

        if config.max_staleness == 0 {
            return Err(Error::InvalidMaxStaleness);
        }

        bump_instance(&env);

        // Check if decimals or asset_peg changed relative to existing config.
        // If so, clear all stored price data to prevent magnitude misinterpretation.
        let existing: Option<OracleConfig> = env.storage().instance().get(&DataKey::Config);
        if let Some(old) = existing {
            if old.decimals != config.decimals || old.asset_peg != config.asset_peg {
                // Clear the legacy single-value price slot.
                env.storage().instance().remove(&DataKey::Price);

                // Clear every per-feeder submission and the submitter list itself.
                // Submissions live in persistent() (see DataKey docs) so clears
                // must target persistent storage and respect the cap.
                let submitters: Vec<Address> = env
                    .storage()
                    .persistent()
                    .get(&DataKey::Submitters)
                    .unwrap_or(Vec::new(&env));
                for feeder in submitters.iter() {
                    env.storage()
                        .persistent()
                        .remove(&DataKey::Submission(feeder));
                }
                env.storage().persistent().remove(&DataKey::Submitters);
            }
        }

        env.storage().instance().set(&DataKey::Config, &config);
        events::oracle_configured(&env, &caller, config);
        Ok(())
    }

    /// Submit a price observation. Gated strictly on `PriceFeeder` — `Admin`
    /// is not sufficient, so a single admin key cannot inject a price point
    /// into the feeder set and shift the aggregated median. The admin can
    /// grant itself `PriceFeeder`, but that is an explicit, auditable on-chain
    /// action rather than an implicit super-user bypass.
    ///
    /// `price` is a fixed-point integer scaled by `10^decimals`, where
    /// `decimals` comes from the oracle's stored `OracleConfig` (set via
    /// `configure_oracle`, max 19 for `u64` — `10^20 > u64::MAX`). For
    /// example, with `decimals: 8`, a real-world price of `100.0` is
    /// submitted as `100_00000000`. `calculate_fiat_stream_payout` divides
    /// by `10^decimals` when converting a submission back to a real value,
    /// so submissions must use the same scale as the currently configured
    /// `decimals` or downstream payouts will be wrong by that scale factor.
    ///
    /// There is no fixed time-bucketed TWAP window. Instead, every
    /// feeder's most recent submission is kept independently
    /// (`DataKey::Submission` in `persistent()`) and `get_twap_price`
    /// aggregates the median (or the average of the two middle values, on an
    /// even count) across every submission still within `max_staleness`
    /// seconds of the current ledger time — see `get_twap_price` for the
    /// aggregation logic and `OracleConfig::max_staleness` for the staleness
    /// window. `Submitters` is capped at [`MAX_SUBMITTERS`] so aggregation
    /// stays bounded.
    ///
    /// Blocked while the oracle is under an emergency pause. Each feeder's
    /// submission is tracked independently (`DataKey::Submission`) and
    /// aggregated by `get_twap_price` — no single feeder's price is trusted
    /// unconditionally.
    ///
    /// If `OracleConfig::max_price` is non-zero, the submitted `price` must
    /// not exceed that ceiling; otherwise [`Error::PriceExceedsMaxPrice`] is
    /// returned before any state is mutated. This catches fat-fingered or
    /// malicious submissions at submission time rather than letting them
    /// propagate into the TWAP window — see issue #226.
    pub fn submit_price(env: Env, caller: Address, price: u64) -> Result<(), Error> {
        if is_paused(&env) {
            return Err(Error::ContractPaused);
        }
        require_role(&env, &caller, Role::PriceFeeder)?;

        if price == 0 {
            return Err(Error::InvalidPrice);
        }

        // Enforce the configured `max_price` ceiling when present. If the
        // oracle has not been configured yet there is no ceiling to enforce
        // (identical to `max_price == 0`), so submission is not blocked — see
        // issue #226.
        if let Some(config) = env.storage().instance().get::<_, OracleConfig>(&DataKey::Config) {
            if config.max_price != 0 && price > config.max_price {
                return Err(Error::PriceExceedsMaxPrice);
            }
        }

        let now = env.ledger().timestamp();
        let data = PriceData {
            price,
            updated_at: now,
        };

        bump_instance(&env);
        // Legacy single-value slot — kept only for backward compatibility with
        // any external readers of the old scalar `Price` key. `price_age` /
        // `is_price_stale` / `price_status` no longer read this; they compute
        // staleness and quorum from the per-feeder `Submission` set below, so
        // they always agree with `get_twap_price`.
        env.storage().instance().set(&DataKey::Price, &data);

        // Per-feeder submissions are in persistent() per the storage-tier rule
        // (unbounded per-entity data → persistent), with TTL extension. Capped
        // via add_submitter.
        env.storage()
            .persistent()
            .set(&DataKey::Submission(caller.clone()), &data);
        env.storage().persistent().extend_ttl(
            &DataKey::Submission(caller.clone()),
            THRESHOLD,
            EXTEND_TO,
        );
        add_submitter(&env, &caller)?;

        events::price_submitted(&env, &caller, price, now);
        Ok(())
    }

    /// Returns the current oracle price, guarded against re-entrancy.
    ///
    /// Aggregates every non-stale submission across all addresses that have
    /// ever called `submit_price` (median, or the average of the two
    /// middle values when there is an even count) rather than trusting the
    /// single most recent submitter unconditionally — see issue #194.
    ///
    /// Errors:
    /// - `OracleNotConfigured` if `configure_oracle` has not been called.
    /// - `NoPriceAvailable` if no price has been submitted yet.
    /// - `OracleStalePrice` if every submission on record is older than the
    ///   configured `max_staleness`.
    /// - `OracleLocked` if called while the re-entrancy guard is already held
    ///   (see the nested-lock warning on `calculate_fiat_stream_payout`).
    pub fn get_twap_price(env: Env) -> Result<u64, Error> {
        with_guard(&env, || {
            let config: OracleConfig = env
                .storage()
                .instance()
                .get(&DataKey::Config)
                .ok_or(Error::OracleNotConfigured)?;

            let submitters: Vec<Address> = env
                .storage()
                .persistent()
                .get(&DataKey::Submitters)
                .unwrap_or(Vec::new(&env));

            let now = env.ledger().timestamp();
            let mut fresh_prices: Vec<u64> = Vec::new(&env);
            let mut saw_any_submission = false;

            for feeder in submitters.iter() {
                let submission: Option<PriceData> =
                    env.storage().persistent().get(&DataKey::Submission(feeder));
                if let Some(data) = submission {
                    saw_any_submission = true;
                    let age = now.saturating_sub(data.updated_at);
                    if age <= config.max_staleness && data.price != 0 {
                        fresh_prices.push_back(data.price);
                    }
                }
            }

            if fresh_prices.is_empty() {
                if saw_any_submission {
                    return Err(Error::OracleStalePrice);
                }
                return Err(Error::NoPriceAvailable);
            }

            Ok(median(fresh_prices))
        })
    }

    /// Full price health, computed from the same per-feeder submission set
    /// that [`get_twap_price`](Self::get_twap_price) aggregates — never the
    /// legacy `DataKey::Price` scalar.
    ///
    /// Exposes the quorum (`fresh_submitter_count`) and the spread of fresh
    /// submission ages (`newest_age` / `oldest_fresh_age`) behind the median, so
    /// a caller can tell whether "the price is fresh" rests on a single feeder
    /// or a broad cluster — the information the old boolean could not convey.
    ///
    /// Errors:
    /// - `OracleNotConfigured` if `configure_oracle` has not been called.
    /// - `NoPriceAvailable` if no price has ever been submitted.
    pub fn price_status(env: Env) -> Result<PriceStatus, Error> {
        load_price_status(&env)
    }

    /// Read-only: seconds since the newest *fresh* price observation, computed
    /// from the same per-feeder set `get_twap_price` uses.
    ///
    /// Returns the age of the youngest submission still within `max_staleness`
    /// (the newest contributor to the TWAP median). Returns `0` when there are
    /// no fresh submissions — check `is_price_stale` / `price_status` to
    /// disambiguate that from a genuinely fresh age-0 price.
    ///
    /// Errors:
    /// - `OracleNotConfigured` if `configure_oracle` has not been called.
    /// - `NoPriceAvailable` if no price has ever been submitted.
    pub fn price_age(env: Env) -> Result<u64, Error> {
        load_price_status(&env).map(|s| s.newest_age)
    }

    /// Read-only: whether the aggregated price set is stale, computed from the
    /// same per-feeder submissions `get_twap_price` aggregates.
    ///
    /// Returns `true` exactly when `get_twap_price` would return
    /// `OracleStalePrice` — i.e. there are zero fresh submissions. Because it no
    /// longer keys off the single most-recent `DataKey::Price` scalar, one
    /// freshly-updated feeder can no longer mask a set whose median is driven by
    /// other (older but still fresh) submissions.
    ///
    /// Errors:
    /// - `OracleNotConfigured` if `configure_oracle` has not been called.
    /// - `NoPriceAvailable` if no price has ever been submitted.
    pub fn is_price_stale(env: Env) -> Result<bool, Error> {
        load_price_status(&env).map(|s| s.stale)
    }

    /// Converts a nominal token amount into its fiat equivalent.
    ///
    /// **Nested-lock warning:** This method calls `get_twap_price`
    /// internally, which acquires and releases its own re-entrancy guard.
    /// If this method is ever wrapped in an outer guard (e.g., via a
    /// `with_guard` call), the depth counter will already be at 1 and the
    /// nested `get_twap_price` call will deadlock with `OracleLocked`.
    /// Do NOT add a guard to this function without first refactoring
    /// `get_twap_price` to accept an optional pre-acquired lock.
    pub fn calculate_fiat_stream_payout(env: Env, token_amount: u64) -> Result<u64, Error> {
        let current_price = Self::get_twap_price(env.clone())?;

        let config: OracleConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(Error::OracleNotConfigured)?;

        let precision = 10u128
            .checked_pow(config.decimals)
            .ok_or(Error::InvalidDecimals)?;

        let value = (token_amount as u128)
            .checked_mul(current_price as u128)
            .ok_or(Error::ArithmeticOverflow)?
            .checked_div(precision)
            .ok_or(Error::ArithmeticOverflow)?;

        if value > u64::MAX as u128 {
            return Err(Error::ArithmeticOverflow);
        }

        Ok(value as u64)
    }

    // ── Emergency pause (Pauser or Admin-gated) ──────────────────────────

    /// Emergency halt: freeze price submission.
    ///
    /// While paused, `submit_price` reverts with `ContractPaused` before
    /// any state is touched. Read-only methods (`get_twap_price`,
    /// `calculate_fiat_stream_payout`) continue to work with the last
    /// committed price.
    ///
    /// Gated on `Role::Pauser` (or `Admin` as super-user), so a dedicated
    /// ops wallet can halt price submission without holding a full `Admin`
    /// key — mirrors `DripGovernor::governor_pause` and closes the
    /// delegation gap noted in the audit.
    pub fn pause(env: Env, caller: Address) -> Result<(), Error> {
        require_role_or_admin(&env, &caller, Role::Pauser)?;
        if is_paused(&env) {
            return Err(Error::AlreadyPaused);
        }
        bump_instance(&env);
        set_paused(&env, true);
        events::paused(&env, &caller, env.ledger().timestamp());
        Ok(())
    }

    /// Lift the emergency pause, allowing `submit_price` again.
    ///
    /// Gated on `Role::Pauser` (or `Admin`).
    pub fn unpause(env: Env, caller: Address) -> Result<(), Error> {
        require_role_or_admin(&env, &caller, Role::Pauser)?;
        if !is_paused(&env) {
            return Err(Error::NotPaused);
        }
        bump_instance(&env);
        set_paused(&env, false);
        events::unpaused(&env, &caller, env.ledger().timestamp());
        Ok(())
    }

    /// Read-only: whether the oracle is currently under an emergency pause.
    pub fn is_paused(env: Env) -> bool {
        is_paused(&env)
    }
}

// ── Internal RBAC helpers (delegate to drip_common::rbac) ─────────────────
//
// These thin wrappers translate the oracle's DataKey / Role types into the
// generic key values expected by drip_common::rbac, keeping every RBAC logic
// change in one place (the shared crate) rather than duplicated here.

fn role_key(role: Role, account: &Address) -> DataKey {
    DataKey::Role(RoleKey {
        role,
        account: account.clone(),
    })
}

fn has_role(env: &Env, role: Role, account: &Address) -> bool {
    rbac::has_role(env, &role_key(role, account))
}

/// Returns every account currently holding `role` from the instance-storage index.
fn role_members(env: &Env, role: Role) -> Vec<Address> {
    rbac::role_members(env, &DataKey::RoleMembers(role))
}

fn grant_role_inner(env: &Env, role: Role, account: &Address) -> bool {
    rbac::grant(
        env,
        &role_key(role, account),
        &DataKey::AdminCount,
        &DataKey::RoleMembers(role),
        role == Role::Admin,
        account,
    )
}

/// Revokes `role` from `account`.
///
/// When the role is `PriceFeeder`, also purges the feeder's submission data
/// via `remove_submitter` — an oracle-specific side-effect kept here rather
/// than in the shared crate.
fn revoke_role_inner(env: &Env, role: Role, account: &Address) -> Result<bool, Error> {
    let removed = rbac::revoke(
        env,
        &role_key(role, account),
        &DataKey::AdminCount,
        &DataKey::RoleMembers(role),
        role == Role::Admin,
        account,
    )
    .map_err(|e| match e {
        rbac::RbacError::LastAdmin => Error::LastAdmin,
        rbac::RbacError::NotAuthorized => Error::NotAuthorized,
    })?;

    // Oracle-specific: clean up submission data when a PriceFeeder is revoked.
    if removed && role == Role::PriceFeeder {
        remove_submitter(env, account);
    }

    Ok(removed)
}

/// Requires that `caller` authorized the transaction and holds `role`
/// specifically. Unlike [`require_role_or_admin`], being `Admin` is **not**
/// sufficient — the role grant itself is checked. `submit_price` uses this
/// so the admin key alone cannot inject a price point into the feeder set
/// and pull the aggregated median (see
/// [`TwapOracle::submit_price`](crate::TwapOracle::submit_price)).
fn require_role(env: &Env, caller: &Address, role: Role) -> Result<(), Error> {
    caller.require_auth();
    if has_role(env, role, caller) {
        Ok(())
    } else {
        Err(Error::NotAuthorized)
    }
}

/// Requires that `caller` authorized the transaction and holds `role` **or**
/// is an `Admin`. Admin acts as a super-user.
///
/// Does NOT bump TTL here — callers call `bump_instance` at their entry point.
fn require_role_or_admin(env: &Env, caller: &Address, role: Role) -> Result<(), Error> {
    rbac::require_role_or_admin(
        env,
        caller,
        &role_key(role, caller),
        &role_key(Role::Admin, caller),
        None, // oracle bumps TTL at the entry-point level, not inside the RBAC check
    )
    .map_err(|_| Error::NotAuthorized)
}

/// Records `account` in the `Submitters` set the first time it submits a
/// price, so `get_twap_price` knows which `DataKey::Submission` entries to
/// aggregate. No-op if already recorded. Caps at [`MAX_SUBMITTERS`] and
/// returns `TooManySubmitters` if a new feeder would exceed the cap.
/// Stored in `persistent()` per the factory `storage.rs` rule (unbounded
/// per-entity data → persistent, not instance).
fn add_submitter(env: &Env, account: &Address) -> Result<(), Error> {
    let mut submitters: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKey::Submitters)
        .unwrap_or(Vec::new(env));

    for existing in submitters.iter() {
        if existing == *account {
            return Ok(());
        }
    }

    if submitters.len() >= MAX_SUBMITTERS {
        return Err(Error::TooManySubmitters);
    }

    submitters.push_back(account.clone());
    env.storage()
        .persistent()
        .set(&DataKey::Submitters, &submitters);
    env.storage()
        .persistent()
        .extend_ttl(&DataKey::Submitters, THRESHOLD, EXTEND_TO);
    Ok(())
}

/// Removes `account` from `Submitters` and deletes `DataKey::Submission(account)`.
/// Also refreshes or removes the legacy `DataKey::Price` if needed.
fn remove_submitter(env: &Env, account: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::Submission(account.clone()));

    let submitters: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKey::Submitters)
        .unwrap_or(Vec::new(env));

    let mut found = false;
    let mut updated: Vec<Address> = Vec::new(env);
    for s in submitters.iter() {
        if s == *account {
            found = true;
        } else {
            updated.push_back(s);
        }
    }

    if found {
        if updated.is_empty() {
            env.storage().persistent().remove(&DataKey::Submitters);
            env.storage().instance().remove(&DataKey::Price);
        } else {
            env.storage()
                .persistent()
                .set(&DataKey::Submitters, &updated);
            env.storage()
                .persistent()
                .extend_ttl(&DataKey::Submitters, THRESHOLD, EXTEND_TO);

            // Refresh legacy single-value price slot to the most recent remaining submission
            let mut latest: Option<PriceData> = None;
            for feeder in updated.iter() {
                if let Some(sub) = env
                    .storage()
                    .persistent()
                    .get::<_, PriceData>(&DataKey::Submission(feeder))
                {
                    if let Some(ref cur) = latest {
                        if sub.updated_at > cur.updated_at {
                            latest = Some(sub);
                        }
                    } else {
                        latest = Some(sub);
                    }
                }
            }
            if let Some(latest_price) = latest {
                env.storage().instance().set(&DataKey::Price, &latest_price);
            } else {
                env.storage().instance().remove(&DataKey::Price);
            }
        }
    }
}

/// Median of `prices` (average of the two middle values for an even-length
/// input). Sorts a copy in place with insertion sort — the submitter count
/// is expected to stay small, so O(n^2) is not a concern.
///
/// Panics if `prices` is empty; callers must guarantee at least one value.
fn median(prices: Vec<u64>) -> u64 {
    let len = prices.len();
    let mut sorted = prices.clone();

    let mut i: u32 = 1;
    while i < len {
        let key = sorted.get(i).unwrap();
        let mut j = i;
        while j > 0 {
            let prev = sorted.get(j - 1).unwrap();
            if prev > key {
                sorted.set(j, prev);
                j -= 1;
            } else {
                break;
            }
        }
        sorted.set(j, key);
        i += 1;
    }

    let mid = len / 2;
    if len & 1 == 0 {
        let a = sorted.get(mid - 1).unwrap() as u128;
        let b = sorted.get(mid).unwrap() as u128;
        ((a + b) / 2) as u64
    } else {
        sorted.get(mid).unwrap()
    }
}

/// Compute price health from the same per-feeder submission set that
/// [`get_twap_price`](TwapOracle::get_twap_price) aggregates.
///
/// Unlike the legacy `DataKey::Price` scalar (overwritten on every submission
/// with whichever feeder submitted last), this reflects the whole fresh-feeder
/// set, so the reported status can never disagree with the median. It applies
/// the identical freshness filter as `get_twap_price` (`age <= max_staleness`
/// **and** `price != 0`), so `stale == true` is exactly the condition under
/// which `get_twap_price` returns `Error::OracleStalePrice`.
///
/// Errors:
/// - `OracleNotConfigured` if `configure_oracle` has not been called.
/// - `NoPriceAvailable` if no feeder has ever submitted (kept distinct from
///   "all submissions are stale", mirroring `get_twap_price`).
fn load_price_status(env: &Env) -> Result<PriceStatus, Error> {
    let config: OracleConfig = env
        .storage()
        .instance()
        .get(&DataKey::Config)
        .ok_or(Error::OracleNotConfigured)?;

    let submitters: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKey::Submitters)
        .unwrap_or(Vec::new(env));

    let now = env.ledger().timestamp();
    let max_staleness = config.max_staleness;

    let mut fresh_submitter_count: u32 = 0;
    let mut newest_age: u64 = 0;
    let mut oldest_fresh_age: u64 = 0;
    let mut saw_any_submission = false;

    for feeder in submitters.iter() {
        let submission: Option<PriceData> =
            env.storage().persistent().get(&DataKey::Submission(feeder));
        if let Some(data) = submission {
            saw_any_submission = true;
            let age = now.saturating_sub(data.updated_at);
            if age <= max_staleness && data.price != 0 {
                if fresh_submitter_count == 0 {
                    newest_age = age;
                    oldest_fresh_age = age;
                } else {
                    newest_age = newest_age.min(age);
                    oldest_fresh_age = oldest_fresh_age.max(age);
                }
                fresh_submitter_count += 1;
            }
        }
    }

    if !saw_any_submission {
        return Err(Error::NoPriceAvailable);
    }

    Ok(PriceStatus {
        fresh_submitter_count,
        newest_age,
        oldest_fresh_age,
        stale: fresh_submitter_count == 0,
    })
}

fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false)
}

fn set_paused(env: &Env, paused: bool) {
    env.storage().instance().set(&DataKey::Paused, &paused);
}

/// Execute `f` under the re-entrancy guard, releasing the lock afterwards.
///
/// Uses a depth counter stored at the `O_Lock` symbol key instead of a
/// boolean flag. See `drip_stream::state::with_guard` for the rationale.
const MAX_REENTRANCY_DEPTH: u32 = 1;

fn with_guard<R>(env: &Env, f: impl FnOnce() -> Result<R, Error>) -> Result<R, Error> {
    let lock_key = soroban_sdk::symbol_short!("O_Lock");
    let depth: u32 = env.storage().instance().get(&lock_key).unwrap_or(0);
    if depth >= MAX_REENTRANCY_DEPTH {
        return Err(Error::OracleLocked);
    }
    env.storage().instance().set(&lock_key, &(depth + 1));
    let result = f();
    let d: u32 = env.storage().instance().get(&lock_key).unwrap_or(1);
    if d > 0 {
        env.storage().instance().set(&lock_key, &(d - 1));
    }
    result
}

// ── Events ────────────────────────────────────────────────────────────────

mod events {
    use soroban_sdk::{symbol_short, Address, Env};

    use super::Role;

    pub fn grant_role(env: &Env, caller: &Address, role: Role, account: &Address) {
        env.events().publish(
            (symbol_short!("grant"), caller.clone()),
            (role, account.clone()),
        );
    }

    pub fn revoke_role(env: &Env, caller: &Address, role: Role, account: &Address) {
        env.events().publish(
            (symbol_short!("revoke"), caller.clone()),
            (role, account.clone()),
        );
    }

    pub fn paused(env: &Env, caller: &Address, paused_at: u64) {
        env.events()
            .publish((symbol_short!("paused"), caller.clone()), paused_at);
    }

    pub fn unpaused(env: &Env, caller: &Address, resumed_at: u64) {
        env.events()
            .publish((symbol_short!("unpaused"), caller.clone()), resumed_at);
    }

    pub fn admin_transferred(env: &Env, old_admin: &Address, new_admin: &Address) {
        env.events().publish(
            (symbol_short!("adm_xfer"), old_admin.clone()),
            new_admin.clone(),
        );
    }

    pub fn price_submitted(env: &Env, caller: &Address, price: u64, timestamp: u64) {
        env.events().publish(
            (symbol_short!("priced"), caller.clone()),
            (price, timestamp),
        );
    }

    pub fn oracle_configured(env: &Env, caller: &Address, config: super::OracleConfig) {
        env.events()
            .publish((symbol_short!("ocfg"), caller.clone()), config);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use soroban_sdk::testutils::storage::Instance as _;
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        Address, Env,
    };

    use super::*;

    fn setup() -> (Env, TwapOracleClient<'static>, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_id = env.register_contract(None, TwapOracle);
        let client = TwapOracleClient::new(&env, &contract_id);
        (env, client, admin)
    }

    /// Grants `admin` the `PriceFeeder` role. `submit_price` requires the
    /// role specifically (Admin alone is rejected), so tests that submit
    /// prices as the admin must grant the role first.
    fn grant_admin_feeder(client: &TwapOracleClient<'static>, admin: &Address) {
        client.grant_role(admin, &Role::PriceFeeder, admin);
    }

    #[test]
    fn initialize_sets_admin_and_grants_role() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        assert!(client.has_role(&Role::Admin, &admin));
        // Second init should fail
        let result = client.try_initialize(&admin);
        assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
    }

    #[test]
    fn configure_oracle_requires_admin_role() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let non_admin = Address::generate(&env);
        let result = client.try_configure_oracle(&non_admin, &config);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    #[test]
    fn configure_oracle_rejects_excessive_decimals() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 39,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        let result = client.try_configure_oracle(&admin, &config);
        assert_eq!(result, Err(Ok(Error::InvalidDecimals)));
    }

    #[test]
    fn configure_oracle_rejects_zero_max_staleness() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 0,
            max_price: 0,
        };
        let result = client.try_configure_oracle(&admin, &config);
        assert_eq!(result, Err(Ok(Error::InvalidMaxStaleness)));
    }

    #[test]
    fn submit_price_requires_price_feeder_role() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let result = client.try_submit_price(&Address::generate(&env), &100);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    #[test]
    fn submit_price_rejects_non_admin_non_feeder() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let impostor = Address::generate(&env);

        let result = client.try_submit_price(&impostor, &100);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));

        client.grant_role(&admin, &Role::PriceFeeder, &impostor);
        let result = client.try_submit_price(&impostor, &100);
        assert!(result.is_ok());
    }

    #[test]
    fn admin_without_price_feeder_role_cannot_submit_price() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        // Admin alone is not a price feeder — must hold PriceFeeder explicitly.
        let result = client.try_submit_price(&admin, &100);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));

        client.grant_role(&admin, &Role::PriceFeeder, &admin);
        let result = client.try_submit_price(&admin, &100);
        assert!(result.is_ok());
    }

    #[test]
    fn price_feeder_can_submit_price() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let feeder = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder);

        let result = client.try_submit_price(&feeder, &100);
        assert!(result.is_ok());
    }

    #[test]
    fn submit_price_rejects_zero() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let result = client.try_submit_price(&admin, &0);
        assert_eq!(result, Err(Ok(Error::InvalidPrice)));
    }

    // ── Issue #226: max_price upper-bound sanity check ──────────────────

    #[test]
    fn submit_price_rejects_price_above_max_price() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 1_000_000_000_000_000_000,
        };
        client.configure_oracle(&admin, &config);

        let feeder = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder);

        // A price just above the ceiling must be rejected.
        let result = client.try_submit_price(&feeder, &1_000_000_000_000_000_001);
        assert_eq!(result, Err(Ok(Error::PriceExceedsMaxPrice)));

        // A wildly out-of-range price (orders of magnitude too large) is also rejected.
        let result = client.try_submit_price(&feeder, &u64::MAX);
        assert_eq!(result, Err(Ok(Error::PriceExceedsMaxPrice)));
    }

    #[test]
    fn submit_price_accepts_price_at_or_below_max_price() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 1_000_000_000_000_000_000,
        };
        client.configure_oracle(&admin, &config);

        // Exactly at the ceiling is allowed.
        client.submit_price(&admin, &1_000_000_000_000_000_000);
        assert_eq!(
            client.get_twap_price(),
            1_000_000_000_000_000_000
        );

        // Below the ceiling is allowed.
        client.submit_price(&admin, &500_000_000);
        assert_eq!(client.get_twap_price(), 500_000_000);
    }

    #[test]
    fn submit_price_accepts_any_price_when_max_price_is_zero() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 0,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        // `max_price == 0` disables the ceiling, so even u64::MAX is accepted.
        client.submit_price(&admin, &u64::MAX);
        assert_eq!(client.get_twap_price(), u64::MAX);
    }

    #[test]
    fn submit_price_without_config_has_no_ceiling() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        // Before `configure_oracle`, there is no ceiling to enforce, so
        // submission still succeeds (backward-compatible behavior).
        let result = client.try_submit_price(&admin, &100);
        assert!(result.is_ok());
    }

    #[test]
    fn get_twap_price_requires_config() {
        let (_env, client, _admin) = setup();
        let result = client.try_get_twap_price();
        assert_eq!(result, Err(Ok(Error::OracleNotConfigured)));
    }

    #[test]
    fn get_twap_price_requires_price_submission() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let result = client.try_get_twap_price();
        assert_eq!(result, Err(Ok(Error::NoPriceAvailable)));
    }

    #[test]
    fn get_twap_price_rejects_stale_price() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 60,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        client.submit_price(&admin, &50_000_000);

        env.ledger().set(LedgerInfo {
            timestamp: 1_000_000 + 61,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        let result = client.try_get_twap_price();
        assert_eq!(result, Err(Ok(Error::OracleStalePrice)));
    }

    #[test]
    fn get_twap_price_returns_fresh_price() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        client.submit_price(&admin, &50_000_000);

        let price = client.get_twap_price();
        assert_eq!(price, 50_000_000);
    }

    #[test]
    fn calculate_fiat_stream_payout_works() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        client.submit_price(&admin, &50_000_000);

        let payout = client.calculate_fiat_stream_payout(&100);
        assert_eq!(payout, 50);
    }

    #[test]
    fn calculate_fiat_stream_payout_overflow() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 0,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        client.submit_price(&admin, &u64::MAX);

        let result = client.try_calculate_fiat_stream_payout(&(u64::MAX));
        assert_eq!(result, Err(Ok(Error::ArithmeticOverflow)));
    }

    #[test]
    fn calculate_fiat_stream_payout_deadlocks_when_outer_lock_held() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);

        let payout = client.calculate_fiat_stream_payout(&100);
        assert_eq!(payout, 50);

        let lock_key = soroban_sdk::symbol_short!("O_Lock");
        env.as_contract(&client.address, || {
            env.storage().instance().set(&lock_key, &1_u32);
        });

        let result = client.try_calculate_fiat_stream_payout(&100);
        assert_eq!(result, Err(Ok(Error::OracleLocked)));
    }

    // ── Role management tests ────────────────────────────────────────────

    #[test]
    fn admin_can_grant_and_revoke_price_feeder() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let feeder = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder);
        assert!(client.has_role(&Role::PriceFeeder, &feeder));

        client.revoke_role(&admin, &Role::PriceFeeder, &feeder);
        assert!(!client.has_role(&Role::PriceFeeder, &feeder));
    }

    #[test]
    fn non_admin_cannot_grant_roles() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let stranger = Address::generate(&env);
        let result = client.try_grant_role(&stranger, &Role::PriceFeeder, &Address::generate(&env));
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    #[test]
    fn revoking_last_admin_is_rejected() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let result = client.try_revoke_role(&admin, &Role::Admin, &admin);
        assert_eq!(result, Err(Ok(Error::LastAdmin)));
    }

    #[test]
    fn admin_can_be_revoked_after_granting_second_admin() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let second = Address::generate(&env);
        client.grant_role(&admin, &Role::Admin, &second);
        client.revoke_role(&admin, &Role::Admin, &admin);

        assert!(!client.has_role(&Role::Admin, &admin));
        assert!(client.has_role(&Role::Admin, &second));
    }

    // ── Emergency pause tests ────────────────────────────────────────────

    #[test]
    fn admin_can_pause_and_unpause() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        assert!(!client.is_paused());
        client.pause(&admin);
        assert!(client.is_paused());
        client.unpause(&admin);
        assert!(!client.is_paused());
    }

    #[test]
    fn pause_blocks_submit_price() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);

        client.pause(&admin);
        let result = client.try_submit_price(&admin, &100);
        assert_eq!(result, Err(Ok(Error::ContractPaused)));
    }

    #[test]
    fn submit_price_works_after_unpause() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        client.pause(&admin);
        client.unpause(&admin);

        let result = client.try_submit_price(&admin, &100);
        assert!(result.is_ok());
    }

    #[test]
    fn double_pause_is_rejected() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        client.pause(&admin);
        let result = client.try_pause(&admin);
        assert_eq!(result, Err(Ok(Error::AlreadyPaused)));
    }

    #[test]
    fn unpause_when_not_paused_is_rejected() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let result = client.try_unpause(&admin);
        assert_eq!(result, Err(Ok(Error::NotPaused)));
    }

    #[test]
    fn get_twap_price_works_while_paused() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);

        client.pause(&admin);
        let price = client.get_twap_price();
        assert_eq!(price, 50_000_000);
    }

    // ── Role::Pauser tests ────────────────────────────────────────────────

    #[test]
    fn pauser_can_pause_and_unpause_without_admin() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let pauser = Address::generate(&env);
        // pauser holds only Role::Pauser — not Admin
        client.grant_role(&admin, &Role::Pauser, &pauser);
        assert!(!client.has_role(&Role::Admin, &pauser));

        assert!(!client.is_paused());
        client.pause(&pauser);
        assert!(client.is_paused());
        client.unpause(&pauser);
        assert!(!client.is_paused());
    }

    #[test]
    fn non_pauser_non_admin_cannot_pause() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let stranger = Address::generate(&env);
        let result = client.try_pause(&stranger);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    #[test]
    fn non_pauser_non_admin_cannot_unpause() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        client.pause(&admin);

        let stranger = Address::generate(&env);
        let result = client.try_unpause(&stranger);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    #[test]
    fn price_feeder_cannot_pause() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let feeder = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder);

        // PriceFeeder does not imply Pauser authority
        let result = client.try_pause(&feeder);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    #[test]
    fn pauser_revoked_can_no_longer_pause() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let pauser = Address::generate(&env);
        client.grant_role(&admin, &Role::Pauser, &pauser);
        client.revoke_role(&admin, &Role::Pauser, &pauser);

        let result = client.try_pause(&pauser);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    // ── RoleMembers index tests ───────────────────────────────────────────

    #[test]
    fn role_members_empty_before_any_grants() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        // PriceFeeder index is empty until someone is granted that role
        let members = client.role_members(&Role::PriceFeeder);
        assert_eq!(members.len(), 0);
    }

    #[test]
    fn role_members_tracks_admin_from_initialize() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let members = client.role_members(&Role::Admin);
        assert_eq!(members.len(), 1);
        assert_eq!(members.get(0).unwrap(), admin);
    }

    #[test]
    fn role_members_grows_on_grant() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let f1 = Address::generate(&env);
        let f2 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.grant_role(&admin, &Role::PriceFeeder, &f2);

        let members = client.role_members(&Role::PriceFeeder);
        assert_eq!(members.len(), 2);
        assert!(members.iter().any(|m| m == f1));
        assert!(members.iter().any(|m| m == f2));
    }

    #[test]
    fn role_members_shrinks_on_revoke() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let f1 = Address::generate(&env);
        let f2 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.grant_role(&admin, &Role::PriceFeeder, &f2);
        client.revoke_role(&admin, &Role::PriceFeeder, &f1);

        let members = client.role_members(&Role::PriceFeeder);
        assert_eq!(members.len(), 1);
        assert_eq!(members.get(0).unwrap(), f2);
    }

    #[test]
    fn role_members_admin_index_updated_on_transfer() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let new_admin = Address::generate(&env);
        client.transfer_admin(&admin, &new_admin);

        let members = client.role_members(&Role::Admin);
        assert_eq!(members.len(), 1);
        assert_eq!(members.get(0).unwrap(), new_admin);
    }

    #[test]
    fn role_members_pauser_index_maintained() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let pauser = Address::generate(&env);
        client.grant_role(&admin, &Role::Pauser, &pauser);

        let members = client.role_members(&Role::Pauser);
        assert_eq!(members.len(), 1);
        assert_eq!(members.get(0).unwrap(), pauser);

        client.revoke_role(&admin, &Role::Pauser, &pauser);
        let members = client.role_members(&Role::Pauser);
        assert_eq!(members.len(), 0);
    }

    #[test]
    fn grant_is_idempotent_and_does_not_double_count_in_index() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let feeder = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder);
        // Granting again is a no-op — index must not grow
        client.grant_role(&admin, &Role::PriceFeeder, &feeder);

        let members = client.role_members(&Role::PriceFeeder);
        assert_eq!(members.len(), 1);
    }

    // ── Admin rotation tests (#192) ───────────────────────────────────────

    #[test]
    fn transfer_admin_moves_role_and_revokes_old() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let new_admin = Address::generate(&env);
        client.transfer_admin(&admin, &new_admin);

        assert!(client.has_role(&Role::Admin, &new_admin));
        assert!(!client.has_role(&Role::Admin, &admin));
    }

    #[test]
    fn transfer_admin_requires_admin_role() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let stranger = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let result = client.try_transfer_admin(&stranger, &new_admin);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
    }

    #[test]
    fn new_admin_can_act_immediately_after_transfer() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let new_admin = Address::generate(&env);
        client.transfer_admin(&admin, &new_admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        let result = client.try_configure_oracle(&admin, &config);
        assert_eq!(result, Err(Ok(Error::NotAuthorized)));
        client.configure_oracle(&new_admin, &config);
    }

    // ── Multi-feeder aggregation tests (#194) ─────────────────────────────

    #[test]
    fn get_twap_price_aggregates_median_across_feeders() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let feeder_b = Address::generate(&env);
        let feeder_c = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder_b);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder_c);

        client.submit_price(&admin, &10);
        client.submit_price(&feeder_b, &20);
        client.submit_price(&feeder_c, &30);

        let price = client.get_twap_price();
        assert_eq!(price, 20);
    }

    #[test]
    fn get_twap_price_averages_middle_two_on_even_count() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let feeder_b = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder_b);

        client.submit_price(&admin, &10);
        client.submit_price(&feeder_b, &20);

        let price = client.get_twap_price();
        assert_eq!(price, 15);
    }

    #[test]
    fn get_twap_price_ignores_stale_submitters_in_aggregate() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 60,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let feeder_b = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &feeder_b);

        client.submit_price(&admin, &10);
        let submitted_at = env.ledger().timestamp();

        env.ledger().set(LedgerInfo {
            timestamp: submitted_at + 61,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        client.submit_price(&feeder_b, &20);

        let price = client.get_twap_price();
        assert_eq!(price, 20);
    }

    #[test]
    fn get_twap_price_errors_stale_when_all_submitters_stale() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 60,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        client.submit_price(&admin, &10);
        let submitted_at = env.ledger().timestamp();

        env.ledger().set(LedgerInfo {
            timestamp: submitted_at + 61,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        let result = client.try_get_twap_price();
        assert_eq!(result, Err(Ok(Error::OracleStalePrice)));
    }

    // ── Staleness introspection tests (#195) ──────────────────────────────

    #[test]
    fn price_age_reports_seconds_since_last_submission() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);
        let submitted_at = env.ledger().timestamp();

        assert_eq!(client.price_age(), 0);

        env.ledger().set(LedgerInfo {
            timestamp: submitted_at + 42,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        assert_eq!(client.price_age(), 42);
    }

    #[test]
    fn price_age_requires_config() {
        let (_env, client, _admin) = setup();
        let result = client.try_price_age();
        assert_eq!(result, Err(Ok(Error::OracleNotConfigured)));
    }

    #[test]
    fn price_age_requires_price_submission() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let result = client.try_price_age();
        assert_eq!(result, Err(Ok(Error::NoPriceAvailable)));
    }

    #[test]
    fn is_price_stale_reflects_max_staleness_without_erroring() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 60,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);
        let submitted_at = env.ledger().timestamp();

        assert!(!client.is_price_stale());

        env.ledger().set(LedgerInfo {
            timestamp: submitted_at + 61,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        assert!(client.is_price_stale());
    }

    #[test]
    fn price_status_reports_config_required() {
        let (_env, client, _admin) = setup();
        let result = client.try_price_status();
        assert_eq!(result, Err(Ok(Error::OracleNotConfigured)));
    }

    #[test]
    fn price_status_reports_no_price_available() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let result = client.try_price_status();
        assert_eq!(result, Err(Ok(Error::NoPriceAvailable)));
    }

    #[test]
    fn price_status_reports_quorum_and_age_spread_across_feeders() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let f1 = Address::generate(&env);
        let f2 = Address::generate(&env);
        let f3 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.grant_role(&admin, &Role::PriceFeeder, &f2);
        client.grant_role(&admin, &Role::PriceFeeder, &f3);

        let base = env.ledger().timestamp();
        // Three feeders, staggered ages: f1 oldest, f3 newest.
        client.submit_price(&f1, &100_000_000);
        env.ledger().set(LedgerInfo {
            timestamp: base + 50,
            ..env.ledger().get()
        });
        client.submit_price(&f2, &200_000_000);
        env.ledger().set(LedgerInfo {
            timestamp: base + 100,
            ..env.ledger().get()
        });
        client.submit_price(&f3, &300_000_000);

        let status = client.price_status();
        assert!(!status.stale);
        // All three are within max_staleness and contribute to the median.
        assert_eq!(status.fresh_submitter_count, 3);
        // Newest is f3 (age 0), oldest is f1 (age 100).
        assert_eq!(status.newest_age, 0);
        assert_eq!(status.oldest_fresh_age, 100);

        // The quorum is visible: this is a genuine 3-feeder median.
        assert_eq!(client.get_twap_price(), 200_000_000);
    }

    #[test]
    fn price_status_exposes_single_feeder_lack_of_quorum() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let f1 = Address::generate(&env);
        let f2 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.grant_role(&admin, &Role::PriceFeeder, &f2);

        let base = env.ledger().timestamp();
        client.submit_price(&f1, &100_000_000);
        client.submit_price(&f2, &200_000_000);

        // Advance beyond max_staleness so BOTH feeders go stale; the fresh set
        // is empty and the oracle is stale.
        env.ledger().set(LedgerInfo {
            timestamp: base + 400,
            ..env.ledger().get()
        });
        let status = client.price_status();
        assert!(status.stale);
        assert_eq!(status.fresh_submitter_count, 0);
        assert_eq!(
            client.try_get_twap_price(),
            Err(Ok(Error::OracleStalePrice))
        );

        // One feeder submits fresh; only a single feeder backs the price now.
        client.submit_price(&f2, &250_000_000);
        let status = client.price_status();
        assert!(!status.stale);
        assert_eq!(status.fresh_submitter_count, 1);
        // f2 is the newest fresh submission (age 0).
        assert_eq!(status.newest_age, 0);
        assert_eq!(status.oldest_fresh_age, 0);
    }

    #[test]
    fn is_price_stale_agrees_with_get_twap_price() {
        // Proves the invariant the old scalar key broke: `is_price_stale() == true`
        // exactly when `get_twap_price` returns OracleStalePrice.
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let f1 = Address::generate(&env);
        let f2 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.grant_role(&admin, &Role::PriceFeeder, &f2);

        let base = env.ledger().timestamp();
        client.submit_price(&f1, &100_000_000);
        client.submit_price(&f2, &200_000_000);

        // All stale -> both the boolean and the twap error agree.
        env.ledger().set(LedgerInfo {
            timestamp: base + 400,
            ..env.ledger().get()
        });
        assert!(client.is_price_stale());
        assert_eq!(
            client.try_get_twap_price(),
            Err(Ok(Error::OracleStalePrice))
        );

        // One fresh feeder -> neither is stale, and a price is readable.
        client.submit_price(&f2, &250_000_000);
        assert!(!client.is_price_stale());
        assert_eq!(client.get_twap_price(), 250_000_000);
    }

    // ── TTL extension tests (#189) ──────────────────────────────────────

    #[test]
    fn initialize_extends_instance_ttl() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
        assert!(ttl >= 100_000, "instance TTL after initialize: {ttl}");
    }

    #[test]
    fn configure_oracle_extends_instance_ttl() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
        assert!(ttl >= 100_000, "instance TTL after configure_oracle: {ttl}");
    }

    #[test]
    fn submit_price_extends_instance_ttl() {
        let (env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);
        client.submit_price(&admin, &50_000_000);

        let ttl = env.as_contract(&client.address, || env.storage().instance().get_ttl());
        assert!(ttl >= 100_000, "instance TTL after submit_price: {ttl}");
    }

    // ── Issue #206: configure_oracle clears stale price on decimals change ────

    #[test]
    fn configure_oracle_clears_price_when_decimals_change() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);

        let price = client.get_twap_price();
        assert_eq!(price, 50_000_000);

        let new_config = OracleConfig {
            decimals: 6,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &new_config);

        let result = client.try_get_twap_price();
        assert_eq!(result, Err(Ok(Error::NoPriceAvailable)));
    }

    #[test]
    fn configure_oracle_clears_price_when_asset_peg_changes() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);

        let price = client.get_twap_price();
        assert_eq!(price, 50_000_000);

        let new_config = OracleConfig {
            decimals: 8,
            asset_peg: 2,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &new_config);

        let result = client.try_get_twap_price();
        assert_eq!(result, Err(Ok(Error::NoPriceAvailable)));
    }

    #[test]
    fn configure_oracle_preserves_price_when_only_staleness_changes() {
        let (_env, client, admin) = setup();
        client.initialize(&admin);
        grant_admin_feeder(&client, &admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);
        client.submit_price(&admin, &50_000_000);

        let price = client.get_twap_price();
        assert_eq!(price, 50_000_000);

        let new_config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 600,
            max_price: 0,
        };
        client.configure_oracle(&admin, &new_config);

        let price_after = client.get_twap_price();
        assert_eq!(price_after, 50_000_000);
    }

    // ── PriceFeeder revocation / purge cleanup tests ───────────────────────

    #[test]
    fn feeder_revocation_immediately_removes_submission_from_twap_median() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let f1 = Address::generate(&env);
        let f2 = Address::generate(&env);
        let f3 = Address::generate(&env);

        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.grant_role(&admin, &Role::PriceFeeder, &f2);
        client.grant_role(&admin, &Role::PriceFeeder, &f3);

        client.submit_price(&f1, &100_000_000);
        client.submit_price(&f2, &200_000_000);
        client.submit_price(&f3, &300_000_000);

        // Median of [100, 200, 300] = 200
        assert_eq!(client.get_twap_price(), 200_000_000);

        // Revoke f3 — should immediately drop f3's submission from aggregation
        client.revoke_role(&admin, &Role::PriceFeeder, &f3);

        // Median of [100, 200] = (100 + 200) / 2 = 150
        assert_eq!(client.get_twap_price(), 150_000_000);

        // Revoke f1 — now only f2 remains
        client.revoke_role(&admin, &Role::PriceFeeder, &f1);
        assert_eq!(client.get_twap_price(), 200_000_000);
    }

    #[test]
    fn revoking_sole_feeder_leaves_no_price_available() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let f1 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.submit_price(&f1, &100_000_000);

        assert_eq!(client.get_twap_price(), 100_000_000);

        client.revoke_role(&admin, &Role::PriceFeeder, &f1);
        let result = client.try_get_twap_price();
        assert_eq!(result, Err(Ok(Error::NoPriceAvailable)));
    }

    #[test]
    fn purge_submitter_removes_feeder_submission_and_requires_admin() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let f1 = Address::generate(&env);
        let f2 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.grant_role(&admin, &Role::PriceFeeder, &f2);

        client.submit_price(&f1, &100_000_000);
        client.submit_price(&f2, &200_000_000);

        // Median of [100, 200] = 150
        assert_eq!(client.get_twap_price(), 150_000_000);

        // Stranger cannot purge
        let stranger = Address::generate(&env);
        let res = client.try_purge_submitter(&stranger, &f1);
        assert_eq!(res, Err(Ok(Error::NotAuthorized)));

        // Admin purges f1
        client.purge_submitter(&admin, &f1);
        assert_eq!(client.get_twap_price(), 200_000_000);
    }

    #[test]
    fn revoked_feeder_re_granted_can_submit_fresh_price() {
        let (env, client, admin) = setup();
        client.initialize(&admin);

        let config = OracleConfig {
            decimals: 8,
            asset_peg: 1,
            max_staleness: 300,
            max_price: 0,
        };
        client.configure_oracle(&admin, &config);

        let f1 = Address::generate(&env);
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.submit_price(&f1, &100_000_000);
        assert_eq!(client.get_twap_price(), 100_000_000);

        // Revoke
        client.revoke_role(&admin, &Role::PriceFeeder, &f1);
        assert_eq!(
            client.try_get_twap_price(),
            Err(Ok(Error::NoPriceAvailable))
        );

        // Re-grant and submit
        client.grant_role(&admin, &Role::PriceFeeder, &f1);
        client.submit_price(&f1, &250_000_000);
        assert_eq!(client.get_twap_price(), 250_000_000);
    }
}
