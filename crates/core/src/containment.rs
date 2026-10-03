//! Pure anti-nuke decisions and quarantine planning (TOG-9809, slice 5).
//!
//! Ports legacy `two-bot` at `d5d1179348feb9157bcac8c875de9399d4f5c76a`:
//! `src/moderation/{containment,containmentStore,containmentDiscord}.ts`.
//! Inputs are explicit timestamps, claimed evidence, incidents and role snapshots.
//! This module neither claims durable records nor executes Discord requests;
//! `containment_store` serializes claims. The future adapter must enforce the
//! staging/identity fence before executing a plan; see `docs/containment.md`.

use std::collections::HashSet;

use crate::onboarding::MentionPolicy;
use crate::raid::StaffAlertMessage;

pub const DEFAULT_CONTAINMENT_WINDOW_MS: i64 = 60_000;
pub const DEFAULT_CONTAINMENT_MAX_AGE_MS: i64 = 120_000;
pub const DEFAULT_CONTAINMENT_HEAT_THRESHOLD: u64 = 5;
pub const CONTAINMENT_FUTURE_SKEW_MS: i64 = 5_000;

pub const DANGEROUS_PERMISSIONS: u64 = (1 << 1) // KickMembers
    | (1 << 2) // BanMembers
    | (1 << 3) // Administrator
    | (1 << 4) // ManageChannels
    | (1 << 5) // ManageGuild
    | (1 << 27) // ManageWebhooks
    | (1 << 28) // ManageRoles
    | (1 << 40); // ModerateMembers

/// Always-logged staff-alert event names (parity §8).
///
/// The adapter must log [`CONTAINMENT_ALERT_EVENT`] with the alert content for
/// every `Alert` signal, even when no staff channel is configured or delivery
/// fails — mirroring legacy `containmentAlert.post`, which logs before any
/// channel check. A `Suppressed` signal (repeat offence inside the
/// per-executor cooldown) must still be logged under
/// [`CONTAINMENT_SUPPRESSED_EVENT`] with guild/executor/heat/threshold fields,
/// but must not produce a second staff post. `Quiet` logs nothing.
pub const CONTAINMENT_ALERT_EVENT: &str = "containment_alert";
pub const CONTAINMENT_SUPPRESSED_EVENT: &str = "containment_alert_suppressed";
/// IDs listed inline in the staff text before `…and N more` truncation,
/// mirroring legacy `removedRoleIds.slice(0, 20)`.
pub const CONTAINMENT_ALERT_MAX_IDS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestructiveAction {
    MemberKick,
    MemberBan,
    ChannelDelete,
    RoleDelete,
    WebhookCreate,
    WebhookUpdate,
    WebhookDelete,
}

impl DestructiveAction {
    /// Legacy wire name (`member.kick`, `channel.delete`, …). Used in the
    /// staff alert only; never free user text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MemberKick => "member.kick",
            Self::MemberBan => "member.ban",
            Self::ChannelDelete => "channel.delete",
            Self::RoleDelete => "role.delete",
            Self::WebhookCreate => "webhook.create",
            Self::WebhookUpdate => "webhook.update",
            Self::WebhookDelete => "webhook.delete",
        }
    }

    /// Unsupported actions never acquire a destructive weight.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "member.kick" => Some(Self::MemberKick),
            "member.ban" => Some(Self::MemberBan),
            "channel.delete" => Some(Self::ChannelDelete),
            "role.delete" => Some(Self::RoleDelete),
            "webhook.create" => Some(Self::WebhookCreate),
            "webhook.update" => Some(Self::WebhookUpdate),
            "webhook.delete" => Some(Self::WebhookDelete),
            _ => None,
        }
    }

    #[must_use]
    pub fn weight(self) -> u64 {
        match self {
            Self::ChannelDelete | Self::RoleDelete => 3,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructiveAuditEvent {
    pub audit_entry_id: String,
    pub guild_id: String,
    pub executor_id: Option<String>,
    pub action: DestructiveAction,
    pub target_id: Option<String>,
    /// `None` represents an invalid timestamp; it is classified as stale.
    pub occurred_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentEventState {
    Observe,
    Contain,
    Ignored,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentReason {
    Counted,
    Stale,
    MissingExecutor,
    TrustedExecutor,
    ProtectedExecutor,
    Future,
}

impl ContainmentReason {
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Counted => "counted toward destructive-action heat",
            Self::Stale => "audit entry is too old to trigger a fresh incident",
            Self::MissingExecutor => "audit entry has no executor; refusing to guess",
            Self::TrustedExecutor => "executor is explicitly trusted",
            Self::ProtectedExecutor => "executor is protected from automatic containment",
            Self::Future => {
                "audit entry is more than 5 seconds in the future; refusing to count it"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainmentDisposition {
    pub state: ContainmentEventState,
    pub reason: ContainmentReason,
}

/// Evidence already accepted by the caller's durable audit-ID claim.
/// Preserve its recorded state: an ignored future entry must never become heat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedContainmentEvent {
    pub event: DestructiveAuditEvent,
    pub state: ContainmentEventState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContainmentPolicyError {
    #[error("containment window must be positive")]
    Window,
    #[error("containment maximum event age must be positive")]
    MaxAge,
    #[error("containment heat threshold must be positive")]
    Threshold,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentPolicy {
    window_ms: i64,
    max_age_ms: i64,
    heat_threshold: u64,
    pub bot_user_id: Option<String>,
    pub protected_user_ids: HashSet<String>,
    pub trusted_user_ids: HashSet<String>,
}

impl Default for ContainmentPolicy {
    fn default() -> Self {
        Self {
            window_ms: DEFAULT_CONTAINMENT_WINDOW_MS,
            max_age_ms: DEFAULT_CONTAINMENT_MAX_AGE_MS,
            heat_threshold: DEFAULT_CONTAINMENT_HEAT_THRESHOLD,
            bot_user_id: None,
            protected_user_ids: HashSet::new(),
            trusted_user_ids: HashSet::new(),
        }
    }
}

impl ContainmentPolicy {
    pub fn new(
        window_ms: i64,
        max_age_ms: i64,
        heat_threshold: u64,
    ) -> Result<Self, ContainmentPolicyError> {
        if window_ms <= 0 {
            return Err(ContainmentPolicyError::Window);
        }
        if max_age_ms <= 0 {
            return Err(ContainmentPolicyError::MaxAge);
        }
        if heat_threshold == 0 {
            return Err(ContainmentPolicyError::Threshold);
        }
        Ok(Self {
            window_ms,
            max_age_ms,
            heat_threshold,
            ..Self::default()
        })
    }

    /// Occurrence-heat window; also the incident cooldown from processing time.
    #[must_use]
    pub fn window_ms(&self) -> i64 {
        self.window_ms
    }

    #[must_use]
    pub fn heat_threshold(&self) -> u64 {
        self.heat_threshold
    }

    #[must_use]
    pub fn disposition(
        &self,
        event: &DestructiveAuditEvent,
        now_ms: i64,
    ) -> ContainmentDisposition {
        use ContainmentReason as Reason;
        let reason = match event.occurred_at_ms {
            None => Reason::Stale,
            Some(at) if i128::from(now_ms) - i128::from(at) > i128::from(self.max_age_ms) => {
                Reason::Stale
            }
            Some(_) => match event.executor_id.as_deref() {
                None => Reason::MissingExecutor,
                Some(id) if self.trusted_user_ids.contains(id) => Reason::TrustedExecutor,
                Some(id)
                    if self.protected_user_ids.contains(id)
                        || self.bot_user_id.as_deref() == Some(id) =>
                {
                    Reason::ProtectedExecutor
                }
                Some(_)
                    if event.occurred_at_ms.is_some_and(|at| {
                        i128::from(at) - i128::from(now_ms) > i128::from(CONTAINMENT_FUTURE_SKEW_MS)
                    }) =>
                {
                    Reason::Future
                }
                Some(_) => Reason::Counted,
            },
        };
        let state = match reason {
            Reason::Counted => ContainmentEventState::Observe,
            Reason::Stale => ContainmentEventState::Stale,
            _ => ContainmentEventState::Ignored,
        };
        ContainmentDisposition { state, reason }
    }

    /// Legacy `claimEvent` occurrence-time window, after a successful claim.
    /// `evidence` must include the newly claimed trigger and its recorded state.
    /// Duplicate IDs are counted once, but this is not a durable dedupe store.
    #[must_use]
    pub fn occurrence_heat(
        &self,
        trigger: &ClaimedContainmentEvent,
        evidence: &[ClaimedContainmentEvent],
        now_ms: i64,
    ) -> u64 {
        if trigger.state != ContainmentEventState::Observe
            || self.disposition(&trigger.event, now_ms).state != ContainmentEventState::Observe
        {
            return 0;
        }
        let (Some(at), Some(executor)) = (
            trigger.event.occurred_at_ms,
            trigger.event.executor_id.as_deref(),
        ) else {
            return 0;
        };
        let at = i128::from(at);
        let window = i128::from(self.window_ms);
        let upper = (at + window).min(i128::from(now_ms) + i128::from(CONTAINMENT_FUTURE_SKEW_MS));
        let mut seen = HashSet::new();
        let mut events: Vec<_> = evidence
            .iter()
            .filter(|row| {
                matches!(
                    row.state,
                    ContainmentEventState::Observe | ContainmentEventState::Contain
                ) && row.event.guild_id == trigger.event.guild_id
                    && row.event.executor_id.as_deref() == Some(executor)
            })
            .filter_map(|row| {
                let occurred = i128::from(row.event.occurred_at_ms?);
                (occurred > at - window
                    && occurred <= upper
                    && seen.insert(row.event.audit_entry_id.as_str()))
                .then_some((
                    occurred,
                    row.event.audit_entry_id.as_str(),
                    row.event.action.weight(),
                ))
            })
            .collect();
        events.sort_unstable_by_key(|(occurred, id, _)| (*occurred, *id));
        let mut heat = 0;
        let mut max_heat = 0;
        let mut left = 0;
        for (right, &(occurred, _, weight)) in events.iter().enumerate() {
            heat += weight;
            while left <= right && occurred - events[left].0 >= window {
                heat -= events[left].2;
                left += 1;
            }
            if occurred >= at {
                max_heat = max_heat.max(heat);
            }
        }
        max_heat
    }

    /// Propose, but do not claim, an incident. The adapter must recheck blockers
    /// and insert the incident atomically under a guild/executor lock.
    #[must_use]
    pub fn incident_candidate(
        &self,
        trigger: &ClaimedContainmentEvent,
        evidence: &[ClaimedContainmentEvent],
        incidents: &[ContainmentIncident],
        now_ms: i64,
    ) -> Option<ContainmentIncident> {
        match self.signal(trigger, evidence, incidents, now_ms) {
            ContainmentSignal::Alert(incident) => Some(incident),
            ContainmentSignal::Suppressed { .. } | ContainmentSignal::Quiet => None,
        }
    }

    /// Per-executor cooldown signal with a still-logged suppression proof.
    ///
    /// `Alert` carries the same proposal `incident_candidate` would return.
    /// `Suppressed` means heat reached threshold but a same-guild/executor
    /// incident is still blocking: the adapter must log (see
    /// [`CONTAINMENT_SUPPRESSED_EVENT`]) and must not post a second staff
    /// alert. `Quiet` means below threshold or unattributable: no log and no
    /// post. Cooldowns are isolated by guild *and* executor; expiry releases
    /// every state except `Uncertain` (see [`ContainmentIncident::blocks`]).
    #[must_use]
    pub fn signal(
        &self,
        trigger: &ClaimedContainmentEvent,
        evidence: &[ClaimedContainmentEvent],
        incidents: &[ContainmentIncident],
        now_ms: i64,
    ) -> ContainmentSignal {
        let heat = self.occurrence_heat(trigger, evidence, now_ms);
        if heat < self.heat_threshold {
            return ContainmentSignal::Quiet;
        }
        let Some(executor) = trigger.event.executor_id.as_deref() else {
            return ContainmentSignal::Quiet;
        };
        if incidents
            .iter()
            .any(|incident| incident.blocks(&trigger.event.guild_id, executor, now_ms))
        {
            return ContainmentSignal::Suppressed {
                guild_id: trigger.event.guild_id.clone(),
                executor_id: executor.to_owned(),
                heat,
                threshold: self.heat_threshold,
            };
        }
        ContainmentSignal::Alert(ContainmentIncident {
            id: trigger.event.audit_entry_id.clone(),
            guild_id: trigger.event.guild_id.clone(),
            executor_id: executor.to_owned(),
            heat,
            state: ContainmentIncidentState::Containing,
            cooldown_until_ms: now_ms.saturating_add(self.window_ms),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentIncidentState {
    Containing,
    Contained,
    DryRun,
    Refused,
    Uncertain,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentIncident {
    /// The triggering audit-entry ID, not a generated retry ID.
    pub id: String,
    pub guild_id: String,
    pub executor_id: String,
    pub heat: u64,
    pub state: ContainmentIncidentState,
    pub cooldown_until_ms: i64,
}

impl ContainmentIncidentState {
    /// Legacy `completeIncident` outcome spelling used in the staff alert head.
    #[must_use]
    pub fn outcome_label(self) -> &'static str {
        match self {
            Self::Containing => "containing",
            Self::Contained => "contained",
            Self::DryRun => "dry_run",
            Self::Refused => "refused",
            Self::Uncertain => "uncertain",
            Self::Failed => "failed",
        }
    }
}

impl ContainmentIncident {
    #[must_use]
    pub fn blocks(&self, guild_id: &str, executor_id: &str, now_ms: i64) -> bool {
        self.guild_id == guild_id
            && self.executor_id == executor_id
            && (self.state == ContainmentIncidentState::Uncertain
                || self.cooldown_until_ms > now_ms)
    }
}

/// Per-executor cooldown signal. The adapter must log `Alert` under
/// [`CONTAINMENT_ALERT_EVENT`] (with the staff text) and `Suppressed` under
/// [`CONTAINMENT_SUPPRESSED_EVENT`]; `Quiet` logs nothing. Only `Alert`
/// authorizes a staff-channel post, and that post must use
/// [`ContainmentAlert::staff_message`] (`MentionPolicy::None`: empty parse,
/// no explicit recipients, no DMs/pings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentSignal {
    Alert(ContainmentIncident),
    Suppressed {
        guild_id: String,
        executor_id: String,
        heat: u64,
        threshold: u64,
    },
    Quiet,
}

impl ContainmentSignal {
    #[must_use]
    pub fn log_event(&self) -> Option<&'static str> {
        match self {
            Self::Alert(_) => Some(CONTAINMENT_ALERT_EVENT),
            Self::Suppressed { .. } => Some(CONTAINMENT_SUPPRESSED_EVENT),
            Self::Quiet => None,
        }
    }

    #[must_use]
    pub fn staff_post_required(&self) -> bool {
        matches!(self, Self::Alert(_))
    }
}

/// Staff-alert proposal, not permission to post. Mirrors legacy
/// `formatContainmentAlert` (`src/discord/containmentAlert.ts`, blob
/// `b3a0e19e6a4230748459b962ae6efd5829005c78`): the head, executor/action/
/// target line, removed-role line (capped at [`CONTAINMENT_ALERT_MAX_IDS`]),
/// restore line, and flag-only footer. Only snowflake IDs, the legacy wire
/// action name, heat/threshold counters and the outcome label appear here —
/// never tokens, options, or user text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentAlert {
    pub guild_id: String,
    pub executor_id: Option<String>,
    pub action: DestructiveAction,
    pub target_id: Option<String>,
    pub heat: u64,
    pub threshold: u64,
    pub outcome: ContainmentIncidentState,
    pub removed_role_ids: Vec<String>,
}

impl ContainmentAlert {
    /// Build the proposal from the completed incident and its trigger. The
    /// adapter supplies the post-execution `outcome` and confirmed removals;
    /// `heat`/`threshold` come from the incident proposal.
    #[must_use]
    pub fn from_trigger(
        trigger: &DestructiveAuditEvent,
        heat: u64,
        threshold: u64,
        outcome: ContainmentIncidentState,
        removed_role_ids: Vec<String>,
    ) -> Self {
        Self {
            guild_id: trigger.guild_id.clone(),
            executor_id: trigger.executor_id.clone(),
            action: trigger.action,
            target_id: trigger.target_id.clone(),
            heat,
            threshold,
            outcome,
            removed_role_ids,
        }
    }

    /// Pinned staff text with [`MentionPolicy::None`]. The executor must send
    /// with empty mention parsing and no explicit recipients.
    #[must_use]
    pub fn staff_message(&self) -> StaffAlertMessage {
        let executor = self.executor_id.as_deref().unwrap_or("unknown");
        let target = self.target_id.as_deref().unwrap_or("unknown");
        let removed = if self.removed_role_ids.is_empty() {
            "No role removal was confirmed.".to_owned()
        } else {
            let listed: Vec<String> = self
                .removed_role_ids
                .iter()
                .take(CONTAINMENT_ALERT_MAX_IDS)
                .map(|id| format!("`{id}`"))
                .collect();
            let extra = self
                .removed_role_ids
                .len()
                .saturating_sub(CONTAINMENT_ALERT_MAX_IDS);
            let suffix = if extra > 0 {
                format!(" …and {extra} more")
            } else {
                String::new()
            };
            format!(
                "Removed dangerous roles ({}): {}{}.",
                self.removed_role_ids.len(),
                listed.join(" "),
                suffix
            )
        };
        let content = [
            format!(
                "**Anti-nuke {}** — destructive heat {}/{}.",
                self.outcome.outcome_label(),
                self.heat,
                self.threshold
            ),
            format!(
                "Executor: `{executor}` · action: `{}` · target: `{target}`.",
                self.action.as_str()
            ),
            removed,
            "Restore check: unavailable.".to_owned(),
            String::new(),
            "No member join was kicked or banned by this feature. Verify the executor and run the guarded staging restore procedure if drift is reported.".to_owned(),
        ]
        .join("\n");
        StaffAlertMessage {
            content,
            mentions: MentionPolicy::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentRole {
    pub id: String,
    pub position: i64,
    pub permissions: u64,
    pub managed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineRefusal {
    ManagedRole,
    Hierarchy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantinePlan {
    DryRun,
    Refused {
        reason: QuarantineRefusal,
        blocked_role_ids: Vec<String>,
    },
    /// Preserve guild-role snapshot order; remove sequentially, stopping on
    /// the first failure. Safe roles and @everyone are never included.
    RemoveRoles {
        role_ids: Vec<String>,
    },
}

/// A plan is not authorization to execute it. Staging, identity, session-mode
/// and successful durable incident claims remain the runtime adapter's fences.
#[must_use]
pub fn plan_quarantine(
    guild_id: &str,
    executor_role_ids: &[String],
    bot_role_ids: &[String],
    roles: &[ContainmentRole],
    dry_run: bool,
) -> QuarantinePlan {
    if dry_run {
        return QuarantinePlan::DryRun;
    }
    let bot_position = roles
        .iter()
        .filter(|role| bot_role_ids.contains(&role.id))
        .map(|role| role.position)
        .fold(0, i64::max);
    let dangerous: Vec<_> = roles
        .iter()
        .filter(|role| {
            executor_role_ids.contains(&role.id)
                && role.id != guild_id
                && role.permissions & DANGEROUS_PERMISSIONS != 0
        })
        .collect();
    let blocked: Vec<_> = dangerous
        .iter()
        .filter(|role| role.managed || role.position >= bot_position)
        .collect();
    if !blocked.is_empty() {
        return QuarantinePlan::Refused {
            reason: if blocked.iter().any(|role| role.managed) {
                QuarantineRefusal::ManagedRole
            } else {
                QuarantineRefusal::Hierarchy
            },
            blocked_role_ids: blocked.iter().map(|role| role.id.clone()).collect(),
        };
    }
    QuarantinePlan::RemoveRoles {
        role_ids: dangerous.iter().map(|role| role.id.clone()).collect(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineFailure {
    Rejected,
    RateLimited,
    Unavailable,
    Timeout,
    Other,
}

/// Interpret one role-removal response, without transport or retry machinery.
pub fn role_removal_status(status: u16) -> Result<(), QuarantineFailure> {
    match status {
        200 | 204 | 404 => Ok(()),
        429 => Err(QuarantineFailure::RateLimited),
        500.. => Err(QuarantineFailure::Unavailable),
        _ => Err(QuarantineFailure::Rejected),
    }
}

/// Confirmed removals survive a later failure; uncertainty never implies retry.
#[must_use]
pub fn quarantine_outcome(
    removed_role_ids: &[String],
    failure: Option<QuarantineFailure>,
) -> ContainmentIncidentState {
    match failure {
        None => ContainmentIncidentState::Contained,
        Some(
            QuarantineFailure::RateLimited
            | QuarantineFailure::Unavailable
            | QuarantineFailure::Timeout,
        ) => ContainmentIncidentState::Uncertain,
        Some(_) if !removed_role_ids.is_empty() => ContainmentIncidentState::Uncertain,
        Some(_) => ContainmentIncidentState::Refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn event(id: &str, action: DestructiveAction, at: i64) -> DestructiveAuditEvent {
        DestructiveAuditEvent {
            audit_entry_id: id.into(),
            guild_id: "guild".into(),
            executor_id: Some("executor".into()),
            action,
            target_id: Some("target".into()),
            occurred_at_ms: Some(at),
        }
    }

    fn claimed(id: &str, action: DestructiveAction, at: i64) -> ClaimedContainmentEvent {
        ClaimedContainmentEvent {
            event: event(id, action, at),
            state: ContainmentEventState::Observe,
        }
    }

    fn role(id: &str, position: i64, permissions: u64, managed: bool) -> ContainmentRole {
        ContainmentRole {
            id: id.into(),
            position,
            permissions,
            managed,
        }
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn actions_derive_legacy_weights_and_reject_unsupported_names() {
        for (name, weight) in [
            ("member.kick", 1),
            ("member.ban", 1),
            ("channel.delete", 3),
            ("role.delete", 3),
            ("webhook.create", 1),
            ("webhook.update", 1),
            ("webhook.delete", 1),
        ] {
            assert_eq!(DestructiveAction::from_name(name).unwrap().weight(), weight);
        }
        for name in [
            "message.delete",
            "member.unban",
            "channel.create",
            "role.update",
            "",
        ] {
            assert_eq!(DestructiveAction::from_name(name), None);
        }
    }

    #[test]
    fn defaults_and_policy_validation() {
        let policy = ContainmentPolicy::default();
        assert_eq!(
            (policy.window_ms, policy.max_age_ms, policy.heat_threshold),
            (60_000, 120_000, 5)
        );
        for window in [0, -1] {
            assert_eq!(
                ContainmentPolicy::new(window, 120_000, 5),
                Err(ContainmentPolicyError::Window)
            );
        }
        assert_eq!(
            ContainmentPolicy::new(60_000, 0, 5),
            Err(ContainmentPolicyError::MaxAge)
        );
        assert_eq!(
            ContainmentPolicy::new(60_000, -1, 5),
            Err(ContainmentPolicyError::MaxAge)
        );
        assert_eq!(
            ContainmentPolicy::new(60_000, 120_000, 0),
            Err(ContainmentPolicyError::Threshold)
        );
    }

    #[test]
    fn age_and_future_boundaries_are_strict() {
        let policy = ContainmentPolicy::default();
        for (at, reason) in [
            (NOW - 120_001, ContainmentReason::Stale),
            (NOW - 120_000, ContainmentReason::Counted),
            (NOW + 5_000, ContainmentReason::Counted),
            (NOW + 5_001, ContainmentReason::Future),
        ] {
            assert_eq!(
                policy
                    .disposition(&event("a", DestructiveAction::MemberKick, at), NOW)
                    .reason,
                reason
            );
        }
        let mut invalid = event("invalid", DestructiveAction::MemberKick, NOW);
        invalid.occurred_at_ms = None;
        assert_eq!(
            policy.disposition(&invalid, NOW).state,
            ContainmentEventState::Stale
        );
    }

    #[test]
    fn timestamp_extremes_do_not_overflow() {
        let policy = ContainmentPolicy::default();
        assert_eq!(
            policy
                .disposition(
                    &event("past", DestructiveAction::MemberKick, i64::MIN),
                    i64::MAX
                )
                .reason,
            ContainmentReason::Stale
        );
        assert_eq!(
            policy
                .disposition(
                    &event("future", DestructiveAction::MemberKick, i64::MAX),
                    i64::MIN
                )
                .reason,
            ContainmentReason::Future
        );
        let row = claimed("current", DestructiveAction::MemberKick, i64::MAX);
        assert_eq!(
            policy.occurrence_heat(&row, std::slice::from_ref(&row), i64::MAX),
            1
        );
    }

    #[test]
    fn unknown_trusted_protected_and_bot_executors_are_ignored() {
        let mut policy = ContainmentPolicy::default();
        policy.trusted_user_ids.insert("trusted".into());
        policy.protected_user_ids.insert("protected".into());
        policy.bot_user_id = Some("bot".into());
        for (executor, reason) in [
            (None, ContainmentReason::MissingExecutor),
            (Some("trusted"), ContainmentReason::TrustedExecutor),
            (Some("protected"), ContainmentReason::ProtectedExecutor),
            (Some("bot"), ContainmentReason::ProtectedExecutor),
        ] {
            let mut row = claimed("a", DestructiveAction::ChannelDelete, NOW);
            row.event.executor_id = executor.map(String::from);
            let disposition = policy.disposition(&row.event, NOW);
            assert_eq!(
                disposition,
                ContainmentDisposition {
                    state: ContainmentEventState::Ignored,
                    reason
                }
            );
            row.state = disposition.state;
            assert_eq!(
                policy.incident_candidate(&row, std::slice::from_ref(&row), &[], NOW),
                None
            );
        }
    }

    #[test]
    fn stale_and_trusted_reasons_take_precedence_over_other_exclusions() {
        let mut policy = ContainmentPolicy::default();
        policy.trusted_user_ids.insert("executor".into());
        policy.protected_user_ids.insert("executor".into());
        assert_eq!(
            policy
                .disposition(&event("a", DestructiveAction::MemberKick, NOW + 6_000), NOW)
                .reason,
            ContainmentReason::TrustedExecutor
        );
        assert_eq!(
            policy
                .disposition(
                    &event("a", DestructiveAction::MemberKick, NOW - 120_001),
                    NOW
                )
                .reason,
            ContainmentReason::Stale
        );
    }

    #[test]
    fn heat_four_observes_and_five_proposes_one_trigger_id() {
        let policy = ContainmentPolicy::default();
        let mut rows = vec![
            claimed("a", DestructiveAction::ChannelDelete, NOW - 1_000),
            claimed("b", DestructiveAction::MemberKick, NOW - 500),
        ];
        assert_eq!(policy.occurrence_heat(&rows[1], &rows, NOW), 4);
        assert_eq!(policy.incident_candidate(&rows[1], &rows, &[], NOW), None);
        rows.push(claimed("c", DestructiveAction::MemberKick, NOW));
        let incident = policy
            .incident_candidate(&rows[2], &rows, &[], NOW)
            .unwrap();
        assert_eq!(incident.id, "c");
        assert_eq!(incident.heat, 5);
        assert_eq!(incident.state, ContainmentIncidentState::Containing);
        assert_eq!(incident.cooldown_until_ms, NOW + 60_000);
        assert_eq!(
            policy.incident_candidate(&rows[2], &rows, &[incident], NOW),
            None
        );
    }

    #[test]
    fn heat_isolated_by_guild_executor_and_claimed_id() {
        let policy = ContainmentPolicy::default();
        let a = claimed("a", DestructiveAction::ChannelDelete, NOW);
        let mut other_executor = a.clone();
        other_executor.event.audit_entry_id = "b".into();
        other_executor.event.executor_id = Some("other".into());
        let mut other_guild = a.clone();
        other_guild.event.audit_entry_id = "c".into();
        other_guild.event.guild_id = "other".into();
        assert_eq!(
            policy.occurrence_heat(
                &a,
                &[a.clone(), a.clone(), other_executor, other_guild],
                NOW
            ),
            3
        );
    }

    #[test]
    fn occurrence_windows_ignore_delivery_order_and_processing_recency() {
        let policy = ContainmentPolicy::default();
        let rows = vec![
            claimed("newest", DestructiveAction::ChannelDelete, NOW - 1_000),
            claimed("middle", DestructiveAction::MemberKick, NOW - 2_000),
            claimed("oldest", DestructiveAction::MemberKick, NOW - 3_000),
        ];
        assert_eq!(policy.occurrence_heat(&rows[2], &rows, NOW), 5);
        let mut outside = rows.clone();
        outside.push(claimed(
            "fresh",
            DestructiveAction::MemberKick,
            NOW + 60_000,
        ));
        assert_eq!(
            policy.occurrence_heat(&outside[3], &outside, NOW + 60_000),
            1
        );
    }

    #[test]
    fn exact_window_width_is_excluded_on_both_sides() {
        let policy = ContainmentPolicy::default();
        let old = claimed("a", DestructiveAction::ChannelDelete, NOW - 60_000);
        let fresh = claimed("b", DestructiveAction::ChannelDelete, NOW);
        let rows = [old.clone(), fresh.clone()];
        assert_eq!(policy.occurrence_heat(&fresh, &rows, NOW), 3);
        assert_eq!(policy.occurrence_heat(&old, &rows, NOW), 3);
        let near = claimed("c", DestructiveAction::MemberKick, NOW - 59_999);
        assert_eq!(
            policy.occurrence_heat(&fresh, &[near, fresh.clone()], NOW),
            4
        );
    }

    #[test]
    fn sliding_heat_does_not_sum_the_whole_two_window_query() {
        let policy = ContainmentPolicy::default();
        let trigger = claimed("trigger", DestructiveAction::MemberKick, NOW);
        let rows = [
            claimed("left", DestructiveAction::ChannelDelete, NOW - 59_999),
            trigger.clone(),
            claimed("right", DestructiveAction::ChannelDelete, NOW + 5_000),
        ];
        assert_eq!(policy.occurrence_heat(&trigger, &rows, NOW), 4);
    }

    #[test]
    fn persisted_future_ignored_state_never_becomes_heat() {
        let policy = ContainmentPolicy::default();
        let mut future = claimed("future", DestructiveAction::ChannelDelete, NOW + 6_000);
        future.state = policy.disposition(&future.event, NOW).state;
        assert_eq!(future.state, ContainmentEventState::Ignored);
        let now = NOW + 1_001;
        let a = claimed("a", DestructiveAction::MemberKick, now);
        let b = claimed("b", DestructiveAction::MemberKick, now - 1_000);
        assert_eq!(policy.occurrence_heat(&a, &[future, a.clone(), b], now), 2);
    }

    #[test]
    fn ignored_and_stale_rows_do_not_count_but_contain_rows_do() {
        let policy = ContainmentPolicy::default();
        let a = claimed("a", DestructiveAction::MemberKick, NOW);
        let mut b = claimed("b", DestructiveAction::ChannelDelete, NOW);
        for state in [ContainmentEventState::Ignored, ContainmentEventState::Stale] {
            b.state = state;
            assert_eq!(policy.occurrence_heat(&a, &[a.clone(), b.clone()], NOW), 1);
        }
        b.state = ContainmentEventState::Contain;
        assert_eq!(policy.occurrence_heat(&a, &[a.clone(), b.clone()], NOW), 4);
        assert_eq!(policy.occurrence_heat(&b, &[a, b.clone()], NOW), 0);
    }

    #[test]
    fn future_query_upper_bound_is_inclusive_and_capped() {
        let policy = ContainmentPolicy::default();
        let trigger = claimed("a", DestructiveAction::MemberKick, NOW);
        let limit = claimed("b", DestructiveAction::MemberKick, NOW + 5_000);
        let beyond = claimed("c", DestructiveAction::ChannelDelete, NOW + 5_001);
        assert_eq!(
            policy.occurrence_heat(&trigger, &[trigger.clone(), limit, beyond], NOW),
            2
        );
    }

    #[test]
    fn cooldown_releases_at_expiry_except_uncertain() {
        let states = [
            ContainmentIncidentState::Containing,
            ContainmentIncidentState::Contained,
            ContainmentIncidentState::DryRun,
            ContainmentIncidentState::Refused,
            ContainmentIncidentState::Failed,
            ContainmentIncidentState::Uncertain,
        ];
        for state in states {
            let incident = ContainmentIncident {
                id: "a".into(),
                guild_id: "guild".into(),
                executor_id: "executor".into(),
                heat: 5,
                state,
                cooldown_until_ms: NOW + 60_000,
            };
            assert!(incident.blocks("guild", "executor", NOW + 59_999));
            assert_eq!(
                incident.blocks("guild", "executor", NOW + 60_000),
                state == ContainmentIncidentState::Uncertain
            );
            assert_eq!(
                incident.blocks("guild", "executor", NOW + 120_000),
                state == ContainmentIncidentState::Uncertain
            );
            assert!(!incident.blocks("other", "executor", NOW));
            assert!(!incident.blocks("guild", "other", NOW));
        }
    }

    #[test]
    fn custom_threshold_and_processing_time_cooldown() {
        let policy = ContainmentPolicy::new(1_000, 120_000, 3).unwrap();
        let row = claimed("a", DestructiveAction::RoleDelete, NOW - 30_000);
        let incident = policy
            .incident_candidate(&row, std::slice::from_ref(&row), &[], NOW)
            .unwrap();
        assert_eq!(incident.cooldown_until_ms, NOW + 1_000);
        assert!(!incident.blocks("guild", "executor", NOW + 1_000));
        let row = claimed("b", DestructiveAction::RoleDelete, NOW + 1_000);
        assert!(policy
            .incident_candidate(&row, std::slice::from_ref(&row), &[incident], NOW + 1_000)
            .is_some());
    }

    #[test]
    fn all_dangerous_permissions_are_planned_and_safe_everyone_unheld_roles_are_not() {
        let mut roles = vec![
            role("bot", 10, 0, false),
            role("guild", 0, DANGEROUS_PERMISSIONS, false),
            role("safe", 1, 1 << 11, false),
            role("unheld", 20, 1 << 3, true),
        ];
        let mut executor = ids(&["guild", "safe"]);
        let mut expected = Vec::new();
        for bit in [1, 2, 3, 4, 5, 27, 28, 40] {
            let id = format!("danger-{bit}");
            executor.push(id.clone());
            expected.push(id.clone());
            roles.push(role(&id, 2, 1 << bit, false));
        }
        assert_eq!(
            plan_quarantine("guild", &executor, &ids(&["bot"]), &roles, false),
            QuarantinePlan::RemoveRoles { role_ids: expected }
        );
    }

    #[test]
    fn one_blocked_role_refuses_the_entire_plan() {
        for (position, managed, reason) in [
            (2, true, QuarantineRefusal::ManagedRole),
            (10, false, QuarantineRefusal::Hierarchy),
            (11, false, QuarantineRefusal::Hierarchy),
        ] {
            let roles = [
                role("removable", 1, 1 << 2, false),
                role("blocked", position, 1 << 3, managed),
                role("bot", 10, 0, false),
            ];
            assert_eq!(
                plan_quarantine(
                    "guild",
                    &ids(&["removable", "blocked"]),
                    &ids(&["bot"]),
                    &roles,
                    false
                ),
                QuarantinePlan::Refused {
                    reason,
                    blocked_role_ids: ids(&["blocked"])
                }
            );
        }
    }

    #[test]
    fn managed_refusal_precedes_hierarchy_and_safe_high_roles_are_untouched() {
        let roles = [
            role("high", 10, 1 << 3, false),
            role("managed", 1, 1 << 3, true),
            role("safe", 100, 1 << 11, true),
            role("bot", 10, 0, false),
        ];
        assert_eq!(
            plan_quarantine(
                "guild",
                &ids(&["high", "managed", "safe"]),
                &ids(&["bot"]),
                &roles,
                false
            ),
            QuarantinePlan::Refused {
                reason: QuarantineRefusal::ManagedRole,
                blocked_role_ids: ids(&["high", "managed"])
            }
        );
        assert_eq!(
            plan_quarantine("guild", &ids(&["safe"]), &ids(&["bot"]), &roles, false),
            QuarantinePlan::RemoveRoles { role_ids: vec![] }
        );
    }

    #[test]
    fn absent_bot_hierarchy_does_not_permit_role_removal() {
        let roles = [role("danger", 1, 1 << 28, false)];
        assert!(matches!(
            plan_quarantine("guild", &ids(&["danger"]), &[], &roles, false),
            QuarantinePlan::Refused { .. }
        ));
        assert!(matches!(
            plan_quarantine(
                "guild",
                &ids(&["danger"]),
                &ids(&["missing"]),
                &roles,
                false
            ),
            QuarantinePlan::Refused { .. }
        ));
    }

    #[test]
    fn dry_run_has_no_role_removals_even_for_blocked_snapshots() {
        let roles = [role("danger", 100, 1 << 3, true)];
        assert_eq!(
            plan_quarantine("guild", &ids(&["danger"]), &[], &roles, true),
            QuarantinePlan::DryRun
        );
    }

    #[test]
    fn role_removal_statuses_preserve_404_success_and_transient_uncertainty() {
        for status in [200, 204, 404] {
            assert_eq!(role_removal_status(status), Ok(()));
        }
        for status in [400, 401, 403, 409] {
            assert_eq!(
                role_removal_status(status),
                Err(QuarantineFailure::Rejected)
            );
        }
        assert_eq!(
            role_removal_status(429),
            Err(QuarantineFailure::RateLimited)
        );
        for status in [500, 502, 503, 504] {
            assert_eq!(
                role_removal_status(status),
                Err(QuarantineFailure::Unavailable)
            );
        }
    }

    #[test]
    fn completion_retains_partial_removals_and_uncertain_failures() {
        for removed in [vec![], ids(&["confirmed"])] {
            assert_eq!(
                quarantine_outcome(&removed, None),
                ContainmentIncidentState::Contained
            );
            for error in [
                QuarantineFailure::RateLimited,
                QuarantineFailure::Unavailable,
                QuarantineFailure::Timeout,
            ] {
                assert_eq!(
                    quarantine_outcome(&removed, Some(error)),
                    ContainmentIncidentState::Uncertain
                );
            }
        }
        for error in [QuarantineFailure::Rejected, QuarantineFailure::Other] {
            assert_eq!(
                quarantine_outcome(&[], Some(error)),
                ContainmentIncidentState::Refused
            );
            assert_eq!(
                quarantine_outcome(&ids(&["confirmed"]), Some(error)),
                ContainmentIncidentState::Uncertain
            );
        }
    }
}
