#![allow(missing_docs)]

use soroban_sdk::{Address, Env, Symbol, Vec};

use crate::{ContractError, DataKey, KycGateConfig, NftContractClient, MAX_NFT_MINT_BATCH};

// ── contributed ───────────────────────────────────────────────────────────────

pub fn emit_contributed(env: &Env, backer: &Address, amount: i128, total_raised: i128) {
    env.events().publish(
        ("crowdfund", "contributed"),
        (backer.clone(), amount, total_raised),
    );
}

// ── goal_reached ──────────────────────────────────────────────────────────────

pub fn emit_goal_reached(env: &Env, total_raised: i128, goal: i128) {
    env.events()
        .publish(("crowdfund", "goal_reached"), (total_raised, goal));
}

// ── withdrawn ─────────────────────────────────────────────────────────────────

pub fn emit_withdrawn(env: &Env, creator: &Address, amount: i128, platform_fee: i128) {
    if amount <= 0 {
        env.panic_with_error(ContractError::InvalidParameter);
    }
    env.events().publish(
        ("crowdfund", "withdrawn"),
        (creator.clone(), amount, platform_fee),
    );
}

// ── refunded ─────────────────────────────────────────────────────────────────

pub fn emit_refunded(env: &Env, backer: &Address, amount: i128) {
    env.events()
        .publish(("crowdfund", "refunded"), (backer.clone(), amount));
}

// ── cancelled ────────────────────────────────────────────────────────────────

pub fn emit_cancelled(env: &Env) {
    env.events().publish(("crowdfund", "cancelled"), ());
}

// ── transfer_skipped ─────────────────────────────────────────────────────────

/// Emitted when a per-entry transfer inside a batch operation
/// (`collect_pledges`, `refund`, `cancel`) fails — e.g. the counterparty is a
/// blocklisted address on a compliance-gated SEP-41 token — and is skipped
/// rather than reverting the whole batch. The entry's storage is left intact
/// (not zeroed) so it can be retried later, either by re-running the batch
/// operation or via the pull-based `refund_single` path.
pub fn emit_transfer_skipped(env: &Env, participant: &Address, amount: i128, context: Symbol) {
    env.events().publish(
        ("crowdfund", "transfer_skipped"),
        (participant.clone(), amount, context),
    );
}

// ── stretch_goal_reached ─────────────────────────────────────────────────────

/// Emitted when a contribution pushes `total_raised` past a stretch-goal milestone.
/// This is a purely informational (UI-driven) event; reaching a stretch goal does
/// **not** automatically alter fee schedules, minting, or fund-release logic.
/// Downstream consumers (e.g. front-ends, indexers) should treat this event as a
/// progress indicator only. If future protocol upgrades require on-chain enforcement
/// (e.g. extra token minting, tiered fees), a new event or storage flag should be
/// added alongside this one — the existing event must keep its informational semantics
/// for backward compatibility.
pub fn emit_stretch_goal_reached(env: &Env, milestone: i128, total_raised: i128) {
    env.events()
        .publish(("crowdfund", "stretch_goal_reached"), (milestone, total_raised));
}

// ── fee_transferred ──────────────────────────────────────────────────────────

pub fn emit_fee_transferred(env: &Env, platform: &Address, fee: i128) {
    if fee <= 0 {
        env.panic_with_error(ContractError::InvalidParameter);
    }
    env.events()
        .publish(("crowdfund", "fee_transferred"), (platform.clone(), fee));
}

// ── nft_batch_minted ─────────────────────────────────────────────────────────

pub fn emit_nft_batch_minted(env: &Env, minted_count: u32) {
    if minted_count == 0 {
        env.panic_with_error(ContractError::InvalidParameter);
    }
    env.events()
        .publish(("crowdfund", "nft_batch_minted"), minted_count);
}

// ── milestones_proposed ───────────────────────────────────────────────────────

pub fn emit_milestones_proposed(env: &Env, creator: &Address, count: u32, total_raised: i128) {
    env.events().publish(
        ("crowdfund", "milestones_proposed"),
        (creator.clone(), count, total_raised),
    );
}

// ── milestone_vote_cast ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn emit_milestone_vote_cast(
    env: &Env,
    milestone_id: u32,
    voter: &Address,
    approve: bool,
    weight: i128,
    yes_weight: i128,
    no_weight: i128,
) {
    env.events().publish(
        ("crowdfund", "milestone_vote_cast"),
        (
            milestone_id,
            voter.clone(),
            approve,
            weight,
            yes_weight,
            no_weight,
        ),
    );
}

// ── milestone_resolved ────────────────────────────────────────────────────────

pub fn emit_milestone_resolved(
    env: &Env,
    milestone_id: u32,
    approved: bool,
    yes_weight: i128,
    no_weight: i128,
) {
    env.events().publish(
        ("crowdfund", "milestone_resolved"),
        (milestone_id, approved, yes_weight, no_weight),
    );
}

// ── milestone_released ────────────────────────────────────────────────────────

pub fn emit_milestone_released(
    env: &Env,
    milestone_id: u32,
    creator: &Address,
    payout: i128,
    platform_fee: i128,
) {
    if payout <= 0 {
        env.panic_with_error(ContractError::InvalidParameter);
    }
    env.events().publish(
        ("crowdfund", "milestone_released"),
        (milestone_id, creator.clone(), payout, platform_fee),
    );
}

// ── milestone_refund_claimed ──────────────────────────────────────────────────

pub fn emit_milestone_refund_claimed(
    env: &Env,
    milestone_id: u32,
    contributor: &Address,
    amount: i128,
) {
    if amount <= 0 {
        env.panic_with_error(ContractError::InvalidParameter);
    }
    env.events().publish(
        ("crowdfund", "milestone_refund_claimed"),
        (milestone_id, contributor.clone(), amount),
    );
}

// ── milestone_schedule_completed ──────────────────────────────────────────────

pub fn emit_milestone_schedule_completed(
    env: &Env,
    total_released: i128,
    total_refunded: i128,
) {
    env.events().publish(
        ("crowdfund", "milestone_schedule_completed"),
        (total_released, total_refunded),
    );
}

// ── kyc_gate_configured ───────────────────────────────────────────────────────

pub fn emit_kyc_gate_configured(env: &Env, config: &KycGateConfig) {
    env.events().publish(
        ("crowdfund", "kyc_gate_configured"),
        (
            config.verifier.clone(),
            config.threshold,
            config.jurisdiction.clone(),
        ),
    );
}

// ── kyc_gate_toggled ──────────────────────────────────────────────────────────

pub fn emit_kyc_gate_toggled(env: &Env, enabled: bool) {
    env.events()
        .publish(("crowdfund", "kyc_gate_toggled"), enabled);
}

// ── NFT batch minting ─────────────────────────────────────────────────────────

/// Mint NFTs to eligible contributors, capped at `MAX_NFT_MINT_BATCH`.
/// Returns the number minted (0 if no NFT contract or no eligible contributors).
pub fn mint_nfts_in_batch(env: &Env, nft_contract: &Option<Address>) -> u32 {
    let Some(nft_addr) = nft_contract else {
        return 0;
    };

    let contributors: Vec<Address> = env
        .storage()
        .persistent()
        .get(&DataKey::Contributors)
        .unwrap_or_else(|| Vec::new(env));

    let client = NftContractClient::new(env, nft_addr);
    let mut minted: u32 = 0;

    for contributor in contributors.iter() {
        if minted >= MAX_NFT_MINT_BATCH {
            break;
        }
        let contribution: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Contribution(contributor.clone()))
            .unwrap_or(0);
        if contribution > 0 {
            client.mint(&contributor);
            minted += 1;
        }
    }

    if minted > 0 {
        emit_nft_batch_minted(env, minted);
    }

    minted
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    #[test]
    #[should_panic]
    fn emit_fee_transferred_rejects_zero() {
        let env = Env::default();
        emit_fee_transferred(&env, &Address::generate(&env), 0);
    }

    #[test]
    #[should_panic]
    fn emit_fee_transferred_rejects_negative() {
        let env = Env::default();
        emit_fee_transferred(&env, &Address::generate(&env), -1);
    }

    #[test]
    fn emit_fee_transferred_accepts_positive() {
        let env = Env::default();
        emit_fee_transferred(&env, &Address::generate(&env), 1);
    }

    #[test]
    #[should_panic]
    fn emit_nft_batch_minted_rejects_zero() {
        let env = Env::default();
        emit_nft_batch_minted(&env, 0);
    }

    #[test]
    fn emit_nft_batch_minted_accepts_positive() {
        let env = Env::default();
        emit_nft_batch_minted(&env, 1);
    }

    #[test]
    #[should_panic]
    fn emit_withdrawn_rejects_zero_payout() {
        let env = Env::default();
        emit_withdrawn(&env, &Address::generate(&env), 0, 0);
    }

    #[test]
    #[should_panic]
    fn emit_withdrawn_rejects_negative_payout() {
        let env = Env::default();
        emit_withdrawn(&env, &Address::generate(&env), -100, 0);
    }

    #[test]
    fn emit_withdrawn_accepts_valid_args() {
        let env = Env::default();
        emit_withdrawn(&env, &Address::generate(&env), 1_000, 0);
    }

    #[test]
    fn emit_withdrawn_accepts_nonzero_fee() {
        let env = Env::default();
        emit_withdrawn(&env, &Address::generate(&env), 950, 50);
    }

    #[test]
    #[should_panic]
    fn emit_milestone_released_rejects_zero_payout() {
        let env = Env::default();
        emit_milestone_released(&env, 0, &Address::generate(&env), 0, 0);
    }

    #[test]
    fn emit_milestone_released_accepts_valid_args() {
        let env = Env::default();
        emit_milestone_released(&env, 0, &Address::generate(&env), 1_000, 50);
    }

    #[test]
    #[should_panic]
    fn emit_milestone_refund_claimed_rejects_zero_amount() {
        let env = Env::default();
        emit_milestone_refund_claimed(&env, 0, &Address::generate(&env), 0);
    }

    #[test]
    fn emit_milestone_refund_claimed_accepts_positive_amount() {
        let env = Env::default();
        emit_milestone_refund_claimed(&env, 0, &Address::generate(&env), 500);
    }
}
