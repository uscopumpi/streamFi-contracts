//! Integration tests: create → withdraw → cancel lifecycle.
//!
//! These tests operate at the workspace level and import the stream contract
//! directly. Run with: `cargo test --all -- stream_lifecycle`

#[cfg(test)]
mod stream_lifecycle {
    use drip_stream::{DripStream, DripStreamClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        token, Address, Env,
    };

    fn base_env() -> Env {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set(LedgerInfo {
            timestamp: 1_000_000,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });
        env
    }

    fn deploy_funded_stream<'a>(
        env: &'a Env,
        sender: &Address,
        recipient: &Address,
        rate: i128,
        duration: u64,
        clawback: bool,
    ) -> (DripStreamClient<'a>, Address) {
        let token_admin = Address::generate(env);
        let token_addr = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();
        let tok = token::StellarAssetClient::new(env, &token_addr);
        let deposit = rate * duration as i128;

        tok.mint(sender, &deposit);

        let stream_id = env.register_contract(None, DripStream);
        let client = DripStreamClient::new(env, &stream_id);

        token::Client::new(env, &token_addr).transfer(sender, &stream_id, &deposit);

        let now = env.ledger().timestamp();
        client.initialize(
            sender,
            recipient,
            &token_addr,
            &rate,
            &now,
            &(now + duration),
            &clawback,
        );

        (client, token_addr)
    }

    fn advance(env: &Env, secs: u64) {
        env.ledger().set(LedgerInfo {
            timestamp: env.ledger().timestamp() + secs,
            ..env.ledger().get()
        });
    }

    #[test]
    fn full_lifecycle_create_withdraw_cancel() {
        let env = base_env();
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let rate = 1_000_i128; // 1_000 stroops/s
        let duration = 3_600_u64; // 1 hour

        let (client, token_addr) =
            deploy_funded_stream(&env, &sender, &recipient, rate, duration, false);
        let tok = token::Client::new(&env, &token_addr);

        // Nothing earned at t=0
        assert_eq!(client.withdrawable(), 0);

        // Advance 100 seconds
        advance(&env, 100);
        assert_eq!(client.withdrawable(), 100_000); // 100 × 1_000

        // Recipient withdraws half
        client.withdraw(&50_000);
        assert_eq!(tok.balance(&recipient), 50_000);
        assert_eq!(client.withdrawable(), 50_000); // other half still there

        // Advance to halfway (1800s total elapsed from start)
        advance(&env, 1_700);
        // Total streamed = 1_800_000; withdrawn = 50_000; withdrawable = 1_750_000
        assert_eq!(client.withdrawable(), 1_750_000);

        // Sender cancels
        let sender_before = tok.balance(&sender);
        let recipient_before = tok.balance(&recipient);
        client.cancel();

        // Recipient gets remaining owed (1_750_000), sender gets back 1_800_000 (half the deposit)
        assert_eq!(tok.balance(&recipient) - recipient_before, 1_750_000);
        assert_eq!(tok.balance(&sender) - sender_before, 1_800_000);
    }

    #[test]
    fn stream_fully_drains_at_end_time() {
        let env = base_env();
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let (client, token_addr) = deploy_funded_stream(&env, &sender, &recipient, 100, 100, false);
        let tok = token::Client::new(&env, &token_addr);

        // Advance past end
        advance(&env, 200);
        assert_eq!(client.withdrawable(), 10_000); // capped at deposit

        client.withdraw(&10_000);
        assert_eq!(tok.balance(&recipient), 10_000);
        assert_eq!(client.withdrawable(), 0);
    }

    #[test]
    fn top_up_extends_effective_stream() {
        let env = base_env();
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let (client, token_addr) = deploy_funded_stream(&env, &sender, &recipient, 100, 100, false);

        let tok_admin = token::StellarAssetClient::new(&env, &token_addr);
        tok_admin.mint(&sender, &50_000);

        client.top_up(&50_000);

        let tok = token::Client::new(&env, &token_addr);
        // Contract now holds 10_000 (original) + 50_000 (top-up) = 60_000
        assert_eq!(tok.balance(&client.address), 60_000);
    }
}
