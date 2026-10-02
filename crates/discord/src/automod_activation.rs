//! Shared automod activation: claim → inspect → dry-run/enforce → executor.
//!
//! The shared async gateway calls [`AutomodActivation::process`] once per
//! translated delivery, in gateway order, BEFORE the S3 funnel, then hands the
//! returned [`FunnelDisposition`] to `Pipeline::handle_with_message_disposition`.
//! Every Discord read and mutation goes through the shared [`ActionExecutor`];
//! there is no private client, dispatcher, router or timer here. The
//! synchronous runtime sits behind a mutex that is never held across an await.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use serde_json::Value;
use twilight_model::channel::Message;
use two_bot_core::automod_runtime::{
    AutomodClaimLedger, AutomodEffect, AutomodMatch, AutomodRuntime, CompletionKind, DeliveryKey,
    FunnelDisposition, Inspection, LedgerClaim, MessageDelivery, MessageSubject, PlanOutcome,
    StoredOutcome, TargetFacts, TargetGate, ViolationRecord,
};
use two_bot_core::containment::DANGEROUS_PERMISSIONS;
use two_bot_core::{ModerationPolicy, ModerationTarget};

use crate::automod::with_fetched_message;
use crate::executor::{timeout_until_iso, ActionExecutor};

const PERM_ADMINISTRATOR: u64 = 1 << 3;

/// Authoritative REST message plus its author's current guild role IDs.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub message: Message,
    pub author_role_ids: Vec<String>,
}

/// Fresh resolver facts for enrichment and target protection. `None` is a
/// failed or uncertain lookup, never an empty role list or an unprotected
/// target.
pub trait AutomodFacts: Send + Sync {
    fn fetch_message(
        &self,
        guild_id: &str,
        channel_id: &str,
        message_id: &str,
    ) -> impl Future<Output = Option<FetchedMessage>> + Send;

    fn target_facts(
        &self,
        subject: &MessageSubject,
    ) -> impl Future<Output = Option<TargetFacts>> + Send;
}

/// Why a claim was kept unsettled. Each case needs recorded reconciliation;
/// nothing here is retried, leased or resent automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainReason {
    /// The claim store failed after this delivery may have been counted.
    Ledger,
    /// An enforcing match could not be preserved before target resolution.
    PreserveRefused,
    /// The counted delivery planned no executable outcome.
    PlanUnavailable,
    /// `mark_mutation_started` did not commit: nothing was sent.
    FenceRefused,
    /// The delete response was uncertain: no follow-up sanction was sent.
    UncertainDelete,
    /// The timeout response was uncertain after a confirmed delete.
    UncertainTimeout,
    /// The receipt could not be committed to the claim.
    CompletionRefused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationOutcome {
    /// Outside the activation fence: no claim, ordinary funnel path.
    Bypassed,
    /// An in-flight or settled delivery: nothing inspected, awarded or sent.
    Duplicate,
    /// Enrichment, identity, claim or target facts were unavailable. Nothing
    /// was counted or sent; an unmutated claim was released.
    Unavailable,
    /// The typed receipt was committed. `deleted` is a confirmed REST result.
    Completed(StoredOutcome),
    Retained(RetainReason),
}

/// One processed delivery. `disposition` is handed once to the funnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    pub disposition: FunnelDisposition,
    pub outcome: ActivationOutcome,
}

pub struct AutomodActivation<L, F> {
    runtime: Mutex<AutomodRuntime>,
    ledger: L,
    facts: F,
    executor: ActionExecutor,
}

impl<L: AutomodClaimLedger, F: AutomodFacts> AutomodActivation<L, F> {
    #[must_use]
    pub fn new(runtime: AutomodRuntime, ledger: L, facts: F, executor: ActionExecutor) -> Self {
        Self {
            runtime: Mutex::new(runtime),
            ledger,
            facts,
            executor,
        }
    }

    /// Shared maintenance tick. Expires idle repeat history; no I/O.
    pub fn expire_repeat_history(&self, now_ms: u64) {
        self.runtime().expire_repeat_history(now_ms);
    }

    /// Process one delivery. Callers serialize deliveries in gateway order so
    /// repeat history observes creates in order. `at_iso` stamps the ledger.
    pub async fn process(&self, delivery: MessageDelivery, at_iso: &str) -> Activation {
        let kind = delivery.kind;
        let (admitted, dry_run) = {
            let runtime = self.runtime();
            (
                runtime.admits(delivery.guild_id.as_deref()),
                runtime.dry_run(),
            )
        };
        if !admitted {
            return activation(kind.funnel(false), ActivationOutcome::Bypassed);
        }
        // Unknown never becomes acceptance: a create keeps raw capture only.
        let unavailable = activation(kind.funnel(true), ActivationOutcome::Unavailable);
        let Some(delivery) = self.enrich(delivery).await else {
            return unavailable;
        };
        let Some(key) = DeliveryKey::from_delivery(&delivery, dry_run) else {
            return unavailable;
        };
        let claim = match self.ledger.ledger_claim(&key).await {
            Ok(claim) => claim,
            Err(error) => {
                tracing::warn!(%error, "automod claim unavailable");
                return unavailable;
            }
        };
        let (claim, matched) = match claim {
            LedgerClaim::InFlight | LedgerClaim::Replayed(_) => {
                return activation(FunnelDisposition::None, ActivationOutcome::Duplicate);
            }
            // The store never preserves a dry-run decision; replay without
            // re-inspecting so swept repeat history cannot change the verdict.
            LedgerClaim::Preserved(claim, matched) => (claim, matched),
            LedgerClaim::Acquired(claim) => {
                let inspection = self.runtime().inspect(&delivery);
                match inspection {
                    Inspection::Accepted(funnel) => {
                        let receipt = receipt(false, false, CompletionKind::Accepted);
                        return self.settle(&claim, funnel, receipt).await;
                    }
                    Inspection::Matched(matched) if dry_run => {
                        return self.dry_run(&claim, &matched).await;
                    }
                    Inspection::Matched(matched) => {
                        match self.ledger.ledger_preserve(&claim, &matched).await {
                            Ok(true) => (claim, matched),
                            // Fail closed: never release an undecided match.
                            _ => return retain(&matched, RetainReason::PreserveRefused),
                        }
                    }
                    Inspection::Ignore
                    | Inspection::FetchMessage { .. }
                    | Inspection::Unavailable => {
                        self.release(&claim).await;
                        return unavailable;
                    }
                }
            }
        };
        self.enforce(&claim, &matched, at_iso).await
    }

    fn runtime(&self) -> MutexGuard<'_, AutomodRuntime> {
        self.runtime.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Partial updates and role-less creates need the authoritative message
    /// and its author's roles. A failed lookup is not an empty snapshot.
    async fn enrich(&self, delivery: MessageDelivery) -> Option<MessageDelivery> {
        if delivery.snapshot.is_some() {
            return Some(delivery);
        }
        let guild_id = delivery.guild_id.as_deref()?;
        let fetched = self
            .facts
            .fetch_message(guild_id, &delivery.channel_id, &delivery.message_id)
            .await?;
        with_fetched_message(&delivery, &fetched.message, &fetched.author_role_ids)
    }

    /// Capture only: no preservation, target fetch, count or mutation.
    async fn dry_run(&self, claim: &L::Claim, matched: &AutomodMatch) -> Activation {
        let plan = self.runtime().plan(
            matched,
            ViolationRecord {
                count: 0,
                inserted: false,
            },
            None,
        );
        if plan.outcome != PlanOutcome::DryRun || !plan.effects.is_empty() {
            self.release(claim).await;
            return activation(matched.funnel, ActivationOutcome::Unavailable);
        }
        let receipt = receipt(true, false, CompletionKind::DryRun);
        self.settle(claim, plan.funnel, receipt).await
    }

    /// Protection before counting; count once; delete first, then the ladder.
    async fn enforce(&self, claim: &L::Claim, matched: &AutomodMatch, at_iso: &str) -> Activation {
        let facts = self.facts.target_facts(&matched.subject).await;
        let gate = self.runtime().target_gate(matched, facts.as_ref());
        if !matches!(gate, TargetGate::Allowed | TargetGate::Protected(_)) {
            self.release(claim).await;
            return activation(matched.funnel, ActivationOutcome::Unavailable);
        }
        let violation = match self
            .ledger
            .ledger_record(claim, &matched.subject, matched.filter, at_iso)
            .await
        {
            Ok(violation) => violation,
            Err(error) => {
                tracing::warn!(%error, "automod violation ledger failed");
                // Release refuses a counted claim, so an ambiguous commit
                // stays as reconciliation evidence.
                if self.release(claim).await {
                    return activation(matched.funnel, ActivationOutcome::Unavailable);
                }
                return retain(matched, RetainReason::Ledger);
            }
        };
        let plan = self.runtime().plan(matched, violation, facts.as_ref());
        let refused = match plan.outcome {
            PlanOutcome::AlreadyProcessed => {
                let receipt = receipt(true, false, CompletionKind::AlreadyProcessed);
                return self.settle(claim, plan.funnel, receipt).await;
            }
            PlanOutcome::Protected(_) => {
                let receipt = receipt(true, false, CompletionKind::Protected);
                return self.settle(claim, plan.funnel, receipt).await;
            }
            PlanOutcome::DryRun | PlanOutcome::Unavailable => {
                return retain(matched, RetainReason::PlanUnavailable);
            }
            PlanOutcome::Ready => false,
            PlanOutcome::SanctionRefused => true,
        };
        if !matches!(self.ledger.ledger_mark_started(claim).await, Ok(true)) {
            return retain(matched, RetainReason::FenceRefused);
        }
        let subject = &plan.subject;
        let reason = format!("automod: {}", plan.filter);
        match self
            .executor
            .delete_message(&subject.channel_id, &subject.message_id, &reason)
            .await
        {
            Ok(()) => {}
            // No sanction without the exact deletion it accompanies.
            Err(error) if error.is_safe_pre_mutation() => {
                let receipt = receipt(true, false, CompletionKind::SanctionRefused);
                return self.settle(claim, plan.funnel, receipt).await;
            }
            Err(_) => return retain(matched, RetainReason::UncertainDelete),
        }
        let outcome = if refused {
            CompletionKind::SanctionRefused
        } else {
            match plan.effects.get(1) {
                None => CompletionKind::Deleted,
                // The counted ledger row is the warning; Discord gets no call.
                Some(AutomodEffect::WarnMember) => CompletionKind::Warned,
                Some(AutomodEffect::TimeoutMember { seconds }) => {
                    let until = timeout_until_iso(*seconds);
                    match self
                        .executor
                        .timeout_member(
                            &subject.guild_id,
                            &subject.author_id,
                            Some(&until),
                            &reason,
                        )
                        .await
                    {
                        Ok(()) => CompletionKind::TimedOut,
                        Err(error) if error.is_safe_pre_mutation() => {
                            CompletionKind::SanctionRefused
                        }
                        Err(_) => return retain(matched, RetainReason::UncertainTimeout),
                    }
                }
                Some(AutomodEffect::DeleteMessage) => CompletionKind::Deleted,
            }
        };
        self.settle(claim, plan.funnel, receipt(true, true, outcome))
            .await
    }

    async fn settle(
        &self,
        claim: &L::Claim,
        funnel: FunnelDisposition,
        receipt: StoredOutcome,
    ) -> Activation {
        match self.ledger.ledger_complete(claim, &receipt).await {
            Ok(true) => activation(funnel, ActivationOutcome::Completed(receipt)),
            Ok(false) => activation(
                funnel,
                ActivationOutcome::Retained(RetainReason::CompletionRefused),
            ),
            Err(error) => {
                tracing::warn!(%error, "automod completion failed");
                activation(
                    funnel,
                    ActivationOutcome::Retained(RetainReason::CompletionRefused),
                )
            }
        }
    }

    /// True only when the store released the unmutated, uncounted claim.
    async fn release(&self, claim: &L::Claim) -> bool {
        match self.ledger.ledger_release(claim).await {
            Ok(released) => released,
            Err(error) => {
                tracing::warn!(%error, "automod claim release failed");
                false
            }
        }
    }
}

fn activation(disposition: FunnelDisposition, outcome: ActivationOutcome) -> Activation {
    Activation {
        disposition,
        outcome,
    }
}

fn receipt(matched: bool, deleted: bool, outcome: CompletionKind) -> StoredOutcome {
    StoredOutcome {
        matched,
        deleted,
        outcome,
    }
}

fn retain(matched: &AutomodMatch, reason: RetainReason) -> Activation {
    tracing::warn!(
        guild_id = %matched.subject.guild_id,
        message_id = %matched.subject.message_id,
        ?reason,
        "automod claim retained for reconciliation"
    );
    activation(matched.funnel, ActivationOutcome::Retained(reason))
}

/// Production facts over the shared executor's strict reads. Owen and the
/// configured protected roles come from authoritative configuration; roles
/// carrying dangerous staff permissions are protected even when unlisted.
pub struct RestAutomodFacts {
    executor: ActionExecutor,
    owen_user_id: String,
    protected_role_ids: HashSet<String>,
    bot_user_id: OnceLock<String>,
}

impl RestAutomodFacts {
    #[must_use]
    pub fn new(
        executor: ActionExecutor,
        owen_user_id: String,
        protected_role_ids: HashSet<String>,
    ) -> Self {
        Self {
            executor,
            owen_user_id,
            protected_role_ids,
            bot_user_id: OnceLock::new(),
        }
    }

    async fn get(&self, path: &str) -> Option<Value> {
        self.executor.get_json_strict(path).await.ok().flatten()
    }

    async fn bot_user_id(&self) -> Option<String> {
        if let Some(id) = self.bot_user_id.get() {
            return Some(id.clone());
        }
        let id = self.executor.current_bot_user_id().await.ok()?.to_string();
        Some(self.bot_user_id.get_or_init(|| id).clone())
    }

    async fn resolve_target(&self, subject: &MessageSubject) -> Option<TargetFacts> {
        let guild_id = subject.guild_id.as_str();
        let guild = self.get(&format!("/guilds/{guild_id}")).await?;
        let owner_id = text(&guild, "owner_id")?;
        let bot_id = self.bot_user_id().await?;
        let bot_member = self
            .get(&format!("/guilds/{guild_id}/members/{bot_id}"))
            .await?;
        // Target membership is the final read before the mutation fence.
        let member = self
            .get(&format!("/guilds/{guild_id}/members/{}", subject.author_id))
            .await?;
        let user = member.get("user")?;
        if text(user, "id")? != subject.author_id || text(bot_member.get("user")?, "id")? != bot_id
        {
            return None;
        }
        let is_bot = user.get("bot").and_then(Value::as_bool).unwrap_or(false);
        let target_roles = member_roles(&member)?;
        let bot_roles = member_roles(&bot_member)?;
        let mut protected = self.protected_role_ids.clone();
        let mut known = HashSet::new();
        let (mut target_top, mut bot_top, mut bot_permissions) = (0_i64, 0_i64, 0_u64);
        for role in guild.get("roles")?.as_array()? {
            let id = text(role, "id")?;
            let position = role.get("position").and_then(Value::as_i64)?;
            let permissions: u64 = text(role, "permissions")?.parse().ok()?;
            known.insert(id.to_owned());
            if permissions & DANGEROUS_PERMISSIONS != 0 {
                protected.insert(id.to_owned());
            }
            if id == guild_id || target_roles.iter().any(|held| held == id) {
                target_top = target_top.max(position);
            }
            if id == guild_id || bot_roles.iter().any(|held| held == id) {
                bot_top = bot_top.max(position);
                bot_permissions |= permissions;
            }
        }
        // An unknown role is an incomplete snapshot, never an unprotected one.
        if !known.contains(guild_id)
            || target_roles
                .iter()
                .chain(&bot_roles)
                .any(|role| !known.contains(role))
        {
            return None;
        }
        if bot_permissions & PERM_ADMINISTRATOR != 0 {
            bot_permissions = u64::MAX;
        }
        let mut role_ids = target_roles;
        role_ids.push(guild_id.to_owned());
        Some(TargetFacts {
            target: ModerationTarget {
                user_id: subject.author_id.clone(),
                role_ids,
                highest_role_position: target_top,
                is_bot,
                is_guild_owner: subject.author_id == owner_id,
            },
            policy: ModerationPolicy {
                owen_user_id: self.owen_user_id.clone(),
                protected_role_ids: protected,
                bot_user_id: Some(bot_id),
            },
            bot_highest_role_position: bot_top,
            bot_permissions,
        })
    }
}

impl AutomodFacts for RestAutomodFacts {
    fn fetch_message(
        &self,
        guild_id: &str,
        channel_id: &str,
        message_id: &str,
    ) -> impl Future<Output = Option<FetchedMessage>> + Send {
        async move {
            let value = self
                .get(&format!("/channels/{channel_id}/messages/{message_id}"))
                .await?;
            let message: Message = serde_json::from_value(value).ok()?;
            let author_id = message.author.id.to_string();
            let member = self
                .get(&format!("/guilds/{guild_id}/members/{author_id}"))
                .await?;
            if text(member.get("user")?, "id")? != author_id {
                return None;
            }
            let author_role_ids = member_roles(&member)?;
            Some(FetchedMessage {
                message,
                author_role_ids,
            })
        }
    }

    fn target_facts(
        &self,
        subject: &MessageSubject,
    ) -> impl Future<Output = Option<TargetFacts>> + Send {
        self.resolve_target(subject)
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn member_roles(member: &Value) -> Option<Vec<String>> {
    member
        .get("roles")?
        .as_array()?
        .iter()
        .map(|role| role.as_str().map(str::to_owned))
        .collect()
}
