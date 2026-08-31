#![no_std]

mod errors;
mod events;
mod math;
mod state;
pub mod storage;
#[cfg(test)]
mod tests;
mod ttl;

use soroban_sdk::{contract, contractimpl, panic_with_error, token, Address, Env};

use drip_common::is_zero_address;

pub use errors::Error;
use storage::{DataKey, StreamInfo, FLAG_CANCELLED, FLAG_CLAWBACK_ENABLED, FLAG_PAUSED};

#[contract]
pub struct DripStream;

/// Check that `caller` is either the stream's `sender` or a delegated
/// `operator`, then consume the caller's auth. Returns `NotAuthorized`
/// when `caller` matches neither role or fails the auth check.
fn require_sender_or_operator(env: &Env, caller: &Address, sender: &Address) -> Result<(), Error> {
    let operator: Option<Address> = env.storage().instance().get(&DataKey::Operator);
    match operator {
        Some(op) => {
            if caller == sender || caller == &op {
                caller.require_auth();
                Ok(())
            } else {
                Err(Error::NotAuthorized)
            }
        }
        None => {
            if caller != sender {
                Err(Error::NotAuthorized)
            } else {
                caller.require_auth();
                Ok(())
            }
        }
    }
}

#[contractimpl]
impl DripStream {
    /// Called once by the factory after deployment.
    ///
    /// Guards against re-initialization: without this check, anyone could
    /// call `initialize` again on an already-funded stream to overwrite
    /// `Sender`/`Recipient` and then drain the escrowed balance via
    /// `cancel()`/`clawback()`.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        sender: Address,
        recipient: Address,
        token: Address,
        rate_per_second: i128,
        start_time: u64,
        end_time: u64,
        clawback_enabled: bool,
    ) {
        if env.storage().instance().has(&DataKey::Config) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }

        // Fail early on empty streams: a zero (or negative) rate would
        // create a stream that escrows tokens but never releases any —
        // an "empty stream". The factory validates this before deploying,
        // but a DripStream can also be deployed and initialized directly
        // (ADR-001: one contract per stream), so this contract must
        // enforce the amount check itself rather than trusting the caller.
        if rate_per_second <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        // ADR-001 permits a DripStream to be initialized directly (not only
        // through the factory), so this contract must independently enforce the
        // recipient invariants `create_stream` checks before deploying. Without
        // these a direct `initialize` call could:
        //   * escrow funds to an unspendable zero-address recipient, or
        //   * create a self-stream (recipient == sender).
        // `is_zero_address` is the exact same helper the factory uses
        // (contracts/common/src/lib.rs), so both paths reject identical inputs.
        if is_zero_address(&env, &recipient) || recipient == sender {
            panic_with_error!(&env, Error::InvalidRecipient);
        }

        // Boundary check on the time range before any state is written.
        // A bounded stream (`end_time > 0`) whose `end_time` is not strictly
        // after `start_time` is malformed: it either streams nothing
        // (`end_time == start_time`) or, worse, becomes permanently stuck.
        // With `end_time < start_time`, once ledger time passes `start_time`
        // the release math computes `end_time - start_time` on a `u64`, which
        // underflows and surfaces as `ArithmeticOverflow`. That error then
        // fires in `withdraw`, `cancel`, and `clawback` alike, so the escrowed
        // balance can be neither withdrawn nor refunded — the funds are locked
        // forever. Reject the malformed payload here, before it is persisted,
        // rather than letting the bad state mutate storage. Open-ended streams
        // (`end_time == 0`) are unaffected and remain valid.
        if end_time > 0 && end_time <= start_time {
            panic_with_error!(&env, Error::InvalidTimeRange);
        }

        // Reject backdated start times so a directly-initialized stream cannot
        // already be "running" at creation (the recipient could immediately
        // drain a lump sum). Mirrors `create_stream`'s backdated-start guard;
        // `start_time == now` is allowed (a stream starting exactly now).
        let now = env.ledger().timestamp();
        if start_time < now {
            panic_with_error!(&env, Error::BackdatedStream);
        }

        ttl::bump(&env);

        let mut flags: u32 = 0;
        if clawback_enabled {
            flags |= FLAG_CLAWBACK_ENABLED;
        }

        let s = env.storage().instance();
        s.set(&DataKey::EventSequence, &0_u64);
        s.set(&DataKey::StorageVersion, &storage::CURRENT_STORAGE_VERSION);

        events::created(
            &env,
            &sender,
            &recipient,
            &token,
            rate_per_second,
            start_time,
            end_time,
        );

        // Write the entire stream state as a single struct — one storage
        // write instead of eleven. All subsequent reads go through
        // state::load(), which fetches the whole struct in one call.
        state::save(
            &env,
            &StreamInfo {
                sender,
                recipient,
                token,
                rate_per_second,
                start_time,
                end_time,
                flags,
                withdrawn: 0,
                paused_at: 0,
            },
        );
    }

    /// Recipient withdraws `amount` tokens.
    pub fn withdraw(env: Env, amount: i128) -> Result<i128, Error> {
        state::with_guard(&env, |env| Self::_withdraw(env, amount))
    }

    fn _withdraw(env: &Env, amount: i128) -> Result<i128, Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        info.recipient.require_auth();

        // `available` is the recipient's accrued-but-unwithdrawn entitlement
        // (rate * elapsed - withdrawn). If nothing has accrued yet there is
        // genuinely nothing to send.
        let available = math::withdrawable(env, &info)?;
        if available == 0 {
            return Err(Error::NothingToWithdraw);
        }

        let tk = token::Client::new(env, &info.token);
        let contract_addr = env.current_contract_address();

        // Clamp the payout to the real tokens the contract actually holds.
        // For an open-ended stream (`end_time == 0`) accrual is unbounded while
        // the funded balance is whatever `top_up` added, so `available` can
        // exceed `balance`; without this clamp the `transfer` below reverts and
        // blocks *every* withdrawal — even the portion that is funded.
        //
        // `balance` is read exactly once and reused for both the clamp and the
        // post-transfer `remaining` figure so a fee-on-transfer / rebasing token
        // can never feed a stale subtraction.
        let balance = tk.balance(&contract_addr);
        let to_send = amount.min(available).min(balance);

        // `available > 0` (checked above) and `amount > 0`, so a zero `to_send`
        // here is solely the balance clamp — the stream accrued but is not
        // funded. Distinguish that from `NothingToWithdraw` ("nothing accrued").
        if to_send == 0 {
            return Err(Error::StreamUnderfunded);
        }

        let new_withdrawn = info
            .withdrawn
            .checked_add(to_send)
            .ok_or(Error::ArithmeticOverflow)?;

        let mut updated = info.clone();
        updated.withdrawn = new_withdrawn;
        state::save(env, &updated);

        // Perform the transfer, then derive `remaining` from the single balance
        // captured above. `checked_sub` (rather than a bare `-`) guarantees no
        // underflow panic even if a fee-on-transfer / rebasing token leaves the
        // contract with less than `to_send`; the worst case is a conservative 0.
        tk.transfer(&contract_addr, &info.recipient, &to_send);

        let remaining = balance.checked_sub(to_send).unwrap_or(0);
        events::withdrawn(env, &info.recipient, to_send, new_withdrawn, remaining);
        Ok(to_send)
    }

    /// Sender (or operator) cancels the stream.
    ///
    /// Settles everything atomically:
    ///   - Tokens the recipient has earned (but not yet withdrawn) are sent
    ///     directly to the recipient.
    ///   - The remaining unstreamed balance is refunded to the sender.
    ///
    /// After cancellation, `withdraw()` is blocked (`StreamCancelled`), so
    /// the recipient's share MUST be transferred here rather than left for
    /// a later `withdraw()` call.
    pub fn cancel(env: Env, caller: Address) -> Result<(), Error> {
        state::with_guard(&env, |env| Self::_cancel(env, &caller))
    }

    fn _cancel(env: &Env, caller: &Address) -> Result<(), Error> {
        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        require_sender_or_operator(env, caller, &info.sender)?;

        let tk = token::Client::new(env, &info.token);
        let contract_addr = env.current_contract_address();
        let balance = tk.balance(&contract_addr);

        // How many tokens the recipient has earned but not yet withdrawn.
        let streamed = math::streamed_amount(env, &info)?;
        let owed_to_recipient = (streamed - info.withdrawn).max(0).min(balance);
        let refund_to_sender = (balance - owed_to_recipient).max(0);

        // Commit the cancelled state before any transfers (state-machine
        // correctness; Soroban already prevents re-entrancy on its own).
        //
        // `withdrawn` is advanced by the amount `cancel` pays the recipient
        // here, so `info().withdrawn` after cancellation reflects every token
        // the recipient actually received — not just the ones they pulled via
        // `withdraw`. Without this, a stream cancelled at the halfway mark
        // reports `withdrawn == 0` even though the recipient was just paid
        // half the deposit.
        let total_withdrawn = info
            .withdrawn
            .checked_add(owed_to_recipient)
            .ok_or(Error::ArithmeticOverflow)?;

        let mut cancelled_info = info.clone();
        cancelled_info.flags |= FLAG_CANCELLED;
        cancelled_info.withdrawn = total_withdrawn;
        state::save(env, &cancelled_info);

        // Pay the recipient their earned-but-unwithdrawn portion.
        if owed_to_recipient > 0 {
            tk.transfer(&contract_addr, &info.recipient, &owed_to_recipient);
        }

        // Refund the unstreamed remainder to the sender.
        if refund_to_sender > 0 {
            tk.transfer(&contract_addr, &info.sender, &refund_to_sender);
        }

        events::cancelled(env, &info.sender, refund_to_sender, total_withdrawn);
        Ok(())
    }

    /// Sender (or operator) pauses the stream.
    pub fn pause(env: Env, caller: Address) -> Result<(), Error> {
        state::with_guard(&env, |env| Self::_pause(env, &caller))
    }

    fn _pause(env: &Env, caller: &Address) -> Result<(), Error> {
        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        if info.is_paused() {
            return Err(Error::AlreadyPaused);
        }
        require_sender_or_operator(env, caller, &info.sender)?;

        let now = env.ledger().timestamp();
        if now < info.start_time {
            return Err(Error::StreamNotStarted);
        }
        let w = math::withdrawable(env, &info)?;

        // Single consolidated save — no separate `state::set_paused()` call
        // (which would save the same struct a second time) and no direct
        // `DataKey::PausedAt` write (`state::save` already covers paused_at
        // via the consolidated `Config` key).
        let mut updated = info.clone();
        updated.flags |= FLAG_PAUSED;
        updated.paused_at = now;
        state::save(env, &updated);

        events::paused(env, caller, now, w);
        Ok(())
    }

    /// Sender (or operator) resumes a paused stream.
    pub fn resume(env: Env, caller: Address) -> Result<(), Error> {
        state::with_guard(&env, |env| Self::_resume(env, &caller))
    }

    fn _resume(env: &Env, caller: &Address) -> Result<(), Error> {
        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        if !info.is_paused() {
            return Err(Error::NotPaused);
        }
        require_sender_or_operator(env, caller, &info.sender)?;

        let now = env.ledger().timestamp();
        let paused_duration = now
            .checked_sub(info.paused_at)
            .ok_or(Error::ArithmeticOverflow)?;

        // Shift start_time forward by paused duration so paused time doesn't
        // count; end_time is shifted by the same amount on resume so the
        // contracted duration is preserved in wall-clock terms.
        let new_start: u64 = info
            .start_time
            .checked_add(paused_duration)
            .ok_or(Error::ArithmeticOverflow)?;

        // Single consolidated save — no separate `state::set_paused()` or
        // direct `DataKey::StartTime` / `DataKey::PausedAt` writes.
        let mut updated = info.clone();
        updated.start_time = new_start;
        updated.flags &= !FLAG_PAUSED;
        updated.paused_at = 0;
        if info.end_time > 0 {
            updated.end_time = info
                .end_time
                .checked_add(paused_duration)
                .ok_or(Error::ArithmeticOverflow)?;
        }
        state::save(env, &updated);

        events::resumed(env, caller, now);
        Ok(())
    }

    /// Sender (or operator) deposits additional tokens into the stream.
    ///
    /// Auth is checked immediately after the minimal state load needed to
    /// know `sender` -- before `ttl::bump` (a storage write) or the
    /// cancellation check -- so an unauthenticated call fails as cheaply
    /// as possible instead of paying for storage-extension instructions
    /// it never needed.
    pub fn top_up(env: Env, caller: Address, amount: i128) -> Result<(), Error> {
        state::with_guard(&env, |env| Self::_top_up(env, &caller, amount))
    }

    fn _top_up(env: &Env, caller: &Address, amount: i128) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let info = state::load(env);
        require_sender_or_operator(env, caller, &info.sender)?;

        ttl::bump(env);
        state::assert_not_cancelled(&info)?;

        // Reject top-ups on a bounded stream that has already ended. Depositing
        // funds into a finished stream just locks them: `streamed_amount`
        // (and thus `withdrawable`) is clamped to `end_time`, so the deposit
        // accrues nothing and can only be recovered via `cancel`.
        // Open-ended streams (`end_time == 0`) never "end" and are unaffected.
        //
        // NOTE: `_extend_duration` and `_top_up_and_extend` intentionally do
        // NOT get this guard — extending *is* their purpose: pushing `end_time`
        // forward re-opens an ended bounded stream, and `top_up_and_extend`
        // pairs the deposit with that very extension so the funds stay
        // streamable. Applying the check there would contradict the function's
        // job.
        let now = env.ledger().timestamp();
        if info.end_time > 0 && now >= info.end_time {
            return Err(Error::StreamEnded);
        }

        let tk = token::Client::new(env, &info.token);
        let contract_addr = env.current_contract_address();

        tk.transfer(&info.sender, &contract_addr, &amount);

        let new_balance = tk.balance(&contract_addr);
        events::topped_up(env, caller, amount, new_balance);
        Ok(())
    }

    /// Sender (or operator) extends the stream duration by `extra_time_seconds`.
    ///
    /// Transfers the exact required deposit (rate_per_second × extra_time_seconds)
    /// from the sender into the contract and updates `end_time`.
    ///
    /// # Governance Duration Bounds Design Note
    ///
    /// `GovernorConfig.max_duration_seconds` is enforced by `DripFactory::create_stream`
    /// at creation time to bound initial upfront deposits and scheduling horizons.
    /// Post-creation extensions (`extend_duration` and `top_up_and_extend`) are
    /// intentionally unbounded by the governor's initial duration cap, allowing
    /// active streams (e.g. payroll, continuous subscriptions) to be extended
    /// indefinitely without requiring redeployment, while keeping `DripStream`
    /// instances fully independent per ADR-001 without cross-contract calls.
    pub fn extend_duration(
        env: Env,
        caller: Address,
        extra_time_seconds: u64,
    ) -> Result<(), Error> {
        state::with_guard(&env, |env| {
            Self::_extend_duration(env, &caller, extra_time_seconds)
        })
    }

    fn _extend_duration(env: &Env, caller: &Address, extra_time_seconds: u64) -> Result<(), Error> {
        if extra_time_seconds == 0 {
            return Err(Error::InvalidTimeRange);
        }
        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        require_sender_or_operator(env, caller, &info.sender)?;

        let mut end_time: u64 = info.end_time;
        if end_time == 0 {
            return Err(Error::InvalidTimeRange);
        }

        let rate_per_sec: i128 = info.rate_per_second;

        let required_deposit = (extra_time_seconds as i128)
            .checked_mul(rate_per_sec)
            .ok_or(Error::ArithmeticOverflow)?;

        let tk = token::Client::new(env, &info.token);
        let contract_addr = env.current_contract_address();

        // Transfer required deposit from sender into the contract
        tk.transfer(&info.sender, &contract_addr, &required_deposit);

        // Update end_time with overflow check
        end_time = end_time
            .checked_add(extra_time_seconds)
            .ok_or(Error::ArithmeticOverflow)?;

        // Single consolidated save — no direct `DataKey::EndTime` write
        // (already covered by the consolidated save).
        let mut updated = info.clone();
        updated.end_time = end_time;
        state::save(env, &updated);

        // Emit topped_up event to indicate funds were deposited
        let new_balance = tk.balance(&contract_addr);
        events::topped_up(env, caller, required_deposit, new_balance);

        Ok(())
    }

    /// Sender (or operator) tops up and extends the stream in a single call.
    ///
    /// Combines [`top_up`](Self::top_up) and [`extend_duration`](Self::extend_duration)
    /// into one authorized transaction, reducing round-trips and the risk of a
    /// sender performing only one half of the pair (which would leave the
    /// stream either underfunded for the extended duration or with idle funds
    /// past the original `end_time`).
    ///
    /// `amount` is deposited into the stream and `extra_time_seconds` is added
    /// to `end_time`. Both must be non-zero. Open-ended streams (`end_time == 0`)
    /// cannot be extended — use `top_up` alone instead.
    ///
    /// # Governance Duration Bounds Design Note
    ///
    /// As with [`extend_duration`](Self::extend_duration), post-creation extension
    /// is intentionally unbounded by `GovernorConfig.max_duration_seconds` to allow
    /// ongoing stream continuation without redeployment.
    pub fn top_up_and_extend(
        env: Env,
        caller: Address,
        amount: i128,
        extra_time_seconds: u64,
    ) -> Result<(), Error> {
        state::with_guard(&env, |env| {
            Self::_top_up_and_extend(env, &caller, amount, extra_time_seconds)
        })
    }

    fn _top_up_and_extend(
        env: &Env,
        caller: &Address,
        amount: i128,
        extra_time_seconds: u64,
    ) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        if extra_time_seconds == 0 {
            return Err(Error::InvalidTimeRange);
        }

        let info = state::load(env);
        require_sender_or_operator(env, caller, &info.sender)?;

        ttl::bump(env);
        state::assert_not_cancelled(&info)?;

        if info.end_time == 0 {
            return Err(Error::InvalidTimeRange);
        }

        let tk = token::Client::new(env, &info.token);
        let contract_addr = env.current_contract_address();

        // Transfer funds from sender into the contract
        tk.transfer(&info.sender, &contract_addr, &amount);

        // Update end_time with overflow check
        let new_end_time = info
            .end_time
            .checked_add(extra_time_seconds)
            .ok_or(Error::ArithmeticOverflow)?;

        let mut updated = info.clone();
        updated.end_time = new_end_time;
        state::save(env, &updated);

        let new_balance = tk.balance(&contract_addr);
        events::topped_up(env, caller, amount, new_balance);

        Ok(())
    }

    /// Sender reclaims unstreamed tokens (only if clawback was enabled).
    pub fn clawback(env: Env, caller: Address) -> Result<i128, Error> {
        state::with_guard(&env, |env| Self::_clawback(env, &caller))
    }

    fn _clawback(env: &Env, caller: &Address) -> Result<i128, Error> {
        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        if !info.is_clawback_enabled() {
            return Err(Error::ClawbackDisabled);
        }
        require_sender_or_operator(env, caller, &info.sender)?;

        let streamed = math::streamed_amount(env, &info)?;
        let owed = (streamed - info.withdrawn).max(0);
        let contract_addr = env.current_contract_address();

        let tk = token::Client::new(env, &info.token);
        let balance = tk.balance(&contract_addr);
        let amount = (balance - owed).max(0);

        if amount > 0 {
            tk.transfer(&contract_addr, &info.sender, &amount);
        }

        events::clawback(env, caller, amount);
        Ok(amount)
    }

    /// Read-only: current withdrawable balance for the recipient.
    pub fn withdrawable(env: Env) -> i128 {
        let info = state::load(&env);
        if info.is_cancelled() {
            return 0;
        }
        math::withdrawable(&env, &info).unwrap_or(0)
    }

    /// Read-only: whether clawback is enabled for this stream.
    ///
    /// Returns the `clawback_enabled` flag that was set at initialization time.
    /// If `false`, calling `clawback()` will be rejected with `ClawbackDisabled`.
    ///
    /// Use this before attempting `clawback()` to avoid unnecessarily executing
    /// a call that will be rejected.
    pub fn clawback_enabled(env: Env) -> bool {
        let info = state::load(&env);
        info.is_clawback_enabled()
    }

    /// Recipient force-cancels a stream that has been paused beyond a threshold.
    ///
    /// Prevents the sender from indefinitely pausing the stream to hold
    /// unstreamed tokens hostage. The threshold is hardcoded to 30 days
    /// (2_592_000 seconds) — a governance-configurable version is planned.
    /// Settles atomically like `cancel()`: earned tokens go to recipient,
    /// unstreamed refund goes to sender.
    pub fn force_cancel(env: Env) -> Result<(), Error> {
        state::with_guard(&env, Self::_force_cancel)
    }

    fn _force_cancel(env: &Env) -> Result<(), Error> {
        const PAUSE_THRESHOLD_SECS: u64 = 2_592_000; // 30 days

        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        if !info.is_paused() {
            return Err(Error::NotPaused);
        }

        let now = env.ledger().timestamp();
        let paused_secs = now.saturating_sub(info.paused_at);
        if paused_secs < PAUSE_THRESHOLD_SECS {
            return Err(Error::PauseThresholdNotMet);
        }

        info.recipient.require_auth();

        let tk = token::Client::new(env, &info.token);
        let contract_addr = env.current_contract_address();
        let balance = tk.balance(&contract_addr);

        let streamed = math::streamed_amount(env, &info)?;
        let owed_to_recipient = (streamed - info.withdrawn).max(0).min(balance);
        let refund_to_sender = (balance - owed_to_recipient).max(0);

        // Advance `withdrawn` by the amount paid out here so post-cancel reads
        // reflect what the recipient actually received (see `_cancel`).
        let total_withdrawn = info
            .withdrawn
            .checked_add(owed_to_recipient)
            .ok_or(Error::ArithmeticOverflow)?;

        let mut cancelled_info = info.clone();
        cancelled_info.flags |= FLAG_CANCELLED;
        cancelled_info.withdrawn = total_withdrawn;
        state::save(env, &cancelled_info);

        if owed_to_recipient > 0 {
            tk.transfer(&contract_addr, &info.recipient, &owed_to_recipient);
        }
        if refund_to_sender > 0 {
            tk.transfer(&contract_addr, &info.sender, &refund_to_sender);
        }

        events::force_cancelled(env, &info.sender, refund_to_sender, total_withdrawn);
        Ok(())
    }

    /// Recipient transfers their right to a new address.
    ///
    /// Any withdrawable balance at the moment of transfer stays accessible
    /// to the new recipient. The sender is intentionally not notified
    /// on-chain (use events); governance can add a sender-veto in future.
    pub fn transfer_recipient(env: Env, new_recipient: Address) -> Result<(), Error> {
        state::with_guard(&env, |env| Self::_transfer_recipient(env, new_recipient))
    }

    fn _transfer_recipient(env: &Env, new_recipient: Address) -> Result<(), Error> {
        ttl::bump(env);

        let info = state::load(env);
        state::assert_not_cancelled(&info)?;
        info.recipient.require_auth();

        let mut updated = info.clone();
        updated.recipient = new_recipient.clone();
        state::save(env, &updated);
        events::recipient_transferred(env, &info.recipient, &new_recipient);
        Ok(())
    }

    /// Sender designates an operator who can perform sender-level actions
    /// (pause, cancel, clawback, top_up, extend_duration) on this stream.
    ///
    /// Only the sender may call this. The operator has no power over
    /// withdrawals (which are recipient-only) or recipient transfers.
    pub fn set_operator(env: Env, caller: Address, operator: Address) -> Result<(), Error> {
        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        if caller != info.sender {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        ttl::bump(&env);

        if let Some(existing) = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Operator)
        {
            if existing != operator {
                return Err(Error::OperatorAlreadySet);
            }
            return Ok(());
        }

        env.storage().instance().set(&DataKey::Operator, &operator);
        events::operator_set(&env, &caller, &operator);
        Ok(())
    }

    /// Sender revokes the operator, removing all delegated sender rights.
    pub fn revoke_operator(env: Env, caller: Address) -> Result<(), Error> {
        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        if caller != info.sender {
            return Err(Error::NotAuthorized);
        }
        caller.require_auth();
        ttl::bump(&env);

        env.storage().instance().remove(&DataKey::Operator);
        events::operator_revoked(&env, &caller);
        Ok(())
    }

    /// Read-only: the current operator address, if any.
    pub fn operator(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Operator)
    }

    /// Read-only: total tokens streamed so far (regardless of withdrawals).
    ///
    /// Useful for UIs that want to show "X streamed, Y withdrawn, Z remaining"
    /// without the caller needing to reimplement the rate × elapsed math.
    pub fn streamed_total(env: Env) -> i128 {
        let info = state::load(&env);
        if info.is_cancelled() {
            return 0;
        }
        math::streamed_amount(&env, &info).unwrap_or(0)
    }

    /// Read-only: full stream state.
    pub fn info(env: Env) -> StreamInfo {
        state::load(&env)
    }

    /// Latest committed event sequence.
    ///
    /// Event consumers can compare this value with the last sequence they
    /// processed after reconnecting. A gap means the missing ledger range
    /// must be replayed before live processing continues.
    pub fn event_sequence(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::EventSequence)
            .unwrap_or(0)
    }

    /// Storage layout version this instance was initialized with.
    ///
    /// Upgrade tooling should read this before invoking a new WASM hash
    /// via `contract upgrade` and confirm it matches the version the new
    /// code expects — pre-existing streams initialized under an older
    /// layout are not automatically migrated.
    pub fn storage_version(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::StorageVersion)
            .unwrap_or(0)
    }
}
