#![no_std]

use soroban_sdk::{
    contract, contractimpl, contracttype, token, Address, BytesN, Env, String, Symbol, Vec,
};

const MAX_PLATFORM_FEE_BPS: u32 = 500;
const MAX_ARBITER_FEE_BPS: u32 = 200;
const FULL_SPLIT_BPS: u32 = 10_000;

// ============================================================
// DATA TYPES
// ============================================================

/// Lifecycle state of a single milestone.
#[contracttype]
#[derive(Clone, PartialEq, Debug)]
pub enum MilestoneStatus {
    /// Retention milestones start here; `unlock_milestone` moves them to `Pending`
    /// once the required ledger window has elapsed.
    Locked,
    /// The milestone is open: the recruiter may submit a proof hash.
    Pending,
    /// The recruiter has submitted a proof hash; the company must now confirm or raise a dispute.
    ProofSubmitted,
    /// The company confirmed the proof; payment has been released to the recruiter.
    Confirmed,
    /// The company raised a dispute; arbiters must vote before the milestone can progress.
    Disputed,
    /// Arbiters voted to approve the milestone payment (dispute resolved in recruiter's favour).
    /// The milestone counts as done for the purposes of completing the engagement.
    Resolved,
}

/// Distinguishes the two business-logic types of milestones.
#[contracttype]
#[derive(Clone, PartialEq)]
pub enum MilestoneKind {
    /// Triggered by the recruiter placing a candidate (offer accepted).
    /// Starts `Pending` so the recruiter can submit proof immediately.
    Placement,
    /// Triggered by the candidate remaining employed past a time gate.
    /// Starts `Locked`; `unlock_milestone` moves it to `Pending` once
    /// `env.ledger().sequence() >= valid_after_ledger`.
    Retention,
}

/// A payment and workflow checkpoint belonging to an [`Engagement`]'s `milestones`.
#[contracttype]
#[derive(Clone)]
pub struct Milestone {
    /// Human-readable label shown in the UI (e.g. "30-day retention").
    pub name: String,
    /// Percentage of `total_amount` released when this milestone is confirmed.
    /// All milestone percentages across an engagement must sum to exactly 100.
    pub payment_percent: u32,
    /// Business-logic type of this milestone — see [`MilestoneKind`].
    /// Determines the starting status and whether a time gate applies.
    pub kind: MilestoneKind,
    /// Ledger sequence number at or after which a `Retention` milestone can be unlocked.
    /// Always `0` for `Placement` milestones (no time gate).
    pub valid_after_ledger: u32,
    /// IPFS CID (or any URI) submitted by the recruiter as proof.
    /// Empty string initially; populated by `submit_proof`; cleared back to empty
    /// when a dispute is rejected or a replacement is requested.
    pub proof_hash: String,
    /// Current lifecycle position of the milestone — see [`MilestoneStatus`].
    pub status: MilestoneStatus,
    /// Ledger at which the most recent proof was submitted; 0 if never submitted.
    pub proof_submitted_at: u32,
    /// Set by `request_replacement` to the gross amount already released for this
    /// (Placement) milestone when it is reset to `Pending` for a replacement
    /// candidate; `0` if never paid out. On re-confirmation, only the difference
    /// between the milestone's current share (which may have grown via
    /// `top_up_escrow`) and this already-paid amount is released, so escrow
    /// added after a replacement is still paid out instead of getting stuck in
    /// the contract. See issue #183.
    pub replacement_paid_out: i128,
}

/// Top-level lifecycle state of an engagement.
#[contracttype]
#[derive(Clone, PartialEq, Debug)]
pub enum EngagementStatus {
    /// The engagement is active and milestones can be submitted, confirmed,
    /// disputed, or otherwise progressed through the normal workflow.
    Active,

    /// All milestones have reached either `Confirmed` or `Resolved` and the
    /// full engagement fee has been released to the recruiter.
    ///
    /// Terminal state — no further state transitions are possible.
    Completed,

    /// The engagement was mutually cancelled by the company and recruiter
    /// before completion. Any unreleased escrow has been refunded to the
    /// company.
    ///
    /// Terminal state.
    Cancelled,

    /// The company requested a replacement candidate after a previously
    /// accepted placement. The placement milestone is reset to `Pending`
    /// while the recruiter searches for a replacement. Once new placement
    /// proof is submitted, the engagement returns to `Active`.
    ReplacementRequested,

    /// The recruiter has requested an early exit from the engagement.
    /// The company must either accept the request (which cancels the
    /// engagement and refunds any unreleased escrow) or reject it,
    /// returning the engagement to `Active`.
    ExitRequested,

    /// The engagement exceeded the configured inactivity timeout and was
    /// closed through `expire_engagement`. Any unreleased escrow was
    /// refunded to the company.
    ///
    /// Terminal state.
    Expired,
}

/// A history entry for a milestone amendment
#[contracttype]
#[derive(Clone)]
pub struct AmendmentEntry {
    /// The party who proposed the amendment
    pub proposer: Address,
    /// Previous payment percentage
    pub old_payment_percent: u32,
    /// New payment percentage
    pub new_payment_percent: u32,
    /// Ledger when the amendment was accepted
    pub ledger: u32,
}

/// A pending amendment proposal for a milestone
#[contracttype]
#[derive(Clone)]
pub struct AmendmentProposal {
    /// The party who proposed the amendment
    pub proposer: Address,
    /// New payment percentage being proposed
    pub new_payment_percent: u32,
    /// Ledger when the proposal was made
    pub proposed_at_ledger: u32,
    /// Ledger at which the proposal expires if not accepted
    pub expires_at_ledger: u32,
}

/// Alias for the frontend-facing name used by `get_pending_amendment`.
pub type PendingAmendment = AmendmentProposal;

/// The full engagement record stored on-chain — note that `proof_submitted_at` on
/// each milestone is set by `submit_proof` and consumed by `force_confirm_milestone`.
#[contracttype]
#[derive(Clone)]
pub struct Engagement {
    /// Unique string identifier chosen by the caller at `create_engagement` time.
    pub id: String,
    /// The hiring company; only this address can confirm milestones, raise disputes, or cancel.
    pub company: Address,
    /// The recruitment agency or recruiter receiving milestone payments.
    pub recruiter: Address,
    /// Ordered list of arbiters; quorum of these must agree to resolve a dispute.
    pub arbiters: Vec<Address>,
    /// Number of arbiter votes required to resolve a dispute (M of N).
    pub quorum: u32,
    /// SAC address of the token held in escrow (e.g. USDC).
    pub token: Address,
    /// Total fee locked in escrow at creation, in the token's smallest unit.
    pub total_amount: i128,
    /// Cumulative amount already paid out across all confirmed/resolved milestones.
    /// `total_amount - released_amount` equals the remaining escrow balance.
    pub released_amount: i128,
    /// Free-text job title stored on-chain for display purposes.
    pub job_title: String,
    /// Optional IPFS CID linking to full job description / contract terms off-chain.
    pub metadata_hash: Option<String>,
    /// Ledger sequence number at which the engagement was created.
    pub created_at_ledger: u32,
    /// Ledger sequence number of the most recent state-changing call.
    /// Used by `expire_engagement` to detect inactivity.
    pub last_activity_ledger: u32,
    /// Ordered list of milestones; indices are stable and used throughout the contract API.
    pub milestones: Vec<Milestone>,
    /// Current lifecycle state of the engagement; drives all state-machine transitions.
    /// See [`EngagementStatus`] for the full list of variants and their semantics.
    pub status: EngagementStatus,
    /// Optional co-recruiter address for split-fee engagements (issue #56).
    /// When `Some`, the milestone payout is split between `recruiter` and `co_recruiter`
    /// according to `recruiter_split_bps`.
    pub co_recruiter: Option<Address>,
    /// Primary recruiter's share of the net payout in basis points (issue #56).
    /// Default is 10 000 (100 % to recruiter). Must be ≤ 10 000.
    pub recruiter_split_bps: u32,
    /// Optional off-chain attestation hash (e.g. SHA-256 of the contract PDF).
    /// Stored at engagement creation for audit and verification purposes.
    pub contract_pdf_hash: Option<String>,
}

/// A lightweight read-only view of an engagement, suitable for list/dashboard APIs.
///
/// For full milestone detail, use `get_engagement`.
#[contracttype]
#[derive(Clone)]
pub struct EngagementSummary {
    /// Unique engagement identifier.
    pub id: String,
    /// Free-text job title stored at creation time.
    pub job_title: String,
    /// The hiring company address.
    pub company: Address,
    /// The recruiter address receiving milestone payments.
    pub recruiter: Address,
    /// Total fee locked in escrow at creation, in the token's smallest unit.
    pub total_amount: i128,
    /// Cumulative amount paid out across all confirmed/resolved milestones so far.
    ///
    /// Remaining escrow = `total_amount - released_amount`.
    pub released_amount: i128,
    /// Current lifecycle status of the engagement.
    pub status: EngagementStatus,
    /// Total number of milestones in the engagement (does not change after creation).
    pub milestone_count: u32,
    /// Ledger sequence number at which the engagement was created.
    pub created_at_ledger: u32,
    /// Optional co-recruiter address for split-fee engagements (issue #56).
    pub co_recruiter: Option<Address>,
    /// Primary recruiter's share of the net payout in basis points (issue #56).
    pub recruiter_split_bps: u32,
    /// Optional off-chain attestation hash (e.g. SHA-256 of the contract PDF).
    pub contract_pdf_hash: Option<String>,
}

/// Per-dispute, per-milestone vote tally stored on-chain until the dispute resolves.
/// Cleared once the dispute is resolved or rejected.
#[contracttype]
#[derive(Clone)]
pub struct ArbiterVoteRecord {
    /// Number of arbiters who voted to approve payment to the recruiter.
    pub approve_votes: u32,
    /// Number of arbiters who voted to reject payment and return the milestone to `Pending`.
    pub reject_votes: u32,
    /// Addresses that have already cast a vote; prevents double-voting.
    pub voted: Vec<Address>,
}

/// Passed to `create_engagement` to configure the arbitration panel for an engagement.
///
/// Soroban's 10-parameter limit prevents passing these as individual args, so they
/// are bundled into this struct.
#[contracttype]
#[derive(Clone)]
pub struct ArbiterSetup {
    /// Ordered list of arbiter addresses eligible to vote on disputes.
    /// Must contain at least one address. All addresses must be distinct.
    pub arbiters: Vec<Address>,
    /// Number of votes required to resolve a dispute (M-of-N).
    /// Must be ≥ 1 and ≤ `arbiters.len()`.
    pub quorum: u32,
}

/// Returned by `get_arbiter_votes`.
#[contracttype]
#[derive(Clone)]
pub struct ArbiterVoteCounts {
    /// Number of arbiters who voted to approve (release) the milestone payment.
    /// Incremented in `cast_arbiter_vote` when `approve` is `true`.
    pub approve_votes: u32,
    /// Number of arbiters who voted to reject (withhold) the milestone payment.
    /// Incremented in `cast_arbiter_vote` when `approve` is `false`.
    pub reject_votes: u32,
}

/// Stored under `DataKey::PendingArbiter` during succession.
#[contracttype]
#[derive(Clone)]
pub struct ArbiterNomination {
    /// The arbiter initiating the handover. Set when `nominate_arbiter_successor`
    /// is called; used by `claim_arbiter` to locate the correct slot in
    /// `engagement.arbiters` and replace it with the incoming nominee.
    pub current: Address,
    /// The address authorised to complete the succession by calling
    /// `claim_arbiter`. Only this exact address may claim the slot; any other
    /// caller is rejected with `"unauthorized"`.
    pub nominee: Address,
}

/// Pending contract WASM upgrade proposal stored until execution (issue #69).
#[contracttype]
#[derive(Clone)]
pub struct UpgradeProposal {
    /// The new WASM hash to apply on execute_upgrade.
    pub new_wasm_hash: BytesN<32>,
    /// Ledger sequence at or after which execute_upgrade may be called.
    pub execute_after_ledger: u32,
}

/// Platform fee configuration deducted from each milestone payment.
#[contracttype]
#[derive(Clone)]
pub struct PlatformFee {
    /// Fee in basis points (1 bp = 0.01%), capped at 500 (5%).
    pub bps: u32,
    /// Address that receives accumulated platform fees.
    pub treasury: Address,
}

/// Bundled optional configuration passed as the last argument of `create_engagement`.
/// Combines `metadata_hash` with the new co-recruiter split fields (issue #56)
/// to stay within Soroban's 10-parameter limit.
#[contracttype]
#[derive(Clone)]
pub struct EngagementConfig {
    /// Optional IPFS CID linking to full job description / contract terms off-chain.
    pub metadata_hash: Option<String>,
    /// Optional co-recruiter address that shares the milestone payout.
    pub co_recruiter: Option<Address>,
    /// Primary recruiter's share in basis points (10 000 = 100 %).
    /// If `co_recruiter` is `None` this field is ignored and the full payout goes to `recruiter`.
    /// Must be ≤ 10 000.
    pub recruiter_split_bps: u32,
    /// Optional off-chain attestation hash (e.g. SHA-256 of the contract PDF).
    /// Must be non-empty if provided.
    pub contract_pdf_hash: Option<String>,
}

// ============================================================
// STORAGE KEYS
// ============================================================

/// Contract storage key space. Instance keys reset between transactions;
/// persistent keys survive across ledgers.
#[contracttype]
pub enum DataKey {
    /// Full engagement record stored by engagement_id (persistent).
    Engagement(String),
    /// Current admin address (instance).
    Admin,
    /// Pending arbiter succession nomination for an engagement.
    PendingArbiter(String),
    /// Platform fee configuration — basis points and treasury address (persistent).
    PlatformFee,
    /// Whether the contract is currently paused (persistent).
    Paused,
    /// Pending admin transfer nomination address (persistent).
    PendingAdmin,
    /// Admin-configurable proof resubmission cooldown in ledgers (default 2 880).
    ProofCooldown,
    /// Ledger at which the last proof was submitted for (engagement_id, milestone_index).
    LastProofAt(String, u32),
    /// Running vote tally for a disputed (engagement_id, milestone_index).
    ArbiterVotes(String, u32),
    /// Active amendment proposal for an engagement milestone (persistent).
    AmendmentProposal(String, u32),
    /// Amendment log entries for an engagement milestone (persistent).
    AmendmentLog(String, u32),
    /// Admin-configurable TTL extension for amendment proposals (persistent).
    AmendmentTTL,
    /// Total number of engagements ever created (issue #34).
    EngagementCount,
    /// Per-company ordered list of engagement IDs (issue #35).
    CompanyEngagements(Address),
    /// Per-recruiter ordered list of engagement IDs (issue #36).
    RecruiterEngagements(Address),
    /// Allowlist of accepted token SAC addresses (issue #26).
    AllowedTokens,
    /// Whether the token allowlist is enabled (issue #26).
    AllowlistEnabled,
    /// Admin-configurable ledgers-per-day constant (issue #41).
    LedgersPerDay,
    /// Dispute reason string stored per (engagement_id, milestone_index) (issue #50).
    DisputeReason(String, u32),
    /// Structured reason code stored per (engagement_id, replacement_index)
    /// when a company calls `request_replacement` (issue #51). Indexed from 0.
    ReplacementReason(String, u32),
    /// Number of replacements ever requested for an engagement (issue #51).
    /// Acts as the next replacement_index when incremented.
    ReplacementCount(String),
    /// Admin-configurable max retention days cap (issue #18).
    MaxRetentionDays,
    /// Admin-configurable inactivity timeout in ledgers (issue #38).
    InactivityTimeoutLedgers,
    /// Admin-configurable storage TTL extension in ledgers (issue #40).
    StorageTtlExtendTo,
    /// Admin-configurable confirm window in ledgers (default 86_400 — ~5 days).
    ConfirmWindow,
    /// Admin-configurable dispute window in ledgers (default 51_840 — ~3 days).
    DisputeWindow,
    /// Contract version string (e.g. "0.2.0") for deployment verification (issue #16).
    Version,
    /// Minimum engagement amount in stroops to prevent dust engagements (issue #17).
    MinEngagementAmount,
    /// Pending contract WASM upgrade proposal (issue #69).
    PendingUpgrade,
    /// Admin-configurable upgrade time-lock duration in ledgers (issue #69, default 17_280).
    UpgradeLockDuration,
    /// Admin-configurable max proof hash length in characters (issue #68, default 200).
    MaxProofHashLength,
    /// Admin-configurable max milestone count (issue #21, default 10).
    MaxMilestones,
    /// Arbiter fee in basis points (0–200, max 2%) deducted from payout on dispute approval (issue #52).
    ArbiterFee,
    /// Set to true once admin has permanently renounced their role (issue #59).
    AdminRenounced,
    /// Admin-configurable maximum simultaneous active engagements per company (default 50).
    MaxActivePerCompany,
    /// Per-company count of currently active (non-terminal) engagements.
    CompanyActiveCount(Address),
    /// Admin-configurable maximum number of replacements allowed per engagement (issue #31, default 3).
    MaxReplacements,
}

// ============================================================
// CONTRACT
// ============================================================

#[contract]
pub struct HireSettleContract;

const LEDGERS_PER_DAY: u32 = 17_280; // 86 400s ÷ 5s per ledger
const DEFAULT_PROOF_COOLDOWN: u32 = 2_880; // ~4 hours
const DEFAULT_MAX_RETENTION_DAYS: u32 = 365;
const DEFAULT_MAX_MILESTONES: u32 = 10;
const DEFAULT_INACTIVITY_TIMEOUT_LEDGERS: u32 = 1_036_800; // ~60 days
const DEFAULT_STORAGE_TTL_EXTEND_TO: u32 = 1_036_800; // ~60 days
const DEFAULT_VERSION: &str = "0.2.0";
/// Maximum length (in characters) of a `request_replacement` reason string
/// (issue #51). Mirrors the dispute-reason cap to keep storage bounded.
const MAX_REPLACEMENT_REASON_LEN: u32 = 128;
const DEFAULT_MIN_ENGAGEMENT_AMOUNT: i128 = 100_000; // 0.01 USDC
const DEFAULT_CONFIRM_WINDOW_LEDGERS: u32 = 86_400; // ~5 days
const DEFAULT_DISPUTE_WINDOW_LEDGERS: u32 = 51_840; // ~3 days
const MAX_VERSION_LENGTH: u32 = 32;
const MAX_PROOF_HASH_LENGTH: u32 = 200;
const MAX_ENGAGEMENT_ID_LENGTH: u32 = 64;
const DEFAULT_MAX_ACTIVE_PER_COMPANY: u32 = 50;
/// Default maximum number of replacements allowed per engagement (issue #31).
const DEFAULT_MAX_REPLACEMENTS: u32 = 3;

/// Shared panic message constants for the most-repeated error strings
/// (issue #171). Keeping these as constants means a typo can't silently
/// create an inconsistent error surface for off-chain consumers matching
/// on these strings.
const ERR_UNAUTHORIZED: &str = "unauthorized";
const ERR_ENGAGEMENT_NOT_ACTIVE: &str = "engagement is not active";
const ERR_INVALID_MILESTONE_INDEX: &str = "invalid milestone index";

#[contractimpl]
impl HireSettleContract {
    // ----------------------------------------------------------
    // INIT
    // ----------------------------------------------------------

    /// Initializes the HireSettle contract.
    ///
    /// # Caller
    /// Called by the contract deployer or initial administrator (`admin`). Requires authentication from `admin`.
    ///
    /// # Initialized State
    /// Sets up default contract storage values:
    /// - `DataKey::Admin`: Set to `admin`
    /// - `DataKey::Paused`: Set to `false`
    /// - `DataKey::PlatformFee`: Set to 0 bps with treasury `admin`
    /// - `DataKey::Version`: Set to `DEFAULT_VERSION` ("0.2.0")
    /// - `DataKey::MinEngagementAmount`: Set to `DEFAULT_MIN_ENGAGEMENT_AMOUNT` (100,000 stroops)
    ///
    /// # One-Time-Only / Calling Twice
    /// Note: No already-initialized guard is currently present. If invoked again, it will overwrite
    /// all initialized storage fields provided `admin.require_auth()` succeeds.
    ///
    /// # Panics
    /// Panics if authentication from `admin` (`admin.require_auth()`) fails.
    pub fn init(env: Env, admin: Address) {
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().persistent().set(&DataKey::Paused, &false);
        env.storage().persistent().set(
            &DataKey::PlatformFee,
            &PlatformFee {
                bps: 0,
                treasury: admin,
            },
        );

        // Issue #16: Initialize contract version
        env.storage()
            .persistent()
            .set(&DataKey::Version, &String::from_str(&env, DEFAULT_VERSION));

        // Issue #17: Initialize minimum engagement amount
        env.storage().persistent().set(
            &DataKey::MinEngagementAmount,
            &DEFAULT_MIN_ENGAGEMENT_AMOUNT,
        );
    }

    // ----------------------------------------------------------
    // ADMIN CONFIGURATION
    // ----------------------------------------------------------

    /// Set the platform fee in basis points and the treasury that receives it.
    /// `bps` is capped at 500 (5%).
    pub fn set_platform_fee(env: Env, admin: Address, bps: u32, treasury: Address) {
        Self::assert_not_paused(&env);
        Self::assert_admin(&env, &admin);

        if bps > MAX_PLATFORM_FEE_BPS {
            panic!("FeeTooHigh");
        }

        env.storage().persistent().set(
            &DataKey::PlatformFee,
            &PlatformFee {
                bps,
                treasury: treasury.clone(),
            },
        );

        env.events()
            .publish((Symbol::new(&env, "platform_fee_set"),), (bps, treasury));
    }

    /// Return the current platform fee configuration.
    pub fn get_platform_fee(env: Env) -> (u32, Address) {
        let fee = Self::get_platform_fee_internal(&env);
        (fee.bps, fee.treasury)
    }

    /// Admin sets the contract version string (issue #16).
    /// `version` must be ≤ 32 characters.
    /// Panics with "VersionTooLong" if version exceeds 32 chars.
    /// Panics with "unauthorized" if caller is not admin.
    pub fn set_version(env: Env, admin: Address, version: String) {
        Self::assert_admin(&env, &admin);

        if version.len() > MAX_VERSION_LENGTH {
            panic!("VersionTooLong");
        }

        env.storage().persistent().set(&DataKey::Version, &version);
        env.events()
            .publish((Symbol::new(&env, "version_set"),), version);
    }

    /// Admin sets the minimum engagement amount, in the raw smallest unit of
    /// whichever token a given `create_engagement` call uses (issue #17). This
    /// single global floor applies to every allowlisted token regardless of its
    /// `decimals()` — the contract is token-decimals-agnostic (see issue #175).
    /// If the allowlist mixes tokens of very different precision (e.g. a
    /// 7-decimal token alongside an 18-decimal token), the admin is responsible
    /// for picking a value that is a sane floor for all of them, or for keeping
    /// the allowlist restricted to tokens of comparable precision.
    /// Panics with "unauthorized" if caller is not admin.
    pub fn set_min_amount(env: Env, admin: Address, amount: i128) {
        Self::assert_admin(&env, &admin);

        env.storage()
            .persistent()
            .set(&DataKey::MinEngagementAmount, &amount);
        env.events()
            .publish((Symbol::new(&env, "min_amount_set"),), amount);
    }

    /// Pause state-changing contract operations.
    pub fn pause(env: Env, admin: Address) {
        Self::assert_admin(&env, &admin);
        env.storage().persistent().set(&DataKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"),), admin);
    }

    /// Resume state-changing contract operations.
    pub fn unpause(env: Env, admin: Address) {
        Self::assert_admin(&env, &admin);
        env.storage().persistent().set(&DataKey::Paused, &false);
        env.events()
            .publish((Symbol::new(&env, "unpaused"),), admin);
    }

    /// Return true if the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        Self::is_paused_internal(&env)
    }

    /// Nominate a new admin. The nominee must call `claim_admin` to complete rotation.
    pub fn nominate_admin(env: Env, current_admin: Address, new_admin: Address) {
        Self::assert_not_paused(&env);
        Self::assert_admin(&env, &current_admin);

        env.storage()
            .persistent()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((Symbol::new(&env, "admin_nominated"),), new_admin);
    }

    /// Claim admin rights after being nominated by the current admin.
    pub fn claim_admin(env: Env, nominee: Address) {
        Self::assert_not_paused(&env);
        nominee.require_auth();

        let pending: Address = env
            .storage()
            .persistent()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic!("no pending admin nomination"));

        if nominee != pending {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        env.storage().instance().set(&DataKey::Admin, &nominee);
        env.storage().persistent().remove(&DataKey::PendingAdmin);
        env.events()
            .publish((Symbol::new(&env, "admin_claimed"),), nominee);
    }

    /// Return the pending admin nominee, if one exists.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage().persistent().get(&DataKey::PendingAdmin)
    }

    /// Return the current contract admin.
    pub fn get_admin(env: Env) -> Address {
        Self::get_admin_internal(&env)
    }

    // ----------------------------------------------------------
    // ADMIN CONFIG
    // ----------------------------------------------------------

    /// Set the minimum ledger gap between successive proof submissions on the
    /// same milestone. Only callable by the admin set during `init`.
    pub fn set_proof_cooldown(env: Env, admin: Address, ledgers: u32) {
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic!("contract not initialised"));
        if admin != stored_admin {
            panic!("{}", ERR_UNAUTHORIZED);
        }
        env.storage()
            .instance()
            .set(&DataKey::ProofCooldown, &ledgers);
    }

    fn get_proof_cooldown(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ProofCooldown)
            .unwrap_or(DEFAULT_PROOF_COOLDOWN)
    }

    // ----------------------------------------------------------
    // CREATE ENGAGEMENT
    // ----------------------------------------------------------

    /// Create a new recruitment engagement and lock funds in escrow.
    ///
    /// # Arguments
    /// - `engagement_id`   — unique string ID for this engagement
    /// - `company`         — company address (must sign this tx)
    /// - `recruiter`       — recruiter address (receives payments)
    /// - `arbiters`        — ordered list of arbiter addresses (min 1)
    /// - `quorum`          — number of arbiter approvals required to release on dispute (M of N)
    /// - `token`           — SAC address of the escrow token (USDC or any allowlisted token,
    ///   see [`Self::add_allowed_token`]). `total_amount` and all amount-based math below are
    ///   raw integer units in this token's smallest denomination (e.g. stroops for a 7-decimal
    ///   token) — the contract does not read or adjust for the token's `decimals()`. Callers
    ///   integrating non-USDC-like tokens are responsible for choosing a `total_amount` (and,
    ///   for admins, a [`Self::set_min_amount`]) that makes sense for that token's precision.
    /// - `total_amount`    — total recruiter fee in the token's smallest unit
    /// - `job_title`       — short job title string
    /// - `milestones`      — ordered milestone list
    /// - `retention_days`  — Vec of retention windows in days (one per Retention milestone)
    /// - `config`          — bundled optional config: metadata_hash, co_recruiter, recruiter_split_bps
    ///
    /// # Panics
    /// - `"CompanyRecruiterCollision"` — `company` and `recruiter` are the same address.
    /// - `"CompanyArbiterCollision"` — `company` also appears in the arbiter set.
    /// - `"RecruiterArbiterCollision"` — `recruiter` also appears in the arbiter set.
    ///
    /// These checks exist so a company cannot name itself (or a colluding address)
    /// as arbiter and vote on its own disputes, or name itself as recruiter to
    /// self-confirm milestones. See issue #174.
    pub fn create_engagement(
        env: Env,
        engagement_id: String,
        company: Address,
        recruiter: Address,
        arbiter_setup: ArbiterSetup,
        token: Address,
        total_amount: i128,
        job_title: String,
        milestones: Vec<Milestone>,
        retention_days: Vec<u32>,
        config: EngagementConfig,
    ) -> String {
        Self::assert_not_paused(&env);
        company.require_auth();

        // Validate engagement_id format: non-empty, ≤ 64 chars, [A-Za-z0-9-] only.
        if engagement_id.len() == 0 || engagement_id.len() > MAX_ENGAGEMENT_ID_LENGTH {
            panic!("InvalidEngagementId");
        }
        let id_len = engagement_id.len() as usize;
        let mut id_buf = [0u8; MAX_ENGAGEMENT_ID_LENGTH as usize];
        engagement_id.copy_into_slice(&mut id_buf[..id_len]);
        for i in 0..id_len {
            let b = id_buf[i];
            let valid = (b >= b'A' && b <= b'Z')
                || (b >= b'a' && b <= b'z')
                || (b >= b'0' && b <= b'9')
                || b == b'-';
            if !valid {
                panic!("InvalidEngagementId");
            }
        }

        // Issue #24: Job title validation
        if job_title.len() == 0 {
            panic!("JobTitleEmpty");
        }
        if job_title.len() > 64 {
            panic!("JobTitleTooLong");
        }

        // Issue #21: Max milestone count cap validation
        if milestones.len() == 0 {
            panic!("ZeroMilestones");
        }
        let max_milestones = Self::get_max_milestones(env.clone());
        if milestones.len() > max_milestones {
            panic!("TooManyMilestones");
        }

        // Issue #22: Milestone name max length validation
        for i in 0..milestones.len() {
            let m = milestones.get(i).unwrap();
            if m.name.len() == 0 {
                panic!("MilestoneNameEmpty: index {}", i);
            }
            if m.name.len() > 64 {
                panic!("MilestoneNameTooLong: index {}", i);
            }
        }

        // Issue #23: Milestone name uniqueness validation
        for i in 0..milestones.len() {
            let m_i = milestones.get(i).unwrap();
            for j in (i + 1)..milestones.len() {
                let m_j = milestones.get(j).unwrap();
                if m_i.name == m_j.name {
                    let mut name_buf = [0u8; 64];
                    let name_len = m_i.name.len() as usize;
                    m_i.name.copy_into_slice(&mut name_buf[..name_len]);
                    let name_str = core::str::from_utf8(&name_buf[..name_len]).unwrap_or("");
                    panic!("DuplicateMilestoneName: {}", name_str);
                }
            }
        }

        if total_amount <= 0 {
            panic!("amount must be greater than zero");
        }

        // Issue #17: Minimum amount validation
        let min_amount = Self::get_min_amount(env.clone());
        if total_amount < min_amount {
            panic!("AmountBelowMinimum");
        }

        // Issue #26: reject token if allowlist is active and token not in it.
        let allowlist_enabled: bool = env
            .storage()
            .persistent()
            .get(&DataKey::AllowlistEnabled)
            .unwrap_or(false);
        if allowlist_enabled {
            let allowed: Vec<Address> = env
                .storage()
                .persistent()
                .get(&DataKey::AllowedTokens)
                .unwrap_or_else(|| Vec::new(&env));
            let is_allowed = (0..allowed.len()).any(|i| allowed.get(i).unwrap() == token);
            if !is_allowed {
                panic!("TokenNotAllowed");
            }
        }

        let arbiters = arbiter_setup.arbiters;
        let quorum = arbiter_setup.quorum;

        if arbiters.is_empty() {
            panic!("at least one arbiter required");
        }

        if quorum == 0 || quorum > arbiters.len() {
            panic!("invalid quorum");
        }

        // Issue #174: reject overlapping company/recruiter/arbiter addresses so a
        // company cannot name itself (or a colluding address) as arbiter and vote
        // on its own disputes, or name itself as recruiter to self-confirm milestones.
        if company == recruiter {
            panic!("CompanyRecruiterCollision");
        }
        for i in 0..arbiters.len() {
            let a = arbiters.get(i).unwrap();
            if a == company {
                panic!("CompanyArbiterCollision");
            }
            if a == recruiter {
                panic!("RecruiterArbiterCollision");
            }
        }

        // Reject empty metadata hash — caller must either omit or provide a real CID.
        if let Some(ref hash) = config.metadata_hash {
            if hash.len() == 0 {
                panic!("InvalidMetadataHash");
            }
        }

        // Reject empty contract_pdf_hash — caller must either omit or provide a real hash.
        if let Some(ref hash) = config.contract_pdf_hash {
            if hash.len() == 0 {
                panic!("InvalidContractPdfHash");
            }
        }

        // Issue #56: validate co-recruiter split basis points.
        if config.recruiter_split_bps > FULL_SPLIT_BPS {
            panic!("InvalidSplitBps");
        }

        let mut total_percent: u32 = 0;
        for i in 0..milestones.len() {
            total_percent += milestones.get(i).unwrap().payment_percent;
        }
        if total_percent != 100 {
            panic!("milestone percentages must sum to 100");
        }

        if env
            .storage()
            .persistent()
            .has(&DataKey::Engagement(engagement_id.clone()))
        {
            panic!("engagement already exists");
        }

        // Cap check: reject if the company is already at or over the active engagement limit.
        let active_count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::CompanyActiveCount(company.clone()))
            .unwrap_or(0u32);
        let max_active: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxActivePerCompany)
            .unwrap_or(DEFAULT_MAX_ACTIVE_PER_COMPANY);
        if active_count >= max_active {
            panic!("CompanyActiveLimitReached");
        }

        let current_ledger = env.ledger().sequence();
        let lpd = Self::get_ledgers_per_day_internal(&env);
        let max_retention_days = Self::get_max_retention_days(env.clone());
        let mut retention_index: u32 = 0;
        let mut resolved_milestones: Vec<Milestone> = Vec::new(&env);

        for i in 0..milestones.len() {
            let mut m = milestones.get(i).unwrap();
            match m.kind {
                MilestoneKind::Placement => {
                    m.valid_after_ledger = 0;
                    m.status = MilestoneStatus::Pending;
                }
                MilestoneKind::Retention => {
                    let days = retention_days.get(retention_index).unwrap_or(30);
                    retention_index += 1;

                    // Issue #19: Zero retention days validation
                    if days == 0 {
                        panic!("RetentionDaysZero");
                    }

                    if days > max_retention_days {
                        panic!("RetentionDaysTooLarge");
                    }
                    m.valid_after_ledger = current_ledger + (days * lpd);
                    m.status = MilestoneStatus::Locked;
                }
            }
            resolved_milestones.push_back(m);
        }

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&company, &env.current_contract_address(), &total_amount);

        let engagement = Engagement {
            id: engagement_id.clone(),
            company: company.clone(),
            recruiter,
            arbiters,
            quorum,
            token,
            total_amount,
            released_amount: 0,
            job_title,
            metadata_hash: config.metadata_hash,
            created_at_ledger: current_ledger,
            last_activity_ledger: current_ledger,
            milestones: resolved_milestones,
            status: EngagementStatus::Active,
            co_recruiter: config.co_recruiter,
            recruiter_split_bps: config.recruiter_split_bps,
            contract_pdf_hash: config.contract_pdf_hash,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);

        Self::extend_engagement_ttl(&env, &engagement_id);

        // Increment per-company active engagement count.
        let new_active = active_count + 1;
        env.storage()
            .persistent()
            .set(&DataKey::CompanyActiveCount(company.clone()), &new_active);
        env.storage().persistent().extend_ttl(
            &DataKey::CompanyActiveCount(company.clone()),
            100_000,
            6_300_000,
        );

        // Issue #34: increment global engagement counter.
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EngagementCount)
            .unwrap_or(0u64);
        env.storage()
            .instance()
            .set(&DataKey::EngagementCount, &(count + 1));

        // Issue #35: append engagement_id to the per-company index.
        let mut company_ids: Vec<String> = env
            .storage()
            .persistent()
            .get(&DataKey::CompanyEngagements(company.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        company_ids.push_back(engagement_id.clone());
        env.storage()
            .persistent()
            .set(&DataKey::CompanyEngagements(company.clone()), &company_ids);
        env.storage().persistent().extend_ttl(
            &DataKey::CompanyEngagements(company.clone()),
            100_000,
            6_300_000,
        );

        // Issue #36: append engagement_id to the per-recruiter index.
        let mut recruiter_ids: Vec<String> = env
            .storage()
            .persistent()
            .get(&DataKey::RecruiterEngagements(recruiter.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        recruiter_ids.push_back(engagement_id.clone());
        env.storage().persistent().set(
            &DataKey::RecruiterEngagements(recruiter.clone()),
            &recruiter_ids,
        );
        env.storage().persistent().extend_ttl(
            &DataKey::RecruiterEngagements(recruiter.clone()),
            100_000,
            6_300_000,
        );

        env.events().publish(
            (
                Symbol::new(&env, "engagement_created"),
                engagement_id.clone(),
            ),
            engagement_id.clone(),
        );

        engagement_id
    }

    // ----------------------------------------------------------
    // UNLOCK RETENTION MILESTONE
    // ----------------------------------------------------------

    pub fn unlock_milestone(env: Env, engagement_id: String, milestone_index: u32) {
        Self::assert_not_paused(&env);
        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        let mut milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        if milestone.status != MilestoneStatus::Locked {
            panic!("milestone is not locked");
        }

        if milestone.kind != MilestoneKind::Retention {
            panic!("only retention milestones can be unlocked this way");
        }

        let current_ledger = env.ledger().sequence();
        if current_ledger < milestone.valid_after_ledger {
            panic!("retention window has not elapsed yet");
        }

        // Capture for the event before mutating the milestone.
        let valid_after_ledger = milestone.valid_after_ledger;
        let unlocked_at_ledger = current_ledger;

        let old_status = milestone.status.clone();
        milestone.status = MilestoneStatus::Pending;
        engagement.milestones.set(milestone_index, milestone);
        engagement.last_activity_ledger = unlocked_at_ledger;

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_milestone_status_changed(
            &env,
            &engagement_id,
            milestone_index,
            old_status,
            MilestoneStatus::Pending,
        );

        // Event body carries the time-gate evidence so off-chain consumers can
        // confirm the unlock was legitimate without a follow-up `get_milestone`
        // query. See issue #54.
        env.events().publish(
            (
                Symbol::new(&env, "milestone_unlocked"),
                engagement_id.clone(),
            ),
            (milestone_index, valid_after_ledger, unlocked_at_ledger),
        );
    }

    // ----------------------------------------------------------
    // SUBMIT PROOF
    // ----------------------------------------------------------

    /// Submit an IPFS CID (or any URI) as proof that a milestone deliverable has been met.
    ///
    /// # Caller
    /// `recruiter` — must match the engagement's recruiter address and sign the transaction.
    ///
    /// # Behaviour
    /// - The engagement must be `Active` or `ReplacementRequested`.
    /// - The target milestone must be in `Pending` status (i.e. already unlocked).
    /// - If a proof was previously submitted and rejected, the caller must wait
    ///   `proof_cooldown` ledgers (default 2 880 ≈ 4 hours) before resubmitting.
    /// - After successful submission the milestone moves to `ProofSubmitted`.
    /// - If the engagement was `ReplacementRequested` and this is the placement milestone,
    ///   the engagement reverts to `Active`.
    ///
    /// # Panics
    /// - `"engagement not active"` — engagement is not `Active` or `ReplacementRequested`.
    /// - `"milestone not pending"` — milestone is not in `Pending` status.
    /// - `"proof cooldown active"` — resubmitting too soon after a previous proof.
    /// - `"proof hash cannot be empty"` — empty string passed as `proof_hash`.
    /// - `"DuplicateProofHash"` — the proof hash is already used by another milestone
    ///   in this engagement.
    ///
    /// # Events
    /// Emits `("proof_submitted", engagement_id)` with `(milestone_index, proof_hash)`.
    pub fn submit_proof(
        env: Env,
        recruiter: Address,
        engagement_id: String,
        milestone_index: u32,
        proof_hash: String,
    ) {
        Self::assert_not_paused(&env);

        // Issue #20: Proof hash format validation (before require_auth for fail-fast)
        if proof_hash.len() == 0 {
            panic!("InvalidProofHash");
        }

        if proof_hash.len() > Self::get_max_proof_hash_length_internal(&env) {
            panic!("ProofHashTooLong");
        }

        recruiter.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active
            && engagement.status != EngagementStatus::ReplacementRequested
        {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if recruiter != engagement.recruiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let mut milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        if milestone.status != MilestoneStatus::Pending {
            panic!("milestone is not pending");
        }

        // Rate-limit resubmissions — first submission (no stored ledger) is always allowed.
        let last_key = DataKey::LastProofAt(engagement_id.clone(), milestone_index);
        let current_ledger = env.ledger().sequence();
        if let Some(last_at) = env.storage().persistent().get::<DataKey, u32>(&last_key) {
            let cooldown = Self::get_proof_cooldown(&env);
            if current_ledger < last_at + cooldown {
                panic!("ResubmitTooSoon");
            }
        }

        // A proof hash identifies the evidence for a milestone and must not be
        // reused by another milestone in the same engagement. Exclude the
        // target milestone so a valid resubmission can replace its own proof.
        for i in 0..engagement.milestones.len() {
            if i != milestone_index {
                let existing_milestone = engagement.milestones.get(i).unwrap();
                if existing_milestone.proof_hash.len() > 0
                    && existing_milestone.proof_hash == proof_hash
                {
                    panic!("DuplicateProofHash");
                }
            }
        }

        // Record this submission ledger for future cooldown checks.
        env.storage().persistent().set(&last_key, &current_ledger);
        env.storage()
            .persistent()
            .extend_ttl(&last_key, 100_000, 6_300_000);

        let is_resubmission = milestone.proof_hash.len() > 0;
        let old_hash = milestone.proof_hash.clone();

        milestone.proof_hash = proof_hash.clone();
        let old_status = milestone.status.clone();
        milestone.status = MilestoneStatus::ProofSubmitted;
        milestone.proof_submitted_at = current_ledger;
        engagement.milestones.set(milestone_index, milestone);

        let old_engagement_status = engagement.status.clone();
        if engagement.status == EngagementStatus::ReplacementRequested {
            engagement.status = EngagementStatus::Active;
        }
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_milestone_status_changed(
            &env,
            &engagement_id,
            milestone_index,
            old_status,
            MilestoneStatus::ProofSubmitted,
        );
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        if is_resubmission {
            env.events().publish(
                (
                    Symbol::new(&env, "proof_resubmitted"),
                    engagement_id.clone(),
                ),
                (milestone_index, old_hash, proof_hash),
            );
        } else {
            env.events().publish(
                (Symbol::new(&env, "proof_submitted"), engagement_id.clone()),
                milestone_index,
            );
        }
    }

    // ----------------------------------------------------------
    // CONFIRM MILESTONE
    // ----------------------------------------------------------

    pub fn confirm_milestone(
        env: Env,
        company: Address,
        engagement_id: String,
        milestone_index: u32,
    ) {
        Self::assert_not_paused(&env);
        company.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if company != engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let mut milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        if milestone.status != MilestoneStatus::ProofSubmitted {
            panic!("milestone proof not yet submitted");
        }

        // Issue #67: enforce sequential confirmation — all prior milestones must be done.
        for i in 0..milestone_index {
            let prev = engagement.milestones.get(i).unwrap();
            if prev.status != MilestoneStatus::Confirmed && prev.status != MilestoneStatus::Resolved
            {
                panic!("PreviousMilestoneNotComplete");
            }
        }

        if milestone.kind == MilestoneKind::Retention {
            let current_ledger = env.ledger().sequence();
            if current_ledger < milestone.valid_after_ledger {
                panic!("retention window has not elapsed — cannot confirm yet");
            }
        }

        // Issue #183: if this milestone was already paid out before a replacement
        // reset it to Pending, only release the difference between its current
        // share (which may have grown via top_up_escrow) and what was already
        // paid — this ensures escrow added after a replacement still reaches
        // the recruiter instead of getting stuck in the contract.
        let full_share = (engagement.total_amount * milestone.payment_percent as i128) / 100;
        let payment = full_share - milestone.replacement_paid_out;
        if payment > 0 {
            let platform_fee = Self::get_platform_fee_internal(&env);
            let fee_amount = (payment * platform_fee.bps as i128) / 10_000;
            let net_payment = payment - fee_amount;
            engagement.released_amount += payment;

            let token_client = token::Client::new(&env, &engagement.token);
            if fee_amount > 0 {
                token_client.transfer(
                    &env.current_contract_address(),
                    &platform_fee.treasury,
                    &fee_amount,
                );
                env.events().publish(
                    (
                        Symbol::new(&env, "platform_fee_collected"),
                        engagement_id.clone(),
                    ),
                    (milestone_index, fee_amount, platform_fee.treasury),
                );
            }
            Self::distribute_recruiter_payout(&env, &engagement, net_payment, &token_client);
        }

        let old_status = milestone.status.clone();
        milestone.status = MilestoneStatus::Confirmed;
        engagement
            .milestones
            .set(milestone_index, milestone.clone());

        let all_done = (0..engagement.milestones.len()).all(|i| {
            let s = engagement.milestones.get(i).unwrap().status;
            s == MilestoneStatus::Confirmed || s == MilestoneStatus::Resolved
        });

        let old_engagement_status = engagement.status.clone();
        if all_done {
            engagement.status = EngagementStatus::Completed;
            Self::decrement_company_active_count(&env, &engagement.company);
        }
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);

        Self::emit_milestone_status_changed(
            &env,
            &engagement_id,
            milestone_index,
            old_status,
            MilestoneStatus::Confirmed,
        );
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        env.events().publish(
            (
                Symbol::new(&env, "milestone_confirmed"),
                engagement_id.clone(),
            ),
            (milestone_index, payment),
        );

        if all_done {
            env.events().publish(
                (
                    Symbol::new(&env, "engagement_completed"),
                    engagement_id.clone(),
                ),
                (
                    engagement_id.clone(),
                    engagement.released_amount,
                    env.ledger().sequence(),
                ),
            );
        }
    }

    // ----------------------------------------------------------
    // RAISE DISPUTE
    // ----------------------------------------------------------

    /// Raise a dispute on a milestone whose proof has been submitted by the
    /// recruiter, moving it into `Disputed` status so that the contract's
    /// arbiters can vote on the outcome.
    ///
    /// # Caller
    /// `company` — must match the engagement's company address and sign
    /// the transaction.
    ///
    /// # Behaviour
    /// - The engagement must be `Active`.
    /// - The target milestone must be in `ProofSubmitted` status.
    /// - The dispute must be raised within the dispute window
    ///   (default 51 840 ledgers ≈ 3 days) counted from
    ///   `proof_submitted_at`.
    /// - The milestone transitions to `Disputed` and the supplied `reason`
    ///   (max 128 bytes) is stored for arbiter review.
    /// - After this call the arbiter-vote flow can begin:
    ///   see [`Self::cast_arbiter_vote`].
    ///
    /// # Panics
    /// - `"ReasonTooLong"` — `reason` exceeds 128 bytes.
    /// - `"engagement is not active"` — engagement is not `Active`.
    /// - `"unauthorized"` — caller is not the engagement's company.
    /// - `"can only dispute a submitted proof"` — milestone is not in
    ///   `ProofSubmitted` status.
    /// - `"DisputeWindowClosed"` — the dispute window has elapsed.
    ///
    /// # Events
    /// Emits `("dispute_raised", engagement_id)` with
    /// `(milestone_index, reason)`.
    pub fn raise_dispute(
        env: Env,
        company: Address,
        engagement_id: String,
        milestone_index: u32,
        reason: String,
    ) {
        company.require_auth();

        if reason.len() > 128 {
            panic!("ReasonTooLong");
        }

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if company != engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let mut milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        if milestone.status != MilestoneStatus::ProofSubmitted {
            panic!("can only dispute a submitted proof");
        }

        let current_ledger = env.ledger().sequence();
        let dispute_window = env
            .storage()
            .instance()
            .get(&DataKey::DisputeWindow)
            .unwrap_or(DEFAULT_DISPUTE_WINDOW_LEDGERS);

        if current_ledger > milestone.proof_submitted_at + dispute_window {
            panic!("DisputeWindowClosed");
        }

        let old_status = milestone.status.clone();
        milestone.status = MilestoneStatus::Disputed;
        engagement.milestones.set(milestone_index, milestone);
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage().persistent().set(
            &DataKey::DisputeReason(engagement_id.clone(), milestone_index),
            &reason.clone(),
        );

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);

        Self::emit_milestone_status_changed(
            &env,
            &engagement_id,
            milestone_index,
            old_status,
            MilestoneStatus::Disputed,
        );

        env.events().publish(
            (Symbol::new(&env, "dispute_raised"), engagement_id.clone()),
            (milestone_index, reason),
        );
    }

    // ----------------------------------------------------------
    // CAST ARBITER VOTE  (#10 multi-arbiter quorum)
    // ----------------------------------------------------------

    /// Each arbiter calls this to cast their vote on a Disputed milestone.
    /// The dispute resolves automatically once either:
    ///   - `approve_votes >= quorum`  → payment released, milestone → Resolved
    ///   - `reject_votes > arbiters.len() - quorum`  → proof cleared, milestone → Pending
    ///
    /// Duplicate votes from the same arbiter are rejected.
    pub fn cast_arbiter_vote(
        env: Env,
        arbiter: Address,
        engagement_id: String,
        milestone_index: u32,
        approve: bool,
    ) {
        Self::assert_not_paused(&env);
        arbiter.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        let is_arbiter =
            (0..engagement.arbiters.len()).any(|i| engagement.arbiters.get(i).unwrap() == arbiter);
        if !is_arbiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let mut milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        if milestone.status != MilestoneStatus::Disputed {
            panic!("milestone is not in disputed status");
        }

        let vote_key = DataKey::ArbiterVotes(engagement_id.clone(), milestone_index);
        let mut record: ArbiterVoteRecord =
            env.storage()
                .persistent()
                .get(&vote_key)
                .unwrap_or(ArbiterVoteRecord {
                    approve_votes: 0,
                    reject_votes: 0,
                    voted: Vec::new(&env),
                });

        // Reject duplicate votes.
        for i in 0..record.voted.len() {
            if record.voted.get(i).unwrap() == arbiter {
                panic!("duplicate vote");
            }
        }

        record.voted.push_back(arbiter.clone());
        if approve {
            record.approve_votes += 1;
        } else {
            record.reject_votes += 1;
        }

        let total_arbiters = engagement.arbiters.len();
        let quorum = engagement.quorum;

        env.events().publish(
            (Symbol::new(&env, "arbiter_voted"), engagement_id.clone()),
            (milestone_index, approve),
        );

        if record.approve_votes >= quorum {
            let payment = (engagement.total_amount * milestone.payment_percent as i128) / 100;
            engagement.released_amount += payment;

            let arbiter_fee_bps: u32 = env
                .storage()
                .instance()
                .get(&DataKey::ArbiterFee)
                .unwrap_or(0u32);
            let arbiter_fee_amount = (payment * arbiter_fee_bps as i128) / 10_000;
            let net_payment = payment - arbiter_fee_amount;

            let token_client = token::Client::new(&env, &engagement.token);
            if arbiter_fee_amount > 0 {
                token_client.transfer(
                    &env.current_contract_address(),
                    &arbiter,
                    &arbiter_fee_amount,
                );
            }
            Self::distribute_recruiter_payout(&env, &engagement, net_payment, &token_client);

            let old_status = milestone.status.clone();
            milestone.status = MilestoneStatus::Resolved;
            engagement.milestones.set(milestone_index, milestone);

            let all_done = (0..engagement.milestones.len()).all(|i| {
                let s = engagement.milestones.get(i).unwrap().status;
                s == MilestoneStatus::Confirmed || s == MilestoneStatus::Resolved
            });
            let old_engagement_status = engagement.status.clone();
            if all_done {
                engagement.status = EngagementStatus::Completed;
                Self::decrement_company_active_count(&env, &engagement.company);
            }

            env.storage().persistent().remove(&vote_key);
            env.storage().persistent().remove(&DataKey::DisputeReason(
                engagement_id.clone(),
                milestone_index,
            ));

            env.events().publish(
                (Symbol::new(&env, "dispute_resolved"), engagement_id.clone()),
                (milestone_index, true),
            );
            Self::emit_milestone_status_changed(
                &env,
                &engagement_id,
                milestone_index,
                old_status,
                MilestoneStatus::Resolved,
            );
            Self::emit_engagement_status_changed(
                &env,
                &engagement_id,
                old_engagement_status,
                engagement.status.clone(),
            );
        } else if record.reject_votes > total_arbiters - quorum {
            let old_status = milestone.status.clone();
            milestone.status = MilestoneStatus::Pending;
            milestone.proof_hash = String::from_str(&env, "");
            milestone.proof_submitted_at = 0;
            engagement.milestones.set(milestone_index, milestone);

            env.storage().persistent().remove(&vote_key);
            // A rejected proof starts a new submission round, so do not make
            // the recruiter wait for the cooldown before replacing it.
            env.storage()
                .persistent()
                .remove(&DataKey::LastProofAt(engagement_id.clone(), milestone_index));
            env.storage().persistent().remove(&DataKey::DisputeReason(
                engagement_id.clone(),
                milestone_index,
            ));

            env.events().publish(
                (Symbol::new(&env, "dispute_resolved"), engagement_id.clone()),
                (milestone_index, false),
            );
            Self::emit_milestone_status_changed(
                &env,
                &engagement_id,
                milestone_index,
                old_status,
                MilestoneStatus::Pending,
            );
        } else {
            env.storage().persistent().set(&vote_key, &record);
            env.storage()
                .persistent()
                .extend_ttl(&vote_key, 100_000, 6_300_000);
        }

        engagement.last_activity_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
    }

    // ----------------------------------------------------------
    // REQUEST REPLACEMENT
    // ----------------------------------------------------------

    /// Company requests a replacement candidate after the placement milestone
    /// was already confirmed. Unconfirmed milestones are reset (Placement →
    /// `Pending`, Retention → `Locked` with its timer restarted).
    ///
    /// A Retention milestone that is currently `Disputed` (a dispute was raised
    /// but arbiters have not yet reached quorum) is included in this reset: it is
    /// forced back to `Locked` and its in-flight vote tally and dispute reason
    /// are cleared, so a future dispute on the same milestone index starts from
    /// a clean vote count instead of inheriting stale votes. See issue #177.
    pub fn request_replacement(env: Env, company: Address, engagement_id: String, reason: String) {
        Self::assert_not_paused(&env);
        company.require_auth();

        // Bound reason length so a single engagement cannot accumulate
        // unbounded replacement metadata. See issue #51.
        if reason.len() > MAX_REPLACEMENT_REASON_LEN {
            panic!("replacement reason too long");
        }

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if company != engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let placement_confirmed = {
            let m0 = engagement.milestones.get(0).unwrap();
            m0.status == MilestoneStatus::Confirmed || m0.status == MilestoneStatus::Resolved
        };

        if !placement_confirmed {
            panic!("placement not yet confirmed — use cancel_engagement instead");
        }

        let replacement_index: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::ReplacementCount(engagement_id.clone()))
            .unwrap_or(0u32);

        if replacement_index >= Self::get_max_replacements_internal(&env) {
            panic!("ReplacementLimitReached");
        }

        let current_ledger = env.ledger().sequence();

        for i in 0..engagement.milestones.len() {
            let mut m = engagement.milestones.get(i).unwrap();
            match m.kind {
                MilestoneKind::Placement => {
                    if m.status == MilestoneStatus::Confirmed
                        || m.status == MilestoneStatus::Resolved
                    {
                        let old_status = m.status.clone();
                        m.status = MilestoneStatus::Pending;
                        m.proof_hash = String::from_str(&env, "");
                        Self::emit_milestone_status_changed(
                            &env,
                            &engagement_id,
                            i,
                            old_status,
                            MilestoneStatus::Pending,
                        );
                        // Clear cooldown so the replacement candidate can submit immediately.
                        env.storage()
                            .persistent()
                            .remove(&DataKey::LastProofAt(engagement_id.clone(), i));
                    }
                }
                MilestoneKind::Retention => {
                    if m.status != MilestoneStatus::Confirmed
                        && m.status != MilestoneStatus::Resolved
                    {
                        // Issue #177: a Retention milestone can be Disputed (dispute
                        // raised, arbiters not yet at quorum) when a replacement is
                        // requested. Resetting it to Locked here must also clear any
                        // in-flight vote tally and dispute reason — otherwise a stale
                        // ArbiterVoteRecord (partial votes, or arbiters who already
                        // "voted") would silently carry over and be read by
                        // `cast_arbiter_vote` the next time this same milestone index
                        // is disputed again, corrupting the new dispute's vote count.
                        let was_disputed = m.status == MilestoneStatus::Disputed;
                        let original_days = (m.valid_after_ledger - engagement.created_at_ledger)
                            / Self::get_ledgers_per_day_internal(&env);
                        m.valid_after_ledger = current_ledger
                            + (original_days * Self::get_ledgers_per_day_internal(&env));
                        let old_status = m.status.clone();
                        m.status = MilestoneStatus::Locked;
                        m.proof_hash = String::from_str(&env, "");
                        if old_status != MilestoneStatus::Locked {
                            Self::emit_milestone_status_changed(
                                &env,
                                &engagement_id,
                                i,
                                old_status,
                                MilestoneStatus::Locked,
                            );
                        }
                        env.storage()
                            .persistent()
                            .remove(&DataKey::LastProofAt(engagement_id.clone(), i));

                        if was_disputed {
                            env.storage()
                                .persistent()
                                .remove(&DataKey::ArbiterVotes(engagement_id.clone(), i));
                            env.storage()
                                .persistent()
                                .remove(&DataKey::DisputeReason(engagement_id.clone(), i));
                        }
                    }
                }
            }
            engagement.milestones.set(i, m);
        }

        let old_engagement_status = engagement.status.clone();
        engagement.status = EngagementStatus::ReplacementRequested;
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);

        // Record the reason under a monotonic per-engagement index so the
        // full replacement history is auditable. See issue #51.
        env.storage().persistent().set(
            &DataKey::ReplacementReason(engagement_id.clone(), replacement_index),
            &reason,
        );
        env.storage().persistent().set(
            &DataKey::ReplacementCount(engagement_id.clone()),
            &(replacement_index + 1),
        );

        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        env.events().publish(
            (
                Symbol::new(&env, "replacement_requested"),
                engagement_id.clone(),
            ),
            (replacement_index, reason),
        );
    }

    /// Returns the structured replacement reason recorded for an engagement at
    /// a given replacement index, or `None` if no such replacement exists.
    /// See issue #51.
    pub fn get_replacement_reason(
        env: Env,
        engagement_id: String,
        replacement_index: u32,
    ) -> Option<String> {
        env.storage().persistent().get(&DataKey::ReplacementReason(
            engagement_id,
            replacement_index,
        ))
    }

    /// Returns the number of replacements ever requested for an engagement.
    /// Useful for paging through `get_replacement_reason`. See issue #51.
    pub fn get_replacement_count(env: Env, engagement_id: String) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ReplacementCount(engagement_id))
            .unwrap_or(0u32)
    }

    // ----------------------------------------------------------
    // ISSUE #31 — REPLACEMENT COUNT LIMIT
    // ----------------------------------------------------------

    /// Admin sets the maximum number of replacements allowed per engagement.
    /// Defaults to 3 when not explicitly configured.
    pub fn set_max_replacements(env: Env, admin: Address, count: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::MaxReplacements, &count);
        env.events()
            .publish((Symbol::new(&env, "max_replacements_set"),), count);
    }

    /// Return the current maximum replacement count cap.
    /// Returns `DEFAULT_MAX_REPLACEMENTS` (3) when not configured.
    pub fn get_max_replacements(env: Env) -> u32 {
        Self::get_max_replacements_internal(&env)
    }

    // ----------------------------------------------------------
    // ISSUE #43 — COMPANY TRANSFER
    // ----------------------------------------------------------

    /// Transfer the company role on an engagement to a new address, effective
    /// immediately (e.g. the company was acquired or restructured).
    ///
    /// # Caller
    /// `current_company` — must match `engagement.company` and sign the transaction.
    ///
    /// # Panics
    /// - `"unauthorized"` — caller is not the engagement's current company.
    /// - `"engagement is not active"` — engagement status is not `Active` or
    ///   `ReplacementRequested`.
    ///
    /// # Events
    /// - `("company_transferred", engagement_id)` with `(old_company, new_company)`.
    pub fn transfer_company(
        env: Env,
        current_company: Address,
        engagement_id: String,
        new_company: Address,
    ) {
        Self::assert_not_paused(&env);
        current_company.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if current_company != engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        if engagement.status != EngagementStatus::Active
            && engagement.status != EngagementStatus::ReplacementRequested
        {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        let old_company = engagement.company.clone();
        engagement.company = new_company.clone();
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);

        env.events().publish(
            (
                Symbol::new(&env, "company_transferred"),
                engagement_id.clone(),
            ),
            (old_company, new_company),
        );
    }

    // ----------------------------------------------------------
    // CANCEL ENGAGEMENT
    // ----------------------------------------------------------

    /// Cancel an engagement and refund any unreleased escrow to the company.
    ///
    /// # Caller
    /// Requires authentication from both `company` and `recruiter` — each
    /// address is validated against `engagement.company` / `engagement.recruiter`
    /// and must call `require_auth`. The two addresses together must agree to
    /// the cancellation; neither party can cancel unilaterally.
    ///
    /// # Precondition
    /// Intended for cancellation **before any milestones have been confirmed**,
    /// the point at which `released_amount == 0` and the full `total_amount`
    /// is still refundable to the company. Once the placement milestone has
    /// already been `Confirmed` / `Resolved`, prefer
    /// [`Self::request_replacement`] instead — `cancel_engagement` is still
    /// callable on a `ReplacementRequested` engagement because
    /// `request_replacement` may have been invoked already, but it will only
    /// refund the unreleased remainder rather than the full fee.
    ///
    /// Strictly enforced: the engagement must be in `Active` or
    /// `ReplacementRequested` status; any other state (`Completed`,
    /// `Cancelled`, `Expired`, `ExitRequested`) is rejected. The contract
    /// must also not be paused, otherwise the call also fails — see
    /// [`Self::assert_not_paused`].
    ///
    /// # Refund behaviour
    /// Transfers `engagement.total_amount - engagement.released_amount` from
    /// the contract's escrow back to `engagement.company` using the
    /// engagement's escrow token.
    ///
    /// Side effects after the refund:
    /// - Engagement status is set to the terminal `Cancelled` state.
    /// - Any pending `AmendmentProposal`s for the engagement's milestones
    ///   are cleared, so `accept_amendment` / `reject_amendment` cannot
    ///   mutate a cancelled engagement (see issue #176).
    /// - The per-company active engagement counter is decremented.
    ///
    /// # Panics
    /// - `"ContractPaused"` — the contract is paused (raised by
    ///   [`Self::assert_not_paused`] before authentication).
    /// - `"engagement is not active"` — engagement is not in `Active` or
    ///   `ReplacementRequested` status.
    /// - `"unauthorized"` — `company` does not match `engagement.company`
    ///   or `recruiter` does not match `engagement.recruiter`.
    ///
    /// # Events
    /// Emits `("engagement_cancelled", engagement_id)` with the `refund`
    /// amount as the event body, in addition to the usual
    /// `engagement_status_changed` status transition event.
    pub fn cancel_engagement(
        env: Env,
        company: Address,
        recruiter: Address,
        engagement_id: String,
    ) {
        Self::assert_not_paused(&env);
        company.require_auth();
        recruiter.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active
            && engagement.status != EngagementStatus::ReplacementRequested
        {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if company != engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        if recruiter != engagement.recruiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let refund = engagement.total_amount - engagement.released_amount;
        let token_client = token::Client::new(&env, &engagement.token);
        token_client.transfer(
            &env.current_contract_address(),
            &engagement.company,
            &refund,
        );

        let old_engagement_status = engagement.status.clone();
        engagement.status = EngagementStatus::Cancelled;
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        // Issue #176: a pending (unexpired) amendment proposal survives a
        // cancellation unless explicitly cleared here — `accept_amendment` and
        // `reject_amendment` never check `engagement.status`, so without this a
        // proposal could still be accepted/rejected after the engagement is
        // terminal, mutating milestone.payment_percent on a Cancelled engagement.
        // Clearing on cancel also makes `get_pending_amendment` correctly report
        // no pending amendment once the engagement is gone.
        for i in 0..engagement.milestones.len() {
            let key = DataKey::AmendmentProposal(engagement_id.clone(), i);
            if env.storage().persistent().has(&key) {
                env.storage().persistent().remove(&key);
            }
        }

        Self::decrement_company_active_count(&env, &engagement.company);

        env.events().publish(
            (
                Symbol::new(&env, "engagement_cancelled"),
                engagement_id.clone(),
            ),
            refund,
        );
    }

    // ----------------------------------------------------------
    // ISSUE #33 — ESCROW TOP-UP
    // ----------------------------------------------------------

    /// Company tops up the escrow balance for an active engagement.
    pub fn top_up_escrow(env: Env, company: Address, engagement_id: String, amount: i128) {
        Self::assert_not_paused(&env);
        company.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active
            && engagement.status != EngagementStatus::ReplacementRequested
        {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if company != engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        if amount <= 0 {
            panic!("amount must be greater than zero");
        }

        let token_client = token::Client::new(&env, &engagement.token);
        token_client.transfer(&company, &env.current_contract_address(), &amount);

        engagement.total_amount += amount;
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);

        env.events().publish(
            (Symbol::new(&env, "escrow_topped_up"), engagement_id.clone()),
            (amount, engagement.total_amount),
        );
    }

    // ----------------------------------------------------------
    // READ-ONLY QUERIES
    // ----------------------------------------------------------

    /// Get the deployed contract version string (issue #16).
    /// No authentication required.
    pub fn get_version(env: Env) -> String {
        env.storage()
            .persistent()
            .get(&DataKey::Version)
            .unwrap_or_else(|| String::from_str(&env, DEFAULT_VERSION))
    }

    /// Get the current minimum engagement amount in stroops (issue #17).
    /// No authentication required.
    pub fn get_min_amount(env: Env) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::MinEngagementAmount)
            .unwrap_or(DEFAULT_MIN_ENGAGEMENT_AMOUNT)
    }

    /// Returns the full engagement record for a given engagement ID.
    ///
    /// # Returns
    /// The complete [`Engagement`] struct containing all engagement details.
    ///
    /// # Panics
    /// Panics with `"engagement not found"` if no engagement exists with the given `engagement_id`.
    pub fn get_engagement(env: Env, engagement_id: String) -> Engagement {
        Self::get_engagement_internal(&env, &engagement_id)
    }

    /// Returns a specific milestone from an engagement.
    ///
    /// # Returns
    /// The [`Milestone`] struct at the given index within the engagement's milestone list.
    ///
    /// # Panics
    /// Panics with `"engagement not found"` if no engagement exists with the given `engagement_id`.
    /// Panics with `"invalid milestone index"` if `milestone_index` is out of bounds for the engagement's milestones.
    pub fn get_milestone(env: Env, engagement_id: String, milestone_index: u32) -> Milestone {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        Self::get_milestone_or_panic(&engagement, milestone_index)
    }

    /// Returns the status of every milestone in the engagement, ordered by
    /// milestone index, in a single call (issue #37). Read-only, permissionless.
    pub fn get_all_milestone_statuses(env: Env, engagement_id: String) -> Vec<MilestoneStatus> {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        let mut statuses = Vec::new(&env);
        for i in 0..engagement.milestones.len() {
            statuses.push_back(engagement.milestones.get(i).unwrap().status);
        }
        statuses
    }

    /// Returns the current escrow balance for an engagement.
    ///
    /// # Returns
    /// The remaining escrow balance in the token's smallest unit (e.g., stroops).
    /// Calculated as `total_amount - released_amount`.
    ///
    /// # Panics
    /// Panics with `"engagement not found"` if no engagement exists with the given `engagement_id`.
    pub fn get_escrow_balance(env: Env, engagement_id: String) -> i128 {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        engagement.total_amount - engagement.released_amount
    }

    /// Returns `true` if the milestone is `Locked` and the current ledger
    /// sequence is greater than or equal to its `valid_after_ledger`, meaning
    /// it can currently be unlocked via `unlock_milestone`.
    pub fn is_milestone_unlockable(env: Env, engagement_id: String, milestone_index: u32) -> bool {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        let milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        milestone.status == MilestoneStatus::Locked
            && env.ledger().sequence() >= milestone.valid_after_ledger
    }

    /// Returns the number of ledgers remaining until the milestone becomes
    /// unlockable, or `0` if it is already unlockable.
    ///
    /// When the result is `0`, `unlock_milestone` can be called immediately.
    /// Otherwise, the caller must wait at least this many more ledgers before
    /// `env.ledger().sequence() >= milestone.valid_after_ledger` holds.
    pub fn ledgers_until_unlock(env: Env, engagement_id: String, milestone_index: u32) -> u32 {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        let milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        let current = env.ledger().sequence();
        if current >= milestone.valid_after_ledger {
            0
        } else {
            milestone.valid_after_ledger - current
        }
    }

    /// Returns approximate seconds until a Locked retention milestone unlocks.
    /// Returns 0 if the milestone is already unlockable or is a Placement milestone.
    pub fn get_estimated_unlock_seconds(
        env: Env,
        engagement_id: String,
        milestone_index: u32,
    ) -> u64 {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        let milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        if milestone.kind == MilestoneKind::Placement {
            return 0;
        }

        let current = env.ledger().sequence();
        if current >= milestone.valid_after_ledger {
            return 0;
        }

        let ledgers_remaining = (milestone.valid_after_ledger - current) as u64;
        // seconds_per_ledger = 86_400 / ledgers_per_day
        let lpd = Self::get_ledgers_per_day_internal(&env) as u64;
        ledgers_remaining * (86_400u64 / lpd)
    }

    /// Return the IPFS CID stored at engagement creation, or None if not provided.
    pub fn get_metadata_hash(env: Env, engagement_id: String) -> Option<String> {
        Self::get_engagement_internal(&env, &engagement_id).metadata_hash
    }

    /// Return the current approve/reject vote counts for a disputed milestone.
    /// Returns (0, 0) if no votes have been cast yet.
    pub fn get_arbiter_votes(
        env: Env,
        engagement_id: String,
        milestone_index: u32,
    ) -> ArbiterVoteCounts {
        let vote_key = DataKey::ArbiterVotes(engagement_id, milestone_index);
        let record: ArbiterVoteRecord =
            env.storage()
                .persistent()
                .get(&vote_key)
                .unwrap_or(ArbiterVoteRecord {
                    approve_votes: 0,
                    reject_votes: 0,
                    voted: Vec::new(&env),
                });
        ArbiterVoteCounts {
            approve_votes: record.approve_votes,
            reject_votes: record.reject_votes,
        }
    }

    /// Total amount released for this engagement, represented by
    /// `Engagement.released_amount`.
    ///
    /// This is not the escrow balance. To get remaining contract funds,
    /// use `total_amount - released_amount`; `get_escrow_balance` provides
    /// that derived value.
    pub fn get_total_released(env: Env, engagement_id: String) -> i128 {
        Self::get_engagement_internal(&env, &engagement_id).released_amount
    }

    /// Return a lightweight summary of an engagement, suitable for list/dashboard views.
    ///
    /// Prefer this over `get_engagement` when you only need top-level fields and not
    /// the full milestone list, as it avoids deserializing the milestone vector.
    ///
    /// # Panics
    ///
    /// - `"EngagementNotFound"` — no engagement with the given `engagement_id` exists.
    pub fn get_engagement_summary(env: Env, engagement_id: String) -> EngagementSummary {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        EngagementSummary {
            id: engagement.id,
            job_title: engagement.job_title,
            company: engagement.company,
            recruiter: engagement.recruiter,
            total_amount: engagement.total_amount,
            released_amount: engagement.released_amount,
            status: engagement.status,
            milestone_count: engagement.milestones.len(),
            created_at_ledger: engagement.created_at_ledger,
            co_recruiter: engagement.co_recruiter,
            recruiter_split_bps: engagement.recruiter_split_bps,
            contract_pdf_hash: engagement.contract_pdf_hash,
        }
    }

    /// Return the off-chain attestation hash (e.g. SHA-256 of the contract PDF)
    /// stored at engagement creation, or None if not provided.
    /// Read-only and permissionless.
    pub fn get_contract_pdf_hash(env: Env, engagement_id: String) -> Option<String> {
        Self::get_engagement_internal(&env, &engagement_id).contract_pdf_hash
    }

    // ----------------------------------------------------------
    // ARBITER SUCCESSION
    // ----------------------------------------------------------

    /// Current arbiter nominates a successor. The successor must call `claim_arbiter`.
    /// Any arbiter in the engagement's arbiter list may initiate succession for their slot.
    ///
    /// # Panics
    /// - `"engagement is in a terminal state"` — the engagement is `Completed`,
    ///   `Cancelled`, or `Expired`. Arbiter succession has no practical function
    ///   once an engagement can no longer be disputed.
    pub fn nominate_arbiter_successor(
        env: Env,
        arbiter: Address,
        engagement_id: String,
        successor: Address,
    ) {
        Self::assert_not_paused(&env);
        arbiter.require_auth();

        let engagement = Self::get_engagement_internal(&env, &engagement_id);

        if Self::is_terminal_status(&engagement.status) {
            panic!("engagement is in a terminal state");
        }

        let is_arbiter =
            (0..engagement.arbiters.len()).any(|i| engagement.arbiters.get(i).unwrap() == arbiter);
        if !is_arbiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let nomination = ArbiterNomination {
            current: arbiter.clone(),
            nominee: successor.clone(),
        };

        env.storage()
            .persistent()
            .set(&DataKey::PendingArbiter(engagement_id.clone()), &nomination);

        env.storage().persistent().extend_ttl(
            &DataKey::PendingArbiter(engagement_id.clone()),
            100_000,
            6_300_000,
        );

        env.events().publish(
            (
                Symbol::new(&env, "arbiter_nominated"),
                engagement_id.clone(),
            ),
            successor,
        );
    }

    /// Nominated successor claims the arbiter slot, replacing the nominating arbiter.
    ///
    /// # Panics
    /// - `"engagement is in a terminal state"` — the engagement reached `Completed`,
    ///   `Cancelled`, or `Expired` after the nomination was made; the seat can no
    ///   longer be claimed.
    pub fn claim_arbiter(env: Env, nominee: Address, engagement_id: String) {
        Self::assert_not_paused(&env);
        nominee.require_auth();

        let nomination: ArbiterNomination = env
            .storage()
            .persistent()
            .get(&DataKey::PendingArbiter(engagement_id.clone()))
            .unwrap_or_else(|| panic!("no pending arbiter nomination"));

        if nominee != nomination.nominee {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if Self::is_terminal_status(&engagement.status) {
            panic!("engagement is in a terminal state");
        }

        // Replace the nominating arbiter's slot with the nominee.
        for i in 0..engagement.arbiters.len() {
            if engagement.arbiters.get(i).unwrap() == nomination.current {
                engagement.arbiters.set(i, nominee.clone());
                break;
            }
        }

        // Migrate the seat's vote identity on any dispute currently in progress
        // (issue #178). Without this, the old arbiter's cast vote no longer
        // matches any address in `engagement.arbiters`, but the successor's
        // address also isn't in `voted`, so `cast_arbiter_vote`'s duplicate-vote
        // check would let the successor cast a second vote for the same seat.
        for i in 0..engagement.milestones.len() {
            if engagement.milestones.get(i).unwrap().status == MilestoneStatus::Disputed {
                let vote_key = DataKey::ArbiterVotes(engagement_id.clone(), i);
                if let Some(mut record) = env
                    .storage()
                    .persistent()
                    .get::<DataKey, ArbiterVoteRecord>(&vote_key)
                {
                    for j in 0..record.voted.len() {
                        if record.voted.get(j).unwrap() == nomination.current {
                            record.voted.set(j, nominee.clone());
                        }
                    }
                    env.storage().persistent().set(&vote_key, &record);
                }
            }
        }

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);

        env.storage()
            .persistent()
            .remove(&DataKey::PendingArbiter(engagement_id.clone()));

        env.events().publish(
            (Symbol::new(&env, "arbiter_claimed"), engagement_id.clone()),
            nominee,
        );
    }

    // ----------------------------------------------------------
    // AMENDMENT PROPOSAL MANAGEMENT
    // ----------------------------------------------------------

    /// Admin sets the amendment proposal TTL in ledgers.
    /// Default is 17,280 ledgers (~1 day).
    ///
    /// # Arguments
    /// - `admin`   — must be the contract admin
    /// - `ledgers` — number of ledgers before a proposal expires
    pub fn set_amendment_ttl(env: Env, admin: Address, ledgers: u32) {
        admin.require_auth();

        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic!("admin not set"));

        if admin != stored_admin {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        env.storage()
            .persistent()
            .set(&DataKey::AmendmentTTL, &ledgers);

        env.storage()
            .persistent()
            .extend_ttl(&DataKey::AmendmentTTL, 100_000, 6_300_000);
    }

    /// Get the current amendment proposal TTL in ledgers.
    /// Returns 17,280 if not yet set.
    pub fn get_amendment_ttl(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::AmendmentTTL)
            .unwrap_or(17_280)
    }

    /// Propose a milestone payment-percent amendment.
    /// Either company or recruiter can propose; the other party must accept.
    /// Only one pending proposal may exist per milestone; a new proposal overwrites.
    ///
    /// # Arguments
    /// - `proposer`             — company or recruiter (must sign)
    /// - `engagement_id`        — the engagement
    /// - `milestone_index`      — the milestone to amend
    /// - `new_payment_percent`  — the proposed new payment percent
    pub fn propose_amendment(
        env: Env,
        proposer: Address,
        engagement_id: String,
        milestone_index: u32,
        new_payment_percent: u32,
    ) {
        proposer.require_auth();

        let engagement = Self::get_engagement_internal(&env, &engagement_id);

        if proposer != engagement.company && proposer != engagement.recruiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let _ = Self::get_milestone_or_panic(&engagement, milestone_index);

        if new_payment_percent > 100 {
            panic!("payment percent must be 0-100");
        }

        let current_ledger = env.ledger().sequence();
        let ttl = env
            .storage()
            .persistent()
            .get(&DataKey::AmendmentTTL)
            .unwrap_or(17_280);

        let proposal = AmendmentProposal {
            proposer: proposer.clone(),
            new_payment_percent,
            proposed_at_ledger: current_ledger,
            expires_at_ledger: current_ledger + ttl,
        };

        env.storage().persistent().set(
            &DataKey::AmendmentProposal(engagement_id.clone(), milestone_index),
            &proposal,
        );

        env.storage().persistent().extend_ttl(
            &DataKey::AmendmentProposal(engagement_id.clone(), milestone_index),
            100_000,
            6_300_000,
        );

        env.events().publish(
            (
                Symbol::new(&env, "amendment_proposed"),
                engagement_id.clone(),
            ),
            (
                milestone_index,
                proposer,
                new_payment_percent,
                current_ledger + ttl,
            ),
        );
    }

    /// Accept a pending amendment proposal, applying the change immediately.
    /// The acceptor must be the other party (not the proposer).
    /// The milestone's payment percent is updated, and an AmendmentEntry is recorded.
    /// If the proposal is expired (current_ledger > expires_at_ledger), reject with "expired".
    ///
    /// # Arguments
    /// - `acceptor`        — company or recruiter (must sign, must NOT be the proposer)
    /// - `engagement_id`   — the engagement
    /// - `milestone_index` — the milestone to accept the amendment for
    pub fn accept_amendment(
        env: Env,
        acceptor: Address,
        engagement_id: String,
        milestone_index: u32,
    ) {
        acceptor.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if acceptor != engagement.company && acceptor != engagement.recruiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let proposal: AmendmentProposal = env
            .storage()
            .persistent()
            .get(&DataKey::AmendmentProposal(
                engagement_id.clone(),
                milestone_index,
            ))
            .unwrap_or_else(|| panic!("no pending amendment proposal"));

        if acceptor == proposal.proposer {
            panic!("proposer cannot accept their own proposal");
        }

        let current_ledger = env.ledger().sequence();

        if current_ledger > proposal.expires_at_ledger {
            env.storage()
                .persistent()
                .remove(&DataKey::AmendmentProposal(
                    engagement_id.clone(),
                    milestone_index,
                ));

            env.events().publish(
                (
                    Symbol::new(&env, "amendment_rejected"),
                    engagement_id.clone(),
                ),
                (milestone_index, acceptor, Symbol::new(&env, "expired")),
            );

            panic!("amendment_expired");
        }

        let mut milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        let old_payment_percent = milestone.payment_percent;
        milestone.payment_percent = proposal.new_payment_percent;
        engagement.milestones.set(milestone_index, milestone);

        let amendment_entry = AmendmentEntry {
            proposer: proposal.proposer.clone(),
            old_payment_percent,
            new_payment_percent: proposal.new_payment_percent,
            ledger: current_ledger,
        };

        let mut log: Vec<AmendmentEntry> = env
            .storage()
            .persistent()
            .get(&DataKey::AmendmentLog(
                engagement_id.clone(),
                milestone_index,
            ))
            .unwrap_or_else(|| Vec::new(&env));

        log.push_back(amendment_entry);

        if log.len() > 20 {
            log.remove(0);
        }

        env.storage().persistent().set(
            &DataKey::AmendmentLog(engagement_id.clone(), milestone_index),
            &log,
        );

        env.storage().persistent().extend_ttl(
            &DataKey::AmendmentLog(engagement_id.clone(), milestone_index),
            100_000,
            6_300_000,
        );

        env.storage()
            .persistent()
            .remove(&DataKey::AmendmentProposal(
                engagement_id.clone(),
                milestone_index,
            ));

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);

        env.events().publish(
            (
                Symbol::new(&env, "amendment_accepted"),
                engagement_id.clone(),
            ),
            (
                milestone_index,
                acceptor,
                old_payment_percent,
                proposal.new_payment_percent,
            ),
        );
    }

    /// Reject a pending amendment proposal.
    /// Either company or recruiter can reject (if they're not the proposer).
    /// On rejection, the proposal is cleared and amendment_rejected event is emitted.
    ///
    /// # Arguments
    /// - `rejector`        — company or recruiter (must sign, must NOT be the proposer)
    /// - `engagement_id`   — the engagement
    /// - `milestone_index` — the milestone with the proposal to reject
    pub fn reject_amendment(
        env: Env,
        rejector: Address,
        engagement_id: String,
        milestone_index: u32,
    ) {
        rejector.require_auth();

        let engagement = Self::get_engagement_internal(&env, &engagement_id);

        if rejector != engagement.company && rejector != engagement.recruiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let proposal: AmendmentProposal = env
            .storage()
            .persistent()
            .get(&DataKey::AmendmentProposal(
                engagement_id.clone(),
                milestone_index,
            ))
            .unwrap_or_else(|| panic!("no pending amendment proposal"));

        if rejector == proposal.proposer {
            panic!("proposer cannot reject their own proposal");
        }

        env.storage()
            .persistent()
            .remove(&DataKey::AmendmentProposal(
                engagement_id.clone(),
                milestone_index,
            ));

        env.events().publish(
            (
                Symbol::new(&env, "amendment_rejected"),
                engagement_id.clone(),
            ),
            (milestone_index, rejector, Symbol::new(&env, "declined")),
        );
    }

    // ----------------------------------------------------------
    // AMENDMENT LOG QUERIES
    // ----------------------------------------------------------

    /// Get the amendment history for a milestone.
    /// Returns entries in chronological order (oldest first).
    /// Capped at 20 entries per milestone (FIFO eviction).
    ///
    /// # Arguments
    /// - `engagement_id`   — the engagement
    /// - `milestone_index` — the milestone
    pub fn get_amendment_log(
        env: Env,
        engagement_id: String,
        milestone_index: u32,
    ) -> Vec<AmendmentEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::AmendmentLog(engagement_id, milestone_index))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Get the current pending amendment proposal for a milestone, if one
    /// exists and has not expired. Returns `None` when there is no active
    /// proposal or the proposal's TTL has elapsed (treated as non-existent).
    pub fn get_pending_amendment(
        env: Env,
        engagement_id: String,
        milestone_index: u32,
    ) -> Option<PendingAmendment> {
        let key = DataKey::AmendmentProposal(engagement_id, milestone_index);
        match env.storage().persistent().get::<_, AmendmentProposal>(&key) {
            Some(proposal) if env.ledger().sequence() <= proposal.expires_at_ledger => {
                Some(proposal)
            }
            _ => None,
        }
    }

    // ----------------------------------------------------------
    // ISSUE #26 — TOKEN ALLOWLIST
    // ----------------------------------------------------------

    /// Add a token SAC address to the allowlist. Admin only.
    ///
    /// Note (issue #175): the contract treats `total_amount`, milestone payouts,
    /// and `MinEngagementAmount` as raw integer units of whichever token is used
    /// for a given engagement — it never queries or adjusts for that token's
    /// `decimals()`. Percentage-based math (`amount * payment_percent / 100`) is
    /// exact integer arithmetic regardless of decimals, but a single admin-wide
    /// `MinEngagementAmount` (see [`Self::set_min_amount`]) means the effective
    /// real-world minimum will differ across tokens with different precision.
    /// Only allowlist tokens whose smallest unit is comparable in scale to what
    /// `MinEngagementAmount` assumes, or adjust the minimum accordingly.
    pub fn add_allowed_token(env: Env, admin: Address, token: Address) {
        Self::assert_admin(&env, &admin);
        let mut allowed: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::AllowedTokens)
            .unwrap_or_else(|| Vec::new(&env));
        let already = (0..allowed.len()).any(|i| allowed.get(i).unwrap() == token);
        if !already {
            allowed.push_back(token.clone());
            env.storage()
                .persistent()
                .set(&DataKey::AllowedTokens, &allowed);
        }
        env.events()
            .publish((Symbol::new(&env, "token_allowlisted"),), token);
    }

    /// Remove a token SAC address from the allowlist. Admin only.
    pub fn remove_allowed_token(env: Env, admin: Address, token: Address) {
        Self::assert_admin(&env, &admin);
        let mut allowed: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::AllowedTokens)
            .unwrap_or_else(|| Vec::new(&env));
        for i in 0..allowed.len() {
            if allowed.get(i).unwrap() == token {
                allowed.remove(i);
                break;
            }
        }
        env.storage()
            .persistent()
            .set(&DataKey::AllowedTokens, &allowed);
        env.events()
            .publish((Symbol::new(&env, "token_removed"),), token);
    }

    /// Enable or disable the token allowlist. Admin only.
    pub fn set_token_allowlist_enabled(env: Env, admin: Address, enabled: bool) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .persistent()
            .set(&DataKey::AllowlistEnabled, &enabled);
        env.events()
            .publish((Symbol::new(&env, "allowlist_enabled_set"),), enabled);
    }

    /// Return all currently allowlisted token addresses.
    pub fn get_allowed_tokens(env: Env) -> Vec<Address> {
        env.storage()
            .persistent()
            .get(&DataKey::AllowedTokens)
            .unwrap_or_else(|| Vec::new(&env))
    }

    // ----------------------------------------------------------
    // ISSUE #32 — RECRUITER EARLY-EXIT
    // ----------------------------------------------------------

    /// Signal that the recruiter wishes to exit the engagement early without completing
    /// all remaining milestones.
    ///
    /// This is the first step of the three-step early-exit protocol. After calling this,
    /// the company must respond with [`accept_early_exit`] or [`reject_early_exit`].
    ///
    /// # Caller
    /// `recruiter` — must match the engagement's recruiter and sign the transaction.
    ///
    /// # Behaviour
    /// Sets the engagement status to `ExitRequested`. The company must then call
    /// [`accept_early_exit`] or [`reject_early_exit`].
    ///
    /// # Panics
    /// - `"engagement is not active"` — engagement is not `Active`.
    /// - `"unauthorized"` — caller is not the recruiter.
    ///
    /// # Events
    /// Emits `("early_exit_requested", engagement_id)`.
    pub fn request_early_exit(env: Env, recruiter: Address, engagement_id: String) {
        Self::assert_not_paused(&env);
        recruiter.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if recruiter != engagement.recruiter {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        let old_engagement_status = engagement.status.clone();
        engagement.status = EngagementStatus::ExitRequested;
        engagement.last_activity_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        env.events().publish(
            (
                Symbol::new(&env, "early_exit_requested"),
                engagement_id.clone(),
            ),
            recruiter,
        );
    }

    /// Accept the recruiter's early-exit request.
    ///
    /// This is the second step of the early-exit protocol, called by the company in
    /// response to a prior [`request_early_exit`]. See also [`reject_early_exit`] for
    /// the alternative resolution.
    ///
    /// # Caller
    /// `company` — must match the engagement's company and sign the transaction.
    ///
    /// # Behaviour
    /// - Pays the recruiter for every milestone already `Confirmed` or `Resolved`
    ///   (proportional to `released_amount`).
    /// - Refunds the remaining escrow balance (`total_amount - released_amount`) to the company.
    /// - Sets the engagement status to `Cancelled`.
    ///
    /// # Panics
    /// - `"no exit request pending"` — engagement is not in `ExitRequested` status.
    /// - `"unauthorized"` — caller is not the company.
    ///
    /// # Events
    /// Emits `("early_exit_accepted", engagement_id)` with the refund amount.
    pub fn accept_early_exit(env: Env, company: Address, engagement_id: String) {
        Self::assert_not_paused(&env);
        company.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);
        Self::assert_exit_request_pending(&engagement, &company);

        let refund = engagement.total_amount - engagement.released_amount;
        if refund > 0 {
            let token_client = token::Client::new(&env, &engagement.token);
            token_client.transfer(
                &env.current_contract_address(),
                &engagement.company,
                &refund,
            );
        }

        let old_engagement_status = engagement.status.clone();
        engagement.status = EngagementStatus::Cancelled;
        engagement.last_activity_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        Self::decrement_company_active_count(&env, &engagement.company);

        env.events().publish(
            (
                Symbol::new(&env, "early_exit_accepted"),
                engagement_id.clone(),
            ),
            refund,
        );
    }

    /// Reject the recruiter's early-exit request, returning the engagement to `Active`.
    ///
    /// This is the second step of the early-exit protocol, called by the company in
    /// response to a prior [`request_early_exit`]. See also [`accept_early_exit`] for
    /// the alternative resolution.
    ///
    /// # Caller
    /// `company` — must match the engagement's company and sign the transaction.
    ///
    /// # Behaviour
    /// Sets the engagement status back to `Active`. The recruiter must continue working
    /// through the remaining milestones.
    ///
    /// # Panics
    /// - `"no exit request pending"` — engagement is not in `ExitRequested` status.
    /// - `"unauthorized"` — caller is not the company.
    ///
    /// # Events
    /// Emits `("early_exit_rejected", engagement_id)`.
    pub fn reject_early_exit(env: Env, company: Address, engagement_id: String) {
        Self::assert_not_paused(&env);
        company.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);
        Self::assert_exit_request_pending(&engagement, &company);

        let old_engagement_status = engagement.status.clone();
        engagement.status = EngagementStatus::Active;
        engagement.last_activity_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        env.events().publish(
            (
                Symbol::new(&env, "early_exit_rejected"),
                engagement_id.clone(),
            ),
            company,
        );
    }

    // ----------------------------------------------------------
    // ISSUE #34 — ENGAGEMENT COUNT
    // ----------------------------------------------------------

    /// Return the total number of engagements ever created.
    pub fn get_engagement_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::EngagementCount)
            .unwrap_or(0u64)
    }

    // ----------------------------------------------------------
    // ISSUE #35 — ENGAGEMENT LIST BY COMPANY
    // ----------------------------------------------------------

    /// Return a paginated slice of engagement IDs for a given company.
    /// `page` is 0-indexed; out-of-range pages return an empty vec.
    pub fn get_engagements_by_company(
        env: Env,
        company: Address,
        page: u32,
        page_size: u32,
    ) -> Vec<String> {
        let ids: Vec<String> = env
            .storage()
            .persistent()
            .get(&DataKey::CompanyEngagements(company))
            .unwrap_or_else(|| Vec::new(&env));

        let total = ids.len();
        if page_size == 0 {
            return Vec::new(&env);
        }
        // Use saturating arithmetic so a huge `page` / `page_size` combination
        // clamps to an out-of-range start (caught below) instead of wrapping
        // around via u32 overflow.
        let start = page.saturating_mul(page_size);
        if start >= total {
            return Vec::new(&env);
        }
        let end = start.saturating_add(page_size).min(total);
        let mut result = Vec::new(&env);
        for i in start..end {
            result.push_back(ids.get(i).unwrap());
        }
        result
    }

    /// Return the total number of engagements associated with a company.
    pub fn get_company_engagement_count(env: Env, company: Address) -> u32 {
        let ids: Vec<String> = env
            .storage()
            .persistent()
            .get(&DataKey::CompanyEngagements(company))
            .unwrap_or_else(|| Vec::new(&env));
        ids.len()
    }

    // ----------------------------------------------------------
    // ISSUE #36 — ENGAGEMENT LIST BY RECRUITER
    // ----------------------------------------------------------

    /// Return a paginated slice of engagement IDs for a given recruiter.
    /// `page` is 0-indexed; out-of-range pages return an empty vec.
    pub fn get_engagements_by_recruiter(
        env: Env,
        recruiter: Address,
        page: u32,
        page_size: u32,
    ) -> Vec<String> {
        let ids: Vec<String> = env
            .storage()
            .persistent()
            .get(&DataKey::RecruiterEngagements(recruiter))
            .unwrap_or_else(|| Vec::new(&env));

        let total = ids.len();
        if page_size == 0 {
            return Vec::new(&env);
        }
        // Use saturating arithmetic so a huge `page` / `page_size` combination
        // clamps to an out-of-range start (caught below) instead of wrapping
        // around via u32 overflow.
        let start = page.saturating_mul(page_size);
        if start >= total {
            return Vec::new(&env);
        }
        let end = start.saturating_add(page_size).min(total);
        let mut result = Vec::new(&env);
        for i in start..end {
            result.push_back(ids.get(i).unwrap());
        }
        result
    }

    /// Return the total number of engagements associated with a recruiter.
    pub fn get_recruiter_engagement_count(env: Env, recruiter: Address) -> u32 {
        let ids: Vec<String> = env
            .storage()
            .persistent()
            .get(&DataKey::RecruiterEngagements(recruiter))
            .unwrap_or_else(|| Vec::new(&env));
        ids.len()
    }

    // ----------------------------------------------------------
    // ISSUE #41 — CONFIGURABLE LEDGERS PER DAY
    // ----------------------------------------------------------

    /// Admin sets how many ledgers constitute one day (min 1, max 25_920).
    pub fn set_ledgers_per_day(env: Env, admin: Address, value: u32) {
        Self::assert_admin(&env, &admin);
        if value < 1 || value > 25_920 {
            panic!("InvalidLedgersPerDay");
        }
        env.storage()
            .instance()
            .set(&DataKey::LedgersPerDay, &value);
        env.events()
            .publish((Symbol::new(&env, "ledgers_per_day_set"),), value);
    }

    /// Return the current ledgers-per-day constant.
    pub fn get_ledgers_per_day(env: Env) -> u32 {
        Self::get_ledgers_per_day_internal(&env)
    }

    // ----------------------------------------------------------
    // ISSUE #18 — MAX RETENTION DAYS CAP
    // ----------------------------------------------------------

    /// Admin sets the maximum retention days cap.
    ///
    /// This cap is enforced only at `create_engagement` time. Lowering it has
    /// no effect on existing engagements whose retention windows already
    /// exceed the new cap — their lifecycle (`unlock_milestone`,
    /// `propose_amendment`, etc.) re-reads stored per-engagement values, not
    /// this config.
    pub fn set_max_retention_days(env: Env, admin: Address, days: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::MaxRetentionDays, &days);
        env.events()
            .publish((Symbol::new(&env, "max_retention_days_set"),), days);
    }

    /// Return the current max retention days cap.
    pub fn get_max_retention_days(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MaxRetentionDays)
            .unwrap_or(DEFAULT_MAX_RETENTION_DAYS)
    }

    // ----------------------------------------------------------
    // ISSUE #21 — MAX MILESTONES CAP
    // ----------------------------------------------------------

    /// Admin sets the maximum milestone count cap.
    ///
    /// This cap is enforced only at `create_engagement` time. Lowering it has
    /// no effect on existing engagements that already have more milestones
    /// than the new cap — their lifecycle (`unlock_milestone`,
    /// `propose_amendment`, etc.) operates on the stored milestone list, not
    /// this config.
    pub fn set_max_milestones(env: Env, admin: Address, count: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::MaxMilestones, &count);
        env.events()
            .publish((Symbol::new(&env, "max_milestones_set"),), count);
    }

    /// Return the current maximum milestone count cap.
    pub fn get_max_milestones(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MaxMilestones)
            .unwrap_or(DEFAULT_MAX_MILESTONES)
    }

    // ----------------------------------------------------------
    // PER-COMPANY ACTIVE ENGAGEMENT CAP
    // ----------------------------------------------------------

    /// Admin sets the maximum number of simultaneously active engagements allowed
    /// per company address. Defaults to 50 when not explicitly configured.
    ///
    /// # Panics
    /// - `"unauthorized"` — caller is not the contract admin.
    /// - `"InvalidMaxActivePerCompany"` — `count` is 0.
    pub fn set_max_active_per_company(env: Env, admin: Address, count: u32) {
        Self::assert_admin(&env, &admin);
        if count == 0 {
            panic!("InvalidMaxActivePerCompany");
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxActivePerCompany, &count);
        env.events()
            .publish((Symbol::new(&env, "max_active_per_company_set"),), count);
    }

    /// Return the current per-company active engagement cap.
    /// Returns `DEFAULT_MAX_ACTIVE_PER_COMPANY` (50) when not configured.
    pub fn get_max_active_per_company(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MaxActivePerCompany)
            .unwrap_or(DEFAULT_MAX_ACTIVE_PER_COMPANY)
    }

    /// Return the current active engagement count for a company.
    pub fn get_company_active_count(env: Env, company: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::CompanyActiveCount(company))
            .unwrap_or(0u32)
    }

    // ----------------------------------------------------------
    // ISSUE #38 — INACTIVITY TIMEOUT
    // ----------------------------------------------------------

    /// Admin sets the inactivity timeout in ledgers.
    pub fn set_inactivity_timeout_ledgers(env: Env, admin: Address, ledgers: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::InactivityTimeoutLedgers, &ledgers);
        env.events()
            .publish((Symbol::new(&env, "inactivity_timeout_set"),), ledgers);
    }

    /// Return the current inactivity timeout in ledgers.
    pub fn get_inactivity_timeout_ledgers(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::InactivityTimeoutLedgers)
            .unwrap_or(DEFAULT_INACTIVITY_TIMEOUT_LEDGERS)
    }

    /// Mark an engagement as `Expired` and refund the remaining escrow to the company.
    /// This is a **permissionless keeper function** — any address may call it; no signature
    /// is required. It is designed to be triggered by off-chain bots or backend pollers.
    ///
    /// # Expiry Condition
    /// The engagement is eligible for expiry when:
    /// `env.ledger().sequence() - last_activity_ledger >= inactivity_timeout_ledgers`
    ///
    /// The default `inactivity_timeout_ledgers` is set at contract initialisation and can be
    /// updated by the admin via `set_inactivity_timeout_ledgers`.
    ///
    /// # Behaviour
    /// - The engagement must not be `Completed`.
    /// - Any escrow balance not yet released (`total_amount - released_amount`) is transferred
    ///   back to the company address.
    /// - The engagement status is set to `Expired`.
    ///
    /// # Panics
    /// - `"Cannot expire completed engagement"` — engagement is already `Completed`.
    /// - `"Inactivity timeout not reached"` — the inactivity threshold has not yet been reached.
    ///
    /// # Events
    /// Emits `("engagement_expired", engagement_id)` with the refund amount.
    pub fn expire_engagement(env: Env, engagement_id: String) {
        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status == EngagementStatus::Completed {
            panic!("Cannot expire completed engagement");
        }

        let timeout = Self::get_inactivity_timeout_ledgers(env.clone());
        let current_ledger = env.ledger().sequence();

        if current_ledger <= engagement.last_activity_ledger + timeout {
            panic!("Inactivity timeout not reached");
        }

        let refund = engagement.total_amount - engagement.released_amount;
        if refund > 0 {
            let token_client = token::Client::new(&env, &engagement.token);
            token_client.transfer(
                &env.current_contract_address(),
                &engagement.company,
                &refund,
            );
        }

        let old_engagement_status = engagement.status.clone();
        engagement.status = EngagementStatus::Expired;
        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        Self::decrement_company_active_count(&env, &engagement.company);

        env.events().publish(
            (
                Symbol::new(&env, "engagement_expired"),
                engagement_id.clone(),
            ),
            refund,
        );
    }

    // ----------------------------------------------------------
    // ISSUE #40 — STORAGE TTL EXTENSION
    // ----------------------------------------------------------

    /// Admin sets the storage TTL extension value in ledgers.
    pub fn set_storage_ttl_extend_to(env: Env, admin: Address, ledgers: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::StorageTtlExtendTo, &ledgers);
        env.events()
            .publish((Symbol::new(&env, "storage_ttl_extend_to_set"),), ledgers);
    }

    /// Return the current storage TTL extension value in ledgers.
    pub fn get_storage_ttl_extend_to(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::StorageTtlExtendTo)
            .unwrap_or(DEFAULT_STORAGE_TTL_EXTEND_TO)
    }

    /// Helper to extend TTL for engagement storage.
    fn extend_engagement_ttl(env: &Env, engagement_id: &String) {
        let extend_to = Self::get_storage_ttl_extend_to(env.clone());
        env.storage().persistent().extend_ttl(
            &DataKey::Engagement(engagement_id.clone()),
            100_000,
            extend_to,
        );
    }

    fn emit_milestone_status_changed(
        env: &Env,
        engagement_id: &String,
        milestone_index: u32,
        old_status: MilestoneStatus,
        new_status: MilestoneStatus,
    ) {
        if old_status != new_status {
            env.events().publish(
                (
                    Symbol::new(env, "milestone_status_changed"),
                    engagement_id.clone(),
                ),
                (milestone_index, old_status, new_status),
            );
        }
    }

    fn emit_engagement_status_changed(
        env: &Env,
        engagement_id: &String,
        old_status: EngagementStatus,
        new_status: EngagementStatus,
    ) {
        if old_status != new_status {
            env.events().publish(
                (Symbol::new(env, "status_changed"), engagement_id.clone()),
                (old_status, new_status),
            );
        }
    }

    // ----------------------------------------------------------
    // ISSUE #39 — BATCH CONFIRM MILESTONES
    // ----------------------------------------------------------

    /// Confirm multiple milestones in a single transaction, releasing payment for each.
    ///
    /// # Caller
    /// `company` — must match the engagement's company address and sign the transaction.
    ///
    /// # Atomicity
    /// All milestones in `milestone_indices` are validated **before** any payment is processed.
    /// If any milestone fails validation (e.g. proof not submitted, retention window not elapsed),
    /// the entire call panics and no funds are transferred.
    ///
    /// # Behaviour
    /// - For each index, releases `(total_amount * payment_percent / 100)` to the recruiter,
    ///   deducting the platform fee first.
    /// - Each confirmed milestone emits a `milestone_confirmed` event.
    /// - If all milestones across the engagement are now `Confirmed` or `Resolved`,
    ///   the engagement transitions to `Completed` and an `engagement_completed` event is emitted.
    ///
    /// # Panics
    /// - `"EmptyIndices"` — `milestone_indices` is empty.
    /// - `"engagement is not active"` — engagement status is not `Active`.
    /// - `"unauthorized"` — caller is not the engagement's company.
    /// - `"invalid milestone index"` — an index is out of bounds.
    /// - `"milestone proof not yet submitted"` — milestone is not in `ProofSubmitted` status.
    /// - `"retention window has not elapsed — cannot confirm yet"` — for `Retention` milestones
    ///   confirmed before their `valid_after_ledger`.
    ///
    /// # Events
    /// - `(\"milestone_confirmed\", engagement_id)` with `(index, payment)` — one per milestone.
    /// - `(\"platform_fee_collected\", engagement_id)` with `(index, fee_amount, treasury)` — when fee > 0.
    /// - `(\"engagement_completed\", engagement_id)` — emitted once if all milestones are done.
    pub fn batch_confirm_milestones(
        env: Env,
        company: Address,
        engagement_id: String,
        milestone_indices: Vec<u32>,
    ) {
        Self::assert_not_paused(&env);
        company.require_auth();

        if milestone_indices.is_empty() {
            panic!("EmptyIndices");
        }

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        if company != engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }

        // Validate all milestones first (atomic rejection)
        for i in 0..milestone_indices.len() {
            let idx = milestone_indices.get(i).unwrap();
            let m = Self::get_milestone_or_panic(&engagement, idx);
            if m.status != MilestoneStatus::ProofSubmitted {
                panic!("milestone proof not yet submitted");
            }
            if m.kind == MilestoneKind::Retention && env.ledger().sequence() < m.valid_after_ledger
            {
                panic!("retention window has not elapsed — cannot confirm yet");
            }
            // Issue #67/#184: enforce the same sequential-confirmation rule as
            // confirm_milestone — every prior milestone must already be done,
            // either from an earlier call or earlier in this same batch.
            for j in 0..idx {
                let prev = engagement.milestones.get(j).unwrap();
                let done_already = prev.status == MilestoneStatus::Confirmed
                    || prev.status == MilestoneStatus::Resolved;
                let done_in_batch =
                    (0..milestone_indices.len()).any(|k| milestone_indices.get(k).unwrap() == j);
                if !done_already && !done_in_batch {
                    panic!("PreviousMilestoneNotComplete");
                }
            }
        }

        let platform_fee = Self::get_platform_fee_internal(&env);
        let token_client = token::Client::new(&env, &engagement.token);

        for i in 0..milestone_indices.len() {
            let idx = milestone_indices.get(i).unwrap();
            let mut m = engagement.milestones.get(idx).unwrap();

            let payment = (engagement.total_amount * m.payment_percent as i128) / 100;
            let fee_amount = (payment * platform_fee.bps as i128) / 10_000;
            let net_payment = payment - fee_amount;
            engagement.released_amount += payment;

            if fee_amount > 0 {
                token_client.transfer(
                    &env.current_contract_address(),
                    &platform_fee.treasury,
                    &fee_amount,
                );
                env.events().publish(
                    (
                        Symbol::new(&env, "platform_fee_collected"),
                        engagement_id.clone(),
                    ),
                    (idx, fee_amount, platform_fee.treasury.clone()),
                );
            }
            Self::distribute_recruiter_payout(&env, &engagement, net_payment, &token_client);

            let old_status = m.status.clone();
            m.status = MilestoneStatus::Confirmed;
            engagement.milestones.set(idx, m);

            Self::emit_milestone_status_changed(
                &env,
                &engagement_id,
                idx,
                old_status,
                MilestoneStatus::Confirmed,
            );

            env.events().publish(
                (
                    Symbol::new(&env, "milestone_confirmed"),
                    engagement_id.clone(),
                ),
                (idx, payment),
            );
        }

        let all_done = (0..engagement.milestones.len()).all(|i| {
            let s = engagement.milestones.get(i).unwrap().status;
            s == MilestoneStatus::Confirmed || s == MilestoneStatus::Resolved
        });

        let old_engagement_status = engagement.status.clone();
        if all_done {
            engagement.status = EngagementStatus::Completed;
            Self::decrement_company_active_count(&env, &engagement.company);
        }
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        if all_done {
            env.events().publish(
                (
                    Symbol::new(&env, "engagement_completed"),
                    engagement_id.clone(),
                ),
                (
                    engagement_id.clone(),
                    engagement.released_amount,
                    env.ledger().sequence(),
                ),
            );
        }
    }

    // ----------------------------------------------------------
    // ISSUE #50 — DISPUTE REASON QUERY
    // ----------------------------------------------------------

    /// Return the stored dispute reason for a milestone, if any.
    pub fn get_dispute_reason(
        env: Env,
        engagement_id: String,
        milestone_index: u32,
    ) -> Option<String> {
        env.storage()
            .persistent()
            .get(&DataKey::DisputeReason(engagement_id, milestone_index))
    }

    // ----------------------------------------------------------
    // CONFIRM WINDOW — AUTO-CONFIRM AFTER INACTION
    // ----------------------------------------------------------

    /// Admin sets the confirm window in ledgers.
    /// Default is 86_400 (~5 days at 5 s/ledger).
    pub fn set_confirm_window(env: Env, admin: Address, ledgers: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::ConfirmWindow, &ledgers);
        env.events()
            .publish((Symbol::new(&env, "confirm_window_set"),), ledgers);
    }

    /// Return the current confirm window in ledgers.
    pub fn get_confirm_window(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ConfirmWindow)
            .unwrap_or(DEFAULT_CONFIRM_WINDOW_LEDGERS)
    }

    // ----------------------------------------------------------
    // DISPUTE WINDOW
    // ----------------------------------------------------------

    /// Admin sets the dispute window in ledgers.
    /// Default is 51_840 (~3 days at 5 s/ledger).
    pub fn set_dispute_window(env: Env, admin: Address, ledgers: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::DisputeWindow, &ledgers);
        env.events()
            .publish((Symbol::new(&env, "dispute_window_set"),), ledgers);
    }

    /// Return the current dispute window in ledgers.
    pub fn get_dispute_window(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::DisputeWindow)
            .unwrap_or(DEFAULT_DISPUTE_WINDOW_LEDGERS)
    }

    /// Force-confirm a milestone after the company has taken no action within the
    /// configured confirm window.  Callable by anyone once the window has elapsed.
    ///
    /// Succeeds only when:
    ///   - `current_ledger > proof_submitted_at + confirm_window`
    ///   - milestone status is exactly `ProofSubmitted`
    ///
    /// Releases payment to the recruiter (with platform fee) and emits
    /// `milestone_force_confirmed`.
    pub fn force_confirm_milestone(
        env: Env,
        caller: Address,
        engagement_id: String,
        milestone_index: u32,
    ) {
        Self::assert_not_paused(&env);
        caller.require_auth();

        let mut engagement = Self::get_engagement_internal(&env, &engagement_id);

        if engagement.status != EngagementStatus::Active {
            panic!("{}", ERR_ENGAGEMENT_NOT_ACTIVE);
        }

        let mut milestone = Self::get_milestone_or_panic(&engagement, milestone_index);

        if milestone.status != MilestoneStatus::ProofSubmitted {
            panic!("milestone is not in ProofSubmitted status");
        }

        let current_ledger = env.ledger().sequence();
        let window = env
            .storage()
            .instance()
            .get(&DataKey::ConfirmWindow)
            .unwrap_or(DEFAULT_CONFIRM_WINDOW_LEDGERS);

        if current_ledger <= milestone.proof_submitted_at + window {
            panic!("ConfirmWindowNotElapsed");
        }

        // Release payment identically to confirm_milestone.
        let payment = (engagement.total_amount * milestone.payment_percent as i128) / 100;
        let platform_fee = Self::get_platform_fee_internal(&env);
        let fee_amount = (payment * platform_fee.bps as i128) / 10_000;
        let net_payment = payment - fee_amount;
        engagement.released_amount += payment;

        let token_client = token::Client::new(&env, &engagement.token);
        if fee_amount > 0 {
            token_client.transfer(
                &env.current_contract_address(),
                &platform_fee.treasury,
                &fee_amount,
            );
            env.events().publish(
                (
                    Symbol::new(&env, "platform_fee_collected"),
                    engagement_id.clone(),
                ),
                (milestone_index, fee_amount, platform_fee.treasury),
            );
        }
        Self::distribute_recruiter_payout(&env, &engagement, net_payment, &token_client);

        let old_status = milestone.status.clone();
        milestone.status = MilestoneStatus::Confirmed;
        engagement.milestones.set(milestone_index, milestone);

        let all_done = (0..engagement.milestones.len()).all(|i| {
            let s = engagement.milestones.get(i).unwrap().status;
            s == MilestoneStatus::Confirmed || s == MilestoneStatus::Resolved
        });

        let old_engagement_status = engagement.status.clone();
        if all_done {
            engagement.status = EngagementStatus::Completed;
            Self::decrement_company_active_count(&env, &engagement.company);
        }
        engagement.last_activity_ledger = env.ledger().sequence();

        env.storage()
            .persistent()
            .set(&DataKey::Engagement(engagement_id.clone()), &engagement);
        Self::extend_engagement_ttl(&env, &engagement_id);
        Self::emit_engagement_status_changed(
            &env,
            &engagement_id,
            old_engagement_status,
            engagement.status.clone(),
        );

        Self::emit_milestone_status_changed(
            &env,
            &engagement_id,
            milestone_index,
            old_status,
            MilestoneStatus::Confirmed,
        );

        env.events().publish(
            (
                Symbol::new(&env, "milestone_force_confirmed"),
                engagement_id.clone(),
            ),
            (milestone_index, payment),
        );

        if all_done {
            env.events().publish(
                (
                    Symbol::new(&env, "engagement_completed"),
                    engagement_id.clone(),
                ),
                (
                    engagement_id.clone(),
                    engagement.released_amount,
                    env.ledger().sequence(),
                ),
            );
        }
    }

    // ----------------------------------------------------------
    // ISSUE #70 — ACTIVE DISPUTE COUNT
    // ----------------------------------------------------------

    /// Return the number of milestones currently in `Disputed` status.
    /// Read-only and permissionless. Returns 0 when no disputes are active.
    pub fn get_active_dispute_count(env: Env, engagement_id: String) -> u32 {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        let mut count: u32 = 0;
        for i in 0..engagement.milestones.len() {
            if engagement.milestones.get(i).unwrap().status == MilestoneStatus::Disputed {
                count += 1;
            }
        }
        count
    }

    // ----------------------------------------------------------
    // ISSUE #69 — CONTRACT UPGRADE MECHANISM
    // ----------------------------------------------------------

    /// Admin proposes a WASM upgrade with a mandatory time-lock (issue #69).
    /// The pending upgrade is stored and becomes executable after `lock_duration` ledgers.
    /// Default time-lock is 17,280 ledgers (~1 day); use `set_upgrade_lock_duration` to change it.
    /// Emits `upgrade_proposed` with `(new_wasm_hash, execute_after_ledger)`.
    ///
    /// # Repeated calls (issue #185)
    /// Calling `propose_upgrade` again while a previous proposal is still pending
    /// **overwrites** it and **resets the timelock countdown** — the new
    /// `execute_after_ledger` is computed from the current ledger, not the
    /// original proposal's. This is intentional: it lets the admin correct or
    /// retract a bad proposal (e.g. wrong wasm hash) without waiting out the
    /// original lock. The tradeoff is that a compromised admin key can grief
    /// legitimate upgrades by indefinitely re-proposing, or delay execution by
    /// repeatedly re-proposing the same hash — this is accepted as inherent to
    /// admin-key trust and is not separately mitigated here.
    pub fn propose_upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) {
        Self::assert_admin(&env, &admin);

        let lock_duration: u32 = env
            .storage()
            .instance()
            .get(&DataKey::UpgradeLockDuration)
            .unwrap_or(LEDGERS_PER_DAY);

        let execute_after_ledger = env.ledger().sequence() + lock_duration;

        let proposal = UpgradeProposal {
            new_wasm_hash: new_wasm_hash.clone(),
            execute_after_ledger,
        };

        env.storage()
            .instance()
            .set(&DataKey::PendingUpgrade, &proposal);

        env.events().publish(
            (Symbol::new(&env, "upgrade_proposed"),),
            (new_wasm_hash, execute_after_ledger),
        );
    }

    /// Execute a pending upgrade after the time-lock has elapsed (issue #69).
    /// Permissionless — anyone may call this once `execute_after_ledger` is reached.
    /// Panics with `"no pending upgrade"` if no proposal exists.
    /// Panics with `"UpgradeLockNotElapsed"` if called before the time-lock expires.
    /// Emits `upgrade_executed` before applying the WASM swap.
    pub fn execute_upgrade(env: Env) {
        let proposal: UpgradeProposal = env
            .storage()
            .instance()
            .get(&DataKey::PendingUpgrade)
            .unwrap_or_else(|| panic!("no pending upgrade"));

        if env.ledger().sequence() < proposal.execute_after_ledger {
            panic!("UpgradeLockNotElapsed");
        }

        env.storage().instance().remove(&DataKey::PendingUpgrade);

        env.events().publish(
            (Symbol::new(&env, "upgrade_executed"),),
            proposal.new_wasm_hash.clone(),
        );

        env.deployer()
            .update_current_contract_wasm(proposal.new_wasm_hash);
    }

    /// Admin sets the upgrade time-lock duration in ledgers (issue #69).
    pub fn set_upgrade_lock_duration(env: Env, admin: Address, ledgers: u32) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::UpgradeLockDuration, &ledgers);
        env.events()
            .publish((Symbol::new(&env, "upgrade_lock_duration_set"),), ledgers);
    }

    /// Return the current upgrade time-lock duration in ledgers (issue #69).
    /// Defaults to 17,280 (~1 day).
    pub fn get_upgrade_lock_duration(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::UpgradeLockDuration)
            .unwrap_or(LEDGERS_PER_DAY)
    }

    // ----------------------------------------------------------
    // ISSUE #68 — ADMIN-CONFIGURABLE MAX PROOF HASH LENGTH
    // ----------------------------------------------------------

    /// Admin sets the maximum proof hash length in characters (issue #68).
    /// `len` must be in the range 1–500. Panics with `"InvalidMaxProofHashLength"` otherwise.
    pub fn set_max_proof_hash_length(env: Env, admin: Address, len: u32) {
        Self::assert_admin(&env, &admin);
        if len < 1 || len > 500 {
            panic!("InvalidMaxProofHashLength");
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxProofHashLength, &len);
        env.events()
            .publish((Symbol::new(&env, "max_proof_hash_length_set"),), len);
    }

    /// Return the current max proof hash length (issue #68).
    /// Defaults to 200 when not configured.
    pub fn get_max_proof_hash_length(env: Env) -> u32 {
        Self::get_max_proof_hash_length_internal(&env)
    }

    // ----------------------------------------------------------
    // ISSUE #52 — ARBITER FEE
    // ----------------------------------------------------------

    /// Admin sets the arbiter fee in basis points (0–200, max 2%) (issue #52).
    /// Panics with "ArbiterFeeTooHigh" if bps > 200.
    pub fn set_arbiter_fee(env: Env, admin: Address, bps: u32) {
        Self::assert_admin(&env, &admin);
        if bps > MAX_ARBITER_FEE_BPS {
            panic!("ArbiterFeeTooHigh");
        }
        env.storage().instance().set(&DataKey::ArbiterFee, &bps);
        env.events()
            .publish((Symbol::new(&env, "arbiter_fee_set"),), bps);
    }

    /// Return the current arbiter fee in basis points (issue #52).
    /// Defaults to 0 when not configured.
    pub fn get_arbiter_fee(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ArbiterFee)
            .unwrap_or(0u32)
    }

    // ----------------------------------------------------------
    // ISSUE #55 — ENGAGEMENT COMPLETION QUERY
    // ----------------------------------------------------------

    /// Returns true if the engagement status is Completed, false for all other statuses.
    /// Read-only and permissionless (issue #55).
    pub fn get_is_engagement_complete(env: Env, engagement_id: String) -> bool {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        engagement.status == EngagementStatus::Completed
    }

    /// Return the fraction of milestones that are unlocked (not in Locked status).
    /// Returns `(unlocked_count, total_count)`.
    /// - `unlocked_count` = number of milestones with status != Locked
    /// - `total_count` = total number of milestones in the engagement
    ///
    /// Read-only and permissionless.
    pub fn get_unlock_progress(env: Env, engagement_id: String) -> (u32, u32) {
        let engagement = Self::get_engagement_internal(&env, &engagement_id);
        let total = engagement.milestones.len();
        let mut unlocked: u32 = 0;
        for i in 0..total {
            let m = engagement.milestones.get(i).unwrap();
            if m.status != MilestoneStatus::Locked {
                unlocked += 1;
            }
        }
        (unlocked, total)
    }

    // ----------------------------------------------------------
    // ISSUE #59 — ADMIN ROLE RENOUNCEMENT
    // ----------------------------------------------------------

    /// Permanently renounce admin privileges (issue #59).
    /// Sets the AdminRenounced flag so all admin-gated functions fail with "NoAdmin".
    /// Irreversible — once renounced, admin cannot be restored.
    /// Emits `admin_renounced` with `final_ledger`.
    pub fn renounce_admin(env: Env, admin: Address) {
        Self::assert_admin(&env, &admin);
        env.storage()
            .instance()
            .set(&DataKey::AdminRenounced, &true);
        // Clear any pending nomination so a stale nominee cannot claim_admin
        // after the role was supposed to be permanently renounced. See issue #182.
        env.storage().persistent().remove(&DataKey::PendingAdmin);
        let final_ledger = env.ledger().sequence();
        env.events()
            .publish((Symbol::new(&env, "admin_renounced"),), final_ledger);
    }

    // ----------------------------------------------------------
    // INTERNAL HELPERS
    // ----------------------------------------------------------

    fn get_engagement_internal(env: &Env, engagement_id: &String) -> Engagement {
        env.storage()
            .persistent()
            .get(&DataKey::Engagement(engagement_id.clone()))
            .unwrap_or_else(|| panic!("engagement not found"))
    }

    /// Shared "load milestone by index or panic" helper (issue #170) — mirrors
    /// `get_engagement_internal` for the milestone lookup, so every call site
    /// panics with the same `"invalid milestone index"` message instead of a
    /// bare Vec index-out-of-bounds panic.
    fn get_milestone_or_panic(engagement: &Engagement, milestone_index: u32) -> Milestone {
        engagement
            .milestones
            .get(milestone_index)
            .unwrap_or_else(|| panic!("{}", ERR_INVALID_MILESTONE_INDEX))
    }

    /// Shared precondition for the second step of the early-exit protocol —
    /// the engagement must be `ExitRequested` and `company` must match the
    /// engagement's company. Used by both `accept_early_exit` and
    /// `reject_early_exit` (issue #173).
    fn assert_exit_request_pending(engagement: &Engagement, company: &Address) {
        if engagement.status != EngagementStatus::ExitRequested {
            panic!("no exit request pending");
        }
        if company != &engagement.company {
            panic!("{}", ERR_UNAUTHORIZED);
        }
    }

    fn get_admin_internal(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic!("admin not initialized"))
    }

    fn assert_admin(env: &Env, admin: &Address) {
        let renounced: bool = env
            .storage()
            .instance()
            .get(&DataKey::AdminRenounced)
            .unwrap_or(false);
        if renounced {
            panic!("NoAdmin");
        }
        admin.require_auth();
        if *admin != Self::get_admin_internal(env) {
            panic!("{}", ERR_UNAUTHORIZED);
        }
    }

    fn is_paused_internal(env: &Env) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    fn assert_not_paused(env: &Env) {
        if Self::is_paused_internal(env) {
            panic!("ContractPaused");
        }
    }

    /// Terminal engagement states — no further state transitions are possible.
    fn is_terminal_status(status: &EngagementStatus) -> bool {
        matches!(
            status,
            EngagementStatus::Completed | EngagementStatus::Cancelled | EngagementStatus::Expired
        )
    }

    fn get_platform_fee_internal(env: &Env) -> PlatformFee {
        env.storage()
            .persistent()
            .get(&DataKey::PlatformFee)
            .unwrap_or_else(|| PlatformFee {
                bps: 0,
                treasury: Self::get_admin_internal(env),
            })
    }

    fn get_ledgers_per_day_internal(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::LedgersPerDay)
            .unwrap_or(LEDGERS_PER_DAY)
    }

    fn get_max_proof_hash_length_internal(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MaxProofHashLength)
            .unwrap_or(MAX_PROOF_HASH_LENGTH)
    }

    fn get_max_replacements_internal(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MaxReplacements)
            .unwrap_or(DEFAULT_MAX_REPLACEMENTS)
    }

    /// Decrement the per-company active engagement count, saturating at 0.
    /// Called whenever an engagement leaves the active pool (completed,
    /// cancelled, expired, etc).
    fn decrement_company_active_count(env: &Env, company: &Address) {
        let active_count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::CompanyActiveCount(company.clone()))
            .unwrap_or(0u32);
        env.storage().persistent().set(
            &DataKey::CompanyActiveCount(company.clone()),
            &active_count.saturating_sub(1),
        );
    }

    // ----------------------------------------------------------
    // ISSUE #56 — CO-RECRUITER SPLIT PAYOUT
    // ----------------------------------------------------------

    /// Distribute the net payment (after platform / arbiter fee) between the
    /// primary recruiter and the optional co-recruiter.
    ///
    /// When `co_recruiter` is `Some`, the primary receives
    /// `net * split_bps / 10_000` and the co-recruiter receives the remainder.
    /// When `co_recruiter` is `None` the full net amount goes to the recruiter.
    fn distribute_recruiter_payout(
        env: &Env,
        engagement: &Engagement,
        net_payment: i128,
        token_client: &token::Client,
    ) {
        match &engagement.co_recruiter {
            Some(co_recruiter) => {
                let split = engagement.recruiter_split_bps as i128;
                let primary_payment = (net_payment * split) / (FULL_SPLIT_BPS as i128);
                let co_payment = net_payment - primary_payment;
                token_client.transfer(
                    &env.current_contract_address(),
                    &engagement.recruiter,
                    &primary_payment,
                );
                token_client.transfer(&env.current_contract_address(), co_recruiter, &co_payment);
            }
            None => {
                token_client.transfer(
                    &env.current_contract_address(),
                    &engagement.recruiter,
                    &net_payment,
                );
            }
        }
    }
}

mod test;
