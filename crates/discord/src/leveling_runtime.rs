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
    classify,
    community_store::{
        message_fact, record_fact, rules_accepted_fact, voice_ended_fact, voice_started_fact,
        CommunityStoreError, FactWrite,
    },
    leveling::{
        leaderboard_reply, plan_reward_roles, rank_reply, XpAward, LEADERBOARD_DEFAULT_LIMIT,
    },
    leveling_store::{self, LevelingStoreError},
    ClassifierConfig, ClassifyInput, FactsSink, FunnelHandlers, FunnelStore, HandlerId,
    InviteSnapshotStore, LevelOutcome, LevelingHook, MemberJoinFact, MessageFact,
    RulesAcceptedFact, Snowflake, VoiceEndedFact, VoiceStartedFact,
};

use two_bot_core::automod_runtime::FunnelDisposition;

use crate::{
    pipeline::MessageEligibility, ActionExecutor, DiscordError, InviteSource, NoClassification,
    NoInvites, Pipeline, PipelineSnapshots,
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
pub struct AwardRequest {
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

/// Failures propagate to the gateway supervisor; Display never includes SQL
/// connection details, Discord tokens, response bodies or member content.
#[derive(Debug, thiserror::Error)]
pub enum CommunityFactsError {
    #[error("community facts store operation failed")]
    Store(#[from] CommunityStoreError),
}

#[derive(Debug, Default)]
struct CommunityFactsState {
    pool: Option<PgPool>,
    config: ClassifierConfig,
    pending: Vec<FactWrite>,
    rules: Vec<RulesAcceptedWrite>,
}

/// One buffered gate-clearing: owned inputs for a later async
/// `community_store::rules_accepted_fact` write. Bots are captured like any
/// other member (classified `bot`, never funnel-counted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesAcceptedWrite {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub occurred_at: String,
    pub source_event_id: String,
    pub source: String,
}

/// Buffered `voice_session_started` / `voice_session_ended`,
/// `message_created` and `rules_accepted` capture. The
/// synchronous [`FactsSink`] hook only classifies and buffers; the serial
/// checkpoint writer drains via [`OrderedLevelingPipeline::drain_facts`],
/// which persists through `community_store::record_fact`. Mirrors
/// [`DeferredLeveling`]: no `block_on`, no detached tasks, no mutex held over
/// an await.
///
/// Voice and message facts classify at record time and are dropped while
/// unarmed (no pool), so a staging-gated rollout captures exactly while it
/// scores. Gate-clearings buffer raw even while unarmed — they are tiny,
/// classified at drain with the same boot config, and the unarmed drain drops
/// them — so funnel replay tests can assert the buffer without a database.
/// Either way, disabled (no pool) persists nothing, exactly like
/// [`two_bot_core::NoopFacts`]. A move's end+start pair buffers two rows for
/// one frame.
#[derive(Debug, Clone, Default)]
pub struct DeferredCommunityFacts(Arc<Mutex<CommunityFactsState>>);

impl DeferredCommunityFacts {
    /// Empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm Postgres capture with the scorecard classifier resolved once from
    /// the process environment. Called once at boot when
    /// `TWO_COMMUNITY_SCORECARD=1`; tests call it with their fixture pool.
    pub fn enable(&self, pool: PgPool) {
        let mut state = self.0.lock().expect("community facts lock");
        state.pool = Some(pool);
        state.config = ClassifierConfig::from_env();
    }

    /// Take every buffered gate-clearing, leaving that buffer empty. Order is
    /// arrival order; gate-clear facts are order-insensitive (once per member).
    pub fn take(&self) -> Vec<RulesAcceptedWrite> {
        std::mem::take(&mut self.0.lock().expect("community facts lock").rules)
    }

    /// Buffered gate-clearing count (tests, health).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().expect("community facts lock").rules.len()
    }

    /// Whether the gate-clearing buffer holds no writes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Persist every buffered fact. Returns the inserted count; a duplicate
    /// delivery returns `false` from the store and is not counted twice.
    pub async fn drain(&self) -> Result<usize, CommunityFactsError> {
        let (pool, config, pending, rules) = {
            let mut state = self.0.lock().expect("community facts lock");
            (
                state.pool.clone(),
                state.config.clone(),
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.rules),
            )
        };
        let Some(pool) = pool else {
            return Ok(0);
        };
        let mut inserted = 0;
        for write in &pending {
            if record_fact(&pool, write).await? {
                inserted += 1;
            }
        }
        for write in &rules {
            let input = ClassifyInput {
                guild_id: write.guild_id.to_string(),
                actor_id: write.member_id.to_string(),
                is_bot: write.is_bot,
                webhook_id: None,
                is_staff_automation: false,
                is_raid: false,
                is_staging: false,
                is_test: false,
            };
            let verdict = classify(&config, &input);
            let fact = rules_accepted_fact(
                &input.guild_id,
                &input,
                &write.occurred_at,
                &write.source_event_id,
                &write.source,
                verdict,
                None,
            );
            if record_fact(&pool, &fact).await? {
                inserted += 1;
            }
        }
        Ok(inserted)
    }
}

fn voice_classify(
    config: &ClassifierConfig,
    guild_id: u64,
    member_id: u64,
    is_bot: bool,
) -> (ClassifyInput, two_bot_core::Classification) {
    let input = ClassifyInput {
        guild_id: guild_id.to_string(),
        actor_id: member_id.to_string(),
        is_bot,
        webhook_id: None,
        is_staff_automation: false,
        is_raid: false,
        is_staging: false,
        is_test: false,
    };
    let verdict = classify(config, &input);
    (input, verdict)
}

impl FactsSink for DeferredCommunityFacts {
    fn record_member_join(&self, _fact: MemberJoinFact<'_>) {}

    fn record_rules_accepted(&self, fact: RulesAcceptedFact<'_>) {
        self.0
            .lock()
            .expect("community facts lock")
            .rules
            .push(RulesAcceptedWrite {
                guild_id: fact.guild_id,
                member_id: fact.member_id,
                is_bot: fact.is_bot,
                occurred_at: fact.occurred_at.to_owned(),
                source_event_id: fact.source_event_id.clone(),
                source: fact.source.to_owned(),
            });
    }

    fn record_message(&self, fact: MessageFact<'_>) {
        let mut state = self.0.lock().expect("community facts lock");
        if state.pool.is_none() {
            return;
        }
        // Content-minimized by construction: IDs plus the classifier verdict
        // plus the channel class only, never message content. Bots, webhooks
        // and staff automation are classified and captured here; the funnel
        // gate in `on_message` already keeps them out of the XP/activity
        // counts, so this sink never filters.
        let input = ClassifyInput {
            guild_id: fact.guild_id.to_string(),
            actor_id: fact.member_id.to_string(),
            is_bot: fact.is_bot,
            webhook_id: fact.webhook_id.map(|w| w.to_string()),
            is_staff_automation: fact.is_staff_automation,
            is_raid: false,
            is_staging: false,
            is_test: false,
        };
        let verdict = classify(&state.config, &input);
        state.pending.push(message_fact(
            &input.guild_id,
            fact.message_id,
            &fact.channel_id.to_string(),
            fact.channel_class.as_str(),
            &input,
            fact.occurred_at,
            verdict,
        ));
    }

    fn record_voice_started(&self, fact: VoiceStartedFact<'_>) -> Option<String> {
        let mut state = self.0.lock().expect("community facts lock");
        state.pool.as_ref()?;
        // Bots are classified and captured here; the funnel gate in
        // `on_voice_join` already keeps them out of the XP/activity counts,
        // so this sink never filters. The durable session key is generated
        // now (guild:member:stamp:channel) so the tracker stores it before
        // any database write; a redelivered join reuses the key and dedupes.
        let (input, verdict) =
            voice_classify(&state.config, fact.guild_id, fact.member_id, fact.is_bot);
        let (key, write) = voice_started_fact(
            &input.guild_id,
            &input,
            &fact.channel_id.to_string(),
            fact.occurred_at,
            None,
            verdict,
        );
        state.pending.push(write);
        Some(key)
    }

    fn record_voice_ended(&self, fact: VoiceEndedFact<'_>) {
        let mut state = self.0.lock().expect("community facts lock");
        if state.pool.is_none() {
            return;
        }
        // End-without-start stays honest: the handler supplies
        // `started_at: None` / `duration: None` with an `unknown-start`
        // session key, and `voice_ended_fact` records `startKnown: false`
        // with nulls — never a fabricated start.
        let (input, verdict) =
            voice_classify(&state.config, fact.guild_id, fact.member_id, fact.is_bot);
        state.pending.push(voice_ended_fact(
            &input.guild_id,
            &input,
            &fact.session_key,
            &fact.channel_id.to_string(),
            fact.occurred_at,
            fact.started_at,
            fact.duration_seconds,
            verdict,
        ));
    }
}

/// Award/reply slice of the shared command runtime; no private router or client.
#[derive(Clone)]
pub struct LevelingRuntime {
    pool: PgPool,
    executor: Arc<ActionExecutor>,
    configured_guild: u64,
    onboarding: two_bot_core::OnboardingGates,
}

impl LevelingRuntime {
    pub fn new(
        pool: PgPool,
        executor: Arc<ActionExecutor>,
        configured_guild: u64,
        onboarding: two_bot_core::OnboardingGates,
    ) -> Self {
        Self {
            pool,
            executor,
            configured_guild,
            onboarding,
        }
    }

    async fn award(&self, request: AwardRequest) -> Result<Option<XpAward>, LevelingRuntimeError> {
        if self.configured_guild != request.guild_id {
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

    /// Execute only the shared router's accepted leveling handler. The guild
    /// fence is repeated here before any store read or Discord callback.
    pub async fn handle_interaction(
        &self,
        interaction: &Interaction,
        handler: HandlerId,
    ) -> Result<bool, LevelingRuntimeError> {
        if interaction.guild_id.map(|id| id.get()) != Some(self.configured_guild)
            || !matches!(handler, HandlerId::Rank | HandlerId::Leaderboard)
        {
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
            .answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &response,
            )
            .await?;
        Ok(true)
    }
}

/// Serial dispatches include member-removal/session-reset barriers as well as
/// messages and voice transitions. This deliberately matches the one-shard
/// checkpoint runner; unrelated shards should own separate pipelines.
///
/// `I` serves invite counters and `P` persists invite snapshots; the
/// persistent gateway runner seeds both from the store, while unit and
/// database tests keep the in-memory defaults.
pub struct OrderedLevelingPipeline<S, I = NoInvites, P = PipelineSnapshots> {
    pipeline: Pipeline<S, DeferredLeveling, DeferredCommunityFacts, I, NoClassification, P>,
    pending: DeferredLeveling,
    facts: DeferredCommunityFacts,
    dispatch: tokio::sync::Mutex<()>,
    runtime: Option<LevelingRuntime>,
}

impl<S: FunnelStore> OrderedLevelingPipeline<S> {
    pub fn new(store: S, runtime: Option<LevelingRuntime>) -> Self {
        let pending = DeferredLeveling::default();
        let facts = DeferredCommunityFacts::default();
        Self {
            pipeline: Pipeline::new(
                store,
                Some(pending.clone()),
                Some(facts.clone()),
                NoInvites,
                NoClassification,
            ),
            pending,
            facts,
            dispatch: tokio::sync::Mutex::new(()),
            runtime,
        }
    }
}

impl<S: FunnelStore, I: InviteSource, P: InviteSnapshotStore> OrderedLevelingPipeline<S, I, P> {
    pub fn with_snapshots(
        store: S,
        runtime: Option<LevelingRuntime>,
        invite_source: I,
        snapshots: P,
    ) -> Self {
        let pending = DeferredLeveling::default();
        let facts = DeferredCommunityFacts::default();
        Self {
            pipeline: Pipeline::with_snapshots(
                store,
                Some(pending.clone()),
                Some(facts.clone()),
                invite_source,
                NoClassification,
                snapshots,
            ),
            pending,
            facts,
            dispatch: tokio::sync::Mutex::new(()),
            runtime,
        }
    }

    pub fn handlers(&self) -> &FunnelHandlers<S, DeferredLeveling, DeferredCommunityFacts> {
        self.pipeline.handlers()
    }

    /// Arm Postgres community-facts capture for all live streams. Called once at boot when `TWO_COMMUNITY_SCORECARD=1`; without
    /// it the sink drops every fact, exactly like the previous no-op seam.
    pub fn enable_community_facts(&self, pool: PgPool) {
        self.facts.enable(pool);
    }

    /// Persist buffered community facts without holding the async
    /// dispatch lock. The caller owns ordering (the serial checkpoint
    /// writer); call on every dispatch, even when no award queued — bots,
    /// webhooks and staff automation capture facts but never awards, and a
    /// move's end+start pair buffers two rows for one frame.
    pub async fn drain_facts(&self) -> Result<usize, CommunityFactsError> {
        self.facts.drain().await
    }

    /// Access the cache (shard runner updates, tests seed).
    #[must_use]
    pub fn cache(&self) -> &twilight_cache_inmemory::InMemoryCache {
        self.pipeline.cache()
    }

    /// Register the post-funnel join observer (raid watch). First wins.
    pub fn set_join_observer(&self, observer: std::sync::Arc<dyn crate::pipeline::JoinObserver>) {
        self.pipeline.set_join_observer(observer);
    }

    /// Register the audit-entry observer (containment watch). First wins.
    pub fn set_audit_entry_observer(
        &self,
        observer: std::sync::Arc<dyn crate::pipeline::AuditEntryObserver>,
    ) {
        self.pipeline.set_audit_entry_observer(observer);
    }

    /// Drain deferred XP awards without holding the async dispatch lock.
    /// The caller owns ordering (the serial checkpoint writer); this only
    /// preserves the funnel-before-award sequence per dispatch.
    pub async fn drain(
        &self,
        requests: Vec<AwardRequest>,
    ) -> Result<Vec<XpAward>, LevelingRuntimeError> {
        let mut results = Vec::with_capacity(requests.len());
        if let Some(runtime) = &self.runtime {
            for request in requests {
                if let Some(award) = runtime.award(request).await? {
                    results.push(award);
                }
            }
        }
        Ok(results)
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
        let requests = self.collect_at(event, at, eligibility);
        self.drain(requests).await
    }

    /// Drive one event through the funnel and return its deferred award
    /// requests for the caller to drain. Separated so the blocking
    /// checkpoint writer can funnel synchronously, then await awards.
    pub fn collect_at(
        &self,
        event: &Event,
        at: &str,
        eligibility: MessageEligibility,
    ) -> Vec<AwardRequest> {
        self.pipeline
            .handle_at_with_eligibility(event, at, eligibility);
        self.pending.take()
    }

    /// [`Self::collect_at`] under the automod decision: the disposition and the
    /// replay clock reach the funnel in one call, never `collect_at` as well.
    /// `CaptureOnly` and `None` queue no award, so XP stays once per message.
    pub fn collect_at_with_message_disposition(
        &self,
        event: &Event,
        at: &str,
        disposition: FunnelDisposition,
    ) -> Vec<AwardRequest> {
        self.pipeline
            .handle_at_with_message_disposition(event, at, disposition);
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate<'a>(
        member_id: Snowflake,
        occurred_at: &'a str,
        source: &'a str,
    ) -> RulesAcceptedFact<'a> {
        RulesAcceptedFact {
            guild_id: 100,
            member_id,
            is_bot: false,
            occurred_at,
            source_event_id: format!("100:{member_id}:rules"),
            source,
        }
    }

    #[test]
    fn rules_take_returns_arrival_order_and_empties() {
        let buffer = DeferredCommunityFacts::new();
        assert!(buffer.is_empty());
        buffer.record_rules_accepted(gate(1, "2026-09-20T12:00:00.000Z", "gateway"));
        buffer.record_rules_accepted(gate(2, "2026-09-20T12:01:00.000Z", "backfill:member_list"));
        assert_eq!(buffer.len(), 2);
        let writes = buffer.take();
        assert_eq!(
            writes.iter().map(|w| w.member_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(writes[1].source, "backfill:member_list");
        assert!(buffer.is_empty());
        assert!(buffer.take().is_empty());
    }

    #[test]
    fn rules_buffer_unarmed_while_messages_and_joins_do_not() {
        // Gate-clearings buffer raw even before `enable` (funnel replay
        // asserts the buffer without a database); messages are dropped while
        // unarmed so a staging-gated rollout captures exactly while it
        // scores; joins and voice never buffer on this slice.
        let buffer = DeferredCommunityFacts::new();
        buffer.record_rules_accepted(gate(1, "2026-09-20T12:00:00.000Z", "gateway"));
        buffer.record_member_join(MemberJoinFact {
            guild_id: 100,
            member_id: 1,
            is_bot: false,
            occurred_at: "2026-09-20T12:00:00.000Z",
            source_event_id: "100:1:2026-09-20T12:00:00.000Z",
            source: "invite:abc",
            inviter_id: None,
        });
        buffer.record_message(MessageFact {
            guild_id: 100,
            member_id: 1,
            is_bot: false,
            webhook_id: None,
            is_staff_automation: false,
            message_id: "m1",
            channel_id: 10,
            channel_class: two_bot_core::ChannelClass::Human,
            occurred_at: "2026-09-20T12:01:00.000Z",
        });
        assert_eq!(
            buffer.record_voice_started(VoiceStartedFact {
                guild_id: 100,
                member_id: 1,
                is_bot: false,
                channel_id: 10,
                occurred_at: "2026-09-20T12:02:00.000Z",
            }),
            None
        );
        buffer.record_voice_ended(VoiceEndedFact {
            guild_id: 100,
            member_id: 1,
            is_bot: false,
            session_key: "s".to_owned(),
            channel_id: 10,
            occurred_at: "2026-09-20T12:03:00.000Z",
            started_at: None,
            duration_seconds: None,
        });
        assert_eq!(buffer.len(), 1, "only the gate-clearing buffers unarmed");
    }
}
