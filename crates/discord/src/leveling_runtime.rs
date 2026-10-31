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
    gateway::event::Event,
};
use two_bot_core::{
    classify,
    community_store::{
        member_join_fact, member_join_metadata, message_fact, rules_accepted_fact,
        voice_ended_fact, voice_started_fact, CommunityStoreError, FactWrite, MessageFactInput,
    },
    leveling::{
        leaderboard_reply_for_season, plan_reward_roles, rank_reply, season_id_for_ms,
        RewardPlanError, XpAward, LEADERBOARD_DEFAULT_LIMIT,
    },
    leveling_store::{self, LevelingStoreError},
    ClassifierConfig, ClassifyInput, FactsSink, FunnelHandlers, FunnelStore, HandlerId,
    InviteSnapshotStore, LevelOutcome, LevelingHook, MemberJoinFact, MessageFact,
    RulesAcceptedFact, Snowflake, VoiceEndedFact, VoiceStartedFact,
};

use two_bot_core::automod_runtime::FunnelDisposition;

use crate::{
    pipeline::MessageEligibility, ActionExecutor, DiscordError, InviteSource, NoInvites, Pipeline,
    PipelineSnapshots, SettingsChannelClassifier,
};

/// Stable reason codes for poisoned deferred capture buffers. These literals
/// are the supervised-failure contract: error Displays, the gateway failure
/// line and `docs/deferred-capture-poison.md` all carry them verbatim, and
/// the poison acceptance suite pins them. Never rename one without updating
/// all three.
pub const DEFERRED_LEVELING_POISONED: &str = "deferred_leveling_poisoned";
pub const DEFERRED_COMMUNITY_FACTS_POISONED: &str = "deferred_community_facts_poisoned";
/// Next action for either poisoned buffer: supervisor restart inspection
/// under the existing machine contract. The buffer never heals in place.
pub const SUPERVISOR_RESTART_INSPECTION: &str = "supervisor_restart_inspection";

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
    /// The award committed but applying its level-up roles failed. The XP and
    /// the computed level-up are durable; `awards` carries them so the caller
    /// never reports the level-up as failed.
    #[error("leveling role application failed after the XP award committed: {cause}")]
    RoleApplication {
        cause: RoleApplicationError,
        awards: Vec<XpAward>,
    },
    /// The deferred leveling buffer mutex is poisoned: a previous holder
    /// panicked, so the buffered award requests are untrusted and stay
    /// queued. Never healed in place; the supervisor restarts under the
    /// existing contract. Carries no committed awards.
    #[error("deferred leveling buffer poisoned (deferred_leveling_poisoned); next action: supervisor_restart_inspection")]
    LevelingPoisoned,
    /// The deferred community-facts buffer mutex is poisoned, observed while
    /// collecting a dispatch: capture already latched its loss, so the owner
    /// refuses before any award work. Same never-heal recovery contract as
    /// the leveling poison refusal.
    #[error("deferred community facts buffer poisoned (deferred_community_facts_poisoned); next action: supervisor_restart_inspection")]
    CommunityFactsPoisoned,
}

impl LevelingRuntimeError {
    /// Awards committed before the error. Only a role-application failure
    /// carries any; every other error returns an empty slice.
    #[must_use]
    pub fn committed_awards(&self) -> &[XpAward] {
        match self {
            Self::RoleApplication { awards, .. } => awards,
            _ => &[],
        }
    }

    /// Put awards committed earlier in the same drain ahead of the ones this
    /// error already carries; non-role errors are unchanged.
    fn with_prior_awards(mut self, mut prior: Vec<XpAward>) -> Self {
        if let Self::RoleApplication { awards, .. } = &mut self {
            prior.append(awards);
            *awards = prior;
        }
        self
    }
}

/// Why a committed level-up's role application failed. Display carries role
/// ids and fixed refusal evidence only: no SQL, tokens or response bodies.
#[derive(Debug, thiserror::Error)]
pub enum RoleApplicationError {
    #[error("role rewards could not be read")]
    Store(#[from] LevelingStoreError),
    /// A read or a pre-mutation refusal: no role was written.
    #[error("role application Discord operation failed")]
    Discord(#[from] DiscordError),
    /// The plan hit the per-list role-write cap: no role was written.
    #[error(transparent)]
    Plan(#[from] RewardPlanError),
    /// The first refused write aborted the rest. Earlier roles stay applied.
    #[error(
        "role {role_id} refused: {reason} ({} applied, {} skipped)",
        applied.len(),
        skipped.len()
    )]
    Refused {
        role_id: String,
        reason: String,
        applied: Vec<String>,
        skipped: Vec<String>,
    },
}

#[derive(Debug)]
pub struct AwardRequest {
    guild_id: Snowflake,
    member_id: Snowflake,
    channel_id: Snowflake,
    at: String,
    duration: Option<u64>,
    /// Transient short-message shape: `None` is unknown (MESSAGE_CONTENT
    /// absent, existing award preserved); `Some` is the trimmed scalar count.
    /// Bodies never travel here, only the count.
    pub content_length: Option<usize>,
    /// Whether the gateway message carried an embed or attachment.
    pub has_embed_or_attachment: bool,
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
        shape: two_bot_core::MessageShape,
    ) -> LevelOutcome {
        self.push_message(guild_id, member_id, at, channel_id, shape)
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
    fn push_message(
        &self,
        guild_id: u64,
        member_id: u64,
        at: &str,
        channel_id: u64,
        shape: two_bot_core::MessageShape,
    ) -> LevelOutcome {
        // The hook is infallible, so a poisoned buffer latches: report no
        // threshold crossing here and let the owning collection/drain refuse
        // with the typed poison error instead of panicking.
        let Ok(mut pending) = self.0.lock() else {
            return LevelOutcome {
                leveled_up: false,
                level: 0,
            };
        };
        pending.push(AwardRequest {
            guild_id,
            member_id,
            channel_id,
            at: at.to_owned(),
            duration: None,
            content_length: shape.content_length,
            has_embed_or_attachment: shape.has_embed_or_attachment,
        });
        // Only the awaited store result can report a threshold crossing.
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }

    fn push(
        &self,
        guild_id: u64,
        member_id: u64,
        at: &str,
        channel_id: u64,
        duration: Option<u64>,
    ) -> LevelOutcome {
        // Same latch as `push_message`: never panic on a poisoned buffer.
        let Ok(mut pending) = self.0.lock() else {
            return LevelOutcome {
                leveled_up: false,
                level: 0,
            };
        };
        pending.push(AwardRequest {
            guild_id,
            member_id,
            channel_id,
            at: at.to_owned(),
            duration,
            content_length: None,
            has_embed_or_attachment: false,
        });
        // Only the awaited store result can report a threshold crossing.
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }

    fn take(&self) -> Result<Vec<AwardRequest>, LevelingRuntimeError> {
        // Never `into_inner` a poisoned buffer and never report its
        // untrusted contents as an empty success: refuse typed.
        let mut pending = self
            .0
            .lock()
            .map_err(|_| LevelingRuntimeError::LevelingPoisoned)?;
        Ok(std::mem::take(&mut *pending))
    }

    /// Refuse when the buffer mutex is poisoned without touching the
    /// untrusted contents. Drains call this before any store or Discord
    /// work, so even an empty batch on a damaged buffer is a typed
    /// refusal, never an `Ok` success.
    fn check_healthy(&self) -> Result<(), LevelingRuntimeError> {
        if self.0.is_poisoned() {
            Err(LevelingRuntimeError::LevelingPoisoned)
        } else {
            Ok(())
        }
    }
}

/// Failures propagate to the gateway supervisor; Display never includes SQL
/// connection details, Discord tokens, response bodies or member content.
#[derive(Debug, thiserror::Error)]
pub enum CommunityFactsError {
    #[error("community facts store operation failed")]
    Store(#[from] CommunityStoreError),
    /// The community-facts buffer mutex is poisoned: a previous holder
    /// panicked, so every buffered family is untrusted and stays queued.
    /// Never healed, cleared or partially drained; distinct from a normal
    /// empty or disabled drain. The supervisor restarts under the existing
    /// contract.
    #[error("deferred community facts buffer poisoned (deferred_community_facts_poisoned); next action: supervisor_restart_inspection")]
    Poisoned,
}

impl CommunityFactsError {
    /// True only for the poisoned-buffer refusal, so the gateway holds the
    /// cursor on poison while ordinary store failures keep their
    /// warn-and-continue precedent.
    #[must_use]
    pub fn is_poison(&self) -> bool {
        matches!(self, Self::Poisoned)
    }
}

#[derive(Debug, Default)]
struct CommunityFactsState {
    pool: Option<PgPool>,
    config: ClassifierConfig,
    pending: Vec<FactWrite>,
    rules: Vec<RulesAcceptedWrite>,
    joins: Vec<MemberJoinWrite>,
    /// Capture was armed at least once. A never-armed buffer is disabled
    /// capture (its drain discards, like `NoopFacts`); once armed, a drain
    /// with no pool retains every queued fact instead of discarding it.
    capture_armed: bool,
    /// Cumulative oldest-first evictions from bounded drain requeues, one
    /// counter per family, in (pending, rules, joins) order.
    dropped_pending: u64,
    dropped_rules: u64,
    dropped_joins: u64,
}

/// Per-family cap for facts requeued by a failed or cancelled drain. It
/// mirrors the game-activity requeue precedent's 8,192 bound: past the cap
/// the oldest facts are evicted and counted, never silently dropped and
/// never at another family's expense.
const MAX_QUEUED_FACTS_PER_FAMILY: usize = 8_192;

/// Put `unwritten` facts back ahead of the facts already in `queue`
/// (arrivals captured while the drain was writing), evicting the oldest
/// past [`MAX_QUEUED_FACTS_PER_FAMILY`] and counting every eviction in
/// `dropped`. Exactly at the cap nothing is evicted.
fn requeue_family<T>(queue: &mut Vec<T>, mut unwritten: Vec<T>, dropped: &mut u64) {
    unwritten.append(queue);
    let overflow = unwritten.len().saturating_sub(MAX_QUEUED_FACTS_PER_FAMILY);
    if overflow > 0 {
        unwritten.drain(..overflow);
        *dropped += overflow as u64;
    }
    *queue = unwritten;
}

/// The batch one drain took from the buffer, with per-family counts of the
/// writes the store acknowledged. Dropping the guard while `complete` is
/// false — the store error path and a caller timeout dropping the drain
/// future alike — requeues exactly the unacknowledged suffix of each
/// family, so acknowledged facts (including `Ok(false)` duplicates) are
/// never recorded twice. Never panics and never holds the buffer mutex
/// over an await; a poisoned buffer keeps its latch and the suffix with it.
struct DrainRequeue {
    state: Arc<Mutex<CommunityFactsState>>,
    pending: Vec<FactWrite>,
    rules: Vec<RulesAcceptedWrite>,
    joins: Vec<MemberJoinWrite>,
    pending_done: usize,
    rules_done: usize,
    joins_done: usize,
    complete: bool,
}

impl Drop for DrainRequeue {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        // The mutex guard derefs as one place, so the counters travel out
        // and back instead of borrowing each queue and its counter in the
        // same call.
        let mut dropped = (
            state.dropped_pending,
            state.dropped_rules,
            state.dropped_joins,
        );
        let pending = self.pending.split_off(self.pending_done);
        requeue_family(&mut state.pending, pending, &mut dropped.0);
        let rules = self.rules.split_off(self.rules_done);
        requeue_family(&mut state.rules, rules, &mut dropped.1);
        let joins = self.joins.split_off(self.joins_done);
        requeue_family(&mut state.joins, joins, &mut dropped.2);
        state.dropped_pending = dropped.0;
        state.dropped_rules = dropped.1;
        state.dropped_joins = dropped.2;
    }
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

/// One buffered gateway join: owned inputs for a later async
/// `community_store::member_join_fact` write. Bots are captured like any
/// other member (classified `bot`, never funnel-counted). The invite
/// attribution (`source` + `inviter_id`) is resolved upstream in the
/// pipeline and travels in the buffer, so the fact keeps it without
/// re-reading invites at drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberJoinWrite {
    pub guild_id: Snowflake,
    pub member_id: Snowflake,
    pub is_bot: bool,
    pub occurred_at: String,
    pub source_event_id: String,
    pub source: String,
    pub inviter_id: Option<Snowflake>,
}

/// Buffered `voice_session_started` / `voice_session_ended`,
/// `message_created`, `rules_accepted` and `member_joined` capture. The
/// synchronous [`FactsSink`] hook only classifies and buffers; the serial
/// checkpoint writer drains via [`OrderedLevelingPipeline::drain_facts`],
/// which persists through `community_store::record_fact`. Mirrors
/// [`DeferredLeveling`]: no `block_on`, no detached tasks, no mutex held over
/// an await.
///
/// Voice and message facts classify at record time and are dropped while
/// unarmed (no pool), so a staging-gated rollout captures exactly while it
/// scores. Gate-clearings and joins buffer raw even while unarmed — they are
/// tiny, classified at drain with the same boot config — so funnel replay
/// tests can assert the buffer without a database. A never-armed (disabled)
/// drain drops them, persisting nothing, exactly like
/// [`two_bot_core::NoopFacts`]; once capture has been armed, a drain with no
/// pool retains them instead. A move's end+start pair buffers two rows for
/// one frame.
///
/// Loss window: once armed, a failed or cancelled (caller-timeout) drain
/// requeues every unwritten fact ahead of newer arrivals, so a transient
/// store outage loses nothing; the queues are memory-only, so facts still
/// buffered at process exit, or behind a poisoned mutex, are lost, and a
/// requeue past 8,192 facts in one family evicts that family's oldest
/// facts, counted by [`DeferredCommunityFacts::dropped_counts`]. A write the
/// store committed but never acknowledged is retried on the next drain and
/// deduped by its idempotency key.
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
    /// A poisoned buffer refuses typed and keeps its previous config.
    pub fn enable(&self, pool: PgPool) -> Result<(), CommunityFactsError> {
        self.enable_with_config(pool, ClassifierConfig::from_env())
    }

    /// Arm Postgres capture with an explicit classifier config, so the
    /// caller can seed the channel classifier from the same config and
    /// the two never disagree. A poisoned buffer refuses typed: the config
    /// is not applied, and the owning drain refuses instead of panicking.
    pub fn enable_with_config(
        &self,
        pool: PgPool,
        config: ClassifierConfig,
    ) -> Result<(), CommunityFactsError> {
        let mut state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        state.pool = Some(pool);
        state.config = config;
        state.capture_armed = true;
        Ok(())
    }

    /// Replace the classifier config used at record/drain time. Paired
    /// with the channel classifier refresh in
    /// [`OrderedLevelingPipeline::refresh_classifier_config`]. A poisoned
    /// buffer refuses typed like [`Self::enable_with_config`].
    pub fn refresh_config(&self, config: ClassifierConfig) -> Result<(), CommunityFactsError> {
        let mut state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        if state.config != config {
            state.config = config;
        }
        Ok(())
    }

    /// Refuse when the buffer mutex is poisoned without touching the
    /// untrusted families. Collectors call this after the funnel so a
    /// latched capture loss reaches the owner before any award work.
    fn check_healthy(&self) -> Result<(), CommunityFactsError> {
        if self.0.is_poisoned() {
            Err(CommunityFactsError::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Take every buffered gate-clearing, leaving that buffer empty. Order is
    /// arrival order; gate-clear facts are order-insensitive (once per member).
    /// Rules only — joins have their own [`Self::take_joins`]. A poisoned
    /// buffer refuses typed and keeps its untrusted contents queued.
    pub fn take(&self) -> Result<Vec<RulesAcceptedWrite>, CommunityFactsError> {
        let mut state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        Ok(std::mem::take(&mut state.rules))
    }

    /// Take every buffered gateway join, leaving that buffer empty. Order is
    /// arrival order; join facts are order-insensitive (one per join burst).
    /// A poisoned buffer refuses typed and keeps its untrusted contents queued.
    pub fn take_joins(&self) -> Result<Vec<MemberJoinWrite>, CommunityFactsError> {
        let mut state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        Ok(std::mem::take(&mut state.joins))
    }

    /// Take every buffered fact write (`message_created`,
    /// `voice_session_started` / `voice_session_ended`), leaving that
    /// buffer empty. Order is arrival order. A poisoned buffer refuses
    /// typed and keeps its untrusted contents queued.
    pub fn take_pending(&self) -> Result<Vec<FactWrite>, CommunityFactsError> {
        let mut state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        Ok(std::mem::take(&mut state.pending))
    }

    /// Buffered gate-clearing count (tests, health). Rules only. A poisoned
    /// buffer refuses typed, never a false zero.
    pub fn len(&self) -> Result<usize, CommunityFactsError> {
        let state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        Ok(state.rules.len())
    }

    /// Whether the gate-clearing buffer holds no writes. Rules only. A
    /// poisoned buffer refuses typed, never a false healthy read.
    pub fn is_empty(&self) -> Result<bool, CommunityFactsError> {
        Ok(self.len()? == 0)
    }

    /// Buffered gateway-join count (tests, health). A poisoned buffer
    /// refuses typed, never a false zero.
    pub fn joins_len(&self) -> Result<usize, CommunityFactsError> {
        let state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        Ok(state.joins.len())
    }

    /// Cumulative facts evicted by bounded drain requeues, in (pending,
    /// rules, joins) order. Successful and empty drains never reset the
    /// counters. A poisoned buffer refuses typed, never a false zero.
    pub fn dropped_counts(&self) -> Result<(u64, u64, u64), CommunityFactsError> {
        let state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
        Ok((
            state.dropped_pending,
            state.dropped_rules,
            state.dropped_joins,
        ))
    }

    /// Persist every buffered fact. Returns the inserted count; a duplicate
    /// delivery returns `false` from the store and is not counted twice. A
    /// poisoned buffer refuses typed before any store work and keeps every
    /// family queued: never a partial batch, never a false zero.
    ///
    /// On the first store error — or when the caller drops this future, as
    /// the gateway checkpoint deadline does — every fact the store has not
    /// acknowledged, across all three families, returns to the front of its
    /// queue ahead of arrivals captured meanwhile (bounded per family,
    /// evictions counted by [`Self::dropped_counts`]), so the next drain
    /// retries exactly the unwritten suffix and never re-records an
    /// acknowledged write. With no pool, a
    /// never-armed buffer discards its raw gate-clearings and joins like
    /// disabled capture always has, while an armed buffer keeps every
    /// queued fact for a later drain; both return zero.
    pub async fn drain(&self) -> Result<usize, CommunityFactsError> {
        let (pool, config, mut guard) = {
            let mut state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;
            let Some(pool) = state.pool.clone() else {
                if !state.capture_armed {
                    state.pending.clear();
                    state.rules.clear();
                    state.joins.clear();
                }
                return Ok(0);
            };
            let guard = DrainRequeue {
                state: Arc::clone(&self.0),
                pending: std::mem::take(&mut state.pending),
                rules: std::mem::take(&mut state.rules),
                joins: std::mem::take(&mut state.joins),
                pending_done: 0,
                rules_done: 0,
                joins_done: 0,
                complete: false,
            };
            (pool, state.config.clone(), guard)
        };
        let mut inserted = 0;
        for write in &guard.pending {
            if record_fact(&pool, write).await? {
                inserted += 1;
            }
            guard.pending_done += 1;
        }
        for write in &guard.rules {
            let fact = {
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
                rules_accepted_fact(
                    &input.guild_id,
                    &input,
                    &write.occurred_at,
                    &write.source_event_id,
                    &write.source,
                    verdict,
                    None,
                )
            };
            if record_fact(&pool, &fact).await? {
                inserted += 1;
            }
            guard.rules_done += 1;
        }
        for write in &guard.joins {
            let fact = {
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
                member_join_fact(
                    &input.guild_id,
                    &input,
                    &write.occurred_at,
                    &write.source_event_id,
                    &write.source,
                    verdict,
                    member_join_metadata(write.inviter_id),
                )
            };
            if record_fact(&pool, &fact).await? {
                inserted += 1;
            }
            guard.joins_done += 1;
        }
        guard.complete = true;
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
    // The sink is infallible, so every callback latches on a poisoned
    // buffer: drop the one fact without panicking and let the owning
    // collection/drain refuse typed. Nothing here claims accepted work.
    fn record_member_join(&self, fact: MemberJoinFact<'_>) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        state.joins.push(MemberJoinWrite {
            guild_id: fact.guild_id,
            member_id: fact.member_id,
            is_bot: fact.is_bot,
            occurred_at: fact.occurred_at.to_owned(),
            source_event_id: fact.source_event_id.to_owned(),
            source: fact.source.to_owned(),
            inviter_id: fact.inviter_id,
        });
    }

    fn record_rules_accepted(&self, fact: RulesAcceptedFact<'_>) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        state.rules.push(RulesAcceptedWrite {
            guild_id: fact.guild_id,
            member_id: fact.member_id,
            is_bot: fact.is_bot,
            occurred_at: fact.occurred_at.to_owned(),
            source_event_id: fact.source_event_id.clone(),
            source: fact.source.to_owned(),
        });
    }

    fn record_message(&self, fact: MessageFact<'_>) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        if state.pool.is_none() {
            return;
        }
        // Content-minimized by construction: IDs plus the classifier verdict
        // plus the channel class plus the measured content-length integer
        // only, never message content. Bots, webhooks
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
        state.pending.push(message_fact(MessageFactInput {
            guild_id: &input.guild_id,
            message_id: fact.message_id,
            channel_id: &fact.channel_id.to_string(),
            channel_class: fact.channel_class.as_str(),
            actor: &input,
            occurred_at: fact.occurred_at,
            content_length: fact.content_length,
            classification: verdict,
        }));
    }

    fn record_voice_started(&self, fact: VoiceStartedFact<'_>) -> Option<String> {
        // Latch like the other callbacks, and never fabricate a session
        // key the drain cannot persist: poisoned means `None`.
        let Ok(mut state) = self.0.lock() else {
            return None;
        };
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
        let Ok(mut state) = self.0.lock() else {
            return;
        };
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

/// Sanitized completion for an acknowledged `/rank` store failure: bounded,
/// actionable, leaking no store detail. Mirrors the repo's "; try again."
/// recovery convention; the store error still propagates for the caller's log.
const RANK_UNAVAILABLE_REPLY: &str = "Rank is unavailable right now; try again.";
/// Sanitized completion for an acknowledged `/leaderboard` store failure:
/// same shape as [`RANK_UNAVAILABLE_REPLY`].
const LEADERBOARD_UNAVAILABLE_REPLY: &str = "Leaderboard is unavailable right now; try again.";

/// Validated store read awaiting its acknowledgement: built from the
/// interaction alone (no store touch) so the defer can precede the read.
enum PendingRead {
    Rank {
        member_id: String,
        display_name: String,
    },
    Leaderboard {
        season: String,
    },
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
                    request.content_length,
                    request.has_embed_or_attachment,
                )
                .await?
            }
        };
        if result.leveled_up
            && !self.onboarding.dry_run
            && two_bot_core::onboarding::level_role_writes_allowed(self.onboarding.mode)
        {
            self.apply_level_roles(&request, &guild, result.level)
                .await
                .map_err(|cause| LevelingRuntimeError::RoleApplication {
                    cause,
                    awards: vec![result],
                })?;
        }
        Ok(Some(result))
    }

    /// Plan and apply the level-up roles after the award committed.
    async fn apply_level_roles(
        &self,
        request: &AwardRequest,
        guild: &str,
        level: u64,
    ) -> Result<(), RoleApplicationError> {
        let rewards = leveling_store::role_rewards(&self.pool, guild).await?;
        if rewards.is_empty() {
            return Ok(());
        }
        let held = self
            .executor
            .member_role_ids(request.guild_id, request.member_id)
            .await?;
        let plan = plan_reward_roles(level, &rewards, &held, true, false)?;
        let (applied, failed, skipped) = self
            .executor
            .execute_reward_roles(request.guild_id, request.member_id, &plan, None)
            .await?;
        match failed {
            None => Ok(()),
            Some((role_id, reason)) => Err(RoleApplicationError::Refused {
                role_id,
                reason,
                applied,
                skipped,
            }),
        }
    }

    /// Execute only the shared router's accepted leveling handler. The guild
    /// fence is repeated here before any store read or Discord callback.
    pub async fn handle_interaction(
        &self,
        interaction: &Interaction,
        handler: HandlerId,
    ) -> Result<bool, LevelingRuntimeError> {
        self.handle_interaction_at(interaction, handler, &two_bot_core::now_iso())
            .await
    }

    /// [`Self::handle_interaction`] at an explicit event-time instant (AC3):
    /// the routed `/leaderboard` serves `season_id_for_ms` of `at`, scoped to
    /// `member_season_xp`, with an explicit season label and no lifetime
    /// fallback. `/rank` stays lifetime-based at every instant. Shaped like
    /// the pipeline's existing `handle_at` ISO-millis convention; the season
    /// convention itself is unchanged.
    ///
    /// Defer-then-edit lifecycle: the initial acknowledgement (ephemeral for
    /// rank, public for leaderboard) precedes either store read, so a slow
    /// database cannot exhaust Discord's initial-response deadline. A failed
    /// or uncertain acknowledgement returns here with zero store reads and no
    /// further sends — only provably pre-wire Blocked contention is retried
    /// inside the executor, and that retry cannot double-acknowledge. A
    /// successful acknowledgement is completed by exactly one original-response
    /// edit: the rendered reply on success, one sanitized actionable
    /// unavailable reply on store failure (the store error still propagates
    /// so the caller logs the real cause). Never a second initial callback.
    pub async fn handle_interaction_at(
        &self,
        interaction: &Interaction,
        handler: HandlerId,
        at: &str,
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
        // Pure validation before the acknowledgement: resolving the rank
        // target and the leaderboard season touches no store.
        let ephemeral = matches!(handler, HandlerId::Rank);
        let read = match handler {
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
                PendingRead::Rank {
                    member_id: user.id.get().to_string(),
                    display_name: user.global_name.as_deref().unwrap_or(&user.name).to_owned(),
                }
            }
            HandlerId::Leaderboard => {
                let at_ms = two_bot_core::parse_iso_millis(at)
                    .ok_or(LevelingRuntimeError::InvalidInteraction)?;
                PendingRead::Leaderboard {
                    season: season_id_for_ms(at_ms),
                }
            }
            _ => unreachable!(),
        };
        self.executor
            .answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &super::interactions::deferred_response(ephemeral),
            )
            .await?;
        let content = match read {
            PendingRead::Rank {
                member_id,
                display_name,
            } => leveling_store::profile(&self.pool, &guild, &member_id)
                .await
                .map(|profile| rank_reply(&profile, &display_name).content),
            PendingRead::Leaderboard { season } => leveling_store::seasonal_leaderboard(
                &self.pool,
                &guild,
                &season,
                LEADERBOARD_DEFAULT_LIMIT,
            )
            .await
            .map(|entries| leaderboard_reply_for_season(&entries, &season).content),
        };
        let application_id = interaction.application_id.get();
        match content {
            Ok(content) => {
                self.executor
                    .edit_interaction_response_with_blocked_retry(
                        application_id,
                        &interaction.token,
                        &content,
                    )
                    .await?;
                Ok(true)
            }
            Err(store) => {
                let unavailable = if ephemeral {
                    RANK_UNAVAILABLE_REPLY
                } else {
                    LEADERBOARD_UNAVAILABLE_REPLY
                };
                // Best-effort completion: attempt the edit, then report the
                // store failure regardless. The store error is the root cause
                // the caller must log; a simultaneous edit failure must not
                // hide it (same precedence as the channel slice's
                // Persistence-over-Edit return).
                let _edit = self
                    .executor
                    .edit_interaction_response_with_blocked_retry(
                        application_id,
                        &interaction.token,
                        unavailable,
                    )
                    .await;
                Err(LevelingRuntimeError::Store(store))
            }
        }
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
    pipeline:
        Pipeline<S, DeferredLeveling, DeferredCommunityFacts, I, SettingsChannelClassifier, P>,
    classifier: SettingsChannelClassifier,
    pending: DeferredLeveling,
    facts: DeferredCommunityFacts,
    dispatch: tokio::sync::Mutex<()>,
    runtime: Option<LevelingRuntime>,
}

impl<S: FunnelStore> OrderedLevelingPipeline<S> {
    pub fn new(store: S, runtime: Option<LevelingRuntime>) -> Self {
        let pending = DeferredLeveling::default();
        let facts = DeferredCommunityFacts::default();
        let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_env());
        let pipeline = Pipeline::new(
            store,
            Some(pending.clone()),
            Some(facts.clone()),
            NoInvites,
            classifier.clone(),
        );
        // The live wrapper fails closed: no lengths are measured until the
        // gateway runtime declares its `MESSAGE_CONTENT` subscription.
        pipeline.set_message_content_delivered(false);
        Self {
            pipeline,
            classifier,
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
        let classifier = SettingsChannelClassifier::new(ClassifierConfig::from_env());
        let pipeline = Pipeline::with_snapshots(
            store,
            Some(pending.clone()),
            Some(facts.clone()),
            invite_source,
            classifier.clone(),
            snapshots,
        );
        // Fail closed like `new`: the gateway runtime declares the real
        // `MESSAGE_CONTENT` subscription before its shard runs.
        pipeline.set_message_content_delivered(false);
        Self {
            pipeline,
            classifier,
            pending,
            facts,
            dispatch: tokio::sync::Mutex::new(()),
            runtime,
        }
    }

    pub fn handlers(&self) -> &FunnelHandlers<S, DeferredLeveling, DeferredCommunityFacts> {
        self.pipeline.handlers()
    }

    /// Declare whether the shard feeding this pipeline subscribed
    /// `MESSAGE_CONTENT`; message lengths are measured only when it did.
    pub fn set_message_content_delivered(&self, delivered: bool) {
        self.pipeline.set_message_content_delivered(delivered);
    }

    /// Arm Postgres community-facts capture for all live streams. Called once at boot when `TWO_COMMUNITY_SCORECARD=1`; without
    /// it the sink drops every fact, exactly like the previous no-op seam.
    /// A poisoned facts buffer refuses typed and the classifier keeps its
    /// previous config, so the two still agree.
    pub fn enable_community_facts(&self, pool: PgPool) -> Result<(), CommunityFactsError> {
        // One config load seeds both the facts sink and the channel
        // classifier, so classification at record time and the stamped
        // channel class always read the same sets.
        let config = ClassifierConfig::from_env();
        self.facts.enable_with_config(pool, config.clone())?;
        self.classifier.refresh(config);
        Ok(())
    }

    /// Refresh the classifier config live (settings poll): the channel
    /// classifier and the facts sink swap together, so a stored channel
    /// set reclassifies without a restart and the two never disagree.
    /// A poisoned facts buffer refuses typed before the classifier swaps,
    /// so a failed refresh changes neither side.
    pub fn refresh_classifier_config(
        &self,
        config: ClassifierConfig,
    ) -> Result<(), CommunityFactsError> {
        self.facts.refresh_config(config.clone())?;
        self.classifier.refresh(config);
        Ok(())
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
    /// preserves the funnel-before-award sequence per dispatch. A fault
    /// after collection refuses before any store or Discord work, so even
    /// an empty batch on a poisoned buffer is a typed refusal, never an
    /// `Ok` success.
    pub async fn drain(
        &self,
        requests: Vec<AwardRequest>,
    ) -> Result<Vec<XpAward>, LevelingRuntimeError> {
        self.pending.check_healthy()?;
        let mut results = Vec::with_capacity(requests.len());
        if let Some(runtime) = &self.runtime {
            for request in requests {
                match runtime.award(request).await {
                    Ok(Some(award)) => results.push(award),
                    Ok(None) => {}
                    Err(error) => return Err(error.with_prior_awards(results)),
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
        let requests = self.collect_at(event, at, eligibility)?;
        self.drain(requests).await
    }

    /// Drive one event through the funnel and return its deferred award
    /// requests for the caller to drain. Separated so the blocking
    /// checkpoint writer can funnel synchronously, then await awards. A
    /// poisoned leveling buffer refuses typed instead of an empty batch,
    /// and a poisoned facts buffer — whose sink latched during the funnel
    /// above — refuses before any award work is claimed.
    pub fn collect_at(
        &self,
        event: &Event,
        at: &str,
        eligibility: MessageEligibility,
    ) -> Result<Vec<AwardRequest>, LevelingRuntimeError> {
        self.pipeline
            .handle_at_with_eligibility(event, at, eligibility);
        self.check_facts_healthy()?;
        self.pending.take()
    }

    /// [`Self::collect_at`] under the automod decision: the disposition and the
    /// replay clock reach the funnel in one call, never `collect_at` as well.
    /// `CaptureOnly` and `None` queue no award, so XP stays once per message.
    /// Poisoned buffers refuse typed even when no award is due: no
    /// disposition bypasses the failure.
    pub fn collect_at_with_message_disposition(
        &self,
        event: &Event,
        at: &str,
        disposition: FunnelDisposition,
    ) -> Result<Vec<AwardRequest>, LevelingRuntimeError> {
        self.pipeline
            .handle_at_with_message_disposition(event, at, disposition);
        self.check_facts_healthy()?;
        self.pending.take()
    }

    /// Surface a latched facts-buffer failure under the leveling owner.
    /// `check_healthy` fails only with the poison refusal, so the mapping
    /// is exact.
    fn check_facts_healthy(&self) -> Result<(), LevelingRuntimeError> {
        self.facts
            .check_healthy()
            .map_err(|_| LevelingRuntimeError::CommunityFactsPoisoned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Method, http::Response, http::Uri, Router};
    use serde_json::{json, Value};
    use std::collections::VecDeque;

    struct RoleRest {
        executor: Arc<ActionExecutor>,
        requests: Arc<Mutex<Vec<(Method, String)>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for RoleRest {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl RoleRest {
        async fn start(script: Vec<(u16, Value)>) -> Self {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let script = Arc::new(Mutex::new(VecDeque::from(script)));
            let recorded = Arc::clone(&requests);
            let app = Router::new().fallback(move |method: Method, uri: Uri| {
                recorded
                    .lock()
                    .unwrap()
                    .push((method, uri.path().to_owned()));
                let (status, body) = script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, Value::Null));
                async move {
                    Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(if status == 204 {
                            Body::empty()
                        } else {
                            Body::from(body.to_string())
                        })
                        .unwrap()
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let executor = Arc::new(
                ActionExecutor::with_proxy("mock-only-token".into(), Some(origin)).unwrap(),
            );
            Self {
                executor,
                requests,
                task,
            }
        }
    }

    #[tokio::test]
    async fn drain_role_refusal_preserves_prior_committed_awards_in_order() {
        let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
            eprintln!("SKIP drain award preservation: TWO_TEST_DATABASE_URL is not set");
            return;
        };
        let db = two_bot_testsupport::TestDatabase::create(
            &url,
            &sqlx::migrate!("../cutover/migrations"),
        )
        .await
        .expect("create disposable test database");
        for (member, xp) in [(200_u64, 99_i64), (201, 250)] {
            sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ('100',$1,$2,$2,NOW())")
                .bind(member.to_string())
                .bind(xp)
                .execute(db.pool())
                .await
                .unwrap();
        }
        leveling_store::replace_role_rewards(
            db.pool(),
            "100",
            &[two_bot_core::leveling::LevelRoleReward {
                level: 1,
                role_id: "555555555555555555".into(),
            }],
        )
        .await
        .unwrap();
        let rest = RoleRest::start(vec![
            (200, json!({"roles": []})),
            (204, Value::Null),
            (200, json!({"roles": []})),
            (
                403,
                json!({"code": 50013, "message": "Missing Permissions"}),
            ),
        ])
        .await;
        let runtime = LevelingRuntime::new(
            db.pool().clone(),
            Arc::clone(&rest.executor),
            100,
            two_bot_core::OnboardingGates {
                mode: two_bot_core::OnboardingMode::Legacy,
                dry_run: false,
            },
        );
        let pipeline = OrderedLevelingPipeline::new(two_bot_core::MemStore::new(), Some(runtime));
        let requests = [200, 201]
            .into_iter()
            .map(|member_id| AwardRequest {
                guild_id: 100,
                member_id,
                channel_id: 300,
                at: "2026-08-15T00:00:00.000Z".into(),
                duration: None,
                content_length: Some(30),
                has_embed_or_attachment: false,
            })
            .collect();
        let result = pipeline.drain(requests).await;
        let persisted: Vec<(String, i64)> =
            sqlx::query_as("SELECT member_id,xp FROM member_levels ORDER BY member_id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        db.close().await.expect("drop disposable test database");

        let error = result.expect_err("second member's role write must be refused");
        assert_eq!(
            error.committed_awards(),
            &[
                XpAward {
                    awarded: 15,
                    total_xp: 114,
                    level: 1,
                    previous_level: 0,
                    leveled_up: true
                },
                XpAward {
                    awarded: 15,
                    total_xp: 265,
                    level: 2,
                    previous_level: 1,
                    leveled_up: true
                },
            ]
        );
        assert!(matches!(error, LevelingRuntimeError::RoleApplication {
            cause: RoleApplicationError::Refused { role_id, reason, applied, skipped }, ..
        } if role_id == "555555555555555555" && reason.contains("403") && applied.is_empty() && skipped.is_empty()));
        assert_eq!(persisted, vec![("200".into(), 114), ("201".into(), 265)]);
        assert_eq!(
            *rest.requests.lock().unwrap(),
            vec![
                (Method::GET, "/api/v10/guilds/100/members/200".into()),
                (
                    Method::PUT,
                    "/api/v10/guilds/100/members/200/roles/555555555555555555".into()
                ),
                (Method::GET, "/api/v10/guilds/100/members/201".into()),
                (
                    Method::PUT,
                    "/api/v10/guilds/100/members/201/roles/555555555555555555".into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn mixed_revoke_refusal_reports_exact_roles_after_successful_grants() {
        use crate::executor::StagingRevokeFence;
        use two_bot_core::leveling::RewardRolePlan;

        const GUILD: u64 = 1545644954272137297;
        const PRODUCTION: u64 = 326474832151838730;
        let rest = RoleRest::start(vec![
            (200, json!({"id": "4444"})),
            (200, json!({"roles": ["8888"]})),
            (
                200,
                json!([
                    {"id": GUILD.to_string(), "permissions": "0", "position": 0, "managed": false},
                    {"id": "8888", "permissions": "268435456", "position": 10, "managed": false},
                    {"id": "6666", "permissions": "0", "position": 1, "managed": false},
                    {"id": "7777", "permissions": "0", "position": 2, "managed": false},
                ]),
            ),
            (204, Value::Null),
            (204, Value::Null),
            (
                403,
                json!({"code": 50013, "message": "Missing Permissions"}),
            ),
        ])
        .await;
        let plan = RewardRolePlan {
            grant: vec!["5555".into(), "5556".into()],
            revoke: vec!["6666".into(), "7777".into()],
            grant_reason: "TWO leveling: reached level 1".into(),
            revoke_reason: Some("TWO leveling: below level thresholds at level 1".into()),
        };
        let (applied, failed, skipped) = rest
            .executor
            .execute_reward_roles(
                GUILD,
                3333,
                &plan,
                Some(StagingRevokeFence::new(GUILD, PRODUCTION).unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(applied, vec!["5555", "5556"]);
        assert_eq!(
            failed,
            Some((
                "6666".into(),
                "discord refused the request: Discord refused the request with 403".into()
            ))
        );
        assert_eq!(skipped, vec!["7777"]);
        assert_eq!(
            *rest.requests.lock().unwrap(),
            vec![
                (Method::GET, "/api/v10/users/@me".into()),
                (Method::GET, format!("/api/v10/guilds/{GUILD}/members/4444")),
                (Method::GET, format!("/api/v10/guilds/{GUILD}/roles")),
                (
                    Method::PUT,
                    format!("/api/v10/guilds/{GUILD}/members/3333/roles/5555")
                ),
                (
                    Method::PUT,
                    format!("/api/v10/guilds/{GUILD}/members/3333/roles/5556")
                ),
                (
                    Method::DELETE,
                    format!("/api/v10/guilds/{GUILD}/members/3333/roles/6666")
                ),
            ]
        );
    }

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
    fn refresh_classifier_config_swaps_classifier_and_sink_together() {
        use crate::ChannelClassifier as _;
        use two_bot_core::ChannelClass;

        const HUMAN: u64 = 200_000_000_000_000_001;
        let pipeline = OrderedLevelingPipeline::new(two_bot_core::MemStore::new(), None);
        assert_eq!(pipeline.classifier.classify(HUMAN), ChannelClass::Other);

        let config = ClassifierConfig::from_map(
            &[
                ("TWO_COMMUNITY_CLASSIFIER_VERSION", "community-v9"),
                ("TWO_COMMUNITY_HUMAN_CHANNEL_IDS", "200000000000000001"),
            ]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        );
        pipeline
            .refresh_classifier_config(config)
            .expect("healthy config swap");

        assert_eq!(pipeline.classifier.classify(HUMAN), ChannelClass::Human);
        let state = pipeline.facts.0.lock().expect("community facts lock");
        assert_eq!(state.config.version, "community-v9");
        assert_eq!(state.config.channel_class(HUMAN), ChannelClass::Human);
    }

    #[tokio::test]
    async fn enable_with_config_arms_the_sink_with_the_given_config() {
        let buffer = DeferredCommunityFacts::new();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool");
        let config = ClassifierConfig::from_map(
            &[("TWO_COMMUNITY_CLASSIFIER_VERSION", "community-v7")]
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        buffer
            .enable_with_config(pool, config)
            .expect("healthy arming");
        let state = buffer.0.lock().expect("community facts lock");
        assert!(state.pool.is_some());
        assert_eq!(state.config.version, "community-v7");
    }

    #[test]
    fn rules_take_returns_arrival_order_and_empties() {
        let buffer = DeferredCommunityFacts::new();
        assert!(buffer.is_empty().expect("healthy health read"));
        buffer.record_rules_accepted(gate(1, "2026-09-20T12:00:00.000Z", "gateway"));
        buffer.record_rules_accepted(gate(2, "2026-09-20T12:01:00.000Z", "backfill:member_list"));
        assert_eq!(buffer.len().expect("healthy count"), 2);
        let writes = buffer.take().expect("healthy rules take");
        assert_eq!(
            writes.iter().map(|w| w.member_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(writes[1].source, "backfill:member_list");
        assert!(buffer.is_empty().expect("healthy emptied read"));
        assert!(buffer.take().expect("healthy empty take").is_empty());
    }

    fn join<'a>(
        member_id: Snowflake,
        occurred_at: &'a str,
        source_event_id: &'a str,
        source: &'a str,
        inviter_id: Option<Snowflake>,
    ) -> MemberJoinFact<'a> {
        MemberJoinFact {
            guild_id: 100,
            member_id,
            is_bot: false,
            occurred_at,
            source_event_id,
            source,
            inviter_id,
        }
    }

    #[test]
    fn joins_buffer_invite_attribution_for_drain() {
        // The pipeline-resolved invite attribution reaches the buffer
        // unchanged: source, inviter and the gateway
        // `guild:member:joined_at` event id all survive `record_member_join`,
        // and bots buffer too (captured, never funnel-counted downstream).
        let buffer = DeferredCommunityFacts::new();
        buffer.record_member_join(join(
            1,
            "2026-09-20T12:00:00.000Z",
            "100:1:2026-09-20T12:00:00.000Z",
            "invite:abc",
            Some(9),
        ));
        buffer.record_member_join(MemberJoinFact {
            guild_id: 100,
            member_id: 7,
            is_bot: true,
            occurred_at: "2026-09-20T12:00:00.000Z",
            source_event_id: "100:7:2026-09-20T12:00:00.000Z",
            source: "invite:abc",
            inviter_id: Some(9),
        });
        assert_eq!(buffer.joins_len().expect("healthy join count"), 2);
        let writes = buffer.take_joins().expect("healthy joins take");
        assert_eq!(writes[0].source, "invite:abc");
        assert_eq!(writes[0].inviter_id, Some(9));
        assert_eq!(writes[0].source_event_id, "100:1:2026-09-20T12:00:00.000Z");
        assert!(!writes[0].is_bot);
        assert!(writes[1].is_bot, "bot joins buffer for capture");
        assert_eq!(buffer.joins_len().expect("healthy emptied joins"), 0);
        assert!(buffer
            .take_joins()
            .expect("healthy empty joins take")
            .is_empty());
    }

    #[test]
    fn rules_and_joins_buffer_unarmed_while_messages_do_not() {
        // Gate-clearings and joins buffer raw even before `enable` (funnel
        // replay asserts the buffer without a database); messages are dropped
        // while unarmed so a staging-gated rollout captures exactly while it
        // scores; voice never buffers on this slice.
        let buffer = DeferredCommunityFacts::new();
        buffer.record_rules_accepted(gate(1, "2026-09-20T12:00:00.000Z", "gateway"));
        buffer.record_member_join(join(
            1,
            "2026-09-20T12:00:00.000Z",
            "100:1:2026-09-20T12:00:00.000Z",
            "invite:abc",
            None,
        ));
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
            content_length: None,
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
        assert_eq!(
            buffer.len().expect("healthy count"),
            1,
            "the gate-clearing buffers unarmed"
        );
        assert_eq!(
            buffer.joins_len().expect("healthy join count"),
            1,
            "the gateway join buffers unarmed alongside it"
        );
    }

    #[test]
    fn requeue_family_prepends_caps_and_counts_evictions() {
        // Unwritten work goes ahead of arrivals; exactly at the cap
        // nothing is evicted and the counter stays put.
        let mut queue: Vec<u64> = (0..(MAX_QUEUED_FACTS_PER_FAMILY as u64 - 2)).collect();
        let mut dropped = 0;
        requeue_family(&mut queue, vec![20_001, 20_002], &mut dropped);
        assert_eq!(queue.len(), MAX_QUEUED_FACTS_PER_FAMILY);
        assert_eq!(queue[..2], [20_001, 20_002]);
        assert_eq!(dropped, 0);

        // Past the cap the oldest (the requeued prefix) is evicted and
        // every eviction is counted cumulatively.
        let mut queue: Vec<u64> = (0..MAX_QUEUED_FACTS_PER_FAMILY as u64).collect();
        requeue_family(&mut queue, vec![20_001, 20_002], &mut dropped);
        assert_eq!(queue.len(), MAX_QUEUED_FACTS_PER_FAMILY);
        assert_eq!(queue[0], 0);
        assert_eq!(dropped, 2);
    }

    #[test]
    fn poison_displays_carry_the_stable_reason_and_action() {
        // The reason-code constants, the gateway failure line and the
        // recovery doc all carry these literals verbatim; a rename must
        // update every surface together.
        for (text, reason) in [
            (
                LevelingRuntimeError::LevelingPoisoned.to_string(),
                DEFERRED_LEVELING_POISONED,
            ),
            (
                LevelingRuntimeError::CommunityFactsPoisoned.to_string(),
                DEFERRED_COMMUNITY_FACTS_POISONED,
            ),
            (
                CommunityFactsError::Poisoned.to_string(),
                DEFERRED_COMMUNITY_FACTS_POISONED,
            ),
        ] {
            assert!(text.contains(reason), "missing reason: {text}");
            assert!(
                text.contains(SUPERVISOR_RESTART_INSPECTION),
                "missing next action: {text}"
            );
            assert!(text.len() <= 256, "diagnostic must stay bounded");
        }
        assert!(CommunityFactsError::Poisoned.is_poison());
    }
}

#[cfg(test)]
#[path = "../tests/support/deferred_capture_poison.rs"]
mod deferred_capture_poison_acceptance;

#[cfg(feature = "test-support")]
#[path = "../tests/support/deferred_worker_poison.rs"]
pub mod deferred_worker_poison;

#[cfg(test)]
#[path = "../tests/support/community_facts_requeue.rs"]
mod community_facts_requeue_acceptance;

#[cfg(test)]
use community_facts_requeue_acceptance::record_fact;
#[cfg(not(test))]
use two_bot_core::community_store::record_fact;
