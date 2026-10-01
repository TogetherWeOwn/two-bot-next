//! Claim admission and immutable planning for the shared command runtime.
//!
//! This feature service does not own a router, listener or HTTP client. The
//! command runtime will compose it with its existing ActionExecutor. Gateway
//! registration remains disabled until execution/repair acceptance lands.

use std::{
    collections::HashSet,
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use two_bot_core::self_roles::{
    plan_select_delta, plan_self_role_change, PanelMode, PlanRejection, RoleOperation,
    SelfRolePanel, SelfRolePlan, SettledOutcome,
};
use two_bot_cutover::self_role_store::{
    AuditEffects, EventClaim, PanelClaim, PanelClaimResult, PanelKey, SelfRoleAudit, SelfRoleStore,
    StoreError,
};
use two_bot_discord::{
    executor::self_roles::{SelfRoleRestError, SelfRoleSnapshot},
    ActionExecutor,
};

const STORE_TIMEOUT: Duration = Duration::from_secs(5);
const LANE_WAIT: Duration = Duration::from_secs(20);
const LANE_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub(crate) enum Selection {
    Button { option_key: String },
    Select { option_keys: Vec<String> },
    Reaction { option_key: String, remove: bool },
}

impl Selection {
    fn source(&self) -> PanelMode {
        match self {
            Self::Button { .. } => PanelMode::Button,
            Self::Select { .. } => PanelMode::Select,
            Self::Reaction { .. } => PanelMode::Reaction,
        }
    }

    fn option_key(&self) -> Option<&str> {
        match self {
            Self::Button { option_key } | Self::Reaction { option_key, .. } => Some(option_key),
            Self::Select { option_keys } => option_keys.first().map(String::as_str),
        }
    }

    fn operation(&self, exclusive: bool) -> RoleOperation {
        match self {
            Self::Reaction { remove: true, .. } => RoleOperation::Remove,
            Self::Button { .. } | Self::Reaction { .. } if !exclusive => RoleOperation::Add,
            _ => RoleOperation::Replace,
        }
    }

    fn plan(
        &self,
        panel: &SelfRolePanel,
        held: &HashSet<String>,
    ) -> Result<SelfRolePlan, PlanRejection> {
        if self.source() != panel.mode {
            return Err(PlanRejection::WrongSource {
                panel_id: panel.id.clone(),
                expected: panel.mode,
                received: self.source(),
            });
        }
        match self {
            Self::Button { option_key } => {
                plan_self_role_change(panel, option_key, held, PanelMode::Button, false)
            }
            Self::Reaction { option_key, remove } => {
                plan_self_role_change(panel, option_key, held, PanelMode::Reaction, *remove)
            }
            Self::Select { option_keys } => {
                let desired = option_keys
                    .iter()
                    .map(|key| {
                        panel
                            .options
                            .iter()
                            .find(|option| &option.key == key)
                            .map(|option| option.role_id.clone())
                            .ok_or_else(|| PlanRejection::UnknownOption {
                                panel_id: panel.id.clone(),
                                option_key: key.clone(),
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                plan_select_delta(panel, held, &desired)
            }
        }
    }
}

/// The adapter mints a reaction delivery id/order once, before detached work.
/// A retry must reuse both; interaction orders come from the signed snowflake.
#[derive(Debug, Clone)]
pub(crate) struct SelfRoleRequest {
    pub event_id: String,
    pub event_order: String,
    pub guild_id: String,
    pub member_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub selection: Selection,
}

/// Scalar diagnostics only. Store/provider detail and claim tokens never escape.
#[derive(Debug)]
pub(crate) enum RuntimeError {
    Store,
    Rest(SelfRoleRestError),
    Stale,
    InvalidSnapshot,
}

async fn store_io<T>(
    operation: impl Future<Output = Result<T, StoreError>>,
) -> Result<T, RuntimeError> {
    tokio::time::timeout(STORE_TIMEOUT, operation)
        .await
        .map_err(|_| RuntimeError::Store)?
        .map_err(|_| RuntimeError::Store)
}

/// Renewal is concurrent with REST/pacing/pool waits. Failure latches a shared
/// stop flag; it does not cancel an in-flight remote mutation or invent success.
/// Drop aborts the task, including on cancellation of the surrounding operation.
struct LeaseKeeper(tokio::task::JoinHandle<()>);

impl LeaseKeeper {
    fn start(
        store: SelfRoleStore,
        event: Option<EventClaim>,
        panel: Option<PanelClaim>,
        lost: Arc<AtomicBool>,
    ) -> Self {
        let renew_after_ms = event
            .as_ref()
            .map(|claim| claim.renew_after_ms)
            .or_else(|| panel.as_ref().map(|claim| claim.renew_after_ms))
            .expect("a renewal keeper has a claim");
        Self(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(renew_after_ms)).await;
                let result = match (&event, &panel) {
                    (Some(event), None) => store_io(store.renew_claim(event)).await,
                    (None, Some(panel)) => store_io(store.renew_panel_claim(panel)).await,
                    _ => unreachable!("one keeper per claim"),
                };
                if !matches!(result, Ok(true)) {
                    lost.store(true, Ordering::SeqCst);
                    return;
                }
            }
        }))
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) enum Admission {
    Duplicate,
    Ignored,
    Rejected(&'static str),
    Ready(Box<PreparedSelfRole>),
}

/// Retained through execution and final settlement. Neither lease renewal task
/// can outlive this owner. Remaining mutations are computed against the fresh
/// member, while the intended outcome is computed against the immutable before.
pub(crate) struct PreparedSelfRole {
    pub audit: SelfRoleAudit,
    pub event: EventClaim,
    pub panel: Option<PanelClaim>,
    pub plan: SelfRolePlan,
    pub remaining: SelfRolePlan,
    pub snapshot: SelfRoleSnapshot,
    store: SelfRoleStore,
    lost: Arc<AtomicBool>,
    _event_keeper: LeaseKeeper,
    _panel_keeper: Option<LeaseKeeper>,
}

impl PreparedSelfRole {
    pub async fn owns(&self) -> Result<bool, RuntimeError> {
        if self.lost.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if !store_io(self.store.owns_claim(&self.event)).await? {
            return Ok(false);
        }
        if let Some(panel) = &self.panel {
            if !store_io(self.store.owns_panel_claim(panel)).await? {
                return Ok(false);
            }
        }
        Ok(!self.lost.load(Ordering::SeqCst))
    }
}

struct PreparedPlans {
    snapshot: SelfRoleSnapshot,
    original: SelfRolePlan,
    remaining: SelfRolePlan,
}

pub(crate) struct SelfRoleRuntime {
    pub store: SelfRoleStore,
    pub executor: ActionExecutor,
    pub guild_id: String,
    pub bot_id: String,
}

impl SelfRoleRuntime {
    /// Claim first, exclusive lane second, authoritative policy/member last.
    /// No gateway/cache role set or permission mask participates in planning.
    pub async fn prepare(
        &self,
        request: &SelfRoleRequest,
        panel: &SelfRolePanel,
    ) -> Result<Admission, RuntimeError> {
        if request.guild_id != self.guild_id
            || request.channel_id != panel.channel_id
            || request.message_id != panel.message_id
        {
            return Ok(Admission::Ignored);
        }
        let mut audit = SelfRoleAudit {
            event_id: request.event_id.clone(),
            event_order: Some(request.event_order.clone()),
            guild_id: request.guild_id.clone(),
            member_id: request.member_id.clone(),
            panel_id: panel.id.clone(),
            source_id: request.message_id.clone(),
            option_key: request.selection.option_key().map(str::to_owned),
            role_id: request.selection.option_key().and_then(|key| {
                panel
                    .options
                    .iter()
                    .find(|option| option.key == key)
                    .map(|option| option.role_id.clone())
            }),
            source: request.selection.source(),
            operation: request.selection.operation(panel.exclusive),
            outcome: SettledOutcome::Rejected,
            code: None,
            reason: None,
            effects: AuditEffects::default(),
            desired_role_ids: vec![],
            pre_mutation_role_ids: vec![],
        };
        let Some(mut event) = store_io(self.store.claim_pending_audit(&audit)).await? else {
            return Ok(Admission::Duplicate);
        };
        let lost = Arc::new(AtomicBool::new(false));
        let event_keeper =
            LeaseKeeper::start(self.store.clone(), Some(event.clone()), None, lost.clone());
        let mut lane = None;
        if panel.exclusive {
            let key = PanelKey {
                guild_id: request.guild_id.clone(),
                member_id: request.member_id.clone(),
                panel_id: panel.id.clone(),
            };
            let deadline = tokio::time::Instant::now() + LANE_WAIT;
            loop {
                if lost.load(Ordering::SeqCst) || !store_io(self.store.owns_claim(&event)).await? {
                    return Err(RuntimeError::Stale);
                }
                match store_io(
                    self.store
                        .claim_panel(&key, Some((&request.event_id, &request.event_order))),
                )
                .await?
                {
                    PanelClaimResult::Acquired(claim) => {
                        lane = Some(claim);
                        break;
                    }
                    PanelClaimResult::Superseded(_) => {
                        return self
                            .reject(audit, &event, None, "superseded_by_later_event")
                            .await;
                    }
                    PanelClaimResult::Busy if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(LANE_BACKOFF).await;
                    }
                    PanelClaimResult::Busy => {
                        return self.reject(audit, &event, None, "panel_busy").await;
                    }
                }
            }
        }
        let panel_keeper = lane.as_ref().map(|claim| {
            LeaseKeeper::start(self.store.clone(), None, Some(claim.clone()), lost.clone())
        });
        // All failure paths below release their lane. Infrastructure/stale errors
        // leave the processing audit recoverable rather than falsely settled.
        let result = self
            .prepare_owned(request, panel, &mut audit, &mut event, &mut lane)
            .await;
        let PreparedPlans {
            snapshot,
            original: plan,
            remaining,
        } = match result {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(code)) => return self.reject(audit, &event, lane.as_ref(), code).await,
            Err(err) => {
                if let Some(lane) = &lane {
                    let _ = store_io(self.store.release_panel_claim(lane)).await;
                }
                return Err(err);
            }
        };
        let prepared = PreparedSelfRole {
            audit,
            event,
            panel: lane,
            plan,
            remaining,
            snapshot,
            store: self.store.clone(),
            lost,
            _event_keeper: event_keeper,
            _panel_keeper: panel_keeper,
        };
        if !prepared.owns().await? {
            return Err(RuntimeError::Stale);
        }
        Ok(Admission::Ready(Box::new(prepared)))
    }

    async fn prepare_owned(
        &self,
        request: &SelfRoleRequest,
        panel: &SelfRolePanel,
        audit: &mut SelfRoleAudit,
        event: &mut EventClaim,
        lane: &mut Option<PanelClaim>,
    ) -> Result<Result<PreparedPlans, &'static str>, RuntimeError> {
        if request.selection.source() == PanelMode::Reaction {
            self.executor
                .fetch_self_role_message(&panel.channel_id, &panel.message_id)
                .await
                .map_err(RuntimeError::Rest)?;
        }
        let snapshot = self
            .executor
            .fetch_self_role_snapshot(&request.guild_id, &request.member_id, &self.bot_id)
            .await
            .map_err(RuntimeError::Rest)?;
        if snapshot.member_is_bot {
            return Ok(Err("bot_member"));
        }
        let offered: Vec<_> = panel
            .options
            .iter()
            .map(|option| option.role_id.clone())
            .collect();
        if snapshot
            .validate(&request.guild_id, panel, &offered)
            .is_some()
        {
            return Ok(Err("unsafe_live_roles"));
        }
        // Seed only from a verifiable exclusive selection, never from new
        // uncommitted input. A malformed/multiple initial selection stays unknown.
        if let Some(lane) = lane.as_mut().filter(|lane| !lane.target.committed) {
            let held: Vec<_> = panel
                .options
                .iter()
                .filter(|option| snapshot.member_role_ids.contains(&option.role_id))
                .collect();
            if held.len() <= 1
                && !store_io(
                    self.store.set_panel_claim_option(
                        lane,
                        held.first().map(|option| option.key.as_str()),
                    ),
                )
                .await?
            {
                return Err(RuntimeError::Stale);
            }
        }
        if !event.intent_initialized {
            let initial = match request.selection.plan(panel, &snapshot.member_role_ids) {
                Ok(plan) => plan,
                Err(rejection) => return Ok(Err(rejection.code())),
            };
            let before = panel_roles(panel, &snapshot.member_role_ids);
            let mut desired: HashSet<_> = before.iter().cloned().collect();
            for role in &initial.remove_role_ids {
                desired.remove(role);
            }
            desired.extend(initial.add_role_ids);
            let desired = panel_roles(panel, &desired);
            if !store_io(self.store.initialize_intent(event, &desired, &before)).await? {
                return Err(RuntimeError::Stale);
            }
        }
        // Fail closed on catalogue drift, not by filtering persisted intent away.
        for ids in [&event.desired_role_ids, &event.pre_mutation_role_ids] {
            if ids.iter().any(|id| !offered.contains(id)) {
                return Err(RuntimeError::InvalidSnapshot);
            }
        }
        let before = event.pre_mutation_role_ids.iter().cloned().collect();
        // Decode the original surface against the IMMUTABLE before snapshot,
        // never against live roles. Require its target to match persisted intent.
        // This preserves explicit-remove no-op outcomes and the clicked option
        // on nonexclusive panels that already contain another held option.
        let plan = request
            .selection
            .plan(panel, &before)
            .map_err(|_| RuntimeError::InvalidSnapshot)?;
        let mut intended = before.clone();
        for role in &plan.remove_role_ids {
            intended.remove(role);
        }
        intended.extend(plan.add_role_ids.iter().cloned());
        if panel_roles(panel, &intended) != event.desired_role_ids {
            return Err(RuntimeError::InvalidSnapshot);
        }
        let remaining =
            plan_select_delta(panel, &snapshot.member_role_ids, &event.desired_role_ids)
                .map_err(|_| RuntimeError::InvalidSnapshot)?;
        audit.option_key = plan.option_key.clone();
        audit.role_id = plan.role_id.clone();
        audit.operation = plan.operation;
        audit.effects = event.effects.clone();
        audit.desired_role_ids = event.desired_role_ids.clone();
        audit.pre_mutation_role_ids = event.pre_mutation_role_ids.clone();
        Ok(Ok(PreparedPlans {
            snapshot,
            original: plan,
            remaining,
        }))
    }

    async fn reject(
        &self,
        mut audit: SelfRoleAudit,
        event: &EventClaim,
        lane: Option<&PanelClaim>,
        code: &'static str,
    ) -> Result<Admission, RuntimeError> {
        audit.code = Some(code.into());
        // Recovery retains all cumulative and unresolved evidence on refusals.
        audit.effects = event.effects.clone();
        let result = store_io(self.store.finish_audit(&audit, event)).await;
        if let Some(lane) = lane {
            store_io(self.store.release_panel_claim(lane)).await?;
        }
        result?;
        Ok(Admission::Rejected(code))
    }
}

fn panel_roles(panel: &SelfRolePanel, roles: &HashSet<String>) -> Vec<String> {
    panel
        .options
        .iter()
        .filter(|option| roles.contains(&option.role_id))
        .map(|option| option.role_id.clone())
        .collect()
}

#[cfg(test)]
#[path = "self_role_runtime_tests.rs"]
mod tests;
