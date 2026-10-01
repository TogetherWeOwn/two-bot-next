//! Member moderation handlers: `/ban` `/tempban` `/kick` `/timeout` `/warn`
//! plus the tempban unban sweep (TOG-10078, S4 member slice).
//!
//! Framework-free domain logic on top of the merged moderation shapes +
//! hierarchy/protected-role policy (`moderation.rs`, slice 3). Inputs are
//! plain data, outcomes are plain data ([`MemberResult`], [`DiscordCall`]);
//! the interaction router (TOG-10075) feeds executions in and the REST
//! executor (TOG-10076) carries the [`DiscordCall`]s out, so this module never
//! touches twilight or HTTP. Storage lives behind [`MemberModerationStore`]
//! (the sqlx implementation is `member_moderation_store` behind
//! the `db` feature; tests use [`MemMemberStore`]).
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - service: `src/moderation/service.ts` (`ModerationService.execute`,
//!   `carryOut`, `runDueUnbans`) — member verbs only; purge/slowmode/
//!   lockdown/unlock belong to the channel slice (TOG-10079).
//! - validation: `src/moderation/actions.ts` (`runModerationAction`) +
//!   `src/moderation/types.ts` (`requireModerationReason`).
//! - idempotency/unbans/warnings/audit: `src/moderation/store.ts`.
//! - Discord calls: `src/moderation/discord.ts` (`ModerationDiscordClient`
//!   member methods; default 5 s abort).
//!
//! Ordering guarantees:
//! - idempotency claims land BEFORE Discord; retries replay or refuse.
//! - both ban kinds persist a prepared generation BEFORE Discord. Accepted
//!   generations fence older expiries; definite refusals restore eligibility.
//! - only durable acceptance permits staged recovery. Prepared/uncertain
//!   bans require reconciliation, never a guessed unban of an existing ban.
//! - sweeps claim atomically, one job immediately before processing, and only
//!   within their owning guild. Running claims have no timer takeover.
//!
//! Staging gate: serving requires [`crate::moderation::ModerationGates`] enabled
//! (`TWO_MODERATION=1`); this module carries no gate check itself — the
//! router slice decides which slices to serve, same posture as slice 3.
//!
//! Deliberately out of scope: the moderation-audit MAC marker (S5 mints and
//! trusts it; the audit-log reason here is the plain moderator reason),
//! the operational-audit refusal/success mirror (S5 `AuditSink`), purge /
//! slowmode / lockdown / unlock (TOG-10079), and the 30 s sweep scheduler —
//! [`UNBAN_SWEEP_INTERVAL_SECONDS`] names the cadence the bot-crate ticker
//! drives once the router/executor slices land.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use sha2::{Digest, Sha256};

use crate::funnel::format_iso_millis;
use crate::moderation::{
    assert_moderation_allowed, require_moderation_reason, ModerationAction, ModerationActor,
    ModerationPolicy, ModerationRequest, ModerationTarget, PolicyError, ReasonError,
};

// --- bounds -----------------------------------------------------------------

/// Minimum tempban/timeout duration in seconds (legacy: option `min_value`
/// 60 on both `duration_seconds` inputs, parity §1 #4/#6).
pub const MIN_DURATION_SECONDS: i64 = 60;
/// Maximum tempban duration: one year (legacy `integerBetween` upper bound
/// `365 * 24 * 60 * 60` for `moderation.tempban`).
pub const MAX_TEMPBAN_SECONDS: i64 = 365 * 24 * 60 * 60;
/// Maximum timeout duration: 28 days, Discord's hard ceiling (legacy
/// `MAX_TIMEOUT_SECONDS`).
pub const MAX_TIMEOUT_SECONDS: i64 = 28 * 24 * 60 * 60;
/// Moderation unban sweep cadence in seconds (parity §4:
/// `moderation_scheduled_unbans`, 30 s). The scheduler lives with the
/// router/executor wiring; this constant keeps the cadence in one place.
pub const UNBAN_SWEEP_INTERVAL_SECONDS: u64 = 30;
/// How many due unban jobs one sweep claims at most (legacy
/// `claimDueUnbans(limit = 25)`).
pub const UNBAN_SWEEP_CLAIM_LIMIT: i64 = 25;

// --- outcomes ---------------------------------------------------------------

/// Terminal outcome of one member moderation verb (legacy `outcome`
/// strings: `banned`, `temporarily_banned`, `kicked`, `timed_out`,
/// `warned`; plus `unbanned` for the sweep).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberOutcome {
    Banned,
    TemporarilyBanned,
    Kicked,
    TimedOut,
    Warned,
    Unbanned,
}

impl MemberOutcome {
    /// Legacy outcome string stored in `moderation_audit` / `result_json`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Banned => "banned",
            Self::TemporarilyBanned => "temporarily_banned",
            Self::Kicked => "kicked",
            Self::TimedOut => "timed_out",
            Self::Warned => "warned",
            Self::Unbanned => "unbanned",
        }
    }

    /// Parse a stored outcome string back (idempotency replay path).
    #[must_use]
    pub fn from_stored(s: &str) -> Option<Self> {
        match s {
            "banned" => Some(Self::Banned),
            "temporarily_banned" => Some(Self::TemporarilyBanned),
            "kicked" => Some(Self::Kicked),
            "timed_out" => Some(Self::TimedOut),
            "warned" => Some(Self::Warned),
            "unbanned" => Some(Self::Unbanned),
            _ => None,
        }
    }
}

/// What `execute` reports: the outcome plus whether it replayed a stored
/// result instead of acting (legacy `replayed: true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberResult {
    pub outcome: MemberOutcome,
    pub replayed: bool,
}

// --- Discord contract -------------------------------------------------------

/// One Discord mutation the executor must carry out (legacy
/// `ModerationDiscordClient` member methods). The REST executor (TOG-10076)
/// implements [`MemberDiscord`] over twilight with the legacy 5 s abort per
/// call and no auto-retry; this enum is the exact handoff surface so the
/// follow-up wiring commit needs no domain change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscordCall {
    Ban {
        guild_id: String,
        user_id: String,
        reason: String,
    },
    Unban {
        guild_id: String,
        user_id: String,
        reason: String,
    },
    Kick {
        guild_id: String,
        user_id: String,
        reason: String,
    },
    Timeout {
        guild_id: String,
        user_id: String,
        until_iso: String,
        reason: String,
    },
}

/// Discord failure modes (legacy `ModerationDiscord` `ActionError` codes).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscordError {
    /// Discord refused the request (4xx): provably no mutation happened, so
    /// the claim is safe to release and the staged unban safe to cancel.
    /// This is the ONLY retry-safe failure (legacy `discord_rejected`).
    #[error("discord refused the request: {0}")]
    Rejected(String),
    /// Discord did not answer in time (legacy `upstream_timeout`): the
    /// mutation is uncertain, so the claim stays `in_flight` and the unban
    /// stays `running` — never released, never guessed.
    #[error("discord did not answer in time")]
    Timeout,
    /// Transport/5xx failure (legacy `discord_unavailable`): uncertain, same
    /// treatment as [`DiscordError::Timeout`].
    #[error("discord was unreachable: {0}")]
    Unavailable(String),
    /// Rate limited (legacy `rate_limited`): uncertain, same treatment as
    /// [`DiscordError::Timeout`]; the executor paces per legacy §6.
    #[error("discord rate-limited this request")]
    RateLimited,
}

impl DiscordError {
    /// True only for failures that prove no mutation happened (legacy
    /// `isSafePreMutationFailure`).
    #[must_use]
    pub fn is_safe_pre_mutation(&self) -> bool {
        matches!(self, Self::Rejected(_))
    }
}

/// The Discord side-effect seam (legacy `ModerationDiscordClient`, member
/// methods only). Implementations MUST abort each call after 5 s (legacy
/// `timeoutMs ?? 5000`) and MUST NOT auto-retry: retries replay through the
/// idempotency claim, never through a second Discord call.
pub trait MemberDiscord: Send + Sync {
    /// `PUT /guilds/{guild}/bans/{user}` (legacy accepts 200/204).
    fn ban(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> impl Future<Output = Result<(), DiscordError>> + Send;
    /// `DELETE /guilds/{guild}/bans/{user}` (legacy accepts 200/204/404 —
    /// unbanning a non-banned user still completes the job).
    fn unban(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> impl Future<Output = Result<(), DiscordError>> + Send;
    /// `DELETE /guilds/{guild}/members/{user}` (legacy accepts 200/204/404).
    fn kick(
        &self,
        guild_id: &str,
        user_id: &str,
        reason: &str,
    ) -> impl Future<Output = Result<(), DiscordError>> + Send;
    /// `PATCH /guilds/{guild}/members/{user}` with
    /// `communication_disabled_until` (legacy accepts 200).
    fn timeout(
        &self,
        guild_id: &str,
        user_id: &str,
        until_iso: &str,
        reason: &str,
    ) -> impl Future<Output = Result<(), DiscordError>> + Send;
}

// --- store contract ---------------------------------------------------------

/// Opaque store failure (the sqlx implementation maps `sqlx::Error` here so
/// core unit tests never need a Postgres driver).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("moderation store error: {message}")]
pub struct StoreError {
    pub message: String,
    /// True only when the failing operation provably persisted nothing for
    /// this request: a rolled-back staging transaction, or a writeless fence
    /// refusal. The idempotency key is then safe to release for a real second
    /// attempt. Ambiguous commits, post-dispatch failures and conflicting
    /// durable rows stay fenced (`false`).
    rolled_back: bool,
}

impl StoreError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            rolled_back: false,
        }
    }

    /// A failure whose transaction rolled back before any durable write for
    /// this request (or that wrote nothing at all): safe to retry, never
    /// fenced. Callers must use this only when no mutation could have
    /// persisted — never for ambiguous commits or post-dispatch failures.
    #[must_use]
    pub fn rolled_back(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            rolled_back: true,
        }
    }

    /// True only for failures that prove no mutation happened, so the claim
    /// is safe to release and the staged state safe to retry (the storage
    /// mirror of [`DiscordError::is_safe_pre_mutation`]).
    #[must_use]
    pub fn is_safe_pre_mutation(&self) -> bool {
        self.rolled_back
    }
}

/// What `claim` reports (legacy `ModerationClaim`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimState {
    /// First attempt won the key; proceed with the mutation.
    Claimed,
    /// The key already completed: replay this outcome, make no Discord call.
    Replayed { outcome: String },
    /// An earlier attempt is uncertain (crashed mid-flight or still
    /// running): refuse with `in_progress`, never take over — a timed
    /// takeover cannot distinguish a dead process from a slow Discord
    /// request, and taking over would permit two destructive mutations.
    InFlight,
    /// The key is bound to different request content: caller bug, refuse.
    Mismatch,
}

/// One row for the `moderation_audit` ledger (legacy `ModerationAuditRow`).
/// `channel_id` is always `None` on the member slice; metadata carries numbers
/// and reconciliation identities/evidence references, never raw response bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub request_id: String,
    pub guild_id: String,
    pub actor_id: String,
    pub action: &'static str,
    pub target_id: Option<String>,
    pub reason: String,
    pub outcome: &'static str,
    pub idempotency_key: String,
    /// Pre-rendered JSON object string (legacy `metadata_json`).
    pub metadata_json: String,
}

/// One claimed-due unban job (legacy `claimDueUnbans` rows).
#[derive(Clone, PartialEq, Eq)]
pub struct UnbanJob {
    pub request_id: String,
    pub claim_token: String,
    pub guild_id: String,
    pub user_id: String,
    pub reason: String,
}

impl std::fmt::Debug for UnbanJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnbanJob")
            .field("request_id", &self.request_id)
            .field("claim_token", &crate::Secret::new(&self.claim_token))
            .field("guild_id", &self.guild_id)
            .field("user_id", &self.user_id)
            .field("reason", &self.reason)
            .finish()
    }
}

/// Authoritative outcome for an uncertain dispatched unban (reconciliation
/// evidence comes from outside this crate — the runtime operator workflow —
/// never from age or from whether the member currently appears banned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnbanResolution {
    /// The dispatched DELETE provably landed: close the job as done.
    Completed,
    /// The dispatched DELETE provably cannot land: requeue a still-required
    /// accepted expiry, or supersede one replaced by a newer accepted ban.
    Void,
}

impl UnbanResolution {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Void => "void",
        }
    }
}

/// Identity of one durable PUT attempt, returned before dispatch. A rejected
/// request may be retried, but the retry gets a different generation. Keep this
/// identity with the operation's evidence; never relabel old evidence using a
/// lookup of the current request. Generation does not prove remote ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BanAttempt {
    pub generation: i64,
}

/// Authoritative evidence that the exact older PUT succeeded and finished
/// BEFORE this exact later accepted PUT. Only the trusted reconciliation
/// workflow may construct this assertion after examining durable evidence.
/// This crate checks identity and records the assertion, not external evidence
/// authenticity. Age, generations, and a current banned snapshot are NOT proof.
/// Evidence IDs are bounded, non-secret references, never raw responses/tokens.
#[derive(Debug, Clone)]
pub struct HistoricalBanAcceptance {
    pub later_request_id: String,
    pub later_attempt: BanAttempt,
    pub acceptance_evidence_id: String,
    pub ordering_evidence_id: String,
    pub actor_id: String,
}

impl HistoricalBanAcceptance {
    pub(crate) fn audit_row(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        attempt: BanAttempt,
    ) -> Result<AuditRow, StoreError> {
        let evidence_id = |id: &str| {
            !id.is_empty()
                && id.len() <= 160
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_:/.".contains(&c))
        };
        if !evidence_id(&self.acceptance_evidence_id)
            || !evidence_id(&self.ordering_evidence_id)
            || self.actor_id.is_empty()
            || self.actor_id.len() > 20
            || !self.actor_id.bytes().all(|c| c.is_ascii_digit())
            || request.is_empty()
            || request == self.later_request_id
            || attempt.generation <= 0
            || self.later_attempt.generation <= attempt.generation
        {
            return Err(StoreError::new(
                "exact historical acceptance and ordering evidence required",
            ));
        }
        let request_id = format!("historical-ban:{}:{request}", attempt.generation);
        Ok(AuditRow {
            idempotency_key: request_id.clone(),
            request_id,
            guild_id: guild.to_owned(),
            actor_id: self.actor_id.clone(),
            action: "ban_reconcile",
            target_id: Some(user.to_owned()),
            reason: "Authoritative acceptance before a later accepted PUT".into(),
            outcome: "accepted_historical",
            metadata_json: serde_json::json!({
                "request_id": request,
                "ban_attempt_generation": attempt.generation,
                "later_request_id": self.later_request_id,
                "later_ban_attempt_generation": self.later_attempt.generation,
                "acceptance_evidence_id": self.acceptance_evidence_id,
                "ordering_evidence_id": self.ordering_evidence_id,
            })
            .to_string(),
        })
    }
}

/// Persistence seam for the member slice (legacy `ModerationStore`,
/// member-slice methods only; lockdown methods belong to TOG-10079).
/// All timestamps are `YYYY-MM-DDTHH:MM:SS.sssZ` ISO strings, bound with
/// `::timestamptz` casts by the sqlx implementation (repo convention).
pub trait MemberModerationStore: Send + Sync {
    /// Run `run` holding this member's serial queue (legacy
    /// `serializeMember`): concurrent tempbans/unbans of one member execute
    /// in arrival order instead of interleaving stage/ban/activate steps.
    fn serialize_member<T, F, Fut>(
        &self,
        guild_id: &str,
        user_id: &str,
        run: F,
    ) -> impl Future<Output = T> + Send
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T> + Send;

    /// Claim `(guild, key)` for this request content, or report its last
    /// disposition (legacy `claim`: atomic insert-or-select).
    fn claim(
        &self,
        guild_id: &str,
        idempotency_key: &str,
        action: &str,
        request_hash: &str,
        claimed_at: &str,
    ) -> impl Future<Output = Result<ClaimState, StoreError>> + Send;

    /// Record the terminal result so a retry replays it (legacy `complete`).
    fn complete(
        &self,
        guild_id: &str,
        idempotency_key: &str,
        outcome: &str,
        result_json: &str,
        completed_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Give the key back after a provably mutation-free failure (legacy
    /// `release`: deletes `in_flight` rows only — a failed attempt made no
    /// lasting change, so a retry must be a real second attempt, not a
    /// cached error).
    fn release(
        &self,
        guild_id: &str,
        idempotency_key: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Append one `moderation_audit` row, ignoring request-id replays
    /// (legacy `recordAudit`: `ON CONFLICT (request_id) DO NOTHING`).
    fn record_audit(&self, row: &AuditRow) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Append one `moderation_warnings` row, ignoring request-id replays
    /// (legacy `addWarning`).
    #[allow(clippy::too_many_arguments)]
    fn add_warning(
        &self,
        warning_id: &str,
        guild_id: &str,
        user_id: &str,
        actor_id: &str,
        reason: &str,
        request_id: &str,
        created_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Persist a prepared permanent-ban intent BEFORE Discord, fencing any
    /// older expiry until this exact attempt is explicitly resolved. The
    /// returned identity must accompany the PUT and its outcome evidence.
    fn stage_ban(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        created_at: &str,
    ) -> impl Future<Output = Result<BanAttempt, StoreError>> + Send;

    /// Record observed acceptance of this exact PUT and supersede strictly
    /// older schedules atomically. Local order is not remote ordering proof.
    fn confirm_ban_attempt(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        attempt: BanAttempt,
        completed_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Resolve an older prepared PUT truthfully without claiming current
    /// ownership. Requires authoritative acceptance AND remote ordering proof
    /// bound to both attempts; unknown ordering must stay fenced. Atomically
    /// append a separate reconciliation audit, accept only this older intent,
    /// and supersede only its never-dispatched expiry. Newer rows and any
    /// dispatched DELETE evidence are untouched. Caller holds the member queue.
    #[allow(clippy::too_many_arguments)]
    fn resolve_historical_ban_acceptance(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        attempt: BanAttempt,
        evidence: &HistoricalBanAcceptance,
        completed_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Identity-only evidence cannot safely distinguish retried PUTs. Kept
    /// for source compatibility, but deliberately never mutates the ledger.
    fn confirm_ban(
        &self,
        _guild_id: &str,
        _user_id: &str,
        _request_id: &str,
        _completed_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        std::future::ready(Err(StoreError::new("exact ban attempt required")))
    }

    /// Persist a prepared intent and `staged` expiry together BEFORE Discord.
    /// A prepared intent is uncertain, never automatically activated.
    fn stage_unban(
        &self,
        guild_id: &str,
        user_id: &str,
        execute_at: &str,
        reason: &str,
        request_id: &str,
        created_at: &str,
    ) -> impl Future<Output = Result<BanAttempt, StoreError>> + Send;

    /// Activate only a confirmed accepted expiry which is still the newest
    /// non-rejected ban intent. Errors when that ownership or staging is lost.
    fn activate_staged_unban(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        completed_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Atomically reject this exact prepared PUT and cancel its staged expiry.
    /// If cleanup fails, the prepared fence survives: no guessed recovery.
    fn reject_ban_attempt(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        attempt: BanAttempt,
        completed_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Like identity-only confirmation, identity-only rejection fails closed.
    fn reject_ban(
        &self,
        _guild_id: &str,
        _user_id: &str,
        _request_id: &str,
        _completed_at: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send {
        std::future::ready(Err(StoreError::new("exact ban attempt required")))
    }

    /// Claim due accepted expiries in THIS guild only. Recover only staged
    /// rows with durable acceptance and current generation ownership. Takes
    /// each member queue internally and rechecks candidates before marking
    /// running; do not call from inside `serialize_member` (non-reentrant).
    /// Dispatchers request one job at a time so a cancelled first request
    /// cannot strand a batch of never-dispatched expiries. Running rows are
    /// never reclaimed by age; prepared bans require reconciliation.
    fn claim_due_unbans(
        &self,
        guild_id: &str,
        now: &str,
        limit: i64,
    ) -> impl Future<Output = Result<Vec<UnbanJob>, StoreError>> + Send;

    /// True while this caller still owns the claim (legacy
    /// `ownsUnbanClaim`: a claimed expiry may have been superseded by a
    /// newer tempban).
    fn owns_unban_claim(
        &self,
        request_id: &str,
        claim_token: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Mark the job `done` (legacy `completeUnban`): errors when the claim
    /// was lost, so a double-unban surfaces instead of vanishing.
    fn complete_unban(
        &self,
        request_id: &str,
        claim_token: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Close an uncertain dispatched unban with an authoritative outcome
    /// (`completed`: the DELETE provably landed; `void`: it provably cannot
    /// land). Running jobs and quarantined imports with durable dispatch
    /// uncertainty resolve by exact token; lost/already closed claims error.
    /// Voiding an imported dispatch leaves its untrusted expiry quarantined;
    /// it does not invent acceptance or cancel that unknown obligation. The
    /// evidence must name the exact guild/member/request; this method never
    /// guesses from age or current remote state. Staging refuses while any
    /// unresolved dispatch exists for the member; this lifts only that fence.
    fn resolve_uncertain_unban(
        &self,
        request_id: &str,
        claim_token: &str,
        resolution: UnbanResolution,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Give a safely-failed claim back as `pending` for the next sweep
    /// (legacy `requeueUnban`).
    fn requeue_unban(
        &self,
        request_id: &str,
        claim_token: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;
}

// --- execution input + validation -------------------------------------------

/// One member-moderation execution (legacy `ModerationExecution` restricted
/// to member verbs, plus the guild fence the adapter fills — same posture
/// as slice 3, which omits `guildId` from `ModerationRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberExecution {
    pub action: ModerationAction,
    pub guild_id: String,
    pub actor: ModerationActor,
    pub target: Option<ModerationTarget>,
    pub bot_highest_role_position: Option<i64>,
    /// Raw moderator reason; trimmed and capped by validation.
    pub reason: String,
    /// Raw `duration_seconds` for tempban/timeout (signed so the
    /// internal-actions path can report negatives as malformed, legacy
    /// `optionalInteger` + `integerBetween`).
    pub duration_seconds: Option<i64>,
    pub request_id: String,
    pub idempotency_key: String,
}

/// Validated member request: policy passed, reason normalised, duration
/// bounded per verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedMemberRequest {
    pub action: ModerationAction,
    pub reason: String,
    pub duration_seconds: Option<u64>,
}

/// Service failure (legacy `ActionError` codes carried per variant).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemberError {
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Reason(#[from] ReasonError),
    /// Malformed input (legacy `malformed`): non-member verb, missing or
    /// out-of-range `duration_seconds`.
    #[error("malformed {field}: {message}")]
    Malformed {
        field: &'static str,
        message: String,
    },
    /// An earlier attempt has an uncertain outcome (legacy `in_progress`,
    /// `moderation_idempotent_in_flight`).
    #[error("an earlier attempt at this moderation action has an uncertain outcome")]
    InFlight,
    /// The key is bound to different content (legacy `malformed`,
    /// `moderation_idempotency_key_reused`).
    #[error("this idempotency key was used for a different moderation request")]
    KeyMismatch,
    #[error(transparent)]
    Discord(#[from] DiscordError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Policy + reason + duration validation (legacy `validateRequest` on top
/// of `assertModerationAllowed`): permission first, then target presence,
/// self-moderation, target protection, bot hierarchy, actor hierarchy, then
/// the per-verb duration bounds.
pub fn validate_member_request(
    action: ModerationAction,
    policy: &ModerationPolicy,
    actor: &ModerationActor,
    target: Option<&ModerationTarget>,
    bot_highest_role_position: Option<i64>,
    reason: &str,
    duration_seconds: Option<i64>,
) -> Result<ValidatedMemberRequest, MemberError> {
    if !action.targets_member() {
        return Err(MemberError::Malformed {
            field: "action",
            message: format!(
                "{} is not a member moderation verb (this service handles ban, tempban, kick, timeout, warn)",
                action.action_name()
            ),
        });
    }
    // Policy sees no duration (it never constrains durations); the bounds
    // check below owns them, mirroring legacy's two-step order.
    assert_moderation_allowed(
        &ModerationRequest {
            action,
            actor: actor.clone(),
            target: target.cloned(),
            bot_highest_role_position,
            reason: reason.to_owned(),
            duration_seconds: None,
            count: None,
            seconds: None,
        },
        policy,
    )
    .map_err(MemberError::Policy)?;
    let reason = require_moderation_reason(reason).map_err(MemberError::Reason)?;
    let duration_seconds = match action {
        ModerationAction::TempBan => Some(bounded_duration(
            duration_seconds,
            MIN_DURATION_SECONDS,
            MAX_TEMPBAN_SECONDS,
        )?),
        ModerationAction::Timeout => Some(bounded_duration(
            duration_seconds,
            MIN_DURATION_SECONDS,
            MAX_TIMEOUT_SECONDS,
        )?),
        _ => None,
    };
    Ok(ValidatedMemberRequest {
        action,
        reason,
        duration_seconds,
    })
}

fn bounded_duration(value: Option<i64>, min: i64, max: i64) -> Result<u64, MemberError> {
    match value {
        Some(v) if (min..=max).contains(&v) => Ok(v as u64),
        _ => Err(MemberError::Malformed {
            field: "duration_seconds",
            message: format!("\"duration_seconds\" must be an integer between {min} and {max}"),
        }),
    }
}

/// Bind an idempotency key to one request content (legacy `hashOf`): action,
/// guild, target, reason and the bounded numbers are what make the request
/// what it is; the reason is included so a key reused for "spam" then
/// "harassment" against the same target is named as the caller bug it is.
#[must_use]
pub fn request_hash(
    action: ModerationAction,
    guild_id: &str,
    target_id: Option<&str>,
    reason: &str,
    duration_seconds: Option<u64>,
) -> String {
    let canonical = serde_json::json!({
        "action": action.action_name(),
        "guildId": guild_id,
        "targetId": target_id,
        "channelId": serde_json::Value::Null,
        "reason": reason,
        "durationSeconds": duration_seconds,
        "count": serde_json::Value::Null,
        "seconds": serde_json::Value::Null,
    });
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

// --- service ----------------------------------------------------------------

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Member moderation service (legacy `ModerationService`, member verbs).
/// `D` carries Discord mutations, `S` the durable rows, `F` the clock
/// (injectable for tests). Warnings never touch Discord; everything else
/// claims idempotency BEFORE the first Discord call.
pub struct MemberModerationService<D, S, F = fn() -> i64>
where
    F: Fn() -> i64 + Send + Sync,
{
    discord: D,
    store: S,
    policy: ModerationPolicy,
    now: F,
}

impl<D, S, F> MemberModerationService<D, S, F>
where
    D: MemberDiscord,
    S: MemberModerationStore,
    F: Fn() -> i64 + Send + Sync,
{
    pub fn new(discord: D, store: S, policy: ModerationPolicy, now: F) -> Self {
        Self {
            discord,
            store,
            policy,
            now,
        }
    }

    /// Execute one member moderation verb, exactly once per idempotency key.
    ///
    /// The durable row is the owner and the recovery record: once a request
    /// has reached Discord the row is never deleted or taken over on a
    /// timer, so a retry either replays a stored result or gets
    /// [`MemberError::InFlight`]. Both ban kinds serialize per member.
    #[expect(
        clippy::manual_async_fn,
        reason = "explicit Send avoids opaque callback lifetime inference at spawn sites (rust-lang/rust#100013)"
    )]
    pub fn execute<'a>(
        &'a self,
        exec: &'a MemberExecution,
    ) -> impl Future<Output = Result<MemberResult, MemberError>> + Send + 'a {
        async move {
            let validated = self.validate(exec)?;
            let hash = request_hash(
                validated.action,
                &exec.guild_id,
                exec.target.as_ref().map(|t| t.user_id.as_str()),
                &validated.reason,
                validated.duration_seconds,
            );
            if matches!(
                validated.action,
                ModerationAction::Ban | ModerationAction::TempBan
            ) {
                let target_id = exec
                    .target
                    .as_ref()
                    .map(|t| t.user_id.as_str())
                    .unwrap_or("");
                self.store
                    .serialize_member(&exec.guild_id, target_id, || {
                        self.execute_claimed(exec, &validated, &hash)
                    })
                    .await
            } else {
                self.execute_claimed(exec, &validated, &hash).await
            }
        }
    }

    /// Fire at most 25 due expiries for the owning guild. Claim immediately
    /// before each dispatch, not a whole batch before the first await. Stop
    /// on error: a definite rejection is requeued for the NEXT sweep, never
    /// retried automatically in this one. Uncertain running claims need
    /// reconciliation; other undispatched jobs stay pending.
    #[expect(
        clippy::manual_async_fn,
        reason = "explicit Send preserves spawnability through the generic queue callback"
    )]
    pub fn run_due_unbans<'a>(
        &'a self,
        guild_id: &'a str,
    ) -> impl Future<Output = Result<usize, MemberError>> + Send + 'a {
        async move {
            let now = format_iso_millis((self.now)());
            let mut completed = 0usize;
            for _ in 0..UNBAN_SWEEP_CLAIM_LIMIT {
                let Some(job) = self.store.claim_due_unbans(guild_id, &now, 1).await?.pop() else {
                    break;
                };
                match self.run_unban_job(&job).await {
                    Ok(true) => completed += 1,
                    Ok(false) => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(completed)
        }
    }

    // -- internals ----------------------------------------------------------

    fn validate(&self, exec: &MemberExecution) -> Result<ValidatedMemberRequest, MemberError> {
        validate_member_request(
            exec.action,
            &self.policy,
            &exec.actor,
            exec.target.as_ref(),
            exec.bot_highest_role_position,
            &exec.reason,
            exec.duration_seconds,
        )
    }

    async fn execute_claimed(
        &self,
        exec: &MemberExecution,
        validated: &ValidatedMemberRequest,
        hash: &str,
    ) -> Result<MemberResult, MemberError> {
        let now = format_iso_millis((self.now)());
        match self
            .store
            .claim(
                &exec.guild_id,
                &exec.idempotency_key,
                validated.action.action_name(),
                hash,
                &now,
            )
            .await?
        {
            ClaimState::Replayed { outcome } => {
                let outcome = MemberOutcome::from_stored(&outcome).ok_or(MemberError::Store(
                    StoreError::new(format!("stored unknown outcome: {outcome}")),
                ))?;
                return Ok(MemberResult {
                    outcome,
                    replayed: true,
                });
            }
            ClaimState::InFlight => return Err(MemberError::InFlight),
            ClaimState::Mismatch => return Err(MemberError::KeyMismatch),
            ClaimState::Claimed => {}
        }

        let outcome = match self.carry_out(exec, validated).await {
            Ok(outcome) => outcome,
            Err(err) => {
                let safe_pre_mutation = match &err {
                    // Discord refused before mutating: provably nothing
                    // happened, so the key is safe to give back.
                    MemberError::Discord(d) => d.is_safe_pre_mutation(),
                    // Storage rolled back before any durable write for this
                    // request (or wrote nothing): equally safe to retry.
                    MemberError::Store(s) => s.is_safe_pre_mutation(),
                    _ => false,
                };
                if safe_pre_mutation {
                    // A real second attempt, not a cached error. Release
                    // failures are ignored: the claim row is `in_flight`
                    // either way, so a retry stays `InFlight`, never a
                    // duplicate mutation.
                    let _ = self
                        .store
                        .release(&exec.guild_id, &exec.idempotency_key)
                        .await;
                }
                return Err(err);
            }
        };

        // Ban acceptance is audited at the PUT boundary, before confirmation
        // or expiry activation can fail. Other effects have no intervening
        // ownership writes, so audit their acceptance here before completion.
        if !matches!(
            validated.action,
            ModerationAction::Ban | ModerationAction::TempBan
        ) {
            self.audit_accepted(exec, validated, outcome, None).await;
        }
        self.store
            .complete(
                &exec.guild_id,
                &exec.idempotency_key,
                outcome.as_str(),
                &serde_json::json!({ "outcome": outcome.as_str() }).to_string(),
                &format_iso_millis((self.now)()),
            )
            .await?;
        Ok(MemberResult {
            outcome,
            replayed: false,
        })
    }

    async fn carry_out(
        &self,
        exec: &MemberExecution,
        validated: &ValidatedMemberRequest,
    ) -> Result<MemberOutcome, MemberError> {
        let target_id = exec
            .target
            .as_ref()
            .map(|t| t.user_id.as_str())
            .unwrap_or("");
        match validated.action {
            ModerationAction::Ban => {
                let attempt = self
                    .store
                    .stage_ban(
                        &exec.guild_id,
                        target_id,
                        &exec.request_id,
                        &format_iso_millis((self.now)()),
                    )
                    .await?;
                self.dispatch_ban(exec, target_id, validated, attempt)
                    .await?;
                Ok(MemberOutcome::Banned)
            }
            ModerationAction::TempBan => {
                let seconds = validated.duration_seconds.unwrap_or(60);
                let execute_at =
                    format_iso_millis((self.now)().saturating_add(seconds as i64 * 1000));
                // Prepared is not evidence of acceptance. Persist acceptance
                // separately before activation; only confirmed rows recover.
                let attempt = self
                    .store
                    .stage_unban(
                        &exec.guild_id,
                        target_id,
                        &execute_at,
                        &format!("Temporary ban expired: {}", validated.reason)
                            .chars()
                            .scan(0, |units, ch| {
                                *units += ch.len_utf16();
                                (*units <= 512).then_some(ch)
                            })
                            .collect::<String>(),
                        &exec.request_id,
                        &format_iso_millis((self.now)()),
                    )
                    .await?;
                self.dispatch_ban(exec, target_id, validated, attempt)
                    .await?;
                self.store
                    .activate_staged_unban(
                        &exec.guild_id,
                        target_id,
                        &exec.request_id,
                        &format_iso_millis((self.now)()),
                    )
                    .await?;
                Ok(MemberOutcome::TemporarilyBanned)
            }
            ModerationAction::Kick => {
                self.discord
                    .kick(&exec.guild_id, target_id, &validated.reason)
                    .await?;
                Ok(MemberOutcome::Kicked)
            }
            ModerationAction::Timeout => {
                let seconds = validated.duration_seconds.unwrap_or(60);
                let until = format_iso_millis((self.now)().saturating_add(seconds as i64 * 1000));
                self.discord
                    .timeout(&exec.guild_id, target_id, &until, &validated.reason)
                    .await?;
                Ok(MemberOutcome::TimedOut)
            }
            ModerationAction::Warn => {
                self.store
                    .add_warning(
                        &exec.request_id,
                        &exec.guild_id,
                        target_id,
                        &exec.actor.user_id,
                        &validated.reason,
                        &exec.request_id,
                        &format_iso_millis((self.now)()),
                    )
                    .await?;
                Ok(MemberOutcome::Warned)
            }
            other => Err(MemberError::Malformed {
                field: "action",
                message: format!("{} is handled by the channel slice", other.action_name()),
            }),
        }
    }

    async fn dispatch_ban(
        &self,
        exec: &MemberExecution,
        target: &str,
        validated: &ValidatedMemberRequest,
        attempt: BanAttempt,
    ) -> Result<(), MemberError> {
        if let Err(err) = self
            .discord
            .ban(&exec.guild_id, target, &validated.reason)
            .await
        {
            if err.is_safe_pre_mutation() {
                self.store
                    .reject_ban_attempt(
                        &exec.guild_id,
                        target,
                        &exec.request_id,
                        attempt,
                        &format_iso_millis((self.now)()),
                    )
                    .await?;
            }
            return Err(MemberError::Discord(err));
        }
        // Observe the accepted PUT independently of every later ledger write.
        // A failed confirmation/activation must retain the uncertain claim,
        // not skip this audit or repeat the destructive effect.
        let outcome = if validated.action == ModerationAction::TempBan {
            MemberOutcome::TemporarilyBanned
        } else {
            MemberOutcome::Banned
        };
        self.audit_accepted(exec, validated, outcome, Some(attempt))
            .await;
        self.store
            .confirm_ban_attempt(
                &exec.guild_id,
                target,
                &exec.request_id,
                attempt,
                &format_iso_millis((self.now)()),
            )
            .await?;
        Ok(())
    }

    async fn audit_accepted(
        &self,
        exec: &MemberExecution,
        validated: &ValidatedMemberRequest,
        outcome: MemberOutcome,
        attempt: Option<BanAttempt>,
    ) {
        if let Err(err) = self
            .store
            .record_audit(&self.audit_row(exec, validated, outcome, attempt))
            .await
        {
            tracing::error!(
                request_id = exec.request_id.as_str(),
                error = err.message.as_str(),
                "moderation_audit_failed"
            );
        }
    }

    fn audit_row(
        &self,
        exec: &MemberExecution,
        validated: &ValidatedMemberRequest,
        outcome: MemberOutcome,
        attempt: Option<BanAttempt>,
    ) -> AuditRow {
        let mut metadata = serde_json::json!({
            "duration_seconds": validated.duration_seconds,
        });
        if let Some(attempt) = attempt {
            metadata["ban_attempt_generation"] = serde_json::json!(attempt.generation);
        }
        AuditRow {
            request_id: exec.request_id.clone(),
            guild_id: exec.guild_id.clone(),
            actor_id: exec.actor.user_id.clone(),
            action: validated.action.action_name(),
            target_id: exec.target.as_ref().map(|t| t.user_id.clone()),
            reason: validated.reason.clone(),
            outcome: outcome.as_str(),
            idempotency_key: exec.idempotency_key.clone(),
            metadata_json: metadata.to_string(),
        }
    }

    async fn run_unban_job(&self, job: &UnbanJob) -> Result<bool, MemberError> {
        self.store
            .serialize_member(&job.guild_id, &job.user_id, || async {
                if !self
                    .store
                    .owns_unban_claim(&job.request_id, &job.claim_token)
                    .await?
                {
                    // Superseded by a newer tempban while claimed.
                    return Ok::<bool, MemberError>(false);
                }
                // No MAC marker without S5: the plain staged reason is the
                // audit-log reason (falls back to the pre-MAC behaviour).
                if let Err(err) = self
                    .discord
                    .unban(&job.guild_id, &job.user_id, &job.reason)
                    .await
                {
                    if err.is_safe_pre_mutation() {
                        self.store
                            .requeue_unban(&job.request_id, &job.claim_token)
                            .await?;
                    }
                    return Err(MemberError::Discord(err));
                }
                // The DELETE landed: persist the observed-success audit
                // independently of the completion write. If completion fails
                // afterward the claim stays `running` (uncertain, correct),
                // but the audit of the successful unban already exists.
                self.audit_unban_job(job).await;
                self.store
                    .complete_unban(&job.request_id, &job.claim_token)
                    .await?;
                Ok(true)
            })
            .await
    }

    /// Best-effort audit of a dispatched scheduled unban. Idempotent on the
    /// derived request id, so a later retry never duplicates it; loss is
    /// logged, never fatal, and never releases or repeats the DELETE.
    async fn audit_unban_job(&self, job: &UnbanJob) {
        let bot_id = self
            .policy
            .bot_user_id
            .clone()
            .unwrap_or_else(|| self.policy.owen_user_id.clone());
        if let Err(err) = self
            .store
            .record_audit(&AuditRow {
                request_id: format!("{}:unban", job.request_id),
                guild_id: job.guild_id.clone(),
                actor_id: bot_id,
                action: "moderation.unban_scheduled",
                target_id: Some(job.user_id.clone()),
                reason: job.reason.clone(),
                outcome: MemberOutcome::Unbanned.as_str(),
                idempotency_key: job.request_id.clone(),
                metadata_json: "{}".to_owned(),
            })
            .await
        {
            tracing::error!(
                request_id = job.request_id.as_str(),
                error = err.message.as_str(),
                "moderation_unban_audit_failed"
            );
        }
    }
}

impl<D, S> MemberModerationService<D, S, fn() -> i64>
where
    D: MemberDiscord,
    S: MemberModerationStore,
{
    /// Build with the wall clock (production constructor).
    pub fn with_system_clock(discord: D, store: S, policy: ModerationPolicy) -> Self {
        Self::new(discord, store, policy, now_millis as fn() -> i64)
    }
}

// --- in-memory doubles ------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct IdempotencyRow {
    action: String,
    hash: String,
    state: IdemState,
    outcome: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IdemState {
    InFlight,
    Done,
}

#[derive(Clone, PartialEq, Eq)]
struct UnbanRow {
    guild_id: String,
    user_id: String,
    execute_at: String,
    reason: String,
    state: UnbanState,
    created_at: String,
    completed_at: Option<String>,
    claim_token: Option<String>,
}

impl std::fmt::Debug for UnbanRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnbanRow")
            .field("guild_id", &self.guild_id)
            .field("user_id", &self.user_id)
            .field("execute_at", &self.execute_at)
            .field("reason", &self.reason)
            .field("state", &self.state)
            .field("created_at", &self.created_at)
            .field("completed_at", &self.completed_at)
            .field("claim_token", &crate::Secret::new(&self.claim_token))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnbanState {
    Staged,
    Pending,
    Running,
    Done,
    Cancelled,
    Superseded,
}

#[derive(Debug, Clone)]
struct BanRow {
    guild_id: String,
    user_id: String,
    generation: i64,
    state: BanState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BanState {
    Prepared,
    Accepted,
    Rejected,
}

#[derive(Debug, Default)]
struct MemInner {
    idempotency: HashMap<(String, String), IdempotencyRow>,
    audits: Vec<AuditRow>,
    warnings: Vec<(String, String, String, String, String, String)>,
    unbans: HashMap<String, UnbanRow>,
    bans: HashMap<String, BanRow>,
    ban_sequence: i64,
    claim_sequence: u64,
}

impl MemInner {
    // Local generations do not order unfinished remote PUTs or DELETEs.
    // Both directions of uncertainty fence new mutations for this member.
    fn refuse_uncertain_effects(&self, guild: &str, user: &str) -> Result<(), StoreError> {
        if self.unbans.values().any(|row| {
            row.guild_id == guild && row.user_id == user && row.state == UnbanState::Running
        }) || self.bans.values().any(|row| {
            row.guild_id == guild && row.user_id == user && row.state == BanState::Prepared
        }) {
            return Err(StoreError::rolled_back(
                "member has an uncertain ban or unban; resolve it before banning",
            ));
        }
        Ok(())
    }

    fn stage_ban(
        &mut self,
        guild: &str,
        user: &str,
        request: &str,
    ) -> Result<BanAttempt, StoreError> {
        self.refuse_uncertain_effects(guild, user)?;
        if self.bans.get(request).is_some_and(|row| {
            row.state != BanState::Rejected || row.guild_id != guild || row.user_id != user
        }) {
            // The competing non-rejected intent survives; nothing was
            // written for this request, so a retry under a fresh key is
            // safe — but never under this same conflicting key.
            return Err(StoreError::rolled_back("ban request id is already in use"));
        }
        let generation = self
            .ban_sequence
            .checked_add(1)
            .ok_or_else(|| StoreError::rolled_back("ban generation exhausted"))?;
        self.ban_sequence = generation;
        self.bans.insert(
            request.to_owned(),
            BanRow {
                guild_id: guild.to_owned(),
                user_id: user.to_owned(),
                generation: self.ban_sequence,
                state: BanState::Prepared,
            },
        );
        Ok(BanAttempt { generation })
    }

    fn current_ban(&self, request: &str) -> bool {
        let Some(row) = self.bans.get(request) else {
            return false;
        };
        row.state != BanState::Rejected
            && !self.bans.values().any(|other| {
                other.guild_id == row.guild_id
                    && other.user_id == row.user_id
                    && ((other.state == BanState::Prepared && other.generation != row.generation)
                        || (other.state != BanState::Rejected && other.generation > row.generation))
            })
    }

    fn accepted_current_ban(&self, request: &str) -> bool {
        self.current_ban(request)
            && self
                .bans
                .get(request)
                .is_some_and(|row| row.state == BanState::Accepted)
    }

    // Historical PUT acceptance does not resolve another in-flight DELETE.
    fn accepted_unfenced_unban(&self, request: &str) -> bool {
        self.accepted_current_ban(request)
            && self.unbans.get(request).is_some_and(|row| {
                !self.unbans.iter().any(|(id, other)| {
                    id != request
                        && other.guild_id == row.guild_id
                        && other.user_id == row.user_id
                        && other.state == UnbanState::Running
                })
            })
    }
}

/// Local FIFO queues, shared by store clones. Like the legacy service, one
/// moderation service owns a guild; these are not cross-process locks.
#[derive(Debug, Default, Clone)]
pub(crate) struct MemberQueues {
    locks: Arc<Mutex<MemberLocks>>,
}

type MemberLocks = HashMap<(String, String), Weak<tokio::sync::Mutex<()>>>;

impl MemberQueues {
    pub(crate) async fn run<T, F, Fut>(&self, guild: &str, user: &str, run: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T> + Send,
    {
        let lock = {
            let mut locks = self.locks.lock().expect("member queues lock");
            locks.retain(|_, lock| lock.strong_count() > 0);
            let entry = locks
                .entry((guild.to_owned(), user.to_owned()))
                .or_default();
            if let Some(lock) = entry.upgrade() {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                *entry = Arc::downgrade(&lock);
                lock
            }
        };
        let _guard = lock.lock().await;
        run().await
    }
}

/// In-memory persistence double. Method-level atomicity and member queues
/// model both durable claims and serialization across Discord awaits.
#[derive(Debug, Default, Clone)]
pub struct MemMemberStore {
    inner: Arc<Mutex<MemInner>>,
    queues: MemberQueues,
}

impl MemMemberStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, MemInner> {
        self.inner.lock().expect("mem moderation store lock")
    }

    /// All audit rows in insert order.
    pub fn audits(&self) -> Vec<AuditRow> {
        self.lock().audits.clone()
    }

    /// `(warning_id, guild, user, actor, reason, request_id)` in order.
    pub fn warnings(&self) -> Vec<(String, String, String, String, String, String)> {
        self.lock().warnings.clone()
    }

    /// Durable attempt identity (test introspection). This is a snapshot, not
    /// authoritative evidence of a PUT outcome or remote execution order.
    pub fn ban_attempt(&self, request_id: &str) -> Option<BanAttempt> {
        self.lock().bans.get(request_id).map(|row| BanAttempt {
            generation: row.generation,
        })
    }

    /// Current unban state per request id (test introspection).
    pub fn unban_state(&self, request_id: &str) -> Option<String> {
        self.lock().unbans.get(request_id).map(|r| {
            match r.state {
                UnbanState::Staged => "staged",
                UnbanState::Pending => "pending",
                UnbanState::Running => "running",
                UnbanState::Done => "done",
                UnbanState::Cancelled => "cancelled",
                UnbanState::Superseded => "superseded",
            }
            .to_owned()
        })
    }
}

impl MemberModerationStore for MemMemberStore {
    async fn serialize_member<T, F, Fut>(&self, guild_id: &str, user_id: &str, run: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T> + Send,
    {
        self.queues.run(guild_id, user_id, run).await
    }

    async fn claim(
        &self,
        guild_id: &str,
        idempotency_key: &str,
        action: &str,
        request_hash: &str,
        _claimed_at: &str,
    ) -> Result<ClaimState, StoreError> {
        let mut inner = self.lock();
        let key = (guild_id.to_owned(), idempotency_key.to_owned());
        if let Some(row) = inner.idempotency.get(&key) {
            if row.hash != request_hash {
                return Ok(ClaimState::Mismatch);
            }
            return Ok(match row.state {
                IdemState::Done => ClaimState::Replayed {
                    outcome: row.outcome.clone().unwrap_or_else(|| "unknown".to_owned()),
                },
                IdemState::InFlight => ClaimState::InFlight,
            });
        }
        inner.idempotency.insert(
            key,
            IdempotencyRow {
                action: action.to_owned(),
                hash: request_hash.to_owned(),
                state: IdemState::InFlight,
                outcome: None,
            },
        );
        Ok(ClaimState::Claimed)
    }

    async fn complete(
        &self,
        guild_id: &str,
        idempotency_key: &str,
        outcome: &str,
        _result_json: &str,
        _completed_at: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let row = inner
            .idempotency
            .get_mut(&(guild_id.to_owned(), idempotency_key.to_owned()))
            .filter(|row| row.state == IdemState::InFlight)
            .ok_or_else(|| StoreError::new("lost moderation claim"))?;
        row.state = IdemState::Done;
        row.outcome = Some(outcome.to_owned());
        Ok(())
    }

    async fn release(&self, guild_id: &str, idempotency_key: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let key = (guild_id.to_owned(), idempotency_key.to_owned());
        if inner
            .idempotency
            .get(&key)
            .is_some_and(|r| r.state == IdemState::InFlight)
        {
            inner.idempotency.remove(&key);
        }
        Ok(())
    }

    async fn record_audit(&self, row: &AuditRow) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if !inner.audits.iter().any(|r| r.request_id == row.request_id) {
            inner.audits.push(row.clone());
        }
        Ok(())
    }

    async fn add_warning(
        &self,
        warning_id: &str,
        guild_id: &str,
        user_id: &str,
        actor_id: &str,
        reason: &str,
        request_id: &str,
        _created_at: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if !inner.warnings.iter().any(|w| w.5 == request_id) {
            inner.warnings.push((
                warning_id.to_owned(),
                guild_id.to_owned(),
                user_id.to_owned(),
                actor_id.to_owned(),
                reason.to_owned(),
                request_id.to_owned(),
            ));
        }
        Ok(())
    }

    async fn stage_ban(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        _created_at: &str,
    ) -> Result<BanAttempt, StoreError> {
        self.lock().stage_ban(guild_id, user_id, request_id)
    }

    async fn confirm_ban_attempt(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        attempt: BanAttempt,
        completed_at: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let intent = inner
            .bans
            .get(request_id)
            .filter(|r| {
                r.guild_id == guild_id
                    && r.user_id == user_id
                    && r.state == BanState::Prepared
                    && r.generation == attempt.generation
            })
            .cloned()
            .ok_or_else(|| StoreError::new("lost prepared ban"))?;
        if !inner.current_ban(request_id) {
            return Err(StoreError::new("ban intent superseded"));
        }
        let old_ids: Vec<_> = inner
            .bans
            .iter()
            .filter(|(_, row)| {
                row.guild_id == guild_id
                    && row.user_id == user_id
                    && row.generation < intent.generation
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in old_ids {
            if let Some(row) = inner.unbans.get_mut(&id) {
                // Never-dispatched schedules supersede here; a `running` row
                // holds a dispatched DELETE that may still land, so only
                // authoritative `resolve_uncertain_unban` may close it.
                if matches!(row.state, UnbanState::Staged | UnbanState::Pending) {
                    row.state = UnbanState::Superseded;
                    row.completed_at = Some(completed_at.to_owned());
                    row.claim_token = None;
                }
            }
        }
        inner.bans.get_mut(request_id).expect("checked ban").state = BanState::Accepted;
        Ok(())
    }

    async fn resolve_historical_ban_acceptance(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        attempt: BanAttempt,
        evidence: &HistoricalBanAcceptance,
        completed_at: &str,
    ) -> Result<(), StoreError> {
        let audit = evidence.audit_row(guild_id, user_id, request_id, attempt)?;
        let mut inner = self.lock();
        let matches = |id: &str, expected: BanAttempt, state: BanState| {
            inner.bans.get(id).is_some_and(|row| {
                row.guild_id == guild_id
                    && row.user_id == user_id
                    && row.generation == expected.generation
                    && row.state == state
            })
        };
        if !matches(request_id, attempt, BanState::Prepared)
            || !matches(
                &evidence.later_request_id,
                evidence.later_attempt,
                BanState::Accepted,
            )
        {
            return Err(StoreError::new(
                "lost historical ban attempt or accepted successor",
            ));
        }
        // Unlike record_audit's replay behavior, conflicting reconciliation
        // evidence must fail before any ledger change.
        if inner
            .audits
            .iter()
            .any(|row| row.request_id == audit.request_id)
        {
            return Err(StoreError::new(
                "historical reconciliation audit already exists",
            ));
        }
        inner.audits.push(audit);
        inner.bans.get_mut(request_id).expect("checked ban").state = BanState::Accepted;
        if let Some(job) = inner.unbans.get_mut(request_id) {
            if job.guild_id == guild_id
                && job.user_id == user_id
                && matches!(job.state, UnbanState::Staged | UnbanState::Pending)
            {
                job.state = UnbanState::Superseded;
                job.completed_at = Some(completed_at.to_owned());
                job.claim_token = None;
            }
        }
        Ok(())
    }

    async fn stage_unban(
        &self,
        guild_id: &str,
        user_id: &str,
        execute_at: &str,
        reason: &str,
        request_id: &str,
        created_at: &str,
    ) -> Result<BanAttempt, StoreError> {
        let mut inner = self.lock();
        inner.refuse_uncertain_effects(guild_id, user_id)?;
        if inner.unbans.get(request_id).is_some_and(|row| {
            row.state != UnbanState::Cancelled || row.guild_id != guild_id || row.user_id != user_id
        }) {
            return Err(StoreError::rolled_back(
                "unban request id is already in use",
            ));
        }
        let attempt = inner.stage_ban(guild_id, user_id, request_id)?;
        inner.unbans.insert(
            request_id.to_owned(),
            UnbanRow {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                execute_at: execute_at.to_owned(),
                reason: reason.to_owned(),
                state: UnbanState::Staged,
                created_at: created_at.to_owned(),
                completed_at: None,
                claim_token: None,
            },
        );
        Ok(attempt)
    }

    async fn activate_staged_unban(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        completed_at: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if !inner.accepted_unfenced_unban(request_id)
            || !inner.unbans.get(request_id).is_some_and(|row| {
                row.state == UnbanState::Staged
                    && row.guild_id == guild_id
                    && row.user_id == user_id
            })
        {
            return Err(StoreError::new("lost accepted staged unban"));
        }
        let _ = completed_at;
        match inner.unbans.get_mut(request_id) {
            Some(row) if row.state == UnbanState::Staged => {
                row.state = UnbanState::Pending;
                Ok(())
            }
            _ => Err(StoreError::new(format!("lost staged unban: {request_id}"))),
        }
    }

    async fn reject_ban_attempt(
        &self,
        guild_id: &str,
        user_id: &str,
        request_id: &str,
        attempt: BanAttempt,
        completed_at: &str,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let intent = inner
            .bans
            .get_mut(request_id)
            .filter(|r| {
                r.state == BanState::Prepared
                    && r.guild_id == guild_id
                    && r.user_id == user_id
                    && r.generation == attempt.generation
            })
            .ok_or_else(|| StoreError::new("lost prepared ban"))?;
        intent.state = BanState::Rejected;
        if let Some(row) = inner.unbans.get_mut(request_id) {
            if row.state == UnbanState::Staged {
                row.state = UnbanState::Cancelled;
                row.completed_at = Some(completed_at.to_owned());
            }
        }
        Ok(())
    }

    async fn claim_due_unbans(
        &self,
        guild_id: &str,
        now: &str,
        limit: i64,
    ) -> Result<Vec<UnbanJob>, StoreError> {
        // Only durable acceptance is recoverable. Prepared/live/uncertain
        // rows never acquire an expiry by guessing whether Discord accepted.
        let mut staged: Vec<_> = {
            let inner = self.lock();
            inner
                .unbans
                .iter()
                .filter(|(id, r)| {
                    r.guild_id == guild_id
                        && r.state == UnbanState::Staged
                        && inner.accepted_unfenced_unban(id)
                })
                .map(|(id, r)| {
                    (
                        inner.bans[id].generation,
                        id.clone(),
                        r.guild_id.clone(),
                        r.user_id.clone(),
                    )
                })
                .collect()
        };
        staged.sort_by(|a, b| b.cmp(a));
        for (_, id, guild_id, user_id) in staged {
            self.serialize_member(&guild_id, &user_id, || async {
                let eligible = {
                    let inner = self.lock();
                    inner.accepted_unfenced_unban(&id)
                        && inner
                            .unbans
                            .get(&id)
                            .is_some_and(|r| r.state == UnbanState::Staged)
                };
                if eligible {
                    self.activate_staged_unban(&guild_id, &user_id, &id, now)
                        .await?;
                }
                Ok::<(), StoreError>(())
            })
            .await?;
        }
        let mut due: Vec<(String, UnbanRow)> = {
            let inner = self.lock();
            inner
                .unbans
                .iter()
                .filter(|(id, r)| {
                    r.guild_id == guild_id
                        && r.state == UnbanState::Pending
                        && r.execute_at.as_str() <= now
                        && inner.accepted_unfenced_unban(id)
                })
                .map(|(id, r)| (id.clone(), r.clone()))
                .collect()
        };
        due.sort_by(|a, b| {
            a.1.execute_at
                .cmp(&b.1.execute_at)
                .then_with(|| a.0.cmp(&b.0))
        });
        let mut jobs = Vec::new();
        for (id, row) in due.into_iter().take(limit.clamp(0, 25) as usize) {
            // Selection is advisory. Staging and dispatch ownership transition
            // share the same queue; recheck after waiting, before marking running.
            let job = self
                .serialize_member(&row.guild_id, &row.user_id, || async {
                    let mut inner = self.lock();
                    if !inner.accepted_unfenced_unban(&id)
                        || !inner.unbans.get(&id).is_some_and(|r| {
                            r.state == UnbanState::Pending && r.execute_at.as_str() <= now
                        })
                    {
                        return None;
                    }
                    inner.claim_sequence += 1;
                    let token = format!("mem-{}", inner.claim_sequence);
                    let stored = inner.unbans.get_mut(&id).expect("checked schedule");
                    stored.state = UnbanState::Running;
                    stored.claim_token = Some(token.clone());
                    Some(UnbanJob {
                        request_id: id.clone(),
                        claim_token: token,
                        guild_id: row.guild_id.clone(),
                        user_id: row.user_id.clone(),
                        reason: stored.reason.clone(),
                    })
                })
                .await;
            if let Some(job) = job {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    async fn owns_unban_claim(
        &self,
        request_id: &str,
        claim_token: &str,
    ) -> Result<bool, StoreError> {
        let inner = self.lock();
        Ok(inner.accepted_current_ban(request_id)
            && inner.unbans.get(request_id).is_some_and(|r| {
                r.state == UnbanState::Running && r.claim_token.as_deref() == Some(claim_token)
            }))
    }

    async fn complete_unban(&self, request_id: &str, claim_token: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if !inner.accepted_current_ban(request_id) {
            return Err(StoreError::new("unban intent superseded"));
        }
        match inner.unbans.get_mut(request_id) {
            Some(row)
                if row.state == UnbanState::Running
                    && row.claim_token.as_deref() == Some(claim_token) =>
            {
                row.state = UnbanState::Done;
                row.claim_token = None;
                Ok(())
            }
            _ => Err(StoreError::new(format!(
                "lost scheduled-unban claim: {request_id}"
            ))),
        }
    }

    async fn requeue_unban(&self, request_id: &str, claim_token: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        if let Some(row) = inner.unbans.get_mut(request_id) {
            if row.state == UnbanState::Running && row.claim_token.as_deref() == Some(claim_token) {
                row.state = UnbanState::Pending;
                row.claim_token = None;
            }
        }
        Ok(())
    }

    async fn resolve_uncertain_unban(
        &self,
        request_id: &str,
        claim_token: &str,
        resolution: UnbanResolution,
    ) -> Result<(), StoreError> {
        let mut inner = self.lock();
        // A void dispatch is not a void expiry obligation. Prepared newer
        // intents only fence it; they are not evidence that it was replaced.
        let expiry_required = inner.bans.get(request_id).is_some_and(|intent| {
            intent.state == BanState::Accepted
                && !inner.bans.values().any(|newer| {
                    newer.guild_id == intent.guild_id
                        && newer.user_id == intent.user_id
                        && newer.generation > intent.generation
                        && newer.state == BanState::Accepted
                })
        });
        match inner.unbans.get_mut(request_id) {
            Some(row)
                if row.state == UnbanState::Running
                    && row.claim_token.as_deref() == Some(claim_token) =>
            {
                row.state = match resolution {
                    UnbanResolution::Completed => UnbanState::Done,
                    UnbanResolution::Void if expiry_required => UnbanState::Pending,
                    UnbanResolution::Void => UnbanState::Superseded,
                };
                row.completed_at =
                    (row.state != UnbanState::Pending).then(|| "resolved".to_owned());
                row.claim_token = None;
                Ok(())
            }
            _ => Err(StoreError::new("lost uncertain scheduled-unban claim")),
        }
    }
}

/// Scripted [`MemberDiscord`] double: records every call, optionally fails
/// one method once (or always) with a fixed error.
#[derive(Debug, Default, Clone)]
pub struct MockMemberDiscord {
    inner: Arc<Mutex<MockInner>>,
}

#[derive(Debug, Default)]
struct MockInner {
    calls: Vec<DiscordCall>,
    fail: HashMap<&'static str, DiscordError>,
}

impl MockMemberDiscord {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail every call to `method` (`"ban"`, `"unban"`, `"kick"`,
    /// `"timeout"`) with `err` until [`MockMemberDiscord::clear_failure`]
    /// removes the entry.
    pub fn fail_with(&self, method: &'static str, err: DiscordError) {
        self.inner
            .lock()
            .expect("mock discord lock")
            .fail
            .insert(method, err);
    }

    /// Remove a scripted failure.
    pub fn clear_failure(&self, method: &'static str) {
        self.inner
            .lock()
            .expect("mock discord lock")
            .fail
            .remove(method);
    }

    /// Every Discord call attempted, in order.
    pub fn calls(&self) -> Vec<DiscordCall> {
        self.inner.lock().expect("mock discord lock").calls.clone()
    }

    /// How many times `method` was attempted.
    pub fn call_count(&self, method: &'static str) -> usize {
        self.inner
            .lock()
            .expect("mock discord lock")
            .calls
            .iter()
            .filter(|c| match c {
                DiscordCall::Ban { .. } => method == "ban",
                DiscordCall::Unban { .. } => method == "unban",
                DiscordCall::Kick { .. } => method == "kick",
                DiscordCall::Timeout { .. } => method == "timeout",
            })
            .count()
    }

    fn attempt(&self, method: &'static str, call: DiscordCall) -> Result<(), DiscordError> {
        let mut inner = self.inner.lock().expect("mock discord lock");
        inner.calls.push(call);
        inner.fail.get(method).cloned().map_or(Ok(()), Err)
    }
}

impl MemberDiscord for MockMemberDiscord {
    async fn ban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.attempt(
            "ban",
            DiscordCall::Ban {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                reason: reason.to_owned(),
            },
        )
    }

    async fn unban(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.attempt(
            "unban",
            DiscordCall::Unban {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                reason: reason.to_owned(),
            },
        )
    }

    async fn kick(&self, guild_id: &str, user_id: &str, reason: &str) -> Result<(), DiscordError> {
        self.attempt(
            "kick",
            DiscordCall::Kick {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                reason: reason.to_owned(),
            },
        )
    }

    async fn timeout(
        &self,
        guild_id: &str,
        user_id: &str,
        until_iso: &str,
        reason: &str,
    ) -> Result<(), DiscordError> {
        self.attempt(
            "timeout",
            DiscordCall::Timeout {
                guild_id: guild_id.to_owned(),
                user_id: user_id.to_owned(),
                until_iso: until_iso.to_owned(),
                reason: reason.to_owned(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const GUILD: &str = "100000000000000001";
    const OWEN_ID: &str = "123456789012345678";
    const ACTOR_ID: &str = "111111111111111111";
    const TARGET_ID: &str = "333333333333333333";
    const STAFF_ROLE: &str = "444444444444444444";
    const BOT_ID: &str = "555555555555555555";

    #[test]
    fn unban_debug_redacts_claim_tokens() {
        let token = "fixture-unban-claim-token";
        let job = UnbanJob {
            request_id: "request".into(),
            claim_token: token.into(),
            guild_id: GUILD.into(),
            user_id: TARGET_ID.into(),
            reason: "expiry".into(),
        };
        let row = UnbanRow {
            guild_id: GUILD.into(),
            user_id: TARGET_ID.into(),
            execute_at: "2023-11-14T23:13:20.000Z".into(),
            reason: "expiry".into(),
            state: UnbanState::Running,
            created_at: "2023-11-14T22:13:20.000Z".into(),
            completed_at: None,
            claim_token: Some(token.into()),
        };
        let inner = MemInner {
            unbans: HashMap::from([("request".into(), row.clone())]),
            ..MemInner::default()
        };
        for output in [
            format!("{job:?}"),
            format!("{job:#?}"),
            format!("{row:?}"),
            format!("{row:#?}"),
            format!("{inner:?}"),
            format!("{inner:#?}"),
        ] {
            assert!(!output.contains(token));
            assert!(output.contains("[REDACTED]"));
        }
        assert_eq!(job.claim_token, token);
        assert_eq!(row.claim_token.as_deref(), Some(token));
    }

    fn policy() -> ModerationPolicy {
        ModerationPolicy {
            owen_user_id: OWEN_ID.to_owned(),
            protected_role_ids: HashSet::from([STAFF_ROLE.to_owned()]),
            bot_user_id: Some(BOT_ID.to_owned()),
        }
    }

    fn actor() -> ModerationActor {
        ModerationActor {
            user_id: ACTOR_ID.to_owned(),
            role_ids: Vec::new(),
            highest_role_position: 50,
            permissions: u64::MAX,
        }
    }

    fn target() -> ModerationTarget {
        ModerationTarget {
            user_id: TARGET_ID.to_owned(),
            role_ids: Vec::new(),
            highest_role_position: 10,
            is_bot: false,
            is_guild_owner: false,
        }
    }

    fn execution(action: ModerationAction) -> MemberExecution {
        execution_with_id(
            action,
            &format!(
                "req-{}",
                action.action_name().trim_start_matches("moderation.")
            ),
        )
    }

    fn execution_with_id(action: ModerationAction, request_id: &str) -> MemberExecution {
        MemberExecution {
            action,
            guild_id: GUILD.to_owned(),
            actor: actor(),
            target: Some(target()),
            bot_highest_role_position: Some(100),
            reason: "spam in #general".to_owned(),
            duration_seconds: match action {
                ModerationAction::TempBan | ModerationAction::Timeout => Some(3600),
                _ => None,
            },
            request_id: request_id.to_owned(),
            idempotency_key: request_id.to_owned(),
        }
    }

    const NOW: &str = "2023-11-14T22:13:20.000Z";

    // Imported ownership may contain an older uncertain PUT plus a later
    // accepted one, a state ordinary staging deliberately cannot create.
    async fn historical_fixture(
        temporary: bool,
    ) -> (MemMemberStore, BanAttempt, HistoricalBanAcceptance) {
        let store = MemMemberStore::new();
        let older = store
            .stage_unban(GUILD, TARGET_ID, NOW, "old expiry", "older", NOW)
            .await
            .unwrap();
        let later = BanAttempt {
            generation: older.generation + 1,
        };
        {
            let mut inner = store.lock();
            inner.ban_sequence = later.generation;
            inner.bans.insert(
                "later".into(),
                BanRow {
                    guild_id: GUILD.into(),
                    user_id: TARGET_ID.into(),
                    generation: later.generation,
                    state: BanState::Accepted,
                },
            );
            if temporary {
                let mut expiry = inner.unbans["older"].clone();
                expiry.reason = "later expiry".into();
                inner.unbans.insert("later".into(), expiry);
            }
        }
        let evidence = HistoricalBanAcceptance {
            later_request_id: "later".into(),
            later_attempt: later,
            acceptance_evidence_id: "fixture:older-put-accepted".into(),
            ordering_evidence_id: "fixture:older-completed-before-later".into(),
            actor_id: ACTOR_ID.into(),
        };
        (store, older, evidence)
    }

    #[tokio::test]
    async fn historical_acceptance_clears_only_exact_older_uncertainty() {
        for temporary in [false, true] {
            let (store, older, evidence) = historical_fixture(temporary).await;
            let later_expiry = store.lock().unbans.get("later").cloned();
            assert!(store
                .confirm_ban_attempt(GUILD, TARGET_ID, "older", older, NOW)
                .await
                .is_err());
            assert!(store
                .claim_due_unbans(GUILD, NOW, 25)
                .await
                .unwrap()
                .is_empty());
            for bad in 0..5 {
                let mut proof = evidence.clone();
                let mut attempt = older;
                match bad {
                    0 => proof.ordering_evidence_id.clear(),
                    1 => proof.acceptance_evidence_id.clear(),
                    2 => proof.later_attempt.generation += 1,
                    3 => attempt.generation += 1,
                    _ => proof.later_request_id = "absent".into(),
                }
                assert!(store
                    .resolve_historical_ban_acceptance(
                        GUILD, TARGET_ID, "older", attempt, &proof, NOW
                    )
                    .await
                    .is_err());
                assert_eq!(store.lock().bans["older"].state, BanState::Prepared);
                assert_eq!(store.unban_state("older").as_deref(), Some("staged"));
                assert_eq!(store.lock().unbans.get("later").cloned(), later_expiry);
                assert!(store.audits().is_empty());
            }
            store
                .serialize_member(GUILD, TARGET_ID, || {
                    store.resolve_historical_ban_acceptance(
                        GUILD, TARGET_ID, "older", older, &evidence, NOW,
                    )
                })
                .await
                .unwrap();
            assert_eq!(store.lock().bans["older"].state, BanState::Accepted);
            assert!(!store.lock().accepted_current_ban("older"));
            assert!(store.lock().accepted_current_ban("later"));
            assert_eq!(store.unban_state("older").as_deref(), Some("superseded"));
            assert_eq!(store.lock().unbans.get("later").cloned(), later_expiry);
            let audits = store.audits();
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0].outcome, "accepted_historical");
            let metadata: serde_json::Value =
                serde_json::from_str(&audits[0].metadata_json).unwrap();
            assert_eq!(metadata["ban_attempt_generation"], older.generation);
            assert_eq!(
                metadata["ordering_evidence_id"],
                evidence.ordering_evidence_id
            );
            let jobs = store.claim_due_unbans(GUILD, NOW, 25).await.unwrap();
            assert_eq!(jobs.len(), usize::from(temporary));
            if temporary {
                assert_eq!(jobs[0].request_id, "later");
                store
                    .complete_unban("later", &jobs[0].claim_token)
                    .await
                    .unwrap();
            }
            assert!(store
                .stage_ban(GUILD, TARGET_ID, "fresh", NOW)
                .await
                .is_ok());
        }
    }

    #[tokio::test]
    async fn historical_acceptance_audit_failure_and_running_delete_stay_fenced() {
        let (store, older, evidence) = historical_fixture(true).await;
        let faulty = FaultyStore::wrap(store.clone());
        faulty
            .fail_audit_once
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(faulty
            .resolve_historical_ban_acceptance(GUILD, TARGET_ID, "older", older, &evidence, NOW)
            .await
            .is_err());
        assert_eq!(store.lock().bans["older"].state, BanState::Prepared);
        assert!(store.audits().is_empty());
        // A collision is not successful evidence recording, either.
        let collision = evidence
            .audit_row(GUILD, TARGET_ID, "older", older)
            .unwrap();
        store.record_audit(&collision).await.unwrap();
        assert!(store
            .resolve_historical_ban_acceptance(GUILD, TARGET_ID, "older", older, &evidence, NOW)
            .await
            .is_err());
        assert_eq!(store.lock().bans["older"].state, BanState::Prepared);
        store.lock().audits.clear();
        let before = {
            let mut inner = store.lock();
            let job = inner.unbans.get_mut("older").unwrap();
            job.state = UnbanState::Running;
            job.claim_token = Some("fixture-delete-claim".into());
            job.clone()
        };
        store
            .resolve_historical_ban_acceptance(GUILD, TARGET_ID, "older", older, &evidence, NOW)
            .await
            .unwrap();
        assert_eq!(store.lock().unbans["older"], before);
        assert!(store
            .stage_ban(GUILD, TARGET_ID, "fresh", NOW)
            .await
            .is_err());
        assert!(store
            .activate_staged_unban(GUILD, TARGET_ID, "later", NOW)
            .await
            .is_err());
        for state in [UnbanState::Staged, UnbanState::Pending] {
            let later_before = {
                let mut inner = store.lock();
                let job = inner.unbans.get_mut("later").unwrap();
                job.state = state;
                job.clone()
            };
            assert!(store
                .claim_due_unbans(GUILD, NOW, 25)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(store.lock().unbans["older"], before);
            assert_eq!(store.lock().unbans["later"], later_before);
        }
        // This member's uncertain DELETE must not block unrelated expiries.
        for (guild, user, request) in [
            (GUILD, "other-user", "other-user-expiry"),
            ("other-guild", TARGET_ID, "other-guild-expiry"),
        ] {
            let attempt = store
                .stage_unban(guild, user, NOW, "expiry", request, NOW)
                .await
                .unwrap();
            store
                .confirm_ban_attempt(guild, user, request, attempt, NOW)
                .await
                .unwrap();
            let jobs = store.claim_due_unbans(guild, NOW, 25).await.unwrap();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].request_id, request);
            assert_eq!(store.lock().unbans["older"], before);
        }
    }

    fn service(
        discord: MockMemberDiscord,
        store: MemMemberStore,
    ) -> MemberModerationService<MockMemberDiscord, MemMemberStore, fn() -> i64> {
        MemberModerationService::new(discord, store, policy(), || 1_700_000_000_000)
    }

    fn validated(action: ModerationAction) -> ValidatedMemberRequest {
        validate_member_request(
            action,
            &policy(),
            &actor(),
            Some(&target()),
            Some(100),
            "spam in #general",
            match action {
                ModerationAction::TempBan | ModerationAction::Timeout => Some(3600),
                _ => None,
            },
        )
        .expect("valid fixture")
    }

    /// Policy-layer verdict for one execution (panics when allowed).
    fn refuse(exec: &MemberExecution) -> MemberError {
        validate_member_request(
            exec.action,
            &policy(),
            &exec.actor,
            exec.target.as_ref(),
            exec.bot_highest_role_position,
            &exec.reason,
            exec.duration_seconds,
        )
        .expect_err("must refuse")
    }

    #[test]
    fn member_verbs_validate_but_channel_verbs_do_not() {
        for action in [
            ModerationAction::Ban,
            ModerationAction::TempBan,
            ModerationAction::Kick,
            ModerationAction::Timeout,
            ModerationAction::Warn,
        ] {
            assert!(
                validated(action).duration_seconds.is_some()
                    == matches!(
                        action,
                        ModerationAction::TempBan | ModerationAction::Timeout
                    )
            );
        }
        for action in [
            ModerationAction::Purge,
            ModerationAction::Slowmode,
            ModerationAction::Lockdown,
            ModerationAction::Unlock,
        ] {
            let err = validate_member_request(action, &policy(), &actor(), None, None, "x", None)
                .expect_err("must fail");
            assert!(matches!(err, MemberError::Malformed { .. }), "{action:?}");
        }
    }

    #[test]
    fn every_policy_refusal_surfaces_per_member_verb() {
        for action in [
            ModerationAction::Ban,
            ModerationAction::TempBan,
            ModerationAction::Kick,
            ModerationAction::Timeout,
            ModerationAction::Warn,
        ] {
            let base = || {
                let mut exec = execution(action);
                exec.request_id = format!("req-{action}-refusal");
                exec.idempotency_key = format!("key-{action}-refusal");
                exec
            };
            // Missing permission.
            let mut exec = base();
            exec.actor.permissions = 0;
            assert!(
                matches!(
                    refuse(&exec),
                    MemberError::Policy(PolicyError::ActorMissingPermission(_))
                ),
                "{action:?} permission"
            );
            // Missing target.
            let mut exec = base();
            exec.target = None;
            let err = refuse(&exec);
            assert_eq!(
                err,
                MemberError::Policy(PolicyError::MissingTarget),
                "{action:?}"
            );
            // Self-moderation.
            let mut exec = base();
            exec.target.as_mut().expect("target").user_id = ACTOR_ID.to_owned();
            let err = refuse(&exec);
            assert_eq!(
                err,
                MemberError::Policy(PolicyError::TargetSelf),
                "{action:?}"
            );
            // Guild owner.
            let mut exec = base();
            exec.target.as_mut().expect("target").is_guild_owner = true;
            let err = refuse(&exec);
            assert_eq!(
                err,
                MemberError::Policy(PolicyError::TargetGuildOwner),
                "{action:?}"
            );
            // Owen (by id and by bot id).
            for id in [OWEN_ID, BOT_ID] {
                let mut exec = base();
                exec.target.as_mut().expect("target").user_id = id.to_owned();
                let err = refuse(&exec);
                assert_eq!(
                    err,
                    MemberError::Policy(PolicyError::TargetOwen),
                    "{action:?} {id}"
                );
            }
            // Bots.
            let mut exec = base();
            exec.target.as_mut().expect("target").is_bot = true;
            let err = refuse(&exec);
            assert_eq!(
                err,
                MemberError::Policy(PolicyError::TargetBot),
                "{action:?}"
            );
            // Protected staff role.
            let mut exec = base();
            exec.target.as_mut().expect("target").role_ids = vec![STAFF_ROLE.to_owned()];
            let err = refuse(&exec);
            assert_eq!(
                err,
                MemberError::Policy(PolicyError::TargetStaffRole),
                "{action:?}"
            );
            // Bot hierarchy (equal included).
            let mut exec = base();
            exec.bot_highest_role_position = Some(10);
            let err = refuse(&exec);
            assert_eq!(
                err,
                MemberError::Policy(PolicyError::BotHierarchy),
                "{action:?}"
            );
            // Actor hierarchy (equal included).
            let mut exec = base();
            exec.target.as_mut().expect("target").highest_role_position = 50;
            let err = refuse(&exec);
            assert_eq!(
                err,
                MemberError::Policy(PolicyError::ActorHierarchy),
                "{action:?}"
            );
        }
    }

    #[test]
    fn reason_and_duration_bounds_match_legacy() {
        // Empty / overlong reasons.
        for reason in ["   ", &"x".repeat(513)] {
            let err = validate_member_request(
                ModerationAction::Ban,
                &policy(),
                &actor(),
                Some(&target()),
                Some(100),
                reason,
                None,
            )
            .expect_err("must fail");
            assert!(matches!(err, MemberError::Reason(_)), "{reason:?}");
        }
        // Reason is trimmed.
        let req = validate_member_request(
            ModerationAction::Ban,
            &policy(),
            &actor(),
            Some(&target()),
            Some(100),
            "  spam  ",
            None,
        )
        .expect("trims");
        assert_eq!(req.reason, "spam");
        // Duration required for tempban/timeout, bounded per verb.
        for (action, max) in [
            (ModerationAction::TempBan, MAX_TEMPBAN_SECONDS),
            (ModerationAction::Timeout, MAX_TIMEOUT_SECONDS),
        ] {
            for bad in [None, Some(59), Some(-5), Some(max + 1)] {
                let err = validate_member_request(
                    action,
                    &policy(),
                    &actor(),
                    Some(&target()),
                    Some(100),
                    "x",
                    bad,
                )
                .expect_err("must fail");
                assert!(
                    matches!(
                        err,
                        MemberError::Malformed {
                            field: "duration_seconds",
                            ..
                        }
                    ),
                    "{action:?} {bad:?}"
                );
            }
            for good in [Some(MIN_DURATION_SECONDS), Some(max)] {
                assert!(
                    validate_member_request(
                        action,
                        &policy(),
                        &actor(),
                        Some(&target()),
                        Some(100),
                        "x",
                        good,
                    )
                    .is_ok(),
                    "{action:?} {good:?}"
                );
            }
        }
    }

    #[test]
    fn request_hash_binds_content() {
        let a = request_hash(ModerationAction::Ban, GUILD, Some(TARGET_ID), "spam", None);
        assert_eq!(
            a,
            request_hash(ModerationAction::Ban, GUILD, Some(TARGET_ID), "spam", None)
        );
        assert_ne!(
            a,
            request_hash(
                ModerationAction::Ban,
                GUILD,
                Some(TARGET_ID),
                "harassment",
                None
            )
        );
        assert_ne!(
            a,
            request_hash(ModerationAction::Kick, GUILD, Some(TARGET_ID), "spam", None)
        );
        assert_ne!(
            a,
            request_hash(
                ModerationAction::TempBan,
                GUILD,
                Some(TARGET_ID),
                "spam",
                Some(60)
            )
        );
        assert_ne!(
            a,
            request_hash(
                ModerationAction::TempBan,
                GUILD,
                Some(TARGET_ID),
                "spam",
                Some(61)
            )
        );
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn outcome_strings_match_legacy() {
        assert_eq!(MemberOutcome::Banned.as_str(), "banned");
        assert_eq!(
            MemberOutcome::TemporarilyBanned.as_str(),
            "temporarily_banned"
        );
        assert_eq!(MemberOutcome::Kicked.as_str(), "kicked");
        assert_eq!(MemberOutcome::TimedOut.as_str(), "timed_out");
        assert_eq!(MemberOutcome::Warned.as_str(), "warned");
        assert_eq!(MemberOutcome::Unbanned.as_str(), "unbanned");
        for outcome in [
            MemberOutcome::Banned,
            MemberOutcome::TemporarilyBanned,
            MemberOutcome::Kicked,
            MemberOutcome::TimedOut,
            MemberOutcome::Warned,
            MemberOutcome::Unbanned,
        ] {
            assert_eq!(MemberOutcome::from_stored(outcome.as_str()), Some(outcome));
        }
        assert_eq!(MemberOutcome::from_stored("purged"), None);
    }

    #[tokio::test]
    async fn ban_kick_timeout_warn_execute_and_audit() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);
        let (discord, store) = (&svc.discord, &svc.store);

        let res = svc
            .execute(&execution(ModerationAction::Ban))
            .await
            .expect("ban");
        assert_eq!(
            res,
            MemberResult {
                outcome: MemberOutcome::Banned,
                replayed: false
            }
        );
        let res = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect("kick");
        assert_eq!(res.outcome, MemberOutcome::Kicked);
        let res = svc
            .execute(&execution(ModerationAction::Timeout))
            .await
            .expect("timeout");
        assert_eq!(res.outcome, MemberOutcome::TimedOut);
        let res = svc
            .execute(&execution(ModerationAction::Warn))
            .await
            .expect("warn");
        assert_eq!(res.outcome, MemberOutcome::Warned);

        // Warn never touches Discord; the other three each made one call.
        assert_eq!(discord.call_count("ban"), 1);
        assert_eq!(discord.call_count("kick"), 1);
        assert_eq!(discord.call_count("timeout"), 1);
        assert_eq!(discord.call_count("unban"), 0);
        // Timeout `until` is now + 3600 s in legacy ISO millis shape.
        let DiscordCall::Timeout { until_iso, .. } = &discord.calls()[2] else {
            panic!("third call is the timeout");
        };
        assert_eq!(until_iso, "2023-11-14T23:13:20.000Z");

        // One warning row (request-id idempotent) + four audit rows.
        assert_eq!(store.warnings().len(), 1);
        assert_eq!(store.warnings()[0].2, TARGET_ID);
        let audits = store.audits();
        assert_eq!(audits.len(), 4);
        assert_eq!(
            audits.iter().map(|a| a.outcome).collect::<Vec<_>>(),
            ["banned", "kicked", "timed_out", "warned"]
        );
        assert!(audits.iter().all(|a| a.guild_id == GUILD));
    }

    #[tokio::test]
    async fn retry_replays_stored_outcome_without_second_mutation() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        let first = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect("kick");
        assert!(!first.replayed);
        let second = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect("replay");
        assert_eq!(
            second,
            MemberResult {
                outcome: MemberOutcome::Kicked,
                replayed: true
            }
        );
        assert_eq!(svc.discord.call_count("kick"), 1);
        // Replay writes no second audit row.
        assert_eq!(svc.store.audits().len(), 1);
    }

    #[tokio::test]
    async fn reused_key_with_different_content_is_a_caller_bug() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        svc.execute(&execution(ModerationAction::Kick))
            .await
            .expect("kick");
        let mut exec = execution(ModerationAction::Kick);
        exec.reason = "different reason, same key".to_owned();
        let err = svc.execute(&exec).await.expect_err("must refuse");
        assert_eq!(err, MemberError::KeyMismatch);
        assert_eq!(svc.discord.call_count("kick"), 1);
    }

    #[tokio::test]
    async fn in_flight_key_refuses_without_takeover() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        // Simulate a crashed first attempt: claim won, never completed.
        let exec = execution(ModerationAction::Ban);
        let hash = request_hash(
            exec.action,
            &exec.guild_id,
            Some(TARGET_ID),
            "spam in #general",
            None,
        );
        let claimed = svc
            .store
            .claim(
                &exec.guild_id,
                &exec.idempotency_key,
                "moderation.ban",
                &hash,
                "x",
            )
            .await
            .expect("claim");
        assert_eq!(claimed, ClaimState::Claimed);

        let err = svc.execute(&exec).await.expect_err("must refuse");
        assert_eq!(err, MemberError::InFlight);
        assert_eq!(svc.discord.call_count("ban"), 0);
    }

    #[tokio::test]
    async fn rejected_discord_call_releases_the_key_for_retry() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        svc.discord
            .fail_with("kick", DiscordError::Rejected("unknown member".to_owned()));
        let err = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect_err("must fail");
        assert!(matches!(
            err,
            MemberError::Discord(DiscordError::Rejected(_))
        ));
        // No audit row for a mutation that provably never happened.
        assert!(svc.store.audits().is_empty());

        // Retry is a real second attempt, not a cached error.
        svc.discord.clear_failure("kick");
        let res = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect("retry");
        assert!(!res.replayed);
        assert_eq!(svc.discord.call_count("kick"), 2);
        assert_eq!(svc.store.audits().len(), 1);
    }

    #[tokio::test]
    async fn uncertain_discord_failure_keeps_the_claim() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        svc.discord.fail_with("ban", DiscordError::Timeout);
        let err = svc
            .execute(&execution(ModerationAction::Ban))
            .await
            .expect_err("must fail");
        assert_eq!(err, MemberError::Discord(DiscordError::Timeout));

        // The claim is NOT released: a retry gets in_progress, never a
        // second ban — Discord may have applied the first.
        svc.discord.clear_failure("ban");
        let err = svc
            .execute(&execution(ModerationAction::Ban))
            .await
            .expect_err("in flight");
        assert_eq!(err, MemberError::InFlight);
        assert_eq!(svc.discord.call_count("ban"), 1);
    }

    #[tokio::test]
    async fn tempban_stages_bans_activates_then_sweep_unbans_at_due_time() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        // Clock pinned at 2023-11-14T22:13:20Z; the tempban runs 3600 s.
        let svc = service(discord, store);

        let res = svc
            .execute(&execution(ModerationAction::TempBan))
            .await
            .expect("tempban");
        assert_eq!(res.outcome, MemberOutcome::TemporarilyBanned);
        assert_eq!(
            svc.store.unban_state("req-tempban"),
            Some("pending".to_owned())
        );
        assert_eq!(svc.discord.call_count("ban"), 1);

        // Not due yet: the sweep claims nothing.
        assert_eq!(svc.run_due_unbans(GUILD).await.expect("sweep"), 0);
        assert_eq!(svc.discord.call_count("unban"), 0);

        // An hour later the sweep unbans exactly once with the staged
        // expiry reason, and records the unban audit row. The service clock
        // is pinned, so drive due-selection at the exact expiry: execute_at
        // is now+3600s, hence a sweep at now+3600s is due.
        let jobs = svc
            .store
            .claim_due_unbans(GUILD, "2023-11-14T23:13:20.000Z", UNBAN_SWEEP_CLAIM_LIMIT)
            .await
            .expect("claim");
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            svc.store.unban_state("req-tempban"),
            Some("running".to_owned())
        );
        // Overlapping sweep cannot claim the same job.
        let again = svc
            .store
            .claim_due_unbans(GUILD, "2023-11-14T23:13:20.000Z", UNBAN_SWEEP_CLAIM_LIMIT)
            .await
            .expect("reclaim");
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn sweep_unbans_and_audits_with_bot_actor() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let clock = AtomicI64::new(1_700_000_000_000);
        let svc = MemberModerationService::new(
            MockMemberDiscord::new(),
            MemMemberStore::new(),
            policy(),
            || clock.load(Ordering::SeqCst),
        );
        svc.execute(&execution(ModerationAction::TempBan))
            .await
            .expect("tempban");
        clock.store(1_700_003_599_999, Ordering::SeqCst);
        assert_eq!(svc.run_due_unbans(GUILD).await.expect("before expiry"), 0);
        clock.store(1_700_003_600_000, Ordering::SeqCst);
        assert_eq!(svc.run_due_unbans(GUILD).await.expect("at expiry"), 1);
        assert_eq!(svc.run_due_unbans(GUILD).await.expect("second sweep"), 0);
        assert_eq!(svc.discord.call_count("unban"), 1);
        let DiscordCall::Unban { reason, .. } = &svc.discord.calls()[1] else {
            panic!("second call is the unban");
        };
        assert_eq!(reason, "Temporary ban expired: spam in #general");
        assert_eq!(
            svc.store.unban_state("req-tempban"),
            Some("done".to_owned())
        );
        let audits = svc.store.audits();
        assert_eq!(audits.len(), 2);
        let unban = &audits[1];
        assert_eq!(unban.actor_id, BOT_ID);
        assert_eq!(unban.action, "moderation.unban_scheduled");
        assert_eq!(unban.request_id, "req-tempban:unban");
        assert_eq!(unban.target_id.as_deref(), Some(TARGET_ID));
    }

    #[tokio::test]
    async fn sweep_requeues_safely_failed_unbans() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        let mut exec = execution(ModerationAction::TempBan);
        exec.request_id = "req-tempban-3".to_owned();
        exec.idempotency_key = "key-tempban-3".to_owned();
        svc.execute(&exec).await.expect("tempban");
        {
            let mut inner = svc.store.inner.lock().expect("lock");
            let row = inner.unbans.get_mut("req-tempban-3").expect("staged");
            row.execute_at = "2023-11-14T22:00:00.000Z".to_owned();
        }
        svc.discord.fail_with(
            "unban",
            DiscordError::Rejected("already unbanned".to_owned()),
        );
        let err = svc
            .run_due_unbans(GUILD)
            .await
            .expect_err("first error rethrown");
        assert!(matches!(
            err,
            MemberError::Discord(DiscordError::Rejected(_))
        ));
        // Requeued as pending: the next sweep retries the exact job.
        assert_eq!(
            svc.store.unban_state("req-tempban-3"),
            Some("pending".to_owned())
        );
        svc.discord.clear_failure("unban");
        assert_eq!(svc.run_due_unbans(GUILD).await.expect("retry sweep"), 1);
        assert_eq!(
            svc.store.unban_state("req-tempban-3"),
            Some("done".to_owned())
        );
    }

    #[tokio::test]
    async fn tempban_ban_rejection_cancels_the_staged_row() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        svc.discord
            .fail_with("ban", DiscordError::Rejected("hierarchy".to_owned()));
        let err = svc
            .execute(&execution(ModerationAction::TempBan))
            .await
            .expect_err("must fail");
        assert!(matches!(
            err,
            MemberError::Discord(DiscordError::Rejected(_))
        ));
        // Provably no ban: staged row cancelled, key released for retry.
        assert_eq!(
            svc.store.unban_state("req-tempban"),
            Some("cancelled".to_owned())
        );
        assert!(svc.store.audits().is_empty());
        svc.discord.clear_failure("ban");
        let res = svc
            .execute(&execution(ModerationAction::TempBan))
            .await
            .expect("retry");
        assert_eq!(res.outcome, MemberOutcome::TemporarilyBanned);
        assert_eq!(
            svc.store.unban_state("req-tempban"),
            Some("pending".to_owned())
        );
    }

    #[tokio::test]
    async fn retempban_supersedes_the_older_pending_job() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord, store);

        svc.execute(&execution(ModerationAction::TempBan))
            .await
            .expect("first");
        let mut exec = execution(ModerationAction::TempBan);
        exec.request_id = "req-tempban-4".to_owned();
        exec.idempotency_key = "key-tempban-4".to_owned();
        svc.execute(&exec).await.expect("second");
        assert_eq!(
            svc.store.unban_state("req-tempban"),
            Some("superseded".to_owned())
        );
        assert_eq!(
            svc.store.unban_state("req-tempban-4"),
            Some("pending".to_owned())
        );
    }

    // F1: an uncertain dispatched unban fences fresh bans for the same
    // member until authoritative resolution lands. A `void` resolution (the
    // DELETE provably never landed) lifts the fence; the late effect can no
    // longer remove the new ban.
    //
    // The fence lives in the ledger row (`running`), not in the claim
    // handle: claiming the job without completing it leaves the member
    // fenced exactly as a timed-out dispatch would. No sweep task is
    // spawned — a sweep dispatches inside the member queue, so a concurrent
    // fresh ban would queue behind it rather than reach the fence; queue
    // serialization itself is covered by
    // `permanent_ban_waits_for_the_same_members_live_unban`.
    #[tokio::test]
    async fn uncertain_unban_fences_fresh_bans_until_resolved() {
        for action in [ModerationAction::Ban, ModerationAction::TempBan] {
            let discord = MockMemberDiscord::new();
            let store = MemMemberStore::new();
            let svc = service(discord.clone(), store.clone());
            svc.execute(&execution_with_id(ModerationAction::TempBan, "old"))
                .await
                .expect("old tempban");
            // Baseline: the staged tempban itself dispatched exactly one ban.
            let bans_before = discord.call_count("ban");
            // The fixed clock pins `now` at stage time, so the 3600 s expiry
            // is in the future. Backdate it so the claim finds the job due
            // (same pattern as `sweep_requeues_safely_failed_unbans`).
            {
                let mut inner = store.inner.lock().expect("lock");
                let row = inner.unbans.get_mut("old").expect("staged");
                row.execute_at = NOW.to_owned();
            }
            // Dispatch the DELETE and leave it uncertain: the job is now
            // `running` in the ledger with its claim token in hand.
            let job = store
                .claim_due_unbans(GUILD, NOW, 25)
                .await
                .expect("claim")
                .pop()
                .expect("due job");
            assert_eq!(job.request_id, "old");
            // Fresh ban while the DELETE is uncertain: must refuse, and must
            // not reach Discord at all.
            let fresh = execution_with_id(action, "new-ban");
            let err = svc.execute(&fresh).await.expect_err("fenced");
            assert!(
                matches!(err, MemberError::Store(_)),
                "uncertain unban must fence fresh bans, got {err:?}"
            );
            assert_eq!(discord.call_count("ban"), bans_before);
            // Only the OLD dispatch is uncertain. The NEW request wrote no
            // intent and sent no PUT, so it must release its own key safely.
            assert!(matches!(
                err,
                MemberError::Store(ref error) if error.is_safe_pre_mutation()
            ));
            assert!(matches!(
                svc.execute(&fresh).await.expect_err("still fenced"),
                MemberError::Store(ref error) if error.is_safe_pre_mutation()
            ));
            assert_eq!(discord.call_count("ban"), bans_before);
            assert_eq!(store.unban_state("old").as_deref(), Some("running"));
            assert!(store
                .owns_unban_claim(&job.request_id, &job.claim_token)
                .await
                .unwrap());
            // Authoritative evidence closes only the old operation. The same
            // unchanged command/key can now proceed, then replay normally.
            store
                .resolve_uncertain_unban(
                    &job.request_id,
                    &job.claim_token,
                    UnbanResolution::Completed,
                )
                .await
                .expect("authoritative completion");
            assert!(!svc.execute(&fresh).await.expect("same-key retry").replayed);
            assert!(svc.execute(&fresh).await.expect("replay").replayed);
            // One old ban, one fresh ban, and no duplicated Discord mutation.
            assert_eq!(discord.call_count("ban"), bans_before + 1);
            assert_eq!(discord.call_count("unban"), 0);
        }
    }

    // F1 (timeout after dispatch): the sweep dispatches the DELETE, Discord
    // times out, and the ledger row stays `running` with its claim token.
    // A fresh permanent ban must refuse — with no Discord PUT at all — and
    // only `resolve_uncertain_unban(Completed)` closes the old row. A
    // subsequent confirmation must NOT supersede the authoritative close.
    #[tokio::test]
    async fn timed_out_unban_fences_fresh_ban_until_authoritative_close() {
        let discord = MockMemberDiscord::new();
        let store = MemMemberStore::new();
        let svc = service(discord.clone(), store.clone());
        svc.execute(&execution_with_id(ModerationAction::TempBan, "old"))
            .await
            .expect("old tempban");
        {
            let mut inner = store.inner.lock().expect("lock");
            let row = inner.unbans.get_mut("old").expect("staged");
            row.execute_at = NOW.to_owned();
        }
        // The sweep dispatches the DELETE; the remote effect times out.
        discord.fail_with("unban", DiscordError::Timeout);
        let err = svc.run_due_unbans(GUILD).await.expect_err("timeout");
        assert!(
            matches!(err, MemberError::Discord(DiscordError::Timeout)),
            "timed-out DELETE must surface, got {err:?}"
        );
        discord.clear_failure("unban");
        assert_eq!(store.unban_state("old").as_deref(), Some("running"));
        // Fresh permanent ban refuses; no PUT reaches Discord.
        let bans_before = discord.call_count("ban");
        let fresh = execution_with_id(ModerationAction::Ban, "fresh");
        assert!(
            svc.execute(&fresh).await.is_err(),
            "running DELETE must fence the fresh ban"
        );
        assert_eq!(discord.call_count("ban"), bans_before);
        // Authoritative evidence that the DELETE provably landed closes the
        // row — only `resolve_uncertain_unban` may close `running`.
        let token = store
            .inner
            .lock()
            .expect("lock")
            .unbans
            .get("old")
            .expect("running row")
            .claim_token
            .clone()
            .expect("claim token retained");
        store
            .resolve_uncertain_unban("old", &token, UnbanResolution::Completed)
            .await
            .expect("authoritative completion");
        assert_eq!(store.unban_state("old").as_deref(), Some("done"));
        // With the uncertainty closed, a fresh ban proceeds exactly once —
        // and its confirmation leaves the authoritatively closed row
        // untouched (the narrowed `confirm_ban` supersedes only
        // never-dispatched schedules).
        svc.execute(&execution_with_id(ModerationAction::Ban, "fresh-2"))
            .await
            .expect("ban after close");
        assert_eq!(discord.call_count("ban"), bans_before + 1);
        assert_eq!(store.unban_state("old").as_deref(), Some("done"));
    }

    // F1 (confirm narrowing, store level): a newer prepared intent confirmed
    // while the older schedule is `running` must NOT supersede it — the
    // dispatched DELETE may still land. The token and the fence survive the
    // confirmation; only `resolve_uncertain_unban` may close the row.
    #[tokio::test]
    async fn confirm_ban_never_supersedes_a_running_schedule() {
        let store = MemMemberStore::new();
        store
            .stage_unban(GUILD, TARGET_ID, NOW, "expiry", "old", NOW)
            .await
            .expect("stage");
        store
            .confirm_ban_attempt(
                GUILD,
                TARGET_ID,
                "old",
                store.ban_attempt("old").unwrap(),
                NOW,
            )
            .await
            .expect("accepted");
        let job = store
            .claim_due_unbans(GUILD, NOW, 25)
            .await
            .expect("claim")
            .pop()
            .expect("job");
        // A newer prepared intent that predates the dispatch window: insert
        // it directly, the way a staging path that ran before the claim
        // would have left it (staging itself now refuses `running` rows).
        let old_generation = {
            let mut inner = store.inner.lock().expect("lock");
            let old_generation = inner.bans.get("old").expect("old intent").generation;
            inner.ban_sequence += 1;
            let generation = inner.ban_sequence;
            inner.bans.insert(
                "new".to_owned(),
                BanRow {
                    guild_id: GUILD.to_owned(),
                    user_id: TARGET_ID.to_owned(),
                    generation,
                    state: BanState::Prepared,
                },
            );
            old_generation
        };
        assert!(old_generation < store.inner.lock().expect("lock").bans["new"].generation);
        store
            .confirm_ban_attempt(
                GUILD,
                TARGET_ID,
                "new",
                store.ban_attempt("new").unwrap(),
                NOW,
            )
            .await
            .expect("newer acceptance");
        // The newer ban is accepted, but the uncertain row is neither closed
        // nor token-stripped: the fence holds for the late DELETE.
        assert_eq!(store.unban_state("old").as_deref(), Some("running"));
        assert_eq!(
            store
                .inner
                .lock()
                .expect("lock")
                .unbans
                .get("old")
                .expect("running row")
                .claim_token
                .as_deref(),
            Some(job.claim_token.as_str())
        );
        // The newer accepted intent correctly fences the old claim (the old
        // generation is no longer current, so neither `complete_unban` nor
        // a fresh `owns` succeeds) — but the row is still `running` with
        // its token, not superseded: only authoritative evidence closes it.
        assert!(!store
            .owns_unban_claim(&job.request_id, &job.claim_token)
            .await
            .expect("ownership"));
        // Only authoritative evidence closes it.
        store
            .resolve_uncertain_unban(&job.request_id, &job.claim_token, UnbanResolution::Void)
            .await
            .expect("authoritative void");
        assert_eq!(store.unban_state("old").as_deref(), Some("superseded"));
    }

    // F1 (task cancellation): the sweep claims the job, dispatches the
    // DELETE, and hangs inside it; the consumer task is aborted exactly like
    // a cancelled worker. The ledger row stays `running` with its token, the
    // fence holds a fresh permanent ban, and no late `confirm_ban` may close
    // the uncertain row — only `resolve_uncertain_unban` may.
    #[tokio::test]
    async fn cancelled_unban_dispatch_keeps_the_fence_and_its_token() {
        struct HangingDiscord {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        impl std::fmt::Debug for HangingDiscord {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("HangingDiscord").finish()
            }
        }
        impl MemberDiscord for HangingDiscord {
            async fn ban(&self, _: &str, _: &str, _: &str) -> Result<(), DiscordError> {
                Ok(())
            }
            async fn unban(&self, _: &str, _: &str, _: &str) -> Result<(), DiscordError> {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(())
            }
            async fn kick(&self, _: &str, _: &str, _: &str) -> Result<(), DiscordError> {
                Ok(())
            }
            async fn timeout(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), DiscordError> {
                Ok(())
            }
        }
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let store = MemMemberStore::new();
        let setup = service(MockMemberDiscord::new(), store.clone());
        setup
            .execute(&execution_with_id(ModerationAction::TempBan, "old"))
            .await
            .expect("old tempban");
        {
            let mut inner = store.inner.lock().expect("lock");
            let row = inner.unbans.get_mut("old").expect("staged");
            row.execute_at = NOW.to_owned();
        }
        // The sweep task owns its service so the future is `'static`; it
        // claims the due job and hangs inside the dispatched DELETE.
        let sweep_store = store.clone();
        let sweep_entered = entered.clone();
        let sweep_release = release.clone();
        let sweep = tokio::spawn(async move {
            let svc = MemberModerationService::new(
                HangingDiscord {
                    entered: sweep_entered,
                    release: sweep_release,
                },
                sweep_store,
                policy(),
                || 1_700_000_000_000,
            );
            svc.run_due_unbans(GUILD).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .expect("sweep must dispatch without panicking");
        sweep.abort();
        assert!(
            sweep.await.expect_err("aborted").is_cancelled(),
            "sweep task must die by cancellation, not by result"
        );
        release.notify_one();
        // The dispatched-but-hung DELETE leaves the row `running` with its
        // claim token: the uncertainty survives the cancellation.
        assert_eq!(store.unban_state("old").as_deref(), Some("running"));
        let token = store
            .inner
            .lock()
            .expect("lock")
            .unbans
            .get("old")
            .expect("running row")
            .claim_token
            .clone()
            .expect("claim token retained");
        // A fresh permanent ban refuses; no PUT reaches Discord.
        let bans = MockMemberDiscord::new();
        let fresh_svc = service(bans.clone(), store.clone());
        assert!(
            fresh_svc
                .execute(&execution_with_id(ModerationAction::Ban, "fresh"))
                .await
                .is_err(),
            "cancelled dispatch must fence the fresh ban"
        );
        assert_eq!(bans.call_count("ban"), 0);
        // A late confirmation for the same expiry cannot close the uncertain
        // row either: the token and the fence survive it.
        assert!(store
            .confirm_ban_attempt(
                GUILD,
                TARGET_ID,
                "old",
                store.ban_attempt("old").unwrap(),
                NOW
            )
            .await
            .is_err());
        assert_eq!(store.unban_state("old").as_deref(), Some("running"));
        // Only authoritative evidence closes the row; a void resolution lifts
        // the fence and the fresh ban proceeds exactly once.
        store
            .resolve_uncertain_unban("old", &token, UnbanResolution::Void)
            .await
            .expect("authoritative void");
        fresh_svc
            .execute(&execution_with_id(ModerationAction::Ban, "fresh-2"))
            .await
            .expect("ban after void");
        assert_eq!(bans.call_count("ban"), 1);
    }

    // F1 (store level): staging refuses while a `running` row exists even
    // when the caller never observed the job — the fence lives in the
    // ledger, not in the claim handle.
    #[tokio::test]
    async fn staging_refuses_while_a_running_row_exists() {
        let store = MemMemberStore::new();
        store
            .stage_unban(GUILD, TARGET_ID, NOW, "expiry", "old", NOW)
            .await
            .expect("stage");
        store
            .confirm_ban_attempt(
                GUILD,
                TARGET_ID,
                "old",
                store.ban_attempt("old").unwrap(),
                NOW,
            )
            .await
            .expect("accepted");
        let job = store
            .claim_due_unbans(GUILD, NOW, 25)
            .await
            .expect("claim")
            .pop()
            .expect("job");
        // A fresh ban intent refuses; nothing is staged for it.
        assert!(store.stage_ban(GUILD, TARGET_ID, "new", NOW).await.is_err());
        // `completed` resolution closes the old job; the fence lifts and the
        // same staging succeeds without a new request id.
        store
            .resolve_uncertain_unban(
                &job.request_id,
                &job.claim_token,
                UnbanResolution::Completed,
            )
            .await
            .expect("authoritative completion");
        assert_eq!(store.unban_state("old").as_deref(), Some("done"));
        store
            .stage_ban(GUILD, TARGET_ID, "new", NOW)
            .await
            .expect("fence lifted");
        // Double resolution surfaces instead of vanishing.
        assert!(store
            .resolve_uncertain_unban(
                &job.request_id,
                &job.claim_token,
                UnbanResolution::Completed
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn void_unban_resolution_preserves_the_current_expiry_obligation() {
        let store = MemMemberStore::new();
        let discord = MockMemberDiscord::new();
        let svc = service(discord.clone(), store.clone());
        svc.execute(&execution_with_id(ModerationAction::TempBan, "old"))
            .await
            .expect("accepted tempban");
        store.lock().unbans.get_mut("old").unwrap().execute_at = NOW.to_owned();
        discord.fail_with("unban", DiscordError::Timeout);
        assert!(matches!(
            svc.run_due_unbans(GUILD).await,
            Err(MemberError::Discord(DiscordError::Timeout))
        ));
        let token = store.lock().unbans["old"].claim_token.clone().unwrap();
        assert!(store
            .resolve_uncertain_unban("old", "wrong", UnbanResolution::Void)
            .await
            .is_err());
        store
            .resolve_uncertain_unban("old", &token, UnbanResolution::Void)
            .await
            .expect("exact DELETE provably cannot land");
        assert_eq!(store.unban_state("old").as_deref(), Some("pending"));
        assert!(store.lock().unbans["old"].claim_token.is_none());
        assert!(store
            .resolve_uncertain_unban("old", &token, UnbanResolution::Void)
            .await
            .is_err());
        discord.clear_failure("unban");
        assert_eq!(
            svc.run_due_unbans(GUILD)
                .await
                .expect("expiry still required"),
            1
        );
        assert_eq!(store.unban_state("old").as_deref(), Some("done"));
        assert_eq!(discord.call_count("unban"), 2);
        assert_eq!(
            store
                .audits()
                .iter()
                .filter(|row| row.request_id == "old:unban")
                .count(),
            1
        );
    }

    /// Store double that fails selected writes once, to exercise service
    /// behavior under storage faults the memory double cannot produce.
    #[derive(Clone)]
    struct FaultyStore {
        inner: MemMemberStore,
        fail_complete_once: Arc<std::sync::atomic::AtomicBool>,
        fail_stage_once: Arc<std::sync::atomic::AtomicBool>,
        fail_audit_once: Arc<std::sync::atomic::AtomicBool>,
        fail_complete_unban_once: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FaultyStore {
        fn wrap(inner: MemMemberStore) -> Self {
            use std::sync::atomic::AtomicBool;
            Self {
                inner,
                fail_complete_once: Arc::new(AtomicBool::new(false)),
                fail_stage_once: Arc::new(AtomicBool::new(false)),
                fail_audit_once: Arc::new(AtomicBool::new(false)),
                fail_complete_unban_once: Arc::new(AtomicBool::new(false)),
            }
        }

        fn fail_once(flag: &Arc<std::sync::atomic::AtomicBool>) -> bool {
            flag.swap(false, std::sync::atomic::Ordering::SeqCst)
        }

        fn arm_complete(&self) {
            self.fail_complete_once
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn arm_stage(&self) {
            self.fail_stage_once
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn arm_audit(&self) {
            self.fail_audit_once
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn arm_complete_unban(&self) {
            self.fail_complete_unban_once
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl MemberModerationStore for FaultyStore {
        async fn serialize_member<T, F, Fut>(&self, guild_id: &str, user_id: &str, run: F) -> T
        where
            F: FnOnce() -> Fut + Send,
            Fut: Future<Output = T> + Send,
        {
            self.inner.serialize_member(guild_id, user_id, run).await
        }

        async fn claim(
            &self,
            guild_id: &str,
            idempotency_key: &str,
            action: &str,
            request_hash: &str,
            claimed_at: &str,
        ) -> Result<ClaimState, StoreError> {
            self.inner
                .claim(guild_id, idempotency_key, action, request_hash, claimed_at)
                .await
        }

        async fn complete(
            &self,
            guild_id: &str,
            idempotency_key: &str,
            outcome: &str,
            result_json: &str,
            completed_at: &str,
        ) -> Result<(), StoreError> {
            if Self::fail_once(&self.fail_complete_once) {
                return Err(StoreError::new("injected completion failure"));
            }
            self.inner
                .complete(
                    guild_id,
                    idempotency_key,
                    outcome,
                    result_json,
                    completed_at,
                )
                .await
        }

        async fn release(&self, guild_id: &str, idempotency_key: &str) -> Result<(), StoreError> {
            self.inner.release(guild_id, idempotency_key).await
        }

        async fn record_audit(&self, row: &AuditRow) -> Result<(), StoreError> {
            if Self::fail_once(&self.fail_audit_once) {
                return Err(StoreError::new("injected audit failure"));
            }
            self.inner.record_audit(row).await
        }

        async fn add_warning(
            &self,
            warning_id: &str,
            guild_id: &str,
            user_id: &str,
            actor_id: &str,
            reason: &str,
            request_id: &str,
            created_at: &str,
        ) -> Result<(), StoreError> {
            self.inner
                .add_warning(
                    warning_id, guild_id, user_id, actor_id, reason, request_id, created_at,
                )
                .await
        }

        async fn stage_ban(
            &self,
            guild_id: &str,
            user_id: &str,
            request_id: &str,
            created_at: &str,
        ) -> Result<BanAttempt, StoreError> {
            if Self::fail_once(&self.fail_stage_once) {
                // A rolled-back staging transaction: nothing persisted, so a
                // retry is provably safe.
                return Err(StoreError::rolled_back("injected stage rollback"));
            }
            self.inner
                .stage_ban(guild_id, user_id, request_id, created_at)
                .await
        }

        async fn confirm_ban_attempt(
            &self,
            guild_id: &str,
            user_id: &str,
            request_id: &str,
            attempt: BanAttempt,
            completed_at: &str,
        ) -> Result<(), StoreError> {
            self.inner
                .confirm_ban_attempt(guild_id, user_id, request_id, attempt, completed_at)
                .await
        }

        async fn resolve_historical_ban_acceptance(
            &self,
            guild_id: &str,
            user_id: &str,
            request_id: &str,
            attempt: BanAttempt,
            evidence: &HistoricalBanAcceptance,
            completed_at: &str,
        ) -> Result<(), StoreError> {
            if Self::fail_once(&self.fail_audit_once) {
                return Err(StoreError::new("injected reconciliation audit failure"));
            }
            self.inner
                .resolve_historical_ban_acceptance(
                    guild_id,
                    user_id,
                    request_id,
                    attempt,
                    evidence,
                    completed_at,
                )
                .await
        }

        async fn stage_unban(
            &self,
            guild_id: &str,
            user_id: &str,
            execute_at: &str,
            reason: &str,
            request_id: &str,
            created_at: &str,
        ) -> Result<BanAttempt, StoreError> {
            if Self::fail_once(&self.fail_stage_once) {
                return Err(StoreError::rolled_back("injected stage rollback"));
            }
            self.inner
                .stage_unban(
                    guild_id, user_id, execute_at, reason, request_id, created_at,
                )
                .await
        }

        async fn activate_staged_unban(
            &self,
            guild_id: &str,
            user_id: &str,
            request_id: &str,
            completed_at: &str,
        ) -> Result<(), StoreError> {
            self.inner
                .activate_staged_unban(guild_id, user_id, request_id, completed_at)
                .await
        }

        async fn reject_ban_attempt(
            &self,
            guild_id: &str,
            user_id: &str,
            request_id: &str,
            attempt: BanAttempt,
            completed_at: &str,
        ) -> Result<(), StoreError> {
            self.inner
                .reject_ban_attempt(guild_id, user_id, request_id, attempt, completed_at)
                .await
        }

        async fn claim_due_unbans(
            &self,
            guild_id: &str,
            now: &str,
            limit: i64,
        ) -> Result<Vec<UnbanJob>, StoreError> {
            self.inner.claim_due_unbans(guild_id, now, limit).await
        }

        async fn owns_unban_claim(
            &self,
            request_id: &str,
            claim_token: &str,
        ) -> Result<bool, StoreError> {
            self.inner.owns_unban_claim(request_id, claim_token).await
        }

        async fn complete_unban(
            &self,
            request_id: &str,
            claim_token: &str,
        ) -> Result<(), StoreError> {
            if Self::fail_once(&self.fail_complete_unban_once) {
                return Err(StoreError::new("injected unban completion failure"));
            }
            self.inner.complete_unban(request_id, claim_token).await
        }

        async fn resolve_uncertain_unban(
            &self,
            request_id: &str,
            claim_token: &str,
            resolution: UnbanResolution,
        ) -> Result<(), StoreError> {
            self.inner
                .resolve_uncertain_unban(request_id, claim_token, resolution)
                .await
        }

        async fn requeue_unban(
            &self,
            request_id: &str,
            claim_token: &str,
        ) -> Result<(), StoreError> {
            self.inner.requeue_unban(request_id, claim_token).await
        }
    }

    // F3: a completion failure after Discord acceptance must still leave the
    // audit row behind. The claim stays uncertain (retry is `InFlight`, never
    // a second Discord call), but the audit of the successful kick exists.
    #[tokio::test]
    async fn accepted_kick_is_audited_when_completion_fails() {
        let discord = MockMemberDiscord::new();
        let store = FaultyStore::wrap(MemMemberStore::new());
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
            1_700_000_000_000
        });

        store.arm_complete();
        let err = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect_err("completion fails");
        assert!(matches!(err, MemberError::Store(_)));
        // The kick landed exactly once, and its audit row exists despite the
        // completion failure.
        assert_eq!(discord.call_count("kick"), 1);
        assert_eq!(store.inner.audits().len(), 1);
        assert_eq!(store.inner.audits()[0].outcome, "kicked");
        // Retry stays uncertain — never a duplicate mutation, never a second
        // audit row.
        assert_eq!(
            svc.execute(&execution(ModerationAction::Kick)).await,
            Err(MemberError::InFlight)
        );
        assert_eq!(discord.call_count("kick"), 1);
        assert_eq!(store.inner.audits().len(), 1);
    }

    // F3 (scheduled effect): a completion failure after the dispatched
    // DELETE landed must still leave the unban audit row behind. The claim
    // stays `running` (uncertain, correct — the next sweep reconciles it via
    // ownership, never by repeating the DELETE), but the audit of the
    // successful unban already exists.
    #[tokio::test]
    async fn accepted_scheduled_unban_is_audited_when_completion_fails() {
        use std::sync::atomic::{AtomicI64, Ordering};
        let clock = AtomicI64::new(1_700_000_000_000);
        let discord = MockMemberDiscord::new();
        let store = FaultyStore::wrap(MemMemberStore::new());
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
            clock.load(Ordering::SeqCst)
        });
        let mut tempban = execution(ModerationAction::TempBan);
        tempban.request_id = "req-tempban-9".to_owned();
        tempban.idempotency_key = "key-tempban-9".to_owned();
        svc.execute(&tempban).await.expect("tempban");
        {
            let mut inner = store.inner.inner.lock().expect("lock");
            let row = inner.unbans.get_mut("req-tempban-9").expect("staged");
            row.execute_at = NOW.to_owned();
        }
        // Only the scheduled completion fails; the dispatched DELETE lands.
        store.arm_complete_unban();
        let err = svc
            .run_due_unbans(GUILD)
            .await
            .expect_err("completion fails");
        assert!(
            matches!(err, MemberError::Store(_)),
            "completion loss must surface, got {err:?}"
        );
        assert_eq!(discord.call_count("unban"), 1);
        // The DELETE landed exactly once and its audit row exists; the claim
        // stays `running` so nothing replays or repeats it.
        assert_eq!(
            store.inner.unban_state("req-tempban-9").as_deref(),
            Some("running")
        );
        let audits = store.inner.audits();
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[1].request_id, "req-tempban-9:unban");
        assert_eq!(audits[1].action, "moderation.unban_scheduled");
        assert_eq!(audits[1].outcome, "unbanned");
        // A second sweep does not repeat the DELETE: the uncertain row still
        // needs authoritative resolution, but the audit is never duplicated.
        let token = store
            .inner
            .inner
            .lock()
            .expect("lock")
            .unbans
            .get("req-tempban-9")
            .expect("running row")
            .claim_token
            .clone()
            .expect("claim token retained");
        store
            .inner
            .resolve_uncertain_unban("req-tempban-9", &token, UnbanResolution::Completed)
            .await
            .expect("authoritative completion");
        assert_eq!(discord.call_count("unban"), 1);
        assert_eq!(store.inner.audits().len(), 2);
    }

    // F3 (audit path unchanged): audit loss alone never fails an accepted
    // action, and the completion still lands so the retry replays.
    #[tokio::test]
    async fn audit_loss_alone_still_completes_and_replays() {
        let discord = MockMemberDiscord::new();
        let store = FaultyStore::wrap(MemMemberStore::new());
        let svc = MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
            1_700_000_000_000
        });

        store.arm_audit();
        let res = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect("audit loss is logged, not fatal");
        assert!(!res.replayed);
        assert!(store.inner.audits().is_empty());
        let replay = svc
            .execute(&execution(ModerationAction::Kick))
            .await
            .expect("replay");
        assert!(replay.replayed);
        assert_eq!(discord.call_count("kick"), 1);
    }

    // F4: a rolled-back pre-dispatch staging failure releases the key, so the
    // same request retries as a real second attempt — no intent, no schedule,
    // no Discord call left behind.
    #[tokio::test]
    async fn rolled_back_stage_failure_allows_retry() {
        for action in [ModerationAction::Ban, ModerationAction::TempBan] {
            let discord = MockMemberDiscord::new();
            let store = FaultyStore::wrap(MemMemberStore::new());
            let svc =
                MemberModerationService::new(discord.clone(), store.clone(), policy(), || {
                    1_700_000_000_000
                });

            store.arm_stage();
            let err = svc
                .execute(&execution(action))
                .await
                .expect_err("stage fails");
            assert!(matches!(err, MemberError::Store(_)));
            assert_eq!(discord.call_count("ban"), 0);
            // Same request retries cleanly: no intent, no schedule, no fence.
            let res = svc.execute(&execution(action)).await.expect("retry");
            assert!(!res.replayed);
            assert_eq!(discord.call_count("ban"), 1);
        }
    }
}
