//! Automod gateway decisions. No Discord client or private dispatcher lives here.
//!
//! The gateway claims a delivery before calling `inspect`, resolves a target
//! only for enforcing matches, then passes the planned effects to the shared
//! REST executor. Rejected creates go to raw capture only; edits never award XP
//! or advance the funnel. A failed/uncertain inspection must not be accepted.

use crate::automod::{
    match_automod, sanction_for, AutomodConfig, AutomodFilter, AutomodMessage, AutomodSanction,
    RepeatTracker, SanctionAction,
};
use crate::commands::PERM_MODERATE_MEMBERS;
use crate::moderation::{
    moderation_target_protection, ModerationPolicy, ModerationTarget, TargetProtection,
};

pub const STAGING_GUILD_ID: &str = "1545644954272137297";
pub const LIVE_GUILD_ID: &str = "326474832151838730";

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
    pub observed_timestamp_ms: u64,
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
}

impl AutomodRuntime {
    #[must_use]
    pub fn new(config: AutomodConfig, scope: AutomodScope) -> Self {
        Self {
            config,
            scope,
            repeats: RepeatTracker::default(),
        }
    }

    #[must_use]
    pub fn dry_run(&self) -> bool {
        self.config.dry_run
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
        // Creates use message time; edits use receipt time, not original creation.
        if delivery.kind == MessageDeliveryKind::Update {
            message.observed_timestamp_ms = delivery.observed_timestamp_ms;
        }
        self.repeats.expire(
            delivery.observed_timestamp_ms,
            self.config.policy.repeated_message_window_seconds,
        );
        let Some(filter) = match_automod(&message, &self.config.policy, &mut self.repeats) else {
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
        violation_count: u64,
        facts: Option<&TargetFacts>,
    ) -> EnforcementPlan {
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
