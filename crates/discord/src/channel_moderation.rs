//! Channel handlers after shared-router adjudication. All HTTP uses ActionExecutor;
//! the durable store owns replay, channel exclusion and exact overwrite recovery.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use twilight_model::{
    application::interaction::{
        application_command::{CommandData, CommandOptionValue},
        Interaction, InteractionData, InteractionType,
    },
    channel::message::{
        component::{ActionRow, Button, ButtonStyle},
        Component, MessageFlags,
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    channel_moderation::{self as domain, ChannelOutcome, EveryoneOverwrite, UnlockPlan},
    lockdown_preview::{plan_preview, PreviewError, PreviewSnapshot, RoleSnapshot},
    router::replies::EXPIRED_COMPONENT_REPLY,
    ChannelAuditRow, ChannelClaim, ChannelClaimTicket, ChannelModerationStore, ComponentHandler,
    ComponentOutcome, HandlerId, InteractionHandler, InteractionRouter, LockdownRecord,
    ModerationAction, SlashContext, SlashOutcome,
};

use crate::{
    executor::PreviewChannel,
    lockdown_preview::{
        cancel_custom_id, confirm_custom_id, parse_preview_custom_id, preview_audit_metadata,
        render_preview, PendingEntry, PendingStore, PreviewPress, RateLimiter, PREVIEW_EXPIRED,
        PREVIEW_TTL_SECS,
    },
    route_interaction, ActionExecutor, ChannelCall, ChannelCallOutcome, DiscordError,
    RoutedInteraction,
};

#[path = "channel_moderation_reads.rs"]
mod reads;

#[derive(Debug)]
struct ChannelHandler(HandlerId);

impl InteractionHandler for ChannelHandler {
    fn id(&self) -> HandlerId {
        self.0
    }
}

/// Register this slice, not a second registry or command publisher.
pub fn register_channel_handlers(router: &mut InteractionRouter) {
    for action in ModerationAction::ALL
        .into_iter()
        .filter(|a| !a.targets_member())
    {
        router.register(Box::new(ChannelHandler(HandlerId::Moderation(action))));
    }
    for handler in [
        HandlerId::PurgePreview,
        HandlerId::ChannelInspect,
        HandlerId::LockdownPreview,
    ] {
        router.register(Box::new(ChannelHandler(handler)));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReply {
    pub text: String,
    pub outcome: String,
    pub replayed: bool,
    /// Present only on an armed `/lockdown-preview` render: the first 32 hex
    /// characters of the preview digest. `respond` attaches the Confirm /
    /// Cancel buttons from it; the confirm slice consumes the pending entry.
    /// Absent on every other reply, including unarmed previews.
    #[serde(default)]
    pub digest32: Option<String>,
}

impl ChannelReply {
    fn refused(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            outcome: "refused".to_owned(),
            replayed: false,
            digest32: None,
        }
    }
    fn uncertain() -> Self {
        Self { text: "This channel action is in progress or requires reconciliation; no new mutation was attempted.".to_owned(), outcome: "in_progress".to_owned(), replayed: false, digest32: None }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelRuntimeError {
    #[error("channel moderation persistence failed; do not repeat Discord effects")]
    Database(#[from] sqlx::Error),
    #[error("invalid stored channel moderation result")]
    StoredResult,
}

/// Bounded, token-free lifecycle errors. Details of SQL/HTTP failures must not
/// be logged: they can contain connection URLs or interaction webhook tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChannelResponseError {
    #[error("interaction defer failed; no channel mutation attempted")]
    Defer,
    #[error("channel action persistence failed; reconciliation required")]
    Persistence,
    #[error("interaction result edit failed; do not repeat the channel action")]
    Edit,
}

#[derive(Debug, Clone)]
pub struct ChannelModerationRuntime {
    store: ChannelModerationStore,
    executor: ActionExecutor,
    /// Armed `/lockdown-preview` entries: in-memory only, gone on
    /// consume/cancel/expiry/restart. Shared across clones so a spawned
    /// responder observes the same pending set. Short critical sections,
    /// never held across an await.
    preview_pending: Arc<Mutex<PendingStore>>,
    /// Per-attempt ownership nonces for armed previews, shared across clones
    /// like the pending set: concurrent same-interaction redeliveries mint
    /// distinct nonces so a losing attempt unwinds only its own entry and can
    /// never remove the winner's.
    preview_attempt: Arc<AtomicU64>,
    /// Preview budget: in-memory sliding windows, reset on restart.
    /// Shared across clones like the pending set.
    preview_limiter: Arc<Mutex<RateLimiter>>,
}

struct Request {
    row: ChannelAuditRow,
    hash: String,
    numeric: Option<u64>,
    validation: Result<(), String>,
}

/// A failed channel call, classified for the claim and lane fences.
struct CallFailure {
    error: DiscordError,
    /// The failure proves no mutation was accepted, so the lane is released.
    no_effect: bool,
}

impl CallFailure {
    fn of(error: DiscordError) -> Self {
        let no_effect = error.proves_no_effect();
        Self { error, no_effect }
    }

    fn refusal(&self) -> ChannelReply {
        ChannelReply::refused(if self.error.is_safe_pre_mutation() {
            "Discord refused the channel action; no mutation was accepted."
        } else {
            "Discord could not complete the channel action right now; no mutation was accepted. \
             Try again shortly."
        })
    }
}

struct Prepared {
    call: ChannelCall,
    /// Consumed only after confirmed restoration, or a proven rejected first lock.
    restore: Option<LockdownRecord>,
    rejected_seed: Option<LockdownRecord>,
}

/// The masks a confirmed preview must reproduce inside the lane (spec section
/// 3 step 5): `prepare`'s own fresh GET must still yield this `@everyone`
/// prior and this planned write, or it refuses before recording a seed or
/// sending a PUT. Carried from the confirm-time plan, which the digest check
/// already proved identical to the previewed snapshot.
#[derive(Debug, Clone, Copy)]
struct PreviewedWrite {
    prior_allow: u64,
    prior_deny: u64,
    write_allow: u64,
    write_deny: u64,
}

struct PreviewInputs {
    doc: PreviewChannel,
    roles: Vec<RoleSnapshot>,
    saved: Option<LockdownRecord>,
}

impl ChannelModerationRuntime {
    #[must_use]
    pub fn new(store: ChannelModerationStore, executor: ActionExecutor) -> Self {
        Self {
            store,
            executor,
            preview_pending: Arc::new(Mutex::new(PendingStore::new())),
            preview_attempt: Arc::new(AtomicU64::new(0)),
            preview_limiter: Arc::new(Mutex::new(RateLimiter::new())),
        }
    }

    /// Ownership only, never authorization. The shared router still adjudicates
    /// every request (including disabled commands and persisted-result replays).
    #[must_use]
    pub fn accepts(interaction: &Interaction) -> bool {
        if matches!(
            (interaction.kind, interaction.data.as_ref()),
            (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(data)))
                if reads::handler(&data.name).is_some()
                    || data.name == "lockdown-preview"
                    || ModerationAction::ALL.into_iter().any(|a| !a.targets_member() && a.command_name() == data.name)
        ) {
            return true;
        }
        matches!(
            (interaction.kind, interaction.data.as_ref()),
            (InteractionType::MessageComponent, Some(InteractionData::MessageComponent(data)))
                if parse_preview_custom_id(&data.custom_id).is_some()
        )
    }

    /// Whether a preview button press collapses its message. Only a press that
    /// names the pressing actor and addresses a live, unexpired pending entry
    /// with the matching digest collapses: the deferred update targets the
    /// button message and the completion edit clears its components. A
    /// foreign, forged, duplicate or post-restart press answers with a fresh
    /// ephemeral notice instead, so a live entry's buttons survive it. The
    /// peek is advisory — `execute` re-checks under the lock — so a lost race
    /// collapses with the expired text while the ledger stays exact.
    fn component_collapse(&self, interaction: &Interaction) -> bool {
        let (InteractionType::MessageComponent, Some(InteractionData::MessageComponent(data))) =
            (interaction.kind, interaction.data.as_ref())
        else {
            return false;
        };
        let Some(press) = parse_preview_custom_id(&data.custom_id) else {
            return false;
        };
        let presser = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .map(|user| user.id.get());
        if presser != Some(press.actor()) {
            return false;
        }
        let Some(guild_id) = interaction.guild_id.map(|id| id.get()) else {
            return false;
        };
        let Some(channel_id) =
            invoking_channel_id(interaction).and_then(|id| id.parse::<u64>().ok())
        else {
            return false;
        };
        let key = (guild_id, channel_id, press.actor());
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        self.preview_pending
            .lock()
            .map(|mut pending| {
                pending
                    .peek(key, now)
                    .is_some_and(|entry| entry.digest.starts_with(&press.digest32()))
            })
            .unwrap_or(false)
    }

    /// Complete the ephemeral interaction lifecycle through the shared executor.
    /// Defer before any SQL or channel HTTP, with no mutation if acknowledgement
    /// is rejected or ambiguous. A result-edit failure never retries the action.
    pub async fn respond(
        &self,
        router: &InteractionRouter,
        interaction: &Interaction,
    ) -> Result<bool, ChannelResponseError> {
        if !Self::accepts(interaction) {
            return Ok(false);
        }
        // A press that addresses a live entry collapses its message: the
        // deferred update targets the button message, and the completion edit
        // below clears its components (the entry is consumed, so no press may
        // arm again from it). Every other press answers with a fresh ephemeral
        // notice and leaves live buttons alone.
        let collapse = self.component_collapse(interaction);
        let defer = if collapse {
            InteractionResponse {
                kind: InteractionResponseType::DeferredUpdateMessage,
                data: None,
            }
        } else {
            InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: Some(InteractionResponseData {
                    flags: Some(MessageFlags::EPHEMERAL),
                    ..Default::default()
                }),
            }
        };
        // Discord's initial response deadline is three seconds. Leave a margin
        // rather than using the executor's five-second mutation abort here.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.executor.answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &defer,
            ),
        )
        .await
        .map_err(|_| ChannelResponseError::Defer)?
        .map_err(|_| ChannelResponseError::Defer)?;
        let result = self.execute(router, interaction).await;
        let text = match &result {
            Ok(Some(reply)) => reply.text.as_str(),
            _ => "The channel action requires reconciliation; do not repeat it.",
        };
        // An armed lockdown preview carries its digest: attach the bound
        // Confirm / Cancel buttons (the component path consumes the entry).
        // The digest never enters logs or metrics; only the buttons do.
        let preview_buttons = match &result {
            Ok(Some(reply)) => reply.digest32.clone(),
            _ => None,
        };
        let edit = if collapse {
            // The entry is consumed or gone: clear the buttons so no further
            // press can address it. A double-press loser overwrites the
            // winner's outcome text here; that race is cosmetic — the buttons
            // die either way and the audit ledger stays the source of truth.
            self.executor
                .edit_interaction_response_with_components_and_blocked_retry(
                    interaction.application_id.get(),
                    &interaction.token,
                    text,
                    &[],
                )
                .await
        } else if let Some(digest32) = preview_buttons {
            let actor = interaction
                .member
                .as_ref()
                .and_then(|member| member.user.as_ref())
                .map(|user| user.id.get())
                .unwrap_or_default();
            let components = vec![Component::ActionRow(ActionRow {
                id: None,
                components: vec![
                    Component::Button(Button {
                        id: None,
                        custom_id: Some(confirm_custom_id(actor, &digest32)),
                        disabled: false,
                        emoji: None,
                        label: Some("Confirm lockdown".to_owned()),
                        style: ButtonStyle::Danger,
                        url: None,
                        sku_id: None,
                    }),
                    Component::Button(Button {
                        id: None,
                        custom_id: Some(cancel_custom_id(actor, &digest32)),
                        disabled: false,
                        emoji: None,
                        label: Some("Cancel".to_owned()),
                        style: ButtonStyle::Secondary,
                        url: None,
                        sku_id: None,
                    }),
                ],
            })];
            // Blocked-retry like every other channel reply: a Blocked
            // attempt never reached the wire, so retrying it cannot repeat
            // an accepted edit, and the moderator is not left on a stuck
            // deferred reply under admission contention.
            self.executor
                .edit_interaction_response_with_components_and_blocked_retry(
                    interaction.application_id.get(),
                    &interaction.token,
                    text,
                    &components,
                )
                .await
        } else if matches!(&result, Ok(Some(reply)) if reply.outcome == "channel_inspect") {
            // Staff must see their bounded saved reason verbatim. The shared
            // private-view edit still disables all allowed_mentions explicitly.
            self.executor
                .edit_interaction_response_without_mention_neutralization_with_blocked_retry(
                    interaction.application_id.get(),
                    &interaction.token,
                    text,
                )
                .await
        } else {
            self.executor
                .edit_interaction_response_with_blocked_retry(
                    interaction.application_id.get(),
                    &interaction.token,
                    text,
                )
                .await
        };
        if result.is_err() {
            return Err(ChannelResponseError::Persistence);
        }
        edit.map_err(|_| ChannelResponseError::Edit)?;
        Ok(true)
    }

    /// Returns None for other slices. The caller defers an ephemeral response
    /// before awaiting this method, then edits that response via the shared executor.
    ///
    /// Records one `two_bot_moderation_enforcements_total` sample per terminal
    /// call on this slice (TOG-28482): `success` for completed enforcements
    /// including idempotent replays, `refused` for router/validation/fence
    /// refusals and proven no-effect Discord rejections, `storage_error` for
    /// persistence failures, and `other` for uncertain/lane outcomes
    /// (`in_progress` collapses here; the website executor classifies
    /// uncertain Discord failures as `discord_error`). The verb is the
    /// `ModerationAction::command_name`; no user id, reason text or
    /// free-form detail ever becomes a label.
    pub async fn execute(
        &self,
        router: &InteractionRouter,
        interaction: &Interaction,
    ) -> Result<Option<ChannelReply>, ChannelRuntimeError> {
        let result = self.execute_inner(router, interaction).await;
        if let (Some(action), Some(outcome)) = (
            channel_action_for(interaction),
            channel_enforcement_outcome(&result),
        ) {
            two_bot_core::metrics::global().moderation_enforcement(action.command_name(), outcome);
        }
        result
    }

    async fn execute_inner(
        &self,
        router: &InteractionRouter,
        interaction: &Interaction,
    ) -> Result<Option<ChannelReply>, ChannelRuntimeError> {
        let routed = route_interaction(router, interaction, None);
        let (name, outcome) = match routed {
            RoutedInteraction::Slash { name, outcome } => (name, outcome),
            RoutedInteraction::Component {
                custom_id,
                outcome:
                    ComponentOutcome::Handled {
                        handler: ComponentHandler::LockdownPreview,
                    },
                ..
            } => {
                return Ok(Some(
                    self.execute_preview_component(router, interaction, &custom_id)
                        .await?,
                ));
            }
            _ => return Ok(None),
        };
        // Reads never enter request/audit/claim/lease admission, even on refusal.
        if let Some(handler) = reads::handler(&name) {
            return Ok(Some(
                reads::execute(self, router, interaction, handler, outcome).await,
            ));
        }
        // The lockdown preview takes no idempotency claim, no channel lane and
        // no lease, and records no enforcement sample: it renders one diff
        // and arms one pending entry. A confirmed execution enters the
        // mutation path below with the pending reason and the confirm
        // interaction id.
        if name == "lockdown-preview" {
            return Ok(Some(
                self.execute_preview(router, interaction, outcome).await?,
            ));
        }
        let Some(action) = ModerationAction::ALL
            .into_iter()
            .find(|a| !a.targets_member() && a.command_name() == name)
        else {
            return Ok(None);
        };
        let Some(mut request) = request(interaction, action) else {
            let reply = ChannelReply::refused("A guild member and channel are required.");
            self.audit_missing_context(interaction, action, &reply)
                .await;
            return Ok(Some(reply));
        };
        // Router authorization is authoritative, including for replays. A stale
        // command cannot replay success to an actor who has since lost permission.
        let refusal = match outcome {
            SlashOutcome::Refuse { refusal } => Some(refusal.message()),
            SlashOutcome::Handled { handler }
                if handler == HandlerId::Moderation(action)
                    && router.handler_for(&handler).is_some() =>
            {
                None
            }
            _ => Some("Channel moderation is not available.".to_owned()),
        };
        if let Some(text) = refusal {
            let reply = ChannelReply::refused(text);
            self.audit_refusal(&mut request, &reply).await?;
            return Ok(Some(reply));
        }
        if let Err(text) = &request.validation {
            let reply = ChannelReply::refused(text.clone());
            self.audit_refusal(&mut request, &reply).await?;
            return Ok(Some(reply));
        }
        Ok(Some(
            self.enter_mutation_path(action, &mut request, None).await?,
        ))
    }

    /// Run one channel mutation through claim → lane → prepare → dispatch →
    /// finish. Shared by the direct slash verbs (`expected: None`) and a
    /// confirmed `/lockdown-preview` (the previewed masks: `prepare`'s own
    /// fresh GET must still reproduce them, or it refuses before recording a
    /// seed or sending a PUT).
    async fn enter_mutation_path(
        &self,
        action: ModerationAction,
        request: &mut Request,
        expected: Option<PreviewedWrite>,
    ) -> Result<ChannelReply, ChannelRuntimeError> {
        let ticket = match self
            .store
            .claim(
                &request.row.guild_id,
                &request.row.idempotency_key,
                action.action_name(),
                &request.hash,
                &request.row.created_at,
            )
            .await?
        {
            ChannelClaim::Claimed { ticket } => ticket,
            ChannelClaim::Replayed { result_json, .. } => {
                let mut reply: ChannelReply = serde_json::from_str(&result_json)
                    .map_err(|_| ChannelRuntimeError::StoredResult)?;
                reply.replayed = true;
                return Ok(reply);
            }
            ChannelClaim::InFlight => {
                let reply = ChannelReply::uncertain();
                // The first uncertain dispatch uses the plain interaction id,
                // which is also the idempotency key for slash and confirm.
                // This refusal is distinct, but still deduplicates redelivery.
                let interaction_id = request.row.idempotency_key.clone();
                request.row.request_id.push_str(":in_progress");
                request.row.metadata_json = json!({
                    "stage": "claim",
                    "request_id": interaction_id,
                })
                .to_string();
                self.audit_refusal(request, &reply).await?;
                return Ok(reply);
            }
            ChannelClaim::Mismatch => {
                let reply = ChannelReply::refused(
                    "This request id is already used or retired; use a new request id.",
                );
                self.audit_refusal(request, &reply).await?;
                return Ok(reply);
            }
        };
        let channel = request
            .row
            .channel_id
            .as_deref()
            .expect("validated channel");
        if !self.store.claim_channel(&ticket, channel).await? {
            // This ticket has dispatched nothing. Release only its own generation.
            // False (stale) is not permission to act or retry.
            let _released = self.store.release(&ticket).await?;
            let reply = ChannelReply::uncertain();
            self.audit_refusal(request, &reply).await?;
            return Ok(reply);
        }
        let prepared = match self.prepare(action, request, expected).await? {
            Ok(prepared) => prepared,
            Err(text) => {
                let reply = ChannelReply::refused(text);
                if !self.finish(&ticket, request, &reply, None).await? {
                    return Ok(ChannelReply::uncertain());
                }
                return Ok(reply);
            }
        };
        let (reply, cleanup) = match self.dispatch(&prepared.call).await {
            Ok(result) => {
                let outcome = match result {
                    ChannelCallOutcome::Purged {
                        affected,
                        stop_reason,
                    } => {
                        let mut metadata = json!({"count": request.numeric, "affected": affected});
                        if let Some(reason) = stop_reason {
                            metadata["stop_reason"] = json!(reason.name());
                        }
                        request.row.metadata_json = metadata.to_string();
                        ChannelOutcome::Purged {
                            affected,
                            stop_reason,
                        }
                    }
                    ChannelCallOutcome::SlowmodeUpdated => ChannelOutcome::SlowmodeUpdated,
                    ChannelCallOutcome::OverwriteWritten
                        if action == ModerationAction::Lockdown =>
                    {
                        ChannelOutcome::LockedDown
                    }
                    ChannelCallOutcome::OverwriteWritten | ChannelCallOutcome::OverwriteDeleted => {
                        ChannelOutcome::Unlocked
                    }
                    ChannelCallOutcome::Posted { .. } | ChannelCallOutcome::MessageDeleted => {
                        unreachable!(
                            "channel moderation never posts or uses single-message deletes"
                        )
                    }
                };
                (
                    ChannelReply {
                        text: domain::moderation_result_text(outcome),
                        outcome: outcome.name().to_owned(),
                        replayed: false,
                        digest32: None,
                    },
                    prepared.restore,
                )
            }
            Err(failure) if failure.no_effect => {
                // A refused first PUT never changed the pre-lock overwrite, so
                // its new seed is safe to retire. A prior lockdown seed survives.
                // Finishing releases the lane, so /unlock stays reachable.
                (failure.refusal(), prepared.rejected_seed)
            }
            Err(_) => {
                // Do not complete or release either claim. Audit uncertainty;
                // neither a same-key retry nor a different key may mutate this lane.
                request.row.outcome = "in_progress".to_owned();
                self.store.record_audit(&request.row).await?;
                return Ok(ChannelReply::uncertain());
            }
        };
        if !self
            .finish(&ticket, request, &reply, cleanup.as_ref())
            .await?
        {
            return Ok(ChannelReply::uncertain());
        }
        Ok(reply)
    }

    /// Unwind a failed `/lockdown-preview` replacement: drop this attempt's
    /// pending entry only while it still owns the key under its per-attempt
    /// nonce, restoring the displaced preview only when its audit committed
    /// so the earlier Confirm survives. A newer same-key preview (including
    /// a concurrent redelivery under the same request id but a different
    /// nonce) keeps its entry and row.
    fn unwind_preview_replacement(
        &self,
        key: (u64, u64, u64),
        preview_request_id: &str,
        nonce: u64,
        prior: Option<PendingEntry>,
        now: i64,
    ) {
        let _unwound = self.preview_pending.lock().map(|mut pending| {
            pending.unwind_replacement(key, preview_request_id, nonce, prior, now)
        });
    }

    /// Render one `/lockdown-preview`: validate, rate-limit, read once each,
    /// plan, audit, and arm. Anything before the audit insert refuses with no
    /// row and no pending entry; a redelivered interaction (zero new audit
    /// rows) renders nothing. Unarmed previews audit but store no entry, so
    /// they can never exhaust the pending cap.
    async fn execute_preview(
        &self,
        router: &InteractionRouter,
        interaction: &Interaction,
        outcome: SlashOutcome,
    ) -> Result<ChannelReply, ChannelRuntimeError> {
        if !matches!(
            outcome,
            SlashOutcome::Handled {
                handler: HandlerId::LockdownPreview
            }
        ) || router.handler_for(&HandlerId::LockdownPreview).is_none()
        {
            let text = match outcome {
                SlashOutcome::Refuse { refusal } => format!(
                    "{} Check the guild and your permissions, then try again.",
                    refusal.message()
                ),
                _ => "Lockdown preview is unavailable; try again shortly.".to_owned(),
            };
            return Ok(ChannelReply::refused(text));
        }
        let channel_id = invoking_channel_id(interaction);
        let member = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref());
        let (Some(guild_id), Some(channel_id), Some(actor)) =
            (interaction.guild_id, channel_id, member)
        else {
            return Ok(ChannelReply::refused(
                "A guild member and channel are required; try again there.",
            ));
        };
        let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
            return Ok(ChannelReply::refused(
                "Use the /lockdown-preview slash command and try again.",
            ));
        };
        if data.options.iter().any(|option| option.name != "reason") {
            return Ok(ChannelReply::refused(
                "Supply only \"reason\" and try again.",
            ));
        }
        let reason = match string_option(data, "reason").and_then(|value| {
            domain::require_channel_reason(value).map_err(|error| error.to_string())
        }) {
            Ok(reason) => reason,
            Err(text) => return Ok(ChannelReply::refused(format!("{text}; try again."))),
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let admitted = self
            .preview_limiter
            .lock()
            .map(|mut limiter| limiter.admit(actor.id.get(), guild_id.get(), now))
            .unwrap_or(false);
        if !admitted {
            return Ok(ChannelReply::refused(
                "Lockdown preview is rate limited; try again shortly. Nothing was changed.",
            ));
        }
        let guild = guild_id.to_string();
        let inputs = match self.read_preview_inputs(&channel_id, &guild).await? {
            Ok(inputs) => inputs,
            Err(reply) => return Ok(reply),
        };
        let PreviewInputs {
            doc: channel_doc,
            roles,
            saved,
        } = inputs;
        let Some(channel_num) = channel_id.parse::<u64>().ok() else {
            return Ok(ChannelReply::refused(
                "A guild member and channel are required; try again there.",
            ));
        };
        let snapshot = PreviewSnapshot {
            guild_id: guild_id.get(),
            channel_id: channel_num,
            channel_type: channel_doc.channel_type,
            parent_id: channel_doc.parent_id,
            actor_id: actor.id.get(),
            reason,
            saved_present: saved.is_some(),
            saved_generation: saved.map_or_else(String::new, |record| record.recovery_generation),
            roles,
            overwrites: channel_doc.overwrites,
        };
        let plan = match plan_preview(&snapshot) {
            Ok(plan) => plan,
            Err(PreviewError::UnsupportedChannel) => {
                return Ok(ChannelReply::refused(
                    "Lockdown preview supports text and announcement channels only. Nothing was changed.",
                ));
            }
            Err(PreviewError::TooComplex) => {
                return Ok(ChannelReply::refused(
                    "This channel is too complex to preview safely; open the channel permissions screen and use /channel-inspect instead. Nothing was changed.",
                ));
            }
            Err(_) => {
                return Ok(ChannelReply::refused(
                    "Could not verify the live channel permissions; try again shortly. Nothing was changed.",
                ));
            }
        };
        let expires_at = now.saturating_add(PREVIEW_TTL_SECS);
        let text = render_preview(
            &plan,
            channel_num,
            channel_doc.channel_type,
            channel_doc.parent_id,
            snapshot.saved_present,
            expires_at,
        );
        // Arm the pending entry before the audit row would double-count a
        // redelivery: a new key at cap refuses PREVIEW_BUSY with no audit
        // row, while a same-key replacement succeeds atomically. The
        // displaced prior entry is held aside so a failed replacement
        // (audit error or redelivery) restores it instead of leaving the
        // earlier Confirm dead; only a newer same-key preview that
        // superseded this attempt keeps the slot. The nonce makes ownership
        // attempt-specific: concurrent redeliveries share the request id but
        // mint distinct nonces, so a loser never removes the winner's entry.
        // Restoration additionally requires the prior's audit to have
        // committed, otherwise overlapping failures would revive an unaudited
        // entry with no row behind it.
        let digest32 = plan.digest32().to_owned();
        let pending_key = (guild_id.get(), channel_num, actor.id.get());
        let interaction_id = interaction.id.to_string();
        let nonce = self.preview_attempt.fetch_add(1, Ordering::Relaxed);
        let mut displaced: Option<PendingEntry> = None;
        if plan.armed {
            let replacement = self
                .preview_pending
                .lock()
                .map(|mut pending| {
                    let prior = pending.peek(pending_key, now);
                    pending
                        .insert(
                            pending_key,
                            PendingEntry {
                                digest: plan.digest.clone(),
                                reason: snapshot.reason.clone(),
                                preview_request_id: interaction_id.clone(),
                                expires_at,
                                nonce,
                            },
                            now,
                        )
                        .map(|()| prior)
                })
                .unwrap_or(Err(crate::lockdown_preview::PendingFull));
            match replacement {
                Ok(prior) => displaced = prior,
                Err(_) => {
                    return Ok(ChannelReply::refused(
                        "Too many live lockdown previews; wait for one to expire and try again. Nothing was changed.",
                    ));
                }
            }
        }
        let row = ChannelAuditRow {
            request_id: interaction_id.clone(),
            guild_id: guild,
            actor_id: actor.id.to_string(),
            action: ModerationAction::Lockdown.action_name().to_owned(),
            channel_id: Some(channel_id),
            reason: String::new(),
            outcome: "lockdown_preview".to_owned(),
            idempotency_key: interaction_id.clone(),
            metadata_json: preview_audit_metadata(&plan),
            created_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
        };
        // Unwind only this attempt's own entry, matched by request id and
        // per-attempt nonce, and restore the displaced preview only when its
        // audit committed so the earlier Confirm survives the failed
        // replacement. A newer same-key preview that superseded this attempt
        // keeps its entry and row: the ownership check fails and nothing is
        // restored.
        let recorded = match self.store.try_record_audit(&row).await {
            Ok(recorded) => recorded,
            Err(error) => {
                if plan.armed {
                    self.unwind_preview_replacement(
                        pending_key,
                        &interaction_id,
                        nonce,
                        displaced,
                        now,
                    );
                }
                return Err(error.into());
            }
        };
        if !recorded {
            if plan.armed {
                self.unwind_preview_replacement(
                    pending_key,
                    &interaction_id,
                    nonce,
                    displaced,
                    now,
                );
            }
            return Ok(ChannelReply::refused(
                "Could not record the preview audit row; try again. Nothing was rendered and nothing was changed.",
            ));
        }
        // The audit row exists, so a later failed replacement may restore
        // this attempt's entry. Unarmed previews store no entry and mark
        // nothing: there is nothing a replacement could displace.
        if plan.armed {
            let _marked = self
                .preview_pending
                .lock()
                .map(|mut pending| pending.mark_committed(&interaction_id, expires_at, now));
        }
        Ok(ChannelReply {
            text,
            outcome: "lockdown_preview".to_owned(),
            replayed: false,
            digest32: plan.armed.then_some(digest32),
        })
    }

    /// The three live inputs behind one preview, read once each. Shared by
    /// the slash and the confirm recheck so their refusals stay
    /// text-identical: any failed read refuses the whole preview with no
    /// audit row and no pending entry. A store outage stays a caller error.
    async fn read_preview_inputs(
        &self,
        channel_id: &str,
        guild: &str,
    ) -> Result<Result<PreviewInputs, ChannelReply>, ChannelRuntimeError> {
        let doc = match self.executor.read_preview_channel(channel_id, guild).await {
            Ok(doc) => doc,
            Err(_) => {
                return Ok(Err(ChannelReply::refused(
                    "Could not read the live channel permissions; try again shortly. Nothing was changed.",
                )));
            }
        };
        let roles = match self.executor.read_preview_roles(guild).await {
            Ok(roles) => roles,
            Err(_) => {
                return Ok(Err(ChannelReply::refused(
                    "Could not read the live role list; try again shortly. Nothing was changed.",
                )));
            }
        };
        let saved = match self.store.get_lockdown(channel_id).await {
            Ok(record) => record,
            Err(error) => return Err(error.into()),
        };
        if saved
            .as_ref()
            .is_some_and(|record| record.guild_id != guild)
        {
            return Ok(Err(ChannelReply::refused(
                "Saved restriction guild does not match; try again in the correct channel.",
            )));
        }
        Ok(Ok(PreviewInputs { doc, roles, saved }))
    }

    /// Answer one `/lockdown-preview` Confirm/Cancel press (spec section 3
    /// steps 4-5). Cancel consumes the entry and collapses the buttons with
    /// no audit row. Confirm consumes the entry, re-runs router adjudication
    /// and the three reads, refuses as stale on any digest mismatch, and
    /// otherwise enters the existing lockdown mutation path with the pending
    /// reason and the confirm interaction id as its idempotency identity.
    ///
    /// Anything that is not an owned live entry — a foreign or forged press,
    /// a double press, cancel-then-confirm, restart loss — gets the collapsed
    /// expired reply and no audit row.
    async fn execute_preview_component(
        &self,
        router: &InteractionRouter,
        interaction: &Interaction,
        custom_id: &str,
    ) -> Result<ChannelReply, ChannelRuntimeError> {
        let expired = || ChannelReply {
            text: EXPIRED_COMPONENT_REPLY.to_owned(),
            outcome: PREVIEW_EXPIRED.to_owned(),
            replayed: false,
            digest32: None,
        };
        let Some(press) = parse_preview_custom_id(custom_id) else {
            return Ok(expired());
        };
        // Ownership from the authenticated interaction, never from the custom
        // id alone: a press by anyone but the button's owner expires without
        // touching the entry.
        let presser = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .map(|user| user.id.get());
        if presser != Some(press.actor()) {
            return Ok(expired());
        }
        match press {
            PreviewPress::Confirm { .. } => {
                self.execute_preview_confirm(router, interaction, press)
                    .await
            }
            PreviewPress::Cancel { .. } => Ok(self.execute_preview_cancel(interaction, press)),
        }
    }

    /// Consume one pending entry for a Cancel press. Removes the entry when
    /// the press addresses it and collapses the buttons; writes no audit row
    /// and performs no mutation. A press against a missing or superseded
    /// entry expires the same way.
    fn execute_preview_cancel(
        &self,
        interaction: &Interaction,
        press: PreviewPress,
    ) -> ChannelReply {
        let expired = || ChannelReply {
            text: EXPIRED_COMPONENT_REPLY.to_owned(),
            outcome: PREVIEW_EXPIRED.to_owned(),
            replayed: false,
            digest32: None,
        };
        let Some(guild_id) = interaction.guild_id.map(|id| id.get()) else {
            return expired();
        };
        let Some(channel_id) =
            invoking_channel_id(interaction).and_then(|id| id.parse::<u64>().ok())
        else {
            return expired();
        };
        let key = (guild_id, channel_id, press.actor());
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let removed = self
            .preview_pending
            .lock()
            .map(|mut pending| pending.take(key, &press.digest32(), now).is_some())
            .unwrap_or(false);
        if !removed {
            return expired();
        }
        ChannelReply {
            text: "Lockdown preview cancelled. Nothing was changed.".to_owned(),
            outcome: "cancelled".to_owned(),
            replayed: false,
            digest32: None,
        }
    }

    /// Consume one pending entry for a Confirm press and execute it through
    /// the existing mutation path. Router adjudication re-runs before any
    /// read (a lost gate refuses with the entry already consumed); the three
    /// inputs are re-read once each and the digest recomputed, and any
    /// mismatch refuses as stale with exactly one audit row and no claim,
    /// lane, seed or PUT. On an exact match the lane's `prepare` must still
    /// reproduce the previewed prior and write, or it refuses before any seed
    /// or PUT.
    async fn execute_preview_confirm(
        &self,
        router: &InteractionRouter,
        interaction: &Interaction,
        press: PreviewPress,
    ) -> Result<ChannelReply, ChannelRuntimeError> {
        let expired = || ChannelReply {
            text: EXPIRED_COMPONENT_REPLY.to_owned(),
            outcome: PREVIEW_EXPIRED.to_owned(),
            replayed: false,
            digest32: None,
        };
        let Some(guild_id) = interaction.guild_id.map(|id| id.get()) else {
            return Ok(expired());
        };
        let Some(channel_id) =
            invoking_channel_id(interaction).and_then(|id| id.parse::<u64>().ok())
        else {
            return Ok(expired());
        };
        let presser = interaction
            .member
            .as_ref()
            .and_then(|member| member.user.as_ref())
            .map(|user| user.id.get())
            .unwrap_or_default();
        let key = (guild_id, channel_id, press.actor());
        let at = time::OffsetDateTime::now_utc();
        let now = at.unix_timestamp();
        // Consume-once: a second press, a forged digest or a post-restart
        // press finds no entry and expires with no audit row.
        let pending = self
            .preview_pending
            .lock()
            .map(|mut pending| pending.take(key, &press.digest32(), now))
            .unwrap_or(None);
        let Some(pending) = pending else {
            return Ok(expired());
        };
        // Re-run the preview's own adjudication on the pressing member before
        // any read. The gate or the `ManageChannels` check can only have
        // closed since the preview; the entry stays consumed either way.
        let actor_permissions = interaction
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .map(|permissions| permissions.bits());
        let adjudicated = SlashContext {
            name: "lockdown-preview",
            guild_id: Some(guild_id),
            actor_permissions,
            custom_row: None,
        };
        match router.route_slash(&adjudicated) {
            SlashOutcome::Handled { handler }
                if handler == HandlerId::LockdownPreview
                    && router.handler_for(&HandlerId::LockdownPreview).is_some() => {}
            SlashOutcome::Refuse { refusal } => {
                return Ok(ChannelReply::refused(format!(
                    "{} Nothing was changed; run /lockdown-preview again.",
                    refusal.message()
                )));
            }
            _ => {
                return Ok(ChannelReply::refused(
                    "Lockdown preview is unavailable; try again shortly. Nothing was changed.",
                ));
            }
        }
        let guild = guild_id.to_string();
        let channel = channel_id.to_string();
        let inputs = match self.read_preview_inputs(&channel, &guild).await? {
            Ok(inputs) => inputs,
            Err(reply) => return Ok(reply),
        };
        let snapshot = PreviewSnapshot {
            guild_id,
            channel_id,
            channel_type: inputs.doc.channel_type,
            parent_id: inputs.doc.parent_id,
            actor_id: presser,
            reason: pending.reason.clone(),
            saved_present: inputs.saved.is_some(),
            saved_generation: inputs
                .saved
                .map_or_else(String::new, |record| record.recovery_generation),
            roles: inputs.roles,
            overwrites: inputs.doc.overwrites,
        };
        let plan = match plan_preview(&snapshot) {
            Ok(plan) => plan,
            Err(PreviewError::UnsupportedChannel) => {
                return Ok(ChannelReply::refused(
                    "Lockdown preview supports text and announcement channels only. Nothing was changed.",
                ));
            }
            Err(PreviewError::TooComplex) => {
                return Ok(ChannelReply::refused(
                    "This channel is too complex to preview safely; open the channel permissions screen and use /channel-inspect instead. Nothing was changed.",
                ));
            }
            Err(_) => {
                return Ok(ChannelReply::refused(
                    "Could not verify the live channel permissions; try again shortly. Nothing was changed.",
                ));
            }
        };
        if plan.digest != pending.digest {
            // Stale: one refused row pinning the divergence, no mutation at
            // risk. If the row itself fails to land, the refusal still stands;
            // log the fixed token like the missing-context path does.
            let mut row =
                confirm_request_row(interaction, &guild, &channel, presser, &pending, &at);
            row.row.outcome = "refused".to_owned();
            row.row.metadata_json = serde_json::json!({"stale_preview": 1}).to_string();
            if self.store.record_audit(&row.row).await.is_err() {
                tracing::error!(
                    action = ModerationAction::Lockdown.action_name(),
                    "channel_refusal_audit_failed"
                );
            }
            return Ok(ChannelReply::refused(
                "The channel permissions changed after this preview; run /lockdown-preview again for a fresh diff. Nothing was changed.",
            ));
        }
        if domain::require_channel_reason(&pending.reason).is_err() {
            return Ok(ChannelReply::refused(
                "The previewed reason is no longer valid; run /lockdown-preview again. Nothing was changed.",
            ));
        }
        let expected = PreviewedWrite {
            prior_allow: plan.prior_allow,
            prior_deny: plan.prior_deny,
            write_allow: plan.planned_allow,
            write_deny: plan.planned_deny,
        };
        let mut request =
            confirm_request_row(interaction, &guild, &channel, presser, &pending, &at);
        self.enter_mutation_path(ModerationAction::Lockdown, &mut request, Some(expected))
            .await
    }

    /// Run one prepared call. Purge is split into its two phases: a failed
    /// history read, even a timeout, proves no delete was sent, so it must not
    /// keep the lane. Every other failure follows [`DiscordError::proves_no_effect`].
    async fn dispatch(&self, call: &ChannelCall) -> Result<ChannelCallOutcome, CallFailure> {
        let ChannelCall::Purge {
            channel_id,
            count,
            reason,
        } = call
        else {
            return self
                .executor
                .execute_channel(call)
                .await
                .map_err(CallFailure::of);
        };
        let ids = self
            .executor
            .list_purge_messages(channel_id, *count)
            .await
            .map_err(|error| CallFailure {
                error,
                no_effect: true,
            })?;
        self.executor
            .purge_messages(channel_id, &ids, reason)
            .await
            .map(|report| ChannelCallOutcome::Purged {
                affected: report.affected,
                stop_reason: report.stop_reason,
            })
            .map_err(CallFailure::of)
    }

    async fn prepare(
        &self,
        action: ModerationAction,
        request: &Request,
        expected: Option<PreviewedWrite>,
    ) -> Result<Result<Prepared, String>, ChannelRuntimeError> {
        let channel_id = request.row.channel_id.clone().expect("validated channel");
        let guild_id = request.row.guild_id.clone();
        let reason = request.row.reason.clone();
        let prepared = match action {
            ModerationAction::Purge => Prepared {
                call: ChannelCall::Purge {
                    channel_id,
                    count: request.numeric.expect("validated count"),
                    reason,
                },
                restore: None,
                rejected_seed: None,
            },
            ModerationAction::Slowmode => Prepared {
                call: ChannelCall::Slowmode {
                    channel_id,
                    seconds: request.numeric.expect("validated seconds"),
                    reason,
                },
                restore: None,
                rejected_seed: None,
            },
            ModerationAction::Lockdown => {
                let prior_record = self.store.get_lockdown(&channel_id).await?;
                if prior_record
                    .as_ref()
                    .is_some_and(|r| r.guild_id != guild_id)
                {
                    return Ok(Err("Recovery guild does not match this channel.".to_owned()));
                }
                // GET is read-only: a read failure has not dispatched a mutation.
                let current = match self
                    .executor
                    .get_everyone_overwrite(&channel_id, &guild_id)
                    .await
                {
                    Ok(current) => current.map(|ow| EveryoneOverwrite {
                        allow: ow.allow,
                        deny: ow.deny,
                    }),
                    Err(_) => return Ok(Err(
                        "Could not verify the current channel overwrite; no mutation attempted."
                            .to_owned(),
                    )),
                };
                let plan = match domain::plan_lockdown(current.as_ref()) {
                    Ok(plan) => plan,
                    Err(_) => return Ok(Err("Invalid channel permission masks.".to_owned())),
                };
                // A confirmed preview must reproduce the previewed masks here:
                // an external edit between the confirm recheck and this lane
                // GET refuses before any seed or PUT (spec section 3 step 5).
                // The masks parsed for the plan above, so only a mismatch can
                // fail these numeric reads; fail closed either way.
                if let Some(expected) = expected {
                    let prior: Option<(u64, u64)> = match current.as_ref() {
                        Some(ow) => ow
                            .allow
                            .parse::<u64>()
                            .ok()
                            .zip(ow.deny.parse::<u64>().ok()),
                        None => Some((0, 0)),
                    };
                    let write: Option<(u64, u64)> = plan
                        .write
                        .allow
                        .parse::<u64>()
                        .ok()
                        .zip(plan.write.deny.parse::<u64>().ok());
                    let reproduced = match (prior, write) {
                        (Some((prior_allow, prior_deny)), Some((write_allow, write_deny))) => {
                            prior_allow == expected.prior_allow
                                && prior_deny == expected.prior_deny
                                && write_allow == expected.write_allow
                                && write_deny == expected.write_deny
                        }
                        _ => false,
                    };
                    if !reproduced {
                        return Ok(Err("The channel permissions changed after this preview; run /lockdown-preview again for a fresh diff. Nothing was changed.".to_owned()));
                    }
                }
                // Persist before PUT. Keep the first seed while locked; a live
                // overwrite without the send deny starts a new recovery cycle.
                let rec = self
                    .store
                    .record_lockdown(
                        &channel_id,
                        &guild_id,
                        &plan.seed,
                        &reason,
                        &request.row.created_at,
                    )
                    .await?;
                Prepared {
                    call: ChannelCall::PutOverwrite {
                        channel_id,
                        guild_id,
                        allow: plan.write.allow,
                        deny: plan.write.deny,
                        reason,
                    },
                    restore: None,
                    rejected_seed: if prior_record
                        .as_ref()
                        .is_none_or(|old| old.recovery_generation != rec.recovery_generation)
                    {
                        Some(rec)
                    } else {
                        None
                    },
                }
            }
            ModerationAction::Unlock => {
                let rec = self.store.get_lockdown(&channel_id).await?;
                if rec.as_ref().is_some_and(|r| r.guild_id != guild_id) {
                    return Ok(Err("Recovery guild does not match this channel.".to_owned()));
                }
                if rec.is_none() {
                    return Ok(Err(domain::UnlockError::NotLocked.to_string()));
                }
                let current = match self
                    .executor
                    .get_everyone_overwrite(&channel_id, &guild_id)
                    .await
                {
                    Ok(current) => current.map(|ow| EveryoneOverwrite {
                        allow: ow.allow,
                        deny: ow.deny,
                    }),
                    Err(_) => return Ok(Err(
                        "Could not verify the current channel overwrite; no mutation attempted."
                            .to_owned(),
                    )),
                };
                let call = match domain::plan_unlock(rec.as_ref(), current.as_ref()) {
                    Ok(UnlockPlan::Restore { allow, deny }) => ChannelCall::PutOverwrite {
                        channel_id,
                        guild_id,
                        allow,
                        deny,
                        reason,
                    },
                    Ok(UnlockPlan::DeleteOverwrite) => ChannelCall::DeleteOverwrite {
                        channel_id,
                        guild_id,
                        reason,
                    },
                    Err(error) => return Ok(Err(error.to_string())),
                };
                Prepared {
                    call,
                    restore: rec,
                    rejected_seed: None,
                }
            }
            _ => unreachable!("only registered channel verbs"),
        };
        Ok(Ok(prepared))
    }

    async fn audit_missing_context(
        &self,
        interaction: &Interaction,
        action: ModerationAction,
        reply: &ChannelReply,
    ) {
        // Preserve every known binding without inventing a member or channel.
        // The shared schema uses non-null text for guild/actor; empty means
        // unavailable here, never an authorization principal.
        let row = ChannelAuditRow {
            request_id: interaction.id.to_string(),
            guild_id: interaction
                .guild_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            actor_id: interaction
                .author_id()
                .map(|id| id.to_string())
                .unwrap_or_default(),
            action: action.action_name().to_owned(),
            channel_id: invoking_channel_id(interaction),
            reason: reply.text.clone(),
            outcome: reply.outcome.clone(),
            idempotency_key: interaction.id.to_string(),
            metadata_json: json!({"stage": "validation", "cause": "missing_context"}).to_string(),
            created_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .expect("UTC formats"),
        };
        if self.store.record_audit(&row).await.is_err() {
            // Incomplete context still gets its fixed refusal if storage fails.
            tracing::error!(
                action = action.action_name(),
                "channel_refusal_audit_failed"
            );
        }
    }

    async fn audit_refusal(
        &self,
        request: &mut Request,
        reply: &ChannelReply,
    ) -> Result<(), sqlx::Error> {
        request.row.outcome = reply.outcome.clone();
        self.store.record_audit(&request.row).await
    }

    async fn finish(
        &self,
        ticket: &ChannelClaimTicket,
        request: &mut Request,
        reply: &ChannelReply,
        restored: Option<&LockdownRecord>,
    ) -> Result<bool, ChannelRuntimeError> {
        request.row.outcome = reply.outcome.clone();
        let result = serde_json::to_string(reply).map_err(|_| ChannelRuntimeError::StoredResult)?;
        // Repeat only the atomic persistence step, never a successful Discord effect.
        for attempt in 0..3 {
            match self
                .store
                .finish(ticket, &request.row, &result, restored)
                .await
            {
                Ok(changed) => return Ok(changed),
                Err(error) if attempt == 2 => return Err(error.into()),
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
        unreachable!()
    }
}

/// The channel moderation verb for this interaction, if it names one.
/// Mirrors `execute_inner`'s routing predicate over the same source
/// (`route_interaction` clones `data.name` into the routed slash name, so
/// matching `data.name` here cannot drift from the inner lookup): slash
/// interactions whose command name is a non-member
/// `ModerationAction::command_name`, plus a `/lockdown-preview` Confirm press,
/// which becomes the `/lockdown` it executes. Cancel becomes nothing and
/// records no sample.
fn channel_action_for(interaction: &Interaction) -> Option<ModerationAction> {
    match (interaction.kind, interaction.data.as_ref()) {
        (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(data))) => {
            ModerationAction::ALL
                .into_iter()
                .find(|a| !a.targets_member() && a.command_name() == data.name)
        }
        (InteractionType::MessageComponent, Some(InteractionData::MessageComponent(data))) => {
            match parse_preview_custom_id(&data.custom_id) {
                Some(PreviewPress::Confirm { .. }) => Some(ModerationAction::Lockdown),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Bounded metrics outcome for a terminal `execute_inner` result (TOG-28482):
/// `None` when this slice did not handle the interaction (no enforcement, no
/// sample), otherwise the `MODERATION_ENFORCEMENT_OUTCOMES` class. Success
/// outcome names (including idempotent replays) count as `success`;
/// `refused` covers every terminal refusal; `in_progress` (lane held,
/// uncertain Discord, stale fence) collapses to `other`; persistence
/// failures count as `storage_error`.
fn channel_enforcement_outcome(
    result: &Result<Option<ChannelReply>, ChannelRuntimeError>,
) -> Option<&'static str> {
    match result {
        Ok(None) => None,
        Ok(Some(reply)) => Some(match reply.outcome.as_str() {
            "refused" | PREVIEW_EXPIRED => "refused",
            "in_progress" => "other",
            _ => "success",
        }),
        Err(_) => Some("storage_error"),
    }
}

#[allow(deprecated)]
pub(crate) fn invoking_channel_id(interaction: &Interaction) -> Option<String> {
    interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.to_string())
        .or_else(|| interaction.channel_id.map(|id| id.to_string()))
}

fn request(interaction: &Interaction, action: ModerationAction) -> Option<Request> {
    let guild_id = interaction.guild_id?.to_string();
    let channel_id = invoking_channel_id(interaction)?;
    let actor_id = interaction.member.as_ref()?.user.as_ref()?.id.to_string();
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        return None;
    };
    let reason = string_option(data, "reason");
    let validated_reason =
        reason.and_then(|r| domain::require_channel_reason(r).map_err(|e| e.to_string()));
    let numeric = match action {
        ModerationAction::Purge => integer_option(data, "count")
            .and_then(|n| domain::validate_purge_count(n).map_err(|e| e.to_string())),
        ModerationAction::Slowmode => integer_option(data, "seconds")
            .and_then(|n| domain::validate_slowmode_seconds(n).map_err(|e| e.to_string())),
        _ => Ok(0),
    };
    let validation = validated_reason
        .as_ref()
        .map(|_| ())
        .map_err(Clone::clone)
        .and_then(|()| numeric.as_ref().map(|_| ()).map_err(Clone::clone));
    let reason = validated_reason.unwrap_or_default();
    // Hash action, actor, channel and the original typed options, not permissions
    // or tokens. The action is also independently compared by the durable claim.
    let canonical = json!([
        action.action_name(),
        guild_id,
        actor_id,
        channel_id,
        data.options
    ])
    .to_string();
    let hash = hex::encode(Sha256::digest(canonical.as_bytes()));
    let numeric = numeric
        .ok()
        .filter(|_| matches!(action, ModerationAction::Purge | ModerationAction::Slowmode));
    let metadata_json = match action {
        ModerationAction::Purge => json!({"count": numeric.filter(|n| (1..=100).contains(n))}),
        ModerationAction::Slowmode => json!({"seconds": numeric.filter(|n| *n <= 21600)}),
        _ => json!({}),
    }
    .to_string();
    Some(Request {
        row: ChannelAuditRow {
            request_id: interaction.id.to_string(),
            guild_id,
            actor_id,
            action: action.action_name().to_owned(),
            channel_id: Some(channel_id),
            reason,
            outcome: "refused".to_owned(),
            idempotency_key: interaction.id.to_string(),
            metadata_json,
            created_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .expect("UTC formats"),
        },
        hash,
        numeric,
        validation,
    })
}

/// One audit row for a confirmed preview's mutation path (and its stale
/// refusal): the existing lockdown row with the preview correlation added.
/// `preview_request_id` is the confirmed preview's interaction id — the same
/// value that preview row stores as `request_id` and `idempotency_key` — so a
/// later preview rendering before the execution row lands can never steal the
/// correlation. No digest, mask or role id is added. Direct `/lockdown` rows
/// keep their existing empty metadata.
fn confirm_request_row(
    interaction: &Interaction,
    guild: &str,
    channel: &str,
    presser: u64,
    pending: &PendingEntry,
    at: &time::OffsetDateTime,
) -> Request {
    let confirm_id = interaction.id.to_string();
    // Hash the claim identity, not permissions or tokens: the action, the
    // parties and the exact preview this confirm spends.
    let canonical = json!([
        ModerationAction::Lockdown.action_name(),
        guild,
        presser.to_string(),
        channel,
        pending.digest,
    ])
    .to_string();
    Request {
        row: ChannelAuditRow {
            request_id: confirm_id.clone(),
            guild_id: guild.to_owned(),
            actor_id: presser.to_string(),
            action: ModerationAction::Lockdown.action_name().to_owned(),
            channel_id: Some(channel.to_owned()),
            reason: pending.reason.clone(),
            outcome: "refused".to_owned(),
            idempotency_key: confirm_id,
            metadata_json: json!({"preview_request_id": pending.preview_request_id}).to_string(),
            created_at: at
                .format(&time::format_description::well_known::Rfc3339)
                .expect("UTC formats"),
        },
        hash: hex::encode(Sha256::digest(canonical.as_bytes())),
        numeric: None,
        validation: Ok(()),
    }
}

fn string_option<'a>(data: &'a CommandData, name: &str) -> Result<&'a str, String> {
    let mut options = data.options.iter().filter(|o| o.name == name);
    match (options.next(), options.next()) {
        (Some(option), None) => match &option.value {
            CommandOptionValue::String(value) => Ok(value),
            _ => Err(format!("\"{name}\" must be a string")),
        },
        _ => Err(format!("\"{name}\" is required exactly once")),
    }
}

fn integer_option(data: &CommandData, name: &str) -> Result<Option<u64>, String> {
    let mut options = data.options.iter().filter(|o| o.name == name);
    match (options.next(), options.next()) {
        (Some(option), None) => match option.value {
            CommandOptionValue::Integer(value) => u64::try_from(value)
                .map(Some)
                .map_err(|_| format!("\"{name}\" cannot be negative")),
            _ => Err(format!("\"{name}\" must be an integer")),
        },
        _ => Err(format!("\"{name}\" is required exactly once")),
    }
}

#[cfg(test)]
#[path = "channel_moderation_tests.rs"]
mod tests;
