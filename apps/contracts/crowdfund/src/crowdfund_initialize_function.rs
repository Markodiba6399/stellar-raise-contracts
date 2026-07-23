//! Validated initialization logic for the crowdfund contract.
//!
//! ## Validation order
//! ```text
//! execute_initialize(env, params)
//!   ├─► re-initialization guard     → AlreadyInitialized
//!   ├─► creator.require_auth()
//!   ├─► validate_goal               → InvalidGoal
//!   ├─► validate_min_contribution   → InvalidMinContribution
//!   ├─► validate_deadline           → DeadlineTooSoon
//!   ├─► validate_platform_fee       → InvalidPlatformFee
//!   ├─► validate_bonus_goal         → InvalidBonusGoal
//!   ├─► validate_token_address      → InvalidTokenAddress
//!   └─► [all passed] write storage → emit event → Ok(())
//! ```

use soroban_sdk::{token, Address, Env, String, Symbol, Vec};

use crate::campaign_goal_minimum::{
    validate_deadline, validate_goal, validate_min_contribution, validate_platform_fee,
};
use crate::{extend_instance_ttl, ContractError, DataKey, PlatformConfig, RoadmapItem, Status};

// ── InitParams ────────────────────────────────────────────────────────────────

/// Named parameter struct for campaign initialization.
///
/// Using a struct instead of positional arguments prevents silent
/// parameter-order bugs when two adjacent fields share the same type.
#[derive(Clone)]
pub struct InitParams {
    /// Admin address authorized to upgrade the contract.
    pub admin: Address,
    /// Campaign creator; must authorize the `initialize()` call.
    pub creator: Address,
    /// SEP-41 token contract address used for contributions.
    pub token: Address,
    /// The expected number of decimals for the configured token.
    pub expected_token_decimals: u32,
    /// Funding goal in token's smallest unit. Must be >= 1.
    pub goal: i128,
    /// Campaign deadline as a Unix ledger timestamp. Must be >= now + 60s.
    pub deadline: u64,
    /// Minimum single contribution. Must be >= 1.
    pub min_contribution: i128,
    /// Optional platform fee configuration. `fee_bps` must be < 10_000 (100%
    /// is rejected — see audit #31, a fee of exactly 100% leaves the creator
    /// with a zero payout).
    pub platform_config: Option<PlatformConfig>,
    /// Optional bonus goal. When provided, must be > `goal`.
    pub bonus_goal: Option<i128>,
    /// Optional human-readable description for the bonus goal.
    pub bonus_goal_description: Option<String>,
}

// ── Validation helpers ────────────────────────────────────────────────────────

/// Validates that `bonus_goal`, when present, is strictly greater than `goal`.
#[inline]
pub fn validate_bonus_goal(bonus_goal: Option<i128>, goal: i128) -> Result<(), ContractError> {
    if let Some(bg) = bonus_goal {
        if bg <= goal {
            return Err(ContractError::InvalidBonusGoal);
        }
    }
    Ok(())
}

/// Validates that `token_address` points to a real SEP-41 token contract
/// by making a cheap read-only call (`name()`).
///
/// Fail-fast: catches a non-token (or invalid) address during initialization
/// rather than surfacing the issue only on the first `contribute()` call.
#[inline]
pub fn validate_token_address(env: &Env, token_address: &Address) -> Result<(), ContractError> {
    let client = token::Client::new(env, token_address);
    // A cheap read-only call that will fail if the address does not
    // implement the SEP-41 token interface. `try_name` (SDK-generated,
    // backed by `env.try_invoke_contract`) catches that instead of
    // panicking the whole `initialize` call.
    match client.try_name() {
        Ok(Ok(_)) => Ok(()),
        _ => Err(ContractError::InvalidTokenAddress),
    }
}

fn validate_init_params(env: &Env, params: &InitParams) -> Result<(), ContractError> {
    validate_goal(params.goal).map_err(|_| ContractError::InvalidGoal)?;
    validate_min_contribution(params.min_contribution)
        .map_err(|_| ContractError::InvalidMinContribution)?;
    validate_deadline(env.ledger().timestamp(), params.deadline)
        .map_err(|_| ContractError::DeadlineTooSoon)?;
    if let Some(ref config) = params.platform_config {
        validate_platform_fee(config.fee_bps).map_err(|_| ContractError::InvalidPlatformFee)?;
    }
    validate_bonus_goal(params.bonus_goal, params.goal)?;
    validate_token_address(env, &params.token)?;
    Ok(())
}

// ── Core initialization ───────────────────────────────────────────────────────

/// Executes the full campaign initialization flow.
///
/// Ordering guarantee:
/// 1. Re-initialization guard (read-only, no mutation).
/// 2. Creator authentication.
/// 3. Full parameter validation (no storage writes yet).
/// 4. Storage writes (all fields in one pass).
/// 5. Event emission.
pub fn execute_initialize(env: &Env, params: InitParams) -> Result<(), ContractError> {
    // 1. Re-initialization guard.
    if env.storage().instance().has(&DataKey::Creator) {
        return Err(ContractError::AlreadyInitialized);
    }

    // 2. Auth before any mutation.
    params.creator.require_auth();

    // 3. Validate — no writes if any check fails.
    validate_init_params(env, &params)?;

    let token_client = soroban_sdk::token::Client::new(env, &params.token);
    if token_client.decimals() != params.expected_token_decimals {
        return Err(ContractError::InvalidParameter);
    }

    // 4. Write required fields.
    env.storage().instance().set(&DataKey::Admin, &params.admin);
    env.storage()
        .instance()
        .set(&DataKey::Creator, &params.creator);
    env.storage().instance().set(&DataKey::Token, &params.token);
    env.storage()
        .instance()
        .set(&DataKey::TokenDecimals, &params.expected_token_decimals);
    env.storage().instance().set(&DataKey::Goal, &params.goal);
    env.storage()
        .instance()
        .set(&DataKey::Deadline, &params.deadline);
    env.storage()
        .instance()
        .set(&DataKey::MinContribution, &params.min_contribution);
    env.storage().instance().set(&DataKey::TotalRaised, &0i128);
    env.storage()
        .instance()
        .set(&DataKey::BonusGoalReachedEmitted, &false);
    env.storage()
        .instance()
        .set(&DataKey::Status, &Status::Active);

    // Write optional fields.
    if let Some(ref config) = params.platform_config {
        env.storage()
            .instance()
            .set(&DataKey::PlatformConfig, config);
    }
    if let Some(bg) = params.bonus_goal {
        env.storage().instance().set(&DataKey::BonusGoal, &bg);
    }
    if let Some(ref desc) = params.bonus_goal_description {
        env.storage()
            .instance()
            .set(&DataKey::BonusGoalDescription, desc);
    }

    // Seed empty collections.
    let empty_contributors: Vec<Address> = Vec::new(env);
    env.storage()
        .persistent()
        .set(&DataKey::Contributors, &empty_contributors);
    // Bump persistent TTL for the Contributors list seeded at initialization.
    env.storage().persistent().extend_ttl(
        &DataKey::Contributors,
        crate::LEDGER_THRESHOLD,
        crate::LEDGER_BUMP_AMOUNT,
    );
    let empty_roadmap: Vec<RoadmapItem> = Vec::new(env);
    env.storage()
        .instance()
        .set(&DataKey::Roadmap, &empty_roadmap);

    // Bump instance TTL after all keys have been written so the contract
    // stays alive from the very first ledger.
    extend_instance_ttl(env);

    // 5. Emit bounded event (scalar fields only).
    log_initialize(
        env,
        &params.creator,
        &params.token,
        params.expected_token_decimals,
        params.goal,
        params.deadline,
        params.min_contribution,
    );

    Ok(())
}

// ── Event helper ──────────────────────────────────────────────────────────────

/// Emits a `("campaign", "initialized")` event with scalar fields only.
///
/// String fields are intentionally excluded to keep event size O(1).
pub fn log_initialize(
    env: &Env,
    creator: &Address,
    token: &Address,
    expected_token_decimals: u32,
    goal: i128,
    deadline: u64,
    min_contribution: i128,
) {
    env.events().publish(
        (
            Symbol::new(env, "campaign"),
            Symbol::new(env, "initialized"),
        ),
        (
            creator.clone(),
            token.clone(),
            expected_token_decimals,
            goal,
            deadline,
            min_contribution,
        ),
    );
}

// ── Frontend helpers ──────────────────────────────────────────────────────────

/// Maps a `ContractError` repr value to a human-readable message.
#[inline]
pub fn describe_init_error(code: u32) -> &'static str {
    match code {
        1 => "Contract is already initialized",
        8 => "Campaign goal must be at least 1",
        9 => "Minimum contribution must be at least 1",
        10 => "Deadline must be at least 60 seconds in the future",
        11 => "Platform fee must be below 100% (10,000 bps)",
        12 => "Bonus goal must be strictly greater than the primary goal",
        19 => "Token address does not implement SEP-41 interface",
        _ => "Unknown initialization error",
    }
}

/// Returns `true` if the error is a correctable input error that can be retried.
///
/// `AlreadyInitialized` (1) is permanent; all validation errors (8–12, 19) are retryable.
#[inline]
pub fn is_init_error_retryable(code: u32) -> bool {
    matches!(code, 8..=12 | 19)
}
