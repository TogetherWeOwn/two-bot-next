//! Channel handlers after shared-router adjudication. All HTTP uses ActionExecutor;
//! the durable store owns replay, channel exclusion and exact overwrite recovery.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use twilight_model::{
    application::interaction::{
        application_command::{CommandData, CommandOptionValue},
        Interaction, InteractionData, InteractionType,
    },
    channel::message::MessageFlags,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    channel_moderation::{self as domain, ChannelOutcome, EveryoneOverwrite, UnlockPlan},
    ChannelAuditRow, ChannelClaim, ChannelClaimTicket, ChannelModerationStore, HandlerId,
    InteractionHandler, InteractionRouter, LockdownRecord, ModerationAction, SlashOutcome,
};

use crate::{
    route_interaction, ActionExecutor, ChannelCall, ChannelCallOutcome, DiscordError,
    RoutedInteraction,
};

#[derive(Debug)]
struct ChannelHandler(ModerationAction);

impl InteractionHandler for ChannelHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Moderation(self.0)
    }
}

/// Register this slice, not a second registry or command publisher.
pub fn register_channel_handlers(router: &mut InteractionRouter) {
    for action in ModerationAction::ALL
        .into_iter()
        .filter(|a| !a.targets_member())
    {
        router.register(Box::new(ChannelHandler(action)));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelReply {
    pub text: String,
    pub outcome: String,
    pub replayed: bool,
}

impl ChannelReply {
    fn refused(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            outcome: "refused".to_owned(),
            replayed: false,
        }
    }
    fn uncertain() -> Self {
        Self { text: "This channel action is in progress or requires reconciliation; no new mutation was attempted.".to_owned(), outcome: "in_progress".to_owned(), replayed: false }
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
}

struct Request {
    row: ChannelAuditRow,
    hash: String,
    numeric: Option<u64>,
    validation: Result<(), String>,
}

struct Prepared {
    call: ChannelCall,
    /// Consumed only after confirmed restoration, or a proven rejected first lock.
    restore: Option<LockdownRecord>,
    rejected_seed: Option<LockdownRecord>,
}

impl ChannelModerationRuntime {
    #[must_use]
    pub fn new(store: ChannelModerationStore, executor: ActionExecutor) -> Self {
        Self { store, executor }
    }

    /// Ownership only, never authorization. The shared router still adjudicates
    /// every request (including disabled commands and persisted-result replays).
    #[must_use]
    pub fn accepts(interaction: &Interaction) -> bool {
        matches!(
            (interaction.kind, interaction.data.as_ref()),
            (InteractionType::ApplicationCommand, Some(InteractionData::ApplicationCommand(data)))
                if ModerationAction::ALL.into_iter().any(|a| !a.targets_member() && a.command_name() == data.name)
        )
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
        let defer = InteractionResponse {
            kind: InteractionResponseType::DeferredChannelMessageWithSource,
            data: Some(InteractionResponseData {
                flags: Some(MessageFlags::EPHEMERAL),
                ..Default::default()
            }),
        };
        // Discord's initial response deadline is three seconds. Leave a margin
        // rather than using the executor's five-second mutation abort here.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.executor
                .answer_interaction(interaction.id.get(), &interaction.token, &defer),
        )
        .await
        .map_err(|_| ChannelResponseError::Defer)?
        .map_err(|_| ChannelResponseError::Defer)?;
        let result = self.execute(router, interaction).await;
        let text = match &result {
            Ok(Some(reply)) => reply.text.as_str(),
            _ => "The channel action requires reconciliation; do not repeat it.",
        };
        let edit = self
            .executor
            .edit_interaction_response(interaction.application_id.get(), &interaction.token, text)
            .await;
        if result.is_err() {
            return Err(ChannelResponseError::Persistence);
        }
        edit.map_err(|_| ChannelResponseError::Edit)?;
        Ok(true)
    }

    /// Returns None for other slices. The caller defers an ephemeral response
    /// before awaiting this method, then edits that response via the shared executor.
    pub async fn execute(
        &self,
        router: &InteractionRouter,
        interaction: &Interaction,
    ) -> Result<Option<ChannelReply>, ChannelRuntimeError> {
        let RoutedInteraction::Slash { name, outcome } =
            route_interaction(router, interaction, None)
        else {
            return Ok(None);
        };
        let Some(action) = ModerationAction::ALL
            .into_iter()
            .find(|a| !a.targets_member() && a.command_name() == name)
        else {
            return Ok(None);
        };
        let Some(mut request) = request(interaction, action) else {
            return Ok(Some(ChannelReply::refused(
                "A guild member and channel are required.",
            )));
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
                return Ok(Some(reply));
            }
            ChannelClaim::InFlight => return Ok(Some(ChannelReply::uncertain())),
            ChannelClaim::Mismatch => {
                let reply =
                    ChannelReply::refused("This request id was used for different action content.");
                self.audit_refusal(&mut request, &reply).await?;
                return Ok(Some(reply));
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
            self.audit_refusal(&mut request, &reply).await?;
            return Ok(Some(reply));
        }
        let prepared = match self.prepare(action, &request).await? {
            Ok(prepared) => prepared,
            Err(text) => {
                let reply = ChannelReply::refused(text);
                if !self.finish(&ticket, &mut request, &reply, None).await? {
                    return Ok(Some(ChannelReply::uncertain()));
                }
                return Ok(Some(reply));
            }
        };
        let (reply, cleanup) = match self.executor.execute_channel(&prepared.call).await {
            Ok(result) => {
                let outcome = match result {
                    ChannelCallOutcome::Purged { affected } => {
                        request.row.metadata_json =
                            json!({"count": request.numeric, "affected": affected}).to_string();
                        ChannelOutcome::Purged { affected }
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
                    ChannelCallOutcome::Posted { .. } => {
                        unreachable!("channel moderation never posts")
                    }
                };
                (
                    ChannelReply {
                        text: domain::moderation_result_text(outcome),
                        outcome: outcome.name().to_owned(),
                        replayed: false,
                    },
                    prepared.restore,
                )
            }
            Err(error) if error.is_safe_pre_mutation() => {
                // A rejected first PUT never changed the pre-lock overwrite, so
                // its new seed is safe to retire. A prior lockdown seed survives.
                (
                    ChannelReply::refused(
                        "Discord refused the channel action; no mutation was accepted.",
                    ),
                    prepared.rejected_seed,
                )
            }
            Err(
                DiscordError::Timeout | DiscordError::Unavailable(_) | DiscordError::RateLimited,
            ) => {
                // Do not complete or release either claim. Audit uncertainty;
                // neither a same-key retry nor a different key may mutate this lane.
                request.row.outcome = "in_progress".to_owned();
                self.store.record_audit(&request.row).await?;
                return Ok(Some(ChannelReply::uncertain()));
            }
            Err(DiscordError::Rejected(_)) => unreachable!("safe rejection handled above"),
        };
        if !self
            .finish(&ticket, &mut request, &reply, cleanup.as_ref())
            .await?
        {
            return Ok(Some(ChannelReply::uncertain()));
        }
        Ok(Some(reply))
    }

    async fn prepare(
        &self,
        action: ModerationAction,
        request: &Request,
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
                // Persist before PUT; repeated locks never replace the first seed.
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
                    rejected_seed: if prior_record.is_none() {
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
                let call = match domain::plan_unlock(rec.as_ref()) {
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

fn request(interaction: &Interaction, action: ModerationAction) -> Option<Request> {
    let guild_id = interaction.guild_id?.to_string();
    let channel_id = interaction.channel.as_ref()?.id.to_string();
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
