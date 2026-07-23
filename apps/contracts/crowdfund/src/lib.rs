#![no_std]
#![allow(missing_docs)]
#![allow(clippy::too_many_arguments)]
#![allow(deprecated)]

use soroban_sdk::{
    contract, contractclient, contractimpl, contracttype, token, Address, Env, String, Symbol, Vec,
};

use withdraw_event_emission::{
    emit_cancelled, emit_contributed, emit_fee_transferred, emit_goal_reached, emit_refunded,
    emit_stretch_goal_reached, emit_transfer_skipped, emit_withdrawn, mint_nfts_in_batch,
};

// --- Modules ---
pub mod admin_upgrade_mechanism;
pub mod campaign_goal_minimum;
pub mod contract_state_size;
pub mod contribute_error_handling;
pub mod crowdfund_initialize_function;
pub mod kyc_gate;
pub mod milestone_release;
pub mod proptest_generator_boundary;
pub mod refund_single_token;
pub mod soroban_sdk_minor;
pub mod withdraw_event_emission;

// --- Imports from Modules ---
use milestone_release::{
    execute_claim_milestone_refund, execute_finalize_milestone_vote,
    execute_propose_milestones, execute_release_milestone, execute_vote_milestone,
};
use refund_single_token::{execute_refund_single, validate_refund_preconditions};

// --- Tests ---
#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod blocklist_transfer_test;
#[cfg(test)]
mod kyc_gate_test;
#[cfg(test)]
mod refund_single_token_security_tests;
#[cfg(test)]
mod test;
#[cfg(test)]
mod withdraw_event_emission_test;
// #[cfg(test)]
// mod contract_state_size_test;
// #[cfg(test)]
// mod refund_single_token_test;
// #[cfg(test)]
// mod refund_single_token_tests;
// #[cfg(test)]
// mod campaign_goal_minimum_test;
// #[cfg(test)]
// mod contribute_error_handling_tests;
// #[cfg(test)]
// mod proptest_generator_boundary_tests;

// #[cfg(test)]
// #[path = "admin_upgrade_mechanism.test.rs"]
// mod admin_upgrade_mechanism_test;

// --- Constants ---
const CONTRACT_VERSION: u32 = 3;
#[allow(dead_code)]
const CONTRIBUTION_COOLDOWN: u64 = 60;

pub const MAX_NFT_MINT_BATCH: u32 = 50;

// ── TTL / Rent-extension policy ───────────────────────────────────────────────
//
// Soroban persistent-storage entries are archived when their live-until ledger
// drops to zero.  The constants below define the bump amounts used uniformly
// across every `extend_ttl` call so that the policy can be reviewed, tested, and
// updated in a single location rather than being scattered as magic numbers.
//
// Chosen values (in ledgers, ~5 s/ledger on Stellar mainnet):
//   LEDGER_THRESHOLD     = 100_000 ledgers ≈ ~578 days.
//     The TTL is extended only when remaining TTL falls *below* this threshold.
//   LEDGER_BUMP_AMOUNT   = 535_000 ledgers ≈ ~31 days extended per bump.
//     After bumping, the entry lives for at least this many additional ledgers.
//
// Instance storage (all keys live together under one entry) is bumped on every
// public entry-point via `extend_instance_ttl`.
// Persistent per-contributor/per-pledger keys are bumped on every write.
/// Minimum remaining TTL (in ledgers) before a persistent-storage entry is bumped.
pub const LEDGER_THRESHOLD: u32 = 100_000;
/// Number of ledgers by which to extend a storage entry's TTL on each bump.
pub const LEDGER_BUMP_AMOUNT: u32 = 535_000;

/// Extend the TTL for the contract's **instance** storage bucket.
///
/// Call this at the top of every public entry-point so that a long-dormant
/// campaign never loses its instance-storage keys to archival.
#[inline]
pub fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD, LEDGER_BUMP_AMOUNT);
}

// ── Data Types ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub enum Status {
    Active,
    Successful,
    Refunded,
    Cancelled,
}

#[derive(Clone)]
#[contracttype]
pub struct RoadmapItem {
    pub date: u64,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
#[contracttype]
pub enum MilestoneStatus {
    Pending,
    Approved,
    Rejected,
    Released,
}

#[derive(Clone)]
#[contracttype]
pub struct Milestone {
    pub id: u32,
    pub description: String,
    pub amount: i128,
    pub status: MilestoneStatus,
    pub yes_weight: i128,
    pub no_weight: i128,
    pub voting_deadline: u64,
}

/// Caller-supplied input for one row of a proposed milestone schedule.
#[derive(Clone)]
#[contracttype]
pub struct MilestoneInput {
    pub description: String,
    pub amount: i128,
}

/// Composite storage key for a single voter's vote on a single milestone.
///
/// `#[contracttype]` only supports tuple-variant enum fields with at most
/// one element, so this pair is wrapped in a named struct instead of e.g.
/// `DataKey::MilestoneVote(u32, Address)`.
#[derive(Clone)]
#[contracttype]
pub struct MilestoneVoteKey {
    pub milestone_id: u32,
    pub voter: Address,
}

/// Composite storage key for a single contributor's claimed-refund flag on
/// a single (rejected) milestone. Same one-field-tuple-variant constraint
/// as [`MilestoneVoteKey`].
#[derive(Clone)]
#[contracttype]
pub struct MilestoneRefundKey {
    pub milestone_id: u32,
    pub contributor: Address,
}

#[derive(Clone)]
#[contracttype]
pub struct PlatformConfig {
    pub address: Address,
    pub fee_bps: u32,
}

/// Pluggable KYC/AML gate configuration for this campaign. Absent (`None`,
/// the default — see [`DataKey::KycGate`]) means the gate is fully off and
/// `contribute`/`pledge` behave exactly as if the feature didn't exist.
///
/// See [`crate::kyc_gate`] for the enforcement logic and the rationale for
/// why this is admin-configured rather than creator-configured.
#[derive(Clone)]
#[contracttype]
pub struct KycGateConfig {
    /// Address of an external attestation contract implementing
    /// [`KycVerifier`]. It is populated off-chain by a KYC provider after
    /// they verify an address; this contract never stores personal data.
    pub verifier: Address,
    /// Once an address's cumulative committed amount (contributions +
    /// pledges) on this campaign reaches this value, further
    /// contributions/pledges from that address require `verifier` to report
    /// them as verified. Set per legal/compliance guidance, not derived
    /// on-chain.
    pub threshold: i128,
    /// Quick on/off switch that preserves `verifier`/`threshold`/`jurisdiction`
    /// across toggles, so re-enabling doesn't require resupplying them.
    pub enabled: bool,
    /// Free-form jurisdiction tag (e.g. an ISO 3166-1 alpha-2 code) recording
    /// *why* this gate is configured, for audit/legal traceability. Purely
    /// informational — never interpreted on-chain; enforcement is driven
    /// only by `threshold` and `enabled`.
    pub jurisdiction: Symbol,
}

#[derive(Clone)]
#[contracttype]
pub struct CampaignStats {
    pub total_raised: i128,
    pub goal: i128,
    pub progress_bps: u32,
    pub contributor_count: u32,
    pub average_contribution: i128,
    pub largest_contribution: i128,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Creator,
    Token,
    Goal,
    Deadline,
    TotalRaised,
    Contribution(Address),
    Contributors,
    Status,
    MinContribution,
    Pledge(Address),
    TotalPledged,
    StretchGoals,
    /// Tracks which stretch-goal milestones have already emitted their
    /// `stretch_goal_reached` event (purely informational, no on-chain enforcement).
    StretchGoalReachedEmitted,
    BonusGoal,
    BonusGoalDescription,
    BonusGoalReachedEmitted,
    GoalReachedEmitted,
    Pledgers,
    Roadmap,
    Admin,
    Title,
    Description,
    SocialLinks,
    PlatformConfig,
    NFTContract,
    TokenDecimals,
    Milestones,
    MilestoneBasis,
    MilestoneVote(MilestoneVoteKey),
    MilestoneRefundClaimed(MilestoneRefundKey),
    KycGate,
    PreviousWasmHash,
}

/// Extend the TTL for a single **persistent** storage key.
///
/// Call this after every `persistent().set(key, …)` that touches a
/// per-contributor or per-pledger entry.
#[inline]
pub fn bump_persistent(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, LEDGER_THRESHOLD, LEDGER_BUMP_AMOUNT);
}

// ── Contract Error ──────────────────────────────────────────────────────────

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum ContractError {
    AlreadyInitialized = 1,
    CampaignEnded = 2,
    CampaignStillActive = 3,
    GoalNotReached = 4,
    GoalReached = 5,
    Overflow = 6,
    NothingToRefund = 7,
    /// goal must be > 0
    InvalidGoal = 8,
    /// min_contribution must be > 0
    InvalidMinContribution = 9,
    /// deadline must be at least MIN_DEADLINE_OFFSET seconds in the future
    DeadlineTooSoon = 10,
    /// platform fee_bps must be < 10_000 (100% is rejected, audit #31)
    InvalidPlatformFee = 11,
    /// bonus_goal must be strictly greater than goal
    InvalidBonusGoal = 12,
    /// bonus_goal_description validation failed
    InvalidBonusGoalDescription = 13,
    ZeroAmount = 14,
    BelowMinimum = 15,
    CampaignNotActive = 16,
    Unauthorized = 17,
    InvalidParameter = 18,
    /// `propose_milestones` called after a schedule already exists.
    MilestonesAlreadyProposed = 19,
    /// Proposed schedule is empty, over capacity, has a non-positive amount
    /// or oversized description, or its amounts don't sum to `total_raised`.
    InvalidMilestoneSchedule = 20,
    /// No milestone exists with the given id.
    MilestoneNotFound = 21,
    /// Milestone is not `Pending`, or its voting window has closed.
    MilestoneNotPending = 22,
    /// `release_milestone` called on a milestone that isn't `Approved`.
    MilestoneNotApproved = 23,
    /// Caller already voted on this milestone.
    AlreadyVoted = 24,
    /// Caller has no recorded contribution, so has zero voting/refund weight.
    NoContributionWeight = 25,
    /// `withdraw`/`cancel`/`collect_pledges` blocked because a milestone
    /// schedule is active for this campaign.
    MilestoneModeActive = 26,
    /// `claim_milestone_refund` called on a milestone that isn't `Rejected`.
    MilestoneNotRejected = 27,
    /// `contribute`/`pledge` blocked: the address's cumulative committed
    /// amount reached the configured KYC threshold and the configured
    /// `KycVerifier` does not report the address as verified.
    KycRequired = 28,
    /// `set_kyc_gate_enabled` called before `configure_kyc_gate` has ever
    /// been called for this campaign.
    KycGateNotConfigured = 29,
    /// `initialize` called with a `token` address that doesn't implement
    /// the SEP-41 token interface (fail-fast check via `validate_token_address`).
    InvalidTokenAddress = 30,
}

#[contractclient(name = "NftContractClient")]
pub trait NftContract {
    fn mint(env: Env, to: Address) -> u128;
}

/// External KYC/AML attestation contract interface. The configured verifier
/// is expected to have already performed its own off-chain identity
/// verification and simply exposes the resulting status on-chain — this
/// contract never receives or stores personal data itself.
#[contractclient(name = "KycVerifierClient")]
pub trait KycVerifier {
    fn is_verified(env: Env, who: Address) -> bool;
}

#[contract]
pub struct CrowdfundContract;

#[contractimpl]
impl CrowdfundContract {
    /// Initializes a new crowdfunding campaign.
    ///
    /// # Arguments
    /// * `creator`            – The campaign creator's address.
    /// * `token`              – The token contract address used for contributions.
    /// * `goal`               – The funding goal (in the token's smallest unit).
    /// * `deadline`           – The campaign deadline as a ledger timestamp.
    /// * `min_contribution`   – The minimum contribution amount.
    /// * `platform_config`    – Optional platform configuration (address and fee in basis points).
    ///
    /// # Panics
    /// * If already initialized.
    /// * If platform fee is >= 10,000 (100%) — a fee of exactly 100% would
    ///   leave the creator with a zero payout (audit #31).
    /// * If bonus goal is not greater than the primary goal.
    pub fn initialize(
        env: Env,
        admin: Address,
        creator: Address,
        token: Address,
        goal: i128,
        deadline: u64,
        min_contribution: i128,
        platform_config: Option<PlatformConfig>,
        bonus_goal: Option<i128>,
        bonus_goal_description: Option<String>,
        expected_token_decimals: u32,
    ) -> Result<(), ContractError> {
        use crowdfund_initialize_function::{execute_initialize, InitParams};
        execute_initialize(
            &env,
            InitParams {
                admin,
                creator,
                token,
                expected_token_decimals,
                goal,
                deadline,
                min_contribution,
                platform_config,
                bonus_goal,
                bonus_goal_description,
            },
        )
    }

    /// Returns the list of all contributor addresses.
    pub fn contributors(env: Env) -> Vec<Address> {
        extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Contributors)
            .unwrap_or(Vec::new(&env))
    }

    /// Contribute tokens to the campaign.
    ///
    /// The contributor must authorize the call. Contributions are rejected
    /// after the deadline has passed or if the campaign is not active.
    pub fn contribute(env: Env, contributor: Address, amount: i128) -> Result<(), ContractError> {
        contributor.require_auth();
        extend_instance_ttl(&env);

        // Guard: campaign must be active.
        let status: Status = env.storage().instance().get(&DataKey::Status).unwrap();
        if status != Status::Active {
            return Err(ContractError::CampaignNotActive);
        }

        if amount == 0 {
            return Err(ContractError::ZeroAmount);
        }

        let min_contribution: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinContribution)
            .unwrap();
        if amount < min_contribution {
            return Err(ContractError::BelowMinimum);
        }

        let deadline: u64 = env.storage().instance().get(&DataKey::Deadline).unwrap();
        if env.ledger().timestamp() > deadline {
            return Err(ContractError::CampaignEnded);
        }

        let contributors: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Contributors)
            .unwrap_or_else(|| Vec::new(&env));
        let is_new_contributor = !contributors.contains(&contributor);
        if is_new_contributor
            && !contract_state_size::validate_contributor_capacity(contributors.len())
        {
            return Err(ContractError::InvalidParameter);
        }

        // KYC gate: no-op unless a gate has been configured for this
        // campaign (see `kyc_gate` module) and this contribution pushes the
        // contributor's cumulative committed amount to/past the threshold.
        kyc_gate::enforce_kyc_gate(&env, &contributor, amount)?;

        let token_address: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_address);
        let stored_decimals: u32 = env.storage().instance().get(&DataKey::TokenDecimals).unwrap();
        if token_client.decimals() != stored_decimals {
            return Err(ContractError::InvalidParameter);
        }

        // Transfer tokens from the contributor to this contract.
        token_client.transfer(&contributor, env.current_contract_address(), &amount);

        // Update the contributor's running total with overflow protection.
        let contribution_key = DataKey::Contribution(contributor.clone());
        let previous_amount: i128 = env
            .storage()
            .persistent()
            .get(&contribution_key)
            .unwrap_or(0);

        let new_contribution = previous_amount
            .checked_add(amount)
            .ok_or(ContractError::Overflow)?;

        env.storage()
            .persistent()
            .set(&contribution_key, &new_contribution);
        bump_persistent(&env, &contribution_key);

        // Update the global total raised with overflow protection.
        let total: i128 = env.storage().instance().get(&DataKey::TotalRaised).unwrap();

        let new_total = total.checked_add(amount).ok_or(ContractError::Overflow)?;

        env.storage()
            .instance()
            .set(&DataKey::TotalRaised, &new_total);

        // Emit goal_reached exactly once when the primary goal is first met.
        let goal: i128 = env.storage().instance().get(&DataKey::Goal).unwrap();
        let goal_already_emitted = env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::GoalReachedEmitted)
            .unwrap_or(false);
        if !goal_already_emitted && total < goal && new_total >= goal {
            emit_goal_reached(&env, new_total, goal);
            env.storage()
                .instance()
                .set(&DataKey::GoalReachedEmitted, &true);
        }

        if let Some(bg) = env.storage().instance().get::<_, i128>(&DataKey::BonusGoal) {
            let already_emitted = env
                .storage()
                .instance()
                .get::<_, bool>(&DataKey::BonusGoalReachedEmitted)
                .unwrap_or(false);
            if !already_emitted && total < bg && new_total >= bg {
                env.events()
                    .publish(("crowdfund", "bonus_goal_reached"), bg);
                env.storage()
                    .instance()
                    .set(&DataKey::BonusGoalReachedEmitted, &true);
            }
        }

        // Emit stretch_goal_reached for each newly-reached stretch-goal milestone.
        // Stretch goals are purely informational — see `add_stretch_goal` docs.
        let stretch_goals: Vec<i128> = env
            .storage()
            .instance()
            .get(&DataKey::StretchGoals)
            .unwrap_or_else(|| Vec::new(&env));
        let mut already_reached: Vec<i128> = env
            .storage()
            .instance()
            .get(&DataKey::StretchGoalReachedEmitted)
            .unwrap_or_else(|| Vec::new(&env));
        for milestone in stretch_goals.iter() {
            if new_total >= milestone && !already_reached.contains(&milestone) {
                emit_stretch_goal_reached(&env, milestone, new_total);
                already_reached.push_back(milestone);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::StretchGoalReachedEmitted, &already_reached);

        let mut contributors: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Contributors)
            .unwrap_or_else(|| Vec::new(&env));

        if !contributors.contains(&contributor) {
            // Enforce contributor list size limit before appending.
            contract_state_size::check_contributor_limit(&env)
                .map_err(|_| ContractError::InvalidParameter)?;
            contributors.push_back(contributor.clone());
            env.storage()
                .persistent()
                .set(&DataKey::Contributors, &contributors);
            bump_persistent(&env, &DataKey::Contributors);
        }

        emit_contributed(&env, &contributor, amount, new_total);

        Ok(())
    }

    /// Sets the NFT contract address used for reward minting.
    ///
    /// Only the campaign creator can configure this value.
    pub fn set_nft_contract(
        env: Env,
        creator: Address,
        nft_contract: Address,
    ) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        let stored_creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        if creator != stored_creator {
            return Err(ContractError::Unauthorized);
        }
        creator.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::NFTContract, &nft_contract);
        Ok(())
    }

    /// Pledge tokens to the campaign without transferring them immediately.
    ///
    /// The pledger must authorize the call. Pledges are recorded off-chain
    /// and only collected if the goal is met after the deadline.
    pub fn pledge(env: Env, pledger: Address, amount: i128) -> Result<(), ContractError> {
        pledger.require_auth();
        extend_instance_ttl(&env);

        let min_contribution: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinContribution)
            .unwrap();
        if amount < min_contribution {
            return Err(ContractError::BelowMinimum);
        }

        let deadline: u64 = env.storage().instance().get(&DataKey::Deadline).unwrap();
        if env.ledger().timestamp() > deadline {
            return Err(ContractError::CampaignEnded);
        }

        let pledgers: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Pledgers)
            .unwrap_or_else(|| Vec::new(&env));
        let is_new_pledger = !pledgers.contains(&pledger);
        if is_new_pledger && !contract_state_size::validate_pledger_capacity(pledgers.len()) {
            return Err(ContractError::InvalidParameter);
        }

        // KYC gate: same guard as `contribute` — no-op unless configured.
        kyc_gate::enforce_kyc_gate(&env, &pledger, amount)?;

        // Update the pledger's running total.
        let pledge_key = DataKey::Pledge(pledger.clone());
        let prev: i128 = env.storage().persistent().get(&pledge_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&pledge_key, &(prev + amount));
        bump_persistent(&env, &pledge_key);

        // Update the global total pledged.
        let total_pledged: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPledged)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalPledged, &(total_pledged + amount));

        // Track pledger address if new.
        let mut pledgers: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Pledgers)
            .unwrap_or_else(|| Vec::new(&env));
        if !pledgers.contains(&pledger) {
            // Enforce pledger list size limit before appending.
            contract_state_size::check_pledger_limit(&env)
                .map_err(|_| ContractError::InvalidParameter)?;
            pledgers.push_back(pledger.clone());
            env.storage()
                .persistent()
                .set(&DataKey::Pledgers, &pledgers);
            bump_persistent(&env, &DataKey::Pledgers);
        }

        env.events()
            .publish(("crowdfund", "pledged"), (pledger, amount));

        Ok(())
    }

    /// Collect all pledges after the deadline when the goal is met.
    ///
    /// This function transfers tokens from all pledgers to the contract.
    /// Only callable after the deadline and when the combined total of
    /// contributions and pledges meets or exceeds the goal.
    pub fn collect_pledges(env: Env) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        let status: Status = env.storage().instance().get(&DataKey::Status).unwrap();
        if status != Status::Active {
            return Err(ContractError::CampaignNotActive);
        }

        let milestones: Vec<Milestone> = env
            .storage()
            .instance()
            .get(&DataKey::Milestones)
            .unwrap_or_else(|| Vec::new(&env));
        if !milestones.is_empty() {
            return Err(ContractError::MilestoneModeActive);
        }

        let deadline: u64 = env.storage().instance().get(&DataKey::Deadline).unwrap();
        if env.ledger().timestamp() <= deadline {
            return Err(ContractError::CampaignStillActive);
        }

        let goal: i128 = env.storage().instance().get(&DataKey::Goal).unwrap();
        let total_raised: i128 = env.storage().instance().get(&DataKey::TotalRaised).unwrap();
        let total_pledged: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalPledged)
            .unwrap_or(0);

        // Check if combined total meets the goal
        if total_raised + total_pledged < goal {
            return Err(ContractError::GoalNotReached);
        }

        let token_address: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_address);

        let pledgers: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Pledgers)
            .unwrap_or_else(|| Vec::new(&env));

        // Collect pledges from all pledgers. A pledger's transfer may fail
        // (e.g. a blocklisted address on a compliance-gated SEP-41 token) —
        // `try_transfer` catches that instead of reverting the whole batch,
        // and the failing entry is left in place so it can be retried later.
        let mut collected_total: i128 = 0;
        for pledger in pledgers.iter() {
            let pledge_key = DataKey::Pledge(pledger.clone());
            let amount: i128 = env.storage().persistent().get(&pledge_key).unwrap_or(0);
            if amount > 0 {
                // Effect before interaction: clear the pledge first so a
                // reentrant call sees nothing owed.
                env.storage().persistent().set(&pledge_key, &0i128);

                match token_client.try_transfer(&pledger, env.current_contract_address(), &amount)
                {
                    Ok(Ok(())) => {
                        bump_persistent(&env, &pledge_key);
                        collected_total = collected_total
                            .checked_add(amount)
                            .expect("pledge collection overflow");
                    }
                    _ => {
                        // Transfer failed — restore the pledge so it remains
                        // retryable on a future call.
                        env.storage().persistent().set(&pledge_key, &amount);
                        bump_persistent(&env, &pledge_key);
                        emit_transfer_skipped(
                            &env,
                            &pledger,
                            amount,
                            Symbol::new(&env, "collect_pledges"),
                        );
                    }
                }
            }
        }

        // Update total raised/pledged to reflect only the pledges actually
        // collected — any skipped entries remain pledged for a later attempt.
        env.storage()
            .instance()
            .set(&DataKey::TotalRaised, &(total_raised + collected_total));
        env.storage().instance().set(
            &DataKey::TotalPledged,
            &(total_pledged - collected_total),
        );

        env.events()
            .publish(("crowdfund", "pledges_collected"), collected_total);

        Ok(())
    }

    /// Withdraw raised funds — only callable by the creator after the
    /// deadline, and only if the goal has been met.
    ///
    /// If a platform fee is configured, deducts the fee and transfers it to
    /// the platform address, then sends the remainder to the creator.
    pub fn withdraw(env: Env) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        let status: Status = env.storage().instance().get(&DataKey::Status).unwrap();
        if status != Status::Active {
            return Err(ContractError::CampaignNotActive);
        }

        let milestones: Vec<Milestone> = env
            .storage()
            .instance()
            .get(&DataKey::Milestones)
            .unwrap_or_else(|| Vec::new(&env));
        if !milestones.is_empty() {
            return Err(ContractError::MilestoneModeActive);
        }

        let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        creator.require_auth();

        let deadline: u64 = env.storage().instance().get(&DataKey::Deadline).unwrap();
        if env.ledger().timestamp() <= deadline {
            return Err(ContractError::CampaignStillActive);
        }

        let goal: i128 = env.storage().instance().get(&DataKey::Goal).unwrap();
        let total: i128 = env.storage().instance().get(&DataKey::TotalRaised).unwrap();
        if total < goal {
            return Err(ContractError::GoalNotReached);
        }

        let token_address: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_address);

        let platform_config: Option<PlatformConfig> =
            env.storage().instance().get(&DataKey::PlatformConfig);

        let (creator_payout, platform_fee) = if let Some(config) = platform_config {
            let fee = total
                .checked_mul(config.fee_bps as i128)
                .expect("fee calculation overflow")
                .checked_div(10_000)
                .expect("fee division by zero");

            token_client.transfer(&env.current_contract_address(), &config.address, &fee);
            emit_fee_transferred(&env, &config.address, fee);
            (
                total.checked_sub(fee).expect("creator payout underflow"),
                fee,
            )
        } else {
            (total, 0)
        };

        token_client.transfer(&env.current_contract_address(), &creator, &creator_payout);

        env.storage().instance().set(&DataKey::TotalRaised, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::Status, &Status::Successful);

        let nft_contract: Option<Address> = env.storage().instance().get(&DataKey::NFTContract);
        mint_nfts_in_batch(&env, &nft_contract);

        emit_withdrawn(&env, &creator, creator_payout, platform_fee);

        Ok(())
    }

    /// Refund all contributors in a single batch transaction.
    ///
    /// # Deprecation Notice
    ///
    /// **This function is deprecated as of contract v3 and will be removed in a future version.**
    ///
    /// Use `refund_single` instead. The pull-based model is preferred because:
    /// - It avoids unbounded iteration over the contributors list (gas safety).
    /// - Each contributor controls their own refund timing.
    /// - It is composable with scripts and automation tooling.
    ///
    /// This function remains callable for backward compatibility but may be
    /// removed in a future upgrade. Scripts and integrations should migrate to
    /// `refund_single`.
    #[allow(deprecated)]
    pub fn refund(env: Env) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        let status: Status = env.storage().instance().get(&DataKey::Status).unwrap();
        if status != Status::Active {
            return Err(ContractError::CampaignNotActive);
        }

        let deadline: u64 = env.storage().instance().get(&DataKey::Deadline).unwrap();
        if env.ledger().timestamp() <= deadline {
            return Err(ContractError::CampaignStillActive);
        }

        let goal: i128 = env.storage().instance().get(&DataKey::Goal).unwrap();
        let total: i128 = env.storage().instance().get(&DataKey::TotalRaised).unwrap();
        if total >= goal {
            return Err(ContractError::GoalReached);
        }

        let token_address: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_address);

        let contributors: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Contributors)
            .unwrap();

        let mut refunded_total: i128 = 0;
        let mut skipped_count: u32 = 0;
        for contributor in contributors.iter() {
            let contribution_key = DataKey::Contribution(contributor.clone());
            let amount: i128 = env
                .storage()
                .persistent()
                .get(&contribution_key)
                .unwrap_or(0);
            if amount > 0 {
                // Effect before interaction: zero the contribution first so a
                // reentrant call sees nothing owed.
                env.storage().persistent().set(&contribution_key, &0i128);

                match token_client.try_transfer(
                    &env.current_contract_address(),
                    &contributor,
                    &amount,
                ) {
                    Ok(Ok(())) => {
                        bump_persistent(&env, &contribution_key);
                        refunded_total = refunded_total
                            .checked_add(amount)
                            .expect("refund total overflow");
                        emit_refunded(&env, &contributor, amount);
                    }
                    _ => {
                        // Transfer failed (e.g. blocklisted address) —
                        // restore the contribution so it remains retryable.
                        env.storage().persistent().set(&contribution_key, &amount);
                        bump_persistent(&env, &contribution_key);
                        skipped_count += 1;
                        emit_transfer_skipped(&env, &contributor, amount, Symbol::new(&env, "refund"));
                    }
                }
            }
        }

        let new_total = total
            .checked_sub(refunded_total)
            .expect("total_raised underflow");
        env.storage().instance().set(&DataKey::TotalRaised, &new_total);
        // Only mark the campaign fully Refunded once every contributor has
        // actually been paid out; otherwise leave it Active so `refund` can
        // be retried later (or stragglers can self-serve via `refund_single`).
        if skipped_count == 0 {
            env.storage()
                .instance()
                .set(&DataKey::Status, &Status::Refunded);
        }

        Ok(())
    }

    /// Claim a refund for a single contributor (pull-based).
    ///
    /// Each contributor independently claims their own refund after the campaign
    /// deadline has passed and the goal was not met.
    ///
    /// # Arguments
    /// * `contributor` – The address claiming the refund. Must match the caller.
    ///
    /// # Errors
    /// * [`ContractError::CampaignStillActive`] – Deadline has not yet passed.
    /// * [`ContractError::GoalReached`]         – Goal was met; no refunds available.
    /// * [`ContractError::NothingToRefund`]     – Caller has no contribution on record.
    ///
    /// # Security
    /// * Requires `contributor.require_auth()` — only the contributor can claim.
    /// * Zeroes the contribution record **before** transfer (checks-effects-interactions).
    /// * Uses `checked_sub` to prevent underflow on `total_raised`.
    pub fn refund_single(env: Env, contributor: Address) -> Result<(), ContractError> {
        contributor.require_auth();
        extend_instance_ttl(&env);
        validate_refund_preconditions(&env, &contributor)?;
        execute_refund_single(&env, &contributor)?;
        Ok(())
    }

    /// Cancel the campaign and refund all contributors — callable only by
    /// the creator while the campaign is still Active.
    pub fn cancel(env: Env) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        let status: Status = env.storage().instance().get(&DataKey::Status).unwrap();
        if status != Status::Active {
            return Err(ContractError::CampaignNotActive);
        }

        let milestones: Vec<Milestone> = env
            .storage()
            .instance()
            .get(&DataKey::Milestones)
            .unwrap_or_else(|| Vec::new(&env));
        if !milestones.is_empty() {
            return Err(ContractError::MilestoneModeActive);
        }

        let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        creator.require_auth();

        let token_address: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let token_client = token::Client::new(&env, &token_address);

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0);

        let contributors: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Contributors)
            .unwrap_or_else(|| Vec::new(&env));

        let mut refunded_total: i128 = 0;
        let mut skipped_count: u32 = 0;
        for contributor in contributors.iter() {
            let contribution_key = DataKey::Contribution(contributor.clone());
            let amount: i128 = env
                .storage()
                .persistent()
                .get(&contribution_key)
                .unwrap_or(0);
            if amount > 0 {
                // Effect before interaction: zero the contribution first so a
                // reentrant call sees nothing owed.
                env.storage().persistent().set(&contribution_key, &0i128);

                match token_client.try_transfer(
                    &env.current_contract_address(),
                    &contributor,
                    &amount,
                ) {
                    Ok(Ok(())) => {
                        bump_persistent(&env, &contribution_key);
                        refunded_total = refunded_total
                            .checked_add(amount)
                            .expect("refund total overflow");
                        emit_refunded(&env, &contributor, amount);
                    }
                    _ => {
                        // Transfer failed (e.g. blocklisted address) —
                        // restore the contribution so it remains retryable.
                        env.storage().persistent().set(&contribution_key, &amount);
                        bump_persistent(&env, &contribution_key);
                        skipped_count += 1;
                        emit_transfer_skipped(&env, &contributor, amount, Symbol::new(&env, "cancel"));
                    }
                }
            }
        }

        let new_total = total
            .checked_sub(refunded_total)
            .expect("total_raised underflow");
        env.storage().instance().set(&DataKey::TotalRaised, &new_total);
        // Only mark the campaign fully Cancelled once every contributor has
        // actually been paid out; otherwise leave it Active so `cancel` can
        // be retried later (or stragglers can self-serve via `refund_single`
        // once the deadline passes).
        if skipped_count == 0 {
            env.storage()
                .instance()
                .set(&DataKey::Status, &Status::Cancelled);
            emit_cancelled(&env);
        }
        Ok(())
    }

    /// Upgrade the contract to a new WASM implementation — admin-only.
    ///
    /// This function allows the designated admin to upgrade the contract's WASM code
    /// without changing the contract's address or storage. The new WASM hash must be
    /// provided and the caller must be authorized as the admin.
    ///
    /// # Arguments
    /// * `new_wasm_hash`     – The SHA-256 hash of the new WASM binary to deploy.
    /// * `current_wasm_hash` – The SHA-256 hash of the currently deployed WASM binary.
    ///                         This is stored as the rollback point before the upgrade
    ///                         is applied. If the new WASM is broken, call
    ///                         `rollback_upgrade` to restore this hash.
    ///
    /// # Panics
    /// * If the caller is not the admin.
    /// * If `new_wasm_hash` is all zeros.
    pub fn upgrade(
        env: Env,
        new_wasm_hash: soroban_sdk::BytesN<32>,
        current_wasm_hash: soroban_sdk::BytesN<32>,
    ) {
        let admin = admin_upgrade_mechanism::validate_admin_upgrade(&env);

        // Store the current WASM hash as the rollback point before applying the upgrade.
        // This ensures we can always restore the previous working implementation.
        admin_upgrade_mechanism::store_current_wasm_hash(&env, &current_wasm_hash);

        admin_upgrade_mechanism::perform_upgrade(&env, new_wasm_hash.clone());

        env.events()
            .publish(("crowdfund", "upgrade"), (admin, current_wasm_hash, new_wasm_hash));
    }

    /// Rollback the contract to the previous WASM implementation — admin-only.
    ///
    /// This function restores the WASM implementation that was stored as the rollback
    /// point during the last successful `upgrade()` call. If the new WASM introduced
    /// a bug or storage layout mismatch, this allows the admin to recover without
    /// losing access to contract funds.
    ///
    /// # Panics
    /// * If the caller is not the admin.
    /// * If no previous WASM hash is stored (no prior upgrade was performed).
    ///
    /// # Returns
    /// The restored WASM hash.
    pub fn rollback_upgrade(env: Env) -> soroban_sdk::BytesN<32> {
        let admin = admin_upgrade_mechanism::validate_admin_upgrade(&env);
        let restored_hash = admin_upgrade_mechanism::rollback_upgrade(&env);

        env.events()
            .publish(("crowdfund", "rollback"), (admin, restored_hash.clone()));

        restored_hash
    }

    /// Update campaign metadata — only callable by the creator while the
    /// campaign is still Active.
    ///
    /// # Arguments
    /// * `creator`     – The campaign creator's address (for authentication).
    /// * `title`       – Optional new title (None to keep existing).
    /// * `description` – Optional new description (None to keep existing).
    /// * `socials`    – Optional new social links (None to keep existing).
    pub fn update_metadata(
        env: Env,
        creator: Address,
        title: Option<String>,
        description: Option<String>,
        socials: Option<String>,
    ) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        // Check campaign is active.
        let status: Status = env.storage().instance().get(&DataKey::Status).unwrap();
        if status != Status::Active {
            return Err(ContractError::CampaignNotActive);
        }

        // Require creator authentication and verify caller is the creator.
        let stored_creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        if creator != stored_creator {
            return Err(ContractError::Unauthorized);
        }
        creator.require_auth();

        // Track which fields were updated for the event.
        let mut updated_fields: Vec<Symbol> = Vec::new(&env);

        let current_title = env.storage().instance().get::<_, String>(&DataKey::Title);
        let current_description = env
            .storage()
            .instance()
            .get::<_, String>(&DataKey::Description);
        let current_socials = env
            .storage()
            .instance()
            .get::<_, String>(&DataKey::SocialLinks);

        let title_length = title
            .as_ref()
            .map(|value| value.len())
            .or_else(|| current_title.as_ref().map(|value| value.len()))
            .unwrap_or(0);
        let description_length = description
            .as_ref()
            .map(|value| value.len())
            .or_else(|| current_description.as_ref().map(|value| value.len()))
            .unwrap_or(0);
        let socials_length = socials
            .as_ref()
            .map(|value| value.len())
            .or_else(|| current_socials.as_ref().map(|value| value.len()))
            .unwrap_or(0);
        if !contract_state_size::validate_metadata_total_length(
            title_length + description_length + socials_length,
        ) {
            return Err(ContractError::InvalidParameter);
        }

        // Update title if provided.
        if let Some(new_title) = title {
            if !contract_state_size::validate_title(&new_title) {
                return Err(ContractError::InvalidParameter);
            }
            env.storage().instance().set(&DataKey::Title, &new_title);
            updated_fields.push_back(Symbol::new(&env, "title"));
        }

        // Update description if provided.
        if let Some(new_description) = description {
            if !contract_state_size::validate_description(&new_description) {
                return Err(ContractError::InvalidParameter);
            }
            env.storage()
                .instance()
                .set(&DataKey::Description, &new_description);
            updated_fields.push_back(Symbol::new(&env, "description"));
        }

        // Update social links if provided.
        if let Some(new_socials) = socials {
            if !contract_state_size::validate_social_links(&new_socials) {
                return Err(ContractError::InvalidParameter);
            }
            env.storage()
                .instance()
                .set(&DataKey::SocialLinks, &new_socials);
            updated_fields.push_back(Symbol::new(&env, "socials"));
        }

        env.events().publish(
            ("crowdfund", "metadata_updated"),
            (creator.clone(), updated_fields),
        );
        Ok(())
    }

    pub fn add_roadmap_item(env: Env, date: u64, description: String) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        creator.require_auth();

        if date <= env.ledger().timestamp() {
            return Err(ContractError::InvalidParameter);
        }

        if description.is_empty() {
            return Err(ContractError::InvalidParameter);
        }

        // Enforce string length and roadmap list size limits.
        contract_state_size::check_string_len(&description)
            .map_err(|_| ContractError::InvalidParameter)?;
        contract_state_size::check_roadmap_limit(&env)
            .map_err(|_| ContractError::InvalidParameter)?;

        let mut roadmap: Vec<RoadmapItem> = env
            .storage()
            .instance()
            .get(&DataKey::Roadmap)
            .unwrap_or_else(|| Vec::new(&env));
        if !contract_state_size::validate_roadmap_capacity(roadmap.len()) {
            return Err(ContractError::InvalidParameter);
        }
        for item in roadmap.iter() {
            if !contract_state_size::validate_roadmap_description(&item.description) {
                return Err(ContractError::InvalidParameter);
            }
        }

        roadmap.push_back(RoadmapItem {
            date,
            description: description.clone(),
        });

        env.storage().instance().set(&DataKey::Roadmap, &roadmap);
        env.events()
            .publish(("crowdfund", "roadmap_item_added"), (date, description));
        Ok(())
    }

    pub fn roadmap(env: Env) -> Vec<RoadmapItem> {
        extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Roadmap)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Add a stretch goal milestone to the campaign.
    ///
    /// Only the creator can add stretch goals. The milestone must be greater
    /// than the primary goal.
    ///
    /// # Design intent (purely informational)
    ///
    /// Stretch goals are **purely informational** — they serve as progress
    /// indicators for UI consumers and do **not** trigger any on-chain
    /// enforcement (no extra minting, no fee adjustment, no fund-release
    /// changes). When a contribution pushes `total_raised` past a stretch-goal
    /// milestone, a `stretch_goal_reached` event is emitted exactly once per
    /// milestone during `contribute()`.
    ///
    /// Downstream indexers and front-ends can listen for this event to
    /// display stretch-goal progress. If future protocol upgrades require
    /// on-chain enforcement at stretch-goal boundaries, new storage flags or
    /// contract logic should be added without altering the existing
    /// informational semantics for backward compatibility.
    pub fn add_stretch_goal(env: Env, milestone: i128) -> Result<(), ContractError> {
        extend_instance_ttl(&env);
        let creator: Address = env.storage().instance().get(&DataKey::Creator).unwrap();
        creator.require_auth();

        let goal: i128 = env.storage().instance().get(&DataKey::Goal).unwrap();
        if milestone <= goal {
            return Err(ContractError::InvalidParameter);
        }

        // Enforce stretch-goal list size limit.
        contract_state_size::check_stretch_goal_limit(&env)
            .map_err(|_| ContractError::InvalidParameter)?;

        let mut stretch_goals: Vec<i128> = env
            .storage()
            .instance()
            .get(&DataKey::StretchGoals)
            .unwrap_or_else(|| Vec::new(&env));
        if !contract_state_size::validate_stretch_goal_capacity(stretch_goals.len()) {
            return Err(ContractError::InvalidParameter);
        }

        stretch_goals.push_back(milestone);
        env.storage()
            .instance()
            .set(&DataKey::StretchGoals, &stretch_goals);
        Ok(())
    }

    /// Returns the next unmet stretch goal milestone.
    ///
    /// Returns 0 if there are no stretch goals or all have been met.
    ///
    /// # Design intent (purely informational)
    ///
    /// This is a **read-only informational getter** for UI/indexer consumption.
    /// Finding a next milestone here does **not** change any contract behavior
    /// (no fee adjustment, no fund-release changes, no extra minting). See
    /// [`add_stretch_goal`](#method.add_stretch_goal) for the full design rationale.
    pub fn current_milestone(env: Env) -> i128 {
        extend_instance_ttl(&env);
        let total_raised: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0);

        let stretch_goals: Vec<i128> = env
            .storage()
            .instance()
            .get(&DataKey::StretchGoals)
            .unwrap_or_else(|| Vec::new(&env));

        for milestone in stretch_goals.iter() {
            if total_raised < milestone {
                return milestone;
            }
        }

        0
    }

    // ── Milestone-gated partial release ──────────────────────────────────────

    /// Proposes a one-time milestone release schedule for the raised funds.
    ///
    /// Creator-only. Only callable once the deadline has passed and the goal
    /// has been met (the same gate as [`Self::withdraw`]), and only if no
    /// schedule has been proposed yet. The schedule's amounts must sum
    /// exactly to `total_raised`.
    pub fn propose_milestones(
        env: Env,
        creator: Address,
        milestones: Vec<MilestoneInput>,
    ) -> Result<(), ContractError> {
        execute_propose_milestones(&env, creator, milestones)
    }

    /// Casts a weighted vote (by contribution amount) to approve or reject
    /// a pending milestone.
    pub fn vote_milestone(
        env: Env,
        voter: Address,
        milestone_id: u32,
        approve: bool,
    ) -> Result<(), ContractError> {
        execute_vote_milestone(&env, voter, milestone_id, approve)
    }

    /// Permissionlessly resolves a milestone whose voting window has closed
    /// without crossing either threshold. Silence resolves to `Rejected`.
    pub fn finalize_milestone_vote(env: Env, milestone_id: u32) -> Result<(), ContractError> {
        execute_finalize_milestone_vote(&env, milestone_id)
    }

    /// Releases an `Approved` milestone's funds to the creator (minus any
    /// platform fee), creator-only.
    pub fn release_milestone(
        env: Env,
        creator: Address,
        milestone_id: u32,
    ) -> Result<(), ContractError> {
        execute_release_milestone(&env, creator, milestone_id)
    }

    /// Claims a pro-rata refund of a `Rejected` milestone's funds.
    pub fn claim_milestone_refund(
        env: Env,
        contributor: Address,
        milestone_id: u32,
    ) -> Result<(), ContractError> {
        execute_claim_milestone_refund(&env, contributor, milestone_id)
    }

    // ── KYC / AML gate (pluggable, off by default) ────────────────────────────

    /// Configures (or reconfigures) this campaign's KYC/AML gate — admin-only.
    ///
    /// Deliberately gated on `Admin`, not `Creator`: the party motivated to
    /// accept large, unverified pledges is the campaign creator, so the
    /// ability to enable, disable, or loosen this gate is kept out of their
    /// hands. The admin (the same role that can upgrade the contract) is
    /// expected to set `threshold`/`jurisdiction` per legal/compliance
    /// guidance — this contract only *enforces* the resulting threshold, it
    /// doesn't decide it.
    ///
    /// A campaign with no configured gate (the default) behaves exactly as
    /// it did before this feature existed.
    pub fn configure_kyc_gate(
        env: Env,
        admin: Address,
        verifier: Address,
        threshold: i128,
        jurisdiction: Symbol,
    ) -> Result<(), ContractError> {
        kyc_gate::execute_configure_kyc_gate(&env, admin, verifier, threshold, jurisdiction)
    }

    /// Toggles the KYC gate on/off without discarding its configured
    /// `verifier`/`threshold`/`jurisdiction` — admin-only. Fails with
    /// [`ContractError::KycGateNotConfigured`] if `configure_kyc_gate` has
    /// never been called for this campaign.
    pub fn set_kyc_gate_enabled(
        env: Env,
        admin: Address,
        enabled: bool,
    ) -> Result<(), ContractError> {
        kyc_gate::execute_set_kyc_gate_enabled(&env, admin, enabled)
    }

    /// Returns this campaign's KYC gate configuration, if one has been set.
    pub fn kyc_gate_config(env: Env) -> Option<KycGateConfig> {
        kyc_gate::kyc_gate_config(&env)
    }

    /// Read-only preflight: would a `contribute`/`pledge` of `amount` by
    /// `who` be allowed right now? Lets a frontend prompt for KYC
    /// verification *before* submitting a transaction that would otherwise
    /// fail on-chain with [`ContractError::KycRequired`], so the gate never
    /// surprises a backer as a failed transaction.
    pub fn kyc_gate_preview(env: Env, who: Address, amount: i128) -> bool {
        kyc_gate::would_pass_kyc_gate(&env, &who, amount)
    }

    /// Returns the full milestone schedule for this campaign (empty if none
    /// has been proposed).
    pub fn milestones(env: Env) -> Vec<Milestone> {
        env.storage()
            .instance()
            .get(&DataKey::Milestones)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Returns a single milestone by id, if it exists.
    pub fn milestone(env: Env, milestone_id: u32) -> Option<Milestone> {
        let milestones: Vec<Milestone> = env
            .storage()
            .instance()
            .get(&DataKey::Milestones)
            .unwrap_or_else(|| Vec::new(&env));
        milestones.iter().find(|m| m.id == milestone_id)
    }

    /// Returns the frozen `total_raised` snapshot the milestone schedule is
    /// reconciled and voted against. `0` if no schedule has been proposed.
    pub fn milestone_basis(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::MilestoneBasis)
            .unwrap_or(0)
    }

    /// Returns `true` if `voter` has already voted on the given milestone.
    pub fn has_voted_milestone(env: Env, milestone_id: u32, voter: Address) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::MilestoneVote(MilestoneVoteKey {
                milestone_id,
                voter,
            }))
    }

    /// Returns `true` if `contributor` has already claimed their pro-rata
    /// refund for the given (rejected) milestone.
    pub fn has_claimed_milestone_refund(env: Env, milestone_id: u32, contributor: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::MilestoneRefundClaimed(MilestoneRefundKey {
                milestone_id,
                contributor,
            }))
            .unwrap_or(false)
    }

    /// Returns the campaign's current status.
    pub fn status(env: Env) -> Status {
        env.storage().instance().get(&DataKey::Status).unwrap()
    }

    pub fn total_raised(env: Env) -> i128 {
        extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0)
    }

    pub fn goal(env: Env) -> i128 {
        extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::Goal).unwrap()
    }

    /// Returns the optional bonus-goal threshold.
    ///
    /// # Design intent (purely informational)
    ///
    /// The bonus goal is a **purely informational** milestone. When
    /// `total_raised` crosses the bonus-goal threshold, a
    /// `bonus_goal_reached` event is emitted exactly once (see
    /// [`contribute`](#method.contribute)), but no on-chain enforcement
    /// (extra minting, fee changes, or fund-release schedule) is triggered.
    /// Downstream consumers should treat this as a progress indicator.
    pub fn bonus_goal(env: Env) -> Option<i128> {
        extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::BonusGoal)
    }

    /// Returns the optional bonus-goal description.
    pub fn bonus_goal_description(env: Env) -> Option<String> {
        extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::BonusGoalDescription)
    }

    /// Returns true if the optional bonus goal has been reached.
    ///
    /// # Design intent (purely informational)
    ///
    /// This is a **read-only informational getter**. Crossing the bonus-goal
    /// threshold does **not** alter contract behavior — see
    /// [`bonus_goal`](#method.bonus_goal) for the full design rationale.
    pub fn bonus_goal_reached(env: Env) -> bool {
        extend_instance_ttl(&env);
        let total_raised: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0);

        if let Some(bg) = env.storage().instance().get::<_, i128>(&DataKey::BonusGoal) {
            total_raised >= bg
        } else {
            false
        }
    }

    /// Returns bonus-goal progress in basis points (capped at 10,000).
    pub fn bonus_goal_progress_bps(env: Env) -> u32 {
        extend_instance_ttl(&env);
        let total_raised: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0);

        if let Some(bg) = env.storage().instance().get::<_, i128>(&DataKey::BonusGoal) {
            if bg > 0 {
                let raw = (total_raised * 10_000) / bg;
                if raw > 10_000 {
                    10_000
                } else {
                    raw as u32
                }
            } else {
                0
            }
        } else {
            0
        }
    }

    /// Returns the campaign deadline.
    pub fn deadline(env: Env) -> u64 {
        extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::Deadline).unwrap()
    }

    pub fn contribution(env: Env, contributor: Address) -> i128 {
        extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Contribution(contributor))
            .unwrap_or(0)
    }

    pub fn min_contribution(env: Env) -> i128 {
        extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::MinContribution)
            .unwrap()
    }

    /// Returns comprehensive campaign statistics.
    pub fn get_stats(env: Env) -> CampaignStats {
        extend_instance_ttl(&env);
        let total_raised: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalRaised)
            .unwrap_or(0);
        let goal: i128 = env.storage().instance().get(&DataKey::Goal).unwrap();
        let contributors: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::Contributors)
            .unwrap_or_else(|| Vec::new(&env));

        let progress_bps = if goal > 0 {
            let raw = (total_raised * 10_000) / goal;
            if raw > 10_000 {
                10_000
            } else {
                raw as u32
            }
        } else {
            0
        };

        let contributor_count = contributors.len();
        let (average_contribution, largest_contribution) = if contributor_count == 0 {
            (0, 0)
        } else {
            let average = total_raised / contributor_count as i128;
            let mut largest = 0i128;
            for contributor in contributors.iter() {
                let amount: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::Contribution(contributor))
                    .unwrap_or(0);
                if amount > largest {
                    largest = amount;
                }
            }
            (average, largest)
        };

        CampaignStats {
            total_raised,
            goal,
            progress_bps,
            contributor_count,
            average_contribution,
            largest_contribution,
        }
    }

    pub fn title(env: Env) -> String {
        extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Title)
            .unwrap_or_else(|| String::from_str(&env, ""))
    }

    pub fn description(env: Env) -> String {
        extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Description)
            .unwrap_or_else(|| String::from_str(&env, ""))
    }

    pub fn socials(env: Env) -> String {
        extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::SocialLinks)
            .unwrap_or_else(|| String::from_str(&env, ""))
    }

    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
    }

    /// Returns the token contract address used for contributions.
    pub fn token(env: Env) -> Address {
        extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::Token).unwrap()
    }

    /// Returns the configured NFT contract address, if any.
    pub fn nft_contract(env: Env) -> Option<Address> {
        extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::NFTContract)
    }

    /// Permissionless rent-extension heartbeat.
    ///
    /// Anyone may call `keep_alive` to bump the TTL of both the instance-storage
    /// bucket and the two global persistent lists (`Contributors`, `Pledgers`) so
    /// that a long-dormant Active campaign cannot be silently archived by the
    /// Soroban network before its deadline arrives.
    ///
    /// # Why this is safe
    /// * No authentication required — extending rent harms no one.
    /// * No state mutation beyond TTL bumps — storage contents are unchanged.
    /// * The function is a no-op on already-healthy entries (the protocol only
    ///   extends when the current TTL is *below* `LEDGER_THRESHOLD`).
    ///
    /// # Errors
    /// Always returns `Ok(())`.
    pub fn keep_alive(env: Env) -> Result<(), ContractError> {
        // Bump instance storage (covers all instance-storage keys in one call).
        extend_instance_ttl(&env);

        // Bump the Contributors persistent list if it exists.
        if env
            .storage()
            .persistent()
            .has(&DataKey::Contributors)
        {
            bump_persistent(&env, &DataKey::Contributors);
        }

        // Bump the Pledgers persistent list if it exists.
        if env.storage().persistent().has(&DataKey::Pledgers) {
            bump_persistent(&env, &DataKey::Pledgers);
        }

        env.events().publish(("crowdfund", "keep_alive"), ());

        Ok(())
    }
}
