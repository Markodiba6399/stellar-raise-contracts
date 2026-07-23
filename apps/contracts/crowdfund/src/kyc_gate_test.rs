#![cfg(test)]

//! Integration tests for the pluggable KYC/AML gate (`kyc_gate` module).
//!
//! Covers both the ungated path (gate never configured — must behave
//! exactly like the base pledge flow) and the gated path (threshold
//! crossed, verified vs. unverified, admin-only configuration, and the
//! on/off toggle).

use soroban_sdk::{
    contract, contractimpl,
    testutils::{Address as _, Ledger},
    token, Address, Env, Symbol,
};

use crate::{ContractError, CrowdfundContract, CrowdfundContractClient};

/// Minimal attestation-contract stand-in implementing the `KycVerifier`
/// interface (`fn is_verified(env, who) -> bool`), with a `set_verified`
/// admin hook so tests can simulate a KYC provider's off-chain decision.
#[contract]
pub struct MockKycVerifier;

#[contractimpl]
impl MockKycVerifier {
    pub fn is_verified(env: Env, who: Address) -> bool {
        env.storage().persistent().get(&who).unwrap_or(false)
    }

    pub fn set_verified(env: Env, who: Address, verified: bool) {
        env.storage().persistent().set(&who, &verified);
    }
}

fn setup() -> (
    Env,
    CrowdfundContractClient<'static>,
    Address, // admin
    Address, // creator
    token::StellarAssetClient<'static>,
    Address, // verifier contract address
) {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(CrowdfundContract, ());
    let client = CrowdfundContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let creator = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_contract_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_address = token_contract_id.address();
    let token_client = token::StellarAssetClient::new(&env, &token_address);

    let deadline = env.ledger().timestamp() + 3600;
    client.initialize(
        &admin,
        &creator,
        &token_address,
        &1_000_000,
        &deadline,
        &1_000,
        &None,
        &None,
        &None,
        &7,
    );

    let verifier_id = env.register(MockKycVerifier, ());

    (env, client, admin, creator, token_client, verifier_id)
}

// ── Ungated path (default) ──────────────────────────────────────────────────

#[test]
fn contribute_succeeds_without_kyc_gate_configured() {
    let (env, client, _admin, _creator, token_client, _verifier) = setup();
    let contributor = Address::generate(&env);
    let amount = 500_000;
    token_client.mint(&contributor, &amount);

    client.contribute(&contributor, &amount);

    assert_eq!(client.total_raised(), amount);
    assert!(client.kyc_gate_config().is_none());
}

#[test]
fn pledge_succeeds_without_kyc_gate_configured() {
    let (env, client, _admin, _creator, _token_client, _verifier) = setup();
    let pledger = Address::generate(&env);

    client.pledge(&pledger, &500_000);

    assert!(client.kyc_gate_config().is_none());
}

#[test]
fn kyc_gate_preview_is_always_true_when_ungated() {
    let (env, client, _admin, _creator, _token_client, _verifier) = setup();
    let who = Address::generate(&env);

    assert!(client.kyc_gate_preview(&who, &1_000_000_000));
}

// ── Admin-only configuration ────────────────────────────────────────────────

#[test]
fn configure_kyc_gate_rejects_non_admin() {
    let (env, client, _admin, creator, _token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");

    let result = client.try_configure_kyc_gate(&creator, &verifier, &10_000, &jurisdiction);

    assert_eq!(result.unwrap_err().unwrap(), ContractError::Unauthorized);
    assert!(client.kyc_gate_config().is_none());
}

#[test]
fn set_kyc_gate_enabled_before_configure_fails() {
    let (_env, client, admin, _creator, _token_client, _verifier) = setup();

    let result = client.try_set_kyc_gate_enabled(&admin, &true);

    assert_eq!(
        result.unwrap_err().unwrap(),
        ContractError::KycGateNotConfigured
    );
}

// ── Gated path: threshold + verification ────────────────────────────────────

#[test]
fn contribute_blocked_when_over_threshold_and_unverified() {
    let (env, client, admin, _creator, token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");
    client.configure_kyc_gate(&admin, &verifier, &10_000, &jurisdiction);

    let contributor = Address::generate(&env);
    let amount = 50_000;
    token_client.mint(&contributor, &amount);

    let result = client.try_contribute(&contributor, &amount);

    assert_eq!(result.unwrap_err().unwrap(), ContractError::KycRequired);
    assert_eq!(client.total_raised(), 0);
}

#[test]
fn contribute_succeeds_when_over_threshold_and_verified() {
    let (env, client, admin, _creator, token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");
    client.configure_kyc_gate(&admin, &verifier, &10_000, &jurisdiction);

    let contributor = Address::generate(&env);
    let amount = 50_000;
    token_client.mint(&contributor, &amount);

    MockKycVerifierClient::new(&env, &verifier)
        .set_verified(&contributor, &true);

    client.contribute(&contributor, &amount);

    assert_eq!(client.total_raised(), amount);
}

#[test]
fn contribute_below_threshold_does_not_require_verification() {
    let (env, client, admin, _creator, token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");
    client.configure_kyc_gate(&admin, &verifier, &10_000, &jurisdiction);

    let contributor = Address::generate(&env);
    let amount = 5_000;
    token_client.mint(&contributor, &amount);

    client.contribute(&contributor, &amount);

    assert_eq!(client.total_raised(), amount);
}

#[test]
fn cumulative_across_contribute_and_pledge_triggers_gate() {
    let (env, client, admin, _creator, token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");
    client.configure_kyc_gate(&admin, &verifier, &10_000, &jurisdiction);

    let backer = Address::generate(&env);
    token_client.mint(&backer, &6_000);

    // First 6,000 stays under the 10,000 threshold.
    client.contribute(&backer, &6_000);

    // A further 6,000 pledge would push the cumulative committed total to
    // 12,000 — over the threshold — even though neither call alone crosses
    // it via the `pledge` flow specifically.
    let result = client.try_pledge(&backer, &6_000);
    assert_eq!(result.unwrap_err().unwrap(), ContractError::KycRequired);

    MockKycVerifierClient::new(&env, &verifier)
        .set_verified(&backer, &true);

    client.pledge(&backer, &6_000);
}

// ── Toggle ───────────────────────────────────────────────────────────────────

#[test]
fn disabling_gate_removes_requirement() {
    let (env, client, admin, _creator, token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");
    client.configure_kyc_gate(&admin, &verifier, &100, &jurisdiction);
    client.set_kyc_gate_enabled(&admin, &false);

    let contributor = Address::generate(&env);
    let amount = 500_000;
    token_client.mint(&contributor, &amount);

    client.contribute(&contributor, &amount);

    assert_eq!(client.total_raised(), amount);
    assert_eq!(client.kyc_gate_config().unwrap().enabled, false);
}

#[test]
fn re_enabling_gate_preserves_previously_configured_threshold() {
    let (env, client, admin, _creator, token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");
    client.configure_kyc_gate(&admin, &verifier, &10_000, &jurisdiction);
    client.set_kyc_gate_enabled(&admin, &false);
    client.set_kyc_gate_enabled(&admin, &true);

    let contributor = Address::generate(&env);
    let amount = 50_000;
    token_client.mint(&contributor, &amount);

    let result = client.try_contribute(&contributor, &amount);

    assert_eq!(result.unwrap_err().unwrap(), ContractError::KycRequired);
    assert_eq!(client.kyc_gate_config().unwrap().threshold, 10_000);
}

// ── Preflight preview ────────────────────────────────────────────────────────

#[test]
fn kyc_gate_preview_reflects_verification_state() {
    let (env, client, admin, _creator, _token_client, verifier) = setup();
    let jurisdiction = Symbol::new(&env, "US");
    client.configure_kyc_gate(&admin, &verifier, &10_000, &jurisdiction);

    let who = Address::generate(&env);

    // Under threshold: allowed regardless of verification.
    assert!(client.kyc_gate_preview(&who, &5_000));

    // Over threshold, unverified: blocked.
    assert!(!client.kyc_gate_preview(&who, &10_000));

    // Over threshold, verified: allowed.
    MockKycVerifierClient::new(&env, &verifier).set_verified(&who, &true);
    assert!(client.kyc_gate_preview(&who, &10_000));
}
