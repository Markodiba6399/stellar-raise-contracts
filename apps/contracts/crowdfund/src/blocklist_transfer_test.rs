//! Regression tests for the batch fund-lock fix (audit backlog #7, Critical):
//! `collect_pledges`, `refund` (deprecated), and `cancel` used to call
//! `token_client.transfer` per entry inside one atomic transaction, so a
//! single blocklisted participant (e.g. on a compliance-gated SEP-41 token)
//! would panic and revert the whole batch for everyone else.
//!
//! These tests exercise a minimal mock token whose `transfer` panics for a
//! configured "frozen" address, standing in for a regulated stablecoin's
//! compliance blocklist, and assert that the batch functions now skip the
//! failing entry (via `try_transfer`) instead of reverting, leave that
//! entry's storage retryable, and only fully settle (`Status::Refunded` /
//! `Status::Cancelled`) once nobody was skipped.

extern crate std;

use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Events, Ledger},
    xdr::{ContractEventBody, ScString, ScVal},
    Address, Env, MuxedAddress, String,
};

use crate::{CrowdfundContract, CrowdfundContractClient, DataKey, Status};

// ── Mock blocklist token ──────────────────────────────────────────────────────
//
// Implements just enough of the SEP-41 surface that the crowdfund contract
// calls (`decimals`, `transfer`, plus `name` for `initialize`'s fail-fast
// SEP-41 check), plus `mint`/`balance`/`set_frozen` test helpers. `transfer`
// panics when either party is on the frozen list, simulating a compliance
// blocklist.

#[derive(Clone)]
#[contracttype]
enum BlocklistKey {
    Balance(Address),
    Frozen(Address),
}

#[contract]
struct BlocklistToken;

#[contractimpl]
impl BlocklistToken {
    pub fn mint(env: Env, to: Address, amount: i128) {
        let key = BlocklistKey::Balance(to);
        let bal: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        env.storage().persistent().set(&key, &(bal + amount));
    }

    pub fn set_frozen(env: Env, who: Address, frozen: bool) {
        env.storage()
            .persistent()
            .set(&BlocklistKey::Frozen(who), &frozen);
    }

    pub fn decimals(_env: Env) -> u32 {
        7
    }

    pub fn name(env: Env) -> String {
        String::from_str(&env, "Blocklist Test Token")
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&BlocklistKey::Balance(id))
            .unwrap_or(0)
    }

    pub fn transfer(env: Env, from: Address, to: MuxedAddress, amount: i128) {
        from.require_auth();
        let to_addr = to.address();

        let from_frozen: bool = env
            .storage()
            .persistent()
            .get(&BlocklistKey::Frozen(from.clone()))
            .unwrap_or(false);
        let to_frozen: bool = env
            .storage()
            .persistent()
            .get(&BlocklistKey::Frozen(to_addr.clone()))
            .unwrap_or(false);
        if from_frozen || to_frozen {
            panic!("compliance: address is blocklisted");
        }

        let from_key = BlocklistKey::Balance(from.clone());
        let to_key = BlocklistKey::Balance(to_addr.clone());
        let from_bal: i128 = env.storage().persistent().get(&from_key).unwrap_or(0);
        let to_bal: i128 = env.storage().persistent().get(&to_key).unwrap_or(0);
        env.storage().persistent().set(
            &from_key,
            &(from_bal.checked_sub(amount).expect("insufficient balance")),
        );
        env.storage().persistent().set(&to_key, &(to_bal + amount));
    }
}

// ── Event helpers (same pattern as withdraw_event_emission_test.rs) ──────────

fn topics_match(e: &soroban_sdk::xdr::ContractEvent, ns: &str, action: &str) -> bool {
    let ContractEventBody::V0(body) = &e.body;
    if body.topics.len() < 2 {
        return false;
    }
    let ns_str = ScVal::String(ScString(ns.try_into().unwrap()));
    let act_str = ScVal::String(ScString(action.try_into().unwrap()));
    body.topics[0] == ns_str && body.topics[1] == act_str
}

/// Count events matching a `("crowdfund", action)` topic pair.
///
/// Must be called *before* any `env.as_contract(...)` access (e.g. via
/// `read_i128`/`read_status` below) — `as_contract` resets the testutils
/// event recording, so events checks always happen right after the
/// mutating call, before any storage peeks.
fn count_events(env: &Env, action: &str) -> usize {
    env.events()
        .all()
        .events()
        .iter()
        .filter(|e| topics_match(e, "crowdfund", action))
        .count()
}

// ── Setup helpers ─────────────────────────────────────────────────────────────

fn setup() -> (
    Env,
    CrowdfundContractClient<'static>,
    Address,
    Address,
    BlocklistTokenClient<'static>,
) {
    let env = Env::default();
    // `collect_pledges`/`refund`/`cancel` never call `require_auth()`
    // themselves for the pledger/contributor being paid — the token
    // contract's own `transfer` does that internally, several call-frames
    // below the top-level test invocation. Plain `mock_all_auths()` only
    // auto-approves auth tied to the root invocation; this variant also
    // allows the non-root case, matching how these flows actually work on
    // real Stellar (the payee's authorization is a pre-signed entry
    // attached to the transaction, not necessarily the top-level signer).
    env.mock_all_auths_allowing_non_root_auth();

    let contract_id = env.register(CrowdfundContract, ());
    let client = CrowdfundContractClient::new(&env, &contract_id);

    let creator = Address::generate(&env);
    let token_id = env.register(BlocklistToken, ());
    let token_client = BlocklistTokenClient::new(&env, &token_id);

    (env, client, creator, token_id, token_client)
}

fn read_i128(env: &Env, contract: &Address, key: &DataKey) -> i128 {
    env.as_contract(contract, || env.storage().instance().get(key).unwrap_or(0))
}

fn read_persistent_i128(env: &Env, contract: &Address, key: &DataKey) -> i128 {
    env.as_contract(contract, || {
        env.storage().persistent().get(key).unwrap_or(0)
    })
}

fn read_status(env: &Env, contract: &Address) -> Status {
    env.as_contract(contract, || {
        env.storage().instance().get(&DataKey::Status).unwrap()
    })
}

// ── collect_pledges ────────────────────────────────────────────────────────────

#[test]
fn test_collect_pledges_skips_frozen_pledger() {
    let (env, client, creator, token_id, token) = setup();
    let deadline = env.ledger().timestamp() + 3_600;
    client.initialize(
        &creator, &creator, &token_id, &900, &deadline, &1, &None, &None, &None, &7,
    );

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env);
    for a in [&alice, &bob, &carol] {
        token.mint(a, &1_000);
        client.pledge(a, &300);
    }

    // Bob is blocklisted by the token's compliance layer before collection.
    token.set_frozen(&bob, &true);

    env.ledger().set_timestamp(deadline + 1);
    client.collect_pledges();

    // Check events right away — `as_contract` (used by the read_* helpers
    // below) resets the testutils event log.
    assert_eq!(count_events(&env, "transfer_skipped"), 1);

    // Alice and Carol were collected; Bob was skipped and left retryable.
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalRaised), 600);
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalPledged), 300);
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Pledge(alice.clone())),
        0
    );
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Pledge(carol.clone())),
        0
    );
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Pledge(bob.clone())),
        300
    );
    assert_eq!(token.balance(&client.address), 600);
    assert_eq!(token.balance(&bob), 1_000);
}

#[test]
fn test_collect_pledges_retryable_after_unfreeze() {
    let (env, client, creator, token_id, token) = setup();
    let deadline = env.ledger().timestamp() + 3_600;
    client.initialize(
        &creator, &creator, &token_id, &900, &deadline, &1, &None, &None, &None, &7,
    );

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env);
    for a in [&alice, &bob, &carol] {
        token.mint(a, &1_000);
        client.pledge(a, &300);
    }

    token.set_frozen(&bob, &true);
    env.ledger().set_timestamp(deadline + 1);
    client.collect_pledges();
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalPledged), 300);

    // Bob is unblocked; re-running the batch sweeps up the straggler.
    token.set_frozen(&bob, &false);
    client.collect_pledges();

    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalRaised), 900);
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalPledged), 0);
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Pledge(bob.clone())),
        0
    );
    assert_eq!(token.balance(&client.address), 900);
}

// ── refund (deprecated batch) ─────────────────────────────────────────────────

#[test]
fn test_refund_skips_frozen_contributor_and_stays_active() {
    let (env, client, creator, token_id, token) = setup();
    let deadline = env.ledger().timestamp() + 3_600;
    client.initialize(
        &creator, &creator, &token_id, &1_000, &deadline, &1, &None, &None, &None, &7,
    );

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env);
    for a in [&alice, &bob, &carol] {
        token.mint(a, &100);
        client.contribute(a, &100);
    }

    token.set_frozen(&bob, &true);
    env.ledger().set_timestamp(deadline + 1);
    client.refund();

    // Goal (1000) was never reached, so refund is valid; Bob's refund failed
    // and must not be dropped on the floor or wrongly marked settled.
    assert_eq!(count_events(&env, "transfer_skipped"), 1);
    assert_eq!(read_status(&env, &client.address), Status::Active);
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalRaised), 100);
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Contribution(alice.clone())),
        0
    );
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Contribution(carol.clone())),
        0
    );
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Contribution(bob.clone())),
        100
    );
    assert_eq!(token.balance(&client.address), 100);

    // Unblock Bob and re-run the batch: it now fully settles.
    token.set_frozen(&bob, &false);
    client.refund();

    assert_eq!(read_status(&env, &client.address), Status::Refunded);
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalRaised), 0);
    assert_eq!(token.balance(&client.address), 0);
}

// ── cancel ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cancel_skips_frozen_contributor_and_stays_active() {
    let (env, client, creator, token_id, token) = setup();
    let deadline = env.ledger().timestamp() + 3_600;
    client.initialize(
        &creator, &creator, &token_id, &1_000, &deadline, &1, &None, &None, &None, &7,
    );

    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let carol = Address::generate(&env);
    for a in [&alice, &bob, &carol] {
        token.mint(a, &100);
        client.contribute(a, &100);
    }

    token.set_frozen(&bob, &true);
    client.cancel();

    assert_eq!(count_events(&env, "cancelled"), 0);
    assert_eq!(count_events(&env, "transfer_skipped"), 1);
    assert_eq!(read_status(&env, &client.address), Status::Active);
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalRaised), 100);
    assert_eq!(
        read_persistent_i128(&env, &client.address, &DataKey::Contribution(bob.clone())),
        100
    );

    token.set_frozen(&bob, &false);
    client.cancel();

    assert_eq!(count_events(&env, "cancelled"), 1);
    assert_eq!(read_status(&env, &client.address), Status::Cancelled);
    assert_eq!(read_i128(&env, &client.address, &DataKey::TotalRaised), 0);
}
