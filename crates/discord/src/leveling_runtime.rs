//! Ordered S3 eligibility → async sqlx awards → shared S4 router/REST effects.
//!
//! The synchronous hook only collects requests. One async dispatch lock covers
//! cache/session transitions, draining, awards and rewards: no block_on, detached
//! tasks or std mutex held over an await. The single-shard runner thus preserves
//! member order (including READY/RESUMED barriers) with bounded backpressure.

use std::sync::{Arc, Mutex};

use sqlx::PgPool;
use twilight_model::{
    application::interaction::{
        application_command::CommandOptionValue, Interaction, InteractionData,
    },
    channel::message::{AllowedMentions, MessageFlags},
    gateway::event::Event,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    leveling::{
        leaderboard_reply, plan_reward_roles, rank_reply, XpAward, LEADERBOARD_DEFAULT_LIMIT,
    },
    leveling_store::{self, LevelingStoreError},
    FunnelHandlers, FunnelStore, HandlerId, InteractionHandler, InteractionRouter, LevelOutcome,
    LevelingHook, NoopFacts, SlashOutcome, Snowflake,
};

use crate::{
    pipeline::MessageEligibility, route_interaction, ActionExecutor, DiscordError,
    NoClassification, NoInvites, Pipeline, RoutedInteraction,
};

/// Failures propagate to the gateway supervisor; Display never includes SQL
/// connection details, Discord tokens, response bodies or member content.
#[derive(Debug, thiserror::Error)]
pub enum LevelingRuntimeError {
    #[error("leveling store operation failed")]
    Store(#[from] LevelingStoreError),
    #[error("leveling Discord operation failed")]
    Discord(#[from] DiscordError),
    #[error("invalid leveling interaction")]
    InvalidInteraction,
}

#[derive(Debug)]
struct AwardRequest {
    guild_id: Snowflake,
    member_id: Snowflake,
    channel_id: Snowflake,
    at: String,
    duration: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct DeferredLeveling(Arc<Mutex<Vec<AwardRequest>>>);

impl LevelingHook for DeferredLeveling {
    fn award_message(
        &self,
        guild_id: u64,
        member_id: u64,
        at: &str,
        channel_id: u64,
    ) -> LevelOutcome {
        self.push(guild_id, member_id, at, channel_id, None)
    }

    fn award_voice(
        &self,
        guild_id: u64,
        member_id: u64,
        duration: u64,
        at: &str,
        channel_id: u64,
    ) -> LevelOutcome {
        self.push(guild_id, member_id, at, channel_id, Some(duration))
    }
}

impl DeferredLeveling {
    fn push(
        &self,
        guild_id: u64,
        member_id: u64,
        at: &str,
        channel_id: u64,
        duration: Option<u64>,
    ) -> LevelOutcome {
        self.0.lock().expect("leveling buffer").push(AwardRequest {
            guild_id,
            member_id,
            channel_id,
            at: at.to_owned(),
            duration,
        });
        // Only the awaited store result can report a threshold crossing.
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }

    fn take(&self) -> Vec<AwardRequest> {
        std::mem::take(&mut *self.0.lock().expect("leveling buffer"))
    }
}

/// The two registered identities share one async implementation and executor.
#[derive(Debug)]
struct LevelingHandler(HandlerId);

impl InteractionHandler for LevelingHandler {
    fn id(&self) -> HandlerId {
        self.0
    }
}

pub struct LevelingRuntime {
    pool: PgPool,
    executor: Arc<ActionExecutor>,
    router: InteractionRouter,
    onboarding: two_bot_core::OnboardingGates,
}

impl LevelingRuntime {
    pub fn new(
        pool: PgPool,
        executor: Arc<ActionExecutor>,
        mut router: InteractionRouter,
        onboarding: two_bot_core::OnboardingGates,
    ) -> Self {
        router.register(Box::new(LevelingHandler(HandlerId::Rank)));
        router.register(Box::new(LevelingHandler(HandlerId::Leaderboard)));
        Self {
            pool,
            executor,
            router,
            onboarding,
        }
    }

    async fn award(&self, request: AwardRequest) -> Result<Option<XpAward>, LevelingRuntimeError> {
        if self.router.gates().configured_guild != Some(request.guild_id) {
            return Ok(None);
        }
        let guild = request.guild_id.to_string();
        let member = request.member_id.to_string();
        let channel = request.channel_id.to_string();
        let result = match request.duration {
            Some(duration) => {
                leveling_store::award_voice(
                    &self.pool,
                    &guild,
                    &member,
                    duration,
                    &request.at,
                    Some(&channel),
                )
                .await?
            }
            None => {
                leveling_store::award_message(
                    &self.pool,
                    &guild,
                    &member,
                    &request.at,
                    Some(&channel),
                )
                .await?
            }
        };
        if result.leveled_up
            && !self.onboarding.dry_run
            && two_bot_core::onboarding::level_role_writes_allowed(self.onboarding.mode)
        {
            let rewards = leveling_store::role_rewards(&self.pool, &guild).await?;
            if !rewards.is_empty() {
                let held = self
                    .executor
                    .member_role_ids(request.guild_id, request.member_id)
                    .await?;
                let plan = plan_reward_roles(result.level, &rewards, &held, true, false);
                self.executor
                    .execute_reward_roles(request.guild_id, request.member_id, &plan, None)
                    .await?;
            }
        }
        Ok(Some(result))
    }

    /// The shared router is authoritative; unknown/nonleveling routes are left
    /// for their owning feature, not dispatched by a second command registry.
    pub async fn handle_interaction(
        &self,
        interaction: &Interaction,
    ) -> Result<bool, LevelingRuntimeError> {
        let RoutedInteraction::Slash {
            outcome: SlashOutcome::Handled { handler },
            ..
        } = route_interaction(&self.router, interaction, None)
        else {
            return Ok(false);
        };
        if !matches!(handler, HandlerId::Rank | HandlerId::Leaderboard) {
            return Ok(false);
        }
        let guild = interaction
            .guild_id
            .ok_or(LevelingRuntimeError::InvalidInteraction)?
            .get()
            .to_string();
        let response = match handler {
            HandlerId::Rank => {
                let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref()
                else {
                    return Err(LevelingRuntimeError::InvalidInteraction);
                };
                let user = if let Some(option) = data.options.iter().find(|o| o.name == "member") {
                    let CommandOptionValue::User(id) = option.value else {
                        return Err(LevelingRuntimeError::InvalidInteraction);
                    };
                    data.resolved.as_ref().and_then(|r| r.users.get(&id))
                } else {
                    interaction
                        .member
                        .as_ref()
                        .and_then(|m| m.user.as_ref())
                        .or(interaction.user.as_ref())
                }
                .ok_or(LevelingRuntimeError::InvalidInteraction)?;
                let profile =
                    leveling_store::profile(&self.pool, &guild, &user.id.get().to_string()).await?;
                let reply = rank_reply(&profile, user.global_name.as_deref().unwrap_or(&user.name));
                InteractionResponse {
                    kind: InteractionResponseType::ChannelMessageWithSource,
                    data: Some(InteractionResponseData {
                        content: Some(reply.content),
                        flags: Some(MessageFlags::EPHEMERAL),
                        ..Default::default()
                    }),
                }
            }
            HandlerId::Leaderboard => {
                let entries =
                    leveling_store::leaderboard(&self.pool, &guild, LEADERBOARD_DEFAULT_LIMIT)
                        .await?;
                let reply = leaderboard_reply(&entries);
                InteractionResponse {
                    kind: InteractionResponseType::ChannelMessageWithSource,
                    data: Some(InteractionResponseData {
                        content: Some(reply.content),
                        allowed_mentions: Some(AllowedMentions::default()),
                        ..Default::default()
                    }),
                }
            }
            _ => unreachable!(),
        };
        self.executor
            .answer_interaction(interaction.id.get(), &interaction.token, &response)
            .await?;
        Ok(true)
    }
}

/// Serial dispatches include member-removal/session-reset barriers as well as
/// messages and voice transitions. This deliberately matches the one-shard
/// checkpoint runner; unrelated shards should own separate pipelines.
pub struct OrderedLevelingPipeline<S> {
    pipeline: Pipeline<S, DeferredLeveling>,
    pending: DeferredLeveling,
    dispatch: tokio::sync::Mutex<()>,
    runtime: Option<LevelingRuntime>,
}

impl<S: FunnelStore> OrderedLevelingPipeline<S> {
    pub fn new(store: S, runtime: Option<LevelingRuntime>) -> Self {
        let pending = DeferredLeveling::default();
        Self {
            pipeline: Pipeline::new(
                store,
                Some(pending.clone()),
                None,
                NoInvites,
                NoClassification,
            ),
            pending,
            dispatch: tokio::sync::Mutex::new(()),
            runtime,
        }
    }

    pub fn handlers(&self) -> &FunnelHandlers<S, DeferredLeveling, NoopFacts> {
        self.pipeline.handlers()
    }

    pub async fn handle(&self, event: &Event) -> Result<Vec<XpAward>, LevelingRuntimeError> {
        self.handle_at(
            event,
            &two_bot_core::now_iso(),
            MessageEligibility::default(),
        )
        .await
    }

    pub async fn handle_at(
        &self,
        event: &Event,
        at: &str,
        eligibility: MessageEligibility,
    ) -> Result<Vec<XpAward>, LevelingRuntimeError> {
        let _dispatch = self.dispatch.lock().await;
        self.pipeline.handle_at(event, at, eligibility);
        let requests = self.pending.take();
        let mut results = Vec::with_capacity(requests.len());
        if let Some(runtime) = &self.runtime {
            for request in requests {
                if let Some(award) = runtime.award(request).await? {
                    results.push(award);
                }
            }
            if let Event::InteractionCreate(interaction) = event {
                runtime.handle_interaction(&interaction.0).await?;
            }
        }
        Ok(results)
    }
}
