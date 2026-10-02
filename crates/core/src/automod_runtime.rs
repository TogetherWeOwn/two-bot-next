//! Automod gateway decisions. No Discord client or private dispatcher lives here.
//!
//! The gateway claims a delivery before calling `inspect`, resolves a target
//! only for enforcing matches, then passes the planned effects to the shared
//! REST executor. Rejected creates go to raw capture only; edits never award XP
//! or advance the funnel. A failed/uncertain inspection must not be accepted.

use std::future::Future;

use crate::automod::{
    match_automod_with_clock, sanction_for, AutomodConfig, AutomodFilter, AutomodMessage,
    AutomodSanction, RepeatObservation, RepeatTracker, SanctionAction,
};
use crate::commands::PERM_MODERATE_MEMBERS;
use crate::moderation::{
    moderation_target_protection, ModerationPolicy, ModerationTarget, TargetProtection,
};

// One source for the guild pins: the backup identity guards already hold the
// reviewed literals (scripts/ci/snowflakes-allowlist.json).
pub use crate::backup::guild_config::{LIVE_GUILD_ID, TWO_STAGING_GUILD_ID as STAGING_GUILD_ID};

/// Approval is supplied by the activation fence, never inferred from ENFORCE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodScope {
    pub guild_id: String,
    pub live_approved: bool,
}

impl AutomodScope {
    #[must_use]
    pub fn permits(&self, guild_id: &str) -> bool {
        self.guild_id == guild_id
            && (guild_id == STAGING_GUILD_ID || (guild_id == LIVE_GUILD_ID && self.live_approved))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDeliveryKind {
    Create,
    Update,
}

/// Absence of a complete snapshot requests a fetch through the shared executor.
/// The adapter must not turn missing update fields into empty content/roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDelivery {
    pub kind: MessageDeliveryKind,
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub message_id: String,
    pub snapshot: Option<AutomodMessage>,
    /// Immutable CREATE facts awaiting authoritative member roles. Not an
    /// inspectable snapshot; enrichment may replace only its role IDs.
    pub create_pending_roles: Option<AutomodMessage>,
    /// Discord's stable edit timestamp, not the gateway receipt timestamp.
    pub edited_timestamp_ms: Option<u64>,
    pub observed_timestamp_ms: u64,
}

/// Receipt timestamps are deliberately excluded: gateway retries must retain
/// identity. Creates key on message ID; edits key on stable revision + facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryKey {
    pub guild_id: String,
    pub message_id: String,
    pub kind: MessageDeliveryKind,
    pub dry_run: bool,
    pub request_hash: String,
}

impl DeliveryKey {
    #[must_use]
    pub fn from_delivery(delivery: &MessageDelivery, dry_run: bool) -> Option<Self> {
        use sha2::{Digest, Sha256};
        let message = delivery.snapshot.as_ref()?;
        let guild_id = delivery.guild_id.as_ref()?;
        if &message.guild_id != guild_id
            || message.channel_id != delivery.channel_id
            || message.message_id != delivery.message_id
        {
            return None;
        }
        // Member roles are current resolver facts, not part of the message's
        // revision identity. Role changes must not bypass a preserved claim.
        let mut mentions = message.mentioned_user_ids.clone();
        let mut attachments = message.attachment_names.clone();
        mentions.sort();
        attachments.sort();
        let revision = match delivery.kind {
            MessageDeliveryKind::Create => {
                serde_json::json!(["create", guild_id, delivery.message_id])
            }
            MessageDeliveryKind::Update => serde_json::json!([
                "update",
                guild_id,
                delivery.message_id,
                delivery.channel_id,
                message.author_id,
                message.author_is_bot,
                mentions,
                attachments,
                message.content,
                delivery.edited_timestamp_ms,
            ]),
        };
        let request_hash = hex::encode(Sha256::digest(revision.to_string().as_bytes()));
        Some(Self {
            guild_id: guild_id.clone(),
            message_id: delivery.message_id.clone(),
            kind: delivery.kind,
            dry_run,
            request_hash,
        })
    }

    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            MessageDeliveryKind::Create => "create",
            MessageDeliveryKind::Update => "update",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViolationRecord {
    pub count: u64,
    pub inserted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunnelDisposition {
    Accept,
    CaptureOnly,
    None,
}

impl MessageDeliveryKind {
    #[must_use]
    pub fn funnel(self, matched: bool) -> FunnelDisposition {
        match self {
            Self::Update => FunnelDisposition::None,
            Self::Create if matched => FunnelDisposition::CaptureOnly,
            Self::Create => FunnelDisposition::Accept,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSubject {
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub author_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodMatch {
    pub subject: MessageSubject,
    pub filter: AutomodFilter,
    pub funnel: FunnelDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    Ignore,
    FetchMessage {
        guild_id: String,
        channel_id: String,
        message_id: String,
    },
    /// Identity mismatch or unavailable facts: no downstream award or mutation.
    Unavailable,
    Accepted(FunnelDisposition),
    Matched(AutomodMatch),
}

pub struct AutomodRuntime {
    config: AutomodConfig,
    scope: AutomodScope,
    repeats: RepeatTracker,
    /// Newest message-clock observation. The maintenance tick never expires
    /// repeat history against a wall clock ahead of it, so a resumed batch
    /// of delayed creates keeps the history it still needs.
    newest_observation_ms: Option<u64>,
}

impl AutomodRuntime {
    #[must_use]
    pub fn new(config: AutomodConfig, scope: AutomodScope) -> Self {
        Self {
            config,
            scope,
            repeats: RepeatTracker::default(),
            newest_observation_ms: None,
        }
    }

    #[must_use]
    pub fn dry_run(&self) -> bool {
        self.config.dry_run
    }

    /// Activation fence applied BEFORE any claim, fetch or inspection: a
    /// guild delivery inside the approved scope with automod enabled. DMs,
    /// other guilds and a disabled config take the ordinary funnel path.
    #[must_use]
    pub fn admits(&self, guild_id: Option<&str>) -> bool {
        self.config.enabled && guild_id.is_some_and(|guild_id| self.scope.permits(guild_id))
    }

    /// The shared maintenance tick can expire idle authors without receiving
    /// another message. This does not inspect content or create any effects.
    pub fn expire_repeat_history(&mut self, now_ms: u64) {
        let now_ms = match self.newest_observation_ms {
            Some(newest) if now_ms > newest => newest,
            _ => now_ms,
        };
        self.repeats
            .expire(now_ms, self.config.policy.repeated_message_window_seconds);
    }

    /// Call only after winning the durable delivery claim. Retries must replay
    /// the stored outcome, not observe the repeat tracker or award the funnel.
    pub fn inspect(&mut self, delivery: &MessageDelivery) -> Inspection {
        let Some(guild_id) = delivery.guild_id.as_deref() else {
            return Inspection::Ignore;
        };
        if !self.scope.permits(guild_id) {
            return Inspection::Ignore;
        }
        if !self.config.enabled {
            return Inspection::Accepted(delivery.kind.funnel(false));
        }
        let Some(snapshot) = delivery.snapshot.as_ref() else {
            return Inspection::FetchMessage {
                guild_id: guild_id.to_owned(),
                channel_id: delivery.channel_id.clone(),
                message_id: delivery.message_id.clone(),
            };
        };
        if snapshot.guild_id != guild_id
            || snapshot.channel_id != delivery.channel_id
            || snapshot.message_id != delivery.message_id
        {
            return Inspection::Unavailable;
        }
        if self.config.policy.is_exempt(snapshot) {
            return Inspection::Accepted(delivery.kind.funnel(false));
        }
        let mut message = snapshot.clone();
        if delivery.kind == MessageDeliveryKind::Update {
            // Inspect the revision's interval, not a later retry receipt.
            // The durable preserved decision remains authoritative on retry;
            // a stable clock alone cannot preserve swept mutable history.
            // Unstamped updates inspect at receipt time without advancing it.
            message.observed_timestamp_ms = delivery
                .edited_timestamp_ms
                .unwrap_or(delivery.observed_timestamp_ms);
        }
        // Only creates advance expiry. A revision may predate retained rows;
        // an unstamped metadata update has no message clock at all. Neither
        // may prune the same author's (or another author's) delayed history.
        let observation = match delivery.kind {
            MessageDeliveryKind::Create => {
                self.repeats.expire(
                    message.observed_timestamp_ms,
                    self.config.policy.repeated_message_window_seconds,
                );
                self.newest_observation_ms = Some(
                    self.newest_observation_ms
                        .map_or(message.observed_timestamp_ms, |newest| {
                            newest.max(message.observed_timestamp_ms)
                        }),
                );
                RepeatObservation::Create
            }
            MessageDeliveryKind::Update if delivery.edited_timestamp_ms.is_some() => {
                RepeatObservation::Revision
            }
            MessageDeliveryKind::Update => RepeatObservation::UnstampedUpdate,
        };
        let Some(filter) = match_automod_with_clock(
            &message,
            &self.config.policy,
            &mut self.repeats,
            observation,
        ) else {
            return Inspection::Accepted(delivery.kind.funnel(false));
        };
        Inspection::Matched(AutomodMatch {
            subject: MessageSubject {
                guild_id: message.guild_id,
                channel_id: message.channel_id,
                message_id: message.message_id,
                author_id: message.author_id,
            },
            filter,
            funnel: delivery.kind.funnel(true),
        })
    }

    /// Resolve protection BEFORE recording a violation or executing any effect.
    /// Dry run deliberately needs no target fetch and never increments the ledger.
    #[must_use]
    pub fn target_gate(&self, matched: &AutomodMatch, facts: Option<&TargetFacts>) -> TargetGate {
        if !self.scope.permits(&matched.subject.guild_id) || !self.config.enabled {
            return TargetGate::Unavailable;
        }
        if self.config.dry_run {
            return TargetGate::DryRun;
        }
        let Some(facts) = facts else {
            return TargetGate::Unavailable;
        };
        if facts.target.user_id != matched.subject.author_id {
            return TargetGate::Unavailable;
        }
        match moderation_target_protection(&facts.target, &facts.policy) {
            Some(reason) => TargetGate::Protected(reason),
            None => TargetGate::Allowed,
        }
    }

    /// Plans only: the shared executor owns deletion, warning and timeout I/O.
    /// `violation_count` comes from the atomic store; protected matches still
    /// count, as in legacy, but an unresolved target is safe to retry and counts
    /// nothing. Warn/timeout hierarchy refusal does not undo the exact deletion.
    #[must_use]
    pub fn plan(
        &self,
        matched: &AutomodMatch,
        violation: ViolationRecord,
        facts: Option<&TargetFacts>,
    ) -> EnforcementPlan {
        let violation_count = violation.count;
        let gate = self.target_gate(matched, facts);
        let sanction = sanction_for(violation_count, &self.config.policy.sanctions);
        let mut plan = EnforcementPlan {
            subject: matched.subject.clone(),
            filter: matched.filter,
            funnel: matched.funnel,
            sanction,
            violation_count: (!matches!(gate, TargetGate::DryRun | TargetGate::Unavailable))
                .then_some(violation_count),
            outcome: PlanOutcome::Ready,
            effects: Vec::new(),
        };
        if matches!(gate, TargetGate::Allowed | TargetGate::Protected(_)) && !violation.inserted {
            plan.outcome = PlanOutcome::AlreadyProcessed;
            return plan;
        }
        if gate == TargetGate::Allowed && violation_count == 0 {
            plan.outcome = PlanOutcome::Unavailable;
            plan.violation_count = None;
            return plan;
        }
        match gate {
            TargetGate::DryRun => plan.outcome = PlanOutcome::DryRun,
            TargetGate::Unavailable => plan.outcome = PlanOutcome::Unavailable,
            TargetGate::Protected(reason) => plan.outcome = PlanOutcome::Protected(reason),
            TargetGate::Allowed => {
                plan.effects.push(AutomodEffect::DeleteMessage);
                if sanction.action != SanctionAction::Delete {
                    let facts = facts.expect("allowed gate has target facts");
                    if facts.bot_permissions & PERM_MODERATE_MEMBERS == 0
                        || facts.bot_highest_role_position <= facts.target.highest_role_position
                    {
                        plan.outcome = PlanOutcome::SanctionRefused;
                    } else {
                        match sanction.action {
                            SanctionAction::Warn => plan.effects.push(AutomodEffect::WarnMember),
                            SanctionAction::Timeout => {
                                plan.effects.push(AutomodEffect::TimeoutMember {
                                    seconds: sanction.timeout_seconds.unwrap_or(600),
                                })
                            }
                            SanctionAction::Delete => unreachable!(),
                        }
                    }
                }
            }
        }
        plan
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetFacts {
    pub target: ModerationTarget,
    pub policy: ModerationPolicy,
    pub bot_highest_role_position: i64,
    pub bot_permissions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetGate {
    DryRun,
    Unavailable,
    Protected(TargetProtection),
    Allowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomodEffect {
    DeleteMessage,
    WarnMember,
    TimeoutMember { seconds: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanOutcome {
    AlreadyProcessed,
    DryRun,
    Unavailable,
    Protected(TargetProtection),
    SanctionRefused,
    Ready,
}

/// IDs and reason code only; never persist message content or matched excerpts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnforcementPlan {
    pub subject: MessageSubject,
    pub filter: AutomodFilter,
    pub funnel: FunnelDisposition,
    pub sanction: AutomodSanction,
    pub violation_count: Option<u64>,
    pub outcome: PlanOutcome,
    pub effects: Vec<AutomodEffect>,
}

/// Execution receipt, not a plan: `deleted` is true only after confirmed REST
/// success. Completion has no arbitrary metadata field to smuggle message text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredOutcome {
    pub matched: bool,
    pub deleted: bool,
    pub outcome: CompletionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    Accepted,
    AlreadyProcessed,
    DryRun,
    Protected,
    Deleted,
    Warned,
    TimedOut,
    SanctionRefused,
}

/// Store-agnostic arbitration outcome, mirroring the durable store's claim
/// result so the shared activation orchestrator (TOG-10261) runs against
/// `AutomodStore` in production and an in-memory ledger in fast tests.
#[derive(Debug)]
pub enum LedgerClaim<C> {
    Acquired(C),
    InFlight,
    Replayed(StoredOutcome),
    /// Released pre-count claim with its preserved decision: replay these
    /// IDs/reason code without re-running the mutable repeat tracker.
    Preserved(C, AutomodMatch),
}

/// Claim-ledger seam for the shared activation orchestrator. One method per
/// durable store operation it needs. Implementations must honour the store
/// contract: claims fence stale completions, counting is insert-first
/// idempotent per message, only an active unmutated enforce claim can carry a
/// preserved decision, and nothing releases a started or counted claim.
pub trait AutomodClaimLedger: Send + Sync {
    type Claim: Send + Sync;
    type Error: std::fmt::Display + Send;

    fn ledger_claim(
        &self,
        key: &DeliveryKey,
    ) -> impl Future<Output = Result<LedgerClaim<Self::Claim>, Self::Error>> + Send;
    fn ledger_preserve(
        &self,
        claim: &Self::Claim,
        matched: &AutomodMatch,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn ledger_mark_started(
        &self,
        claim: &Self::Claim,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn ledger_complete(
        &self,
        claim: &Self::Claim,
        outcome: &StoredOutcome,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn ledger_release(
        &self,
        claim: &Self::Claim,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send;
    fn ledger_record(
        &self,
        claim: &Self::Claim,
        subject: &MessageSubject,
        filter: AutomodFilter,
        at_iso: &str,
    ) -> impl Future<Output = Result<ViolationRecord, Self::Error>> + Send;
}
