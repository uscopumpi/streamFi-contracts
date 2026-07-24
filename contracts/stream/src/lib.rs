#![no_std]

mod errors;
mod events;
mod math;
mod state;
pub mod storage;
#[cfg(test)]
mod tests;
mod ttl;
mod yield_integration;

use soroban_sdk::{contract, contractimpl, panic_with_error, token, Address, Env};

pub use errors::Error;
use storage::{DataKey, StreamInfo, FLAG_CANCELLED, FLAG_CLAWBACK_ENABLED, FLAG_PAUSED};

#[contract]
pub struct DripStream;

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

        ttl::bump(&env);

        let mut flags: u32 = 0;
        if clawback_enabled {
            flags |= FLAG_CLAWBACK_ENABLED;
        }

        let s = env.storage().instance();
        s.set(&DataKey::Sender, &sender);
        s.set(&DataKey::Recipient, &recipient);
        s.set(&DataKey::Token, &token);
        s.set(&DataKey::RatePerSecond, &rate_per_second);
        s.set(&DataKey::StartTime, &start_time);
        s.set(&DataKey::EndTime, &end_time);
        s.set(&DataKey::Withdrawn, &0_i128);
        s.set(&DataKey::PausedAt, &0_u64);
        s.set(&DataKey::Flags, &flags);
        s.set(&DataKey::EventSequence, &0_u64);
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
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        info.recipient.require_auth();

        let available = math::withdrawable(&env, &info)?;
        if available == 0 {
            return Err(Error::NothingToWithdraw);
        }
        let to_send = amount.min(available);

        let new_withdrawn = info
            .withdrawn
            .checked_add(to_send)
            .ok_or(Error::ArithmeticOverflow)?;
        state::save_withdrawn(&env, new_withdrawn);

        let tk = token::Client::new(&env, &info.token);
        let contract_addr = env.current_contract_address();
        let remaining = tk.balance(&contract_addr) - to_send;

        tk.transfer(&contract_addr, &info.recipient, &to_send);

        events::withdrawn(&env, &info.recipient, to_send, new_withdrawn, remaining);
        Ok(to_send)
    }

    /// Sender cancels the stream.
    ///
    /// Settles everything atomically:
    ///   - Tokens the recipient has earned (but not yet withdrawn) are sent
    ///     directly to the recipient.
    ///   - The remaining unstreamed balance is refunded to the sender.
    ///
    /// After cancellation, `withdraw()` is blocked (`StreamCancelled`), so
    /// the recipient's share MUST be transferred here rather than left for
    /// a later `withdraw()` call.
    pub fn cancel(env: Env) -> Result<(), Error> {
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        info.sender.require_auth();

        let tk = token::Client::new(&env, &info.token);
        let contract_addr = env.current_contract_address();
        let balance = tk.balance(&contract_addr);

        // How many tokens the recipient has earned but not yet withdrawn.
        let streamed = math::streamed_amount(&env, &info)?;
        let owed_to_recipient = (streamed - info.withdrawn).max(0).min(balance);
        let refund_to_sender = (balance - owed_to_recipient).max(0);

        // Mark cancelled before any transfers to prevent re-entrancy
        // (Soroban's execution model already prevents re-entrancy, but this
        // is still the correct ordering for state-machine correctness).
        state::set_cancelled(&env);
        let mut cancelled_info = info.clone();
        cancelled_info.flags |= FLAG_CANCELLED;
        state::save(&env, &cancelled_info);

        // Pay the recipient their earned-but-unwithdrawn portion.
        if owed_to_recipient > 0 {
            tk.transfer(&contract_addr, &info.recipient, &owed_to_recipient);
        }

        // Refund the unstreamed remainder to the sender.
        if refund_to_sender > 0 {
            tk.transfer(&contract_addr, &info.sender, &refund_to_sender);
        }

        events::cancelled(&env, &info.sender, refund_to_sender, info.withdrawn);
        Ok(())
    }

    /// Sender pauses the stream.
    pub fn pause(env: Env) -> Result<(), Error> {
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        if info.is_paused() {
            return Err(Error::AlreadyPaused);
        }
        info.sender.require_auth();

        let now = env.ledger().timestamp();
        let w = math::withdrawable(&env, &info)?;

        // Single consolidated save — no separate `state::set_paused()` call
        // (which would save the same struct a second time) and no direct
        // `DataKey::PausedAt` write (`state::save` already covers paused_at
        // via the consolidated `Config` key).
        let mut updated = info.clone();
        updated.flags |= FLAG_PAUSED;
        updated.paused_at = now;
        state::save(&env, &updated);

        events::paused(&env, &info.sender, now, w);
        Ok(())
    }

    /// Sender resumes a paused stream.
    pub fn resume(env: Env) -> Result<(), Error> {
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        if !info.is_paused() {
            return Err(Error::NotPaused);
        }
        info.sender.require_auth();

        let now = env.ledger().timestamp();
        let paused_duration = now - info.paused_at;

        // Shift start_time forward by paused duration so paused time doesn't
        // count; end_time is shifted by the same amount on resume so the
        // contracted duration is preserved in wall-clock terms.
        let new_start: u64 = info.start_time + paused_duration;

        // Single consolidated save — no separate `state::set_paused()` or
        // direct `DataKey::StartTime` / `DataKey::PausedAt` writes.
        let mut updated = info.clone();
        updated.start_time = new_start;
        updated.flags &= !FLAG_PAUSED;
        updated.paused_at = 0;
        if info.end_time > 0 {
            updated.end_time = info.end_time + paused_duration;
        }
        state::save(&env, &updated);

        events::resumed(&env, &info.sender, now);
        Ok(())
    }

    /// Sender deposits additional tokens into the stream.
    ///
    /// Auth is checked immediately after the minimal state load needed to
    /// know `sender` -- before `ttl::bump` (a storage write) or the
    /// cancellation check -- so an unauthenticated call fails as cheaply
    /// as possible instead of paying for storage-extension instructions
    /// it never needed.
    pub fn top_up(env: Env, amount: i128) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let info = state::load(&env);
        info.sender.require_auth();

        ttl::bump(&env);
        state::assert_not_cancelled(&info)?;

        let tk = token::Client::new(&env, &info.token);
        let contract_addr = env.current_contract_address();

        tk.transfer(&info.sender, &contract_addr, &amount);

        let new_balance = tk.balance(&contract_addr);
        events::topped_up(&env, &info.sender, amount, new_balance);
        Ok(())
    }

    /// Sender extends the stream duration by `extra_time_seconds`.
    ///
    /// Transfers the exact required deposit (rate_per_second × extra_time_seconds)
    /// from the sender into the contract and updates `end_time`.
    pub fn extend_duration(env: Env, extra_time_seconds: u64) -> Result<(), Error> {
        if extra_time_seconds == 0 {
            return Err(Error::InvalidTimeRange);
        }
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        info.sender.require_auth();

        let mut end_time: u64 = info.end_time;
        if end_time == 0 {
            return Err(Error::InvalidTimeRange);
        }

        let rate_per_sec: i128 = info.rate_per_second;

        let required_deposit = (extra_time_seconds as i128)
            .checked_mul(rate_per_sec)
            .ok_or(Error::ArithmeticOverflow)?;

        let tk = token::Client::new(&env, &info.token);
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
        state::save(&env, &updated);

        // Emit topped_up event to indicate funds were deposited
        let new_balance = tk.balance(&contract_addr);
        events::topped_up(&env, &info.sender, required_deposit, new_balance);

        Ok(())
    }

    /// Sender reclaims unstreamed tokens (only if clawback was enabled).
    pub fn clawback(env: Env) -> Result<i128, Error> {
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        if !info.is_clawback_enabled() {
            return Err(Error::ClawbackDisabled);
        }
        info.sender.require_auth();

        let streamed = math::streamed_amount(&env, &info)?;
        let owed = (streamed - info.withdrawn).max(0);
        let contract_addr = env.current_contract_address();

        let tk = token::Client::new(&env, &info.token);
        let balance = tk.balance(&contract_addr);
        let amount = (balance - owed).max(0);

        if amount > 0 {
            tk.transfer(&contract_addr, &info.sender, &amount);
        }

        events::clawback(&env, &info.sender, amount);
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

    /// Recipient force-cancels a stream that has been paused beyond a threshold.
    ///
    /// Prevents the sender from indefinitely pausing the stream to hold
    /// unstreamed tokens hostage. The threshold is hardcoded to 30 days
    /// (2_592_000 seconds) — a governance-configurable version is planned.
    /// Settles atomically like `cancel()`: earned tokens go to recipient,
    /// unstreamed refund goes to sender.
    pub fn force_cancel(env: Env) -> Result<(), Error> {
        const PAUSE_THRESHOLD_SECS: u64 = 2_592_000; // 30 days

        ttl::bump(&env);

        let info = state::load(&env);
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

        let tk = token::Client::new(&env, &info.token);
        let contract_addr = env.current_contract_address();
        let balance = tk.balance(&contract_addr);

        let streamed = math::streamed_amount(&env, &info)?;
        let owed_to_recipient = (streamed - info.withdrawn).max(0).min(balance);
        let refund_to_sender = (balance - owed_to_recipient).max(0);

        state::set_cancelled(&env);
        let mut cancelled_info = info.clone();
        cancelled_info.flags |= FLAG_CANCELLED;
        state::save(&env, &cancelled_info);

        if owed_to_recipient > 0 {
            tk.transfer(&contract_addr, &info.recipient, &owed_to_recipient);
        }
        if refund_to_sender > 0 {
            tk.transfer(&contract_addr, &info.sender, &refund_to_sender);
        }

        events::cancelled(&env, &info.sender, refund_to_sender, info.withdrawn);
        Ok(())
    }

    /// Recipient transfers their right to a new address.
    ///
    /// Any withdrawable balance at the moment of transfer stays accessible
    /// to the new recipient. The sender is intentionally not notified
    /// on-chain (use events); governance can add a sender-veto in future.
    pub fn transfer_recipient(env: Env, new_recipient: Address) -> Result<(), Error> {
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        info.recipient.require_auth();

        let mut updated = info.clone();
        updated.recipient = new_recipient.clone();
        state::save(&env, &updated);
        events::recipient_transferred(&env, &info.recipient, &new_recipient);
        Ok(())
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
}
