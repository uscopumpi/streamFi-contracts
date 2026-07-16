#![no_std]

mod errors;
mod events;
mod math;
mod state;
mod storage;
#[cfg(test)]
mod tests;
mod ttl;

use soroban_sdk::{contract, contractimpl, panic_with_error, token, Address, Env};

pub use errors::Error;
use storage::{DataKey, StreamInfo};

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
        if env.storage().instance().has(&DataKey::Sender) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        ttl::bump(&env);

        let s = env.storage().instance();
        s.set(&DataKey::Sender, &sender);
        s.set(&DataKey::Recipient, &recipient);
        s.set(&DataKey::Token, &token);
        s.set(&DataKey::RatePerSecond, &rate_per_second);
        s.set(&DataKey::StartTime, &start_time);
        s.set(&DataKey::EndTime, &end_time);
        s.set(&DataKey::ClawbackEnabled, &clawback_enabled);
        s.set(&DataKey::Withdrawn, &0_i128);
        s.set(&DataKey::Paused, &false);
        s.set(&DataKey::PausedAt, &0_u64);
        s.set(&DataKey::Cancelled, &false);
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
        env.storage().instance().set(&DataKey::Cancelled, &true);

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
        if info.paused {
            return Err(Error::AlreadyPaused);
        }
        info.sender.require_auth();

        let now = env.ledger().timestamp();
        let w = math::withdrawable(&env, &info)?;

        env.storage().instance().set(&DataKey::Paused, &true);
        env.storage().instance().set(&DataKey::PausedAt, &now);

        events::paused(&env, &info.sender, now, w);
        Ok(())
    }

    /// Sender resumes a paused stream.
    pub fn resume(env: Env) -> Result<(), Error> {
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        if !info.paused {
            return Err(Error::NotPaused);
        }
        info.sender.require_auth();

        let now = env.ledger().timestamp();
        let paused_duration = now - info.paused_at;

        // Shift start_time forward by paused duration so paused time doesn't count
        let new_start: u64 = info.start_time + paused_duration;
        env.storage()
            .instance()
            .set(&DataKey::StartTime, &new_start);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::PausedAt, &0_u64);

        if info.end_time > 0 {
            let new_end = info.end_time + paused_duration;
            env.storage().instance().set(&DataKey::EndTime, &new_end);
        }

        events::resumed(&env, &info.sender, now);
        Ok(())
    }

    /// Sender deposits additional tokens into the stream.
    pub fn top_up(env: Env, amount: i128) -> Result<(), Error> {
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        info.sender.require_auth();

        let tk = token::Client::new(&env, &info.token);
        let contract_addr = env.current_contract_address();

        tk.transfer(&info.sender, &contract_addr, &amount);

        let new_balance = tk.balance(&contract_addr);
        events::topped_up(&env, &info.sender, amount, new_balance);
        Ok(())
    }

    /// Sender reclaims unstreamed tokens (only if clawback was enabled).
    pub fn clawback(env: Env) -> Result<i128, Error> {
        ttl::bump(&env);

        let info = state::load(&env);
        state::assert_not_cancelled(&info)?;
        if !info.clawback_enabled {
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
        if info.cancelled {
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
        if !info.paused {
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

        env.storage().instance().set(&DataKey::Cancelled, &true);

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

        env.storage()
            .instance()
            .set(&DataKey::Recipient, &new_recipient);
        events::recipient_transferred(&env, &info.recipient, &new_recipient);
        Ok(())
    }

    /// Read-only: total tokens streamed so far (regardless of withdrawals).
    ///
    /// Useful for UIs that want to show "X streamed, Y withdrawn, Z remaining"
    /// without the caller needing to reimplement the rate × elapsed math.
    pub fn streamed_total(env: Env) -> i128 {
        let info = state::load(&env);
        if info.cancelled {
            return 0;
        }
        math::streamed_amount(&env, &info).unwrap_or(0)
    }

    /// Read-only: full stream state.
    pub fn info(env: Env) -> StreamInfo {
        state::load(&env)
    }
}
