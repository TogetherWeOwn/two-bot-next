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
    event_order_from_snowflake, plan_select_delta, plan_self_role_change, PanelMode, PlanRejection,
    RoleOperation, SelfRolePanel, SelfRolePlan, SettledOutcome,
};
use two_bot_cutover::self_role_store::{
    AuditEffects, EventClaim, PanelClaim, PanelClaimResult, PanelKey, RecoverableAudit,
    SelfRoleAudit, SelfRoleStore, StoreError,
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
    PendingExchange,
}

async fn store_io<T>(
    operation: impl Future<Output = Result<T, StoreError>>,
) -> Result<T, RuntimeError> {
    tokio::time::timeout(STORE_TIMEOUT, operation)
        .await
        .map_err(|_| RuntimeError::Store)?
        .map_err(|error| match error {
            StoreError::StaleClaim => RuntimeError::Stale,
            StoreError::PendingExchange => RuntimeError::PendingExchange,
            _ => RuntimeError::Store,
        })
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

/// Execution alone is not settlement. The caller must still atomically publish
/// the final audit/target, or reconcile a stale exclusive worker before replying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Execution {
    Applied,
    Compensated,
}

/// Stale-worker repair is authorized by a NEW maintenance lane, never by the
/// expired event or its old panel fence. It retains the committed chronology.
struct RepairLease {
    claim: PanelClaim,
    lost: Arc<AtomicBool>,
    _keeper: LeaseKeeper,
}

impl RepairLease {
    async fn owns(&self, store: &SelfRoleStore) -> Result<bool, RuntimeError> {
        if self.lost.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let owned = store_io(store.owns_panel_claim(&self.claim)).await?;
        Ok(owned && !self.lost.load(Ordering::SeqCst))
    }
}

async fn owns_step(
    prepared: &PreparedSelfRole,
    repair: Option<&RepairLease>,
) -> Result<bool, RuntimeError> {
    match repair {
        Some(repair) => repair.owns(&prepared.store).await,
        None => prepared.owns().await,
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
    /// Bounded convergence using singular operations. All reads and sends share
    /// the existing executor. A recovered rollback never resumes the old target.
    /// Keep processing claims on infrastructure/stale failures for recovery.
    pub async fn execute(
        &self,
        prepared: &mut PreparedSelfRole,
        panel: &SelfRolePanel,
    ) -> Result<Execution, RuntimeError> {
        if prepared.audit.guild_id != self.guild_id
            || prepared.audit.panel_id != panel.id
            || prepared.audit.source_id != panel.message_id
            || !prepared.event.intent_initialized
        {
            return Err(RuntimeError::InvalidSnapshot);
        }
        if prepared.event.exchange_pending {
            // A crashed/timeout exchange must not resume the original intention
            // or become settled success just because a later read looks right.
            prepared.event.compensating = true;
            self.checkpoint(prepared).await?;
        }
        // At most one remove and add per option in each phase. A moving external
        // target cannot keep an event in an unbounded mutation/retry loop.
        for _ in 0..(panel.options.len() * 4 + 2) {
            if !prepared.owns().await? {
                return Err(RuntimeError::Stale);
            }
            let snapshot = self
                .executor
                .fetch_self_role_snapshot(&self.guild_id, &prepared.audit.member_id, &self.bot_id)
                .await
                .map_err(RuntimeError::Rest)?;
            if !prepared.owns().await? {
                return Err(RuntimeError::Stale);
            }
            observe_prepared(prepared, &snapshot.member_role_ids);
            self.checkpoint(prepared).await?;
            let offered: Vec<_> = panel.options.iter().map(|o| o.role_id.clone()).collect();
            if snapshot.member_is_bot
                || snapshot.validate(&self.guild_id, panel, &offered).is_some()
            {
                prepared.event.compensating = true;
                self.checkpoint(prepared).await?;
                return Err(RuntimeError::Rest(SelfRoleRestError::Snapshot));
            }
            let target = if prepared.event.compensating {
                &prepared.event.pre_mutation_role_ids
            } else {
                &prepared.event.desired_role_ids
            };
            if target.iter().any(|id| !offered.contains(id)) {
                return Err(RuntimeError::InvalidSnapshot);
            }
            let remaining = plan_select_delta(panel, &snapshot.member_role_ids, target)
                .map_err(|_| RuntimeError::InvalidSnapshot)?;
            prepared.remaining = remaining;
            prepared.snapshot = snapshot;
            let next = prepared
                .remaining
                .remove_role_ids
                .first()
                .map(|id| (id.clone(), false))
                .or_else(|| {
                    prepared
                        .remaining
                        .add_role_ids
                        .first()
                        .map(|id| (id.clone(), true))
                });
            let Some((role, add)) = next else {
                return Ok(if prepared.event.compensating {
                    Execution::Compensated
                } else {
                    Execution::Applied
                });
            };
            self.execute_step(prepared, &role, add, None).await?;
        }
        prepared.event.compensating = true;
        self.checkpoint(prepared).await?;
        Err(RuntimeError::Rest(SelfRoleRestError::Ambiguous))
    }

    async fn checkpoint(&self, prepared: &mut PreparedSelfRole) -> Result<(), RuntimeError> {
        let stored = store_io(self.store.checkpoint_exchange(
            &prepared.event,
            &prepared.audit.effects,
            prepared.event.compensating,
            Some(prepared.event.exchange_pending),
        ))
        .await?;
        if !stored {
            // A later panel event may have rejected this audit while its remote
            // exchange was in flight. Evidence is allowed; more sends are not.
            store_io(self.store.record_superseded_exchange(
                &prepared.event,
                &prepared.audit.effects,
                Some(prepared.event.exchange_pending),
            ))
            .await?;
            return Err(RuntimeError::Stale);
        }
        prepared.event.effects = prepared.audit.effects.clone();
        Ok(())
    }

    async fn record_evidence(
        &self,
        event: &EventClaim,
        effects: &AuditEffects,
        compensating: bool,
        pending: bool,
    ) -> Result<bool, RuntimeError> {
        if store_io(
            self.store
                .checkpoint_exchange(event, effects, compensating, Some(pending)),
        )
        .await?
        {
            return Ok(true);
        }
        store_io(
            self.store
                .record_superseded_exchange(event, effects, Some(pending)),
        )
        .await
    }

    async fn execute_step(
        &self,
        prepared: &mut PreparedSelfRole,
        role: &str,
        add: bool,
        repair: Option<&RepairLease>,
    ) -> Result<(), RuntimeError> {
        let compensating = repair.is_some() || prepared.event.compensating;
        let prior_pending = prepared.event.exchange_pending;
        let mut attempted = prepared.audit.effects.clone();
        mark_attempt(&mut attempted, role, add);
        let journaled = AtomicBool::new(false);
        // Journal AFTER pacing; fence again after the database wait. A crash
        // after journal cannot be distinguished from an unknown in-flight send.
        let exchange = self
            .executor
            .self_role_step_journaled(
                &self.guild_id,
                &prepared.audit.member_id,
                role,
                add,
                || async {
                    owns_step(prepared, repair)
                        .await
                        .map_err(|_| SelfRoleRestError::StaleClaim)
                },
                || async {
                    if !self
                        .record_evidence(&prepared.event, &attempted, compensating, true)
                        .await
                        .map_err(|_| SelfRoleRestError::StaleClaim)?
                    {
                        return Err(SelfRoleRestError::StaleClaim);
                    }
                    journaled.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
        let exchange = match exchange {
            Ok(exchange) => exchange,
            Err(error) => {
                if journaled.load(Ordering::SeqCst) {
                    // The shared step returns Err only BEFORE send. Retain send
                    // intent history but do not invent remote uncertainty here.
                    clear_unresolved(&mut attempted, role, add);
                    if prior_pending {
                        attempted.unresolved_added_role_ids =
                            prepared.audit.effects.unresolved_added_role_ids.clone();
                        attempted.unresolved_removed_role_ids =
                            prepared.audit.effects.unresolved_removed_role_ids.clone();
                    }
                    prepared.audit.effects = attempted;
                    self.record_evidence(
                        &prepared.event,
                        &prepared.audit.effects,
                        compensating,
                        prior_pending,
                    )
                    .await?;
                }
                return Err(if error == SelfRoleRestError::StaleClaim {
                    RuntimeError::Stale
                } else {
                    RuntimeError::Rest(error)
                });
            }
        };
        prepared.audit.effects = attempted;
        // A later acknowledged rollback cannot erase an earlier unknown send.
        prepared.event.exchange_pending = prior_pending || !exchange.response_received;
        match exchange.result {
            Ok(()) => {
                if add {
                    prepared.snapshot.member_role_ids.insert(role.into());
                } else {
                    prepared.snapshot.member_role_ids.remove(role);
                }
                let held = prepared.snapshot.member_role_ids.clone();
                observe_prepared(prepared, &held);
                if compensating {
                    let restored = if add {
                        &mut prepared.audit.effects.compensated_added_role_ids
                    } else {
                        &mut prepared.audit.effects.compensated_removed_role_ids
                    };
                    push_role(restored, role);
                }
            }
            Err(SelfRoleRestError::Ambiguous) => {} // retain unresolved intent
            Err(_) if !prior_pending => clear_unresolved(&mut prepared.audit.effects, role, add),
            Err(_) => {}
        }
        if exchange.owned_after && exchange.result.is_err() && repair.is_none() {
            prepared.event.compensating = true;
        }
        if repair.is_some() {
            if !self
                .record_evidence(
                    &prepared.event,
                    &prepared.audit.effects,
                    compensating,
                    prepared.event.exchange_pending,
                )
                .await?
            {
                return Err(RuntimeError::Stale);
            }
            prepared.event.effects = prepared.audit.effects.clone();
        } else {
            self.checkpoint(prepared).await?;
        }
        if !exchange.owned_after || !owns_step(prepared, repair).await? {
            return Err(RuntimeError::Stale);
        }
        if compensating {
            exchange.result.map_err(RuntimeError::Rest)?;
        }
        Ok(())
    }

    /// Verify observed convergence again, then publish audit and exclusive
    /// target in one live, locked transaction. A provisional result is not a
    /// reply, and no snapshot can clear an interrupted exchange's uncertainty.
    pub async fn settle(
        &self,
        prepared: &mut PreparedSelfRole,
        panel: &SelfRolePanel,
        execution: Execution,
    ) -> Result<SettledOutcome, RuntimeError> {
        if prepared.audit.guild_id != self.guild_id
            || prepared.audit.panel_id != panel.id
            || prepared.audit.source_id != panel.message_id
            || !prepared.event.intent_initialized
            || (prepared.event.compensating != (execution == Execution::Compensated))
            || (panel.exclusive != prepared.panel.is_some())
        {
            return Err(RuntimeError::InvalidSnapshot);
        }
        if prepared.event.exchange_pending {
            return Err(RuntimeError::PendingExchange);
        }
        if !prepared.owns().await? {
            return Err(RuntimeError::Stale);
        }
        let snapshot = self
            .executor
            .fetch_self_role_snapshot(&self.guild_id, &prepared.audit.member_id, &self.bot_id)
            .await
            .map_err(RuntimeError::Rest)?;
        if !prepared.owns().await? {
            return Err(RuntimeError::Stale);
        }
        let offered: Vec<_> = panel.options.iter().map(|o| o.role_id.clone()).collect();
        let target = if prepared.event.compensating {
            &prepared.event.pre_mutation_role_ids
        } else {
            &prepared.event.desired_role_ids
        };
        if snapshot.member_is_bot
            || snapshot.validate(&self.guild_id, panel, &offered).is_some()
            || panel_roles(panel, &snapshot.member_role_ids) != *target
        {
            return Err(RuntimeError::InvalidSnapshot);
        }
        let option = if panel.exclusive {
            match target.as_slice() {
                [] => None,
                [role] => Some(
                    panel
                        .options
                        .iter()
                        .find(|o| &o.role_id == role)
                        .ok_or(RuntimeError::InvalidSnapshot)?
                        .key
                        .clone(),
                ),
                _ => return Err(RuntimeError::InvalidSnapshot),
            }
        } else {
            None
        };
        observe_prepared(prepared, &snapshot.member_role_ids);
        let outcome = if execution == Execution::Compensated {
            prepared.audit.code = Some("compensated".into());
            SettledOutcome::Rejected
        } else {
            prepared.plan.outcome
        };
        prepared.audit.outcome = outcome;
        let settled = if let Some(lane) = &mut prepared.panel {
            store_io(self.store.finish_audit_and_set_panel_option(
                &prepared.audit,
                &prepared.event,
                lane,
                option.as_deref(),
            ))
            .await?
        } else {
            store_io(
                self.store
                    .finish_owned_audit(&prepared.audit, &prepared.event),
            )
            .await?;
            true
        };
        if !settled {
            return Err(RuntimeError::Stale);
        }
        // The write above is authoritative. Release failure must not falsely
        // turn an already committed success into a rejected audit/user reply.
        if let Some(lane) = &prepared.panel {
            let _ = store_io(self.store.release_panel_claim(lane)).await;
        }
        prepared._event_keeper.0.abort();
        prepared._panel_keeper.take();
        Ok(outcome)
    }

    /// Audit a simulation without executing or committing the proposed target.
    /// A recovered mutation must never be disguised as a fresh dry run.
    pub async fn settle_dry_run(
        &self,
        prepared: &mut PreparedSelfRole,
        panel: &SelfRolePanel,
    ) -> Result<(), RuntimeError> {
        if prepared.audit.guild_id != self.guild_id
            || prepared.audit.panel_id != panel.id
            || prepared.audit.source_id != panel.message_id
            || !prepared.event.intent_initialized
            || (panel.exclusive != prepared.panel.is_some())
            || prepared.event.compensating
            || prepared.audit.effects != AuditEffects::default()
        {
            return Err(RuntimeError::InvalidSnapshot);
        }
        if prepared.event.exchange_pending {
            return Err(RuntimeError::PendingExchange);
        }
        if !prepared.owns().await? {
            return Err(RuntimeError::Stale);
        }
        prepared.audit.outcome = SettledOutcome::Rejected;
        prepared.audit.code = Some("dry_run".into());
        store_io(
            self.store
                .finish_owned_audit(&prepared.audit, &prepared.event),
        )
        .await?;
        // Keep the last actual committed target, not the simulated intention.
        if let Some(lane) = &prepared.panel {
            let _ = store_io(self.store.release_panel_claim(lane)).await;
        }
        prepared._event_keeper.0.abort();
        prepared._panel_keeper.take();
        Ok(())
    }

    /// Stop this owner without settling unresolved work. Fenced lane release
    /// cannot retire a different worker; the processing audit remains durable.
    pub async fn park(&self, prepared: &mut PreparedSelfRole) {
        prepared._event_keeper.0.abort();
        prepared._panel_keeper.take();
        if let Some(lane) = &prepared.panel {
            let _ = store_io(self.store.release_panel_claim(lane)).await;
        }
    }

    /// A stale worker may repair ONLY the last committed target under a new
    /// maintenance lane. Never restore its obsolete immutable before snapshot,
    /// reinterpret an uncommitted null as empty, or rewrite the winner's target.
    pub async fn reconcile_stale(
        &self,
        prepared: &mut PreparedSelfRole,
        panel: &SelfRolePanel,
    ) -> Result<(), RuntimeError> {
        if !panel.exclusive
            || prepared.audit.guild_id != self.guild_id
            || prepared.audit.panel_id != panel.id
            || prepared.audit.source_id != panel.message_id
        {
            return Err(RuntimeError::InvalidSnapshot);
        }
        if prepared.owns().await? {
            return Err(RuntimeError::InvalidSnapshot);
        }
        // Stop the old keepers before safely releasing only our old fence.
        prepared._event_keeper.0.abort();
        prepared._panel_keeper.take();
        let old = prepared
            .panel
            .as_ref()
            .ok_or(RuntimeError::InvalidSnapshot)?;
        let key = old.key.clone();
        store_io(self.store.release_panel_claim(old)).await?;
        let deadline = tokio::time::Instant::now() + LANE_WAIT;
        let claim = loop {
            match store_io(self.store.claim_panel(&key, None)).await? {
                PanelClaimResult::Acquired(claim) => break claim,
                PanelClaimResult::Busy if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(LANE_BACKOFF).await;
                }
                PanelClaimResult::Busy => return Err(RuntimeError::Stale),
                PanelClaimResult::Superseded(_) => return Err(RuntimeError::Stale),
            }
        };
        let lost = Arc::new(AtomicBool::new(false));
        let keeper =
            LeaseKeeper::start(self.store.clone(), None, Some(claim.clone()), lost.clone());
        let repair = RepairLease {
            claim,
            lost,
            _keeper: keeper,
        };
        let result = self.reconcile_owned(prepared, panel, &repair).await;
        // A stale release is harmless; cancellation falls back to lease expiry.
        let _ = store_io(self.store.release_panel_claim(&repair.claim)).await;
        result
    }

    async fn reconcile_owned(
        &self,
        prepared: &mut PreparedSelfRole,
        panel: &SelfRolePanel,
        repair: &RepairLease,
    ) -> Result<(), RuntimeError> {
        if !repair.claim.target.committed {
            return Err(RuntimeError::InvalidSnapshot);
        }
        let target = match repair.claim.target.option_key.as_deref() {
            None => vec![],
            Some(key) => vec![panel
                .options
                .iter()
                .find(|o| o.key == key)
                .ok_or(RuntimeError::InvalidSnapshot)?
                .role_id
                .clone()],
        };
        let offered: Vec<_> = panel.options.iter().map(|o| o.role_id.clone()).collect();
        for _ in 0..(panel.options.len() * 2 + 1) {
            if !repair.owns(&self.store).await? {
                return Err(RuntimeError::Stale);
            }
            let snapshot = self
                .executor
                .fetch_self_role_snapshot(&self.guild_id, &prepared.audit.member_id, &self.bot_id)
                .await
                .map_err(RuntimeError::Rest)?;
            if !repair.owns(&self.store).await? {
                return Err(RuntimeError::Stale);
            }
            observe_prepared(prepared, &snapshot.member_role_ids);
            if !self
                .record_evidence(
                    &prepared.event,
                    &prepared.audit.effects,
                    prepared.event.compensating,
                    prepared.event.exchange_pending,
                )
                .await?
            {
                return Err(RuntimeError::Stale);
            }
            if snapshot.member_is_bot
                || snapshot.validate(&self.guild_id, panel, &offered).is_some()
            {
                return Err(RuntimeError::Rest(SelfRoleRestError::Snapshot));
            }
            prepared.remaining = plan_select_delta(panel, &snapshot.member_role_ids, &target)
                .map_err(|_| RuntimeError::InvalidSnapshot)?;
            prepared.snapshot = snapshot;
            let next = prepared
                .remaining
                .remove_role_ids
                .first()
                .map(|id| (id.clone(), false))
                .or_else(|| {
                    prepared
                        .remaining
                        .add_role_ids
                        .first()
                        .map(|id| (id.clone(), true))
                });
            let Some((role, add)) = next else {
                return if prepared.event.exchange_pending {
                    Err(RuntimeError::PendingExchange)
                } else {
                    Ok(())
                };
            };
            self.execute_step(prepared, &role, add, Some(repair))
                .await?;
        }
        Err(RuntimeError::Rest(SelfRoleRestError::Ambiguous))
    }

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
        let audit = SelfRoleAudit {
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
        self.prepare_audit(audit, panel, Some(request.selection.clone()))
            .await
    }

    /// Recovery is driven by durable discovery, not a retained gateway payload.
    /// Discovery grants no ownership: the claim reloads intent/effects and rotates
    /// the generation before reconstructing a surface against immutable intent.
    pub async fn recover(
        &self,
        candidate: &RecoverableAudit,
        panel: &SelfRolePanel,
    ) -> Result<Admission, RuntimeError> {
        if candidate.guild_id != self.guild_id
            || candidate.panel_id != panel.id
            || candidate.source_id != panel.message_id
            || candidate.source != panel.mode
        {
            return Ok(Admission::Ignored);
        }
        let audit = SelfRoleAudit {
            event_id: candidate.event_id.clone(),
            event_order: candidate.event_order.clone(),
            guild_id: candidate.guild_id.clone(),
            panel_id: candidate.panel_id.clone(),
            member_id: candidate.member_id.clone(),
            source_id: candidate.source_id.clone(),
            option_key: candidate.option_key.clone(),
            role_id: candidate.role_id.clone(),
            source: candidate.source,
            operation: candidate.operation,
            outcome: SettledOutcome::Rejected,
            code: None,
            reason: None,
            effects: AuditEffects::default(),
            desired_role_ids: vec![],
            pre_mutation_role_ids: vec![],
        };
        self.prepare_audit(audit, panel, None).await
    }

    pub async fn recovery_candidates(
        &self,
        panel: &SelfRolePanel,
        limit: usize,
    ) -> Result<Vec<RecoverableAudit>, RuntimeError> {
        store_io(self.store.recoverable_audits(
            &self.guild_id,
            &panel.id,
            &panel.message_id,
            panel.mode,
            limit,
        ))
        .await
    }

    async fn prepare_audit(
        &self,
        mut audit: SelfRoleAudit,
        panel: &SelfRolePanel,
        selection: Option<Selection>,
    ) -> Result<Admission, RuntimeError> {
        let Some(mut event) = store_io(self.store.claim_pending_audit(&audit)).await? else {
            return Ok(Admission::Duplicate);
        };
        let selection = match selection {
            Some(selection) => selection,
            None if !event.intent_initialized => {
                // The original multi-select input was not durably initialized.
                // Reject before REST, rather than inventing an empty selection.
                return self
                    .reject(audit, &event, None, "interrupted_before_intent")
                    .await;
            }
            None => recovery_selection(&audit, &event, panel)?,
        };
        let request = SelfRoleRequest {
            event_id: audit.event_id.clone(),
            event_order: audit
                .event_order
                .clone()
                .or_else(|| event_order_from_snowflake(&audit.event_id))
                .ok_or(RuntimeError::InvalidSnapshot)?,
            guild_id: audit.guild_id.clone(),
            member_id: audit.member_id.clone(),
            channel_id: panel.channel_id.clone(),
            message_id: audit.source_id.clone(),
            selection,
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
            .prepare_owned(&request, panel, &mut audit, &mut event, &mut lane)
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
        let recovery_target = if event.compensating || event.exchange_pending {
            &event.pre_mutation_role_ids
        } else {
            &event.desired_role_ids
        };
        let remaining = plan_select_delta(panel, &snapshot.member_role_ids, recovery_target)
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
        if event.exchange_pending || event.compensating || event.effects != AuditEffects::default()
        {
            // Policy/refusal during recovery cannot silently settle unfinished
            // mutation or discard its pending remote work.
            if let Some(lane) = lane {
                store_io(self.store.release_panel_claim(lane)).await?;
            }
            return Err(if event.exchange_pending {
                RuntimeError::PendingExchange
            } else {
                RuntimeError::InvalidSnapshot
            });
        }
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

fn recovery_selection(
    audit: &SelfRoleAudit,
    event: &EventClaim,
    panel: &SelfRolePanel,
) -> Result<Selection, RuntimeError> {
    if !event.intent_initialized || audit.source != panel.mode {
        return Err(RuntimeError::InvalidSnapshot);
    }
    let offered: Vec<_> = panel.options.iter().map(|o| &o.role_id).collect();
    if [&event.desired_role_ids, &event.pre_mutation_role_ids]
        .iter()
        .any(|ids| ids.iter().any(|id| !offered.contains(&id)))
    {
        return Err(RuntimeError::InvalidSnapshot);
    }
    if audit.source == PanelMode::Select {
        // Initialized empty is meaningful. Before initialization, recovery must
        // refuse rather than interpreting the default arrays as empty input.
        let option_keys = event
            .desired_role_ids
            .iter()
            .map(|id| {
                panel
                    .options
                    .iter()
                    .find(|o| &o.role_id == id)
                    .map(|o| o.key.clone())
                    .ok_or(RuntimeError::InvalidSnapshot)
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Selection::Select { option_keys });
    }
    let key = audit
        .option_key
        .as_ref()
        .ok_or(RuntimeError::InvalidSnapshot)?;
    let option = panel
        .options
        .iter()
        .find(|o| &o.key == key)
        .ok_or(RuntimeError::InvalidSnapshot)?;
    if audit.role_id.as_ref() != Some(&option.role_id) {
        return Err(RuntimeError::InvalidSnapshot);
    }
    Ok(match audit.source {
        PanelMode::Button => Selection::Button {
            option_key: key.clone(),
        },
        PanelMode::Reaction => Selection::Reaction {
            option_key: key.clone(),
            remove: audit.operation == RoleOperation::Remove,
        },
        PanelMode::Select => unreachable!(),
    })
}

fn push_role(ids: &mut Vec<String>, role: &str) {
    if !ids.iter().any(|id| id == role) {
        ids.push(role.into());
    }
}

fn mark_attempt(effects: &mut AuditEffects, role: &str, add: bool) {
    let (attempts, unresolved) = if add {
        (
            &mut effects.attempted_added_role_ids,
            &mut effects.unresolved_added_role_ids,
        )
    } else {
        (
            &mut effects.attempted_removed_role_ids,
            &mut effects.unresolved_removed_role_ids,
        )
    };
    push_role(attempts, role);
    push_role(unresolved, role);
}

fn clear_unresolved(effects: &mut AuditEffects, role: &str, add: bool) {
    let unresolved = if add {
        &mut effects.unresolved_added_role_ids
    } else {
        &mut effects.unresolved_removed_role_ids
    };
    unresolved.retain(|id| id != role);
}

/// Evidence is the net state of ATTEMPTED panel roles versus immutable before,
/// not a guess that every difference on Discord was caused by this event.
fn observe_effects(effects: &mut AuditEffects, before: &[String], held: &HashSet<String>) {
    effects.added_role_ids = effects
        .attempted_added_role_ids
        .iter()
        .filter(|id| !before.contains(id) && held.contains(*id))
        .cloned()
        .collect();
    effects.removed_role_ids = effects
        .attempted_removed_role_ids
        .iter()
        .filter(|id| before.contains(id) && !held.contains(*id))
        .cloned()
        .collect();
    effects.unresolved_added_role_ids.clear();
    effects.unresolved_removed_role_ids.clear();
}

/// Observation can replace net effects, but cannot clear unknown in-flight
/// work. Only the original completed response/no-send path clears pending.
fn observe_prepared(prepared: &mut PreparedSelfRole, held: &HashSet<String>) {
    let added = prepared.audit.effects.unresolved_added_role_ids.clone();
    let removed = prepared.audit.effects.unresolved_removed_role_ids.clone();
    observe_effects(
        &mut prepared.audit.effects,
        &prepared.event.pre_mutation_role_ids,
        held,
    );
    if prepared.event.exchange_pending {
        prepared.audit.effects.unresolved_added_role_ids = added;
        prepared.audit.effects.unresolved_removed_role_ids = removed;
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
